//! 反馈回写：使用者对一条通知的处置，以及**下一次判断要不要记得它**。
//!
//! ## 这一层解决两个不同的问题
//!
//! **问题一：同一类信息不该反复问。**
//!
//! 没有这一层时，`check` 每跑一次就会把**所有未读邮件重新判一遍**。
//! 一封被"攒着"的邮件，下一轮又会被判一次——如果那轮恰好落在非安静时段、
//! 又恰好模型说 Speak，使用者就会**为同一封邮件被通知第二次**。
//! 那不是"判定不准"，是**没有记忆**。
//!
//! 所以：[`seen`] 从台账里取出"已经判过的"，`check` 跳过它们。
//!
//! **问题二：使用者说"这类以后别烦我"，得真的记住。**
//!
//! 这句话要落成两样东西：
//!
//! | 落到哪 | 有什么用 |
//! |---|---|
//! | 台账（[`crate::EventKind::FeedbackRecorded`]） | 事后能说清这条规则**是谁在什么时候为什么加的** |
//! | 策略文件（[`crate::triage::save_policy`]） | 下一次判断真的会用它 |
//!
//! 只写台账不改策略 = 记了不做；只改策略不写台账 = 做了但说不清来由。
//! 两样都要。
//!
//! ## "这类"是多大一类
//!
//! [`Scope`] 是这个问题的答案，而且**默认取最窄的那一档**：
//!
//! - [`Scope::Sender`]（默认）——只屏蔽这一个地址
//! - [`Scope::Domain`]——屏蔽整个域名
//!
//! 默认窄是刻意的：使用者对着 `noreply@shop.example.com` 的一封促销说
//! "别烦我"，多半不想因此收不到这家公司的**订单发货通知**。
//! 想扩大范围要显式说。
//!
//! **没有 [`Scope::Subject`] 或"屏蔽所有群发"这种选项**：那类规则看起来方便，
//! 但使用者说那句话时心里想的是具体的一封，把他的意思扩大到一整类
//! 是替他做了他没做的决定。

use serde::{Deserialize, Serialize};

use crate::info::InfoItem;
use crate::triage::TriagePolicy;

/// 使用者对一条通知的处置。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Feedback {
    /// 看过了，有用。
    Read,
    /// 看到了但没管（划掉 / 没点）。
    Ignored,
    /// **这类以后别烦我。** 要落成持久规则。
    Never,
}

impl Feedback {
    pub fn label(self) -> &'static str {
        match self {
            Feedback::Read => "已读",
            Feedback::Ignored => "忽略",
            Feedback::Never => "以后别烦我",
        }
    }

    /// 这条反馈要不要改策略。
    ///
    /// 只有 [`Feedback::Never`] 会。**"已读"和"忽略"不该自动改策略**——
    /// 忽略一封不等于永远不想看这一类，把两者混起来会让系统
    /// 悄悄越走越窄，最后什么都不告诉你。
    pub fn changes_policy(self) -> bool {
        self == Feedback::Never
    }
}

/// "这类"是多大一类。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    /// 只这一个发件人地址。**默认。**
    Sender,
    /// 整个发件人域名。
    Domain,
}

impl Scope {
    pub fn label(self) -> &'static str {
        match self {
            Scope::Sender => "只这个发件人",
            Scope::Domain => "整个域名",
        }
    }

    /// 从一条信息推出要加进黑名单的规则串。
    ///
    /// 推不出来时返回 `Err`——**不猜**。一个地址都拿不到的邮件，
    /// 硬编一条 `@` 之类的规则会屏蔽掉全部邮件。
    pub fn rule_for(self, item: &InfoItem) -> Result<String, String> {
        let addr = item.from_addr.trim();
        if addr.is_empty() {
            return Err("这条信息没有发件人地址，无法据此建规则".to_string());
        }
        let Some((_, host)) = addr.rsplit_once('@') else {
            return Err(format!("发件人地址 {addr} 里没有 @，无法据此建规则"));
        };
        if host.is_empty() {
            return Err(format!("发件人地址 {addr} 的域名是空的"));
        }
        Ok(match self {
            Scope::Sender => addr.to_ascii_lowercase(),
            Scope::Domain => format!("@{}", host.to_ascii_lowercase()),
        })
    }
}

/// 把一条"以后别烦我"落成策略改动。
///
/// 返回新的策略与**这次到底加了什么**（用于回显与台账）。
/// 规则已存在时返回 `added = None`——**不重复加**，
/// 否则策略文件会被同一句话撑大。
pub fn apply_never(
    policy: &TriagePolicy,
    item: &InfoItem,
    scope: Scope,
) -> Result<(TriagePolicy, Option<String>), String> {
    let rule = scope.rule_for(item)?;
    let mut next = policy.clone();

    // 已经在黑名单里就什么都不做
    if next
        .block_senders
        .iter()
        .any(|r| r.eq_ignore_ascii_case(&rule))
    {
        return Ok((next, None));
    }

    // **从白名单里摘掉。** 否则两条规则同时命中，而黑名单优先级更高——
    // 结果是"能屏蔽掉"，但策略文件里留着一对自相矛盾的规则，
    // 下次看到它的人会不知道哪条算数。
    next.allow_senders
        .retain(|r| !r.eq_ignore_ascii_case(&rule));

    next.block_senders.push(rule.clone());
    Ok((next, Some(rule)))
}

/// 已经判过的信息 id。
///
/// ## 为什么要从台账里取，而不是另存一份"已读列表"
///
/// 因为**台账已经是唯一事实来源**（ADR D11）。另存一份就有了两个真相，
/// 而它们迟早会不一致——那时"这条到底判过没有"就没有答案了。
///
/// ## 什么算"判过"
///
/// **只有走到过通知那一步的才算**（`NoticeSent`）。
/// 被判定成 `Hold` 的那些**不算**——攒着的意思就是"以后再说"，
/// 把它们也标成"判过"会让攒着的东西永远不再被提及。
pub fn seen(events: &[crate::Event]) -> std::collections::BTreeSet<String> {
    use crate::EventKind;
    let mut out = std::collections::BTreeSet::new();
    for ev in events {
        match ev.kind {
            EventKind::NoticeSent => {
                // 演练模式**不算真的通知过**——它没到你眼前，
                // 下一轮该重新判。不然演练一次就把邮件永久吞了。
                if ev.data.get("dry_run").and_then(|v| v.as_bool()) == Some(true) {
                    continue;
                }
                if let Some(id) = ev.data.get("id").and_then(|v| v.as_str()) {
                    out.insert(format!(
                        "{}:{}",
                        ev.data
                            .get("source")
                            .and_then(|v| v.as_str())
                            .unwrap_or("?"),
                        id
                    ));
                }
            }
            // 使用者明确处置过的也算：说过"以后别烦我"的东西不该再冒出来
            EventKind::FeedbackRecorded => {
                if let Some(id) = ev.data.get("id").and_then(|v| v.as_str()) {
                    out.insert(format!(
                        "{}:{}",
                        ev.data
                            .get("source")
                            .and_then(|v| v.as_str())
                            .unwrap_or("?"),
                        id
                    ));
                }
            }
            _ => {}
        }
    }
    out
}

/// 这条信息是不是已经处理过了。
pub fn already_seen(seen: &std::collections::BTreeSet<String>, item: &InfoItem) -> bool {
    seen.contains(&format!("{}:{}", item.source, item.id))
}

/// 从台账里找回一条信息**当时的样貌**。
///
/// ## 为什么不重新去信息源取一遍
///
/// 两个理由，第二个更重要：
///
/// 1. **不用联网。** sidecar 没起来时"以后别烦我"仍然应该能生效——
///    那正是使用者最想说这句话的时刻（刚被烦到）。
/// 2. **它给的是判定时看到的那一条。** 重新取一遍有可能取到同一 UID 的
///    更新版本（发件人换了、主题改了），于是规则建在**另一条信息**上。
///    台账里的记录才是判定的依据，规则就该建在它上面。
///
/// 从 `InfoTriaged` 和 `NoticeSent` 两处找：前者记了 `from` 与 `subject`，
/// 后者记了投递结果。判定过但没通知的（Hold/Quiet）也该能反馈——
/// 使用者可能就是想对一条"攒着"的说"以后别提了"。
pub fn find_item(events: &[crate::Event], id: &str) -> Option<InfoItem> {
    use crate::EventKind;
    // 从后往前：同一条信息可能被判过多次（攒着的东西下轮会再判一次），
    // 最近那次才是使用者刚看到的
    events.iter().rev().find_map(|ev| match ev.kind {
        EventKind::InfoTriaged | EventKind::NoticeSent => {
            let got = ev.data.get("id").and_then(|v| v.as_str())?;
            if got != id {
                return None;
            }
            item_from_event(ev)
        }
        _ => None,
    })
}

/// 最近一条**真的通知到使用者眼前**的信息。
///
/// "刚才那条以后别烦我"是最自然的说法，而这个函数就是它的落点。
/// **只认确认送达或已交给系统的**——把一条还在演练里的、
/// 或者根本没发出去的算进来，会让使用者对着一封没见过的邮件建规则。
pub fn last_notified(events: &[crate::Event]) -> Option<InfoItem> {
    use crate::EventKind;
    events.iter().rev().find_map(|ev| match ev.kind {
        EventKind::NoticeSent => {
            if ev.data.get("dry_run").and_then(|v| v.as_bool()) == Some(true) {
                return None;
            }
            // `confirmed` 为假且不是 dry_run，说明是"已交给系统但未确认可见"。
            // 那种也算通知过了——使用者可能已经在通知中心里看到了。
            item_from_event(ev)
        }
        _ => None,
    })
}

/// 从事件载荷里还原一条 [`InfoItem`]。
///
/// 缺字段就给空值，**不猜**。发件人地址缺失时 [`Scope::rule_for`]
/// 会报错——那比在这里编一个地址安全得多。
fn item_from_event(ev: &crate::Event) -> Option<InfoItem> {
    let s = |k: &str| -> String {
        ev.data
            .get(k)
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string()
    };
    let id = s("id");
    if id.is_empty() {
        return None;
    }
    Some(InfoItem {
        source: {
            let src = s("source");
            if src.is_empty() {
                "mail".to_string()
            } else {
                src
            }
        },
        id,
        from_name: s("from_name"),
        from_addr: s("from"),
        subject: s("subject"),
        preview: String::new(),
        received_at_ms: 0,
        direct: false,
        recipient_count: 0,
        addressed_directly: false,
        has_attachments: false,
    })
}

/// 事件载荷：一条反馈。供调用方 append 到台账。
///
/// **要记下"当时那条信息长什么样"**（发件人、主题），而不只是记一个 id。
/// 事后翻台账的人看到 `id=17` 什么也判断不了；看到
/// "`noreply@shop.example.com` 的「您的订单已发货」"才知道这条规则合不合理。
pub fn feedback_event_data(
    item: &InfoItem,
    feedback: Feedback,
    scope: Option<Scope>,
    rule: Option<&str>,
    note: &str,
) -> serde_json::Value {
    serde_json::json!({
        "source": item.source,
        "id": item.id,
        "from": item.from_addr,
        "subject": item.subject,
        "feedback": match feedback {
            Feedback::Read => "read",
            Feedback::Ignored => "ignored",
            Feedback::Never => "never",
        },
        "scope": scope.map(|s| match s {
            Scope::Sender => "sender",
            Scope::Domain => "domain",
        }),
        // 这条反馈**实际改动了什么**。None 表示没改（比如规则已存在）。
        "rule_added": rule,
        "note": note,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Event, EventKind};

    fn item(addr: &str, subject: &str) -> InfoItem {
        InfoItem {
            source: "mail".into(),
            id: "42".into(),
            from_name: "Shop".into(),
            from_addr: addr.into(),
            subject: subject.into(),
            preview: String::new(),
            received_at_ms: 1_000,
            direct: true,
            recipient_count: 1,
            addressed_directly: true,
            has_attachments: false,
        }
    }

    fn ev(kind: EventKind, data: serde_json::Value) -> Event {
        Event {
            seq: 1,
            at: 0,
            kind,
            span: None,
            job: None,
            data,
        }
    }

    // ---- 反馈的语义 ----

    #[test]
    fn only_never_changes_the_policy() {
        // **"已读"和"忽略"不该自动改策略。**
        // 忽略一封不等于永远不想看这一类，把两者混起来会让系统
        // 悄悄越走越窄，最后什么都不告诉你。
        assert!(Feedback::Never.changes_policy());
        assert!(!Feedback::Read.changes_policy());
        assert!(!Feedback::Ignored.changes_policy());
    }

    #[test]
    fn feedback_labels_are_human_readable() {
        assert_eq!(Feedback::Never.label(), "以后别烦我");
        assert_eq!(Feedback::Read.label(), "已读");
        assert_eq!(Feedback::Ignored.label(), "忽略");
    }

    // ---- 规则推导 ----

    #[test]
    fn sender_scope_uses_the_full_address() {
        let r = Scope::Sender.rule_for(&item("Shop <a@x.com>", "s"));
        // `from_addr` 已经由 sidecar 拆好了，这里拿到的就是纯地址
        assert_eq!(r.unwrap(), "shop <a@x.com>".to_ascii_lowercase());
    }

    #[test]
    fn sender_scope_on_a_plain_address() {
        assert_eq!(
            Scope::Sender
                .rule_for(&item("noreply@shop.example.com", "s"))
                .unwrap(),
            "noreply@shop.example.com"
        );
    }

    #[test]
    fn domain_scope_uses_at_host() {
        // 前缀 @ 是给对方匹配用的形式（见 triage::sender_matches）
        assert_eq!(
            Scope::Domain
                .rule_for(&item("noreply@shop.example.com", "s"))
                .unwrap(),
            "@shop.example.com"
        );
    }

    #[test]
    fn scope_is_case_insensitive() {
        assert_eq!(
            Scope::Sender
                .rule_for(&item("Noreply@Shop.Example.COM", "s"))
                .unwrap(),
            "noreply@shop.example.com"
        );
    }

    #[test]
    fn a_missing_address_is_an_error_not_a_guess() {
        // **不猜。** 一个地址都拿不到的邮件，硬编一条规则会屏蔽掉全部邮件。
        assert!(Scope::Sender.rule_for(&item("", "s")).is_err());
        assert!(Scope::Sender.rule_for(&item("   ", "s")).is_err());
        assert!(Scope::Sender.rule_for(&item("no-at-sign", "s")).is_err());
        assert!(Scope::Sender.rule_for(&item("user@", "s")).is_err());
    }

    #[test]
    fn the_error_explains_why() {
        let e = Scope::Sender
            .rule_for(&item("no-at-sign", "s"))
            .unwrap_err();
        assert!(e.contains("no-at-sign"), "要把出问题的地址说出来: {e}");
    }

    // ---- 落成策略改动 ----

    #[test]
    fn never_adds_a_block_rule() {
        let (p, added) = apply_never(
            &TriagePolicy::default(),
            &item("noreply@shop.example.com", "促销"),
            Scope::Sender,
        )
        .unwrap();
        assert_eq!(added.as_deref(), Some("noreply@shop.example.com"));
        assert_eq!(
            p.block_senders,
            vec!["noreply@shop.example.com".to_string()]
        );
    }

    #[test]
    fn never_honours_the_domain_scope() {
        let (p, added) = apply_never(
            &TriagePolicy::default(),
            &item("noreply@shop.example.com", "s"),
            Scope::Domain,
        )
        .unwrap();
        assert_eq!(added.as_deref(), Some("@shop.example.com"));
        assert_eq!(p.block_senders, vec!["@shop.example.com".to_string()]);
    }

    #[test]
    fn adding_the_same_rule_twice_is_a_no_op() {
        // 否则策略文件会被同一句话反复撑大
        let first = apply_never(
            &TriagePolicy::default(),
            &item("a@x.com", "s"),
            Scope::Sender,
        )
        .unwrap()
        .0;
        let (second, added) = apply_never(&first, &item("a@x.com", "s"), Scope::Sender).unwrap();
        assert!(added.is_none(), "重复加不该再报'加了'");
        assert_eq!(second.block_senders.len(), 1);
    }

    #[test]
    fn duplicate_detection_is_case_insensitive() {
        let mut policy = TriagePolicy::default();
        policy.block_senders.push("a@x.com".into());
        let (p, added) = apply_never(&policy, &item("A@X.COM", "s"), Scope::Sender).unwrap();
        assert!(added.is_none());
        assert_eq!(p.block_senders.len(), 1);
    }

    #[test]
    fn never_removes_a_contradicting_allow_rule() {
        // **不清掉的话会留下一对自相矛盾的规则。**
        // 黑名单优先级更高所以行为是对的，但下次看到这个文件的人
        // 不知道哪条算数。
        let policy = TriagePolicy {
            allow_senders: vec!["a@x.com".into(), "@friend.com".into()],
            ..Default::default()
        };
        let (p, _) = apply_never(&policy, &item("a@x.com", "s"), Scope::Sender).unwrap();
        assert_eq!(p.block_senders, vec!["a@x.com".to_string()]);
        assert_eq!(
            p.allow_senders,
            vec!["@friend.com".to_string()],
            "只该摘掉冲突的那条，别的白名单要留着"
        );
    }

    #[test]
    fn the_returned_policy_is_a_new_value() {
        // 改策略要产出新值而不是就地改——调用方失败时可以整个丢掉
        let original = TriagePolicy::default();
        let (_p, _) = apply_never(&original, &item("a@x.com", "s"), Scope::Sender).unwrap();
        assert!(original.block_senders.is_empty(), "原策略不该被改动");
    }

    // ---- 已经判过的 ----

    #[test]
    fn notice_sent_marks_an_item_as_seen() {
        // **这一条解决"同一封邮件被通知两次"。**
        // 没有它，check 每跑一次都会把未读重新判一遍。
        let events = vec![ev(
            EventKind::NoticeSent,
            serde_json::json!({"source":"mail","id":"42","confirmed":true}),
        )];
        let s = seen(&events);
        assert!(already_seen(&s, &item("a@x.com", "s")));
    }

    #[test]
    fn dry_run_does_not_mark_as_seen() {
        // **演练模式没到你眼前，下一轮该重新判。**
        // 不算的话，跑一次演练就把邮件永久吞了。
        let events = vec![ev(
            EventKind::NoticeSent,
            serde_json::json!({"source":"mail","id":"42","dry_run":true}),
        )];
        assert!(!already_seen(&seen(&events), &item("a@x.com", "s")));
    }

    #[test]
    fn held_items_are_not_marked_as_seen() {
        // **攒着的意思就是"以后再说"。**
        // 把 Hold 也标成"判过"会让攒着的东西永远不再被提及——
        // 而它恰恰是应该在被提一次的那一类。
        let events = vec![ev(
            EventKind::InfoTriaged,
            serde_json::json!({"source":"mail","id":"42","action":"Hold"}),
        )];
        assert!(!already_seen(&seen(&events), &item("a@x.com", "s")));
    }

    #[test]
    fn feedback_marks_as_seen() {
        // 使用者明确处置过的东西不该再冒出来
        let events = vec![ev(
            EventKind::FeedbackRecorded,
            serde_json::json!({"source":"mail","id":"42","feedback":"never"}),
        )];
        assert!(already_seen(&seen(&events), &item("a@x.com", "s")));
    }

    #[test]
    fn seen_is_scoped_by_source() {
        // 邮件的 id=42 和消息的 id=42 是两条不同的东西
        let events = vec![ev(
            EventKind::NoticeSent,
            serde_json::json!({"source":"mail","id":"42"}),
        )];
        let s = seen(&events);
        let mut other = item("a@x.com", "s");
        other.source = "calendar".into();
        assert!(!already_seen(&s, &other), "别把不同来源的同号搞混");
    }

    #[test]
    fn events_without_an_id_are_ignored() {
        // 老台账里可能有缺字段的事件，不该因此 panic 或产生垃圾条目
        let events = vec![
            ev(EventKind::NoticeSent, serde_json::json!({})),
            ev(EventKind::NoticeSent, serde_json::json!({"id": 42})),
            ev(EventKind::InfoFetched, serde_json::json!({"count": 3})),
        ];
        assert!(seen(&events).is_empty());
    }

    #[test]
    fn unrelated_event_kinds_do_not_mark_as_seen() {
        let events = vec![ev(
            EventKind::InfoFetched,
            serde_json::json!({"source":"mail","id":"42"}),
        )];
        assert!(!already_seen(&seen(&events), &item("a@x.com", "s")));
    }

    // ---- 台账载荷 ----

    #[test]
    fn the_payload_records_what_the_item_looked_like() {
        // **要记下当时那条信息长什么样，而不只是 id。**
        // 事后翻台账的人看到 id=17 什么也判断不了；看到
        // "noreply@shop.example.com 的「促销」"才知道这条规则合不合理。
        let d = feedback_event_data(
            &item("noreply@shop.example.com", "限时促销"),
            Feedback::Never,
            Some(Scope::Sender),
            Some("noreply@shop.example.com"),
            "使用者点了以后别烦我",
        );
        assert_eq!(d["from"], "noreply@shop.example.com");
        assert_eq!(d["subject"], "限时促销");
        assert_eq!(d["feedback"], "never");
        assert_eq!(d["scope"], "sender");
        assert_eq!(d["rule_added"], "noreply@shop.example.com");
    }

    #[test]
    fn the_payload_marks_when_nothing_was_added() {
        // 规则已存在时 `rule_added` 要是 null，不能假装改了
        let d = feedback_event_data(
            &item("a@x.com", "s"),
            Feedback::Never,
            Some(Scope::Sender),
            None,
            "规则已存在",
        );
        assert!(d["rule_added"].is_null());
    }

    #[test]
    fn read_and_ignored_carry_no_scope_or_rule() {
        let d = feedback_event_data(&item("a@x.com", "s"), Feedback::Read, None, None, "");
        assert_eq!(d["feedback"], "read");
        assert!(d["scope"].is_null());
        assert!(d["rule_added"].is_null());
    }

    #[test]
    fn feedback_round_trips_through_json() {
        for f in [Feedback::Read, Feedback::Ignored, Feedback::Never] {
            let v = serde_json::to_value(f).unwrap();
            assert_eq!(serde_json::from_value::<Feedback>(v).unwrap(), f);
        }
        for s in [Scope::Sender, Scope::Domain] {
            let v = serde_json::to_value(s).unwrap();
            assert_eq!(serde_json::from_value::<Scope>(v).unwrap(), s);
        }
    }

    // ---- 从台账里找回当时那条 ----

    #[test]
    fn finds_an_item_from_a_triage_event() {
        let events = vec![ev(
            EventKind::InfoTriaged,
            serde_json::json!({
                "source":"mail","id":"42",
                "from":"noreply@shop.example.com","subject":"限时促销"
            }),
        )];
        let it = find_item(&events, "42").expect("该找到");
        assert_eq!(it.from_addr, "noreply@shop.example.com");
        assert_eq!(it.subject, "限时促销");
        assert_eq!(it.source, "mail");
    }

    #[test]
    fn finds_an_item_from_a_notice_event() {
        let events = vec![ev(
            EventKind::NoticeSent,
            serde_json::json!({"source":"mail","id":"7","from":"a@x.com","subject":"s"}),
        )];
        assert_eq!(find_item(&events, "7").unwrap().from_addr, "a@x.com");
    }

    #[test]
    fn the_most_recent_judgement_wins() {
        // 攒着的东西下一轮会被再判一次。使用者看到的、想反馈的是**最近那次**。
        let events = vec![
            ev(
                EventKind::InfoTriaged,
                serde_json::json!({"source":"mail","id":"42","from":"old@x.com","subject":"旧"}),
            ),
            ev(
                EventKind::InfoTriaged,
                serde_json::json!({"source":"mail","id":"42","from":"new@x.com","subject":"新"}),
            ),
        ];
        let it = find_item(&events, "42").unwrap();
        assert_eq!(it.from_addr, "new@x.com", "该取最近一次的记录");
    }

    #[test]
    fn find_item_returns_nothing_for_an_unknown_id() {
        let events = vec![ev(
            EventKind::InfoTriaged,
            serde_json::json!({"source":"mail","id":"42","from":"a@x.com"}),
        )];
        assert!(find_item(&events, "999").is_none());
        assert!(find_item(&[], "42").is_none());
    }

    #[test]
    fn last_notified_skips_dry_runs() {
        // **别让使用者对着一封没见过的邮件建规则。**
        let events = vec![ev(
            EventKind::NoticeSent,
            serde_json::json!({"source":"mail","id":"9","from":"a@x.com","dry_run":true}),
        )];
        assert!(last_notified(&events).is_none());
    }

    #[test]
    fn last_notified_accepts_handed_off_deliveries() {
        // "已交给系统但未确认可见"也算通知过了——使用者可能已经在
        // 通知中心里看到了
        let events = vec![ev(
            EventKind::NoticeSent,
            serde_json::json!({
                "source":"mail","id":"9","from":"a@x.com","confirmed":false
            }),
        )];
        assert_eq!(last_notified(&events).unwrap().id, "9");
    }

    #[test]
    fn last_notified_takes_the_newest() {
        let events = vec![
            ev(
                EventKind::NoticeSent,
                serde_json::json!({"source":"mail","id":"1","from":"a@x.com"}),
            ),
            ev(
                EventKind::NoticeSent,
                serde_json::json!({"source":"mail","id":"2","from":"b@x.com"}),
            ),
        ];
        assert_eq!(last_notified(&events).unwrap().id, "2");
    }

    #[test]
    fn an_event_without_a_sender_yields_an_item_that_cannot_become_a_rule() {
        // **不猜。** 缺地址时 `rule_for` 要报错，而不是编一个地址出来。
        let events = vec![ev(
            EventKind::InfoTriaged,
            serde_json::json!({"source":"mail","id":"42","subject":"s"}),
        )];
        let it = find_item(&events, "42").expect("有 id 就该找得到");
        assert!(Scope::Sender.rule_for(&it).is_err());
        assert!(apply_never(&TriagePolicy::default(), &it, Scope::Sender).is_err());
    }

    #[test]
    fn an_event_without_an_id_is_not_returned() {
        let events = vec![ev(
            EventKind::InfoTriaged,
            serde_json::json!({"source":"mail","from":"a@x.com"}),
        )];
        assert!(find_item(&events, "42").is_none());
    }

    // ---- 两头连起来：反馈 -> 规则 -> 下次不再打扰 ----

    #[test]
    fn a_never_feedback_actually_stops_the_next_one() {
        // 端到端一点：**这正是验收标准里那句
        // "用户说一句这类以后别烦我 → 系统记住，下一条同类不再打扰"。**
        use crate::companion::Intervention;
        use crate::triage::{deterministic_triage, rules};

        // 1. 系统判过一条，记进台账
        let events = vec![ev(
            EventKind::InfoTriaged,
            serde_json::json!({
                "source":"mail","id":"42",
                "from":"noreply@shop.example.com","subject":"限时促销"
            }),
        )];

        // 2. 使用者说"以后别烦我"
        let it = find_item(&events, "42").unwrap();
        let (policy, added) = apply_never(&TriagePolicy::default(), &it, Scope::Sender).unwrap();
        assert_eq!(added.as_deref(), Some("noreply@shop.example.com"));

        // 3. **下一条同类不再打扰**
        let next = item("noreply@shop.example.com", "双十一又来了");
        let (action, rule, _) = deterministic_triage(&next, &policy).expect("黑名单该命中");
        assert_eq!(action, Intervention::Quiet);
        assert_eq!(rule, rules::BLOCKED_SENDER);
    }

    #[test]
    fn the_domain_scope_stops_the_whole_company() {
        use crate::companion::Intervention;
        use crate::triage::deterministic_triage;

        let events = vec![ev(
            EventKind::InfoTriaged,
            serde_json::json!({
                "source":"mail","id":"42","from":"noreply@shop.example.com"
            }),
        )];
        let it = find_item(&events, "42").unwrap();
        let (policy, _) = apply_never(&TriagePolicy::default(), &it, Scope::Domain).unwrap();

        let (action, _, _) =
            deterministic_triage(&item("promo@shop.example.com", "s"), &policy).unwrap();
        assert_eq!(action, Intervention::Quiet);

        // **但别的公司不受影响。**
        // 反过来也说明默认取 Sender 是有道理的：想扩大范围要显式说。
        assert!(deterministic_triage(&item("a@other.com", "s"), &policy).is_none());
    }
}
