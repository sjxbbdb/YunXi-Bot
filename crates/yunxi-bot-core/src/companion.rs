//! 陪伴层：把「何时介入」交给决策层判断，再用确定性约束兜底。
//!
//! ## 核心机制
//!
//! ```text
//! 记忆 → state（裁剪过）  +  typed questions
//!            ↓
//!        决策模型 → 概率/标签          ← 接管全部【判断】
//!            ↓
//!        约束层（确定性）→ 只能往【更保守】修正
//!            ↓
//!          Speak / Hold / Quiet
//! ```
//!
//! ## 约束层为什么必须存在
//!
//! ADR §三.1：**约束层永远不会「发明」一个决定，只会否决或降级到更保守的选项。**
//!
//! 这条把"模型的概率不可靠"这个现实挡在安全边界之外：模型可以说"现在该说话"，
//! 但"现在是深夜"是使用者设定的硬事实，不归模型判断。
//!
//! 修正方向是**单向的**：`Speak → Hold → Quiet`，绝不反向。这与项目既有的
//! 「只能更保守」原则同源。

use serde_json::json;

use crate::decide::{Decider, DecisionClass, DecisionEngine, DecisionOutcome, Question};
use crate::memory::{Memory, Situation};

/// 介入动作。按打扰程度排序：`Speak` 最强，`Quiet` 最弱。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Intervention {
    /// 主动开口。
    Speak,
    /// 留着，等合适的时机（不打扰，但不丢弃）。
    Hold,
    /// 保持静默。
    Quiet,
}

impl Intervention {
    pub fn label(self) -> &'static str {
        match self {
            Intervention::Speak => "主动开口",
            Intervention::Hold => "延后聚合",
            Intervention::Quiet => "保持静默",
        }
    }

    /// 取更保守的那个。
    ///
    /// ⚠️ 用 `max` 而不是 `min`：`Ord` 按声明顺序派生，`Speak < Hold < Quiet`，
    /// 所以**越"大"越保守**。写成 `min` 会拿到最激进的动作，约束收紧会完全失效。
    ///
    /// 公开是因为**约束收紧这件事不只陪伴层要用**：信息源触发的介入判定
    /// （见 [`crate::triage`]）同样要过这一关。两处各写一遍 `max`/`min`
    /// 迟早会有一处写反——而写反的表现是"约束静默失效"。
    pub fn more_conservative(self, other: Intervention) -> Intervention {
        self.max(other)
    }
}

/// 使用者设定的陪伴策略。这些是**硬事实**，不交给模型判断。
#[derive(Debug, Clone, PartialEq)]
pub struct CompanionPolicy {
    /// 安静时段起点（本地小时，含）。
    pub quiet_start_hour: u8,
    /// 安静时段终点（本地小时，不含）。
    pub quiet_end_hour: u8,
    /// 每天最多主动打扰几次。
    pub max_interventions_per_day: u32,
    /// 两次打扰之间至少间隔多少分钟。
    pub min_minutes_between_interventions: u64,
}

impl Default for CompanionPolicy {
    fn default() -> Self {
        Self {
            quiet_start_hour: 23,
            quiet_end_hour: 8,
            max_interventions_per_day: 3,
            min_minutes_between_interventions: 120,
        }
    }
}

impl CompanionPolicy {
    /// 判断某个本地小时是否落在安静时段。支持跨零点。
    pub fn is_quiet_hour(&self, hour: u8) -> bool {
        if self.quiet_start_hour == self.quiet_end_hour {
            return false; // 起止相同视为"没有安静时段"
        }
        if self.quiet_start_hour < self.quiet_end_hour {
            hour >= self.quiet_start_hour && hour < self.quiet_end_hour
        } else {
            // 跨零点，例如 23..8
            hour >= self.quiet_start_hour || hour < self.quiet_end_hour
        }
    }
}

/// 一次介入决策的结果。
#[derive(Debug, Clone, PartialEq)]
pub struct InterventionDecision {
    pub action: Intervention,
    /// 为什么是这个结果。必须可读、可审计。
    pub reason: String,
    /// 是否由降级产生。降级必须可见（ADR §7.3 第 1 条）。
    pub degraded: bool,
    /// 模型是否建议过开口（便于观察"模型想说但被约束拦下"）。
    pub model_suggested_speak: bool,
}

/// 陪伴层的内置问题集。
///
/// 命题都写成**可测试**的形式（ADR §5.2）："是否明确要求…" 而不是"是否不开心"。
pub fn intervention_questions() -> Vec<Question> {
    vec![
        Question::choice(
            "intervention",
            "此刻应当如何介入？",
            &[
                ("speak", "有值得主动说明的信息，且现在说不会打扰"),
                ("hold", "有信息但现在不适合说，应留到合适时机"),
                ("quiet", "没有需要主动说明的信息，保持静默即可"),
            ],
        ),
        Question::noul(
            "needs_support",
            "使用者此刻是否处于明确需要情绪支持的状态（而非只是没说话）？",
        ),
        Question::noul(
            "is_anomaly",
            "近期的运行结果中是否出现了显著偏离常态、需要人知道的情况？",
        ),
    ]
}

/// 约束层的判定结果。
#[derive(Debug, Clone, PartialEq)]
pub struct ConstraintVerdict {
    /// 约束允许的上限。动作不得比它更激进。
    pub ceiling: Intervention,
    pub reason: String,
}

/// 约束层：只做"往更保守修正"，绝不发明决定。
///
/// 返回一个**上限**——任何比它更激进的动作都要被压下来。
pub fn constraint_ceiling(
    ctx: &Situation,
    policy: &CompanionPolicy,
    local_hour: u8,
) -> ConstraintVerdict {
    if policy.is_quiet_hour(local_hour) {
        return ConstraintVerdict {
            ceiling: Intervention::Hold,
            reason: format!("当前处于安静时段（{local_hour} 点）"),
        };
    }
    if ctx.quiet_hours {
        return ConstraintVerdict {
            ceiling: Intervention::Hold,
            reason: "情境标记为安静时段".into(),
        };
    }
    if ctx.interventions_today >= policy.max_interventions_per_day {
        return ConstraintVerdict {
            ceiling: Intervention::Hold,
            reason: format!(
                "今天已打扰 {} 次，达到上限 {}",
                ctx.interventions_today, policy.max_interventions_per_day
            ),
        };
    }
    if ctx.minutes_since_last_interaction < policy.min_minutes_between_interventions {
        return ConstraintVerdict {
            ceiling: Intervention::Hold,
            reason: format!(
                "距上次互动仅 {} 分钟，未达最小间隔 {} 分钟",
                ctx.minutes_since_last_interaction, policy.min_minutes_between_interventions
            ),
        };
    }
    ConstraintVerdict {
        ceiling: Intervention::Speak,
        reason: "无约束限制".into(),
    }
}

/// 把模型给出的 choice 解析成介入动作。无法识别时按最保守处理。
fn parse_action(choice: Option<&str>) -> Intervention {
    match choice {
        Some("speak") => Intervention::Speak,
        Some("hold") => Intervention::Hold,
        Some("quiet") => Intervention::Quiet,
        // 未知选项按最保守处理——绝不因为"没看懂"而打扰人
        _ => Intervention::Quiet,
    }
}

/// 完整的一次介入决策。
///
/// 流程：`state` 构造 → 模型判断 → 约束修正。
/// 模型不可用时按 `Interrupt` 类别降级，方向 fail-closed（不打扰）。
pub fn decide_intervention<D: Decider>(
    engine: &mut DecisionEngine<D>,
    memory: &Memory,
    ctx: &Situation,
    policy: &CompanionPolicy,
    local_hour: u8,
    now_ms: u64,
) -> InterventionDecision {
    let state = memory.build_decision_state(ctx, now_ms, 5);
    let req = crate::decide::DecisionRequest::new(state, intervention_questions());
    let outcome = engine.decide(&req);

    let verdict = constraint_ceiling(ctx, policy, local_hour);

    match outcome {
        DecisionOutcome::Decided(result) => {
            let model_action = parse_action(result.choice("intervention"));
            let final_action = model_action.more_conservative(verdict.ceiling);

            let reason = if final_action == model_action {
                format!("模型判断：{}（{}）", model_action.label(), verdict.reason)
            } else {
                format!(
                    "模型建议「{}」，被约束收紧为「{}」：{}",
                    model_action.label(),
                    final_action.label(),
                    verdict.reason
                )
            };

            InterventionDecision {
                action: final_action,
                reason,
                degraded: false,
                model_suggested_speak: model_action == Intervention::Speak,
            }
        }
        DecisionOutcome::Degraded { action, reason, .. } => {
            // 降级方向由类别决定：Interrupt 是 fail-closed。
            // 约束仍然生效——降级不等于绕过使用者的设定。
            let degraded_action = Intervention::Hold.more_conservative(verdict.ceiling);
            InterventionDecision {
                action: degraded_action,
                reason: format!("决策模型不可用，按 Interrupt 类别降级（{action}）：{reason}"),
                degraded: true,
                model_suggested_speak: false,
            }
        }
    }
}

/// 把一条信息写进记忆的事件载荷。供调用方 append 到台账。
pub fn memory_event_data(
    id: &str,
    kind: crate::memory::MemoryKind,
    text: &str,
) -> serde_json::Value {
    json!({ "id": id, "kind": kind, "text": text })
}

/// 陪伴层使用的决策引擎构造。
pub fn companion_engine<D: Decider>(decider: D) -> DecisionEngine<D> {
    // 类别固定为 Interrupt：陪伴介入属于"打扰"类，降级方向 fail-closed
    DecisionEngine::new(decider, DecisionClass::Interrupt)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decide::StubDecider;

    fn ctx() -> Situation {
        Situation {
            quiet_hours: false,
            minutes_since_last_interaction: 600,
            unread_events: 2,
            recent_failures: 0,
            interventions_today: 0,
            relationship_stage: "熟悉".into(),
        }
    }

    fn memory_with_facts() -> Memory {
        use crate::ledger::{Event, EventKind};
        Memory::from_events(&[Event {
            seq: 1,
            at: 1_000,
            kind: EventKind::MemoryRecorded,
            span: None,
            job: None,
            data: json!({"id": "f1", "kind": "fact", "text": "在准备面试"}),
        }])
    }

    #[test]
    fn quiet_hours_wraps_midnight() {
        let p = CompanionPolicy {
            quiet_start_hour: 23,
            quiet_end_hour: 8,
            ..Default::default()
        };
        assert!(p.is_quiet_hour(23));
        assert!(p.is_quiet_hour(0));
        assert!(p.is_quiet_hour(7));
        assert!(!p.is_quiet_hour(8));
        assert!(!p.is_quiet_hour(12));
        assert!(!p.is_quiet_hour(22));
    }

    #[test]
    fn constraint_can_only_lower_never_raise() {
        // 安静时段下上限是 Hold —— 模型就算说 speak 也要被压下来
        let p = CompanionPolicy::default();
        let v = constraint_ceiling(&ctx(), &p, 2);
        assert_eq!(v.ceiling, Intervention::Hold);
        assert!(v.reason.contains("安静时段"));
    }

    #[test]
    fn model_speaking_during_quiet_hours_is_pressed_down() {
        let stub = StubDecider::succeeding()
            .with_choice("intervention", "speak")
            .with_noul("needs_support", 0.9)
            .with_noul("is_anomaly", 0.1);
        let mut engine = companion_engine(stub);

        // 凌晨 3 点：模型想说，但约束不允许
        let d = decide_intervention(
            &mut engine,
            &memory_with_facts(),
            &ctx(),
            &CompanionPolicy::default(),
            3,
            1_000_000,
        );
        assert_eq!(d.action, Intervention::Hold, "深夜不得打扰");
        assert!(d.model_suggested_speak, "应记录模型确实建议过开口");
        assert!(
            d.reason.contains("被约束收紧"),
            "理由要说明是被谁拦下的: {}",
            d.reason
        );
    }

    #[test]
    fn model_speaking_in_daytime_is_allowed() {
        let stub = StubDecider::succeeding()
            .with_choice("intervention", "speak")
            .with_noul("needs_support", 0.2)
            .with_noul("is_anomaly", 0.1);
        let mut engine = companion_engine(stub);
        let d = decide_intervention(
            &mut engine,
            &memory_with_facts(),
            &ctx(),
            &CompanionPolicy::default(),
            14,
            1_000_000,
        );
        assert_eq!(d.action, Intervention::Speak);
        assert!(!d.degraded);
    }

    #[test]
    fn daily_limit_caps_interventions() {
        let stub = StubDecider::succeeding().with_choice("intervention", "speak");
        let mut engine = companion_engine(stub);
        let mut c = ctx();
        c.interventions_today = 5;
        let d = decide_intervention(
            &mut engine,
            &memory_with_facts(),
            &c,
            &CompanionPolicy::default(),
            14,
            1_000_000,
        );
        assert_eq!(d.action, Intervention::Hold);
        assert!(d.reason.contains("上限"));
    }

    #[test]
    fn model_failure_degrades_to_not_interrupting() {
        let mut engine = companion_engine(StubDecider::failing("连接被拒绝"));
        let d = decide_intervention(
            &mut engine,
            &memory_with_facts(),
            &ctx(),
            &CompanionPolicy::default(),
            14,
            1_000_000,
        );
        assert!(d.degraded, "模型不可用必须标记为降级");
        assert_ne!(
            d.action,
            Intervention::Speak,
            "打扰类降级方向是 fail-closed"
        );
        assert!(d.reason.contains("降级"));
    }

    #[test]
    fn unknown_choice_is_treated_as_most_conservative() {
        // 模型编了个没声明的选项，validate 会拒绝 → 走降级 → 不打扰
        let stub = StubDecider::succeeding().with_choice("intervention", "yell");
        let mut engine = companion_engine(stub);
        let d = decide_intervention(
            &mut engine,
            &memory_with_facts(),
            &ctx(),
            &CompanionPolicy::default(),
            14,
            1_000_000,
        );
        assert_ne!(d.action, Intervention::Speak);
        assert!(d.degraded, "未声明选项应被校验拦下并降级");
    }

    #[test]
    fn degradation_does_not_bypass_user_policy() {
        // 降级也不能绕过"今天已经打扰够了"这种硬设定
        let mut engine = companion_engine(StubDecider::failing("boom"));
        let mut c = ctx();
        c.interventions_today = 99;
        let d = decide_intervention(
            &mut engine,
            &memory_with_facts(),
            &c,
            &CompanionPolicy::default(),
            14,
            1_000_000,
        );
        assert_eq!(d.action, Intervention::Hold);
        assert!(d.degraded);
    }
}
