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
/// 助理巡览：取信息 → 判断 → 通知 → 落台账。
///
/// 放在内核里而不是 CLI 里，是因为它有**两个调用方**：
/// `yunxi-bot check`（你主动问）和 daemon 的每一轮（它自己看）。
/// 抄两份的话两边迟早漂移，而漂移的表现是"手动跑没问题，常驻跑出问题"。
pub mod assistant;
pub mod companion;
pub mod costlog;
pub mod decide;
pub mod diff;
pub mod embedding;
pub mod exec;
/// 反馈回写：使用者的处置，以及下一次判断要不要记得它。
pub mod feedback;
/// 信息源：助理"看外面"的入口。
pub mod info;
pub mod instance;
pub mod job;
pub mod ledger;
pub mod mcp;
pub mod memory;
pub mod notify;
/// Windows toast 后端。**不在非 Windows 平台上编译**——它依赖 PowerShell + WinRT，
/// 在没有它们的平台上放一个永远返回失败的实现只会制造噪音。
#[cfg(windows)]
pub mod notify_windows;
pub mod policy;
pub mod profile;
pub mod rules;
pub mod runner;
pub mod task;
pub mod think;
pub mod tool;
/// 打扰判定：这条信息值不值得现在告诉你。
pub mod triage;
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

/// 数据目录。**这是内核里唯一一份定义。**
///
/// ## 为什么这件事值得有"唯一一份"
///
/// 之前 CLI 自己算一份、sidecar 在 Python 里再算一份。两边算法看起来一样
/// （`YUNXI_BOT_HOME` → `%LOCALAPPDATA%\YunXiBot` → `~/.yunxi-bot`），
/// 但只要有一处漏了对齐，表现就是**最难查的那类错**：
/// sidecar 说"没配置"，而使用者的配置文件明明就在那儿。
///
/// 端到端测试抓到过一次真实的路径不一致。修法不是"让两边神奇地一致"
/// ——那不可靠；而是**让不一致可被看见**（见 `info::mail` 里 health
/// 的路径比对）。但源头仍然应该只有一份。
///
/// 与 `sidecar/mail_server.py` 的 `default_home()` 必须是同一套规则，
/// 靠 `YUNXI_BOT_HOME` 对齐。Python 那边改不动这个事实，
/// 所以 Rust 侧至少要保证自己不重复。
pub fn default_home() -> std::path::PathBuf {
    if let Ok(v) = std::env::var("YUNXI_BOT_HOME")
        && !v.trim().is_empty()
    {
        return std::path::PathBuf::from(v);
    }
    if cfg!(windows)
        && let Ok(la) = std::env::var("LOCALAPPDATA")
    {
        return std::path::PathBuf::from(la).join("YunXiBot");
    }
    let mut p = std::path::PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()));
    p.push(".yunxi-bot");
    p
}
