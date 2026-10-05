//! 任务域模型与状态机。
//!
//! 状态迁移是**显式白名单**：不在表内的迁移一律拒绝，而不是"看起来没问题就允许"。
//! 这是 fail-closed 在最基础一层的体现。

use serde::{Deserialize, Serialize};

/// 任务标识。
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct JobId(pub String);

impl JobId {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for JobId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// 任务状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    /// 到期但尚未开始
    Pending,
    /// 需要人工批准才能继续
    WaitingApproval,
    /// 正在执行
    Running,
    Succeeded,
    Failed,
}

impl JobState {
    /// 终态：不会再自动触发。
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed)
    }

    /// 该状态下任务此刻是否**可以**被调度执行。
    ///
    /// - `Running` 不可调度——这正是单飞的实现：上一次还没跑完，本轮不重入。
    /// - `WaitingApproval` 不可调度——等人批准。
    /// - 终态不可调度。
    pub fn is_schedulable(self) -> bool {
        matches!(self, Self::Pending)
    }
}

/// 合法状态迁移白名单。
///
/// 不在表内的组合一律非法。特别地：
/// - `WaitingApproval` 只能回到 `Pending`（经批准）或留在原地；
/// - `Running` 只能走向终态或回到 `Pending`（崩溃残留回收后重试）。
pub fn can_transition(from: JobState, to: JobState) -> bool {
    use JobState::*;
    matches!(
        (from, to),
        (Pending, WaitingApproval)
            | (Pending, Running)
            | (WaitingApproval, Pending)
            | (Running, Succeeded)
            | (Running, Failed)
            | (Running, Pending)
    )
}

/// 触发方式。`Manual` 之外的三种都是**主动触发**。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Trigger {
    /// 只在显式要求时运行。不会被任何自动调度触发。
    Manual,
    /// 固定间隔，单位秒。
    Every { seconds: u64 },
    /// 5 字段 cron：分 时 日 月 周。
    Cron { expr: String },
    /// 监听路径 mtime 变化。
    Watch { path: String },
}

/// 触发方式的中文短名，供展示使用。
impl Trigger {
    pub fn kind_name(&self) -> &'static str {
        match self {
            Trigger::Manual => "manual",
            Trigger::Every { .. } => "every",
            Trigger::Cron { .. } => "cron",
            Trigger::Watch { .. } => "watch",
        }
    }
}

/// 任务定义（不可变部分）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobSpec {
    pub name: String,
    /// 要执行的命令，**argv 形式**，不经 shell 展开。
    ///
    /// 刻意不支持字符串命令：没有拼接就没有注入面，也让策略层能在
    /// argv 粒度上做判定。
    pub command: Vec<String>,
    pub cwd: String,
    pub trigger: Trigger,
    /// `true` 表示该任务会产生**不可逆副作用**（发送、删除、支付、发布…）。
    ///
    /// 置位后：每次运行前都需要人工批准（见 `policy`），且批准不参与降级。
    #[serde(default)]
    pub irreversible: bool,
    #[serde(default = "default_max_attempts")]
    pub max_attempts: u32,
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
}

fn default_max_attempts() -> u32 {
    1
}
fn default_timeout_ms() -> u64 {
    60_000
}

/// 任务运行时状态（由台账投影得出，可随时丢弃重建）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Job {
    pub id: JobId,
    pub spec: JobSpec,
    pub state: JobState,
    pub attempts: u32,
    /// 累计批准次数。不可逆任务要求 `approvals > attempts` 才允许运行。
    pub approvals: u32,
    pub created_at: u64,
    pub updated_at: u64,
    pub last_run_at: Option<u64>,
    pub last_success_at: Option<u64>,
    /// 上一次因单飞被跳过的时刻。
    ///
    /// 这是**纯观测字段**：单飞跳过不改变 `state`，因为"任务仍在运行"
    /// 这个事实不能被一次跳过覆盖掉。
    pub last_skipped_at: Option<u64>,
    pub last_error: Option<String>,
    /// `Watch` 触发器上次观测到的 mtime，避免同一变更重复触发。
    pub watch_mtime_ms: Option<u64>,
}

impl Job {
    /// 由定义构造初始状态。
    ///
    /// **不可逆任务创建即进入待批准**——这是 fail-closed 的入口：
    /// 不存在"先建了再说"的窗口。
    pub fn new(id: JobId, spec: JobSpec, at: u64) -> Self {
        let state = if spec.irreversible {
            JobState::WaitingApproval
        } else {
            JobState::Pending
        };
        Self {
            id,
            spec,
            state,
            attempts: 0,
            approvals: 0,
            created_at: at,
            updated_at: at,
            last_run_at: None,
            last_success_at: None,
            last_skipped_at: None,
            last_error: None,
            watch_mtime_ms: None,
        }
    }

    /// 该任务此刻是否需要人工批准才能运行。
    ///
    /// - 不可逆任务：**每次运行都要批准**（`approvals <= attempts` 即需批准）。
    ///   这样"批准一次然后长期自动跑"是不可能的——不可逆动作不给你养成习惯。
    /// - 其他任务：仅当策略判定为 `ask` 且从未批准过时需要。
    pub fn needs_approval(&self, policy_wants_ask: bool) -> bool {
        if self.spec.irreversible {
            self.approvals <= self.attempts
        } else {
            policy_wants_ask && self.approvals == 0
        }
    }

    /// 还有重试额度吗。
    pub fn has_retry_budget(&self) -> bool {
        self.attempts < self.spec.max_attempts
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(irreversible: bool) -> JobSpec {
        JobSpec {
            name: "t".into(),
            command: vec!["echo".into(), "hi".into()],
            cwd: ".".into(),
            trigger: Trigger::Manual,
            irreversible,
            max_attempts: 2,
            timeout_ms: 1000,
        }
    }

    #[test]
    fn running_is_not_schedulable_which_is_how_single_flight_works() {
        // 单飞不是靠额外开关，而是靠"Running 不可调度"这一条
        assert!(!JobState::Running.is_schedulable());
        assert!(!JobState::WaitingApproval.is_schedulable());
        assert!(!JobState::Succeeded.is_schedulable());
        assert!(!JobState::Failed.is_schedulable());
        assert!(JobState::Pending.is_schedulable());
    }

    #[test]
    fn skip_does_not_touch_state() {
        // 单飞跳过只写观测字段，不得把 Running 覆盖成别的状态
        let mut j = Job::new(JobId::new("a"), spec(false), 0);
        j.state = JobState::Running;
        let before = j.state;
        j.last_skipped_at = Some(123);
        assert_eq!(j.state, before, "跳过不得改变状态");
    }

    #[test]
    fn irreversible_starts_waiting_approval() {
        let j = Job::new(JobId::new("a"), spec(true), 0);
        assert_eq!(j.state, JobState::WaitingApproval);
    }

    #[test]
    fn reversible_starts_pending() {
        let j = Job::new(JobId::new("a"), spec(false), 0);
        assert_eq!(j.state, JobState::Pending);
    }

    #[test]
    fn irreversible_requires_approval_every_run() {
        let mut j = Job::new(JobId::new("a"), spec(true), 0);
        j.state = JobState::Pending;
        assert!(j.needs_approval(false), "首次运行需批准");

        j.approvals = 1;
        assert!(!j.needs_approval(false), "批准后可运行一次");

        // 跑过一次之后，又需要新的批准
        j.attempts = 1;
        assert!(j.needs_approval(false), "不可逆动作每次都要重新批准");
    }

    #[test]
    fn illegal_transitions_are_rejected() {
        assert!(!can_transition(JobState::Succeeded, JobState::Running));
        assert!(!can_transition(JobState::Failed, JobState::Succeeded));
        assert!(can_transition(JobState::Running, JobState::Pending));
    }
}
