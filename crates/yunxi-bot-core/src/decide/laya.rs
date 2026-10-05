//! 决策模型接入：`Decider` 抽象、Laya 的本地 sidecar 适配器，以及测试用 Stub。
//!
//! ## 有线契约
//!
//! Rust 侧只认一个固定形状的 JSON 契约，具体模型库的差异全部由 sidecar 吸收：
//!
//! ```text
//! POST /decide
//! { "state": {...},
//!   "questions": [ {"id":"q1","kind":{"type":"noul"},"instructions":"..."} ] }
//!
//! → 200
//! { "answers": { "q1": {"noul": 0.83} },
//!   "model": "laya-multilingual" }
//! ```
//!
//! 这样做的理由：上游 `laya` 包的服务端 API 可能随版本变化，把差异关在
//! sidecar 里，Rust 侧就不必跟着改。仓库附带 `sidecar/laya_server.py` 实现该契约。
//!
//! ## 不信任模型返回值
//!
//! 与审批结果同一条原则：**模型返回的内容必须校验**。选项不在声明的判据里、
//! 概率越界、缺少答案——一律判为非法响应并降级，绝不"将就用"。

use std::collections::BTreeMap;

use super::http;
use super::{Answer, DecisionRequest, DecisionResult, QuestionKind};

/// 决策模型调用失败的形态。对应 ADR §7.1 的六种失败形态。
#[derive(Debug)]
pub enum DecisionError {
    /// 1. 服务未启动 / 端口不通
    Unreachable(String),
    /// 2. 超时
    Timeout,
    /// 3/5. 返回非法（schema 校验失败 / 概率缺失 / 越界）
    Invalid(String),
    /// 4. 服务端错误
    Server(String),
}

impl std::fmt::Display for DecisionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DecisionError::Unreachable(m) => write!(f, "服务不可达: {m}"),
            DecisionError::Timeout => write!(f, "调用超时"),
            DecisionError::Invalid(m) => write!(f, "响应非法: {m}"),
            DecisionError::Server(m) => write!(f, "服务端错误: {m}"),
        }
    }
}

impl std::error::Error for DecisionError {}

impl From<http::HttpError> for DecisionError {
    fn from(e: http::HttpError) -> Self {
        match e {
            http::HttpError::Timeout => DecisionError::Timeout,
            http::HttpError::Connect(m) => DecisionError::Unreachable(m),
            http::HttpError::Status(c, m) => DecisionError::Server(format!("{c}: {m}")),
            http::HttpError::Malformed(m) | http::HttpError::Io(m) => DecisionError::Invalid(m),
            http::HttpError::TooLarge => DecisionError::Invalid("响应体过大".into()),
        }
    }
}

/// 决策模型的能力面。实现者可以是本地 sidecar、HTTP 服务或测试桩。
pub trait Decider: Send + Sync {
    fn decide(&self, req: &DecisionRequest) -> Result<DecisionResult, DecisionError>;
}

/// 指向本地 Laya sidecar 的决策器。
#[derive(Debug, Clone)]
pub struct LayaDecider {
    endpoint: String,
    timeout_ms: u64,
}

/// 决策调用的默认超时预算。
///
/// 官方口径本地推理可到毫秒级，这里给足余量（约 3 个数量级），
/// 但绝不无限等——常驻进程不能被一次卡住的调用拖死。
pub const DEFAULT_TIMEOUT_MS: u64 = 2_000;

impl LayaDecider {
    /// `endpoint` 形如 `http://127.0.0.1:17870/decide`。非回环地址会被拒绝。
    pub fn new(endpoint: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into(),
            timeout_ms: DEFAULT_TIMEOUT_MS,
        }
    }

    pub fn with_timeout_ms(mut self, ms: u64) -> Self {
        self.timeout_ms = ms;
        self
    }

    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }
}

/// 响应体形状。
#[derive(Debug, serde::Deserialize)]
struct WireResponse {
    answers: BTreeMap<String, Answer>,
    #[serde(default)]
    model: Option<String>,
}

impl Decider for LayaDecider {
    fn decide(&self, req: &DecisionRequest) -> Result<DecisionResult, DecisionError> {
        let body = serde_json::to_string(req)
            .map_err(|e| DecisionError::Invalid(format!("请求无法序列化: {e}")))?;

        let text = http::post_json(&self.endpoint, &body, self.timeout_ms)?;

        let wire: WireResponse = serde_json::from_str(&text)
            .map_err(|e| DecisionError::Invalid(format!("响应不是预期结构: {e}")))?;

        let result = DecisionResult {
            answers: wire.answers,
            model: wire.model.unwrap_or_else(|| "unknown".into()),
        };

        // 注意：**校验不在这里做**。它是引擎的无条件必经步骤
        // （见 `DecisionEngine::decide`），放在适配器里会让别的适配器漏掉。
        Ok(result)
    }
}

/// 校验模型返回的内容。
///
/// **这是"不信任模型返回值"的落点。** 每条都必须过：
///
/// - 每个问题都有答案（缺答案 = 模型没答完，不能猜）；
/// - `choice` 必须落在声明的判据里（模型编出一个新选项是最危险的情况）；
/// - `score` 的等级必须落在声明的等级里；
/// - 概率全部在 `[0, 1]`；
/// - 分布非空时，各概率之和应接近 1。
pub fn validate(req: &DecisionRequest, result: &DecisionResult) -> Result<(), DecisionError> {
    for q in &req.questions {
        let Some(a) = result.answers.get(&q.id) else {
            return Err(DecisionError::Invalid(format!("问题 {} 没有答案", q.id)));
        };

        match &q.kind {
            QuestionKind::Choice { criteria } => {
                let Some(choice) = a.choice.as_deref() else {
                    return Err(DecisionError::Invalid(format!(
                        "问题 {} 是 choice，但没给出选项",
                        q.id
                    )));
                };
                if !criteria.contains_key(choice) {
                    return Err(DecisionError::Invalid(format!(
                        "问题 {} 返回了未声明的选项 {choice:?}（允许: {:?}）",
                        q.id,
                        criteria.keys().collect::<Vec<_>>()
                    )));
                }
                check_distribution(
                    &q.id,
                    &a.distribution,
                    Some(criteria.keys().map(String::as_str).collect()),
                )?;
            }
            QuestionKind::Score { levels } => {
                if let Some(chosen) = a.choice.as_deref() {
                    if !levels.iter().any(|l| l == chosen) {
                        return Err(DecisionError::Invalid(format!(
                            "问题 {} 返回了未声明的等级 {chosen:?}",
                            q.id
                        )));
                    }
                }
                check_distribution(
                    &q.id,
                    &a.distribution,
                    Some(levels.iter().map(String::as_str).collect()),
                )?;
            }
            QuestionKind::Noul => {
                let Some(p) = a.noul else {
                    return Err(DecisionError::Invalid(format!(
                        "问题 {} 是 noul，但没给出概率",
                        q.id
                    )));
                };
                if !(0.0..=1.0).contains(&p) || !p.is_finite() {
                    return Err(DecisionError::Invalid(format!(
                        "问题 {} 的概率 {p} 越界",
                        q.id
                    )));
                }
            }
        }
    }
    Ok(())
}

fn check_distribution(
    qid: &str,
    dist: &BTreeMap<String, f64>,
    allowed: Option<Vec<&str>>,
) -> Result<(), DecisionError> {
    if dist.is_empty() {
        return Ok(());
    }
    let mut sum = 0.0;
    for (k, v) in dist {
        if !(0.0..=1.0).contains(v) || !v.is_finite() {
            return Err(DecisionError::Invalid(format!(
                "问题 {qid} 的分布项 {k}={v} 越界"
            )));
        }
        if let Some(allowed) = &allowed {
            if !allowed.contains(&k.as_str()) {
                return Err(DecisionError::Invalid(format!(
                    "问题 {qid} 的分布含未声明项 {k:?}"
                )));
            }
        }
        sum += v;
    }
    if (sum - 1.0).abs() > 0.05 {
        return Err(DecisionError::Invalid(format!(
            "问题 {qid} 的分布之和为 {sum:.3}，不接近 1"
        )));
    }
    Ok(())
}

/// 测试用决策桩。可配置成功/失败与固定答案。
#[derive(Debug, Clone)]
pub struct StubDecider {
    fail_with: Option<String>,
    noul: BTreeMap<String, f64>,
    choice: BTreeMap<String, String>,
    confidence: Option<f64>,
    calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl StubDecider {
    /// 总是成功，返回空答案。
    pub fn succeeding() -> Self {
        Self {
            fail_with: None,
            noul: BTreeMap::new(),
            choice: BTreeMap::new(),
            confidence: None,
            calls: Default::default(),
        }
    }

    /// 总是失败。
    pub fn failing(msg: impl Into<String>) -> Self {
        Self {
            fail_with: Some(msg.into()),
            ..Self::succeeding()
        }
    }

    pub fn with_noul(mut self, id: &str, p: f64) -> Self {
        self.noul.insert(id.to_string(), p);
        self
    }

    pub fn with_choice(mut self, id: &str, c: &str) -> Self {
        self.choice.insert(id.to_string(), c.to_string());
        self
    }

    /// 调用次数，用于验证熔断后确实不再打模型。
    pub fn calls(&self) -> usize {
        self.calls.load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl Decider for StubDecider {
    fn decide(&self, req: &DecisionRequest) -> Result<DecisionResult, DecisionError> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);

        if let Some(msg) = &self.fail_with {
            return Err(DecisionError::Unreachable(msg.clone()));
        }

        let mut answers = BTreeMap::new();
        for q in &req.questions {
            let mut a = Answer::default();
            if let Some(p) = self.noul.get(&q.id) {
                a.noul = Some(*p);
            }
            if let Some(c) = self.choice.get(&q.id) {
                a.choice = Some(c.clone());
            }
            // 无配置时给一个合理的默认值，保证 validate 能通过
            if a.noul.is_none() && a.choice.is_none() {
                match &q.kind {
                    QuestionKind::Noul => a.noul = Some(self.confidence.unwrap_or(0.8)),
                    QuestionKind::Choice { criteria } => {
                        a.choice = criteria.keys().next().cloned();
                    }
                    QuestionKind::Score { levels } => {
                        a.choice = levels.first().cloned();
                    }
                }
            }
            answers.insert(q.id.clone(), a);
        }

        Ok(DecisionResult {
            answers,
            model: "stub".into(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decide::Question;

    fn req() -> DecisionRequest {
        DecisionRequest::new(
            serde_json::json!({"subject": "x"}),
            vec![
                Question::choice(
                    "dept",
                    "归属哪个团队？",
                    &[("billing", "账单"), ("tech", "故障")],
                ),
                Question::noul("churn", "客户是否明确威胁要取消？"),
            ],
        )
    }

    #[test]
    fn rejects_choice_outside_declared_criteria() {
        // 模型编出一个未声明的选项是最危险的情况：绝不能将就
        let r = DecisionResult {
            answers: BTreeMap::from([(
                "dept".to_string(),
                Answer {
                    choice: Some("marketing".into()),
                    ..Default::default()
                },
            )]),
            model: "t".into(),
        };
        let err = validate(&req(), &r).unwrap_err();
        assert!(format!("{err}").contains("未声明"), "{err}");
    }

    #[test]
    fn rejects_missing_answer() {
        let r = DecisionResult {
            answers: BTreeMap::new(),
            model: "t".into(),
        };
        assert!(validate(&req(), &r).is_err());
    }

    #[test]
    fn rejects_out_of_range_probability() {
        let r = DecisionResult {
            answers: BTreeMap::from([
                (
                    "dept".to_string(),
                    Answer {
                        choice: Some("billing".into()),
                        ..Default::default()
                    },
                ),
                (
                    "churn".to_string(),
                    Answer {
                        noul: Some(1.7),
                        ..Default::default()
                    },
                ),
            ]),
            model: "t".into(),
        };
        assert!(validate(&req(), &r).is_err(), "越界概率必须拒绝");
    }

    #[test]
    fn rejects_distribution_not_summing_to_one() {
        let r = DecisionResult {
            answers: BTreeMap::from([
                (
                    "dept".to_string(),
                    Answer {
                        choice: Some("billing".into()),
                        distribution: BTreeMap::from([
                            ("billing".to_string(), 0.2),
                            ("tech".to_string(), 0.2),
                        ]),
                        ..Default::default()
                    },
                ),
                (
                    "churn".to_string(),
                    Answer {
                        noul: Some(0.5),
                        ..Default::default()
                    },
                ),
            ]),
            model: "t".into(),
        };
        assert!(validate(&req(), &r).is_err());
    }

    #[test]
    fn accepts_a_well_formed_answer() {
        let r = DecisionResult {
            answers: BTreeMap::from([
                (
                    "dept".to_string(),
                    Answer {
                        choice: Some("billing".into()),
                        ..Default::default()
                    },
                ),
                (
                    "churn".to_string(),
                    Answer {
                        noul: Some(0.83),
                        ..Default::default()
                    },
                ),
            ]),
            model: "t".into(),
        };
        assert!(validate(&req(), &r).is_ok());
    }

    #[test]
    fn stub_failure_maps_to_unreachable() {
        let stub = StubDecider::failing("连接被拒绝");
        let err = stub.decide(&req()).unwrap_err();
        assert!(matches!(err, DecisionError::Unreachable(_)));
    }

    #[test]
    fn non_loopback_endpoint_is_refused_before_any_io() {
        let d = LayaDecider::new("http://evil.example.com:80/decide");
        let err = d.decide(&req()).unwrap_err();
        assert!(
            format!("{err}").contains("非回环"),
            "私人 state 不得离开本机: {err}"
        );
    }
}
