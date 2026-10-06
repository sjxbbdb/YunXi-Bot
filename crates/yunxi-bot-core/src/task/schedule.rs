//! 任务调度：守护进程该推进哪些任务，以及哪些任务在等人。
//!
//! ## 这个模块补的是 goal 开头点名的第一条断链
//!
//! > 任务卡住后**没有任何东西自动推进它**
//!
//! 在此之前，任务只能由 `yunxi-bot do` 同步跑完；进程被杀、或者跑到
//! 「等人工」之后，**唯一的推进方式是人在终端敲 `resume`**。
//! 对"常驻助理"这个定位，那意味着任务链路的"常驻"是不成立的。
//!
//! ## 两条必须守住的边界
//!
//! **一、不碰 `AwaitingHuman`。**
//!
//! 那个状态的含义是"决策模型弃权了，或某一步需要批准"——
//! **自动续跑它等于自动批准**，而那正是审批门禁存在的理由。
//! 所以这个模块只会把这类任务**报出来**（[`attention_needed`]），
//! 让使用者知道有事等他，而不是替他做决定。
//!
//! 同理，守护进程的审批者必须是"一律拒绝"：**无人值守就不该获得新权限**。
//! 那条约束不在这里，在调用方的构造里——但它是这一层能安全存在的前提。
//!
//! **二、不碰"刚刚还在动"的任务。**
//!
//! 使用者在终端里跑一个任务时，守护进程也在看同一本台账。
//! 没有这道闸，两边会同时执行同一批步骤——而步骤可能有副作用（写文件、
//! 跑命令）。所以候选必须**已经安静了一段时间**
//! （[`AutoAdvance::idle_ms`]）。
//!
//! 这不是"乐观锁"，是承认一件事：**没有租约机制时，时间是最简单的
//! 互斥手段**。真的要做并发控制，该由台账上的租约事件承担，
//! 而不是在这里假装做到了。

use std::collections::BTreeMap;

use crate::ledger::Event;
use crate::task::{Task, TaskState};

/// 自动推进的判定参数。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AutoAdvance {
    /// 任务安静多久之后才允许自动推进。
    ///
    /// 默认 120 秒：比任何一次正常的模型往返都长，又短到
    /// "进程被杀了"能在一个合理的时间内被接上。
    pub idle_ms: u64,
    pub now_ms: u64,
}

impl AutoAdvance {
    pub fn new(now_ms: u64) -> Self {
        Self {
            idle_ms: DEFAULT_IDLE_MS,
            now_ms,
        }
    }

    pub fn with_idle_ms(mut self, ms: u64) -> Self {
        self.idle_ms = ms;
        self
    }
}

/// 默认的安静时长。
pub const DEFAULT_IDLE_MS: u64 = 120_000;

/// 一个任务最后一次写进台账是什么时候。
///
/// 没有事件时返回 0——调用方要退回 `created_at`，**不能当成"刚刚"**
/// （那会让一个刚建的任务永远不被推进）。
pub fn last_activity_ms(events: &[Event], task_id: &str) -> u64 {
    events
        .iter()
        .filter(|e| e.data.get("task").and_then(|v| v.as_str()) == Some(task_id))
        .map(|e| e.at)
        .max()
        .unwrap_or(0)
}

/// 这个状态的任务能不能被自动推进。
///
/// 只有 `Planning` 和 `Running`。其余一律不动，理由见模块文档。
pub fn is_auto_advanceable(state: TaskState) -> bool {
    matches!(state, TaskState::Planning | TaskState::Running)
}

/// 守护进程可以自动推进的任务 id。**按 id 升序，保证可重复。**
///
/// 顺序稳定是有意的：一次跑多个任务时，日志读起来才不会每轮变样。
pub fn auto_advance_candidates(
    tasks: &BTreeMap<String, Task>,
    events: &[Event],
    cfg: AutoAdvance,
) -> Vec<String> {
    tasks
        .values()
        .filter(|t| is_auto_advanceable(t.state))
        .filter(|t| {
            let last = {
                let a = last_activity_ms(events, &t.id);
                // 没有事件就退回创建时间。**不能当成"刚刚"**——
                // 那会让一个刚建的任务永远不被推进。
                if a == 0 { t.created_at } else { a }
            };
            // 时钟回拨时 `saturating_sub` 给 0，于是"不够安静"——
            // 失败方向朝"不动手"，这是对的。
            cfg.now_ms.saturating_sub(last) >= cfg.idle_ms
        })
        .map(|t| t.id.clone())
        .collect()
}

/// 需要使用者介入的任务。**自动推进不该碰它们，但不该瞒着你。**
///
/// 两类：
///
/// - `AwaitingHuman`：决策模型弃权，或某一步需要批准
/// - `Stalled`：没有可跑的步骤，但也没全部终态（依赖成环，或某步失败后
///   依赖它的一批全被跳过）
///
/// 这两类任务在台账里静静躺着，而**使用者不会主动去查**。
/// 所以它们必须能被报出来——这正是"干了事不会告诉你"的另一面。
pub fn attention_needed(tasks: &BTreeMap<String, Task>) -> Vec<(&Task, &'static str)> {
    let mut out: Vec<(&Task, &'static str)> = tasks
        .values()
        .filter_map(|t| match t.state {
            TaskState::AwaitingHuman => Some((t, "在等你决定（自动推进不会替你批）")),
            TaskState::Stalled => Some((t, "卡住了：没有可跑的步骤，但也没全部结束")),
            _ => None,
        })
        .collect();
    out.sort_by(|a, b| a.0.id.cmp(&b.0.id));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::EventKind;
    use serde_json::json;

    fn task(id: &str, state: TaskState, created: u64) -> Task {
        Task {
            id: id.into(),
            goal: format!("目标 {id}"),
            steps: Vec::new(),
            state,
            created_at: created,
            routing: None,
        }
    }

    fn ev(at: u64, task_id: &str) -> Event {
        Event {
            seq: 1,
            at,
            kind: EventKind::StepRunning,
            span: None,
            job: None,
            data: json!({"task": task_id}),
        }
    }

    fn set(items: Vec<Task>) -> BTreeMap<String, Task> {
        items.into_iter().map(|t| (t.id.clone(), t)).collect()
    }

    // ---- 什么能推进 ----

    #[test]
    fn only_planning_and_running_are_advanceable() {
        // **只有这两个状态。** 其余一律不动，理由见模块文档。
        assert!(is_auto_advanceable(TaskState::Planning));
        assert!(is_auto_advanceable(TaskState::Running));
        for s in [
            TaskState::Stalled,
            TaskState::AwaitingHuman,
            TaskState::Done,
            TaskState::Failed,
            TaskState::Cancelled,
        ] {
            assert!(!is_auto_advanceable(s), "{s:?} 不该被自动推进");
        }
    }

    #[test]
    fn a_stalled_task_is_never_auto_advanced() {
        // 没有可跑的步骤——再跑一遍还是同样的结论，只会刷屏。
        // 它该被**报出来**（attention_needed），不是被反复重试。
        let tasks = set(vec![task("t1", TaskState::Stalled, 0)]);
        let got =
            auto_advance_candidates(&tasks, &[ev(1_000_000, "t1")], AutoAdvance::new(9_999_999));
        assert!(got.is_empty());
    }

    #[test]
    fn awaiting_human_is_never_auto_advanced() {
        // **自动续跑它等于自动批准。**
        // 那正是审批门禁存在的理由——无人值守不该获得新权限。
        let tasks = set(vec![task("t1", TaskState::AwaitingHuman, 0)]);
        let got =
            auto_advance_candidates(&tasks, &[ev(1_000_000, "t1")], AutoAdvance::new(9_999_999));
        assert!(got.is_empty(), "等人工的任务绝不能被自动推进");
    }

    #[test]
    fn terminal_tasks_are_skipped() {
        let tasks = set(vec![
            task("a", TaskState::Done, 0),
            task("b", TaskState::Failed, 0),
            task("c", TaskState::Cancelled, 0),
        ]);
        assert!(auto_advance_candidates(&tasks, &[], AutoAdvance::new(9_999_999)).is_empty());
    }

    // ---- 安静时长这道闸 ----

    #[test]
    fn a_freshly_active_task_is_left_alone() {
        // **这道闸防的是"守护进程和终端里的使用者抢同一个任务"。**
        // 两边同时执行同一批步骤，而步骤可能有副作用。
        let tasks = set(vec![task("t1", TaskState::Running, 0)]);
        let cfg = AutoAdvance::new(1_000_000).with_idle_ms(120_000);
        // 10 秒前还有活动
        let got = auto_advance_candidates(&tasks, &[ev(990_000, "t1")], cfg);
        assert!(got.is_empty(), "刚还在动的任务不该被抢");
    }

    #[test]
    fn an_idle_task_becomes_advanceable() {
        // "进程被杀了"该在一个合理时间内被接上
        let tasks = set(vec![task("t1", TaskState::Running, 0)]);
        let cfg = AutoAdvance::new(1_000_000).with_idle_ms(120_000);
        let got = auto_advance_candidates(&tasks, &[ev(800_000, "t1")], cfg);
        assert_eq!(got, vec!["t1".to_string()]);
    }

    #[test]
    fn exactly_at_the_idle_boundary_is_advanceable() {
        // 边界要明确：>= 而不是 >，否则"恰好 120 秒"永远差一轮
        let tasks = set(vec![task("t1", TaskState::Running, 0)]);
        let cfg = AutoAdvance::new(1_000_000).with_idle_ms(120_000);
        assert_eq!(
            auto_advance_candidates(&tasks, &[ev(880_000, "t1")], cfg),
            vec!["t1".to_string()]
        );
    }

    #[test]
    fn a_task_with_no_events_falls_back_to_created_at() {
        // **不能把"没有事件"当成"刚刚"。**
        // 那会让一个刚建就被放弃的任务永远不被推进。
        let tasks = set(vec![task("t1", TaskState::Planning, 0)]);
        let cfg = AutoAdvance::new(1_000_000).with_idle_ms(120_000);
        let got = auto_advance_candidates(&tasks, &[], cfg);
        assert_eq!(got, vec!["t1".to_string()]);
    }

    #[test]
    fn a_clock_rollback_makes_us_wait_not_act() {
        // 活动时间在未来（时钟回拨/机器时间错）→ saturating_sub 给 0
        // → "不够安静" → 不动手。**失败方向朝不动手。**
        let tasks = set(vec![task("t1", TaskState::Running, 0)]);
        let cfg = AutoAdvance::new(1_000).with_idle_ms(120_000);
        assert!(auto_advance_candidates(&tasks, &[ev(9_999_999, "t1")], cfg).is_empty());
    }

    #[test]
    fn last_activity_ignores_other_tasks() {
        // 别的任务在动，不该把我也算成"刚活动过"
        let events = vec![ev(900_000, "other"), ev(100_000, "mine")];
        assert_eq!(last_activity_ms(&events, "mine"), 100_000);
    }

    #[test]
    fn last_activity_takes_the_newest() {
        let events = vec![ev(100, "t"), ev(500, "t"), ev(300, "t")];
        assert_eq!(last_activity_ms(&events, "t"), 500);
    }

    #[test]
    fn last_activity_of_an_unknown_task_is_zero() {
        assert_eq!(last_activity_ms(&[ev(100, "t")], "nope"), 0);
    }

    // ---- 顺序与多条 ----

    #[test]
    fn candidates_are_sorted_by_id() {
        // 顺序稳定是有意的：日志读起来才不会每轮变样
        let tasks = set(vec![
            task("zeta", TaskState::Running, 0),
            task("alpha", TaskState::Running, 0),
            task("mid", TaskState::Running, 0),
        ]);
        let got = auto_advance_candidates(&tasks, &[], AutoAdvance::new(9_999_999));
        assert_eq!(got, vec!["alpha", "mid", "zeta"]);
    }

    #[test]
    fn a_mixed_batch_picks_only_the_right_ones() {
        let tasks = set(vec![
            task("run_idle", TaskState::Running, 0),
            task("plan_idle", TaskState::Planning, 0),
            task("waiting", TaskState::AwaitingHuman, 0),
            task("stuck", TaskState::Stalled, 0),
            task("finished", TaskState::Done, 0),
            task("busy", TaskState::Running, 0),
        ]);
        let events = vec![
            ev(100_000, "run_idle"),
            ev(100_000, "plan_idle"),
            ev(100_000, "waiting"),
            ev(100_000, "stuck"),
            ev(100_000, "finished"),
            // busy 刚刚还在动
            ev(999_000, "busy"),
        ];
        let cfg = AutoAdvance::new(1_000_000).with_idle_ms(120_000);
        assert_eq!(
            auto_advance_candidates(&tasks, &events, cfg),
            vec!["plan_idle".to_string(), "run_idle".to_string()]
        );
    }

    // ---- 需要人介入的 ----

    #[test]
    fn attention_needed_reports_waiting_and_stalled() {
        // **这两类任务在台账里静静躺着，而使用者不会主动去查。**
        // 所以它们必须能被报出来——"干了事不会告诉你"的另一面。
        let tasks = set(vec![
            task("w1", TaskState::AwaitingHuman, 0),
            task("s1", TaskState::Stalled, 0),
            task("r1", TaskState::Running, 0),
            task("d1", TaskState::Done, 0),
        ]);
        let got = attention_needed(&tasks);
        let ids: Vec<&str> = got.iter().map(|(t, _)| t.id.as_str()).collect();
        assert_eq!(ids, vec!["s1", "w1"]);
        for (_, why) in &got {
            assert!(!why.is_empty(), "要说清为什么需要人");
        }
    }

    #[test]
    fn attention_needed_says_why_it_needs_a_human() {
        let tasks = set(vec![task("w1", TaskState::AwaitingHuman, 0)]);
        let got = attention_needed(&tasks);
        assert!(got[0].1.contains("等你"), "{}", got[0].1);
    }

    #[test]
    fn attention_needed_is_empty_when_nothing_needs_you() {
        // 常驻进程每分钟报一次"有事等你"会让噪音淹没真信号
        let tasks = set(vec![
            task("a", TaskState::Running, 0),
            task("b", TaskState::Done, 0),
        ]);
        assert!(attention_needed(&tasks).is_empty());
    }

    #[test]
    fn the_two_sets_never_overlap() {
        // **这是一条不变量：能被自动推进的，和需要人的，必须互斥。**
        // 重叠意味着某个任务既会被自动跑、又在等人——那是最坏的情况。
        let tasks = set(vec![
            task("a", TaskState::Planning, 0),
            task("b", TaskState::Running, 0),
            task("c", TaskState::AwaitingHuman, 0),
            task("d", TaskState::Stalled, 0),
        ]);
        let auto: std::collections::BTreeSet<String> =
            auto_advance_candidates(&tasks, &[], AutoAdvance::new(9_999_999))
                .into_iter()
                .collect();
        let human: std::collections::BTreeSet<String> = attention_needed(&tasks)
            .into_iter()
            .map(|(t, _)| t.id.clone())
            .collect();
        assert!(
            auto.is_disjoint(&human),
            "自动推进集和等人集重叠了: {auto:?} vs {human:?}"
        );
    }

    #[test]
    fn default_idle_is_long_enough_to_not_race_a_normal_run() {
        // 比任何一次正常的模型往返都长，又短到"进程被杀了"能合理地被接上。
        //
        // 用 `const` 块而不是普通断言：这是**编译期的性质**——
        // 常量改错了应该在构建阶段就炸，而不是等某次跑测试才被发现。
        const {
            assert!(DEFAULT_IDLE_MS >= 30_000, "太短会和交互运行抢");
            assert!(DEFAULT_IDLE_MS <= 600_000, "太长会让被杀的进程迟迟接不上");
        }
    }
}
