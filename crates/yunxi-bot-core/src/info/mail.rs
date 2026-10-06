//! 邮件信息源：连本机的只读邮件 sidecar。
//!
//! ## 为什么 Rust 侧不直接说 IMAP
//!
//! 因为 **Rust 侧不该看见邮箱密码**。密码只在 Python 侧读
//! （`<home>/secrets/mail.json`），Rust 只知道"17871 端口上有个 sidecar"。
//! 少一处传播就少一处泄露，而 IMAP 那些真正难的部分（MIME 解码、
//! RFC 2047 邮件头、各种中文编码）Python 标准库几十年打磨过。
//!
//! 契约与决策模型 sidecar 同构，所以复用了同一个回环 HTTP 客户端
//! （[`crate::decide::http`]）——**顺带白拿"只允许回环"那条安全约束**：
//! 邮件内容是私人信息，那个限制保证它不可能被发到外部主机。
//!
//! ## 三种失败必须分开
//!
//! | 情况 | 含义 | 返回 |
//! |---|---|---|
//! | 连不上 sidecar | 服务没起来 | `Err`，提示怎么启动 |
//! | sidecar 起来了但没配邮箱 | 缺配置 | `Err`，提示配置文件在哪 |
//! | 配好了但取不到（网络/密码错） | 真的失败了 | `Err`，**转述 sidecar 的原话** |
//! | 取到了，真的没有未读 | 正常 | `Ok` + 空列表 |
//!
//! 前三行都是 `Err`。**把它们压成"空列表"就是让故障伪装成安静**——
//! 而安静正是使用者最不会去查的状态。

use std::path::PathBuf;
use std::time::Duration;

use serde::Deserialize;

use super::{FetchBatch, InfoItem, InfoSource};
use crate::decide::http::{HttpError, get_json, post_json};

/// 默认端口。**与决策模型 sidecar 的 17870 错开**，
/// 免得两个 sidecar 抢同一个端口时错误信息指向错的那个。
pub const DEFAULT_PORT: u16 = 17871;

/// 单次请求超时。
///
/// 比决策模型的 5 秒长得多：取未读要走一次真实的 IMAP 登录 + 若干次 FETCH，
/// 慢是正常的。但也**必须有上限**——没有超时的常驻进程会被一个卡住的
/// IMAP 服务器拖死。
const DEFAULT_TIMEOUT_MS: u64 = 30_000;

/// 连本机 sidecar 的邮件源。
#[derive(Debug, Clone)]
pub struct MailSource {
    base: String,
    timeout_ms: u64,
}

impl Default for MailSource {
    fn default() -> Self {
        Self::new(DEFAULT_PORT)
    }
}

impl MailSource {
    pub fn new(port: u16) -> Self {
        Self {
            base: format!("http://127.0.0.1:{port}"),
            timeout_ms: DEFAULT_TIMEOUT_MS,
        }
    }

    /// 自定义端点。**只接受回环**——由 [`crate::decide::http`] 强制。
    pub fn at(endpoint: impl Into<String>) -> Self {
        Self {
            base: endpoint.into().trim_end_matches('/').to_string(),
            timeout_ms: DEFAULT_TIMEOUT_MS,
        }
    }

    pub fn with_timeout_ms(mut self, ms: u64) -> Self {
        self.timeout_ms = ms;
        self
    }

    /// sidecar 的启动命令。**失败信息里要带上它**——
    /// 使用者看到"连不上"时，下一步该做什么不该靠猜。
    pub fn start_hint(&self) -> String {
        format!(
            "启动邮件 sidecar：python sidecar/mail_server.py --port {}（当前指向 {}）",
            self.base.rsplit(':').next().unwrap_or("17871"),
            self.base
        )
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base, path)
    }

    /// 描述 sidecar 上配的是哪个邮箱。**读不到配置时返回 `None`。**
    ///
    /// 这个信息必须能显示出来：使用者看到"未读 12 封"时，得能确认
    /// **读的是哪个邮箱**。一个助理报了个数字却不说从哪读的，
    /// 你没法判断它是不是看错了地方。
    pub fn describe(&self) -> Option<String> {
        let body = get_json(&self.url("/health"), 5_000).ok()?;
        let h: HealthResponse = serde_json::from_str(&body).ok()?;
        if !h.configured {
            return None;
        }
        Some(match (h.username, h.imap_host) {
            (Some(u), Some(host)) => format!("{u} @ {host}"),
            (Some(u), None) => u,
            (None, Some(host)) => format!("(未报账号) @ {host}"),
            (None, None) => "(sidecar 没报账号信息)".to_string(),
        })
    }
}

/// sidecar 的健康响应。
#[derive(Debug, Deserialize)]
struct HealthResponse {
    #[serde(default)]
    configured: bool,
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    imap_host: Option<String>,
    #[serde(default)]
    username: Option<String>,
    /// **sidecar 自己算出来的配置文件路径。**
    ///
    /// sidecar 与 CLI 各自算一遍 `default_home()`，靠 `YUNXI_BOT_HOME`
    /// 对齐。约定没对齐时表现是 sidecar 说"没配置"、而配置文件明明就在那——
    /// 这是最难查的一类错。有了这个字段，调用方能把两个路径摆在一起看。
    #[serde(default)]
    config_path: Option<String>,
}

/// sidecar 的未读响应。
#[derive(Debug, Deserialize)]
struct UnreadResponse {
    #[serde(default)]
    items: Vec<serde_json::Value>,
    #[serde(default)]
    total_unseen: usize,
    #[serde(default)]
    skipped: usize,
}

/// 读接口的响应。
#[derive(Debug, Deserialize)]
struct ReadResponse {
    #[serde(default)]
    body: String,
}

/// sidecar 报错时的形状 `{"error": "..."}`。
#[derive(Debug, Deserialize)]
struct ErrorResponse {
    #[serde(default)]
    error: String,
}

/// 从 [`HttpError`] 里挖出 sidecar 说的话。
///
/// **要转述原话而不是翻译成自己的话**：sidecar 的报错里有"授权码不是登录密码"
/// 这类关键的排查线索，换成一句笼统的"邮件读取失败"就把线索丢了。
fn explain(e: HttpError, s: &MailSource) -> String {
    match e {
        HttpError::Connect(m) => format!("连不上邮件 sidecar：{m}。\n{}", s.start_hint()),
        HttpError::Timeout(_) => format!(
            "邮件 sidecar 在 {} 毫秒内没有响应（IMAP 服务器可能是慢或卡住了）",
            s.timeout_ms
        ),
        HttpError::Status(code, body) => {
            let msg = serde_json::from_str::<ErrorResponse>(&body)
                .ok()
                .map(|e| e.error)
                .filter(|e| !e.is_empty())
                .unwrap_or_else(|| body.chars().take(300).collect());
            format!("邮件 sidecar 返回 HTTP {code}：{msg}")
        }
        other => format!("邮件 sidecar 通信失败：{other}"),
    }
}

/// 把 sidecar 给的一条 JSON 解成 [`InfoItem`]。
///
/// **单条解不出来不该让整批失败**：跳过它，让调用方看得见少了几条。
/// 但如果**每一条**都解不出来，那就是契约变了，得报错而不是报"没有邮件"。
fn to_item(v: &serde_json::Value) -> Option<InfoItem> {
    let id = v.get("uid")?.as_str()?.to_string();
    if id.is_empty() {
        return None;
    }
    let s = |k: &str| -> String {
        v.get(k)
            .and_then(|x| x.as_str())
            .unwrap_or_default()
            .to_string()
    };
    Some(InfoItem {
        source: "mail".to_string(),
        id,
        from_name: s("from_name"),
        from_addr: s("from_addr"),
        subject: s("subject"),
        preview: s("preview"),
        received_at_ms: v
            .get("received_at_ms")
            .and_then(|x| x.as_u64())
            .unwrap_or(0),
        direct: v.get("direct").and_then(|x| x.as_bool()).unwrap_or(false),
        recipient_count: v
            .get("recipient_count")
            .and_then(|x| x.as_u64())
            .unwrap_or(0) as usize,
        addressed_directly: v
            .get("addressed_directly")
            .and_then(|x| x.as_bool())
            .unwrap_or(false),
        has_attachments: v
            .get("has_attachments")
            .and_then(|x| x.as_bool())
            .unwrap_or(false),
    })
}

impl InfoSource for MailSource {
    fn name(&self) -> &'static str {
        "邮件"
    }

    fn health(&self) -> Result<(), String> {
        let body = get_json(&self.url("/health"), 5_000).map_err(|e| explain(e, self))?;
        let h: HealthResponse = serde_json::from_str(&body)
            .map_err(|e| format!("邮件 sidecar 的健康响应不是预期结构: {e}"))?;
        if !h.configured {
            // **"没配"和"连不上"必须分开报。** 连不上要去启动服务，
            // 没配要去写配置文件——两件事的下一步完全不同。
            let mut msg = format!(
                "邮件 sidecar 在跑，但没配置邮箱：{}",
                h.reason.unwrap_or_else(|| "未说明原因".into())
            );
            // 如果 sidecar 报的路径和我们以为的不一样，**那才是真正的原因**：
            // 不是"没配"，是两边算出的 home 不一致。不说出来的话，
            // 使用者会对着一个确实存在的配置文件反复怀疑自己。
            if let Some(their) = &h.config_path {
                let ours = credentials_path(&crate::default_home());
                if their != &ours.to_string_lossy() {
                    msg.push_str(&format!(
                        "\n  ⚠ 两边算出的配置路径不同——这多半才是真正的原因：\
                         \n    sidecar 找的是: {their}\
                         \n    本程序以为的是: {}",
                        ours.display()
                    ));
                    msg.push_str("\n    修法：给 sidecar 也设上同一个 YUNXI_BOT_HOME 环境变量。");
                }
            }
            return Err(msg);
        }
        Ok(())
    }

    fn fetch(&self, limit: usize) -> Result<FetchBatch, String> {
        let req = serde_json::json!({ "limit": limit.max(1), "preview": true }).to_string();
        let body = post_json(&self.url("/v1/unread"), &req, self.timeout_ms)
            .map_err(|e| explain(e, self))?;
        let r: UnreadResponse = serde_json::from_str(&body)
            .map_err(|e| format!("邮件 sidecar 的未读响应不是预期结构: {e}"))?;

        let raw_count = r.items.len();
        let items: Vec<InfoItem> = r.items.iter().filter_map(to_item).collect();

        // 全都没解出来，但 sidecar 明明给了东西 → 是契约变了，不是没有邮件。
        // 报错而不是报空：让"该修了"不要伪装成"今天很安静"。
        if raw_count > 0 && items.is_empty() {
            return Err(format!(
                "邮件 sidecar 返回了 {raw_count} 条，但没有一条能解析出 uid——接口契约可能变了"
            ));
        }

        let parse_skipped = raw_count.saturating_sub(items.len());
        Ok(FetchBatch {
            items,
            total_unseen: r.total_unseen,
            skipped: r.skipped + parse_skipped,
        })
    }

    fn read(&self, id: &str) -> Result<String, String> {
        let req = serde_json::json!({ "uid": id }).to_string();
        let body = post_json(&self.url("/v1/read"), &req, self.timeout_ms)
            .map_err(|e| explain(e, self))?;
        let r: ReadResponse = serde_json::from_str(&body)
            .map_err(|e| format!("邮件 sidecar 的读取响应不是预期结构: {e}"))?;
        Ok(r.body)
    }
}

/// 配置文件的位置。**与 sidecar 侧必须是同一套规则**——
/// 两处不一致时 sidecar 会去空目录找凭证，报出来却是"没配置"。
pub fn credentials_path(home: &std::path::Path) -> PathBuf {
    home.join("secrets").join("mail.json")
}

/// 默认超时。给 CLI 显示用。
pub fn default_timeout() -> Duration {
    Duration::from_millis(DEFAULT_TIMEOUT_MS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::info::InfoSource;

    fn src() -> MailSource {
        MailSource::new(17871)
    }

    // ---- 端点与回环限制 ----

    #[test]
    fn builds_the_expected_urls() {
        let s = src();
        assert_eq!(s.url("/health"), "http://127.0.0.1:17871/health");
        assert_eq!(s.url("/v1/unread"), "http://127.0.0.1:17871/v1/unread");
    }

    #[test]
    fn at_strips_a_trailing_slash() {
        // 不剥的话会拼出 `//v1/unread`，有些服务端会 404
        let s = MailSource::at("http://127.0.0.1:9999/");
        assert_eq!(s.url("/health"), "http://127.0.0.1:9999/health");
    }

    #[test]
    fn default_port_differs_from_the_decide_sidecar() {
        // 两个 sidecar 抢同一个端口时，错误信息会指向错的那个
        assert_ne!(DEFAULT_PORT, 17870);
    }

    #[test]
    fn non_loopback_endpoints_are_rejected_by_the_client() {
        // **邮件内容是私人信息。** 回环限制保证它不可能被发到外部主机——
        // 这条约束由共用的 http 客户端提供，不是这里自己写的。
        let s = MailSource::at("http://evil.example.com:17871");
        let err = s.fetch(5).unwrap_err();
        assert!(err.contains("非回环"), "{err}");
    }

    #[test]
    fn start_hint_names_the_command_and_the_port() {
        // 看到"连不上"时，下一步该做什么不该靠猜
        let h = src().start_hint();
        assert!(h.contains("mail_server.py"), "{h}");
        assert!(h.contains("17871"), "{h}");
    }

    // ---- 失败分类：三种"没有"必须分开 ----

    #[test]
    fn connection_failure_says_how_to_start_it() {
        // 用一个几乎不可能有人监听的端口
        let s = MailSource::new(1).with_timeout_ms(300);
        let err = s.health().unwrap_err();
        assert!(err.contains("连不上"), "{err}");
        assert!(err.contains("mail_server.py"), "要说清怎么启动: {err}");
    }

    #[test]
    fn fetch_failure_is_an_error_not_an_empty_batch() {
        // **"取不到"不能伪装成"没有"。** 压成空批次会让故障看起来像安静，
        // 而安静是使用者最不会去查的状态。
        let s = MailSource::new(1).with_timeout_ms(300);
        assert!(s.fetch(10).is_err());
    }

    #[test]
    fn read_failure_is_an_error() {
        let s = MailSource::new(1).with_timeout_ms(300);
        assert!(s.read("1").is_err());
    }

    #[test]
    fn timeout_maps_to_a_message_that_names_the_budget() {
        let s = MailSource::new(17871);
        let e = explain(HttpError::Timeout("测试".into()), &s);
        assert!(e.contains("30000"), "要说清等了多少: {e}");
        assert!(e.contains("IMAP"), "要指出可能的原因: {e}");
    }

    #[test]
    fn sidecar_error_body_is_relayed_verbatim() {
        // sidecar 的报错里有"授权码不是登录密码"这类关键线索，
        // 换成自己的话就把线索丢了
        let s = src();
        let body = r#"{"error":"IMAP 登录被拒。注意：QQ/163 用的是授权码而不是登录密码"}"#;
        let e = explain(HttpError::Status(502, body.into()), &s);
        assert!(e.contains("授权码"), "{e}");
        assert!(e.contains("502"), "{e}");
    }

    #[test]
    fn non_json_error_body_is_still_shown() {
        // sidecar 崩了可能返回 HTML 错误页；不能因此什么线索都不给
        let s = src();
        let e = explain(HttpError::Status(500, "<html>boom</html>".into()), &s);
        assert!(e.contains("boom"), "{e}");
    }

    // ---- 条目解析 ----

    #[test]
    fn parses_a_full_item() {
        let v = serde_json::json!({
            "uid": "42",
            "from_name": "张三",
            "from_addr": "a@x.com",
            "subject": "问候",
            "preview": "你好",
            "received_at_ms": 1791165600000u64,
            "direct": true,
            "recipient_count": 1,
            "addressed_directly": true,
            "has_attachments": false
        });
        let i = to_item(&v).expect("应能解析");
        assert_eq!(i.id, "42");
        assert_eq!(i.source, "mail");
        assert_eq!(i.from_name, "张三");
        assert!(i.direct);
        assert!(!i.has_attachments);
    }

    #[test]
    fn item_without_a_uid_is_skipped() {
        // 没有 id 就没法去重、没法读全文、没法记"处理过了"
        assert!(to_item(&serde_json::json!({"subject": "x"})).is_none());
        assert!(to_item(&serde_json::json!({"uid": ""})).is_none());
    }

    #[test]
    fn item_tolerates_missing_optional_fields() {
        let i = to_item(&serde_json::json!({"uid": "1"})).expect("应能解析");
        assert_eq!(i.id, "1");
        assert_eq!(i.subject, "");
        assert!(!i.direct);
        assert_eq!(i.received_at_ms, 0);
    }

    #[test]
    fn a_sidecar_error_item_is_skipped_not_parsed_as_a_message() {
        // sidecar 对取不到的单条会回 {"uid":"...","error":"..."}
        let v = serde_json::json!({"uid": "9", "error": "取邮件头失败"});
        let i = to_item(&v).expect("有 uid 就该能解析成条目");
        assert_eq!(i.id, "9");
        assert_eq!(i.subject, "", "没有主题就是空，不该编一个");
    }

    // ---- 配置路径 ----

    #[test]
    fn credentials_path_is_under_secrets() {
        // 与 sidecar 侧必须是同一套规则；两处不一致会让 sidecar 去
        // 空目录找凭证，报出来却是"没配置"
        let p = credentials_path(std::path::Path::new("C:/home"));
        assert!(p.ends_with("secrets/mail.json") || p.ends_with(r"secrets\mail.json"));
    }

    #[test]
    fn default_timeout_is_generous_but_bounded() {
        let d = default_timeout();
        assert!(d.as_millis() >= 5_000, "IMAP 登录 + 若干 FETCH 本来就慢");
        assert!(d.as_millis() <= 60_000, "没有上限会拖死常驻进程");
    }

    #[test]
    fn source_reports_its_name() {
        assert_eq!(src().name(), "邮件");
    }
}
