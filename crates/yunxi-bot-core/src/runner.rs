//! 调度循环：把 job / ledger / trigger / policy / exec 串成一个可运行的回合。
//!
//! 一个 `tick` 做四件事：
//!
//! 1. **崩溃残留回收**：卡在 `Running` 超过宽限期的任务按失败处理；
//! 2. **计算到期**：交给 `trigger::collect_due`，含单飞跳过；
//! 3. **批准门禁**：需要批准的任务转待批准，**不执行**；
//! 4. **执行并记账**：`exec` 之前先写 durable ack，执行后写结果。
//!
//! 失败方向一律朝"不执行"：任何一步不确定，都转为待批准而不是放行。

use chrono::Local;
use serde_json::json;

use crate::exec::{ExecOptions, IsolationRequirement, run_command};
use crate::job::{GateDecision, Job, JobId, gate};
use crate::ledger::{EventKind, Ledger, LedgerError};
use crate::policy::{ApprovalPolicy, fold_permission_state};
use crate::trigger::{collect_due, is_stale_run, read_mtime_ms};
use crate::{Trigger, now_millis};

#[derive(Debug, Clone)]
pub struct TickOptions {
    /// 部署默认的审批策略。会话事件可覆盖它。
    pub approval: ApprovalPolicy,
    /// 是否要求 OS 级写入隔离。要求而拿不到时任务会失败，而不是无隔离执行。
    pub require_os_isolation: bool,
    /// 每个任务的单进程内存上限（字节）。`None` 用默认值。
    pub task_memory_limit_bytes: Option<u64>,
    /// 每个任务的进程数上限。`None` 用默认值。
    pub task_max_processes: Option<u32>,
    /// 注入时间戳，便于测试。`None` 表示取当前时间。
    pub now_ms: Option<u64>,
}

impl Default for TickOptions {
    fn default() -> Self {
        Self {
            approval: ApprovalPolicy::Ask,
            require_os_isolation: false,
            now_ms: None,
            // 默认给每个任务一道资源护栏：单进程 2 GiB、最多 16 个进程。
            // 目的是"一个任务不能把常驻进程拖垮"，不是精确配额。
            task_memory_limit_bytes: Some(2 * 1024 * 1024 * 1024),
            task_max_processes: Some(16),
        }
    }
}

/// 一个回合的结果。用于展示与测试断言。
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct TickReport {
    /// 台账里的任务总数
    pub checked: usize,
    /// 回收的崩溃残留
    pub stale_recovered: usize,
    /// 本轮到期
    pub due: usize,
    /// 单飞跳过
    pub single_flight_skipped: usize,
    /// 因需要批准而未执行
    pub needs_approval: usize,
    pub succeeded: usize,
    pub failed: usize,
}

impl TickReport {
    /// 本回合是否有任何值得注意的动静。
    pub fn is_quiet(&self) -> bool {
        self.stale_recovered == 0
            && self.due == 0
            && self.single_flight_skipped == 0
            && self.needs_approval == 0
    }
}

/// 跑一个调度回合。
pub fn tick(ledger: &mut Ledger, opts: &TickOptions) -> Result<TickReport, LedgerError> {
    let now_ms = match opts.now_ms {
        Some(v) => v,
        None => now_millis()?,
    };
    let now = Local::now();
    let mut report = TickReport::default();

    // —— 1. 崩溃残留回收 ——
    let jobs = ledger.rebuild();
    report.checked = jobs.len();
    for j in jobs.values() {
        if is_stale_run(j, now_ms) {
            ledger.append(
                EventKind::JobFailed,
                Some(&j.id),
                json!({ "error": "检测到崩溃残留：任务长时间停留在 Running，已回收" }),
            )?;
            report.stale_recovered += 1;
        }
    }

    // 回收后重新投影，避免基于过期状态调度
    let jobs = ledger.rebuild();

    // —— 2. 计算到期 ——
    let due = collect_due(jobs.values(), now_ms, &now);
    report.due = due.due.len();
    report.single_flight_skipped = due.skipped.len();

    for id in &due.skipped {
        ledger.append(
            EventKind::JobSkipped,
            Some(&JobId::new(id.clone())),
            json!({}),
        )?;
    }

    // —— 3 & 4. 批准门禁与执行 ——
    let perm = fold_permission_state(ledger.events());
    let effective_approval = perm.effective_approval(opts.approval);

    for id in &due.due {
        let jid = JobId::new(id.clone());
        let Some(job) = jobs.get(&jid) else { continue };

        // —— 批准门禁 ——
        // `approval` 旋钮只影响"需要批准的动作"如何处置，不影响普通任务。
        match gate(job, effective_approval) {
            GateDecision::Run => {}
            GateDecision::NeedsApproval => {
                ledger.append(
                    EventKind::JobApprovalRequired,
                    Some(&jid),
                    json!({
                        "reason": GateDecision::NeedsApproval.reason(job.spec.irreversible),
                        "command": job.spec.command,
                    }),
                )?;
                report.needs_approval += 1;
                continue;
            }
            GateDecision::AutoRejected => {
                // never 策略下自动拒绝。注意方向：是「拒绝」，不是「放行」。
                ledger.append(
                    EventKind::JobFailed,
                    Some(&jid),
                    json!({
                        "error": GateDecision::AutoRejected.reason(job.spec.irreversible),
                    }),
                )?;
                report.failed += 1;
                continue;
            }
        }

        // 执行前 durable ack：进程此刻被杀也不会丢指令
        ledger.append(
            EventKind::JobStarted,
            Some(&jid),
            json!({ "command": job.spec.command }),
        )?;

        let outcome = run_command(
            &job.spec.command,
            &ExecOptions {
                cwd: job.spec.cwd.clone().into(),
                timeout_ms: job.spec.timeout_ms,
                isolation: if opts.require_os_isolation {
                    IsolationRequirement::RequireOsWriteIsolation
                } else {
                    IsolationRequirement::ProcessOnly
                },
                // 每个任务默认给一道资源上限，避免单个任务把常驻进程拖垮
                memory_limit_bytes: opts.task_memory_limit_bytes,
                max_processes: opts.task_max_processes,
            },
        );

        match outcome {
            Ok(out) if out.succeeded() => {
                ledger.append(
                    EventKind::JobSucceeded,
                    Some(&jid),
                    json!({
                        "exit_code": out.exit_code,
                        "duration_ms": out.duration_ms,
                        "isolation": out.isolation.describe(),
                        "stdout": out.stdout,
                    }),
                )?;
                report.succeeded += 1;
            }
            Ok(out) => {
                ledger.append(
                    EventKind::JobFailed,
                    Some(&jid),
                    json!({
                        "exit_code": out.exit_code,
                        "duration_ms": out.duration_ms,
                        // 失败也是"执行过一次"，隔离级别同样要如实入账——
                        // ADR D7 承诺"每条执行记录都记录实际达到的级别"
                        "isolation": out.isolation.describe(),
                        "error": if out.timed_out {
                            format!("超时（{}ms）后被强制结束", job.spec.timeout_ms)
                        } else if out.stderr.trim().is_empty() {
                            format!("退出码 {}", out.exit_code)
                        } else {
                            out.stderr
                        },
                    }),
                )?;
                report.failed += 1;
            }
            Err(e) => {
                // 派生阶段的失败（含"隔离级别拿不到"）——绝不无隔离跑
                ledger.append(
                    EventKind::JobFailed,
                    Some(&jid),
                    json!({ "error": format!("未能执行: {e}") }),
                )?;
                report.failed += 1;
            }
        }

        record_watch_observation(ledger, job)?;
    }

    Ok(report)
}

/// 记录 watch 目标本轮观测到的 mtime，避免同一变更被重复触发。
fn record_watch_observation(ledger: &mut Ledger, job: &Job) -> Result<(), LedgerError> {
    if let Trigger::Watch { path } = &job.spec.trigger
        && let Some(m) = read_mtime_ms(path)
    {
        ledger.append(
            EventKind::JobWatchObserved,
            Some(&job.id),
            json!({ "mtime_ms": m }),
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job::{JobSpec, Trigger};
    use std::path::PathBuf;

    fn tmp(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("yunxi-bot-runner-{}-{}", std::process::id(), name));
        let _ = std::fs::remove_file(&p);
        p
    }

    fn spec(name: &str, cmd: Vec<&str>, irreversible: bool) -> JobSpec {
        JobSpec {
            name: name.into(),
            command: cmd.into_iter().map(String::from).collect(),
            cwd: ".".into(),
            trigger: Trigger::Manual,
            irreversible,
            max_attempts: 1,
            timeout_ms: 10_000,
        }
    }

    /// Manual 任务不会被自动触发，所以测试里先手动把它推到 Pending 并给个 every 触发器。
    fn every_spec(name: &str, cmd: Vec<&str>, irreversible: bool) -> JobSpec {
        JobSpec {
            trigger: Trigger::Every { seconds: 1 },
            ..spec(name, cmd, irreversible)
        }
    }

    #[cfg(windows)]
    fn echo(s: &str) -> Vec<&str> {
        vec!["cmd", "/C", s]
    }

    #[cfg(windows)]
    #[test]
    fn runs_due_job_and_records_success() {
        let p = tmp("ok");
        let mut l = Ledger::open(&p).unwrap();
        let id = JobId::new("ok1");
        l.append(
            EventKind::JobCreated,
            Some(&id),
            json!({ "spec": every_spec("回声", echo("echo hi"), false) }),
        )
        .unwrap();

        let r = tick(&mut l, &TickOptions::default()).unwrap();
        assert_eq!(r.due, 1);
        assert_eq!(r.succeeded, 1, "应有 1 个成功");

        let jobs = l.rebuild();
        assert_eq!(jobs[&id].state, crate::JobState::Succeeded);

        // 台账里必须留下完整的执行链
        let kinds: Vec<_> = l.events().iter().map(|e| e.kind).collect();
        assert!(kinds.contains(&EventKind::JobStarted));
        assert!(kinds.contains(&EventKind::JobSucceeded));
        let _ = std::fs::remove_file(&p);
    }

    #[cfg(windows)]
    #[test]
    fn irreversible_job_must_be_reapproved_for_every_run() {
        // ADR D5：只有 `allowed-once` 是授权，且只对本次动作有效。
        //
        // 不可逆任务有两条门禁路径：
        //   a) 创建即 WaitingApproval（不等调度，直接拦在门口）；
        //   b) 授权消耗后再到期，运行时被 gate 拦下并记 approval_required。
        let p = tmp("gate");
        let mut l = Ledger::open(&p).unwrap();
        let id = JobId::new("irr1");
        l.append(
            EventKind::JobCreated,
            Some(&id),
            // 用会成功的命令，避免连续失败把任务停用而干扰本测试的焦点
            json!({ "spec": every_spec("发布", echo("echo published"), true) }),
        )
        .unwrap();

        // ① 创建即在门口拦下：不参与调度，也就不会执行
        assert_eq!(l.rebuild()[&id].state, crate::JobState::WaitingApproval);
        let r0 = tick(&mut l, &TickOptions::default()).unwrap();
        assert_eq!(r0.due, 0, "待批准的任务不参与调度");
        assert_eq!(r0.succeeded + r0.failed, 0, "绝不能在未批准时执行");

        // ② 批准后进入待运行，触发时执行一次
        l.append(EventKind::JobApproved, Some(&id), json!({}))
            .unwrap();
        assert_eq!(l.rebuild()[&id].state, crate::JobState::Pending);
        let r1 = tick(&mut l, &TickOptions::default()).unwrap();
        assert_eq!(r1.due, 1);
        assert_eq!(r1.needs_approval, 0, "已获批准，本轮应放行");
        assert_eq!(r1.succeeded + r1.failed, 1, "应实际执行过一次");

        // ③ 授权已消耗：把时钟推到触发间隔之后，运行时被拦下
        let opts = TickOptions {
            now_ms: Some(u64::MAX / 4),
            ..Default::default()
        };
        let r2 = tick(&mut l, &opts).unwrap();
        assert_eq!(r2.due, 1, "推到触发间隔之后应重新到期");
        assert_eq!(r2.needs_approval, 1, "不可逆动作每次运行都要重新批准");
        assert_eq!(r2.succeeded + r2.failed, 0, "未重新批准前不得执行");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn manual_job_is_never_auto_triggered() {
        let p = tmp("manual");
        let mut l = Ledger::open(&p).unwrap();
        let id = JobId::new("m1");
        l.append(
            EventKind::JobCreated,
            Some(&id),
            json!({ "spec": spec("手动", vec!["definitely-not-real"], false) }),
        )
        .unwrap();
        let r = tick(&mut l, &TickOptions::default()).unwrap();
        assert_eq!(r.due, 0, "Manual 任务不得被自动触发");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn requiring_os_isolation_never_runs_unsandboxed() {
        // 契约：要求了 OS 级写入隔离，就**绝不**无隔离执行。
        // 现在隔离已可用，所以正常路径是真的在沙箱里跑；程序不存在则记失败。
        // 无论哪条路，台账里的隔离级别都不能是 `进程隔离`。
        let p = tmp("iso");
        let mut l = Ledger::open(&p).unwrap();
        let id = JobId::new("iso1");
        l.append(
            EventKind::JobCreated,
            Some(&id),
            json!({ "spec": every_spec("需隔离", vec!["definitely-not-real"], false) }),
        )
        .unwrap();

        let opts = TickOptions {
            require_os_isolation: true,
            ..Default::default()
        };
        let r = tick(&mut l, &opts).unwrap();
        // 程序不存在 → 必然失败；但失败原因不能是"缺隔离而放行"
        assert_eq!(r.succeeded, 0);

        let err = l
            .rebuild()
            .get(&id)
            .and_then(|j| j.last_error.clone())
            .unwrap_or_default();
        assert!(
            err.contains("隔离") || err.contains("受限") || err.contains("启动"),
            "错误信息应能说明是隔离/启动环节的问题: {err}"
        );

        // 关键断言：台账里记录的隔离级别绝不能是"仅进程隔离"
        let recorded = l
            .events()
            .iter()
            .filter(|e| e.kind == EventKind::JobFailed)
            .filter_map(|e| e.data.get("isolation").and_then(|v| v.as_str()))
            .map(str::to_string)
            .collect::<Vec<_>>();
        assert!(
            recorded.iter().all(|s| !s.contains("未实施 OS 级强制")),
            "要求隔离时绝不允许以无隔离级别执行: {recorded:?}"
        );
        let _ = std::fs::remove_file(&p);
    }
}
