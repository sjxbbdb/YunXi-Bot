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
///
/// 这一条是**不留痕**的旧签名，等价于 [`decide_intervention_with_ledger`]
/// 传一个空口子。
///
/// ## 为什么旧签名还要留着，而不是让所有调用方都传台账
///
/// 这一处和别处不同的地方在于：**两个真的调用它的地方本来就已经在记账**
/// （[`crate::agent::run_cycle`] 和 `cmd_companion` 各自写一对
/// `DecisionClass::Interrupt` 决策事件，字段就是下面算出来的
/// action / reason / degraded）。
///
/// 所以这里**不能**再记一遍：一次介入会变成两条 `decision_decided`，
/// 而 `agent::derive_situation` 正是按 `DecisionDecided` 数
/// "今天打扰了几次""距上次互动多久"的——重复记会让约束层以为
/// 自己刚打扰过两次，**主动开口于是被自己压下去**。
///
/// 留着这个空口子是为了另一个场景：将来多一个调用方（比如别的前端),
/// 它手里有台账、又不走 `run_cycle` 那条路，就直接用带台账的那个版本，
/// 不必照抄一遍 `record_decision` 的字段。
pub fn decide_intervention<D: Decider>(
    engine: &mut DecisionEngine<D>,
    memory: &Memory,
    ctx: &Situation,
    policy: &CompanionPolicy,
    local_hour: u8,
    now_ms: u64,
) -> InterventionDecision {
    decide_intervention_with_ledger(engine, memory, ctx, policy, local_hour, now_ms, None)
}

/// 同 [`decide_intervention`]，但把这次判断**记进台账**。
///
/// ## 记的是"最终动作"，不是"模型建议"
///
/// 台账里的 `action` 是**约束收紧之后**的那个动作（`Speak/Hold/Quiet`），
/// 和 `InterventionDecision.action` 是同一个值——这样"台账说延后、
/// 终端说延后、实际也延后"三处才是同一份数据（D128 的教训）。
/// 模型原本建议什么在 `reason` 里（"模型建议「主动开口」，被约束收紧为…"）。
///
/// ## `degraded` 如实来
///
/// 模型正常答了是 `false`（哪怕它的建议随后被约束压下去了——**被约束压住
/// 不是降级**，那是设计好的第二层），模型不可用才是 `true`。
/// 把这两件事混起来正是早先版本犯过的错：调用方为了省事一律构造 `Degraded`，
/// 台账于是把成功的判断也标成降级。
///
/// ## 只记"真的问了模型"的那条路
///
/// 这个函数一定会去问（`DecisionEngine::decide` 永不返回 Err，
/// 熔断打开时会给出降级结论——那也是"问过了、引擎说它不可用"）。
/// 所以这里只有一条路径：留痕。
#[allow(clippy::too_many_arguments)]
pub fn decide_intervention_with_ledger<D: Decider>(
    engine: &mut DecisionEngine<D>,
    memory: &Memory,
    ctx: &Situation,
    policy: &CompanionPolicy,
    local_hour: u8,
    now_ms: u64,
    decision_sink: crate::decide::DecisionSink<'_>,
) -> InterventionDecision {
    let state = memory.build_decision_state(ctx, now_ms, 5);
    let questions = intervention_questions();
    // **问句 id 从真正发出去的那一份问题集里取**，不在这里手抄一遍：
    // 手抄的那份迟早会和 `intervention_questions()` 漂开，而漂开的表现是
    // 台账说"我问了 X"，实际问的是 Y——D128 的形状。
    let question_ids: Vec<String> = questions.iter().map(|q| q.id.clone()).collect();
    let req = crate::decide::DecisionRequest::new(state, questions);
    let outcome = engine.decide(&req);

    let verdict = constraint_ceiling(ctx, policy, local_hour);

    let (decision, model) = match outcome {
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

            (
                InterventionDecision {
                    action: final_action,
                    reason,
                    degraded: false,
                    model_suggested_speak: model_action == Intervention::Speak,
                },
                // 回答的模型名要进台账：归因是这一层留痕的意义所在。
                Some(result.model),
            )
        }
        DecisionOutcome::Degraded { action, reason, .. } => {
            // 降级方向由类别决定：Interrupt 是 fail-closed。
            // 约束仍然生效——降级不等于绕过使用者的设定。
            let degraded_action = Intervention::Hold.more_conservative(verdict.ceiling);
            (
                InterventionDecision {
                    action: degraded_action,
                    reason: format!("决策模型不可用，按 Interrupt 类别降级（{action}）：{reason}"),
                    degraded: true,
                    model_suggested_speak: false,
                },
                // 没有模型答过这一问——别把归因引到一次没发生过的调用上。
                None,
            )
        }
    };

    if let Err(e) = crate::decide::record_decision_optional(
        decision_sink,
        // **类别就是 `Interrupt`**，和 `companion_engine` 建引擎时用的是同一个：
        // 陪伴介入属于"打扰/通知"，降级方向 fail-closed（不确定就不打扰）。
        // 这和 `InterventionDecision.degraded` 是两件事，不要合并。
        DecisionClass::Interrupt,
        decision.degraded,
        decision.action.label(),
        &decision.reason,
        model.as_deref(),
        &question_ids,
    ) {
        // 写失败只报不中断：留痕失败不该让人错过一次本来就该发生的开口。
        eprintln!("⚠ 陪伴介入留痕写入失败（不影响判断）: {e}");
    }

    decision
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

    // ---- 留痕：这一处"问过模型没有、它是不是弃权"必须查得出来 ----

    /// 一个临时台账。文件名带 tag，避免并行跑的测试互相踩。
    fn tmp_ledger(tag: &str) -> (std::path::PathBuf, crate::ledger::Ledger) {
        let p = std::env::temp_dir().join(format!(
            "yunxi-companion-{}-{tag}.jsonl",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&p);
        let l = crate::ledger::Ledger::open(&p).expect("开台账");
        (p, l)
    }

    fn decided_event(ledger: &crate::ledger::Ledger) -> &crate::ledger::Event {
        ledger
            .events()
            .iter()
            .find(|e| e.kind == crate::ledger::EventKind::DecisionDecided)
            .expect("应有一条 decision_decided")
    }

    /// **模型正常判断 → 记一条 `Interrupt`、`degraded = false`。**
    ///
    /// 用真台账（临时文件）：要验的四条（类别 / degraded / 动作 / 问句 id）
    /// 全在 `record_decision` 那一层，换掉器件就等于把要验的那层换掉了。
    #[test]
    fn a_companion_judgment_is_recorded_when_a_ledger_is_attached() {
        let (path, mut ledger) = tmp_ledger("judged");
        let stub = StubDecider::succeeding()
            .with_choice("intervention", "speak")
            .with_noul("needs_support", 0.2)
            .with_noul("is_anomaly", 0.1);
        let mut engine = companion_engine(stub);
        let d = decide_intervention_with_ledger(
            &mut engine,
            &memory_with_facts(),
            &ctx(),
            &CompanionPolicy::default(),
            14,
            1_000_000,
            Some(&mut ledger),
        );
        assert_eq!(d.action, Intervention::Speak, "留痕不该改变结论");

        let decided = decided_event(&ledger);
        assert_eq!(
            decided.data["class"],
            serde_json::json!("interrupt"),
            "陪伴介入是打扰类——它的降级方向是不打扰: {}",
            decided.data
        );
        assert_eq!(
            decided.data["degraded"],
            serde_json::json!(false),
            "模型答了不是降级: {}",
            decided.data
        );
        assert_eq!(
            decided.data["action"],
            serde_json::json!(Intervention::Speak.label()),
            "台账里的动作必须和真正执行的那个是同一个值（D128）: {}",
            decided.data
        );
        assert_eq!(decided.data["model"], serde_json::json!("stub"));

        // 问句 id 从真正发出去的问题集里取：三个一个都不能少、也不能编。
        let asked = ledger
            .events()
            .iter()
            .find(|e| e.kind == crate::ledger::EventKind::DecisionAsked)
            .expect("应有一条 decision_asked");
        assert_eq!(
            asked.data["questions"],
            serde_json::json!(["intervention", "needs_support", "is_anomaly"]),
            "台账里记的问句必须是真正问出去的那几个: {}",
            asked.data
        );
        assert_eq!(asked.span, decided.span, "审计对要落在同一个边界里");
        let _ = std::fs::remove_file(&path);
    }

    /// **模型不可用 → `degraded = true`，而且理由里带故障原文。**
    ///
    /// 这一条和"模型答了 hold"在外部行为上几乎一样（都不打扰），
    /// 在台账上必须分得开：一个是判断，一个是故障。
    #[test]
    fn a_degraded_companion_judgment_says_so_in_the_ledger() {
        let (path, mut ledger) = tmp_ledger("degraded");
        let mut engine = companion_engine(StubDecider::failing("连接被拒绝"));
        let d = decide_intervention_with_ledger(
            &mut engine,
            &memory_with_facts(),
            &ctx(),
            &CompanionPolicy::default(),
            14,
            1_000_000,
            Some(&mut ledger),
        );
        assert!(d.degraded);

        let decided = decided_event(&ledger);
        assert_eq!(decided.data["class"], serde_json::json!("interrupt"));
        assert_eq!(
            decided.data["degraded"],
            serde_json::json!(true),
            "模型不可用必须如实记成降级: {}",
            decided.data
        );
        assert_eq!(
            decided.data["model"],
            serde_json::json!(null),
            "没有模型答过这一问，别把归因引到一次没发生过的调用上: {}",
            decided.data
        );
        let reason = decided.data["reason"].as_str().unwrap_or("");
        assert!(
            reason.contains("连接被拒绝"),
            "故障原文要留下——'它为什么不说话了'的答案在这句里: {reason}"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// 约束把模型压下去的那一种**不是降级**：模型答了，只是被设计好的
    /// 第二层收紧了。混起来会让台账把正常判断说成故障。
    #[test]
    fn being_pressed_down_by_the_constraint_is_not_a_degradation() {
        let (path, mut ledger) = tmp_ledger("pressed");
        let stub = StubDecider::succeeding().with_choice("intervention", "speak");
        let mut engine = companion_engine(stub);
        // 凌晨 3 点：模型想说，但约束不允许。
        let d = decide_intervention_with_ledger(
            &mut engine,
            &memory_with_facts(),
            &ctx(),
            &CompanionPolicy::default(),
            3,
            1_000_000,
            Some(&mut ledger),
        );
        assert_eq!(d.action, Intervention::Hold);

        let decided = decided_event(&ledger);
        assert_eq!(
            decided.data["degraded"],
            serde_json::json!(false),
            "被约束收紧不是降级——那是设计好的第二层: {}",
            decided.data
        );
        assert_eq!(
            decided.data["action"],
            serde_json::json!(Intervention::Hold.label()),
            "记的是收紧之后的最终动作: {}",
            decided.data
        );
        let _ = std::fs::remove_file(&path);
    }

    /// **不挂台账 = 与从前逐字节相同**：没有那一对事件，判断一个字都不变。
    #[test]
    fn a_companion_judgment_without_a_ledger_keeps_todays_behaviour() {
        let (path, ledger) = tmp_ledger("no-sink");
        let stub = StubDecider::succeeding().with_choice("intervention", "speak");
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
        assert!(
            ledger.events().is_empty(),
            "没挂台账却写了东西: {:?}",
            ledger.events()
        );
        let _ = std::fs::remove_file(&path);
    }
}
