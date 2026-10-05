//! # YunXi Bot 内聚内核
//!
//! 设计见 [`docs/adr/0001-架构与边界.md`](../../../docs/adr/0001-架构与边界.md)。
//!
//! 本 crate 是**内聚的具体内核**，不是元内核：它明确知道自己管什么——
//! 任务、台账、触发、约束、决策、执行。没有服务查找、没有事件总线、
//! 没有插件生命周期，依赖全部是编译期显式的。
//!
//! 四条不可违背的原则（ADR §八）：
//!
//! 1. 失败方向朝「不执行」——判定不明一律转待批准，绝不放行。
//! 2. 不可逆动作必须人工批准，且批准不参与降级。
//! 3. 决策模型的判断不能替代策略——模型给概率，约束层做决定。
//! 4. 不写入边界外的审计事件——宁可抛错。

pub mod agent;
pub mod companion;
pub mod costlog;
pub mod decide;
pub mod exec;
pub mod instance;
pub mod job;
pub mod ledger;
pub mod memory;
pub mod policy;
pub mod runner;
pub mod task;
pub mod think;
pub mod trigger;
pub mod win_job;
#[cfg(windows)]
pub mod win_token;

pub use exec::{ExecError, ExecOptions, ExecOutcome, IsolationLevel, IsolationRequirement};
pub use job::{Job, JobId, JobSpec, JobState, Trigger};
pub use ledger::{Event, EventKind, Ledger, LedgerError, SpanGuard};
pub use policy::{
    ApprovalOutcome, ApprovalPolicy, PermissionState, SandboxMode, normalize_outcome,
};
pub use runner::{TickOptions, TickReport, tick};

/// 内核错误。所有失败都显式表达，不使用 panic 作为控制流。
#[derive(Debug)]
pub enum CoreError {
    /// 台账读写失败
    Ledger(String),
    /// 策略判定拒绝
    Policy(String),
    /// 状态机非法迁移
    InvalidTransition {
        from: JobState,
        to: JobState,
        reason: &'static str,
    },
    /// 时间戳不可用
    Clock(String),
}

impl std::fmt::Display for CoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CoreError::Ledger(m) => write!(f, "台账错误: {m}"),
            CoreError::Policy(m) => write!(f, "策略拒绝: {m}"),
            CoreError::InvalidTransition { from, to, reason } => {
                write!(f, "非法状态迁移 {from:?} -> {to:?}: {reason}")
            }
            CoreError::Clock(m) => write!(f, "时钟错误: {m}"),
        }
    }
}

impl std::error::Error for CoreError {}

/// 当前 Unix 毫秒时间戳。
///
/// 时间在系统时钟回拨时仍返回错误而不是静默返回 0 —— 台账时序是审计依据，
/// 不能用一个看似合理的时间戳掩盖问题。
pub fn now_millis() -> Result<u64, CoreError> {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .map_err(|e| CoreError::Clock(e.to_string()))
}
