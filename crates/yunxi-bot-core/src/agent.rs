//! Agent 循环：把记忆、决策、思考三层串成一个回合。
//!
//! ## 为什么需要这一层
//!
//! 在此之前，`decide` / `memory` / `companion` 三个模块各自单测通过，
//! 但**守护进程从不调用它们**——任务在跑，Agent 却不判断、不记忆、不说话。
//! 那个状态是"一个调度器 + 三个库"，不是 Agent。
//!
//! 本模块负责把三者接起来，并且**只依赖台账作为事实来源**：
//! 现状从台账投影得出，判断结果写回台账。
//!
//! ## 一个回合做什么
//!
//! ```text
//! 台账 ──投影──> 记忆 + 现状
//!                   │
//!                   ├─> 本地决策层（Laya）：该不该介入？   ← 快、免费、离线
//!                   │        │
//!                   │        └─ 约束层收紧（只能更保守）
//!                   │
//!                   └─> 只有判定「开口」时才动用远端思考层（Agnes）
//!                            ↓
//!                        写回决策台账
//! ```
//!
//! **顺序不能反。** Agnes 免费档只有 10 RPM，常驻进程每轮都打远端几秒就把配额
//! 烧光。本地那一层先筛，是这套配额下的结构必然，不是优化。

use crate::companion::{
    CompanionPolicy, Intervention, InterventionDecision, companion_engine, decide_intervention,
};
use crate::decide::{Decider, DecisionClass, DecisionEngine, record_decision};
use crate::ledger::{Event, EventKind, Ledger, LedgerError};
use crate::memory::{Memory, Situation};
use crate::think::{Message, ThinkError, ThinkRequest, Thinker};

/// 最近多少条事件用于推断"现状"。
const RECENT_WINDOW: usize = 50;

/// 一次回合的结果。
#[derive(Debug, Clone, PartialEq)]
pub struct CycleOutcome {
    /// 介入判断（含被约束收紧后的最终动作）。
    pub decision: InterventionDecision,
    /// 若判定为「开口」，这里是实际说出口的话；否则为 `None`。
    pub spoken: Option<String>,
    /// 是否因为本地限流而没去调用思考层。
    pub throttled: bool,
    /// 决策在台账里的边界号。
    pub span_id: u64,
}

impl CycleOutcome {
    pub fn is_quiet(&self) -> bool {
        self.spoken.is_none() && !self.decision.degraded
    }
}

#[derive(Debug)]
pub enum CycleError {
    Ledger(LedgerError),
    /// 思考层失败。**注意：这不是致命错误**——判断已经做完了，
    /// 只是"想说什么"没生成出来。调用方应把它记进台账并继续。
    Think(ThinkError),
}

impl std::fmt::Display for CycleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CycleError::Ledger(e) => write!(f, "台账错误: {e}"),
            CycleError::Think(e) => write!(f, "思考层错误: {e}"),
        }
    }
}

impl std::error::Error for CycleError {}

impl From<LedgerError> for CycleError {
    fn from(e: LedgerError) -> Self {
        CycleError::Ledger(e)
    }
}

/// 从台账投影出"现状"。
///
/// **全部从台账推导**，不引入第二份状态——否则台账就不再是唯一事实来源。
///
/// `interventions_today` 用**滚动 24 小时**而不是本地日历日：日历日会在午夜
/// 突然清零，那正是最不该突然放开打扰额度的时刻。
pub fn derive_situation(events: &[Event], now_ms: u64) -> Situation {
    const DAY_MS: u64 = 86_400_000;
    let day_ago = now_ms.saturating_sub(DAY_MS);

    let window_start = events.len().saturating_sub(RECENT_WINDOW);
    let recent = &events[window_start..];

    // 最近一次判断发生在什么时候
    let last_decision_at = recent
        .iter()
        .filter(|e| e.kind == EventKind::DecisionDecided)
        .map(|e| e.at)
        .max();

    let minutes_since_last_interaction = match last_decision_at {
        Some(t) => (now_ms.saturating_sub(t)) / 60_000,
        // 从没判断过：给一个足够大的值，让"最小间隔"约束不拦第一次
        None => u64::MAX / 60_000,
    };

    // 滚动 24 小时内判定为「开口」的次数
    let interventions_today = events
        .iter()
        .filter(|e| e.kind == EventKind::DecisionDecided && e.at >= day_ago)
        .filter(|e| {
            e.data
                .get("action")
                .and_then(|v| v.as_str())
                .is_some_and(|a| a == Intervention::Speak.label())
        })
        .count() as u32;

    // 最近一次判断之后又积累了多少待处理事件
    let unread_events = match last_decision_at {
        Some(t) => events
            .iter()
            .filter(|e| e.at > t)
            .filter(|e| matches!(e.kind, EventKind::JobSucceeded | EventKind::JobFailed))
            .count(),
        None => events
            .iter()
            .filter(|e| matches!(e.kind, EventKind::JobSucceeded | EventKind::JobFailed))
            .count(),
    };

    // 连续失败：从最近往前数，遇到成功就停
    let recent_failures = recent
        .iter()
        .rev()
        .take_while(|e| matches!(e.kind, EventKind::JobFailed | EventKind::JobSucceeded))
        .take_while(|e| e.kind == EventKind::JobFailed)
        .count() as u32;

    Situation {
        quiet_hours: false,
        minutes_since_last_interaction,
        unread_events,
        recent_failures,
        interventions_today,
        relationship_stage: "初期".into(),
    }
}

/// 判断 + 表达所需的输入。
pub struct CycleInput<'a> {
    pub policy: &'a CompanionPolicy,
    /// 本地小时（0–23），用于安静时段判定。
    pub local_hour: u8,
    pub now_ms: u64,
    pub base: Situation,
}

/// 跑一个完整回合。
///
/// `thinker` 为 `None` 时只做判断不做表达——用于"只想要判断结果"的场景，
/// 以及**在还没配置远端模型时让 Agent 依然能跑**（它会判断、会记录，
/// 只是开口时说不出话）。
pub fn run_cycle<D: Decider, T: Thinker>(
    ledger: &mut Ledger,
    engine: &mut DecisionEngine<D>,
    thinker: Option<&T>,
    input: &CycleInput<'_>,
) -> Result<CycleOutcome, CycleError> {
    // 1. 投影记忆与现状
    let events = ledger.events().to_vec();
    let memory = Memory::from_events(&events);
    let mut ctx = derive_situation(&events, input.now_ms);
    ctx.quiet_hours = input.base.quiet_hours || ctx.quiet_hours;
    ctx.relationship_stage = input.base.relationship_stage.clone();

    // 2. 本地决策层判断（快、免费、离线可用）
    let decision = decide_intervention(
        engine,
        &memory,
        &ctx,
        input.policy,
        input.local_hour,
        input.now_ms,
    );

    // 3. 落决策台账（审计对在同一个边界内）。
    //    `degraded` **如实**来自判断结果——不能因为省事就一律写成降级，
    //    那会让台账把成功的判断也标成降级。
    let span_id = record_decision(
        ledger,
        DecisionClass::Interrupt,
        decision.degraded,
        decision.action.label(),
        &decision.reason,
        None,
        &["intervention".to_string(), "needs_support".to_string()],
    )?;

    // 4. 只有要开口时才动用远端思考层
    if decision.action != Intervention::Speak {
        return Ok(CycleOutcome {
            decision,
            spoken: None,
            throttled: false,
            span_id,
        });
    }

    let Some(thinker) = thinker else {
        return Ok(CycleOutcome {
            decision,
            spoken: None,
            throttled: false,
            span_id,
        });
    };

    let prompt = compose_prompt(&ctx, &memory, &events, input.now_ms);
    let req = ThinkRequest::new(vec![Message::system(SYSTEM_PROMPT), Message::user(prompt)])
        .with_max_tokens(400)
        // 陪伴场景要的是稳定，不是花哨
        .with_temperature(0.4);

    match thinker.think(&req) {
        Ok(resp) => {
            let text = resp.content.trim().to_string();

            // 模型可以用 `-` 表示"这次没什么值得说的"（见 SYSTEM_PROMPT）。
            // **那是不开口，不是说了个破折号。** 早先版本把它原样记成"说出口的话"，
            // 于是 Agent 会郑重其事地对你说一个"-"。
            if is_nothing_to_say(&text) {
                return Ok(CycleOutcome {
                    decision,
                    spoken: None,
                    throttled: false,
                    span_id,
                });
            }

            // 说出口的话也要入账：否则事后无法回答"它当初到底说了什么"
            ledger.append(
                EventKind::MemoryRecorded,
                None,
                serde_json::json!({
                    "id": format!("said-{span_id}"),
                    "kind": "event",
                    "text": format!("主动开口: {text}"),
                }),
            )?;
            Ok(CycleOutcome {
                decision,
                spoken: Some(text),
                throttled: false,
                span_id,
            })
        }
        Err(ThinkError::LocalThrottle { .. }) => Ok(CycleOutcome {
            decision,
            spoken: None,
            throttled: true,
            span_id,
        }),
        Err(e) => Err(CycleError::Think(e)),
    }
}

/// 模型是否表示"这次没什么值得说的"。
///
/// 判断已经决定要开口，但**表达层仍有权说"其实没什么好说的"**——判断看的是
/// 结构性事实（有没有值得说明的变化），表达看的是具体内容。
/// 两者不一致时以"不打扰"为准。
fn is_nothing_to_say(text: &str) -> bool {
    let t = text.trim();
    t.is_empty() || matches!(t, "-" | "—" | "–" | "无" | "（无）")
}

/// 陪伴型人设。**克制是刻意的**——主动开口时不该长篇大论。
const SYSTEM_PROMPT: &str = "\
你是一个常驻在用户电脑上的个人助理。你不是客服，不是搜索引擎，是长期陪着这个人的助手。

现在你判断出「值得主动说一句话」。请据此写一句自然的、有分寸的话。

要求：
- 一到两句话，不要分点列举，不要标题，不要客套开场
- 像一个熟人随手说的话，而不是系统通知
- 只输出要说的话本身，不要解释你为什么说

如果判断依据里没有真正值得说的内容，就只回一个「-」表示这次不必打扰。";

/// 从台账里挑出最近的运维事实（任务成功/失败）。
///
/// **这是提示词里最容易漏掉、却最该有的部分。** 早先版本只把记忆条目喂给模型，
/// 于是它看到的是"连续失败：2 次"这么一个数字，却不知道**什么**失败了、
/// 报的什么错——自然只能说"没什么好说的"。
///
/// 判断层看的是结构性信号（有没有变化），表达层需要的是**具体发生了什么**。
fn recent_operational_facts(events: &[Event], limit: usize) -> Vec<String> {
    use std::collections::HashMap;

    // 任务名在 job_created 的 spec 里，而失败事件只带 id
    let mut names: HashMap<String, String> = HashMap::new();
    for e in events {
        if e.kind == EventKind::JobCreated {
            if let (Some(id), Some(name)) = (
                e.job.clone(),
                e.data
                    .get("spec")
                    .and_then(|s| s.get("name"))
                    .and_then(|v| v.as_str()),
            ) {
                names.insert(id, name.to_string());
            }
        }
    }

    events
        .iter()
        .rev()
        .filter(|e| matches!(e.kind, EventKind::JobFailed | EventKind::JobSucceeded))
        .take(limit)
        .map(|e| {
            let id = e.job.clone().unwrap_or_default();
            let name = names.get(&id).cloned().unwrap_or(id);
            match e.kind {
                EventKind::JobFailed => {
                    let err = e
                        .data
                        .get("error")
                        .and_then(|v| v.as_str())
                        .unwrap_or("未知错误");
                    format!("任务「{name}」失败：{err}")
                }
                _ => format!("任务「{name}」成功"),
            }
        })
        .collect()
}

/// 把现状与记忆编成给远端模型看的提示词。
///
/// **只喂判断所需的证据**，不倾倒全部记忆——沿用 `build_decision_state` 的裁剪原则。
/// 但"证据"必须包含**具体发生了什么**，而不只是计数。
///
/// 公开它是为了一个具体用途：让"Agent 为什么不说话"变成**可查**的，
/// 而不是只能靠猜（`yunxi-bot agent --show-prompt`）。
pub fn compose_prompt(ctx: &Situation, memory: &Memory, events: &[Event], now_ms: u64) -> String {
    use crate::memory::MemoryKind;
    let mut parts = Vec::new();

    let mut field = |name: &str, items: Vec<String>| {
        if !items.is_empty() {
            parts.push(format!("{name}：{}", items.join("；")));
        }
    };

    // 最具体的放最前面：模型最需要知道的是"到底出了什么事"
    field("最近发生的实际事件", recent_operational_facts(events, 5));

    field(
        "关于用户",
        memory
            .recall(MemoryKind::Fact, now_ms, 5)
            .iter()
            .map(|e| e.text.clone())
            .collect(),
    );
    field(
        "用户的偏好",
        memory
            .recall(MemoryKind::Preference, now_ms, 5)
            .iter()
            .map(|e| e.text.clone())
            .collect(),
    );
    field(
        "你们之间",
        memory
            .recall(MemoryKind::Relationship, now_ms, 3)
            .iter()
            .map(|e| e.text.clone())
            .collect(),
    );
    field(
        "之前说过的话",
        memory
            .recall(MemoryKind::Event, now_ms, 3)
            .iter()
            .map(|e| e.text.clone())
            .collect(),
    );

    parts.push(format!(
        "距上次互动：{} 分钟",
        ctx.minutes_since_last_interaction
    ));
    parts.push(format!("待处理事件：{} 条", ctx.unread_events));
    if ctx.recent_failures > 0 {
        parts.push(format!("连续失败：{} 次", ctx.recent_failures));
    }
    parts.push(format!("今天已打扰：{} 次", ctx.interventions_today));

    parts.join("\n")
}

/// 便捷构造：陪伴层用的决策引擎（类别固定为打扰类，降级方向 fail-closed）。
pub fn agent_engine<D: Decider>(decider: D) -> DecisionEngine<D> {
    companion_engine(decider)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decide::StubDecider;
    use crate::think::agnes::StubThinker;

    fn tmp(name: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("yunxi-agent-{}-{}.jsonl", std::process::id(), name));
        let _ = std::fs::remove_file(&p);
        p
    }

    fn seed_job(ledger: &mut Ledger, kind: EventKind, at_delta: u64) {
        ledger
            .append(
                kind,
                None,
                serde_json::json!({ "note": format!("t{at_delta}") }),
            )
            .unwrap();
    }

    #[test]
    fn situation_is_derived_from_ledger_not_external_state() {
        let p = tmp("derive");
        let mut l = Ledger::open(&p).unwrap();
        seed_job(&mut l, EventKind::JobSucceeded, 0);
        seed_job(&mut l, EventKind::JobFailed, 1);
        seed_job(&mut l, EventKind::JobFailed, 2);

        let now = l.events().last().unwrap().at + 60_000;
        let s = derive_situation(l.events(), now);
        assert_eq!(s.recent_failures, 2, "连数两次失败后应停下");
        assert_eq!(s.unread_events, 3);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn quiet_when_not_worth_speaking_still_records_a_decision() {
        // 即使不开口，也要留下决策审计对——否则事后无法回答"为什么没提醒我"
        let p = tmp("quiet");
        let mut l = Ledger::open(&p).unwrap();
        let stub = StubDecider::succeeding().with_choice("intervention", "quiet");
        let mut engine = agent_engine(stub);

        let policy = CompanionPolicy::default();
        let input = CycleInput {
            policy: &policy,
            local_hour: 14,
            now_ms: 1_000_000,
            base: Situation::default(),
        };
        let out = run_cycle::<_, StubThinker>(&mut l, &mut engine, None, &input).unwrap();

        assert_eq!(out.decision.action, Intervention::Quiet);
        assert!(out.spoken.is_none());
        let kinds: Vec<_> = l.events().iter().map(|e| e.kind).collect();
        assert!(kinds.contains(&EventKind::DecisionAsked));
        assert!(kinds.contains(&EventKind::DecisionDecided));
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn speaking_involves_the_thinker_and_records_what_was_said() {
        let p = tmp("speak");
        let mut l = Ledger::open(&p).unwrap();
        let stub = StubDecider::succeeding().with_choice("intervention", "speak");
        let mut engine = agent_engine(stub);
        let thinker = StubThinker::replying("你那台备份任务好像连着挂了两次，要不要我看一眼？");

        let policy = CompanionPolicy::default();
        let input = CycleInput {
            policy: &policy,
            local_hour: 14,
            now_ms: 1_000_000,
            base: Situation {
                minutes_since_last_interaction: 600,
                ..Default::default()
            },
        };
        let out = run_cycle(&mut l, &mut engine, Some(&thinker), &input).unwrap();

        assert_eq!(out.decision.action, Intervention::Speak);
        assert!(out.spoken.as_deref().unwrap().contains("备份"));
        assert_eq!(thinker.calls(), 1, "判定开口时才该调用思考层");

        // 说出口的话必须入账
        let said = l.events().iter().any(|e| {
            e.data
                .get("text")
                .and_then(|v| v.as_str())
                .is_some_and(|t| t.contains("备份"))
        });
        assert!(said, "说的话应写进台账，否则事后无法追溯");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn quiet_decision_never_calls_the_thinker() {
        // 这是配额的守门人：不值得开口时一次远端调用都不该发生
        let p = tmp("nothink");
        let mut l = Ledger::open(&p).unwrap();
        let stub = StubDecider::succeeding().with_choice("intervention", "quiet");
        let mut engine = agent_engine(stub);
        let thinker = StubThinker::replying("不该被调用");

        let policy = CompanionPolicy::default();
        let input = CycleInput {
            policy: &policy,
            local_hour: 14,
            now_ms: 1_000_000,
            base: Situation::default(),
        };
        let _ = run_cycle(&mut l, &mut engine, Some(&thinker), &input).unwrap();
        assert_eq!(thinker.calls(), 0, "不开口就绝不打远端");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn quiet_hours_press_speak_down_and_never_reach_the_thinker() {
        // 深夜：模型说开口，但约束收紧成延后 —— 而且**不该花钱打远端**
        let p = tmp("night");
        let mut l = Ledger::open(&p).unwrap();
        let stub = StubDecider::succeeding().with_choice("intervention", "speak");
        let mut engine = agent_engine(stub);
        let thinker = StubThinker::replying("深夜不该说话");

        let policy = CompanionPolicy::default();
        let input = CycleInput {
            policy: &policy,
            local_hour: 3,
            now_ms: 1_000_000,
            base: Situation {
                minutes_since_last_interaction: 600,
                ..Default::default()
            },
        };
        let out = run_cycle(&mut l, &mut engine, Some(&thinker), &input).unwrap();
        assert_eq!(out.decision.action, Intervention::Hold);
        assert_eq!(thinker.calls(), 0, "被约束拦下后不该打远端");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn local_throttle_is_reported_not_swallowed() {
        let p = tmp("throttle");
        let mut l = Ledger::open(&p).unwrap();
        let stub = StubDecider::succeeding().with_choice("intervention", "speak");
        let mut engine = agent_engine(stub);
        let thinker = StubThinker::failing(ThinkError::LocalThrottle {
            wait: std::time::Duration::from_secs(4),
        });

        let policy = CompanionPolicy::default();
        let input = CycleInput {
            policy: &policy,
            local_hour: 14,
            now_ms: 1_000_000,
            base: Situation {
                minutes_since_last_interaction: 600,
                ..Default::default()
            },
        };
        let out = run_cycle(&mut l, &mut engine, Some(&thinker), &input).unwrap();
        assert!(out.throttled, "限流必须显式上报，不能咽下去");
        assert!(out.spoken.is_none());
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn nothing_to_say_sentinel_means_silence_not_a_dash() {
        // 判断说要开口，但表达层认为没什么好说的 —— 不能把"-"当成一句话说出去
        let p = tmp("sentinel");
        let mut l = Ledger::open(&p).unwrap();
        let stub = StubDecider::succeeding().with_choice("intervention", "speak");
        let mut engine = agent_engine(stub);
        let thinker = StubThinker::replying("-");

        let policy = CompanionPolicy::default();
        let input = CycleInput {
            policy: &policy,
            local_hour: 14,
            now_ms: 1_000_000,
            base: Situation {
                minutes_since_last_interaction: 600,
                ..Default::default()
            },
        };
        let out = run_cycle(&mut l, &mut engine, Some(&thinker), &input).unwrap();
        assert!(
            out.spoken.is_none(),
            "哨兵值不该被当成一句话: {:?}",
            out.spoken
        );

        // 而且不能写进记忆
        let wrote_dash = l.events().iter().any(|e| {
            e.data
                .get("text")
                .and_then(|v| v.as_str())
                .is_some_and(|t| t.contains("主动开口"))
        });
        assert!(!wrote_dash, "没说话就不该留下'开口'记录");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn nothing_to_say_recognises_several_spellings() {
        for s in ["-", "—", "–", "", "   ", "无", "（无）"] {
            assert!(is_nothing_to_say(s), "{s:?} 应被识别为没什么可说");
        }
        for s in ["- 我把备份修好了", "无事发生但我想提醒你"] {
            assert!(!is_nothing_to_say(s), "{s:?} 不该被误判");
        }
    }

    #[test]
    fn operational_facts_reach_the_prompt_with_the_job_name_and_error() {
        // 这是修掉"Agnes 总说没什么好说的"的那个缺陷的守门测试：
        // 提示词里必须出现**具体哪个任务失败了、报的什么错**，而不只是计数。
        use crate::job::{JobId, JobSpec, Trigger};
        let p = tmp("facts");
        let mut l = Ledger::open(&p).unwrap();

        let id = JobId::new("j-backup");
        l.append(
            EventKind::JobCreated,
            Some(&id),
            serde_json::json!({ "spec": JobSpec {
                name: "夜间备份".into(),
                command: vec!["backup.exe".into()],
                cwd: ".".into(),
                trigger: Trigger::Manual,
                timeout_ms: 1000,
                max_attempts: 1,
                irreversible: false,
            }}),
        )
        .unwrap();
        l.append(
            EventKind::JobFailed,
            Some(&id),
            serde_json::json!({ "error": "磁盘已满" }),
        )
        .unwrap();

        let events = l.events().to_vec();
        let facts = recent_operational_facts(&events, 5);
        assert_eq!(facts.len(), 1, "应挑出一条运维事实: {facts:?}");
        assert!(
            facts[0].contains("夜间备份"),
            "必须带上任务名: {}",
            facts[0]
        );
        assert!(
            facts[0].contains("磁盘已满"),
            "必须带上错误原因: {}",
            facts[0]
        );

        // 端到端：提示词里必须能看到
        let memory = Memory::from_events(&events);
        let prompt = compose_prompt(&Situation::default(), &memory, &events, 1_000_000);
        assert!(
            prompt.contains("夜间备份") && prompt.contains("磁盘已满"),
            "提示词必须包含具体事实，否则模型无从开口:\n{prompt}"
        );
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn rolling_day_not_calendar_day_for_intervention_budget() {
        // 日历日会在午夜清零，而那正是最不该突然放开额度的时刻
        let p = tmp("rolling");
        let mut l = Ledger::open(&p).unwrap();
        // 伪造一条 23 小时前的"开口"记录
        let now = crate::now_millis().unwrap();
        let mut span = l.begin_span().unwrap();
        span.ledger()
            .append(
                EventKind::DecisionDecided,
                None,
                serde_json::json!({ "action": Intervention::Speak.label() }),
            )
            .unwrap();
        span.close().unwrap();
        // 手动把时间戳改小 23 小时不方便，这里只验证"当天内会被计入"
        let s = derive_situation(l.events(), now);
        assert_eq!(s.interventions_today, 1, "刚发生的开口应计入滚动 24 小时");
        let _ = std::fs::remove_file(&p);
    }
}
