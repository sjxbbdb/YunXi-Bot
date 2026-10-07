//! 助理巡览：**取信息 → 判断 → 通知 → 落台账**，一轮走完。
//!
//! ## 为什么这份逻辑在 core 里，而不是写在 CLI 里
//!
//! 因为它有**两个调用方**：
//!
//! - `yunxi-bot check` —— 你主动问"现在有什么"
//! - daemon 的每一轮 —— 它自己看，有事才叫你
//!
//! 抄两份的话两边迟早漂移，而漂移的表现最恶心：**手动跑没问题，
//! 常驻跑就出问题**（或者反过来）。那类 bug 极难查，因为你会相信
//! "我刚刚手动跑过是好的"。
//!
//! ## 常驻场景下这一层最要紧的性质
//!
//! **`already_seen` 不是优化，是正确性。**
//!
//! 手动跑一次重复通知是烦一下；daemon 每轮都跑，没有这条记忆就意味着
//! **同一封邮件被反复通知，直到你把它读掉**。所以这里跳过"已经真的
//! 通知过你的"，而[`crate::feedback::seen`]是它的依据。
//!
//! 而"攒着的"（Hold）**不算通知过**——攒着的意思就是"以后再说"。

use crate::EventKind;
use crate::Ledger;
use crate::companion::{CompanionPolicy, Intervention};
use crate::decide::{Decider, DecisionEngine};
use crate::feedback;
use crate::info::InfoSource;
use crate::memory::Situation;
use crate::notify::{Delivery, Notification, Notifier, Urgency};
use crate::triage::{TriagePolicy, triage_item_with_ledger};

/// 一轮巡览的结果。
///
/// 每个计数都分得清——**"没通知"有四种不同的原因，压成一个数字
/// 就再也答不出"为什么这封没告诉我"**。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PassResult {
    /// 信息源里的未读总数（不是本次取到的条数）。
    pub total_unseen: usize,
    /// 本次取到的条数。
    pub fetched: usize,
    /// 取不到的条数。**"没看全"和"没有"是两件事。**
    pub skipped: usize,
    /// 因为已经通知过而跳过的。
    pub already_seen: usize,
    pub notified: usize,
    pub held: usize,
    pub silent: usize,
}

impl PassResult {
    /// 这一轮有没有值得报出来的事。
    ///
    /// 常驻进程每分钟都打一行"无事发生"会淹没真正重要的那行——
    /// 日志刷屏的代价不是难看，是**你会开始不看它**。
    pub fn is_quiet(&self) -> bool {
        self.notified == 0 && self.skipped == 0 && self.fetched == 0
    }

    /// 一行摘要。进日志用。
    pub fn summary(&self) -> String {
        let mut s = format!(
            "未读 {} · 本次 {} · 通知 {} · 攒着 {} · 不提 {}",
            self.total_unseen, self.fetched, self.notified, self.held, self.silent
        );
        if self.already_seen > 0 {
            s.push_str(&format!(" · 已告知过 {}", self.already_seen));
        }
        if self.skipped > 0 {
            s.push_str(&format!(" · 取不到 {}", self.skipped));
        }
        s
    }

    /// 能不能对使用者说"我这一轮看全了"。
    ///
    /// `skipped > 0` 时为假。**一个助理说"没有新邮件"而其实有 3 封没读到，
    /// 比它不说更糟**——你会因此不再自己去看。
    pub fn is_complete(&self) -> bool {
        self.skipped == 0
    }
}

/// 一轮巡览的输入。
pub struct PassInput<'a, D: Decider, N: Notifier + ?Sized> {
    pub source: &'a dyn InfoSource,
    pub engine: &'a mut DecisionEngine<D>,
    /// ?Sized 是为了能直接传 &dyn Notifier——CLI 侧的后端是运行时选的，
    /// 编译期不知道具体类型。
    pub notifier: &'a N,
    pub policy: TriagePolicy,
    pub companion: CompanionPolicy,
    /// 当前情境（今天已经打扰过几次等）。**常驻进程要每轮重算**，
    /// 不然首页那封会把一天里所有通知都放行。
    pub ctx: Situation,
    pub local_hour: u8,
    pub now_ms: u64,
    pub limit: usize,
    /// 演练：判定与台账照走，**只是不真的发通知**。
    pub dry_run: bool,
}

/// 跑一轮：取信息 → 判断 → 通知 → 落台账。
///
/// 返回 `Err` 只在**取不到信息**时——那是"这一轮什么都没看到"，
/// 调用方必须能区分它和"看到了，没有值得说的"。
/// 单条判定出错不会中断整轮（记进台账、继续下一条）。
pub fn run_pass<D: Decider, N: Notifier + ?Sized>(
    ledger: &mut Ledger,
    input: &mut PassInput<'_, D, N>,
) -> Result<PassResult, String> {
    let mut out = PassResult::default();

    // ---- 取 ----
    let batch = match input.source.fetch(input.limit) {
        Ok(b) => b,
        Err(e) => {
            // **取不到要落台账。** 不记的话，"今天没通知"和"今天取不到"
            // 事后看是一模一样的——而这两件事的下一步完全不同。
            let _ = ledger.append(
                EventKind::InfoFetched,
                None,
                serde_json::json!({
                    "source": input.source.name(),
                    "ok": false,
                    "error": e,
                    "at_ms": input.now_ms,
                }),
            );
            return Err(e);
        }
    };

    out.total_unseen = batch.total_unseen;
    out.fetched = batch.items.len();
    out.skipped = batch.skipped;

    ledger
        .append(
            EventKind::InfoFetched,
            None,
            serde_json::json!({
                "source": input.source.name(),
                "ok": true,
                "count": batch.items.len(),
                "total_unseen": batch.total_unseen,
                "skipped": batch.skipped,
                "at_ms": input.now_ms,
            }),
        )
        .map_err(|e| e.to_string())?;

    if batch.items.is_empty() {
        return Ok(out);
    }

    // **从台账重取一次**：上面刚 append 过，而"已经通知过"的依据必须是最新的。
    // 常驻进程里这一条尤其要紧——它每一轮都在同一个 Ledger 上追加。
    let seen = feedback::seen(ledger.events());

    for it in &batch.items {
        // 已经真的通知过你的，不再判第二次。见模块文档：
        // 这在常驻场景下不是优化，是正确性。
        if feedback::already_seen(&seen, it) {
            out.already_seen += 1;
            continue;
        }

        let d = triage_item_with_ledger(
            input.engine,
            it,
            &input.policy,
            &input.companion,
            &input.ctx,
            input.local_hour,
            input.now_ms,
            // **这一轮手里就有台账，所以必须挂上。** 走到这里的每一条都是
            // 确定性规则没判出来的那些——问过模型的那一种。
            // 漏挂的表现和 D133 一样：通知照样发、`InfoTriaged` 照样全，
            // 而"这一步问没问过模型、它是不是弃权"在决策台账里查不出来。
            Some(&mut *ledger),
        );

        // **每条判定都落台账，包括"决定不通知"的那些。**
        ledger
            .append(
                EventKind::InfoTriaged,
                None,
                serde_json::json!({
                    "source": it.source,
                    "id": it.id,
                    "from": it.from_addr,
                    "from_name": it.from_name,
                    "subject": it.subject,
                    "action": format!("{:?}", d.action),
                    "rule": d.rule,
                    "reason": d.reason,
                    "used_model": d.used_model,
                    "degraded": d.degraded,
                    "constrained": d.constrained,
                    "at_ms": input.now_ms,
                }),
            )
            .map_err(|e| e.to_string())?;

        if !d.should_notify() {
            match d.action {
                Intervention::Hold => out.held += 1,
                _ => out.silent += 1,
            }
            continue;
        }

        // 验证码这类有时效，值得标紧急；其余一律普通。
        // **不滥用"紧急"**——一个总在喊紧急的助理会被很快学会忽略。
        let urgency = if d.rule == Some(crate::triage::rules::VERIFICATION_CODE) {
            Urgency::High
        } else {
            Urgency::Normal
        };
        let n = Notification::new(it.display_from(), it.display_subject())
            .with_tag(format!("{}.{}", it.source, it.id))
            .with_urgency(urgency);

        let delivery = if input.dry_run {
            Delivery::Blocked {
                reason: "演练模式：没有真的发通知".to_string(),
            }
        } else {
            input.notifier.notify(&n)
        };

        ledger
            .append(
                EventKind::NoticeSent,
                None,
                serde_json::json!({
                    "channel": input.notifier.name(),
                    "source": it.source,
                    "id": it.id,
                    // **结构化字段要和渲染好的 title/body 一起记。**
                    // 规则要从 from 推，而展示串的格式随时会变。
                    "from": it.from_addr,
                    "from_name": it.from_name,
                    "subject": it.subject,
                    "title": n.title,
                    "body": n.body,
                    "result": delivery.label(),
                    "detail": delivery.detail(),
                    // **"能不能说通知你了"单独记一位。**
                    // 事后统计"通知了多少"时只有它为真的才算数。
                    "confirmed": delivery.can_claim_user_notified(),
                    "dry_run": input.dry_run,
                    "at_ms": input.now_ms,
                }),
            )
            .map_err(|e| e.to_string())?;

        out.notified += 1;
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Event;
    use crate::decide::StubDecider;
    use crate::info::{FetchBatch, InfoItem};
    use crate::notify::Delivery;
    use std::sync::Mutex;

    // ---- 测试替身 ----

    struct FakeSource {
        items: Vec<InfoItem>,
        total: usize,
        skipped: usize,
        fail: Option<String>,
    }

    impl FakeSource {
        fn with(items: Vec<InfoItem>) -> Self {
            Self {
                total: items.len(),
                items,
                skipped: 0,
                fail: None,
            }
        }
    }

    impl InfoSource for FakeSource {
        fn name(&self) -> &'static str {
            "假源"
        }
        fn health(&self) -> Result<(), String> {
            Ok(())
        }
        fn fetch(&self, _limit: usize) -> Result<FetchBatch, String> {
            if let Some(e) = &self.fail {
                return Err(e.clone());
            }
            Ok(FetchBatch {
                items: self.items.clone(),
                total_unseen: self.total,
                skipped: self.skipped,
            })
        }
        fn read(&self, _id: &str) -> Result<String, String> {
            Ok(String::new())
        }
    }

    /// 记录发出去的通知，并能模拟"送不到"。
    struct SpyNotifier {
        sent: Mutex<Vec<String>>,
        verdict: Option<Delivery>,
    }

    impl SpyNotifier {
        fn new() -> Self {
            Self {
                sent: Mutex::new(Vec::new()),
                verdict: None,
            }
        }
        fn sent(&self) -> Vec<String> {
            self.sent.lock().unwrap().clone()
        }
    }

    impl Notifier for SpyNotifier {
        fn name(&self) -> &'static str {
            "间谍"
        }
        fn notify(&self, n: &Notification) -> Delivery {
            self.sent.lock().unwrap().push(n.title.clone());
            self.verdict.clone().unwrap_or(Delivery::Confirmed {
                evidence: "测试".into(),
            })
        }
    }

    fn item(id: &str, addr: &str, subject: &str) -> InfoItem {
        InfoItem {
            source: "mail".into(),
            id: id.into(),
            from_name: String::new(),
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

    fn ctx() -> Situation {
        Situation {
            relationship_stage: "初期".into(),
            minutes_since_last_interaction: 999,
            ..Default::default()
        }
    }

    fn ledger() -> (Ledger, tempdir::Dir) {
        let d = tempdir::Dir::new();
        let l = Ledger::open(d.path().join("ledger.jsonl")).expect("开台账");
        (l, d)
    }

    /// 极简临时目录（避免为一个测试引 tempfile 依赖）。
    mod tempdir {
        use std::path::{Path, PathBuf};
        pub struct Dir(PathBuf);
        impl Dir {
            pub fn new() -> Self {
                let p = std::env::temp_dir().join(format!(
                    "yunxi-assistant-{}-{:?}",
                    std::process::id(),
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_nanos())
                        .unwrap_or(0)
                ));
                std::fs::create_dir_all(&p).unwrap();
                Self(p)
            }
            pub fn path(&self) -> &Path {
                &self.0
            }
        }
        impl Drop for Dir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }

    fn kinds(ledger: &Ledger) -> Vec<EventKind> {
        ledger.events().iter().map(|e: &Event| e.kind).collect()
    }

    // ---- 基本路径 ----

    #[test]
    fn notifies_only_the_ones_that_should_be_notified() {
        let (mut l, _d) = ledger();
        let src = FakeSource::with(vec![
            item("1", "a@x.com", "验证码 1234"),  // Speak（时效）
            item("2", "noreply@x.com", "已发货"), // Hold（机器发件人）
            item("3", "b@spam.com", "促销"),      // Quiet（黑名单）
        ]);
        let n = SpyNotifier::new();
        let mut engine = DecisionEngine::new(
            StubDecider::succeeding(),
            crate::decide::DecisionClass::Interrupt,
        );
        let mut input = PassInput {
            source: &src,
            engine: &mut engine,
            notifier: &n,
            policy: TriagePolicy {
                block_senders: vec!["@spam.com".into()],
                ..Default::default()
            },
            companion: CompanionPolicy::default(),
            ctx: ctx(),
            local_hour: 14,
            now_ms: 1_000,
            limit: 20,
            dry_run: false,
        };
        let r = run_pass(&mut l, &mut input).unwrap();

        assert_eq!(r.notified, 1);
        assert_eq!(r.held, 1);
        assert_eq!(r.silent, 1);
        assert_eq!(n.sent(), vec!["a@x.com".to_string()], "只有验证码该被通知");
    }

    #[test]
    fn hold_does_not_notify() {
        // **攒着的意思就是"以后再说"，不是"现在弹一下"。**
        // 攒着也弹的话，安静时段就白设了。
        let (mut l, _d) = ledger();
        let src = FakeSource::with(vec![item("1", "noreply@x.com", "已发货")]);
        let n = SpyNotifier::new();
        let mut engine = DecisionEngine::new(
            StubDecider::succeeding(),
            crate::decide::DecisionClass::Interrupt,
        );
        let mut input = PassInput {
            source: &src,
            engine: &mut engine,
            notifier: &n,
            policy: TriagePolicy::default(),
            companion: CompanionPolicy::default(),
            ctx: ctx(),
            local_hour: 14,
            now_ms: 1_000,
            limit: 20,
            dry_run: false,
        };
        let r = run_pass(&mut l, &mut input).unwrap();
        assert_eq!(r.held, 1);
        assert!(n.sent().is_empty(), "攒着的不该弹通知");
    }

    // ---- 留痕：这一轮问过模型没有，必须能从台账回答 ----

    /// **`run_pass` 手里就有台账，所以走上模型那一层的每一条都必须留痕。**
    ///
    /// 缺了它，事后只能靠 `InfoTriaged.used_model` 这个布尔反推
    /// "这一步问没问过模型"——而那个布尔回答不了"它答了什么、
    /// 是不是弃权"。弃权那一种在行为上和"模型说 hold"几乎一样。
    #[test]
    fn a_model_judged_item_records_a_decision_in_the_pass_ledger() {
        let (mut l, _d) = ledger();
        let src = FakeSource::with(vec![item("1", "zhangsan@example.com", "下午的会改到三点")]);
        let n = SpyNotifier::new();
        let stub = StubDecider::succeeding().with_choice("triage", "speak");
        let mut engine = DecisionEngine::new(stub, crate::decide::DecisionClass::Interrupt);
        let mut input = PassInput {
            source: &src,
            engine: &mut engine,
            notifier: &n,
            policy: TriagePolicy::default(),
            companion: CompanionPolicy::default(),
            ctx: ctx(),
            local_hour: 14,
            now_ms: 1_000,
            limit: 20,
            dry_run: false,
        };
        run_pass(&mut l, &mut input).unwrap();

        let decided = l
            .events()
            .iter()
            .find(|e| e.kind == EventKind::DecisionDecided)
            .expect("问过模型就必须留下那条决策事件");
        assert_eq!(
            decided.data["class"],
            serde_json::json!("interrupt"),
            "类别要和共用引擎的那一类一致（打扰/通知）: {}",
            decided.data
        );
        assert_eq!(decided.data["degraded"], serde_json::json!(false));
        assert_eq!(decided.data["action"], serde_json::json!("主动开口"));
        assert!(
            decided.span.is_some(),
            "决策事件必须被边界包住，否则 reload 时会被当崩溃残尾丢掉"
        );
    }

    /// 规则判定那一类**一条决策痕都不留**：它没问过模型。
    ///
    /// 这一条同时是降噪的证据：收件箱里绝大多数信件在第一层就定了，
    /// 台账不会因为这一层被灌满。
    #[test]
    fn a_rule_judged_item_records_no_decision() {
        let (mut l, _d) = ledger();
        let src = FakeSource::with(vec![item("1", "b@spam.com", "促销")]);
        let n = SpyNotifier::new();
        let mut engine = DecisionEngine::new(
            StubDecider::succeeding(),
            crate::decide::DecisionClass::Interrupt,
        );
        let mut input = PassInput {
            source: &src,
            engine: &mut engine,
            notifier: &n,
            policy: TriagePolicy {
                block_senders: vec!["@spam.com".into()],
                ..Default::default()
            },
            companion: CompanionPolicy::default(),
            ctx: ctx(),
            local_hour: 14,
            now_ms: 1_000,
            limit: 20,
            dry_run: false,
        };
        run_pass(&mut l, &mut input).unwrap();

        assert!(
            !l.events().iter().any(|e| matches!(
                e.kind,
                EventKind::DecisionAsked | EventKind::DecisionDecided
            )),
            "规则判定没问过模型，写 asked 就是假账: {:?}",
            kinds(&l)
        );
    }

    // ---- 常驻场景的正确性：不重复打扰 ----

    #[test]
    fn a_second_pass_does_not_notify_the_same_item_again() {
        // **这是常驻场景下最要紧的一条。**
        //
        // 手动跑一次重复通知只是烦一下；daemon 每轮都跑，
        // 没有这条记忆就意味着同一封邮件被反复通知，直到你把它读掉。
        let (mut l, _d) = ledger();
        let src = FakeSource::with(vec![item("1", "a@x.com", "验证码 1234")]);
        let n = SpyNotifier::new();
        let mut engine = DecisionEngine::new(
            StubDecider::succeeding(),
            crate::decide::DecisionClass::Interrupt,
        );

        for round in 0..3 {
            let mut input = PassInput {
                source: &src,
                engine: &mut engine,
                notifier: &n,
                policy: TriagePolicy::default(),
                companion: CompanionPolicy::default(),
                ctx: ctx(),
                local_hour: 14,
                now_ms: 1_000,
                limit: 20,
                dry_run: false,
            };
            let r = run_pass(&mut l, &mut input).unwrap();
            if round == 0 {
                assert_eq!(r.notified, 1, "第一轮该通知");
                assert_eq!(r.already_seen, 0);
            } else {
                assert_eq!(r.notified, 0, "第 {} 轮不该再通知", round + 1);
                assert_eq!(r.already_seen, 1);
            }
        }
        assert_eq!(n.sent().len(), 1, "三轮跑下来只该弹一次");
    }

    #[test]
    fn held_items_are_re_judged_every_pass() {
        // **攒着的不能被永久吞掉。**
        // 跳过它们会让攒着的东西永远不再被提及——而它恰恰该被再提一次。
        let (mut l, _d) = ledger();
        let src = FakeSource::with(vec![item("1", "noreply@x.com", "已发货")]);
        let n = SpyNotifier::new();
        let mut engine = DecisionEngine::new(
            StubDecider::succeeding(),
            crate::decide::DecisionClass::Interrupt,
        );

        for _ in 0..2 {
            let mut input = PassInput {
                source: &src,
                engine: &mut engine,
                notifier: &n,
                policy: TriagePolicy::default(),
                companion: CompanionPolicy::default(),
                ctx: ctx(),
                local_hour: 14,
                now_ms: 1_000,
                limit: 20,
                dry_run: false,
            };
            let r = run_pass(&mut l, &mut input).unwrap();
            assert_eq!(r.held, 1, "每一轮都该重新判它");
            assert_eq!(r.already_seen, 0);
        }
    }

    #[test]
    fn dry_run_records_but_does_not_count_as_told() {
        // 演练没到你眼前，下一轮该重新判——
        // 不然跑一次演练就把邮件永久吞了。
        let (mut l, _d) = ledger();
        let src = FakeSource::with(vec![item("1", "a@x.com", "验证码 1234")]);
        let n = SpyNotifier::new();
        let mut engine = DecisionEngine::new(
            StubDecider::succeeding(),
            crate::decide::DecisionClass::Interrupt,
        );

        for _ in 0..2 {
            let mut input = PassInput {
                source: &src,
                engine: &mut engine,
                notifier: &n,
                policy: TriagePolicy::default(),
                companion: CompanionPolicy::default(),
                ctx: ctx(),
                local_hour: 14,
                now_ms: 1_000,
                limit: 20,
                dry_run: true,
            };
            let r = run_pass(&mut l, &mut input).unwrap();
            assert_eq!(r.notified, 1);
            assert_eq!(r.already_seen, 0, "演练的不该算已告知过");
        }
        assert!(n.sent().is_empty(), "演练模式不该真的发通知");
    }

    #[test]
    fn dry_run_notices_report_blocked_delivery() {
        // 台账里要能看出"这条是演练，没真发"
        let (mut l, _d) = ledger();
        let src = FakeSource::with(vec![item("1", "a@x.com", "验证码 1234")]);
        let n = SpyNotifier::new();
        let mut engine = DecisionEngine::new(
            StubDecider::succeeding(),
            crate::decide::DecisionClass::Interrupt,
        );
        let mut input = PassInput {
            source: &src,
            engine: &mut engine,
            notifier: &n,
            policy: TriagePolicy::default(),
            companion: CompanionPolicy::default(),
            ctx: ctx(),
            local_hour: 14,
            now_ms: 1_000,
            limit: 20,
            dry_run: true,
        };
        run_pass(&mut l, &mut input).unwrap();

        let ev = l
            .events()
            .iter()
            .find(|e| e.kind == EventKind::NoticeSent)
            .expect("该有通知事件");
        assert_eq!(ev.data["dry_run"], true);
        assert_eq!(ev.data["confirmed"], false, "演练不该声称送达");
    }

    #[test]
    fn a_denied_delivery_is_not_recorded_as_confirmed() {
        // **投递结果如实上报。** 系统阻止了通知，台账就必须说没送到——
        // 这是"不许虚报送达"在链路上的最后一环。
        let (mut l, _d) = ledger();
        let src = FakeSource::with(vec![item("1", "a@x.com", "验证码 1234")]);
        let n = SpyNotifier {
            sent: Mutex::new(Vec::new()),
            verdict: Some(Delivery::Blocked {
                reason: "应用通知被关闭".into(),
            }),
        };
        let mut engine = DecisionEngine::new(
            StubDecider::succeeding(),
            crate::decide::DecisionClass::Interrupt,
        );
        let mut input = PassInput {
            source: &src,
            engine: &mut engine,
            notifier: &n,
            policy: TriagePolicy::default(),
            companion: CompanionPolicy::default(),
            ctx: ctx(),
            local_hour: 14,
            now_ms: 1_000,
            limit: 20,
            dry_run: false,
        };
        let r = run_pass(&mut l, &mut input).unwrap();
        assert_eq!(r.notified, 1, "尝试过了，所以算一次尝试");

        let ev = l
            .events()
            .iter()
            .find(|e| e.kind == EventKind::NoticeSent)
            .unwrap();
        assert_eq!(ev.data["confirmed"], false, "被阻止的通知不能算送达");
        assert_eq!(ev.data["result"], "被系统阻止");
    }

    // ---- 台账 ----

    #[test]
    fn every_item_gets_a_triage_record_including_the_not_notified_ones() {
        // **"决定不通知"的那些也必须留痕。**
        // 使用者问"为什么这封没告诉我"时，答案必须在台账里。
        let (mut l, _d) = ledger();
        let src = FakeSource::with(vec![
            item("1", "a@x.com", "验证码 1234"),
            item("2", "noreply@x.com", "已发货"),
        ]);
        let n = SpyNotifier::new();
        let mut engine = DecisionEngine::new(
            StubDecider::succeeding(),
            crate::decide::DecisionClass::Interrupt,
        );
        let mut input = PassInput {
            source: &src,
            engine: &mut engine,
            notifier: &n,
            policy: TriagePolicy::default(),
            companion: CompanionPolicy::default(),
            ctx: ctx(),
            local_hour: 14,
            now_ms: 1_000,
            limit: 20,
            dry_run: false,
        };
        run_pass(&mut l, &mut input).unwrap();

        let triaged = l
            .events()
            .iter()
            .filter(|e| e.kind == EventKind::InfoTriaged)
            .count();
        assert_eq!(triaged, 2, "两条都要有判定记录");
        assert!(kinds(&l).contains(&EventKind::InfoFetched));
    }

    #[test]
    fn a_triage_record_carries_a_readable_reason() {
        let (mut l, _d) = ledger();
        let src = FakeSource::with(vec![item("1", "noreply@x.com", "已发货")]);
        let n = SpyNotifier::new();
        let mut engine = DecisionEngine::new(
            StubDecider::succeeding(),
            crate::decide::DecisionClass::Interrupt,
        );
        let mut input = PassInput {
            source: &src,
            engine: &mut engine,
            notifier: &n,
            policy: TriagePolicy::default(),
            companion: CompanionPolicy::default(),
            ctx: ctx(),
            local_hour: 14,
            now_ms: 1_000,
            limit: 20,
            dry_run: false,
        };
        run_pass(&mut l, &mut input).unwrap();
        let ev = l
            .events()
            .iter()
            .find(|e| e.kind == EventKind::InfoTriaged)
            .unwrap();
        assert!(
            ev.data["reason"]
                .as_str()
                .map(|s| !s.is_empty())
                .unwrap_or(false),
            "理由不能是空的"
        );
        assert!(ev.data["from"].as_str().is_some(), "要记发件人");
    }

    #[test]
    fn fetch_failure_records_a_failed_fetch_event() {
        // **不记的话，"今天没通知"和"今天取不到"事后看是一模一样的。**
        let (mut l, _d) = ledger();
        let src = FakeSource {
            items: vec![],
            total: 0,
            skipped: 0,
            fail: Some("连不上 sidecar".into()),
        };
        let n = SpyNotifier::new();
        let mut engine = DecisionEngine::new(
            StubDecider::succeeding(),
            crate::decide::DecisionClass::Interrupt,
        );
        let mut input = PassInput {
            source: &src,
            engine: &mut engine,
            notifier: &n,
            policy: TriagePolicy::default(),
            companion: CompanionPolicy::default(),
            ctx: ctx(),
            local_hour: 14,
            now_ms: 1_000,
            limit: 20,
            dry_run: false,
        };
        let err = run_pass(&mut l, &mut input).unwrap_err();
        assert!(err.contains("连不上"));

        let ev = l
            .events()
            .iter()
            .find(|e| e.kind == EventKind::InfoFetched)
            .expect("取不到也要落台账");
        assert_eq!(ev.data["ok"], false);
        assert!(ev.data["error"].as_str().unwrap().contains("连不上"));
    }

    #[test]
    fn fetch_failure_is_an_error_not_an_empty_result() {
        // **"取不到"不能伪装成"没有"。**
        // 压成空结果会让故障看起来像安静——而安静是使用者最不会去查的状态。
        let (mut l, _d) = ledger();
        let src = FakeSource {
            items: vec![],
            total: 0,
            skipped: 0,
            fail: Some("boom".into()),
        };
        let n = SpyNotifier::new();
        let mut engine = DecisionEngine::new(
            StubDecider::succeeding(),
            crate::decide::DecisionClass::Interrupt,
        );
        let mut input = PassInput {
            source: &src,
            engine: &mut engine,
            notifier: &n,
            policy: TriagePolicy::default(),
            companion: CompanionPolicy::default(),
            ctx: ctx(),
            local_hour: 14,
            now_ms: 1_000,
            limit: 20,
            dry_run: false,
        };
        assert!(run_pass(&mut l, &mut input).is_err());
    }

    // ---- 摘要 ----

    #[test]
    fn partial_reads_are_not_reported_as_complete() {
        // **一个助理说"没有新邮件"而其实有 3 封没读到，比它不说更糟**——
        // 你会因此不再自己去看。
        let r = PassResult {
            total_unseen: 10,
            fetched: 7,
            skipped: 3,
            ..Default::default()
        };
        assert!(!r.is_complete());
        assert!(r.summary().contains("取不到 3"));

        let clean = PassResult {
            total_unseen: 3,
            fetched: 3,
            ..Default::default()
        };
        assert!(clean.is_complete());
    }

    #[test]
    fn a_pass_with_nothing_to_say_is_quiet() {
        // 常驻进程每分钟打一行"无事发生"会淹没真正重要的那行——
        // 日志刷屏的代价不是难看，是你会开始不看它。
        let nothing = PassResult {
            total_unseen: 0,
            ..Default::default()
        };
        assert!(nothing.is_quiet());

        // 有未读但都被攒着：静默（但下了判断，所以不是"无事发生"）
        let held = PassResult {
            total_unseen: 3,
            fetched: 3,
            held: 3,
            ..Default::default()
        };
        assert!(!held.is_quiet());
    }

    #[test]
    fn summary_mentions_every_nonzero_bucket() {
        let r = PassResult {
            total_unseen: 9,
            fetched: 5,
            skipped: 1,
            already_seen: 2,
            notified: 1,
            held: 3,
            silent: 1,
        };
        let s = r.summary();
        for part in [
            "未读 9",
            "本次 5",
            "通知 1",
            "攒着 3",
            "不提 1",
            "已告知过 2",
            "取不到 1",
        ] {
            assert!(s.contains(part), "摘要缺 {part}: {s}");
        }
    }
}
