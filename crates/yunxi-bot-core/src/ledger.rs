//! 任务台账：append-only 事件日志 + 可丢弃投影。
//!
//! 设计依据 ADR-0001 §六 D6：
//!
//! 1. **事件日志是唯一真相来源**，状态全部是投影，可随时丢弃重建。
//! 2. **审计事件必须被「边界」包住**：落在边界之外的事件与崩溃残尾无法区分，
//!    reload 时会被静默丢弃。因此宁可直接返回错误，也不写入边界外的事件。
//! 3. **格式版本化**，为后续相邻迁移链（vN → vN+1）留出入口。

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::job::{Job, JobId, JobSpec, JobState};
use crate::{CoreError, now_millis};

/// 当前台账格式版本。
///
/// 变更格式时必须**新增相邻迁移**（vN → vN+1），不可跳版、不可重命名。
pub const LEDGER_FORMAT_VERSION: u32 = 1;

/// 台账文件头。第一行固定是它。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Header {
    pub yunxi_bot_ledger: u32,
}

/// 事件类型。**封闭词汇表**——不在此列的类型无法反序列化，直接报错。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    // —— 权限（由 policy::apply_permission_event 折叠）——
    PermissionPreset,
    SandboxModeSet,
    ApprovalPolicySet,
    SessionSeeded,

    // —— 任务生命周期 ——
    JobCreated,
    JobApprovalRequired,
    JobApproved,
    JobStarted,
    JobSucceeded,
    JobFailed,
    JobSkipped,
    /// `Watch` 触发器本轮观测到的 mtime，用于避免同一变更被重复触发。
    JobWatchObserved,

    // —— 记忆（由 memory::Memory::from_events 投影）——
    MemoryRecorded,
    MemoryReinforced,
    MemoryForgotten,

    // —— 审计对（必须位于边界内）——
    DecisionAsked,
    DecisionDecided,
    ApprovalAsked,
    ApprovalDecided,
}

impl EventKind {
    /// 该类型是否属于「审计对」，因而**必须**被边界包住。
    pub fn requires_span(self) -> bool {
        matches!(
            self,
            EventKind::DecisionAsked
                | EventKind::DecisionDecided
                | EventKind::ApprovalAsked
                | EventKind::ApprovalDecided
        )
    }
}

/// 一条台账事件。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub seq: u64,
    pub at: u64,
    pub kind: EventKind,
    /// 所属边界 id。`None` 表示不在边界内。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub span: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job: Option<String>,
    #[serde(default)]
    pub data: Value,
}

/// 台账错误。
#[derive(Debug)]
pub enum LedgerError {
    Io(String),
    /// 头部缺失或版本不受支持。
    Format(String),
    /// 审计事件落在边界之外。
    AuditOutsideSpan(EventKind),
    /// 边界状态非法（重复开启 / 未开启就关闭）。
    SpanState(&'static str),
    /// 值不是无损 JSON。
    LossyJson(String),
    Core(CoreError),
}

impl std::fmt::Display for LedgerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LedgerError::Io(m) => write!(f, "台账 IO 错误: {m}"),
            LedgerError::Format(m) => write!(f, "台账格式错误: {m}"),
            LedgerError::AuditOutsideSpan(k) => write!(
                f,
                "审计事件 {k:?} 位于边界之外：这类事件与崩溃残尾无法区分，reload 时会被丢弃，因此拒绝写入"
            ),
            LedgerError::SpanState(m) => write!(f, "边界状态错误: {m}"),
            LedgerError::LossyJson(m) => write!(f, "值不是无损 JSON: {m}"),
            LedgerError::Core(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for LedgerError {}

impl From<CoreError> for LedgerError {
    fn from(e: CoreError) -> Self {
        LedgerError::Core(e)
    }
}

/// 判断一个 JSON 值是否**无损**（可安全往返序列化）。
///
/// 拒绝：非有限浮点、负零。理由：`-0.0` 经 JSON 往返会变成 `0.0`，
/// 这类静默改写会破坏"台账是审计依据"的前提。
pub fn is_lossless_json(v: &Value) -> Result<(), String> {
    match v {
        Value::Null | Value::Bool(_) | Value::String(_) => Ok(()),
        Value::Number(n) => {
            if let Some(f) = n.as_f64() {
                if !f.is_finite() {
                    return Err(format!("非有限浮点 {f}"));
                }
                if f == 0.0 && f.is_sign_negative() {
                    return Err("负零在 JSON 往返中不稳定".into());
                }
            }
            Ok(())
        }
        Value::Array(a) => a.iter().try_for_each(is_lossless_json),
        Value::Object(o) => o.values().try_for_each(is_lossless_json),
    }
}

/// 台账。持有文件句柄、当前序号、边界状态与已加载的事件。
#[derive(Debug)]
pub struct Ledger {
    path: PathBuf,
    file: File,
    seq: u64,
    open_span: Option<u64>,
    next_span: u64,
    events: Vec<Event>,
}

impl Ledger {
    /// 打开（或创建）台账文件。
    ///
    /// 已存在的文件必须带合法头部；事件流中任何无法解析的行都视为**损坏**
    /// 而不是跳过——静默跳行会让投影悄悄偏离真相。
    pub fn open(path: impl AsRef<Path>) -> Result<Self, LedgerError> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(|e| LedgerError::Io(e.to_string()))?;
            }
        }

        let mut events = Vec::new();
        let existed = path.exists()
            && std::fs::metadata(&path)
                .map(|m| m.len() > 0)
                .unwrap_or(false);

        if existed {
            let f = File::open(&path).map_err(|e| LedgerError::Io(e.to_string()))?;
            let mut lines = BufReader::new(f).lines();
            let head = lines
                .next()
                .ok_or_else(|| LedgerError::Format("台账为空".into()))?
                .map_err(|e| LedgerError::Io(e.to_string()))?;
            let header: Header = serde_json::from_str(&head)
                .map_err(|e| LedgerError::Format(format!("头部不可解析: {e}")))?;
            if header.yunxi_bot_ledger != LEDGER_FORMAT_VERSION {
                return Err(LedgerError::Format(format!(
                    "不支持的台账版本 {}，本程序只支持 {}",
                    header.yunxi_bot_ledger, LEDGER_FORMAT_VERSION
                )));
            }
            for (i, line) in lines.enumerate() {
                let line = line.map_err(|e| LedgerError::Io(e.to_string()))?;
                if line.trim().is_empty() {
                    continue;
                }
                let ev: Event = serde_json::from_str(&line).map_err(|e| {
                    LedgerError::Format(format!("第 {} 条事件不可解析: {e}", i + 1))
                })?;
                events.push(ev);
            }
        }

        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| LedgerError::Io(e.to_string()))?;

        if !existed {
            let header = json!({ "yunxi_bot_ledger": LEDGER_FORMAT_VERSION });
            writeln!(file, "{header}").map_err(|e| LedgerError::Io(e.to_string()))?;
            file.flush().map_err(|e| LedgerError::Io(e.to_string()))?;
        }

        let seq = events.last().map(|e| e.seq).unwrap_or(0);
        let next_span = seq + 1;

        Ok(Self {
            path,
            file,
            seq,
            open_span: None,
            next_span,
            events,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn len(&self) -> usize {
        self.events.len()
    }

    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    pub fn events(&self) -> &[Event] {
        &self.events
    }

    /// 开启一个边界。边界是崩溃恢复的提交单位，审计事件必须落在其中。
    pub fn begin_span(&mut self) -> Result<SpanGuard<'_>, LedgerError> {
        if self.open_span.is_some() {
            return Err(LedgerError::SpanState("边界已开启，不可嵌套"));
        }
        let id = self.next_span;
        self.next_span += 1;
        self.open_span = Some(id);
        Ok(SpanGuard {
            ledger: self,
            id,
            closed: false,
        })
    }

    pub(crate) fn end_span_inner(&mut self) -> Result<(), LedgerError> {
        if self.open_span.take().is_none() {
            return Err(LedgerError::SpanState("没有开启中的边界"));
        }
        Ok(())
    }

    pub fn has_open_span(&self) -> bool {
        self.open_span.is_some()
    }

    /// 追加一条事件。
    ///
    /// 需要边界的类型在边界外调用会**直接返回错误**，不写入任何内容。
    pub fn append(
        &mut self,
        kind: EventKind,
        job: Option<&JobId>,
        data: Value,
    ) -> Result<Event, LedgerError> {
        if kind.requires_span() && self.open_span.is_none() {
            return Err(LedgerError::AuditOutsideSpan(kind));
        }
        is_lossless_json(&data).map_err(LedgerError::LossyJson)?;

        let seq = self.seq + 1;
        let ev = Event {
            seq,
            at: now_millis()?,
            kind,
            span: self.open_span,
            job: job.map(|j| j.0.clone()),
            data,
        };
        let line = serde_json::to_string(&ev).map_err(|e| LedgerError::Io(e.to_string()))?;
        writeln!(self.file, "{line}").map_err(|e| LedgerError::Io(e.to_string()))?;
        // 落盘后再更新内存状态：宁可重复写也不丢写
        self.file
            .flush()
            .map_err(|e| LedgerError::Io(e.to_string()))?;

        self.seq = seq;
        self.events.push(ev.clone());
        Ok(ev)
    }

    /// 重建任务投影。**这就是"跨重启存活"的实现**：状态全部从日志重算。
    pub fn rebuild(&self) -> BTreeMap<JobId, Job> {
        project(&self.events)
    }
}

/// 边界守卫。析构时自动关闭边界，避免忘记关闭导致的悬挂状态。
#[derive(Debug)]
pub struct SpanGuard<'a> {
    ledger: &'a mut Ledger,
    id: u64,
    closed: bool,
}

impl<'a> SpanGuard<'a> {
    pub fn id(&self) -> u64 {
        self.id
    }

    /// 在边界内操作台账。
    pub fn ledger(&mut self) -> &mut Ledger {
        self.ledger
    }

    /// 显式关闭边界。
    pub fn close(mut self) -> Result<(), LedgerError> {
        self.closed = true;
        self.ledger.end_span_inner()
    }
}

impl Drop for SpanGuard<'_> {
    fn drop(&mut self) {
        if !self.closed {
            let _ = self.ledger.end_span_inner();
        }
    }
}

/// 把事件流折叠成任务集合。纯函数，便于测试。
pub fn project(events: &[Event]) -> BTreeMap<JobId, Job> {
    let mut jobs: BTreeMap<JobId, Job> = BTreeMap::new();

    for e in events {
        let Some(job_str) = &e.job else { continue };
        let id = JobId::new(job_str.clone());

        match e.kind {
            EventKind::JobCreated => {
                let Some(spec) = e
                    .data
                    .get("spec")
                    .and_then(|v| serde_json::from_value::<JobSpec>(v.clone()).ok())
                else {
                    continue;
                };
                jobs.insert(id.clone(), Job::new(id, spec, e.at));
            }
            EventKind::JobApprovalRequired => {
                if let Some(j) = jobs.get_mut(&id) {
                    j.state = JobState::WaitingApproval;
                    j.last_error = e
                        .data
                        .get("reason")
                        .and_then(|v| v.as_str())
                        .map(str::to_string);
                    j.updated_at = e.at;
                }
            }
            EventKind::JobApproved => {
                if let Some(j) = jobs.get_mut(&id) {
                    j.approvals += 1;
                    if j.state == JobState::WaitingApproval {
                        j.state = JobState::Pending;
                    }
                    j.updated_at = e.at;
                }
            }
            EventKind::JobStarted => {
                if let Some(j) = jobs.get_mut(&id) {
                    j.state = JobState::Running;
                    j.attempts += 1;
                    j.last_run_at = Some(e.at);
                    j.updated_at = e.at;
                }
            }
            EventKind::JobSucceeded => {
                if let Some(j) = jobs.get_mut(&id) {
                    j.state = JobState::Succeeded;
                    j.last_success_at = Some(e.at);
                    j.last_error = None;
                    // 成功清零连续失败计数
                    j.consecutive_failures = 0;
                    j.updated_at = e.at;
                }
            }
            EventKind::JobFailed => {
                if let Some(j) = jobs.get_mut(&id) {
                    j.last_error = e
                        .data
                        .get("error")
                        .and_then(|v| v.as_str())
                        .map(str::to_string);
                    // 连续失败计数 +1，再决定是留在可调度状态还是停用。
                    // 用连续而非累计，是为了让周期性任务扛得住偶发失败。
                    j.consecutive_failures += 1;
                    j.state = if j.has_retry_budget() {
                        // 仍在预算内：记为失败，但任务依旧可被触发
                        JobState::Failed
                    } else {
                        // 超限：停用，需人工重置
                        JobState::Disabled
                    };
                    j.updated_at = e.at;
                }
            }
            EventKind::JobSkipped => {
                if let Some(j) = jobs.get_mut(&id) {
                    // 纯观测事件：单飞跳过不改变状态——"任务仍在运行"
                    // 这个事实不能被一次跳过覆盖掉。
                    j.last_skipped_at = Some(e.at);
                    j.updated_at = e.at;
                }
            }
            EventKind::JobWatchObserved => {
                if let Some(j) = jobs.get_mut(&id) {
                    j.watch_mtime_ms = e.data.get("mtime_ms").and_then(|v| v.as_u64());
                    j.updated_at = e.at;
                }
            }
            _ => {}
        }
    }

    jobs
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job::Trigger;

    fn tmp(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("yunxi-bot-test-{}-{}", std::process::id(), name));
        let _ = std::fs::remove_file(&p);
        p
    }

    fn spec(name: &str, irreversible: bool) -> JobSpec {
        JobSpec {
            name: name.into(),
            command: vec!["echo".into()],
            cwd: ".".into(),
            trigger: Trigger::Manual,
            irreversible,
            max_attempts: 2,
            timeout_ms: 1000,
        }
    }

    #[test]
    fn rejects_negative_zero() {
        assert!(is_lossless_json(&json!({ "a": -0.0 })).is_err());
        assert!(is_lossless_json(&json!({ "a": 0.0 })).is_ok());
    }

    #[test]
    fn audit_outside_span_is_rejected() {
        let p = tmp("audit");
        let mut l = Ledger::open(&p).unwrap();
        let err = l
            .append(EventKind::ApprovalAsked, None, json!({}))
            .unwrap_err();
        assert!(matches!(err, LedgerError::AuditOutsideSpan(_)));
        // 拒绝时不得写入任何内容
        assert_eq!(l.len(), 0);
    }

    #[test]
    fn audit_inside_span_is_accepted_and_tagged() {
        let p = tmp("span");
        let mut l = Ledger::open(&p).unwrap();
        {
            let mut g = l.begin_span().unwrap();
            let id = g.id();
            let ev = g
                .ledger()
                .append(EventKind::ApprovalAsked, None, json!({"tool":"x"}))
                .unwrap();
            assert_eq!(ev.span, Some(id));
            g.close().unwrap();
        }
        assert!(!l.has_open_span());
    }

    #[test]
    fn state_survives_reopen() {
        let p = tmp("survive");
        {
            let mut l = Ledger::open(&p).unwrap();
            let id = JobId::new("j1");
            l.append(
                EventKind::JobCreated,
                Some(&id),
                json!({ "spec": spec("备份", false) }),
            )
            .unwrap();
            l.append(EventKind::JobStarted, Some(&id), json!({}))
                .unwrap();
            l.append(
                EventKind::JobSucceeded,
                Some(&id),
                json!({ "exit_code": 0 }),
            )
            .unwrap();
        }

        // 重新打开：状态必须完整重建
        let l2 = Ledger::open(&p).unwrap();
        let jobs = l2.rebuild();
        let j = jobs.get(&JobId::new("j1")).expect("任务应存在");
        assert_eq!(j.state, JobState::Succeeded);
        assert_eq!(j.attempts, 1);
        assert_eq!(j.spec.name, "备份");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn irreversible_job_starts_waiting_approval() {
        let p = tmp("irrev");
        let mut l = Ledger::open(&p).unwrap();
        let id = JobId::new("j2");
        l.append(
            EventKind::JobCreated,
            Some(&id),
            json!({ "spec": spec("发布", true) }),
        )
        .unwrap();
        let jobs = l.rebuild();
        assert_eq!(jobs[&id].state, JobState::WaitingApproval);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn consecutive_failures_disable_the_job_only_after_budget_exhausted() {
        let p = tmp("retry");
        let mut l = Ledger::open(&p).unwrap();
        let id = JobId::new("j3");
        l.append(
            EventKind::JobCreated,
            Some(&id),
            json!({ "spec": spec("重试", false) }),
        )
        .unwrap();
        l.append(EventKind::JobStarted, Some(&id), json!({}))
            .unwrap();
        l.append(EventKind::JobFailed, Some(&id), json!({ "error": "boom" }))
            .unwrap();

        let jobs = l.rebuild();
        // max_attempts = 2，连续失败 1 次：记为失败，但**仍可被触发**
        assert_eq!(jobs[&id].state, JobState::Failed);
        assert!(
            jobs[&id].state.is_schedulable(),
            "周期性任务不能被一次失败打死"
        );
        assert_eq!(jobs[&id].consecutive_failures, 1);

        l.append(EventKind::JobStarted, Some(&id), json!({}))
            .unwrap();
        l.append(EventKind::JobFailed, Some(&id), json!({ "error": "boom2" }))
            .unwrap();
        let jobs = l.rebuild();
        assert_eq!(jobs[&id].state, JobState::Disabled, "连续失败超限后停用");
        assert!(jobs[&id].state.is_terminal());

        // 成功一次应清零连续失败计数
        let id2 = JobId::new("j4");
        l.append(
            EventKind::JobCreated,
            Some(&id2),
            json!({ "spec": spec("会成功", false) }),
        )
        .unwrap();
        l.append(EventKind::JobStarted, Some(&id2), json!({}))
            .unwrap();
        l.append(EventKind::JobFailed, Some(&id2), json!({ "error": "boom" }))
            .unwrap();
        l.append(EventKind::JobStarted, Some(&id2), json!({}))
            .unwrap();
        l.append(EventKind::JobSucceeded, Some(&id2), json!({}))
            .unwrap();
        let jobs = l.rebuild();
        assert_eq!(jobs[&id2].consecutive_failures, 0, "成功应清零连续失败");
        assert_eq!(jobs[&id2].state, JobState::Succeeded);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn unsupported_format_version_is_rejected() {
        let p = tmp("badver");
        std::fs::write(&p, "{\"yunxi_bot_ledger\":999}\n").unwrap();
        let err = Ledger::open(&p).unwrap_err();
        assert!(matches!(err, LedgerError::Format(_)));
        let _ = std::fs::remove_file(&p);
    }
}
