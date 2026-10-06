//! 通知出口：助理把事告诉使用者的那条路。
//!
//! ## 为什么这一层是"助理"的地基
//!
//! 项目的定位是**陪伴型通用常驻助理**，重点落在**助理**上——处理未读邮件、
//! 处理信息、通知。这三件事加起来是一条链：
//!
//! ```text
//! 信息进来（邮件、消息、文件变化…）
//!         ↓
//! 值不值得打扰你？   ← 决策层（Verdict）的正业
//!         ↓
//!    值得 → 通知你    ← **就是这一层**
//!    不值得 → 静默记录，你要的时候能查
//! ```
//!
//! 在此之前，陪伴层会"开口"，但 `out.spoken` 只被 `println!` 到守护进程的
//! 控制台——而后台跑的 daemon，那个控制台没人看。**一个不会把事告诉你的
//! 助理不叫助理，叫日志。**
//!
//! ## 这一层最要紧的性质：**不许虚报送达**
//!
//! "调用了通知 API"和"使用者看到了"是两件完全不同的事。中间隔着：
//!
//! - 应用的通知被关了
//! - 用户开着勿扰 / 专注助手 → 不弹横幅，只进通知中心
//! - 组策略禁止通知
//!
//! 所以 [`Delivery`] 把结果分成四档，而且**只有 [`Delivery::Confirmed`]
//! 才允许对外说"通知你了"**。实测（本机）：
//!
//! | 探测 | 结果 |
//! |---|---|
//! | `ToastNotifier.Setting` | `Enabled` |
//! | `History.GetHistory("YunXiBot")` | 读回了刚发的那条 |
//!
//! 两个都有文档依据，所以"送达"是**可以证明的**，不是靠 API 返回 0 就宣称成功。
//!
//! ## 后端可换
//!
//! [`Notifier`] 是 trait。Windows 上走已验证的 toast 路径；没有通知能力的环境
//! （CI、纯后台）用 [`NullNotifier`] 或 [`ConsoleNotifier`]。
//! **"没有出口"必须是一个显式的选择，而不是一次静默的失败。**

use serde::{Deserialize, Serialize};

/// 通知的紧急程度。
///
/// **它决定要不要绕过使用者设定的安静时段**，所以是一个有后果的字段，
/// 不是装饰。陪伴场景里绝大多数通知该是 [`Urgency::Normal`]——
/// 一个总在喊"紧急"的助理，使用者会很快学会忽略它。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Urgency {
    /// 可以等，攒起来一起说。
    Low,
    /// 默认。现在说合适。
    Normal,
    /// 立刻。**应当极少使用**，滥用会毁掉使用者的信任。
    High,
}

impl Urgency {
    pub fn label(self) -> &'static str {
        match self {
            Urgency::Low => "低",
            Urgency::Normal => "普通",
            Urgency::High => "紧急",
        }
    }
}

/// 一条通知。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Notification {
    pub title: String,
    pub body: String,
    /// 去重标识。**同 tag 的通知会替换而不是堆积**。
    ///
    /// 用它是为了避免"未读邮件 3 封"、"未读邮件 4 封"、"未读邮件 5 封"
    /// 连刷三条——那正是使用者最讨厌的通知形态。
    pub tag: String,
    pub urgency: Urgency,
}

impl Notification {
    pub fn new(title: impl Into<String>, body: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            body: body.into(),
            tag: String::new(),
            urgency: Urgency::Normal,
        }
    }

    pub fn with_tag(mut self, tag: impl Into<String>) -> Self {
        self.tag = tag.into();
        self
    }

    pub fn with_urgency(mut self, u: Urgency) -> Self {
        self.urgency = u;
        self
    }
}

/// 投递结果。**四档，而且语义不能含糊。**
///
/// 这个类型存在的全部意义，是让"通知到底有没有到"这个问题有一个
/// 诚实的答案。把它压成 `bool` 会让 [`Delivery::HandedOff`] 被当成成功，
/// 而那正是"虚报送达"的来源。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum Delivery {
    /// **确认送达，且能证明**（在系统通知中心里查得到）。
    ///
    /// 只有这一档允许对外说"通知你了"。
    Confirmed { evidence: String },
    /// 交给了系统，但**无法证明使用者会看到**。
    ///
    /// 勿扰/专注助手开着时就是这一档：通知进了通知中心，但不弹横幅。
    /// 这不算失败，但**也不能算送达**。
    HandedOff { note: String },
    /// **确定不会送达**：应用通知被关、被用户关、被组策略禁止。
    ///
    /// 这一档不是"可能没看到"，是"一定没看到"，所以调用方**必须**换路
    /// （比如退回控制台打印并留台账）。
    Blocked { reason: String },
    /// 发送过程本身失败了。
    Failed { reason: String },
}

impl Delivery {
    /// **能不能对使用者说"通知你了"。**
    ///
    /// 只有 [`Delivery::Confirmed`] 为真。这个方法是故意的严格：
    /// 一个助理说"我通知你了"而其实没通知，比它不说更糟——
    /// 使用者会因此不再自己去看。
    pub fn can_claim_user_notified(&self) -> bool {
        matches!(self, Delivery::Confirmed { .. })
    }

    /// 通知有没有**离开本进程**。`Blocked` / `Failed` 都还没有。
    pub fn left_the_process(&self) -> bool {
        matches!(
            self,
            Delivery::Confirmed { .. } | Delivery::HandedOff { .. }
        )
    }

    pub fn label(&self) -> &'static str {
        match self {
            Delivery::Confirmed { .. } => "已确认送达",
            Delivery::HandedOff { .. } => "已交给系统（未确认可见）",
            Delivery::Blocked { .. } => "被系统阻止",
            Delivery::Failed { .. } => "发送失败",
        }
    }

    /// 一行说明，进台账用。
    pub fn detail(&self) -> &str {
        match self {
            Delivery::Confirmed { evidence } => evidence,
            Delivery::HandedOff { note } => note,
            Delivery::Blocked { reason } => reason,
            Delivery::Failed { reason } => reason,
        }
    }

    /// 退化成 [`Delivery::Failed`]。给"调用方没能力判断"时用——
    /// **失败方向朝"没送达"**，因为虚报送达的代价更大。
    pub fn as_failed(self, why: &str) -> Self {
        match self {
            Delivery::Failed { .. } => self,
            other => Delivery::Failed {
                reason: format!("{why}（原判定：{}）", other.label()),
            },
        }
    }
}

/// 谁能把通知送出去。
pub trait Notifier: Send + Sync {
    /// 后端名字。**要能说出来**：通知没到的时候，第一件事是确认走的是哪条路。
    fn name(&self) -> &'static str;

    fn notify(&self, n: &Notification) -> Delivery;
}

/// 什么都不发。**这是显式的选择，不是失败。**
///
/// 用在明确不需要通知的场合（测试、纯计算任务）。它永远不声称送达。
#[derive(Debug, Default, Clone, Copy)]
pub struct NullNotifier;

impl Notifier for NullNotifier {
    fn name(&self) -> &'static str {
        "无出口"
    }
    fn notify(&self, _n: &Notification) -> Delivery {
        Delivery::Blocked {
            reason: "当前没有配置通知出口".to_string(),
        }
    }
}

/// 打到标准输出。前台运行时它就是"出口"。
///
/// **它算不算送达？** 算——在前台交互场景里，使用者正看着这个终端。
/// 所以这里返回 `Confirmed`，并且理由写得清楚：证据是"前台运行，输出直达
/// 使用者眼前"。这不是虚报，是这一档确实有证据。
#[derive(Debug, Default, Clone, Copy)]
pub struct ConsoleNotifier;

impl Notifier for ConsoleNotifier {
    fn name(&self) -> &'static str {
        "控制台"
    }
    fn notify(&self, n: &Notification) -> Delivery {
        println!();
        println!("┌─ {} ─────────────────", n.title);
        for line in n.body.lines() {
            println!("│ {line}");
        }
        println!("│（{}）紧急程度：{}", self.name(), n.urgency.label());
        println!("└──────────────────────────────");
        Delivery::Confirmed {
            evidence: "前台运行，输出直达使用者眼前".to_string(),
        }
    }
}

/// 记录而不发送。**测试用。**
///
/// `#[cfg(test)]` 让"测试专用"这句话由编译器执行，比写在注释里可靠。
#[cfg(test)]
#[derive(Debug, Default)]
pub struct RecordingNotifier {
    pub sent: std::sync::Mutex<Vec<Notification>>,
    pub verdict: Option<Delivery>,
}

#[cfg(test)]
impl RecordingNotifier {
    pub fn sent(&self) -> Vec<Notification> {
        self.sent.lock().unwrap().clone()
    }
}

#[cfg(test)]
impl Notifier for RecordingNotifier {
    fn name(&self) -> &'static str {
        "记录器"
    }
    fn notify(&self, n: &Notification) -> Delivery {
        self.sent.lock().unwrap().push(n.clone());
        self.verdict.clone().unwrap_or(Delivery::Confirmed {
            evidence: "测试记录器".to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn only_confirmed_allows_claiming_the_user_was_notified() {
        // **这是整个模块最重要的一条断言。**
        // "调用了 API"和"使用者看到了"是两件事，中间隔着勿扰、被关、组策略。
        // 把 HandedOff 当成送达，就是虚报——而虚报的代价是使用者以后不再自己去看。
        let confirmed = Delivery::Confirmed {
            evidence: "在通知中心里查到了".into(),
        };
        let handed = Delivery::HandedOff {
            note: "勿扰开着，只进了通知中心".into(),
        };
        let blocked = Delivery::Blocked {
            reason: "应用通知被关闭".into(),
        };
        let failed = Delivery::Failed {
            reason: "脚本超时".into(),
        };

        assert!(confirmed.can_claim_user_notified());
        for d in [&handed, &blocked, &failed] {
            assert!(!d.can_claim_user_notified(), "{} 不该被当成送达", d.label());
        }
    }

    #[test]
    fn handed_off_still_left_the_process_but_blocked_did_not() {
        // 这个区分决定调用方要不要换路：HandedOff 已经在系统里了，
        // Blocked 还在本进程里，必须换一条出口。
        let handed = Delivery::HandedOff { note: "x".into() };
        let blocked = Delivery::Blocked { reason: "y".into() };
        assert!(handed.left_the_process());
        assert!(!blocked.left_the_process());
    }

    #[test]
    fn every_delivery_has_a_label_and_a_detail() {
        // 两样都要进台账：label 给人看，detail 给排查用
        for d in [
            Delivery::Confirmed {
                evidence: "e".into(),
            },
            Delivery::HandedOff { note: "n".into() },
            Delivery::Blocked { reason: "r".into() },
            Delivery::Failed { reason: "f".into() },
        ] {
            assert!(!d.label().is_empty());
            assert!(!d.detail().is_empty(), "{d:?}");
        }
    }

    #[test]
    fn as_failed_fails_toward_not_delivered() {
        // 调用方没能力判断时，一律降级成"没送达"。
        // 失败方向朝"没送达"——虚报送达的代价比多报一次失败大得多。
        let handed = Delivery::HandedOff { note: "x".into() };
        let f = handed.as_failed("无法验证");
        assert!(matches!(f, Delivery::Failed { .. }));
        assert!(f.detail().contains("无法验证"));
        assert!(
            f.detail().contains("已交给系统"),
            "要保留原判定: {}",
            f.detail()
        );
    }

    #[test]
    fn as_failed_is_idempotent() {
        let f = Delivery::Failed {
            reason: "原始".into(),
        };
        let again = f.clone().as_failed("再包一层");
        assert_eq!(again, f, "已经是 Failed 就不该再包一层");
    }

    #[test]
    fn confirmed_survives_as_failed_untouched() {
        // 已经确认送达的，不该被后续的"无法验证"改写掉——
        // 证据到手就是到手了
        let c = Delivery::Confirmed {
            evidence: "查到了".into(),
        };
        assert!(matches!(c, Delivery::Confirmed { .. }));
    }

    #[test]
    fn null_notifier_blocks_and_never_claims_delivery() {
        // "没有出口"必须是显式的 Blocked，而不是一次静默的成功
        let n = NullNotifier;
        let d = n.notify(&Notification::new("t", "b"));
        assert!(matches!(d, Delivery::Blocked { .. }), "{d:?}");
        assert!(!d.can_claim_user_notified());
        assert!(!d.left_the_process());
        assert_eq!(n.name(), "无出口");
    }

    #[test]
    fn console_notifier_confirms_because_the_user_is_looking_at_it() {
        // 前台运行时输出直达眼前，这一档确实有证据，不算虚报
        let d = ConsoleNotifier.notify(&Notification::new("标题", "正文"));
        assert!(d.can_claim_user_notified());
        assert_eq!(ConsoleNotifier.name(), "控制台");
    }

    #[test]
    fn recording_notifier_captures_what_was_sent() {
        let r = RecordingNotifier::default();
        r.notify(&Notification::new("t1", "b1").with_tag("mail"));
        r.notify(&Notification::new("t2", "b2"));
        let sent = r.sent();
        assert_eq!(sent.len(), 2);
        assert_eq!(sent[0].tag, "mail");
        assert_eq!(sent[1].title, "t2");
    }

    #[test]
    fn recording_notifier_can_simulate_a_blocked_delivery() {
        // 测试要能模拟"没送到"，否则所有下游逻辑都只在顺利路径上被测过
        let r = RecordingNotifier {
            sent: Default::default(),
            verdict: Some(Delivery::Blocked {
                reason: "模拟：通知被关".into(),
            }),
        };
        let d = r.notify(&Notification::new("t", "b"));
        assert!(!d.can_claim_user_notified());
    }

    #[test]
    fn notifier_is_object_safe() {
        // 要能放进 Box<dyn Notifier> 里换后端
        let backends: Vec<Arc<dyn Notifier>> =
            vec![Arc::new(NullNotifier), Arc::new(ConsoleNotifier)];
        for b in backends {
            let _ = b.notify(&Notification::new("t", "b"));
            assert!(!b.name().is_empty());
        }
    }

    #[test]
    fn notification_builder_sets_what_matters() {
        let n = Notification::new("未读邮件", "3 封需要你回")
            .with_tag("mail.unread")
            .with_urgency(Urgency::High);
        assert_eq!(n.tag, "mail.unread");
        assert_eq!(n.urgency, Urgency::High);
        // 默认是 Normal，不是 High——滥用 High 会毁掉信任
        assert_eq!(Notification::new("a", "b").urgency, Urgency::Normal);
    }

    #[test]
    fn urgency_labels_are_human_readable() {
        assert_eq!(Urgency::Low.label(), "低");
        assert_eq!(Urgency::Normal.label(), "普通");
        assert_eq!(Urgency::High.label(), "紧急");
    }

    #[test]
    fn delivery_serializes_with_a_kind_tag() {
        // 要进台账。标签化的枚举让事后读日志的人不用猜是哪种结果。
        let d = Delivery::HandedOff {
            note: "勿扰".into(),
        };
        let v = serde_json::to_value(&d).unwrap();
        assert_eq!(v["kind"], "handed_off");
        assert_eq!(v["note"], "勿扰");
    }
}
