//! 决策层：System-1 决策模型的接入、结果归一与降级。
//!
//! 设计依据 ADR-0001 §五与§七。三条不可动摇的规则：
//!
//! 1. **模型判断是策略的输入，不是策略的替代**（官方文档原文）。
//!    本模块只负责"拿到概率"，不做最终决定——最终决定在约束层。
//! 2. **降级方向按类别相反**：打扰类 fail-closed（不打扰），
//!    安全类 fail-open（升级给人）。统一的"模型挂了就走规则兜底"会同时犯两个错。
//! 3. **降级必须可见**：任何降级都带 `degraded: true` 与原因，进决策台账。

pub mod http;
pub mod laya;

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

pub use laya::{Decider, DecisionError, LayaDecider, StubDecider};

/// 问题类型。对应 System-1 决策模型的三类 typed question。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum QuestionKind {
    /// 从命名选项中选择。选项必须有可区分的判据。
    Choice {
        /// 选项名 → 判据说明。判据是给模型看的，也是给人复核的。
        criteria: BTreeMap<String, String>,
    },
    /// 有序等级评分。
    ///
    /// ⚠️ 上游已声明这是弱项。使用前必须用自己的数据测混淆矩阵。
    Score { levels: Vec<String> },
    /// 估计命题为真的概率 P(true)。
    ///
    /// 命题**必须可测试**："客户是否明确威胁取消" ✅ / "客户是否不开心" ❌。
    Noul,
}

/// 一个类型化问题。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Question {
    pub id: String,
    pub kind: QuestionKind,
    /// 给模型的指令，例如"根据报告的故障判断归属团队"。
    pub instructions: String,
}

impl Question {
    pub fn choice(
        id: impl Into<String>,
        instructions: impl Into<String>,
        criteria: &[(&str, &str)],
    ) -> Self {
        Self {
            id: id.into(),
            kind: QuestionKind::Choice {
                criteria: criteria
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
            },
            instructions: instructions.into(),
        }
    }

    pub fn noul(id: impl Into<String>, instructions: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            kind: QuestionKind::Noul,
            instructions: instructions.into(),
        }
    }

    pub fn score(id: impl Into<String>, instructions: impl Into<String>, levels: &[&str]) -> Self {
        Self {
            id: id.into(),
            kind: QuestionKind::Score {
                levels: levels.iter().map(|s| s.to_string()).collect(),
            },
            instructions: instructions.into(),
        }
    }
}

/// 一次决策请求。
///
/// **`state` 必须主动裁剪**：只放判断所需的证据，不放"服务知道的一切"。
/// 密钥与无关字段一律不得进入。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DecisionRequest {
    pub state: serde_json::Value,
    pub questions: Vec<Question>,
}

impl DecisionRequest {
    pub fn new(state: serde_json::Value, questions: Vec<Question>) -> Self {
        Self { state, questions }
    }
}

/// 单个问题的答案。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Answer {
    /// `choice` 选中的选项。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub choice: Option<String>,
    /// 选项/等级 → 概率。用于约束层做阈值判断。
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub distribution: BTreeMap<String, f64>,
    /// `score` 的期望分。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_score: Option<f64>,
    /// `noul` 的 P(true)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub noul: Option<f64>,
}

impl Answer {
    /// 该答案的置信度（最高概率）。用于"模型自己不确定"这一失败形态的判定。
    pub fn confidence(&self) -> Option<f64> {
        if let Some(n) = self.noul {
            // P(true) 与 P(false) 的较大者
            return Some(n.max(1.0 - n));
        }
        self.distribution
            .values()
            .copied()
            .fold(None, |acc: Option<f64>, v| {
                Some(acc.map_or(v, |a| a.max(v)))
            })
    }
}

/// 一次决策的结果。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DecisionResult {
    pub answers: BTreeMap<String, Answer>,
    /// 实际回答的模型标识，进台账便于事后归因。
    pub model: String,
}

impl DecisionResult {
    pub fn answer(&self, id: &str) -> Option<&Answer> {
        self.answers.get(id)
    }

    /// 取某个 `noul` 答案的概率。
    pub fn probability(&self, id: &str) -> Option<f64> {
        self.answers.get(id).and_then(|a| a.noul)
    }

    /// 取某个 `choice` 答案的选项。
    pub fn choice(&self, id: &str) -> Option<&str> {
        self.answers.get(id).and_then(|a| a.choice.as_deref())
    }
}

/// 决策类别。**降级方向由此决定**——这是整个降级策略的枢纽。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionClass {
    /// 打扰 / 通知。
    Interrupt,
    /// 升级 / 人工介入。
    Escalate,
    /// 不可逆动作。
    Irreversible,
    /// 分类 / 归档。
    Classify,
    /// 紧急度评分。
    Urgency,
    /// 异常识别。
    Anomaly,
    /// 路由 / 选端点。**"这一轮该发给谁"的判定就是这一类。**
    ///
    /// 单独一类而不是借 [`DecisionClass::Classify`]：那一类记的是
    /// "这句话属于哪一类"，而这里的判定是"原来选的那个端点用不了"。
    /// 两者在台账上混在一起的话，事后按类别翻记录的人
    /// 会把**端点替换**读成**分类结果**——D128 的教训正是
    /// "台账说了什么，就得是什么"。
    Route,
}

/// 降级方向。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DegradationDirection {
    /// 朝"不做"倒：不确定就不打扰、不升级、不动。
    FailClosed,
    /// 朝"告警"倒：不确定就叫人。
    FailOpen,
    /// 不参与降级：该类别有硬约束，模型判断不改变行为。
    NotApplicable,
}

impl DecisionClass {
    /// 该类别在模型不可用时的降级方向。
    ///
    /// **注意第一项与第二项方向相反**——这正是必须先分类的原因。
    pub fn direction(self) -> DegradationDirection {
        match self {
            // 宁可漏报，不可半夜吵
            DecisionClass::Interrupt => DegradationDirection::FailClosed,
            // 宁可误报，不可漏掉危险
            DecisionClass::Escalate => DegradationDirection::FailOpen,
            // 批准是硬约束，不是判断
            DecisionClass::Irreversible => DegradationDirection::NotApplicable,
            // 猜错比不猜代价高
            DecisionClass::Classify => DegradationDirection::FailClosed,
            // 不升级
            DecisionClass::Urgency => DegradationDirection::FailClosed,
            // 安全侧倾向
            DecisionClass::Anomaly => DegradationDirection::FailOpen,
            // **路由回落到能干活的那一边。**
            //
            // 方向是 fail-open：本地模型没就绪时**不拦这一轮**，只是换个端点把它办完。
            // 两边的代价不对称——错判成远端多花几分钱，错判成本地则这一轮必然失败。
            // 和 `ModelRouter` 里"拿不准就往远端倒"是同一条判据。
            DecisionClass::Route => DegradationDirection::FailOpen,
        }
    }

    /// 降级时采取的保守动作。
    pub fn degraded_action(self) -> &'static str {
        match self {
            DecisionClass::Interrupt => "延后聚合，本轮不打扰",
            DecisionClass::Escalate => "升级给人",
            DecisionClass::Irreversible => "保持强制人工批准（行为不变）",
            DecisionClass::Classify => "归入待分类队列，不猜",
            DecisionClass::Urgency => "按普通处理",
            DecisionClass::Anomaly => "按可疑处理并记日志",
            DecisionClass::Route => "改走远端（Agnes），这一轮照常办",
        }
    }

    /// 人类可读的类别名。
    pub fn label(self) -> &'static str {
        match self {
            DecisionClass::Interrupt => "打扰/通知",
            DecisionClass::Escalate => "升级/人工介入",
            DecisionClass::Irreversible => "不可逆动作",
            DecisionClass::Classify => "分类/归档",
            DecisionClass::Urgency => "紧急度",
            DecisionClass::Anomaly => "异常识别",
            DecisionClass::Route => "路由/选端点",
        }
    }
}

/// 一次决策的最终走向。
#[derive(Debug, Clone, PartialEq)]
pub enum DecisionOutcome {
    /// 模型正常作答。**这仍然只是策略的输入**，不是最终决定。
    Decided(DecisionResult),
    /// 模型不可用，已降级。`action` 是保守动作，`reason` 必须留痕。
    Degraded {
        class: DecisionClass,
        direction: DegradationDirection,
        action: &'static str,
        reason: String,
    },
}

impl DecisionOutcome {
    pub fn is_degraded(&self) -> bool {
        matches!(self, DecisionOutcome::Degraded { .. })
    }

    /// 台账里要写的事件载荷：降级必须显式可见。
    pub fn ledger_data(&self) -> serde_json::Value {
        match self {
            DecisionOutcome::Decided(r) => serde_json::json!({
                "degraded": false,
                "model": r.model,
            }),
            DecisionOutcome::Degraded {
                class,
                direction,
                action,
                reason,
            } => serde_json::json!({
                "degraded": true,
                "class": class,
                "direction": direction,
                "action": action,
                "reason": reason,
            }),
        }
    }
}

/// 熔断前的连续失败阈值。
pub const DEFAULT_FAILURE_THRESHOLD: u32 = 3;

/// 把一次决策写进台账。
///
/// **审计对必须被边界包住**（ADR D6）：`decision_asked` 与 `decision_decided`
/// 若落在边界之外，就与崩溃残尾无法区分，reload 时会被静默丢弃。所以这里
/// 开一个边界、写两条、关边界，任何一步失败都不会留下半对记录。
///
/// 各字段由调用方**如实**填入。早先版本要求传一个 [`DecisionOutcome`]，
/// 结果调用方为了省事无论真实结果如何都构造 `Degraded`——台账于是把成功的判断
/// 也标成降级。**宁可多几个参数，也不要让"如实上报"变成需要绕过的障碍。**
#[allow(clippy::too_many_arguments)]
pub fn record_decision(
    ledger: &mut crate::ledger::Ledger,
    class: DecisionClass,
    degraded: bool,
    action: &str,
    reason: &str,
    model: Option<&str>,
    question_ids: &[String],
) -> Result<u64, crate::ledger::LedgerError> {
    use crate::ledger::EventKind;

    let mut span = ledger.begin_span()?;
    let span_id = span.id();

    span.ledger().append(
        EventKind::DecisionAsked,
        None,
        serde_json::json!({
            "class": class,
            "questions": question_ids,
        }),
    )?;

    span.ledger().append(
        EventKind::DecisionDecided,
        None,
        serde_json::json!({
            "class": class,
            "degraded": degraded,
            "action": action,
            "reason": reason,
            "model": model,
        }),
    )?;

    span.close()?;
    Ok(span_id)
}

/// 一条决策的**可选**留痕口。
///
/// ## 为什么是一个类型别名，而不是"每个调用点自己判断"
///
/// 决策模型的调用点有六处（输入分类 / 路由回落 / 召回门控 / 工具审批 /
/// 任务决策点 / 陪伴介入与通知分流），其中**四处**此前一条痕都不留：
/// 问过了、用了答案、然后什么都没写下来。后果是台账回答不了
/// "这一步到底问没问过模型、它是不是弃权了"——而 D128 正是被这种
/// 假账骗过去的（终端标签、台账、成本三处都在说模型被调用了，实际一次都没调）。
///
/// 把它写成一个**具名的**口子，是为了让"留痕"这件事在签名上就看得见：
/// `DecisionSink<'_>` 出现在参数里，读代码的人不需要翻实现就知道
/// "这一处会把判断记进台账"。
///
/// ## `None` 是什么意思
///
/// **没挂口子 = 一个字节的 I/O 都没有**：不是"写进一个丢掉的地方"，
/// 也不是"先分配一个结构再扔掉"。core 里这些函数被近千条测试反复调用，
/// 默认真去开一本台账的话，每条测试都会凭空多出一批审计事件。
/// 所以默认永远是 `None`（见各处的 `with_*_ledger` 构造方法），
/// 由真正知道自己跑在哪的调用方显式挂上。
pub type DecisionSink<'a> = Option<&'a mut crate::ledger::Ledger>;

/// 往**可选**的留痕口写一条决策。没挂口子就什么都不做。
///
/// 这是六处调用点共用的收口：`class` / `degraded` / `action` / `reason`
/// 全部由调用方**如实**填（理由见 [`record_decision`] 的文档——
/// 早先版本要求传一个 `DecisionOutcome`，结果调用方为了省事一律构造
/// `Degraded`，台账于是把成功的判断也标成降级）。
///
/// 返回值只保留"成不成"：边界号是 [`record_decision`] 的细节，
/// 这几处调用点没有一处需要它（陪伴那一处由 `agent::run_cycle` 自己记账）。
#[allow(clippy::too_many_arguments)]
pub fn record_decision_optional(
    sink: DecisionSink<'_>,
    class: DecisionClass,
    degraded: bool,
    action: &str,
    reason: &str,
    model: Option<&str>,
    question_ids: &[String],
) -> Result<(), crate::ledger::LedgerError> {
    let Some(ledger) = sink else {
        return Ok(());
    };
    record_decision(ledger, class, degraded, action, reason, model, question_ids).map(|_| ())
}

/// 决策引擎：包住一个 [`Decider`]，负责降级与熔断。
#[derive(Debug)]
pub struct DecisionEngine<D: Decider> {
    decider: D,
    class: DecisionClass,
    consecutive_failures: u32,
    threshold: u32,
    /// 置信度下限。低于它视为"模型自己不确定"，同样降级。
    min_confidence: Option<f64>,
}

impl<D: Decider> DecisionEngine<D> {
    pub fn new(decider: D, class: DecisionClass) -> Self {
        Self {
            decider,
            class,
            consecutive_failures: 0,
            threshold: DEFAULT_FAILURE_THRESHOLD,
            min_confidence: None,
        }
    }

    pub fn with_threshold(mut self, threshold: u32) -> Self {
        self.threshold = threshold.max(1);
        self
    }

    /// 设置置信度下限。低于它按"模型不确定"处理。
    ///
    /// ⚠️ 只应在**用自己的数据校准过**之后设置（ADR §7.3 第 5 条）。
    pub fn with_min_confidence(mut self, c: f64) -> Self {
        self.min_confidence = Some(c.clamp(0.0, 1.0));
        self
    }

    pub fn class(&self) -> DecisionClass {
        self.class
    }

    /// 熔断是否已打开（连续失败超阈值）。
    pub fn circuit_open(&self) -> bool {
        self.consecutive_failures >= self.threshold
    }

    pub fn consecutive_failures(&self) -> u32 {
        self.consecutive_failures
    }

    /// 执行一次决策。**永不返回 Err**——失败一律转成降级，
    /// 因为调用方必须拿到一个明确的、可留痕的走向。
    pub fn decide(&mut self, req: &DecisionRequest) -> DecisionOutcome {
        if self.circuit_open() {
            return self.degrade("熔断已打开：连续失败达到阈值，暂停调用模型");
        }

        match self.decider.decide(req) {
            Ok(result) => {
                // 校验放在引擎里，而不是某个 Decider 实现里。
                //
                // 理由：校验必须是**无条件必经之路**。如果把它塞进 LayaDecider，
                // 那么换一个模型适配器（或测试桩）就会漏掉——而未校验的模型
                // 返回值恰恰是最危险的东西（比如编出一个没声明的选项）。
                if let Err(e) = laya::validate(req, &result) {
                    self.consecutive_failures += 1;
                    return self.degrade(&format!("模型返回非法结果：{e}"));
                }

                // 模型自己不确定 → 同样按降级处理
                if let Some(min) = self.min_confidence {
                    let low = result
                        .answers
                        .values()
                        .filter_map(|a| a.confidence())
                        .any(|c| c < min);
                    if low {
                        self.consecutive_failures += 1;
                        return self.degrade(&format!("模型置信度低于阈值 {min:.2}，按不确定处理"));
                    }
                }
                self.consecutive_failures = 0;
                DecisionOutcome::Decided(result)
            }
            Err(e) => {
                self.consecutive_failures += 1;
                let mut reason = format!("决策模型不可用：{e}");
                if self.circuit_open() {
                    reason.push_str(&format!(
                        "（连续失败 {} 次，已熔断；需人工重置）",
                        self.consecutive_failures
                    ));
                }
                self.degrade(&reason)
            }
        }
    }

    /// 人工重置熔断。
    pub fn reset_circuit(&mut self) {
        self.consecutive_failures = 0;
    }

    fn degrade(&self, reason: &str) -> DecisionOutcome {
        DecisionOutcome::Degraded {
            class: self.class,
            direction: self.class.direction(),
            action: self.class.degraded_action(),
            reason: reason.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req() -> DecisionRequest {
        DecisionRequest::new(
            serde_json::json!({ "subject": "重复扣款" }),
            vec![Question::noul("churn", "客户是否明确威胁要取消？")],
        )
    }

    #[test]
    fn interrupt_and_escalate_degrade_in_opposite_directions() {
        // 这是整个降级策略的枢纽：方向必须相反
        assert_eq!(
            DecisionClass::Interrupt.direction(),
            DegradationDirection::FailClosed,
            "打扰类不确定就不打扰"
        );
        assert_eq!(
            DecisionClass::Escalate.direction(),
            DegradationDirection::FailOpen,
            "安全类不确定就叫人"
        );
    }

    #[test]
    fn irreversible_does_not_participate_in_degradation() {
        assert_eq!(
            DecisionClass::Irreversible.direction(),
            DegradationDirection::NotApplicable
        );
    }

    #[test]
    fn a_route_degradation_falls_open_to_the_working_endpoint() {
        // 本地模型没就绪时**不拦这一轮**，只是换个端点把它办完。
        // 两边代价不对称：错判成远端多花几分钱，错判成本地则这一轮必然失败。
        assert_eq!(
            DecisionClass::Route.direction(),
            DegradationDirection::FailOpen,
            "本地槽位用不了时该往能干活的那边倒"
        );
        let action = DecisionClass::Route.degraded_action();
        assert!(action.contains("远端"), "要说清落到了哪边: {action}");
        // 它必须和"分类"分得开：混在一起的话，事后按类别翻台账的人
        // 会把端点替换读成分类结果
        assert_ne!(
            DecisionClass::Route.label(),
            DecisionClass::Classify.label()
        );
    }

    #[test]
    fn degradation_is_always_visible_in_ledger_data() {
        let mut engine =
            DecisionEngine::new(StubDecider::failing("连接被拒绝"), DecisionClass::Interrupt);
        let out = engine.decide(&req());
        let data = out.ledger_data();
        assert_eq!(data["degraded"], serde_json::json!(true));
        assert!(data["reason"].as_str().unwrap().contains("连接被拒绝"));
        assert!(data["action"].as_str().unwrap().contains("不打扰"));
    }

    #[test]
    fn circuit_opens_after_threshold_and_stops_calling() {
        let mut engine = DecisionEngine::new(StubDecider::failing("boom"), DecisionClass::Escalate)
            .with_threshold(2);

        assert!(!engine.circuit_open());
        let _ = engine.decide(&req());
        assert_eq!(engine.consecutive_failures(), 1);
        assert!(!engine.circuit_open());

        let _ = engine.decide(&req());
        assert!(engine.circuit_open(), "达到阈值后应熔断");

        // 熔断后再调用不应继续打模型
        let out = engine.decide(&req());
        match out {
            DecisionOutcome::Degraded { reason, .. } => {
                assert!(reason.contains("熔断"), "应说明已熔断: {reason}")
            }
            _ => panic!("熔断后必须是降级"),
        }

        engine.reset_circuit();
        assert!(!engine.circuit_open());
    }

    #[test]
    fn success_resets_failure_counter() {
        let mut engine = DecisionEngine::new(StubDecider::succeeding(), DecisionClass::Classify);
        let out = engine.decide(&req());
        assert!(!out.is_degraded());
        assert_eq!(engine.consecutive_failures(), 0);
    }

    #[test]
    fn low_confidence_degrades_and_counts_as_failure() {
        let stub = StubDecider::succeeding().with_noul("churn", 0.52);
        let mut engine = DecisionEngine::new(stub, DecisionClass::Anomaly).with_min_confidence(0.7);
        let out = engine.decide(&req());
        assert!(out.is_degraded(), "低置信度应按不确定处理");
        // 降级方向仍按类别走：异常识别是 fail-open
        match out {
            DecisionOutcome::Degraded { direction, .. } => {
                assert_eq!(direction, DegradationDirection::FailOpen)
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn confidence_of_noul_uses_the_larger_side() {
        let high = Answer {
            noul: Some(0.9),
            ..Default::default()
        };
        assert!((high.confidence().unwrap() - 0.9).abs() < 1e-9);

        // P(true)=0.1 时，"不是真的"才是高置信的一侧
        let low = Answer {
            noul: Some(0.1),
            ..Default::default()
        };
        assert!((low.confidence().unwrap() - 0.9).abs() < 1e-9);
    }

    #[test]
    fn record_decision_writes_an_audit_pair_inside_one_span() {
        use crate::ledger::{EventKind, Ledger};
        let mut p = std::env::temp_dir();
        p.push(format!("yunxi-decide-span-{}.jsonl", std::process::id()));
        let _ = std::fs::remove_file(&p);

        let mut ledger = Ledger::open(&p).expect("打开台账");
        let span_id = record_decision(
            &mut ledger,
            DecisionClass::Interrupt,
            true,
            "延后聚合，本轮不打扰",
            "模型不可用",
            None,
            &["intervention".to_string()],
        )
        .expect("写决策");

        let events = ledger.events();
        assert_eq!(events.len(), 2, "一次决策恰好两条事件");
        assert_eq!(events[0].kind, EventKind::DecisionAsked);
        assert_eq!(events[1].kind, EventKind::DecisionDecided);
        assert!(
            events.iter().all(|e| e.span == Some(span_id)),
            "审计对必须落在同一个边界内，否则 reload 时会被当崩溃残尾丢弃"
        );
        assert!(!ledger.has_open_span(), "写完应关闭边界");
        // degraded 必须如实落盘——早先版本在这里写死了"降级"
        assert_eq!(events[1].data["degraded"], serde_json::json!(true));

        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn record_decision_does_not_mislabel_a_successful_judgment_as_degraded() {
        use crate::ledger::{EventKind, Ledger};
        let mut p = std::env::temp_dir();
        p.push(format!("yunxi-decide-ok-{}.jsonl", std::process::id()));
        let _ = std::fs::remove_file(&p);

        let mut ledger = Ledger::open(&p).expect("打开台账");
        let _ = record_decision(
            &mut ledger,
            DecisionClass::Interrupt,
            false,
            "主动开口",
            "模型判断：主动开口",
            Some("laya-multilingual"),
            &["intervention".to_string()],
        )
        .expect("写决策");

        let decided = ledger
            .events()
            .iter()
            .find(|e| e.kind == EventKind::DecisionDecided)
            .expect("应有 decided 事件");
        assert_eq!(
            decided.data["degraded"],
            serde_json::json!(false),
            "成功的判断不能被标成降级"
        );
        assert_eq!(
            decided.data["model"],
            serde_json::json!("laya-multilingual")
        );

        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn record_decision_is_rejected_without_leaving_partial_state() {
        use crate::ledger::{EventKind, Ledger};
        // 直接写审计事件（无边界）必须被拒——这是台账层的硬约束
        let mut p = std::env::temp_dir();
        p.push(format!("yunxi-decide-nospan-{}.jsonl", std::process::id()));
        let _ = std::fs::remove_file(&p);

        let mut ledger = Ledger::open(&p).expect("打开台账");
        let err = ledger.append(EventKind::DecisionAsked, None, serde_json::json!({}));
        assert!(err.is_err(), "边界外的审计事件必须被拒绝");
        assert!(ledger.events().is_empty(), "拒绝时不得写入任何内容");

        let _ = std::fs::remove_file(&p);
    }
}
