//! 生成层：本地模型承担对话、摘要与陪伴表达；复杂任务可路由到 DeepSeek。
//! 决策层 Laya 提供分类建议，执行权限仍由确定性策略控制。

pub mod client;
pub mod context;
pub mod cost;
pub mod image;
pub mod local_health;
pub mod prompt;
pub mod router;
pub mod session;
/// 路由阈值。**集中在一处**，便于按实际账单调整。
pub mod thresholds {
    pub use super::router::thresholds::*;
}

use std::sync::Mutex;
use std::time::{Duration, Instant};

pub use client::{OpenAiThinker, ThinkerConfig};
pub use cost::{Cost, PriceTable, Usage};
pub use image::ImageInput;
pub use router::{
    ModelRouter, ModelSpec, ReasoningEffort, Routing, TaskKind, TaskProfile, Thinking, detect_code,
    detect_explicit_multi, is_peak_now, profile_task,
};

/// 对话角色。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    /// 工具返回。**只在带工具调用的一轮里出现。**
    ///
    /// 单独一个角色而不是塞进 `User`：工具返回的内容是**不可信数据**
    /// （网页、文件、第三方 server 的输出），文字不能冒充用户指令；D153仅为接口限制
    /// 给工具截图使用带明确来源的user封装，仍不构成授权。
    Tool,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
        }
    }
}

/// 一条消息。
///
/// ## 工具调用为什么需要三个额外字段
///
/// OpenAI 兼容的工具调用协议要求：助手消息带回 `tool_calls`，随后每条工具
/// 结果必须用 `role: "tool"` 且带上对应的 `tool_call_id`。少一个字段服务端
/// 就报 400——**而不是"忽略工具结果"**，所以这里不能省。
#[derive(Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Message {
    pub role: Role,
    /// 工具结果的消息内容可以是空字符串（比如工具只返回结构化数据）。
    pub content: String,
    /// 图片仅随当前进程发送；存档只保留附图说明，不保存原图。
    #[serde(skip)]
    pub images: Vec<ImageInput>,
    #[serde(skip)]
    pub live_content: Option<String>,
    #[serde(skip)]
    pub live_tool_calls: Vec<serde_json::Value>,
    /// Provider continuation is transient protocol data, never persisted or displayed.
    #[serde(skip)]
    pub live_reasoning: Option<String>,
    /// 助手请求调用的工具。只出现在助手消息上。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<serde_json::Value>,
    /// 工具结果对应的调用 id。只出现在工具消息上。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl std::fmt::Debug for Message {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Message")
            .field("role", &self.role)
            .field("content", &self.content)
            .field("images", &self.images)
            .field("tool_calls", &self.tool_calls)
            .field("tool_call_id", &self.tool_call_id)
            .field("has_private_content", &self.live_content.is_some())
            .field("has_private_arguments", &!self.live_tool_calls.is_empty())
            .finish()
    }
}

impl Message {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: Role::System,
            content: content.into(),
            images: Vec::new(),
            live_content: None,
            live_tool_calls: Vec::new(),
            live_reasoning: None,
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: content.into(),
            images: Vec::new(),
            live_content: None,
            live_tool_calls: Vec::new(),
            live_reasoning: None,
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }
    pub fn user_with_images(content: impl Into<String>, images: Vec<ImageInput>) -> Self {
        let mut message = Self::user(content);
        if !images.is_empty() {
            message.content.push_str("\n\n");
            message.content.push_str(image::IMAGE_NOTICE);
            message.images = images;
        }
        message
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: content.into(),
            images: Vec::new(),
            live_content: None,
            live_tool_calls: Vec::new(),
            live_reasoning: None,
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }

    /// 助手请求调用工具的那条消息。**必须原样回传**，否则服务端报 400。
    pub fn assistant_tool_calls(tool_calls: Vec<serde_json::Value>) -> Self {
        Self {
            role: Role::Assistant,
            content: String::new(),
            images: Vec::new(),
            live_content: None,
            live_tool_calls: Vec::new(),
            live_reasoning: None,
            tool_calls,
            tool_call_id: None,
        }
    }

    /// 一条工具结果。**必须带 `tool_call_id`**，否则服务端报 400。
    pub fn tool_result(call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: Role::Tool,
            content: content.into(),
            images: Vec::new(),
            live_content: None,
            live_tool_calls: Vec::new(),
            live_reasoning: None,
            tool_calls: Vec::new(),
            tool_call_id: Some(call_id.into()),
        }
    }
}

/// 一次思考请求。
#[derive(Debug, Clone, PartialEq)]
pub struct ThinkRequest {
    pub messages: Vec<Message>,
    pub max_tokens: Option<u32>,
    /// 采样温度。陪伴场景宜低——要的是稳定，不是花哨。
    pub temperature: Option<f32>,
    /// 本次请求的思考模式。`None` 表示用客户端配置。
    ///
    /// **放这里是因为思考模式是任务的属性，不是客户端的属性。** 同一个 DeepSeek
    /// 客户端可能这一轮在处理分析任务（要思考），下一轮在批量改文件（不要思考）。
    /// 客户端上的 `thinking` 只表示"这个端点支不支持"。
    pub thinking: Option<Thinking>,
    /// 工具描述（OpenAI 兼容格式）。空表示不带工具。
    ///
    /// 实测：**开思考时带 `tools` 不会报错**，正常返回 `tool_calls`。
    pub tools: Vec<serde_json::Value>,
    /// 机器读取的结果显式请求 JSON；普通聊天保持服务端默认格式。
    pub json_output: bool,
}

impl ThinkRequest {
    pub fn new(messages: Vec<Message>) -> Self {
        Self {
            messages,
            max_tokens: None,
            temperature: None,
            thinking: None,
            tools: Vec::new(),
            json_output: false,
        }
    }

    pub fn with_max_tokens(mut self, n: u32) -> Self {
        self.max_tokens = Some(n);
        self
    }

    pub fn with_temperature(mut self, t: f32) -> Self {
        self.temperature = Some(t);
        self
    }

    /// 指定本次的思考模式。由路由决定，不由调用点拍脑袋。
    pub fn with_thinking(mut self, t: Thinking) -> Self {
        self.thinking = Some(t);
        self
    }

    pub fn with_tools(mut self, tools: Vec<serde_json::Value>) -> Self {
        self.tools = tools;
        self
    }

    /// 最终要发出去的思考模式。请求上的优先于客户端配置。
    ///
    /// **端点的能力是硬约束**：客户端配了 [`Thinking::ServerDefault`]
    /// 说明它不吃这个字段，此时即使请求要求思考也不发（发了是错的行为）。
    pub fn effective_thinking(&self, client: Thinking) -> Thinking {
        if matches!(client, Thinking::ServerDefault) {
            return Thinking::ServerDefault;
        }
        self.thinking.unwrap_or(client)
    }
}

/// 一次思考的结果。
#[derive(Debug, Clone, PartialEq)]
pub struct ThinkResponse {
    pub content: String,
    pub model: String,
    pub usage: Usage,
    pub finish_reason: Option<String>,
    /// 思考过程原文。**不开思考时为空。**
    ///
    /// 单独留着而不是塞进 `content`，是因为它对使用者没用、对排查有用，
    /// 而且它按输出 token 计费——用量要能对上账。
    pub reasoning: Option<String>,
    /// 本次实际生效的思考模式。
    pub thinking: Thinking,
    /// 模型请求的工具调用。目前只记录，不自动执行（见 ADR D15）。
    pub tool_calls: Vec<serde_json::Value>,
}

/// 思考层失败的形态。
///
/// **刻意区分 `RateLimited` 与 `Auth`**：前者等一会儿就好，后者等到天亮也没用。
/// 常驻进程对这两者的处理必须不同。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThinkError {
    /// 401：密钥无效
    Auth(String),
    /// 402：余额或配额不足
    Payment(String),
    /// 403：无权访问该模型
    Forbidden(String),
    /// 404：地址或模型名不对
    NotFound(String),
    /// 429：超出 RPM
    RateLimited {
        detail: String,
        retry_after: Option<Duration>,
    },
    /// 400 / 413 / 422：请求本身有问题，重试无用
    BadRequest { status: u16, detail: String },
    /// 408 / 5xx：服务端问题，值得重试
    Transient { status: Option<u16>, detail: String },
    /// 网络层失败
    Network(String),
    /// 响应结构不是预期的
    Malformed(String),
    /// 本地限流：还没到下次可发送的时间
    LocalThrottle { wait: Duration },
}

impl ThinkError {
    /// 是否值得稍后重试。
    ///
    /// `Auth` / `Payment` / `Forbidden` / `BadRequest` **不重试**——
    /// 反复重试只会浪费配额并刷屏日志。
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            ThinkError::RateLimited { .. }
                | ThinkError::Transient { .. }
                | ThinkError::Network(_)
                | ThinkError::LocalThrottle { .. }
        )
    }
}

impl std::fmt::Display for ThinkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ThinkError::Auth(m) => write!(f, "密钥无效或被拒绝: {m}"),
            ThinkError::Payment(m) => write!(f, "余额或配额不足: {m}"),
            ThinkError::Forbidden(m) => write!(f, "无权访问该模型: {m}"),
            ThinkError::NotFound(m) => write!(f, "地址或模型名不正确: {m}"),
            ThinkError::RateLimited {
                detail,
                retry_after,
            } => match retry_after {
                Some(d) => write!(f, "超出速率限制，建议等待 {}s: {detail}", d.as_secs()),
                None => write!(f, "超出速率限制: {detail}"),
            },
            ThinkError::BadRequest { status, detail } => {
                write!(f, "请求无效（{status}）: {detail}")
            }
            ThinkError::Transient { status, detail } => match status {
                Some(s) => write!(f, "服务端临时故障（{s}）: {detail}"),
                None => write!(f, "服务端临时故障: {detail}"),
            },
            ThinkError::Network(m) => write!(f, "网络失败: {m}"),
            ThinkError::Malformed(m) => write!(f, "响应结构异常: {m}"),
            ThinkError::LocalThrottle { wait } => {
                write!(f, "本地限流：还需等待 {}s", wait.as_secs())
            }
        }
    }
}

impl std::error::Error for ThinkError {}

/// 思考层的能力面。实现者可以是远端模型、本地模型或测试桩。
pub trait Thinker: Send + Sync {
    fn think(&self, req: &ThinkRequest) -> Result<ThinkResponse, ThinkError>;

    /// 流式思考：每收到一段正文就调一次 `on_delta`。
    ///
    /// ## 为什么默认实现是"一次性"
    ///
    /// **不是所有 Thinker 都能流式**——本地决策模型、测试桩、
    /// 以后可能的其他 provider 都可能只能给完整结果。
    ///
    /// 默认实现退化成一次调用、把正文整体交出去一次。
    /// 对调用方来说**行为完全一致，只是没有"边生成边显示"**——
    /// 所以调用方不必写两条路径，也不必判断"这个 thinker 支不支持流"。
    ///
    /// **`on_delta` 只收正文，不收工具调用。**
    /// 工具调用的增量是参数 JSON 的碎片，打出来是噪声——
    /// 而且它们本来就不是给人读的。
    fn think_stream(
        &self,
        req: &ThinkRequest,
        on_delta: &mut dyn FnMut(&str),
    ) -> Result<ThinkResponse, ThinkError> {
        let r = self.think(req)?;
        if !r.content.is_empty() {
            on_delta(&r.content);
        }
        Ok(r)
    }

    /// 用于台账归因的模型标识。
    fn model(&self) -> &str;
}

/// 本地限流器：保证两次远端调用之间有最小间隔。
///
/// 用"最小间隔"而不是令牌桶——配额是 RPM 这种粗粒度限制，
/// 平滑发送比允许突发更不容易触发 429。
#[derive(Debug)]
pub struct RateLimiter {
    min_interval: Duration,
    last: Mutex<Option<Instant>>,
}

impl RateLimiter {
    /// 按每分钟请求数构造。`rpm` 为 0 时按"不限速"处理（只用于测试）。
    pub fn with_rpm(rpm: u32) -> Self {
        let min_interval = if rpm == 0 {
            Duration::ZERO
        } else {
            Duration::from_millis(60_000 / rpm as u64)
        };
        Self {
            min_interval,
            last: Mutex::new(None),
        }
    }

    /// 尝试占用一次调用配额。
    ///
    /// - 返回 `None`：可以立刻发，**且配额已被占用**；
    /// - 返回 `Some(d)`：还需等 `d`，**配额未被占用**（调用方可以跳过本轮）。
    ///
    /// 做成非阻塞是刻意的：常驻循环不该为了等一次调用而卡住整轮调度。
    pub fn try_acquire(&self) -> Option<Duration> {
        let now = Instant::now();
        let mut guard = self.last.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(prev) = *guard {
            let elapsed = now.saturating_duration_since(prev);
            if elapsed < self.min_interval {
                return Some(self.min_interval - elapsed);
            }
        }
        *guard = Some(now);
        None
    }

    /// 只看还需等多久，**不占用配额**。用于调度前判断。
    pub fn peek_wait(&self) -> Option<Duration> {
        let now = Instant::now();
        let guard = self.last.lock().unwrap_or_else(|e| e.into_inner());
        let prev = (*guard)?;
        let elapsed = now.saturating_duration_since(prev);
        if elapsed < self.min_interval {
            Some(self.min_interval - elapsed)
        } else {
            None
        }
    }

    pub fn min_interval(&self) -> Duration {
        self.min_interval
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rpm_maps_to_min_interval() {
        // 免费档 10 RPM → 每 6 秒一次
        assert_eq!(
            RateLimiter::with_rpm(10).min_interval(),
            Duration::from_secs(6)
        );
        assert_eq!(
            RateLimiter::with_rpm(60).min_interval(),
            Duration::from_secs(1)
        );
        assert_eq!(RateLimiter::with_rpm(0).min_interval(), Duration::ZERO);
    }

    #[test]
    fn first_call_passes_then_throttles() {
        let rl = RateLimiter::with_rpm(10);
        assert!(rl.try_acquire().is_none(), "第一次应放行");
        let wait = rl.try_acquire().expect("紧接着的第二次必须被挡下");
        assert!(wait <= Duration::from_secs(6));
        assert!(wait > Duration::from_secs(5), "剩余等待应接近 6s: {wait:?}");
    }

    #[test]
    fn throttled_call_does_not_consume_quota() {
        // 被挡下的调用不能占用配额，否则永远等不到窗口
        let rl = RateLimiter::with_rpm(600); // 100ms 间隔
        assert!(rl.try_acquire().is_none());
        let _ = rl.try_acquire(); // 被挡
        std::thread::sleep(Duration::from_millis(120));
        assert!(rl.try_acquire().is_none(), "等够间隔后应重新放行");
    }

    #[test]
    fn retryability_distinguishes_transient_from_permanent() {
        // 这条区分很重要：等一会儿就好的，和等到天亮也没用的，处理必须不同
        assert!(
            ThinkError::RateLimited {
                detail: "x".into(),
                retry_after: None
            }
            .is_retryable()
        );
        assert!(
            ThinkError::Transient {
                status: Some(503),
                detail: "x".into()
            }
            .is_retryable()
        );
        assert!(ThinkError::Network("x".into()).is_retryable());

        assert!(!ThinkError::Auth("x".into()).is_retryable());
        assert!(!ThinkError::Payment("x".into()).is_retryable());
        assert!(!ThinkError::Forbidden("x".into()).is_retryable());
        assert!(
            !ThinkError::BadRequest {
                status: 400,
                detail: "x".into()
            }
            .is_retryable()
        );
    }

    #[test]
    fn role_serializes_to_openai_names() {
        assert_eq!(Role::System.as_str(), "system");
        assert_eq!(Role::User.as_str(), "user");
        assert_eq!(Role::Assistant.as_str(), "assistant");
    }
}
