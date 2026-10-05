//! 任务域模型与状态机。
//!
//! 状态迁移是**显式白名单**：不在表内的迁移一律拒绝，而不是"看起来没问题就允许"。
//! 这是 fail-closed 在最基础一层的体现。

use serde::{Deserialize, Serialize};

use crate::policy::ApprovalPolicy;

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
///
/// **注意这一层最容易搞混的地方**：`Succeeded` / `Failed` 描述的是**上一次运行的
/// 结果**，不是任务的生命周期终点。周期性任务跑成功一次之后，明天还得再跑。
///
/// 真正的终态只有 [`JobState::Disabled`]——连续失败超限被停用，需要人工重置。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    /// 等待触发
    Pending,
    /// 需要人工批准才能继续
    WaitingApproval,
    /// 正在执行
    Running,
    /// 上次运行成功（任务仍会被触发）
    Succeeded,
    /// 上次运行失败，但仍在连续失败预算内（任务仍会被触发）
    Failed,
    /// 连续失败超限，已停用。**需人工重置**。这是唯一的终态。
    Disabled,
}

impl JobState {
    /// 终态：不会再自动触发，需要人工干预。
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Disabled)
    }

    /// 该状态下任务此刻是否**可以**被调度执行。
    ///
    /// - `Running` 不可调度——这正是单飞的实现：上一次还没跑完，本轮不重入。
    /// - `WaitingApproval` 不可调度——等人批准。
    /// - `Disabled` 不可调度——需人工重置。
    pub fn is_schedulable(self) -> bool {
        matches!(self, Self::Pending | Self::Succeeded | Self::Failed)
    }
}

/// 合法状态迁移白名单。
///
/// 不在表内的组合一律非法。特别地：
/// - `WaitingApproval` 只能回到 `Pending`（经批准）；
/// - `Running` 只能走向运行结果或回到 `Pending`（崩溃残留回收）；
/// - `Succeeded` / `Failed` 之间可以互相迁移（下一次运行结果）；
/// - `Disabled` 只能通过人工重置回到 `Pending`。
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
            | (Succeeded, Running)
            | (Failed, Running)
            | (Succeeded, WaitingApproval)
            | (Failed, WaitingApproval)
            | (Succeeded, Disabled)
            | (Failed, Disabled)
            | (Disabled, Pending)
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
    /// **连续**失败次数。成功一次即清零。
    ///
    /// 判定是否停用任务看的是它，而不是累计 `attempts`：周期性任务被一次
    /// 网络抖动打死，对常驻 Agent 是不可接受的。
    pub consecutive_failures: u32,
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
            consecutive_failures: 0,
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
    /// **只有不可逆任务需要批准。** 普通任务不需要——这是 ADR D4/D5 的本意：
    /// `approval` 旋钮约束的是"需要批准的动作"如何处置，而不是把所有动作
    /// 都变成需要批准。弄反了会让常驻 Agent 什么都干不了。
    ///
    /// 不可逆任务要求 `approvals > attempts`，即**每次运行都要重新批准**。
    pub fn requires_approval(&self) -> bool {
        self.spec.irreversible && self.approvals <= self.attempts
    }

    /// 还能继续尝试吗。
    ///
    /// 依据**连续失败**次数，而不是累计次数——周期性任务不应被一次
    /// 偶发失败判定为永久停用。
    pub fn has_retry_budget(&self) -> bool {
        self.consecutive_failures < self.spec.max_attempts
    }
}

/// 批准门禁的判定结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateDecision {
    /// 直接执行。
    Run,
    /// 转入待批准，等人。
    NeedsApproval,
    /// `never` 策略下自动拒绝——注意**不是自动放行**。
    AutoRejected,
}

impl GateDecision {
    /// 门外语义的一句话解释，用于台账与展示。
    pub fn reason(self, irreversible: bool) -> &'static str {
        match self {
            GateDecision::Run => "无需批准，直接执行",
            GateDecision::NeedsApproval => {
                if irreversible {
                    "任务被标记为不可逆：每次运行都需要人工批准"
                } else {
                    "该动作需要人工批准"
                }
            }
            GateDecision::AutoRejected => "审批策略为 never：需要批准的动作被自动拒绝（不是放行）",
        }
    }
}

/// 决定一个任务此刻该走哪条路。
///
/// `approval` 旋钮只影响"需要批准的动作"如何处置：
/// - `Ask` → 转待批准（失败方向朝不执行）；
/// - `Never` → 自动拒绝（**不是**自动放行）。
pub fn gate(job: &Job, approval: ApprovalPolicy) -> GateDecision {
    if !job.requires_approval() {
        return GateDecision::Run;
    }
    match approval {
        ApprovalPolicy::Ask => GateDecision::NeedsApproval,
        ApprovalPolicy::Never => GateDecision::AutoRejected,
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
        assert!(!JobState::Disabled.is_schedulable());
        assert!(JobState::Pending.is_schedulable());
    }

    #[test]
    fn run_outcome_does_not_kill_a_periodic_job() {
        // 这是修掉的核心缺陷：Succeeded / Failed 描述的是【上一次运行结果】，
        // 不是任务的生命周期终点。否则周期性任务跑一次就永久死了。
        assert!(JobState::Succeeded.is_schedulable(), "成功过还要再跑");
        assert!(JobState::Failed.is_schedulable(), "失败过还要再试");
        assert!(!JobState::Succeeded.is_terminal());
        assert!(!JobState::Failed.is_terminal());
        // 唯一的终态是"连续失败超限被停用"
        assert!(JobState::Disabled.is_terminal());
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
        assert!(j.requires_approval(), "首次运行需批准");

        j.approvals = 1;
        assert!(!j.requires_approval(), "批准后可运行一次");

        // 跑过一次之后，又需要新的批准
        j.attempts = 1;
        assert!(j.requires_approval(), "不可逆动作每次都要重新批准");
    }

    #[test]
    fn ordinary_job_never_requires_approval() {
        // 这是修掉的缺陷：曾经只要策略是 ask 就让所有任务都要批准，
        // 结果常驻 Agent 什么都干不了
        let j = Job::new(JobId::new("a"), spec(false), 0);
        assert!(!j.requires_approval());
        assert_eq!(gate(&j, ApprovalPolicy::Ask), GateDecision::Run);
        assert_eq!(gate(&j, ApprovalPolicy::Never), GateDecision::Run);
    }

    #[test]
    fn never_policy_auto_rejects_rather_than_allows() {
        // ADR D4：never 的含义是「需要批准的动作自动拒绝」，不是自动放行
        let j = Job::new(JobId::new("a"), spec(true), 0);
        assert_eq!(gate(&j, ApprovalPolicy::Ask), GateDecision::NeedsApproval);
        assert_eq!(gate(&j, ApprovalPolicy::Never), GateDecision::AutoRejected);
    }

    #[test]
    fn illegal_transitions_are_rejected() {
        // 停用是终态：不能自己爬回来，必须经人工重置到 Pending
        assert!(!can_transition(JobState::Disabled, JobState::Running));
        assert!(!can_transition(JobState::Disabled, JobState::Succeeded));
        assert!(can_transition(JobState::Disabled, JobState::Pending));
        // 运行结果之间不能直接互跳，必须再次经过 Running
        assert!(!can_transition(JobState::Succeeded, JobState::Failed));
        // 崩溃残留回收：Running 可以回到 Pending
        assert!(can_transition(JobState::Running, JobState::Pending));
        // 周期性任务：上次成功之后仍要能被再次触发
        assert!(can_transition(JobState::Succeeded, JobState::Running));
    }
}
