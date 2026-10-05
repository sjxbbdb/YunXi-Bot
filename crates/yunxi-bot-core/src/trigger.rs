//! 触发器与调度：主动触发的实现。
//!
//! 支持 `manual` / `every` / `cron` / `watch` 四种触发方式。
//!
//! 两条关键策略：
//!
//! - **单飞（single-flight）**：上次还在跑就跳过本次，避免两个进程改同一份数据。
//! - **崩溃残留检测**：进程被杀会让任务卡在 `Running`，超过宽限期按失败处理并回队列。
//!
//! cron 按**本地时间**解析——个人助理的"每天早上 9 点"必须指本地 9 点。

use std::collections::BTreeSet;

use chrono::{DateTime, Datelike, Local, Timelike};

use crate::job::{Job, JobState, Trigger};

/// 解析后的 cron 字段。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CronFields {
    pub minute: BTreeSet<u32>,
    pub hour: BTreeSet<u32>,
    pub dom: BTreeSet<u32>,
    pub month: BTreeSet<u32>,
    pub dow: BTreeSet<u32>,
}

const RANGES: [(&str, u32, u32); 5] = [
    ("minute", 0, 59),
    ("hour", 0, 23),
    ("dom", 1, 31),
    ("month", 1, 12),
    ("dow", 0, 6),
];

/// 解析单个 cron 字段。支持 `*`、`n`、`a-b`、`a,b,c`、`*/n`、`a-b/n`。
///
/// 非法输入返回 `None`——**绝不静默降级为"总是匹配"**。
fn parse_field(spec: &str, name: &str) -> Option<BTreeSet<u32>> {
    let (_, min, max) = RANGES.iter().find(|(n, _, _)| *n == name)?;
    let (min, max) = (*min, *max);
    let mut out = BTreeSet::new();

    for part in spec.split(',') {
        let (range_part, step_part) = match part.split_once('/') {
            Some((r, s)) => (r, Some(s)),
            None => (part, None),
        };
        let step: u32 = match step_part {
            None => 1,
            Some(s) => s.parse().ok().filter(|v| *v > 0)?,
        };

        let (lo, hi) = if range_part == "*" {
            (min, max)
        } else if let Some((a, b)) = range_part.split_once('-') {
            let a: u32 = a.parse().ok()?;
            let b: u32 = b.parse().ok()?;
            (a, b)
        } else {
            let v: u32 = range_part.parse().ok()?;
            (v, v)
        };

        if lo < min || hi > max || lo > hi {
            return None;
        }
        let mut v = lo;
        while v <= hi {
            out.insert(v);
            v += step;
        }
    }

    if out.is_empty() { None } else { Some(out) }
}

/// 解析 5 字段 cron 表达式：`分 时 日 月 周`。非法返回 `None`。
pub fn parse_cron(expr: &str) -> Option<CronFields> {
    let parts: Vec<&str> = expr.split_whitespace().collect();
    if parts.len() != 5 {
        return None;
    }
    Some(CronFields {
        minute: parse_field(parts[0], "minute")?,
        hour: parse_field(parts[1], "hour")?,
        dom: parse_field(parts[2], "dom")?,
        month: parse_field(parts[3], "month")?,
        dow: parse_field(parts[4], "dow")?,
    })
}

/// 判断某时刻是否命中 cron（本地时间，分钟粒度）。
pub fn cron_matches(expr: &str, t: &DateTime<Local>) -> bool {
    let Some(f) = parse_cron(expr) else {
        return false;
    };
    f.minute.contains(&t.minute())
        && f.hour.contains(&t.hour())
        && f.dom.contains(&t.day())
        && f.month.contains(&t.month())
        && f.dow.contains(&t.weekday().num_days_from_sunday())
}

/// 单飞检查：正在跑的任务本轮跳过。
pub fn is_single_flight_blocked(job: &Job) -> bool {
    job.state == JobState::Running
}

/// 崩溃残留检测：卡在 `Running` 超过超时宽限期（两倍）的任务，视为死锁。
pub fn is_stale_run(job: &Job, now_ms: u64) -> bool {
    if job.state != JobState::Running {
        return false;
    }
    let timeout = job.spec.timeout_ms;
    let since = now_ms.saturating_sub(job.last_run_at.unwrap_or(job.updated_at));
    since > timeout.saturating_mul(2)
}

/// 本轮调度结论。
#[derive(Debug, Default)]
pub struct DueResult {
    /// 应开始执行的任务
    pub due: Vec<String>,
    /// 被单飞策略跳过的任务
    pub skipped: Vec<String>,
}

/// 计算此刻应该运行的任务。
///
/// **`Manual` 永远不会被自动触发**——这是刻意的：显式请求才执行。
///
/// 触发器会对**所有非终态**任务求值，而不只是 `Pending` 的：只有这样才能
/// 观测到"计划到点了，但上一次还在跑"这种单飞情况。单飞本身不靠额外开关，
/// 而是靠 `Running` 不可调度这一条。
pub fn collect_due<'a, I>(jobs: I, now_ms: u64, now: &DateTime<Local>) -> DueResult
where
    I: IntoIterator<Item = &'a Job>,
{
    let mut out = DueResult::default();

    for job in jobs {
        if job.state.is_terminal() {
            continue;
        }

        let matched = match &job.spec.trigger {
            Trigger::Manual => false,
            Trigger::Every { seconds } => {
                if *seconds == 0 {
                    false
                } else {
                    match job.last_run_at {
                        None => true,
                        Some(last) => now_ms.saturating_sub(last) >= seconds.saturating_mul(1000),
                    }
                }
            }
            Trigger::Cron { expr } => cron_matches(expr, now),
            Trigger::Watch { path } => match std::fs::metadata(path) {
                Ok(md) => match md.modified() {
                    Ok(m) => {
                        let mtime_ms = m
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_millis() as u64)
                            .unwrap_or(0);
                        job.watch_mtime_ms.is_none_or(|prev| mtime_ms > prev)
                    }
                    Err(_) => false,
                },
                // 路径不存在时不触发，也不报错
                Err(_) => false,
            },
        };

        if !matched {
            continue;
        }

        if is_single_flight_blocked(job) {
            // 单飞：上一次还在跑，本轮跳过（只记录观测，不改状态）
            out.skipped.push(job.id.0.clone());
        } else if job.state.is_schedulable() {
            out.due.push(job.id.0.clone());
        }
        // WaitingApproval：已在台账里标记过，不重复报告
    }

    out
}

/// 读取监听目标的当前 mtime（毫秒）。
pub fn read_mtime_ms(path: &str) -> Option<u64> {
    std::fs::metadata(path)
        .ok()?
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_millis() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job::{JobId, JobSpec};
    use chrono::TimeZone;

    fn at(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> DateTime<Local> {
        Local.with_ymd_and_hms(y, mo, d, h, mi, 0).unwrap()
    }

    fn job(trigger: Trigger, state: JobState, last_run_at: Option<u64>) -> Job {
        let spec = JobSpec {
            name: "t".into(),
            command: vec!["echo".into()],
            cwd: ".".into(),
            trigger,
            irreversible: false,
            max_attempts: 1,
            timeout_ms: 1000,
        };
        let mut j = Job::new(JobId::new("j"), spec, 0);
        j.state = state;
        j.last_run_at = last_run_at;
        j
    }

    #[test]
    fn cron_parses_and_matches() {
        assert!(parse_cron("0 9 * * *").is_some());
        assert!(cron_matches("0 9 * * *", &at(2026, 10, 5, 9, 0)));
        assert!(!cron_matches("0 9 * * *", &at(2026, 10, 5, 9, 1)));
    }

    #[test]
    fn invalid_cron_never_matches() {
        // 关键：非法表达式不得静默降级为"总是匹配"
        for bad in ["", "* * *", "99 9 * * *", "0 9 * * 9", "abc", "* * * * * *"] {
            assert!(parse_cron(bad).is_none(), "{bad:?} 应解析失败");
            assert!(!cron_matches(bad, &at(2026, 10, 5, 9, 0)));
        }
    }

    #[test]
    fn step_and_list_and_range() {
        let f = parse_cron("*/15 9-17 1,15 * 1-5").unwrap();
        assert_eq!(
            f.minute.iter().copied().collect::<Vec<_>>(),
            vec![0, 15, 30, 45]
        );
        assert_eq!(f.hour.len(), 9); // 9..=17
        assert_eq!(f.dom.iter().copied().collect::<Vec<_>>(), vec![1, 15]);
        assert_eq!(f.dow.len(), 5);
    }

    #[test]
    fn manual_is_never_auto_triggered() {
        let j = job(Trigger::Manual, JobState::Pending, None);
        let r = collect_due([&j], 1_000_000, &at(2026, 10, 5, 9, 0));
        assert!(r.due.is_empty());
    }

    #[test]
    fn every_triggers_when_interval_elapsed() {
        let now = 100_000u64;
        let j = job(
            Trigger::Every { seconds: 60 },
            JobState::Pending,
            Some(now - 59_000),
        );
        let r = collect_due([&j], now, &at(2026, 10, 5, 9, 0));
        assert!(r.due.is_empty(), "未到间隔不应触发");

        let j2 = job(
            Trigger::Every { seconds: 60 },
            JobState::Pending,
            Some(now - 60_000),
        );
        let r2 = collect_due([&j2], now, &at(2026, 10, 5, 9, 0));
        assert_eq!(r2.due, vec!["j".to_string()]);
    }

    #[test]
    fn single_flight_skips_running_job() {
        let now = 100_000u64;
        let j = job(
            Trigger::Every { seconds: 1 },
            JobState::Running,
            Some(now - 5000),
        );
        let r = collect_due([&j], now, &at(2026, 10, 5, 9, 0));
        assert!(r.due.is_empty());
        assert_eq!(r.skipped, vec!["j".to_string()]);
    }

    #[test]
    fn skip_is_observational_and_does_not_kill_the_job() {
        // 修掉的缺陷：曾经把"单飞跳过"投影成任务状态，那会让任务被跳过
        // 一次就再也不能运行。现在跳过只写 last_skipped_at，状态不变。
        let now = 100_000u64;
        let mut j = job(
            Trigger::Every { seconds: 1 },
            JobState::Running,
            Some(now - 5000),
        );
        let r = collect_due([&j], now, &at(2026, 10, 5, 9, 0));
        assert_eq!(r.skipped, vec!["j".to_string()]);
        assert_eq!(j.state, JobState::Running, "跳过不得改变状态");

        // 上一次跑完之后，任务回到 Pending，下一轮照常到期
        j.state = JobState::Pending;
        let r2 = collect_due([&j], now, &at(2026, 10, 5, 9, 0));
        assert_eq!(r2.due, vec!["j".to_string()], "跑完后应重新到期");
    }

    #[test]
    fn waiting_approval_is_neither_due_nor_skipped() {
        let now = 100_000u64;
        let j = job(
            Trigger::Every { seconds: 1 },
            JobState::WaitingApproval,
            Some(now - 5000),
        );
        let r = collect_due([&j], now, &at(2026, 10, 5, 9, 0));
        assert!(r.due.is_empty());
        assert!(r.skipped.is_empty(), "待批准不属于单飞跳过");
    }

    #[test]
    fn stale_run_detected_after_two_timeouts() {
        let spec_timeout = 1000u64;
        let mut j = job(Trigger::Manual, JobState::Running, Some(0));
        j.spec.timeout_ms = spec_timeout;
        assert!(!is_stale_run(&j, 1500));
        assert!(is_stale_run(&j, 2500));
    }

    #[test]
    fn missing_watch_path_does_not_trigger() {
        let j = job(
            Trigger::Watch {
                path: "Z:\\definitely-not-here\\x".into(),
            },
            JobState::Pending,
            None,
        );
        let r = collect_due([&j], 1, &at(2026, 10, 5, 9, 0));
        assert!(r.due.is_empty());
    }
}
