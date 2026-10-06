//! 任务拆解、决策点与执行。
//!
//! 数据结构与状态机在 [`model`]，拆解在 [`plan`]，决策点在 [`decide`]，
//! 执行循环在 [`engine`]。

pub mod decide;
pub mod engine;
pub mod model;
pub mod plan;
pub mod schedule;

pub use decide::{TaskDecision, decision_point, options_request, parse_options};
pub use engine::{Advance, Engine, EngineOutcome};
pub use model::{
    Budget, BudgetLedger, RoutingRecord, Step, StepState, Task, TaskError, TaskSet, TaskState,
    validate_dependencies,
};
pub use plan::{PlannedStep, parse_plan, plan_prompt, planner_request};
