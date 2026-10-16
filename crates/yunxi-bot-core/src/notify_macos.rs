//! 云熙应用自己的通知中心出口；读取权限并核对投递记录，不把提交当成可见。
use crate::notify::{Delivery, Notification, Notifier};
use crate::tool::macos::{bundled_app, request};
use serde_json::{Value, json};

#[derive(Debug, Default)]
pub struct MacNotification;

fn delivery(value: Value) -> Delivery {
    let detail_key = match value["kind"].as_str() {
        Some("confirmed") => "evidence",
        Some("handed_off") => "note",
        Some("blocked" | "failed") => "reason",
        _ => {
            return Delivery::Failed {
                reason: "原生通知回执类型不明确".into(),
            };
        }
    };
    if value[detail_key]
        .as_str()
        .is_none_or(|s| s.trim().is_empty())
    {
        return Delivery::Failed {
            reason: "原生通知回执缺少投递依据".into(),
        };
    }
    serde_json::from_value(value).unwrap_or_else(|_| Delivery::Failed {
        reason: "原生通知回执无效".into(),
    })
}

impl Notifier for MacNotification {
    fn name(&self) -> &'static str {
        "云熙 macOS 通知中心"
    }
    fn notify(&self, n: &Notification) -> Delivery {
        match request(
            &bundled_app(),
            "notification_send",
            &json!({"title":n.title,"body":n.body,"tag":n.tag}),
        ) {
            Ok(value) => delivery(value),
            Err(_) => Delivery::Failed {
                reason: "云熙原生通知调用失败或超时；请检查 mac status，不代表用户已收到".into(),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn notification_receipt_needs_explicit_kind_and_evidence() {
        for value in [
            json!({}),
            json!({"kind":"ok"}),
            json!({"kind":"confirmed"}),
            json!({"kind":"confirmed","evidence":""}),
        ] {
            assert!(!delivery(value).left_the_process());
        }
        assert!(delivery(json!({"kind":"confirmed","evidence":"通知中心标识和内容回读匹配","identifier":"id"})).can_claim_user_notified());
    }
    #[test]
    fn handoff_and_permission_denial_never_claim_delivery() {
        let handed = delivery(json!({"kind":"handed_off","note":"未回读到"}));
        assert!(handed.left_the_process());
        assert!(!handed.can_claim_user_notified());
        for kind in ["blocked", "failed"] {
            assert!(!delivery(json!({"kind":kind,"reason":"未授权或系统错误"})).left_the_process());
        }
    }
}
