//! 信息源：助理"看外面"的入口。
//!
//! ## 为什么抽象成"信息源"而不是直接写邮件
//!
//! 因为助理的定位是**处理信息然后通知你**，而邮件只是第一种信息。
//! 后面还会有日历、消息、文件变化、提醒……
//!
//! 它们的共同形状是：
//!
//! ```text
//! 有一批"还没处理过的东西"
//!     ↓
//! 每一条能给出：谁、什么事、什么时候、要不要我办
//!     ↓
//! 交给"值不值得打扰你"去判断
//! ```
//!
//! 判断那一层（[`crate::decide`]）**不需要知道信息从哪来**。所以这一层的
//! 职责边界是：把任意来源归一成 [`InfoItem`]，并把"没读到"和"没有新的"
//! 严格区分开。
//!
//! ## 两条纪律
//!
//! **1. "取不到"不能伪装成"没有"。**
//! [`InfoSource::fetch`] 返回 `Result`，`Err` 是"这次没读到"。把它压成空列表
//! 会让一次故障看起来像一段安静——而安静正是使用者最不会去查的状态。
//!
//! **2. 摘要不是正文。**
//! [`InfoItem`] 只带"够判断要不要打扰你"的信息，不带整封邮件。
//! 想细看再单独取（[`InfoSource::read`]）。这条和模型路由同一条纪律：
//! **先便宜地筛，再花代价地看**。

pub mod mail;

use serde::{Deserialize, Serialize};

/// 一条待处理的信息。**与来源无关。**
///
/// 字段是按"判断要不要打扰你"的需要挑的，不是照搬邮件头——
/// 每多一个用不上的字段，就多一处要维护的映射。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InfoItem {
    /// 来源名：`mail` / `calendar` / …。
    pub source: String,
    /// **来源内的稳定标识。** 邮件用 UID。用它去重、去读全文、记"已经处理过"。
    pub id: String,
    /// 发件人显示名。可能为空（很多机器发的邮件没有名字）。
    #[serde(default)]
    pub from_name: String,
    /// 发件人地址。**打扰判定主要看它**——"这人是谁"比"主题写什么"更能决定要不要理。
    #[serde(default)]
    pub from_addr: String,
    #[serde(default)]
    pub subject: String,
    /// 一段短摘要，够判断"要不要理"即可。
    #[serde(default)]
    pub preview: String,
    /// 收到时间（Unix 毫秒）。**0 表示取不到**，不是"刚刚"。
    #[serde(default)]
    pub received_at_ms: u64,
    /// 只发给我一个人（不是群发、没有抄送别人）。
    ///
    /// 这是打扰判定里最有用的一个信号：群发邮件几乎从来不需要立刻打扰人。
    #[serde(default)]
    pub direct: bool,
    /// 收件人总数（含抄送）。与 `direct` 配合用。
    #[serde(default)]
    pub recipient_count: usize,
    /// 我被放在 To 而不是 Cc：**要你办** vs **知会你**。
    #[serde(default)]
    pub addressed_directly: bool,
    #[serde(default)]
    pub has_attachments: bool,
}

impl InfoItem {
    /// 发件人怎么称呼。名字没有就用地址，两个都没有就"未知"。
    ///
    /// **不要返回空串**：通知里出现空的发件人，使用者会以为程序坏了。
    pub fn display_from(&self) -> &str {
        if !self.from_name.trim().is_empty() {
            &self.from_name
        } else if !self.from_addr.trim().is_empty() {
            &self.from_addr
        } else {
            "未知发件人"
        }
    }

    /// 主题怎么显示。没主题的邮件很常见（尤其机器通知）。
    pub fn display_subject(&self) -> &str {
        if self.subject.trim().is_empty() {
            "（无主题）"
        } else {
            &self.subject
        }
    }

    /// 一封邮件够不够"像群发"。打扰判定的确定性规则会用到。
    pub fn looks_bulk(&self) -> bool {
        !self.direct && self.recipient_count > 3
    }
}

/// 一次取回的结果。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FetchBatch {
    pub items: Vec<InfoItem>,
    /// 来源里的未读总数（**不是 `items.len()`**）。
    ///
    /// 列表被 limit 截断时，这个数字告诉使用者"还有多少没看"。
    /// 只报 `items.len()` 会让"一共 200 封未读，我给你看了 20 封"变成
    /// "有 20 封未读"——那是在用截断冒充全部。
    pub total_unseen: usize,
    /// 取不到的条数。**必须报出来**：它意味着"这次没看全"，
    /// 而"没看全"和"没有"对使用者是完全不同的两件事。
    #[serde(default)]
    pub skipped: usize,
}

/// 一个信息源。
pub trait InfoSource: Send + Sync {
    /// 来源名，进台账与通知用。
    fn name(&self) -> &'static str;

    /// 这个源现在可用吗。`Err` 里要说清**为什么不可用**——
    /// "sidecar 没起来"和"没配邮箱"对使用者的意义完全不同。
    fn health(&self) -> Result<(), String>;

    /// 取未读。**按时间从新到旧。**
    fn fetch(&self, limit: usize) -> Result<FetchBatch, String>;

    /// 取一条的正文。摘要不够判断时才调。
    fn read(&self, id: &str) -> Result<String, String>;
}

/// 没有任何信息源。**这是显式的选择，不是失败。**
#[derive(Debug, Default, Clone, Copy)]
pub struct NoSource;

impl InfoSource for NoSource {
    fn name(&self) -> &'static str {
        "无信息源"
    }
    fn health(&self) -> Result<(), String> {
        Err("当前没有配置任何信息源".to_string())
    }
    fn fetch(&self, _limit: usize) -> Result<FetchBatch, String> {
        Err("当前没有配置任何信息源".to_string())
    }
    fn read(&self, _id: &str) -> Result<String, String> {
        Err("当前没有配置任何信息源".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(name: &str, addr: &str, subject: &str) -> InfoItem {
        InfoItem {
            source: "mail".into(),
            id: "1".into(),
            from_name: name.into(),
            from_addr: addr.into(),
            subject: subject.into(),
            preview: String::new(),
            received_at_ms: 0,
            direct: true,
            recipient_count: 1,
            addressed_directly: true,
            has_attachments: false,
        }
    }

    #[test]
    fn display_from_prefers_the_name() {
        assert_eq!(item("张三", "a@x.com", "s").display_from(), "张三");
    }

    #[test]
    fn display_from_falls_back_to_the_address() {
        assert_eq!(item("", "a@x.com", "s").display_from(), "a@x.com");
    }

    #[test]
    fn display_from_never_returns_empty() {
        // 通知里出现空的发件人，使用者会以为程序坏了
        assert_eq!(item("", "", "s").display_from(), "未知发件人");
        assert_eq!(item("   ", "  ", "s").display_from(), "未知发件人");
    }

    #[test]
    fn display_subject_handles_missing_subject() {
        // 机器通知邮件没主题很常见
        assert_eq!(item("a", "a@x", "").display_subject(), "（无主题）");
        assert_eq!(item("a", "a@x", "  ").display_subject(), "（无主题）");
        assert_eq!(item("a", "a@x", "正经主题").display_subject(), "正经主题");
    }

    #[test]
    fn bulk_detection_needs_both_signals() {
        // 只凭"抄送多"或只凭"不只发给我"都会误判
        let mut i = item("a", "a@x", "s");
        i.direct = false;
        i.recipient_count = 10;
        assert!(i.looks_bulk());

        i.recipient_count = 2;
        assert!(!i.looks_bulk(), "只多一个人不算群发");

        i.recipient_count = 10;
        i.direct = true;
        assert!(!i.looks_bulk(), "直接发给我的不算群发");
    }

    #[test]
    fn zero_timestamp_means_unknown_not_now() {
        // 用"现在"填补会让日期坏掉的邮件插到通知队列最前面
        let i = item("a", "a@x", "s");
        assert_eq!(i.received_at_ms, 0);
    }

    #[test]
    fn no_source_reports_a_reason_not_an_empty_list() {
        // **"取不到"不能伪装成"没有"。**
        // 压成空列表会让一次故障看起来像一段安静——而安静是使用者最不会去查的状态。
        let s = NoSource;
        assert!(s.health().is_err());
        assert!(s.fetch(10).is_err());
        assert!(s.read("1").is_err());
        assert_eq!(s.name(), "无信息源");
    }

    #[test]
    fn info_item_round_trips_through_json() {
        // 要能进台账，也要能从 sidecar 的响应里解出来
        let i = item("张三", "a@x.com", "问候");
        let v = serde_json::to_value(&i).unwrap();
        let back: InfoItem = serde_json::from_value(v).unwrap();
        assert_eq!(i, back);
    }

    #[test]
    fn info_item_tolerates_missing_optional_fields() {
        // sidecar 少给一个可选字段不该让整批都解不出来
        let v = serde_json::json!({
            "source": "mail", "id": "7", "from_addr": "a@x.com", "subject": "s"
        });
        let i: InfoItem = serde_json::from_value(v).unwrap();
        assert_eq!(i.id, "7");
        assert!(!i.direct);
        assert_eq!(i.recipient_count, 0);
    }

    #[test]
    fn fetch_batch_distinguishes_total_from_returned() {
        // 只报 items.len() 会让"一共 200 封未读，给你看 20 封"
        // 变成"有 20 封未读"——那是在用截断冒充全部。
        let b = FetchBatch {
            items: vec![item("a", "a@x", "s")],
            total_unseen: 200,
            skipped: 0,
        };
        assert_ne!(b.items.len(), b.total_unseen);
        assert_eq!(b.total_unseen, 200);
    }

    #[test]
    fn fetch_batch_reports_skipped() {
        // "没看全"和"没有"对使用者是完全不同的两件事
        let b = FetchBatch {
            items: vec![],
            total_unseen: 5,
            skipped: 5,
        };
        assert_eq!(b.skipped, 5);
        assert!(b.items.is_empty());
    }
}
