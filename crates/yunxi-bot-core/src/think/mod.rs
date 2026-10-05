//! 思考层：Agent 的通用能力来自这里。
//!
//! ## 分工
//!
//! - **思考层（本模块）**：Agent 的"脑子"。通用推理、写东西、规划、干活。
//!   默认接远端模型（Agnes）。慢、贵、有配额，但是通用的。
//! - **决策层（[`crate::decide`]）**：只管"该不该介入"这一类判断。
//!   本地跑（Laya），快、免费、离线可用。
//!
//! 两者不是一回事：**Laya 不负责思考，只负责判断要不要打扰你。**
//!
//! ## 为什么必须有本地那一层做前置
//!
//! Agnes 免费档的文本模型 **RPM 只有 10**（2026-09 起从 20 下调 50%）。
//! 常驻进程如果每轮都去打远端，几秒钟就把配额烧光并触发限流。
//!
//! 所以顺序是：**本地 Laya 先筛 → 只有值得的才花一次远端调用**。
//! 这不是为了省钱的优化，是这个配额下的结构必然。

pub mod agnes;

use std::sync::Mutex;
use std::time::{Duration, Instant};

pub use agnes::{AgnesConfig, AgnesThinker};

/// 对话角色。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
        }
    }
}

/// 一条消息。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: String,
}

impl Message {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: Role::System,
            content: content.into(),
        }
    }
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: content.into(),
        }
    }
    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: content.into(),
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
}

impl ThinkRequest {
    pub fn new(messages: Vec<Message>) -> Self {
        Self {
            messages,
            max_tokens: None,
            temperature: None,
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
}

/// token 用量。常驻进程需要它来观察配额消耗。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub prompt_tokens: u64,
    #[serde(default)]
    pub completion_tokens: u64,
    #[serde(default)]
    pub total_tokens: u64,
}

/// 一次思考的结果。
#[derive(Debug, Clone, PartialEq)]
pub struct ThinkResponse {
    pub content: String,
    pub model: String,
    pub usage: Usage,
    pub finish_reason: Option<String>,
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
