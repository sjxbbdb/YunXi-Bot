//! Agnes AI 接入（OpenAI 兼容）。
//!
//! - Base URL：`https://api.agnes-ai.cn/v1`
//! - 端点：`POST /v1/chat/completions`
//! - 认证：`Authorization: Bearer <key>`
//! - 默认模型：`agnes-3.0-flash`（512K 上下文、支持工具调用、面向 Agent 任务）
//!
//! 文档：<https://wiki.agnes-ai.cn/zh-Hans/docs/overview>

use std::path::{Path, PathBuf};
use std::time::Duration;

use super::router::Thinking;
use super::{RateLimiter, ThinkError, ThinkRequest, ThinkResponse, Thinker, Usage};

/// 默认 Base URL。
pub const DEFAULT_BASE_URL: &str = "https://api.agnes-ai.cn/v1";
/// 默认模型。3.0 Flash 面向 Agent 任务与工具调用，是本项目的主力。
pub const DEFAULT_MODEL: &str = "agnes-3.0-flash";
/// 免费档文本模型的实际 RPM（2026-09 起从 20 下调到 10）。
pub const FREE_TIER_RPM: u32 = 10;

/// API 密钥。
///
/// **刻意不实现 `Display`，`Debug` 也只打印前 6 位。**
/// 密钥一旦被打进日志、错误信息或台账，就很难收回——所以从类型上堵住这条路，
/// 而不是靠"记得别打印"。要取原文只能调 [`ApiKey::expose`]，那是一个显眼的动作。
#[derive(Clone)]
pub struct ApiKey(String);

impl ApiKey {
    pub fn new(raw: impl Into<String>) -> Self {
        Self(raw.into().trim().to_string())
    }

    /// 从环境变量或本地文件读取。
    ///
    /// 顺序：`YUNXI_BOT_AGNES_KEY` 环境变量 → `key_file`。
    /// 文件方式优先于命令行参数，**避免密钥出现在进程列表里**。
    pub fn load(key_file: &Path) -> Result<Self, ThinkError> {
        if let Ok(v) = std::env::var("YUNXI_BOT_AGNES_KEY") {
            if !v.trim().is_empty() {
                return Ok(Self::new(v));
            }
        }
        let text = std::fs::read_to_string(key_file).map_err(|e| {
            ThinkError::Auth(format!(
                "读不到密钥文件 {}（可设 YUNXI_BOT_AGNES_KEY 覆盖）: {e}",
                key_file.display()
            ))
        })?;
        let key = text.trim();
        if key.is_empty() {
            return Err(ThinkError::Auth(format!(
                "密钥文件为空: {}",
                key_file.display()
            )));
        }
        Ok(Self::new(key))
    }

    /// 取出原文。**调用点应当显而易见**——目前只有拼装认证头时会用到。
    pub fn expose(&self) -> &str {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl std::fmt::Debug for ApiKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let head: String = self.0.chars().take(6).collect();
        write!(f, "ApiKey({head}…已隐藏)")
    }
}

/// Agnes 客户端配置。
#[derive(Debug, Clone)]
pub struct ThinkerConfig {
    pub base_url: String,
    pub model: String,
    pub timeout: Duration,
    /// 每分钟请求数上限。默认按免费档 10 处理。
    pub rpm: u32,
    /// 密钥文件名（相对 `<home>/secrets/`）。Agnes 与 DeepSeek 各一份。
    pub key_file: &'static str,
    /// 思考模式。DeepSeek **默认开**，那是输出 token 的大头。
    pub thinking: Thinking,
}

impl Default for ThinkerConfig {
    fn default() -> Self {
        Self {
            base_url: DEFAULT_BASE_URL.to_string(),
            model: DEFAULT_MODEL.to_string(),
            // 长上下文 + 大输出的请求可能要跑一会儿；给足余量但不无限等
            timeout: Duration::from_secs(120),
            rpm: FREE_TIER_RPM,
            key_file: "agnes.key",
            thinking: Thinking::ServerDefault,
        }
    }
}

impl ThinkerConfig {
    /// DeepSeek Flash。
    ///
    /// `thinking` 这里给 [`Thinking::Enabled`]，含义是**"这个端点支持思考"**，
    /// 不是"每个请求都开"——实测开思考输出 token 是关掉的 2.5 倍。
    /// 实际发什么由路由按任务类型逐任务决定（见 [`super::router::ReasoningEffort`]），
    /// 调用方拿到 [`super::router::Routing`] 后应用 `routing.thinking_field()` 覆盖。
    pub fn deepseek() -> Self {
        Self {
            base_url: "https://api.deepseek.com/v1".into(),
            model: "deepseek-flash".into(),
            timeout: Duration::from_secs(180),
            rpm: 60,
            key_file: "deepseek.key",
            thinking: Thinking::Enabled,
        }
    }

    /// Agnes 3.0 Flash。
    pub fn agnes() -> Self {
        Self::default()
    }
}

/// 默认的密钥文件位置：`<home>/secrets/agnes.key`。
///
/// 放在**仓库之外**（`%LOCALAPPDATA%\YunXiBot`），并在 `.gitignore` 里用
/// `*.key` / `secrets/` 兜底。
pub fn key_file_for(home: &Path, name: &str) -> PathBuf {
    home.join("secrets").join(name)
}

/// 默认（Agnes）的密钥文件。
pub fn default_key_file(home: &Path) -> PathBuf {
    key_file_for(home, "agnes.key")
}

/// Agnes 客户端。
pub struct OpenAiThinker {
    config: ThinkerConfig,
    api_key: ApiKey,
    agent: ureq::Agent,
    limiter: RateLimiter,
}

impl OpenAiThinker {
    pub fn new(config: ThinkerConfig, api_key: ApiKey) -> Self {
        let agent = ureq::AgentBuilder::new()
            .timeout(config.timeout)
            .user_agent(concat!("yunxi-bot/", env!("CARGO_PKG_VERSION")))
            .build();
        let limiter = RateLimiter::with_rpm(config.rpm);
        Self {
            config,
            api_key,
            agent,
            limiter,
        }
    }

    /// 走默认配置 + 默认密钥文件构造。
    pub fn from_home(home: &Path, config: ThinkerConfig) -> Result<Self, ThinkError> {
        let key = ApiKey::load(&key_file_for(home, config.key_file))?;
        Ok(Self::new(config, key))
    }

    pub fn config(&self) -> &ThinkerConfig {
        &self.config
    }

    /// 距离下次可发送还需多久。`None` 表示现在就能发。
    pub fn throttle_wait(&self) -> Option<Duration> {
        // 只探测不占用
        self.limiter.peek_wait()
    }

    /// 两次调用之间的最小间隔（由 RPM 推得）。
    pub fn min_interval(&self) -> Duration {
        self.limiter.min_interval()
    }

    fn endpoint(&self) -> String {
        format!(
            "{}/chat/completions",
            self.config.base_url.trim_end_matches('/')
        )
    }

    /// 把 HTTP 状态码映射成有语义的错误。
    ///
    /// 映射依据官方错误码文档。**关键是区分"重试有用"与"重试无用"**：
    /// 401/402/403/404/400/413/422 反复重试只会浪费配额并刷屏日志。
    fn map_status(status: u16, detail: String, retry_after: Option<Duration>) -> ThinkError {
        match status {
            401 => ThinkError::Auth(detail),
            402 => ThinkError::Payment(detail),
            403 => ThinkError::Forbidden(detail),
            404 => ThinkError::NotFound(detail),
            429 => ThinkError::RateLimited {
                detail,
                retry_after,
            },
            408 | 409 => ThinkError::Transient {
                status: Some(status),
                detail,
            },
            400 | 413 | 415 | 422 => ThinkError::BadRequest { status, detail },
            500..=599 => ThinkError::Transient {
                status: Some(status),
                detail,
            },
            _ => ThinkError::Transient {
                status: Some(status),
                detail,
            },
        }
    }
}

/// 有线响应形状（OpenAI 兼容）。
#[derive(Debug, serde::Deserialize)]
struct WireResponse {
    #[serde(default)]
    model: String,
    #[serde(default)]
    choices: Vec<WireChoice>,
    #[serde(default)]
    usage: Usage,
}

#[derive(Debug, serde::Deserialize)]
struct WireChoice {
    #[serde(default)]
    message: WireMessage,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Default, serde::Deserialize)]
struct WireMessage {
    /// **可以是 `null`**：模型只返回工具调用时 `content` 就是 null。
    /// 早先把 `None` 当畸形响应，会在工具调用路径上误报失败。
    #[serde(default)]
    content: Option<String>,
    /// 思考过程。不开思考时不存在。
    #[serde(default)]
    reasoning_content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<serde_json::Value>,
}

impl Thinker for OpenAiThinker {
    fn think(&self, req: &ThinkRequest) -> Result<ThinkResponse, ThinkError> {
        if self.api_key.is_empty() {
            return Err(ThinkError::Auth("密钥为空".into()));
        }
        if req.messages.is_empty() {
            return Err(ThinkError::BadRequest {
                status: 0,
                detail: "消息列表为空".into(),
            });
        }

        // 本地限流：拿不到配额就如实报告，让调用方决定跳过还是等待。
        // 不在这里 sleep——常驻循环不该为一次远端调用卡住整轮调度。
        if let Some(wait) = self.limiter.try_acquire() {
            return Err(ThinkError::LocalThrottle { wait });
        }

        let mut body = serde_json::json!({
            "model": self.config.model,
            "messages": req
                .messages
                .iter()
                .map(|m| serde_json::json!({ "role": m.role.as_str(), "content": m.content }))
                .collect::<Vec<_>>(),
        });
        if let Some(n) = req.max_tokens {
            body["max_tokens"] = serde_json::json!(n);
        }
        if let Some(t) = req.temperature {
            // ⚠️ 思考模式下 temperature 不生效（官方文档：设了不报错，但也不生效）
            body["temperature"] = serde_json::json!(t);
        }
        // 思考模式开关。**请求上的设置优先于客户端配置**——思考模式是任务的属性。
        // 端点不支持思考时 effective_thinking 会返回 ServerDefault，即不打这个字段。
        let thinking = req.effective_thinking(self.config.thinking);
        if let Some(t) = thinking.body_field() {
            body["thinking"] = t;
        }
        // 工具描述。实测开思考时带 tools 不报错。
        if !req.tools.is_empty() {
            body["tools"] = serde_json::json!(req.tools);
        }

        let resp = self
            .agent
            .post(&self.endpoint())
            .set(
                "Authorization",
                // 唯一取用原文的地方
                &format!("Bearer {}", self.api_key.expose()),
            )
            .set("Content-Type", "application/json")
            .set("Accept", "application/json")
            .send_json(body);

        let resp = match resp {
            Ok(r) => r,
            Err(ureq::Error::Status(code, r)) => {
                // Retry-After 是秒数；拿不到就让调用方按平台默认（1 分钟）处理
                let retry_after = r
                    .header("Retry-After")
                    .and_then(|v| v.trim().parse::<u64>().ok())
                    .map(Duration::from_secs);
                let detail = r.into_string().unwrap_or_default();
                let detail: String = detail.chars().take(300).collect();
                return Err(Self::map_status(code, detail, retry_after));
            }
            Err(ureq::Error::Transport(t)) => {
                // 传输层错误信息可能带 URL，但不会带请求头，所以密钥不会泄露
                let msg = t.to_string();
                if msg.contains("timed out") || msg.contains("timeout") {
                    return Err(ThinkError::Transient {
                        status: None,
                        detail: format!("请求超时（{:?}）", self.config.timeout),
                    });
                }
                return Err(ThinkError::Network(msg));
            }
        };

        let wire: WireResponse = resp
            .into_json()
            .map_err(|e| ThinkError::Malformed(format!("响应不是预期结构: {e}")))?;

        let choice = wire
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| ThinkError::Malformed("响应里没有 choices".into()))?;

        let msg = choice.message;
        let tool_calls = msg.tool_calls;
        // content 为 null 是合法的——只要带回了工具调用。
        // 两者都空才是真的畸形响应。
        let content = match msg.content {
            Some(c) => c,
            None if !tool_calls.is_empty() => String::new(),
            None => {
                return Err(ThinkError::Malformed(
                    "响应里既没有 message.content 也没有 tool_calls".into(),
                ));
            }
        };

        Ok(ThinkResponse {
            content,
            model: if wire.model.is_empty() {
                self.config.model.clone()
            } else {
                wire.model
            },
            usage: wire.usage,
            finish_reason: choice.finish_reason,
            reasoning: msg.reasoning_content,
            thinking,
            tool_calls,
        })
    }

    fn model(&self) -> &str {
        &self.config.model
    }
}

/// 测试桩：不发网络请求。
#[derive(Debug, Clone)]
pub struct StubThinker {
    reply: String,
    fail_with: Option<ThinkError>,
    calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl StubThinker {
    pub fn replying(text: impl Into<String>) -> Self {
        Self {
            reply: text.into(),
            fail_with: None,
            calls: Default::default(),
        }
    }

    pub fn failing(err: ThinkError) -> Self {
        Self {
            reply: String::new(),
            fail_with: Some(err),
            calls: Default::default(),
        }
    }

    pub fn calls(&self) -> usize {
        self.calls.load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl Thinker for StubThinker {
    fn think(&self, _req: &ThinkRequest) -> Result<ThinkResponse, ThinkError> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if let Some(e) = &self.fail_with {
            return Err(e.clone());
        }
        Ok(ThinkResponse {
            content: self.reply.clone(),
            model: "stub".into(),
            usage: Usage::default(),
            finish_reason: Some("stop".into()),
            reasoning: None,
            thinking: _req.effective_thinking(Thinking::ServerDefault),
            tool_calls: Vec::new(),
        })
    }
    fn model(&self) -> &str {
        "stub"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::think::Message;

    #[test]
    fn api_key_never_prints_itself() {
        let k = ApiKey::new("sk-abcdefghijklmnop");
        let shown = format!("{k:?}");
        assert!(
            !shown.contains("abcdefghijklmnop"),
            "Debug 不得泄露原文: {shown}"
        );
        assert!(shown.contains("sk-abc"), "应保留可辨识的前缀: {shown}");
    }

    #[test]
    fn api_key_trims_whitespace() {
        assert_eq!(ApiKey::new("  sk-x\n").expose(), "sk-x");
    }

    #[test]
    fn maps_429_with_retry_after() {
        let e = OpenAiThinker::map_status(429, "slow down".into(), Some(Duration::from_secs(30)));
        match e {
            ThinkError::RateLimited { retry_after, .. } => {
                assert_eq!(retry_after, Some(Duration::from_secs(30)))
            }
            other => panic!("429 应映射为 RateLimited，实际 {other:?}"),
        }
    }

    #[test]
    fn permanent_failures_are_not_retryable() {
        // 这些重试只会浪费配额并刷屏
        for code in [400u16, 401, 402, 403, 404, 413, 422] {
            let e = OpenAiThinker::map_status(code, "x".into(), None);
            assert!(!e.is_retryable(), "{code} 不该被判定为可重试: {e:?}");
        }
    }

    #[test]
    fn transient_failures_are_retryable() {
        for code in [408u16, 409, 429, 500, 502, 503, 504] {
            let e = OpenAiThinker::map_status(code, "x".into(), None);
            assert!(e.is_retryable(), "{code} 应可重试: {e:?}");
        }
    }

    #[test]
    fn endpoint_has_no_double_slash() {
        let t = OpenAiThinker::new(
            ThinkerConfig {
                base_url: "https://api.agnes-ai.cn/v1/".into(),
                ..Default::default()
            },
            ApiKey::new("sk-x"),
        );
        assert_eq!(t.endpoint(), "https://api.agnes-ai.cn/v1/chat/completions");
    }

    #[test]
    fn empty_message_list_is_rejected_before_any_io() {
        let t = OpenAiThinker::new(ThinkerConfig::default(), ApiKey::new("sk-x"));
        let err = t.think(&ThinkRequest::new(vec![])).unwrap_err();
        assert!(matches!(err, ThinkError::BadRequest { .. }), "{err:?}");
    }

    #[test]
    fn local_throttle_blocks_the_second_call() {
        let t = OpenAiThinker::new(ThinkerConfig::default(), ApiKey::new("sk-x"));
        // 第一次不受本地限流影响（会真的去发请求，但我们只关心限流分支）
        let first = t.think(&ThinkRequest::new(vec![Message::user("hi")]));
        assert!(
            !matches!(first, Err(ThinkError::LocalThrottle { .. })),
            "第一次不该被本地限流: {first:?}"
        );
        let second = t.think(&ThinkRequest::new(vec![Message::user("hi")]));
        assert!(
            matches!(second, Err(ThinkError::LocalThrottle { .. })),
            "紧接着的第二次必须被本地限流挡下: {second:?}"
        );
    }

    #[test]
    fn default_key_file_is_outside_any_repo_path() {
        let p = default_key_file(Path::new("C:/Users/x/AppData/Local/YunXiBot"));
        assert!(p.ends_with("secrets/agnes.key") || p.ends_with("secrets\\agnes.key"));
    }

    #[test]
    fn stub_reports_calls() {
        let s = StubThinker::replying("你好");
        let _ = s.think(&ThinkRequest::new(vec![Message::user("hi")]));
        let _ = s.think(&ThinkRequest::new(vec![Message::user("hi")]));
        assert_eq!(s.calls(), 2);
    }

    #[test]
    fn peek_wait_does_not_consume_quota() {
        let rl = RateLimiter::with_rpm(600);
        assert!(rl.peek_wait().is_none());
        assert!(rl.try_acquire().is_none(), "第一次应能占用");
        assert!(rl.peek_wait().is_some(), "占用后应显示还需等待");
        // peek 多次不应改变状态
        assert!(rl.peek_wait().is_some());
        assert!(rl.try_acquire().is_some(), "仍然被挡，说明 peek 没消耗配额");
    }
}
