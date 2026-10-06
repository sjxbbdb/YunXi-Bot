//! 任务执行框架：把"人类给一个目标"变成"一组有序执行的步骤"。
//!
//! ## 链路
//!
//! ```text
//! 人类输入任务
//!      ↓
//! 【拆解】把系统提示词 + 人格 + 任务一并发给模型 → 得到步骤清单（可带依赖）
//!      ↓
//! 【逐个执行】取依赖已满足的步骤（可并行的是一批）
//!      ↓        ↑
//!      │        └── 完成一个，回填结果
//!      ↓
//! 【决策点】模型产生选项 → 本地决策模型选一个
//!      │                           │
//!      │                           └─ 它弃权 → 升级为人工介入
//!      ↓
//! 【循环】直到所有步骤完成
//!      ↓
//! 【汇总】把各步结果合成一个答复
//! ```
//!
//! ## 三条设计约束
//!
//! 1. **台账是唯一事实来源。** 任务的每一步状态都写台账，进程重启后从台账投影
//!    恢复。内存里的 `Task` 只是台账的视图，不是真相。
//! 2. **每个任务路由一次。** 拆解本身是一个任务，每个步骤各自也是一个任务——
//!    各自按自己的类型和调用次数选模型。**但一个任务执行中途不换模型**，
//!    因为那会把已经构建好的前缀缓存全部作废。
//! 3. **预算是硬上限。** 模型调用次数用尽就停，不"再试一次"。
//!    失控的循环比失败的任务贵得多。
//!
//! ## 为什么状态机单独写
//!
//! "哪些步骤现在可以跑"是一个**纯函数**：给一组步骤状态，返回可执行集合。
//! 把它写成不依赖模型、不依赖 IO 的纯函数，就能在没有网络的情况下测出
//! 拓扑正确性——而这正是最容易写错的地方（死锁、漏依赖、环）。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::CoreError;
use crate::think::{Routing, TaskKind};

/// 一个任务。**`goal` 是人类原话**，不改写——改写会丢掉意图。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    /// 人类原话。
    pub goal: String,
    #[serde(default)]
    pub steps: Vec<Step>,
    pub state: TaskState,
    #[serde(default)]
    pub created_at: u64,
    /// 路由决定（模型 + 思考模式 + 理由）。拆解完成后填入。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub routing: Option<RoutingRecord>,
}

impl Task {
    pub fn new(id: impl Into<String>, goal: impl Into<String>, created_at: u64) -> Self {
        Self {
            id: id.into(),
            goal: goal.into(),
            steps: Vec::new(),
            state: TaskState::Planning,
            created_at,
            routing: None,
        }
    }

    pub fn step(&self, id: &str) -> Option<&Step> {
        self.steps.iter().find(|s| s.id == id)
    }

    pub fn step_mut(&mut self, id: &str) -> Option<&mut Step> {
        self.steps.iter_mut().find(|s| s.id == id)
    }

    /// 所有步骤都已终态（完成/失败/跳过）。
    pub fn all_settled(&self) -> bool {
        !self.steps.is_empty() && self.steps.iter().all(|s| s.state.is_settled())
    }

    /// 汇总：完成/失败/跳过各几个。
    pub fn tally(&self) -> (usize, usize, usize) {
        let done = self
            .steps
            .iter()
            .filter(|s| s.state == StepState::Succeeded)
            .count();
        let failed = self
            .steps
            .iter()
            .filter(|s| s.state == StepState::Failed)
            .count();
        let skipped = self
            .steps
            .iter()
            .filter(|s| s.state == StepState::Skipped)
            .count();
        (done, failed, skipped)
    }
}

/// 任务状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    /// 正在拆解。
    Planning,
    /// 有步骤可跑。
    Running,
    /// 卡住了：没有可跑的步骤，但也没全部终态。
    ///
    /// **这个状态必须存在。** 没有它，"依赖成环"和"还在跑"会长得一模一样，
    /// 于是死锁表现为无限等待。
    Stalled,
    /// 等人工：决策模型弃权，或某步需要批准。
    AwaitingHuman,
    Done,
    Failed,
    Cancelled,
}

impl TaskState {
    pub fn label(self) -> &'static str {
        match self {
            TaskState::Planning => "拆解中",
            TaskState::Running => "执行中",
            TaskState::Stalled => "卡住",
            TaskState::AwaitingHuman => "等人工",
            TaskState::Done => "完成",
            TaskState::Failed => "失败",
            TaskState::Cancelled => "已取消",
        }
    }

    /// 终态：不会再变。
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            TaskState::Done | TaskState::Failed | TaskState::Cancelled
        )
    }

    /// 允许的迁移。**非法迁移一律报错，不静默接受。**
    pub fn can_go_to(self, to: TaskState) -> bool {
        use TaskState::*;
        match (self, to) {
            // 终态不动
            (a, b) if a == b => true,
            (a, _) if a.is_terminal() => false,
            (Planning, Running | Stalled | Failed | Cancelled | AwaitingHuman) => true,
            (Running, Running | Stalled | AwaitingHuman | Done | Failed | Cancelled) => true,
            (Stalled, Running | AwaitingHuman | Failed | Cancelled) => true,
            (AwaitingHuman, Running | Failed | Cancelled) => true,
            _ => false,
        }
    }
}

/// 一个步骤。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Step {
    pub id: String,
    /// 这一步要干什么。给模型看的就是它。
    pub instruction: String,
    /// 依赖的步骤 id。**空表示第一步就能跑。**
    #[serde(default)]
    pub depends_on: Vec<String>,
    /// 这一步属于哪种任务——**决定要不要开思考**。
    pub kind: TaskKind,
    pub state: StepState,
    /// 已经尝试了几次。
    #[serde(default)]
    pub attempts: u32,
    /// 这一步的结果（成功时是正文，失败时是错误说明）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
    /// 这一步实际的路由决定。**留痕是为了事后能解释账单。**
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub routing: Option<RoutingRecord>,
}

impl Step {
    pub fn new(id: impl Into<String>, instruction: impl Into<String>, kind: TaskKind) -> Self {
        Self {
            id: id.into(),
            instruction: instruction.into(),
            depends_on: Vec::new(),
            kind,
            state: StepState::Pending,
            attempts: 0,
            result: None,
            routing: None,
        }
    }

    pub fn with_depends_on(mut self, deps: Vec<String>) -> Self {
        self.depends_on = deps;
        self
    }
}

/// 步骤状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepState {
    Pending,
    /// 已选中，正在执行。
    Running,
    Succeeded,
    Failed,
    /// 依赖失败，被跳过。**和"失败"不同**：它自己没出错。
    Skipped,
    /// 等人工批准。
    Blocked,
}

impl StepState {
    pub fn label(self) -> &'static str {
        match self {
            StepState::Pending => "待执行",
            StepState::Running => "执行中",
            StepState::Succeeded => "成功",
            StepState::Failed => "失败",
            StepState::Skipped => "已跳过",
            StepState::Blocked => "待批准",
        }
    }

    pub fn is_settled(self) -> bool {
        matches!(
            self,
            StepState::Succeeded | StepState::Failed | StepState::Skipped
        )
    }
}

/// 路由留痕。**只存能解释账单的字段，不存整个 [`Routing`]。**
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RoutingRecord {
    pub provider: String,
    pub model: String,
    pub kind: TaskKind,
    pub thinking: bool,
    pub estimated_calls: usize,
    pub reason: String,
}

impl RoutingRecord {
    pub fn from_routing(r: &Routing) -> Self {
        Self {
            provider: r.spec.provider.to_string(),
            model: r.spec.model.to_string(),
            kind: r.kind,
            thinking: r.thinking,
            estimated_calls: r.estimated_calls,
            reason: r.reason.clone(),
        }
    }
}

/// 任务执行预算。**硬上限，用尽即停。**
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Budget {
    /// 允许的模型调用次数上限。
    pub max_model_calls: u32,
    /// 允许的步骤数上限（防止拆解失控）。
    pub max_steps: u32,
    /// 单步最大尝试次数。
    pub max_attempts_per_step: u32,
}

impl Default for Budget {
    fn default() -> Self {
        Self {
            // 默认 20 次：够一个十来步的任务跑完，又不至于失控烧配额
            max_model_calls: 20,
            max_steps: 12,
            max_attempts_per_step: 2,
        }
    }
}

/// 预算账本。**每次调用前先扣，扣不动就不调用。**
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BudgetLedger {
    budget: Budget,
    used_model_calls: u32,
}

impl BudgetLedger {
    pub fn new(budget: Budget) -> Self {
        Self {
            budget,
            used_model_calls: 0,
        }
    }

    pub fn budget(&self) -> Budget {
        self.budget
    }

    pub fn used(&self) -> u32 {
        self.used_model_calls
    }

    pub fn remaining(&self) -> u32 {
        self.budget
            .max_model_calls
            .saturating_sub(self.used_model_calls)
    }

    /// 申请一次调用。够就扣，不够就拒绝——**不排队、不等待、不重试**。
    pub fn try_charge(&mut self) -> Result<(), TaskError> {
        if self.remaining() == 0 {
            return Err(TaskError::BudgetExhausted {
                limit: self.budget.max_model_calls,
            });
        }
        self.used_model_calls += 1;
        Ok(())
    }

    /// 拆解出来的步骤数是否超限。
    pub fn steps_allowed(&self, n: usize) -> Result<(), TaskError> {
        if n as u32 > self.budget.max_steps {
            return Err(TaskError::TooManySteps {
                got: n as u32,
                limit: self.budget.max_steps,
            });
        }
        Ok(())
    }
}

/// 执行框架的错误。
#[derive(Debug)]
pub enum TaskError {
    Core(CoreError),
    /// 预算用尽。
    BudgetExhausted {
        limit: u32,
    },
    /// 拆解出来的步骤太多。
    TooManySteps {
        got: u32,
        limit: u32,
    },
    /// 模型返回的步骤清单无法解析。**带上原文**，否则没法排查。
    UnparsablePlan {
        detail: String,
        raw: String,
    },
    /// 依赖里引用了不存在的步骤。
    UnknownDependency {
        step: String,
        dep: String,
    },
    /// 依赖成环。
    CircularDependency {
        steps: Vec<String>,
    },
    /// 单步重试次数用尽。
    StepFailed {
        step: String,
        reason: String,
    },
    /// 需要人工介入（决策模型弃权 / 需要批准）。
    NeedsHuman {
        reason: String,
    },
    /// 任务卡住：没有可跑的步骤，也没全部终态。
    Stalled {
        remaining: Vec<String>,
    },
}

impl std::fmt::Display for TaskError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TaskError::Core(e) => write!(f, "{e}"),
            TaskError::BudgetExhausted { limit } => {
                write!(f, "模型调用预算用尽（上限 {limit} 次），任务停在原地")
            }
            TaskError::TooManySteps { got, limit } => {
                write!(f, "拆解出 {got} 个步骤，超过上限 {limit}")
            }
            TaskError::UnparsablePlan { detail, raw } => {
                let head: String = raw.chars().take(200).collect();
                write!(f, "无法解析步骤清单: {detail}；原文开头: {head}")
            }
            TaskError::UnknownDependency { step, dep } => {
                write!(f, "步骤 {step} 依赖了不存在的步骤 {dep}")
            }
            TaskError::CircularDependency { steps } => {
                write!(f, "步骤依赖成环: {}", steps.join(" -> "))
            }
            TaskError::StepFailed { step, reason } => {
                write!(f, "步骤 {step} 失败且重试次数用尽: {reason}")
            }
            TaskError::NeedsHuman { reason } => write!(f, "需要人工介入: {reason}"),
            TaskError::Stalled { remaining } => write!(
                f,
                "任务卡住：剩余步骤没有可执行的（{}）",
                remaining.join(", ")
            ),
        }
    }
}

impl std::error::Error for TaskError {}

impl From<CoreError> for TaskError {
    fn from(e: CoreError) -> Self {
        TaskError::Core(e)
    }
}

impl TaskError {
    /// 这个错误该不该升级为"等人工"。
    ///
    /// **失败方向朝「不执行」**：预算用尽、成环、卡住、解析失败都停下等人，
    /// 只有单步执行失败是可以自己重试的。
    pub fn needs_human(&self) -> bool {
        match self {
            TaskError::StepFailed { .. } | TaskError::Core(_) => false,
            TaskError::NeedsHuman { .. } => true,
            _ => true,
        }
    }
}

/// 投影出来的任务集合。
pub type TaskSet = BTreeMap<String, Task>;

/// 检查依赖图是否合法：**引用的步骤都存在，且没有环**。
///
/// 这是纯函数，不碰 IO。成环必须在执行前查出来——否则它会表现为"一直在等"，
/// 而不是一个错误。
pub fn validate_dependencies(steps: &[Step]) -> Result<(), TaskError> {
    let ids: std::collections::BTreeSet<&str> = steps.iter().map(|s| s.id.as_str()).collect();

    for s in steps {
        for dep in &s.depends_on {
            if !ids.contains(dep.as_str()) {
                return Err(TaskError::UnknownDependency {
                    step: s.id.clone(),
                    dep: dep.clone(),
                });
            }
            if dep == &s.id {
                return Err(TaskError::CircularDependency {
                    steps: vec![s.id.clone(), s.id.clone()],
                });
            }
        }
    }

    // Kahn 拓扑排序：能排完就没有环。
    // 边的方向是 dep -> s，所以入度就是每个步骤的 depends_on 数量。
    let mut deg: BTreeMap<&str, usize> = steps.iter().map(|s| (s.id.as_str(), 0)).collect();
    for s in steps {
        for _ in &s.depends_on {
            if let Some(d) = deg.get_mut(s.id.as_str()) {
                *d += 1;
            }
        }
    }
    let mut queue: Vec<&str> = deg
        .iter()
        .filter(|(_, d)| **d == 0)
        .map(|(k, _)| *k)
        .collect();
    let mut seen = 0usize;
    while let Some(id) = queue.pop() {
        seen += 1;
        // 所有依赖 id 的步骤，度数减一
        for s in steps {
            if s.depends_on.iter().any(|d| d == id)
                && let Some(d) = deg.get_mut(s.id.as_str())
            {
                *d -= 1;
                if *d == 0 {
                    queue.push(s.id.as_str());
                }
            }
        }
    }

    if seen != steps.len() {
        let remaining: Vec<String> = steps
            .iter()
            .filter(|s| deg.get(s.id.as_str()).copied().unwrap_or(1) > 0)
            .map(|s| s.id.clone())
            .collect();
        return Err(TaskError::CircularDependency { steps: remaining });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(id: &str, deps: &[&str]) -> Step {
        Step::new(id, format!("做 {id}"), TaskKind::Generation)
            .with_depends_on(deps.iter().map(|s| s.to_string()).collect())
    }

    #[test]
    fn terminal_states_never_move() {
        for t in [TaskState::Done, TaskState::Failed, TaskState::Cancelled] {
            assert!(t.is_terminal());
            for to in [
                TaskState::Running,
                TaskState::Stalled,
                TaskState::AwaitingHuman,
            ] {
                assert!(!t.can_go_to(to), "{t:?} 不该能变成 {to:?}");
            }
        }
    }

    #[test]
    fn running_can_settle_but_not_replan_from_done() {
        assert!(TaskState::Running.can_go_to(TaskState::Done));
        assert!(TaskState::Running.can_go_to(TaskState::Stalled));
        // 拆解中不能直接跳到完成——必须先跑
        assert!(!TaskState::Planning.can_go_to(TaskState::Done));
    }

    #[test]
    fn same_state_is_always_allowed() {
        for t in [
            TaskState::Planning,
            TaskState::Running,
            TaskState::Stalled,
            TaskState::AwaitingHuman,
            TaskState::Done,
        ] {
            assert!(t.can_go_to(t), "{t:?} 幂等更新应允许");
        }
    }

    #[test]
    fn budget_is_hard_capped() {
        let mut b = BudgetLedger::new(Budget {
            max_model_calls: 2,
            ..Default::default()
        });
        assert_eq!(b.remaining(), 2);
        assert!(b.try_charge().is_ok());
        assert!(b.try_charge().is_ok());
        assert_eq!(b.remaining(), 0);
        // 第三次必须被拒，不能"再试一次"
        match b.try_charge() {
            Err(TaskError::BudgetExhausted { limit }) => assert_eq!(limit, 2),
            other => panic!("应被预算拒绝，实际 {other:?}"),
        }
        assert_eq!(b.used(), 2, "被拒的调用不该扣费");
    }

    #[test]
    fn too_many_steps_is_rejected() {
        let b = BudgetLedger::new(Budget {
            max_steps: 3,
            ..Default::default()
        });
        assert!(b.steps_allowed(3).is_ok());
        assert!(matches!(
            b.steps_allowed(4),
            Err(TaskError::TooManySteps { got: 4, limit: 3 })
        ));
    }

    #[test]
    fn dependency_graph_accepts_a_dag() {
        let steps = vec![step("a", &[]), step("b", &["a"]), step("c", &["a", "b"])];
        assert!(validate_dependencies(&steps).is_ok());
    }

    #[test]
    fn dependency_graph_rejects_a_cycle() {
        // 成环必须报错，不能表现为"一直在等"
        let steps = vec![step("a", &["c"]), step("b", &["a"]), step("c", &["b"])];
        match validate_dependencies(&steps) {
            Err(TaskError::CircularDependency { steps }) => {
                assert_eq!(steps.len(), 3, "三个都该被点名: {steps:?}")
            }
            other => panic!("应报成环，实际 {other:?}"),
        }
    }

    #[test]
    fn dependency_graph_rejects_unknown_step() {
        let steps = vec![step("a", &["nope"])];
        match validate_dependencies(&steps) {
            Err(TaskError::UnknownDependency { dep, .. }) => assert_eq!(dep, "nope"),
            other => panic!("应报未知依赖，实际 {other:?}"),
        }
    }

    #[test]
    fn dependency_graph_rejects_self_loop() {
        let steps = vec![step("a", &["a"])];
        assert!(matches!(
            validate_dependencies(&steps),
            Err(TaskError::CircularDependency { .. })
        ));
    }

    #[test]
    fn tally_counts_settled_states() {
        let mut t = Task::new("t1", "目标", 0);
        let mut a = step("a", &[]);
        a.state = StepState::Succeeded;
        let mut b = step("b", &["a"]);
        b.state = StepState::Failed;
        let mut c = step("c", &["a"]);
        c.state = StepState::Skipped;
        t.steps = vec![a, b, c];
        assert!(t.all_settled());
        assert_eq!(t.tally(), (1, 1, 1));
    }

    #[test]
    fn empty_task_is_not_settled() {
        // 空步骤集的"全部完成"是个陷阱：没有步骤的任务不该算完成
        let t = Task::new("t1", "目标", 0);
        assert!(!t.all_settled());
    }

    #[test]
    fn errors_that_need_a_human_are_marked() {
        // 预算用尽、成环、卡住、解析失败 → 停下等人
        assert!(TaskError::BudgetExhausted { limit: 1 }.needs_human());
        assert!(
            TaskError::CircularDependency {
                steps: vec!["a".into()]
            }
            .needs_human()
        );
        assert!(TaskError::Stalled { remaining: vec![] }.needs_human());
        assert!(
            TaskError::UnparsablePlan {
                detail: "x".into(),
                raw: "y".into()
            }
            .needs_human()
        );
        assert!(TaskError::NeedsHuman { reason: "x".into() }.needs_human());
        // 单步失败可以先自己重试
        assert!(
            !TaskError::StepFailed {
                step: "a".into(),
                reason: "超时".into()
            }
            .needs_human()
        );
    }

    #[test]
    fn unparsable_plan_error_keeps_the_raw_text() {
        // 不带原文的解析错误没法排查
        let e = TaskError::UnparsablePlan {
            detail: "不是 JSON".into(),
            raw: "我建议这样：".into(),
        };
        let s = e.to_string();
        assert!(s.contains("不是 JSON"), "{s}");
        assert!(s.contains("我建议这样"), "必须带上原文开头: {s}");
    }

    #[test]
    fn routing_record_keeps_what_explains_the_bill() {
        use crate::think::{ModelSpec, ReasoningEffort, TaskProfile, Thinking};
        let router = crate::think::ModelRouter::default();
        let r = router.route(
            "分析这两个方案的利弊，需要多步",
            TaskKind::Analysis,
            &TaskProfile {
                prompt_chars: 200,
                step_count: 6,
                ..Default::default()
            },
            ReasoningEffort::Auto,
            None,
        );
        let rec = RoutingRecord::from_routing(&r);
        assert_eq!(rec.provider, "deepseek");
        assert_eq!(rec.model, "deepseek-flash");
        assert!(rec.thinking);
        assert!(!rec.reason.is_empty());
        // 落盘的 provider/model 必须与端点常量一致，别写死字符串
        assert_eq!(rec.model, ModelSpec::DEEPSEEK_FLASH.model);
        assert_eq!(r.thinking_field(), Thinking::Enabled);
    }
}
