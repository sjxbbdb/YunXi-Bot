//! 打扰判定：这条信息值不值得**现在**告诉你。
//!
//! ## 这就是最初那条需求
//!
//! 项目立项时的问题原话是「**新邮件要不要通知人工**」。这一层就是它的答案。
//!
//! ## 复用陪伴层的三层，不另造一套
//!
//! [`crate::companion`] 已经把"何时介入"做成了三层：
//!
//! ```text
//! 判断（模型） → 确定性约束（只能往更保守修正） → Speak / Hold / Quiet
//! ```
//!
//! **这一层是同一个问题的另一个触发源**：陪伴层由"关系状态"触发，
//! 这里由"一条外部信息"触发。所以直接复用 [`Intervention`] 那套词汇和
//! [`constraint_ceiling`] 那道约束——两套词汇描述同一件事，迟早会漂移。
//!
//! 唯一不同的是**喂给模型的 state**：那边是关系与记忆，这边是这封邮件本身。
//!
//! ## 三层，顺序不能反
//!
//! ```text
//! 1. 确定性规则（不花钱、不耗时）
//!      黑名单 / 白名单 / 验证码 / 群发 / 机器发件人
//!              ↓ 一条都没命中
//! 2. 本地 Verdict（免费、离线、约 15ms）
//!      问："这封邮件此刻应当如何介入？"
//!              ↓ 它不可用 / 弃权
//! 3. 保守兜底：Hold（**不是 Speak**）
//! ```
//!
//! **顺序不能反**：每条信息都问一次模型，等于为了省钱先花钱——而邮件是
// 一天几十封的东西，反问一次就是几十次调用。
//!
//! ## 一条必须守住的边界
//!
//! 确定性规则放进第一层的理由不是"它们一定对"，而是**"它们对到可以省掉
//! 一次模型调用"**。所以第一层只放**明确的信号**：使用者显式写下的黑白名单、
//! 一眼可辨的验证码、收件人数这种硬事实。
//!
//! 拿不准的一律下沉到第二层。把模糊规则塞进第一层，会得到一个
//! "又快又经常错"的判定器——而错的方向往往是**漏掉该告诉使用者的事**。

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::companion::{CompanionPolicy, ConstraintVerdict, Intervention, constraint_ceiling};
use crate::decide::{Decider, DecisionEngine, DecisionOutcome, Question};
use crate::info::InfoItem;
use crate::memory::Situation;

/// 确定性规则的标识。**进台账**——事后要能说清"这条为什么没告诉你"。
pub mod rules {
    /// 使用者的黑名单命中。
    pub const BLOCKED_SENDER: &str = "blocked_sender";
    /// 使用者的白名单命中。
    pub const ALLOWED_SENDER: &str = "allowed_sender";
    /// 像一次验证码/一次性口令：有时效性，错过就废了。
    pub const VERIFICATION_CODE: &str = "verification_code";
    /// 群发或邮件列表：几乎从来不需要立刻打扰人。
    pub const BULK: &str = "bulk";
    /// 机器发件人（`noreply@` 之类）：一般不急。
    pub const MACHINE_SENDER: &str = "machine_sender";
}

/// 一次打扰判定。
#[derive(Debug, Clone, PartialEq)]
pub struct TriageDecision {
    pub action: Intervention,
    /// 为什么是这个结果。**必须可读、可审计。**
    pub reason: String,
    /// 由哪条确定性规则定的。`None` 表示走了模型或兜底。
    pub rule: Option<&'static str>,
    /// **这次判定花掉一次模型调用了吗。**
    ///
    /// 单独留一个字段而不是让调用方从 `rule.is_none()` 推：
    /// 兜底路径既没有规则也没调模型，两者混在一起就分不清
    /// "省了钱"和"没办成事"。有测试断言确定性路径这里是 `false`。
    pub used_model: bool,
    /// 是否由降级产生。降级必须可见（ADR §7.3 第 1 条）。
    pub degraded: bool,
    /// 模型是否建议过开口（便于观察"模型想说但被约束拦下"）。
    pub model_suggested_speak: bool,
    /// 约束层是否收紧过。收紧必须可见——否则使用者会以为
    /// "它觉得这封不重要"，而其实只是"现在太晚了"。
    pub constrained: bool,
}

impl TriageDecision {
    /// 要不要真的弹一条桌面通知。
    ///
    /// **只有 `Speak` 才是"现在告诉他"。** `Hold` 是攒着——攒着的东西
    /// 不该弹通知，否则和 Speak 没区别，安静时段也就白设了。
    pub fn should_notify(&self) -> bool {
        self.action == Intervention::Speak
    }
}

/// 判定策略。**这些是使用者设定的事实，不交给模型判断。**
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TriagePolicy {
    /// 白名单：这些发件人的邮件一定值得说。支持后缀匹配（`@company.com`）。
    #[serde(default)]
    pub allow_senders: Vec<String>,
    /// 黑名单：这些发件人的邮件不提。**优先级高于白名单。**
    #[serde(default)]
    pub block_senders: Vec<String>,
    /// 收件人超过几个算群发。
    #[serde(default = "default_bulk_threshold")]
    pub bulk_threshold: usize,
}

/// 配置文件里漏写 `bulk_threshold` 时用的值。
///
/// **不能用 `#[derive(Default)]` 的 0**：0 会让"收件人超过 0 个"全部算群发，
/// 于是所有邮件都被攒着，一条通知都发不出去。
fn default_bulk_threshold() -> usize {
    TriagePolicy::default().bulk_threshold
}

impl Default for TriagePolicy {
    fn default() -> Self {
        Self {
            allow_senders: Vec::new(),
            block_senders: Vec::new(),
            // 实测里"本周产品周报"这种有 6 个收件人。3 是"多了一个人不算"和
            // "一看就是群发"之间的分界。
            bulk_threshold: 3,
        }
    }
}

/// 策略配置文件的默认位置（相对数据目录）。
///
/// **和凭证分开**：凭证是秘密，策略是使用者随时想改的偏好。
/// 混在一起会让"我想加个黑名单"变成"我要动 secrets 目录"。
pub const POLICY_FILE: &str = "triage.json";

/// 从数据目录读策略。**文件不存在不是错误**——用默认值。
///
/// 这条区分很重要：第一次跑的时候没有这个文件是正常的，
/// 把它当成错误会让每个新使用者第一眼就看到一条报错。
/// 但**读到了却解析不了是错误**——那说明文件坏了，
/// 静默用默认值会让使用者以为自己写的规则生效了。
pub fn load_policy(home: &std::path::Path) -> Result<TriagePolicy, String> {
    let path = home.join(POLICY_FILE);
    if !path.exists() {
        return Ok(TriagePolicy::default());
    }
    let text =
        std::fs::read_to_string(&path).map_err(|e| format!("读不到 {}: {e}", path.display()))?;
    if text.trim().is_empty() {
        return Ok(TriagePolicy::default());
    }
    serde_json::from_str(&text).map_err(|e| {
        format!(
            "{} 不是合法的策略文件: {e}\n\
             期望形如：{{\"block_senders\":[\"@spam.com\"],\"allow_senders\":[\"@company.com\"]}}",
            path.display()
        )
    })
}

/// 写策略。供"以后别烦我"这类反馈落盘用。
pub fn save_policy(home: &std::path::Path, policy: &TriagePolicy) -> Result<(), String> {
    let path = home.join(POLICY_FILE);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("建不了目录 {}: {e}", dir.display()))?;
    }
    let text = serde_json::to_string_pretty(policy).map_err(|e| e.to_string())?;
    std::fs::write(&path, text).map_err(|e| format!("写不了 {}: {e}", path.display()))
}

/// 判断两个发件人标识是否匹配。**后缀匹配，且必须有边界。**
///
/// `@company.com` 要能命中 `a@company.com`，但**不能**命中
/// `a@evil-company.com`——后者是经典的前缀伪造。
///
/// 这条和 [`crate::tool::boundary_match`] 是同一个道理（规则 `docs.rs`
/// 不该命中 `docs.rs.evil.example`），只是场景不同。
fn sender_matches(pattern: &str, addr: &str, name: &str) -> bool {
    let p = pattern.trim().to_ascii_lowercase();
    if p.is_empty() {
        return false;
    }
    let a = addr.trim().to_ascii_lowercase();
    let n = name.trim().to_ascii_lowercase();

    // 完整地址：必须完全相等
    if p.contains('@') && !p.starts_with('@') {
        return a == p;
    }
    // 域名后缀：@company.com
    if let Some(dom) = p.strip_prefix('@') {
        if dom.is_empty() {
            return false;
        }
        // 取地址里 @ 之后的部分比对，**不用 ends_with 拼字符串**——
        // `a@evil-company.com`.ends_with("company.com") 是真的，
        // 那是伪造。
        return match a.rsplit_once('@') {
            Some((_, host)) => host == dom,
            None => false,
        };
    }
    // 光秃秃的关键词：**在显示名里做子串匹配**。
    //
    // ⚠ 这一档是**可被伪造的**：显示名由发件人自己填，谁都能把自己叫成"老板"。
    // 所以它是三档里最弱的一档，精确的形式（完整地址、`@域名`）应该优先。
    // 这里没有更好的判据——使用者就是想按称呼匹配时，子串是他要的语义。
    n.contains(&p)
}

/// 像一次验证码吗。
///
/// 判据是"关键词 + 一个 4-8 位数字"。**只认关键词会把「验证码使用说明」
/// 这类文档也判成验证码**，而只认数字会把所有含数字的邮件都拉进来。
///
/// 偏向**多认一点**：漏掉一次验证码的代价（使用者登不进去）比多弹一次
/// 通知大得多。
pub fn looks_like_verification_code(subject: &str, preview: &str) -> bool {
    const KEYWORDS: [&str; 10] = [
        "验证码",
        "校验码",
        "动态码",
        "一次性密码",
        "安全码",
        "verification code",
        "verify code",
        "one-time",
        "otp",
        "passcode",
    ];
    let hay_s = subject.to_ascii_lowercase();
    let hay_p = preview.to_ascii_lowercase();
    let has_kw = KEYWORDS
        .iter()
        .any(|k| hay_s.contains(k) || hay_p.contains(k));
    if !has_kw {
        return false;
    }
    // 关键词有了，再要求一个 4-8 位的数字串。纯字母的 OTP 也有，但少；
    // 而且漏掉的代价远小于误报，所以只认数字。
    has_short_digit_run(&hay_s) || has_short_digit_run(&hay_p)
}

/// 找一个长度 4..=8 的连续数字串。
///
/// **上界 8 很重要**：不加的话，一封含长订单号（比如 16 位）的邮件
/// 也会被当成验证码。下界 4 是因为 3 位数太常见（价格、日期）。
fn has_short_digit_run(s: &str) -> bool {
    let mut run = 0usize;
    // **一旦这一段超过 8 位，整段作废。**
    // 只把 run 归零是不够的：16 位订单号归零后剩下的 7 位会被重新算成
    // "短数字串"，于是订单号被当成验证码。这个 bug 是测试抓到的。
    let mut too_long = false;
    for c in s.chars() {
        if c.is_ascii_digit() {
            run += 1;
            if run > 8 {
                too_long = true;
            }
        } else {
            if !too_long && (4..=8).contains(&run) {
                return true;
            }
            run = 0;
            too_long = false;
        }
    }
    !too_long && (4..=8).contains(&run)
}

/// 像机器发的吗。
fn looks_machine_sent(addr: &str) -> bool {
    let a = addr.to_ascii_lowercase();
    let Some((local, _)) = a.rsplit_once('@') else {
        return false;
    };
    const MARKERS: [&str; 9] = [
        "noreply",
        "no-reply",
        "donotreply",
        "do-not-reply",
        "notification",
        "notifications",
        "mailer-daemon",
        "postmaster",
        "bounce",
    ];
    MARKERS.iter().any(|m| {
        local == *m
            || local.starts_with(&format!("{m}-"))
            || local.starts_with(&format!("{m}_"))
            || local.starts_with(&format!("{m}."))
    })
}

/// **第一层：确定性规则。不花钱、不耗时。**
///
/// 返回 `None` 表示"这些规则都定不了"——那是该往下一层走的信号，
/// **不是失败**。
///
/// 规则**按优先级排**，越靠前越硬：
/// 使用者显式写的黑白名单 → 一眼可辨的时效性内容 → 结构性事实。
pub fn deterministic_triage(
    item: &InfoItem,
    policy: &TriagePolicy,
) -> Option<(Intervention, &'static str, String)> {
    let addr = &item.from_addr;
    let name = &item.from_name;

    // 1. 黑名单。**优先级最高**——使用者说"别提这个发件人"就是别提，
    //    哪怕它发的是验证码。
    if let Some(p) = policy
        .block_senders
        .iter()
        .find(|p| sender_matches(p, addr, name))
    {
        return Some((
            Intervention::Quiet,
            rules::BLOCKED_SENDER,
            format!("发件人在黑名单里（匹配 {p}）"),
        ));
    }

    // 2. 白名单。使用者显式说过"这个人的邮件要告诉我"。
    if let Some(p) = policy
        .allow_senders
        .iter()
        .find(|p| sender_matches(p, addr, name))
    {
        return Some((
            Intervention::Speak,
            rules::ALLOWED_SENDER,
            format!("发件人在白名单里（匹配 {p}）"),
        ));
    }

    // 3. 验证码。**这是唯一一条"确定性规则直接判定要打扰你"的非白名单规则**，
    //    理由只有一个：时效性。晚十分钟这条信息就废了。
    if looks_like_verification_code(&item.subject, &item.preview) {
        return Some((
            Intervention::Speak,
            rules::VERIFICATION_CODE,
            "像一次验证码/一次性口令，有时效性".to_string(),
        ));
    }

    // 4. 群发。几乎从来不需要立刻打扰人，但**攒着而不是丢掉**——
    //    万一是全员通知。区分"群发"和"列表邮件"是模型该干的活，不是规则。
    if item.looks_bulk() {
        return Some((
            Intervention::Hold,
            rules::BULK,
            format!("群发（{} 个收件人），先攒着", item.recipient_count),
        ));
    }

    // 5. 机器发件人。一般不急，但也可能有验证码——所以放在验证码规则之后。
    if looks_machine_sent(addr) {
        return Some((
            Intervention::Hold,
            rules::MACHINE_SENDER,
            format!("机器发件人（{addr}），先攒着"),
        ));
    }

    None
}

/// 邮件场景下问模型的问题。
///
/// 和陪伴层的问题集**故意不同**：那边问"此刻该不该开口"（关于关系），
/// 这边问"这封邮件该怎么处置"（关于一条具体信息）。
/// 共用一套问题会让模型拿不到判断所需的证据（ADR §九 第 6 条）。
pub fn triage_questions() -> Vec<Question> {
    vec![
        Question::choice(
            "triage",
            "这一封邮件此刻应当如何处置？",
            &[
                ("speak", "值得现在就告诉使用者，晚了他会不方便"),
                ("hold", "有内容但不必现在说，攒着等他自己看"),
                ("quiet", "与使用者无关或纯噪音，不值得提"),
            ],
        ),
        Question::noul(
            "needs_action",
            "这封邮件是否明确需要使用者本人做一件事（而不只是知会他）？",
        ),
    ]
}

/// 构造喂给模型的 state。
///
/// **只喂判断所需的证据**（ADR §九 第 6 条）：发件人、主题、摘要、时间、
/// 收件人结构。**不喂整封正文**——正文可能几千字，而判定"要不要打扰"
/// 用不上那么多；把它塞进去既费 token 又稀释信号。
fn build_state(item: &InfoItem, now_ms: u64, hour: u8) -> serde_json::Value {
    // 摘要也截一下：sidecar 已经截到 1200 字，但判定用不了那么多
    let preview: String = item.preview.chars().take(400).collect();

    // **时间取不到就是取不到，不编一个数。**
    //
    // sidecar 契约里 `received_at_ms == 0` 表示"日期解不出来"。
    // 把它当成"刚刚"会让读取失败的通知插到队列最前面；填一个很大的数
    // （"很旧"）虽然方向安全，但那是**用一个假数据掩盖缺失**——
    // 模型会以为自己知道年龄。所以这里是 `null`，并另给一个显式标志。
    //
    // 时间戳超前（有些机器的时钟是错的）当作年龄 0，不 panic 也不算负数。
    let age_minutes: Option<u64> = if item.received_at_ms == 0 {
        None
    } else if item.received_at_ms >= now_ms {
        Some(0)
    } else {
        Some((now_ms - item.received_at_ms) / 60_000)
    };

    json!({
        "subject": item.subject,
        "from_name": item.from_name,
        "from_addr": item.from_addr,
        "preview": preview,
        "direct": item.direct,
        "addressed_directly": item.addressed_directly,
        "recipient_count": item.recipient_count,
        "has_attachments": item.has_attachments,
        "received_at_ms": item.received_at_ms,
        // 显式的"这个字段可不可信"，比让模型从 null 猜要好
        "received_at_known": item.received_at_ms > 0,
        // 模型不擅长做时间减法，所以给"多久之前"而不是两个时间戳
        "age_minutes": age_minutes,
        "now_ms": now_ms,
        "local_hour": hour,
    })
}

/// 完整的一次打扰判定。
///
/// `engine` 是决策引擎（带熔断与降级），`ctx` 是当前情境
/// （今天已经打扰过几次、是不是安静时段标记）。
///
/// 这一条是**不留痕**的旧签名，等价于 [`triage_item_with_ledger`]
/// 传一个空口子。
pub fn triage_item<D: Decider>(
    engine: &mut DecisionEngine<D>,
    item: &InfoItem,
    policy: &TriagePolicy,
    companion: &CompanionPolicy,
    ctx: &Situation,
    local_hour: u8,
    now_ms: u64,
) -> TriageDecision {
    triage_item_with_ledger(
        engine, item, policy, companion, ctx, local_hour, now_ms, None,
    )
}

/// 同 [`triage_item`]，但把**决定了这一封命运的那次判断**记进台账。
///
/// ## 为什么这一处要留痕
///
/// 这一层和陪伴层**共用同一个决策引擎**（都是 `Interrupt` 类、降级方向
/// 都是不打扰），但两者的输入完全不同：那边看的是内部状态与记忆，
/// 这边看的是外面进来的一封邮件。它们问的问题也不同
/// （`triage` vs `intervention`）。
///
/// 在此之前这一处只写一条 `InfoTriaged`——它记的是**结论**
/// （action / rule / reason / used_model / degraded），而且写得很全。
/// 但它和陪伴层的决策事件在台账上**长得不一样**：按 `DecisionClass`
/// 翻记录的人只看得到陪伴那一半，于是"通知分流这一步到底有没有问过模型"
/// 只能靠 `used_model` 这个布尔反推——而那个布尔回答不了
/// "它答了什么、是不是弃权"。
///
/// ## 只有走到模型那一层才记
///
/// 确定性规则（黑名单 / 白名单 / 验证码 / 群发 / 机器发件人）在上面就
/// `return` 了，**一个字都没问过模型**。那几条路上记一条 `decision_asked`
/// 就是假账（D128）。所以留痕写在下面那一层里，不是写在函数出口。
///
/// ## 类别是 `Interrupt`，和共用引擎的那一类一致
///
/// 这一问的实质就是"要不要打扰使用者"：模型答 `speak` 就是打扰提醒，
/// 弃权时兜底是 `Hold`（攒着不打扰）——正好是 `Interrupt` 的降级方向
/// fail-closed。借 `Classify`（归档）会把"要不要吵你"记成"归到哪一类"，
/// 而那一类的降级动作是"归入待分类队列"，不是"攒着"。
#[allow(clippy::too_many_arguments)]
pub fn triage_item_with_ledger<D: Decider>(
    engine: &mut DecisionEngine<D>,
    item: &InfoItem,
    policy: &TriagePolicy,
    companion: &CompanionPolicy,
    ctx: &Situation,
    local_hour: u8,
    now_ms: u64,
    decision_sink: crate::decide::DecisionSink<'_>,
) -> TriageDecision {
    // 约束层先算好：**任何路径都要过它**，包括确定性规则。
    // 使用者说"现在别打扰我"时，一条确定性规则也不能绕过去。
    let verdict = constraint_ceiling(ctx, companion, local_hour);

    // ---- 第一层：确定性规则 ----
    if let Some((action, rule, reason)) = deterministic_triage(item, policy) {
        // **这一条不留痕**：规则判定没有问过模型，也永远不会问。
        return apply_ceiling(
            action,
            reason,
            Some(rule),
            false,
            false,
            action == Intervention::Speak,
            &verdict,
        );
    }

    // ---- 第二层：本地决策模型 ----
    let state = build_state(item, now_ms, local_hour);
    let questions = triage_questions();
    let req = crate::decide::DecisionRequest::new(state, questions);
    let outcome = engine.decide(&req);

    let (decision, model, degraded) = match outcome {
        DecisionOutcome::Decided(result) => {
            let model_action = parse_triage(result.choice("triage"));
            let suggested_speak = model_action == Intervention::Speak;
            let mut d = apply_ceiling(
                model_action,
                format!("本地决策模型判断（{}）", result.model),
                None,
                true,
                false,
                suggested_speak,
                &verdict,
            );
            // 置信度也带上：模型"不太确定"这件事本身就是证据
            if let Some(c) = result.answer("triage").and_then(|a| a.confidence()) {
                d.reason.push_str(&format!("，置信度 {c:.2}"));
            }
            let model = result.model;
            (d, Some(model), false)
        }
        DecisionOutcome::Degraded { action, reason, .. } => {
            // **兜底是 Hold 而不是 Speak。** 失败方向朝"不打扰"——
            // 判定器坏掉时，一个安静的助理比一个乱说话的助理好。
            //
            // 但也不是 Quiet：那等于**把信息丢了**。Hold 保住了它，
            // 使用者主动看的时候还在。这是"不打扰"和"不丢失"之间
            // 唯一同时成立的那个选择。
            let d = apply_ceiling(
                Intervention::Hold,
                format!("决策模型不可用（{action}）：{reason}；按保守方向攒着不打扰"),
                None,
                true,
                true,
                false,
                &verdict,
            );
            // 没有模型答过这一问。
            (d, None, true)
        }
    };

    // 留痕写在**决策之后、返回之前**，`action` 用的是过了约束层的最终动作：
    // 台账、`InfoTriaged` 的 `action`、`TriageDecision.action` 三处必须是
    // 同一个值，否则"台账说通知了、实际只是攒着"这种漂就又会发生。
    if let Err(e) = crate::decide::record_decision_optional(
        decision_sink,
        crate::decide::DecisionClass::Interrupt,
        degraded,
        decision.action.label(),
        &decision.reason,
        model.as_deref(),
        &["triage".to_string(), "needs_action".to_string()],
    ) {
        eprintln!("⚠ 通知分流留痕写入失败（不影响判定）: {e}");
    }

    decision
}

/// 把约束层套到一个动作上。**所有路径都必须经过这里。**
#[allow(clippy::too_many_arguments)]
fn apply_ceiling(
    action: Intervention,
    reason: String,
    rule: Option<&'static str>,
    used_model: bool,
    degraded: bool,
    model_suggested_speak: bool,
    verdict: &ConstraintVerdict,
) -> TriageDecision {
    let final_action = action.more_conservative(verdict.ceiling);
    let constrained = final_action != action;
    let reason = if constrained {
        // **收紧必须说出来。** 否则使用者会以为"它觉得这封不重要"，
        // 而其实只是"现在太晚了"——这两种解释的下一步完全不同。
        format!(
            "{}；但被约束收紧为「{}」：{}",
            reason,
            final_action.label(),
            verdict.reason
        )
    } else {
        reason
    };
    TriageDecision {
        action: final_action,
        reason,
        rule,
        used_model,
        degraded,
        model_suggested_speak,
        constrained,
    }
}

/// 把模型的 choice 解析成动作。**认不出的一律按最保守处理。**
fn parse_triage(choice: Option<&str>) -> Intervention {
    match choice {
        Some("speak") => Intervention::Speak,
        Some("hold") => Intervention::Hold,
        // 认不出、缺答案 —— 都按 Quiet（最保守）
        _ => Intervention::Quiet,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decide::StubDecider;

    fn item(from_addr: &str, subject: &str) -> InfoItem {
        InfoItem {
            source: "mail".into(),
            id: "1".into(),
            from_name: String::new(),
            from_addr: from_addr.into(),
            subject: subject.into(),
            preview: String::new(),
            received_at_ms: 0,
            direct: true,
            recipient_count: 1,
            addressed_directly: true,
            has_attachments: false,
        }
    }

    /// 一个"约束层不会拦"的情境。
    ///
    /// **`Situation::default()` 不是这个。** 它的 `minutes_since_last_interaction`
    /// 是 0，而默认策略要求两次打扰间隔至少 120 分钟——于是约束层会把一切都
    /// 压成 Hold。那是对的（约束层本来就该这样），但会让"测模型那一层"的
    /// 测试全部测到约束层上去，看不出想测的东西。
    fn day_ctx() -> Situation {
        Situation {
            relationship_stage: "初期".into(),
            minutes_since_last_interaction: 999,
            ..Default::default()
        }
    }

    // ---- 发件人匹配：边界是安全相关的 ----

    #[test]
    fn full_address_must_match_exactly() {
        assert!(sender_matches("a@x.com", "a@x.com", ""));
        assert!(!sender_matches("a@x.com", "b@x.com", ""));
        assert!(!sender_matches("a@x.com", "a@x.com.evil.com", ""));
    }

    #[test]
    fn domain_suffix_matches_the_host_exactly() {
        assert!(sender_matches("@company.com", "anyone@company.com", ""));
        assert!(sender_matches("@company.com", "ANYONE@COMPANY.COM", ""));
    }

    /// **安全：前缀伪造不能命中。**
    ///
    /// `a@evil-company.com`.ends_with("company.com") 是真的——
    /// 用字符串后缀匹配就放行了。这和工具审批里 `docs.rs` 命中
    /// `docs.rs.evil.example` 是同一个洞。
    #[test]
    fn domain_suffix_does_not_match_a_longer_lookalike_host() {
        assert!(
            !sender_matches("@company.com", "a@evil-company.com", ""),
            "前缀伪造的域名被白名单放行了"
        );
        assert!(!sender_matches(
            "@company.com",
            "a@company.com.evil.com",
            ""
        ));
        assert!(!sender_matches("@company.com", "a@notcompany.com", ""));
    }

    #[test]
    fn bare_keyword_matches_the_display_name_as_a_substring() {
        // ⚠ 这一档**可被伪造**：显示名由发件人自己填。
        // 所以它是三档里最弱的一档——测试要把这个事实写下来，
        // 免得有人以为它和地址匹配一样可靠。
        assert!(sender_matches("老板", "", "王老板"));
        assert!(sender_matches("boss", "", "The Boss"));
        assert!(
            sender_matches("boss", "", "bossman"),
            "子串匹配，这是它的语义"
        );
        assert!(!sender_matches("boss", "", "张三"));
    }

    #[test]
    fn display_name_rules_are_marked_as_spoofable() {
        // 这条不是测行为，是**把设计判断钉在测试里**：
        // 一个只按显示名匹配的白名单，任何人在显示名里写上那个词就能过。
        // 所以文档和注释里必须说清楚"精确形式优先"。
        // 如果将来有人把这一档改成"安全匹配"，他会先看到这条注释。
        assert!(sender_matches("财务部", "", "财务部"));
        // 地址匹配则不受显示名影响
        assert!(!sender_matches("a@real.com", "a@fake.com", "财务部"));
    }

    #[test]
    fn empty_pattern_matches_nothing() {
        // 空串匹配一切是灾难性的：一条空规则会让所有邮件都走同一条路
        assert!(!sender_matches("", "a@x.com", "someone"));
        assert!(!sender_matches("@", "a@x.com", ""));
    }

    // ---- 验证码识别 ----

    #[test]
    fn detects_a_verification_code() {
        assert!(looks_like_verification_code("验证码 123456", ""));
        assert!(looks_like_verification_code("", "您的验证码是 8899"));
        assert!(looks_like_verification_code(
            "Your verification code",
            "1234"
        ));
        assert!(looks_like_verification_code("OTP", "passcode: 5566"));
    }

    #[test]
    fn requires_both_a_keyword_and_a_code() {
        // 只认关键词会把"验证码使用说明"也判进来；
        // 只认数字会把所有含数字的邮件都拉进来
        assert!(!looks_like_verification_code("验证码使用说明", "请勿泄露"));
        assert!(!looks_like_verification_code("订单已发货", "单号 123456"));
    }

    #[test]
    fn long_digit_runs_are_not_codes() {
        // 不加 8 位上界的话，含 16 位订单号的邮件会被当成验证码
        assert!(!looks_like_verification_code(
            "验证码",
            "订单号 1234567890123456"
        ));
    }

    #[test]
    fn short_digit_runs_are_not_codes() {
        // 3 位数太常见：价格、日期、件数
        assert!(!looks_like_verification_code("验证码", "共 123 元"));
    }

    #[test]
    fn digit_run_at_the_very_end_is_found() {
        // 边界处理：数字串在字符串末尾时不能被漏掉
        assert!(has_short_digit_run("code 1234"));
        assert!(has_short_digit_run("1234"));
        assert!(!has_short_digit_run("code 12"));
    }

    // ---- 机器发件人 ----

    #[test]
    fn detects_machine_senders() {
        assert!(looks_machine_sent("noreply@x.com"));
        assert!(looks_machine_sent("no-reply@x.com"));
        assert!(looks_machine_sent("notifications@x.com"));
        assert!(looks_machine_sent("noreply-2@x.com"));
    }

    #[test]
    fn does_not_flag_a_human() {
        assert!(!looks_machine_sent("zhangsan@x.com"));
        // `noreplyish` 是个正常用户名，不是机器发件人
        assert!(!looks_machine_sent("noreplyish@x.com"));
    }

    // ---- 第一层：确定性规则 ----

    #[test]
    fn blocked_sender_wins_over_everything() {
        // 使用者说"别提这个发件人"就是别提，哪怕它发的是验证码
        let p = TriagePolicy {
            block_senders: vec!["@spam.com".into()],
            ..Default::default()
        };
        let mut it = item("x@spam.com", "验证码 1234");
        it.preview = "您的验证码是 1234".into();
        let (a, rule, _) = deterministic_triage(&it, &p).expect("黑名单应命中");
        assert_eq!(a, Intervention::Quiet);
        assert_eq!(rule, rules::BLOCKED_SENDER);
    }

    #[test]
    fn verification_code_beats_bulk_and_machine() {
        // 时效性最强：验证码从群发地址发出来也值得现在说
        let it = {
            let mut i = item("noreply@list.com", "验证码 5566");
            i.preview = "您的验证码是 5566".into();
            i.direct = false;
            i.recipient_count = 99;
            i
        };
        let (a, rule, _) = deterministic_triage(&it, &TriagePolicy::default()).unwrap();
        assert_eq!(a, Intervention::Speak);
        assert_eq!(rule, rules::VERIFICATION_CODE);
    }

    #[test]
    fn bulk_is_held_not_dropped() {
        // **攒着而不是丢掉**：万一是全员通知。
        // 区分"群发"和"列表邮件"是模型该干的活，不是规则。
        let mut it = item("a@x.com", "本周周报");
        it.direct = false;
        it.recipient_count = 20;
        let (a, rule, _) = deterministic_triage(&it, &TriagePolicy::default()).unwrap();
        assert_eq!(a, Intervention::Hold, "群发该攒着而不是静默丢弃");
        assert_eq!(rule, rules::BULK);
    }

    #[test]
    fn machine_sender_is_held() {
        let it = item("noreply@x.com", "您的订单已发货");
        let (a, rule, _) = deterministic_triage(&it, &TriagePolicy::default()).unwrap();
        assert_eq!(a, Intervention::Hold);
        assert_eq!(rule, rules::MACHINE_SENDER);
    }

    #[test]
    fn an_ordinary_personal_email_hits_no_rule() {
        // **这条测试是"省钱"的核心**：普通私人邮件必须落不到确定性规则上，
        // 才能说明第一层不是"什么都管"。
        let it = item("zhangsan@example.com", "下午的会改到三点");
        assert!(deterministic_triage(&it, &TriagePolicy::default()).is_none());
    }

    #[test]
    fn every_rule_has_a_stable_identifier() {
        // 规则标识要进台账——事后得能说清"这条为什么没告诉你"
        let ids = [
            rules::BLOCKED_SENDER,
            rules::ALLOWED_SENDER,
            rules::VERIFICATION_CODE,
            rules::BULK,
            rules::MACHINE_SENDER,
        ];
        let uniq: std::collections::BTreeSet<_> = ids.iter().collect();
        assert_eq!(uniq.len(), ids.len(), "规则标识不能重复");
    }

    // ---- 第二层与第三层 ----

    #[test]
    fn deterministic_path_never_calls_the_model() {
        // **这是整个模块最重要的一条断言。**
        // 邮件一天几十封，每封都问一次模型，等于为了省钱先花钱。
        let mut engine = crate::decide::DecisionEngine::new(
            StubDecider::succeeding().with_choice("triage", "speak"),
            crate::decide::DecisionClass::Interrupt,
        );

        // 白名单那一条只在配了名单时才命中，所以黑名单版本单独测一次
        let plain = TriagePolicy::default();
        let configured = TriagePolicy {
            allow_senders: vec!["@friend.com".into()],
            block_senders: vec!["@spam.com".into()],
            ..Default::default()
        };

        let cases = [
            // 黑名单（只有配了才命中）
            (item("x@spam.com", "s"), &configured),
            // 白名单（只有配了才命中）
            (item("a@friend.com", "s"), &configured),
            // 验证码（默认策略就命中）
            (item("a@x.com", "验证码 1234"), &plain),
            // 群发
            (
                {
                    let mut i = item("a@x.com", "周报");
                    i.direct = false;
                    i.recipient_count = 30;
                    i
                },
                &plain,
            ),
            // 机器发件人
            (item("noreply@x.com", "已发货"), &plain),
        ];

        for (it, policy) in &cases {
            let d = triage_item(
                &mut engine,
                it,
                policy,
                &CompanionPolicy::default(),
                &day_ctx(),
                14,
                1_000,
            );
            assert!(!d.used_model, "确定性规则能定却调了模型: {it:?}");
            assert!(
                d.rule.is_some(),
                "确定性路径必须留下规则标识（事后要说清为什么）: {it:?}"
            );
        }
    }

    #[test]
    fn ambiguous_falls_through_to_the_model() {
        let stub = StubDecider::succeeding().with_choice("triage", "speak");
        let mut engine =
            crate::decide::DecisionEngine::new(stub, crate::decide::DecisionClass::Interrupt);
        let it = item("zhangsan@example.com", "下午的会改到三点");
        let d = triage_item(
            &mut engine,
            &it,
            &TriagePolicy::default(),
            &CompanionPolicy::default(),
            &day_ctx(),
            14,
            1_000,
        );
        assert!(d.used_model, "拿不准就该问模型");
        assert_eq!(d.action, Intervention::Speak);
        assert!(d.rule.is_none());
    }

    #[test]
    fn decider_failure_falls_back_to_hold_not_speak() {
        // **兜底是 Hold 而不是 Speak。**
        // 判定器坏掉时，一个安静的助理比一个乱说话的助理好。
        // 但也不是 Quiet——那等于把信息丢了。
        let stub = StubDecider::failing("sidecar 没起来");
        let mut engine =
            crate::decide::DecisionEngine::new(stub, crate::decide::DecisionClass::Interrupt);
        let it = item("zhangsan@example.com", "下午的会改到三点");
        let d = triage_item(
            &mut engine,
            &it,
            &TriagePolicy::default(),
            &CompanionPolicy::default(),
            &day_ctx(),
            14,
            1_000,
        );
        assert_eq!(d.action, Intervention::Hold, "兜底必须朝不打扰");
        assert!(d.degraded, "降级必须可见");
        assert!(!d.should_notify(), "降级时不该弹通知");
    }

    #[test]
    fn an_undeclared_model_choice_is_rejected_and_held() {
        // 模型编了个没声明的选项 → `laya::validate` 拦下 → 降级。
        //
        // **这个结果比"悄悄当成 Quiet"好：降级是可见的**，会被记进台账。
        // 悄悄变 Quiet 会让"模型坏了"伪装成"今天很安静"。
        let stub = StubDecider::succeeding().with_choice("triage", "nonsense");
        let mut engine =
            crate::decide::DecisionEngine::new(stub, crate::decide::DecisionClass::Interrupt);
        let it = item("zhangsan@example.com", "s");
        let d = triage_item(
            &mut engine,
            &it,
            &TriagePolicy::default(),
            &CompanionPolicy::default(),
            &day_ctx(),
            14,
            1_000,
        );
        assert!(!d.should_notify(), "编出来的选项绝不该导致打扰");
        assert_eq!(d.action, Intervention::Hold);
        assert!(d.degraded, "模型乱答必须可见");
        // 理由要能让人看懂"为什么这条没告诉我"
        assert!(d.reason.contains("非法"), "{}", d.reason);
        assert!(d.reason.contains("攒着"), "{}", d.reason);
    }

    #[test]
    fn a_hold_answer_holds() {
        // 模型选了 hold：**攒着而不是丢掉。**
        //
        // 顺带记一个容易踩的点：`Question::choice` 的选项是**按键排序**存的，
        // 所以测试桩"没配置时取第一个选项"取到的是字母序第一个（hold），
        // 不是声明顺序里的第一个（speak）。想测 speak 必须显式配——
        // 上面那几条用了 `.with_choice("triage", "speak")` 就是为这个。
        let stub = StubDecider::succeeding().with_choice("triage", "hold");
        let mut engine =
            crate::decide::DecisionEngine::new(stub, crate::decide::DecisionClass::Interrupt);
        let it = item("zhangsan@example.com", "s");
        let d = triage_item(
            &mut engine,
            &it,
            &TriagePolicy::default(),
            &CompanionPolicy::default(),
            &day_ctx(),
            14,
            1_000,
        );
        assert_eq!(d.action, Intervention::Hold);
        assert!(!d.degraded, "这是正常判断，不是降级");
        assert!(!d.should_notify(), "hold 不该弹通知");
    }

    #[test]
    fn parse_triage_is_defensive_because_the_engine_already_guards() {
        // 引擎的校验拦在前面，所以经 `triage_item` 走不到这些分支。
        // 保留它们是因为**这一层不该依赖上游的校验一直存在**——
        // 换一个引擎实现、或者校验被放宽，这里仍要 fail-closed。
        assert_eq!(parse_triage(Some("speak")), Intervention::Speak);
        assert_eq!(parse_triage(Some("hold")), Intervention::Hold);
        assert_eq!(parse_triage(Some("quiet")), Intervention::Quiet);
        // 认不出的一律最保守
        assert_eq!(parse_triage(None), Intervention::Quiet);
        assert_eq!(parse_triage(Some("SPEAK")), Intervention::Quiet);
        assert_eq!(parse_triage(Some("")), Intervention::Quiet);
    }

    // ---- 约束层：所有路径都要过 ----

    #[test]
    fn quiet_hours_suppress_even_a_verification_code() {
        // **使用者说"现在别打扰我"时，确定性规则也不能绕过去。**
        let stub = StubDecider::succeeding();
        let mut engine =
            crate::decide::DecisionEngine::new(stub, crate::decide::DecisionClass::Interrupt);
        let it = {
            let mut i = item("bank@x.com", "验证码 1234");
            i.preview = "您的验证码是 1234".into();
            i
        };
        // 凌晨 3 点，落在默认安静时段 23..8 里
        let d = triage_item(
            &mut engine,
            &it,
            &TriagePolicy::default(),
            &CompanionPolicy::default(),
            &day_ctx(),
            3,
            1_000,
        );
        assert_eq!(d.action, Intervention::Hold, "安静时段该把 Speak 压成 Hold");
        assert!(d.constrained, "收紧必须可见");
        assert!(!d.should_notify());
        // 但**信息没丢**，而且理由里说得清为什么
        assert!(d.reason.contains("安静时段"), "{}", d.reason);
        assert!(d.reason.contains("验证码"), "原判定要保留: {}", d.reason);
    }

    #[test]
    fn daily_cap_suppresses_notifications() {
        // 模型说"该告诉他"，但今天已经打扰够多次了——约束层要压住。
        let stub = StubDecider::succeeding().with_choice("triage", "speak");
        let mut engine =
            crate::decide::DecisionEngine::new(stub, crate::decide::DecisionClass::Interrupt);
        let policy = CompanionPolicy {
            max_interventions_per_day: 3,
            ..Default::default()
        };
        let ctx = Situation {
            interventions_today: 3,
            ..day_ctx()
        };
        let it = item("zhangsan@example.com", "紧急：下午的会");
        let d = triage_item(
            &mut engine,
            &it,
            &TriagePolicy::default(),
            &policy,
            &ctx,
            14,
            1_000,
        );
        assert!(!d.should_notify());
        assert!(d.constrained);
        assert!(d.reason.contains("上限"), "{}", d.reason);
        assert!(d.model_suggested_speak, "模型确实想说，是被约束拦下的");
    }

    #[test]
    fn min_interval_between_interventions_is_enforced() {
        // 约束层的另一道闸：两次打扰之间要有间隔。
        // 连着的两封邮件不该各弹一条通知。
        let stub = StubDecider::succeeding().with_choice("triage", "speak");
        let mut engine =
            crate::decide::DecisionEngine::new(stub, crate::decide::DecisionClass::Interrupt);
        let ctx = Situation {
            minutes_since_last_interaction: 5,
            ..day_ctx()
        };
        let it = item("zhangsan@example.com", "下午的会");
        let d = triage_item(
            &mut engine,
            &it,
            &TriagePolicy::default(),
            &CompanionPolicy::default(),
            &ctx,
            14,
            1_000,
        );
        assert!(!d.should_notify(), "刚打扰过就不该再弹");
        assert!(d.reason.contains("间隔"), "{}", d.reason);
    }

    #[test]
    fn constraint_can_only_make_it_more_conservative() {
        // 约束层的天花板是 Hold 时，Quiet 不该被"放松"成 Hold
        let verdict = ConstraintVerdict {
            ceiling: Intervention::Hold,
            reason: "测试".into(),
        };
        let d = apply_ceiling(
            Intervention::Quiet,
            "原判定".into(),
            None,
            false,
            false,
            false,
            &verdict,
        );
        assert_eq!(d.action, Intervention::Quiet, "约束不能放宽动作");
        assert!(!d.constrained, "没变化就不该标成收紧过");
    }

    #[test]
    fn should_notify_only_for_speak() {
        // **Hold 是攒着。** 攒着的东西不该弹通知，否则和 Speak 没区别，
        // 安静时段也就白设了。
        let mk = |a| TriageDecision {
            action: a,
            reason: String::new(),
            rule: None,
            used_model: false,
            degraded: false,
            model_suggested_speak: false,
            constrained: false,
        };
        assert!(mk(Intervention::Speak).should_notify());
        assert!(!mk(Intervention::Hold).should_notify());
        assert!(!mk(Intervention::Quiet).should_notify());
    }

    #[test]
    fn model_speaking_but_constrained_is_observable() {
        // "模型想说但被拦下"要能被观察到——否则调阈值时没有依据
        let stub = StubDecider::succeeding().with_choice("triage", "speak");
        let mut engine =
            crate::decide::DecisionEngine::new(stub, crate::decide::DecisionClass::Interrupt);
        let it = item("zhangsan@example.com", "s");
        let d = triage_item(
            &mut engine,
            &it,
            &TriagePolicy::default(),
            &CompanionPolicy::default(),
            &day_ctx(),
            3,
            1_000,
        );
        assert!(d.model_suggested_speak, "模型确实建议过开口");
        assert_eq!(d.action, Intervention::Hold, "但被安静时段压住了");
        assert!(d.constrained);
    }

    // ---- 留痕：分流这一步"问过模型没有、它是不是弃权"必须查得出来 ----

    /// 一个临时台账。文件名带 tag，避免并行跑的测试互相踩。
    fn tmp_ledger(tag: &str) -> (std::path::PathBuf, crate::ledger::Ledger) {
        let p =
            std::env::temp_dir().join(format!("yunxi-triage-{}-{tag}.jsonl", std::process::id()));
        let _ = std::fs::remove_file(&p);
        let l = crate::ledger::Ledger::open(&p).expect("开台账");
        (p, l)
    }

    fn decided_event(ledger: &crate::ledger::Ledger) -> &crate::ledger::Event {
        ledger
            .events()
            .iter()
            .find(|e| e.kind == crate::ledger::EventKind::DecisionDecided)
            .expect("应有一条 decision_decided")
    }

    /// **模型判过了 → 记一条 `Interrupt`。**
    ///
    /// `InfoTriaged` 记的是结论（而且记得很全），但它回答不了
    /// "这一步到底有没有问过决策模型"——那正是按 `DecisionClass`
    /// 翻台账的人要问的第一个问题。
    #[test]
    fn a_model_triage_judgment_is_recorded() {
        let (path, mut ledger) = tmp_ledger("judged");
        let stub = StubDecider::succeeding().with_choice("triage", "speak");
        let mut engine =
            crate::decide::DecisionEngine::new(stub, crate::decide::DecisionClass::Interrupt);
        let it = item("zhangsan@example.com", "下午的会改到三点");
        let d = triage_item_with_ledger(
            &mut engine,
            &it,
            &TriagePolicy::default(),
            &CompanionPolicy::default(),
            &day_ctx(),
            14,
            1_000,
            Some(&mut ledger),
        );
        assert!(d.used_model, "留痕不该改变判定");

        let decided = decided_event(&ledger);
        assert_eq!(
            decided.data["class"],
            serde_json::json!("interrupt"),
            "这一问的实质是'要不要打扰使用者'——和共用引擎的那一类一致: {}",
            decided.data
        );
        assert_eq!(
            decided.data["degraded"],
            serde_json::json!(false),
            "模型答了不是降级: {}",
            decided.data
        );
        assert_eq!(
            decided.data["action"],
            serde_json::json!(Intervention::Speak.label()),
            "台账里的动作必须和 `InfoTriaged`/实际通知是同一个值: {}",
            decided.data
        );
        assert_eq!(decided.data["model"], serde_json::json!("stub"));
        let reason = decided.data["reason"].as_str().unwrap_or("");
        assert!(reason.contains("本地决策模型"), "{reason}");

        let asked = ledger
            .events()
            .iter()
            .find(|e| e.kind == crate::ledger::EventKind::DecisionAsked)
            .expect("应有一条 decision_asked");
        assert_eq!(
            asked.data["questions"],
            serde_json::json!(["triage", "needs_action"]),
            "问句 id 要进台账：它同时是'哪个环节问的'的唯一标识: {}",
            asked.data
        );
        assert_eq!(asked.span, decided.span, "审计对要落在同一个边界里");
        let _ = std::fs::remove_file(&path);
    }

    /// **弃权/故障也要记，而且是降级。**
    ///
    /// 这一条在外部行为上和"模型答了 hold"几乎一样（都攒着不打扰），
    /// 在台账上必须分得开：前者是判断，后者是"这一层根本没工作"。
    #[test]
    fn a_degraded_triage_judgment_says_so_in_the_ledger() {
        let (path, mut ledger) = tmp_ledger("degraded");
        let stub = StubDecider::failing("sidecar 没起来");
        let mut engine =
            crate::decide::DecisionEngine::new(stub, crate::decide::DecisionClass::Interrupt);
        let it = item("zhangsan@example.com", "下午的会改到三点");
        let d = triage_item_with_ledger(
            &mut engine,
            &it,
            &TriagePolicy::default(),
            &CompanionPolicy::default(),
            &day_ctx(),
            14,
            1_000,
            Some(&mut ledger),
        );
        assert!(d.degraded);

        let decided = decided_event(&ledger);
        assert_eq!(decided.data["class"], serde_json::json!("interrupt"));
        assert_eq!(
            decided.data["degraded"],
            serde_json::json!(true),
            "模型不可用必须如实记成降级: {}",
            decided.data
        );
        assert_eq!(
            decided.data["model"],
            serde_json::json!(null),
            "没有模型答过这一问: {}",
            decided.data
        );
        let reason = decided.data["reason"].as_str().unwrap_or("");
        assert!(
            reason.contains("sidecar 没起来"),
            "故障原文要留下——'判定器坏多久了'只能靠它回答: {reason}"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// **确定性规则判出来的那一条不留痕**：它没有问过模型，也永远不会问。
    ///
    /// 这也是这一处的降噪闸门：收件箱里绝大多数信件（验证码、群发、
    /// 机器发件人、黑名单）在第一层就定了，一条决策事件都不会写。
    #[test]
    fn a_rule_judged_item_leaves_no_decision_trace() {
        let (path, mut ledger) = tmp_ledger("rule");
        // 黑名单直接判 Quiet，走不到模型那一层。
        let stub = StubDecider::succeeding().with_choice("triage", "speak");
        let mut engine =
            crate::decide::DecisionEngine::new(stub, crate::decide::DecisionClass::Interrupt);
        let it = item("b@spam.com", "促销");
        let policy = TriagePolicy {
            block_senders: vec!["@spam.com".into()],
            ..Default::default()
        };
        let d = triage_item_with_ledger(
            &mut engine,
            &it,
            &policy,
            &CompanionPolicy::default(),
            &day_ctx(),
            14,
            1_000,
            Some(&mut ledger),
        );
        assert!(!d.used_model, "这一条该由规则判定");
        assert!(
            ledger.events().is_empty(),
            "规则判定没问过模型，写 asked 就是假账: {:?}",
            ledger.events()
        );
        let _ = std::fs::remove_file(&path);
    }

    /// **不挂台账 = 与从前逐字节相同。**
    #[test]
    fn triage_without_a_ledger_keeps_todays_behaviour() {
        let (path, ledger) = tmp_ledger("no-sink");
        let stub = StubDecider::succeeding().with_choice("triage", "speak");
        let mut engine =
            crate::decide::DecisionEngine::new(stub, crate::decide::DecisionClass::Interrupt);
        let it = item("zhangsan@example.com", "下午的会改到三点");
        let without = triage_item(
            &mut engine,
            &it,
            &TriagePolicy::default(),
            &CompanionPolicy::default(),
            &day_ctx(),
            14,
            1_000,
        );
        assert_eq!(without.action, Intervention::Speak);
        assert_eq!(
            without,
            triage_item_with_ledger(
                &mut engine,
                &it,
                &TriagePolicy::default(),
                &CompanionPolicy::default(),
                &day_ctx(),
                14,
                1_000,
                None,
            ),
            "空留痕口不该改变任何一项判定"
        );
        assert!(
            ledger.events().is_empty(),
            "没挂台账却写了东西: {:?}",
            ledger.events()
        );
        let _ = std::fs::remove_file(&path);
    }

    // ---- state 构造 ----

    #[test]
    fn state_carries_the_evidence_but_not_the_whole_body() {
        let mut it = item("a@x.com", "主题");
        it.preview = "长".repeat(1000);
        let s = build_state(&it, 2_000_000, 14);
        assert_eq!(s["subject"], "主题");
        assert_eq!(s["from_addr"], "a@x.com");
        let preview = s["preview"].as_str().unwrap();
        assert!(
            preview.chars().count() <= 400,
            "摘要是给人判断用的，不是正文备份：{} 字",
            preview.chars().count()
        );
    }

    #[test]
    fn state_computes_age_instead_of_making_the_model_do_subtraction() {
        // 模型不擅长做时间减法。给它"多久之前"比给它两个时间戳好。
        let mut it = item("a@x.com", "s");
        it.received_at_ms = 1_000_000;
        let s = build_state(&it, 1_000_000 + 5 * 60_000, 14);
        assert_eq!(s["age_minutes"], 5);
        assert_eq!(s["received_at_known"], true);
    }

    #[test]
    fn state_handles_a_future_timestamp_without_panicking() {
        // 有些机器发的邮件时间戳是错的（超前）。别因此 panic，
        // 也别算出个负数让模型困惑。
        let mut it = item("a@x.com", "s");
        it.received_at_ms = 9_999_999_999;
        let s = build_state(&it, 1_000, 14);
        assert_eq!(s["age_minutes"], 0);
    }

    #[test]
    fn unknown_timestamp_is_null_not_a_made_up_number() {
        // **不编一个数。**
        //
        // `received_at_ms == 0` 是 sidecar 契约里的"日期解不出来"。
        // 当成"刚刚"会让读取失败的通知插到队列最前面；填一个很大的数
        // （"很旧"）虽然方向安全，但那是用一个假数据掩盖缺失——
        // 模型会以为自己知道年龄。
        let it = item("a@x.com", "s"); // received_at_ms = 0
        let s = build_state(&it, 1_000_000_000, 14);
        assert!(s["age_minutes"].is_null(), "取不到时间就该是 null");
        assert_eq!(
            s["received_at_known"], false,
            "要显式告诉模型这个字段不可信"
        );
        assert_eq!(s["received_at_ms"], 0);
    }

    // ---- 策略文件 ----

    fn tmp_home(tag: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!(
            "yunxi-triage-{tag}-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn missing_policy_file_is_default_not_an_error() {
        // **第一次跑的时候没有这个文件是正常的。**
        // 把它当成错误会让每个新使用者第一眼就看到一条报错。
        let home = tmp_home("missing");
        let p = load_policy(&home).expect("没有文件不该是错误");
        assert_eq!(p, TriagePolicy::default());
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn empty_policy_file_is_also_default() {
        let home = tmp_home("empty");
        std::fs::write(home.join(POLICY_FILE), "   \n").unwrap();
        assert_eq!(load_policy(&home).unwrap(), TriagePolicy::default());
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn broken_policy_file_is_an_error_not_silently_default() {
        // **读到了却解析不了是错误。**
        // 静默用默认值会让使用者以为自己写的规则生效了——
        // 于是他等一个永远不会来的通知。
        let home = tmp_home("broken");
        std::fs::write(home.join(POLICY_FILE), "{不是 json").unwrap();
        let e = load_policy(&home).unwrap_err();
        assert!(e.contains("不是合法"), "{e}");
        // 报错要给出期望的形状，不然使用者不知道该怎么改
        assert!(e.contains("block_senders"), "{e}");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn policy_round_trips_through_the_file() {
        let home = tmp_home("roundtrip");
        let policy = TriagePolicy {
            allow_senders: vec!["@company.com".into()],
            block_senders: vec!["@spam.com".into()],
            bulk_threshold: 5,
        };
        save_policy(&home, &policy).unwrap();
        assert_eq!(load_policy(&home).unwrap(), policy);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn a_partial_policy_file_fills_in_the_defaults() {
        // 使用者只写想改的那几项，不该被迫写全
        let home = tmp_home("partial");
        std::fs::write(home.join(POLICY_FILE), r#"{"block_senders":["@spam.com"]}"#).unwrap();
        let p = load_policy(&home).unwrap();
        assert_eq!(p.block_senders, vec!["@spam.com".to_string()]);
        assert!(p.allow_senders.is_empty());
        // **群发阈值必须回到 3 而不是 0。**
        // 0 会让"收件人超过 0 个"全部算群发，于是所有邮件都被攒着，
        // 一条通知都发不出去。
        assert_eq!(p.bulk_threshold, 3, "漏写阈值不能变成 0");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn the_loaded_policy_actually_changes_a_decision() {
        // 端到端一点：配置真的会改变判定，而不是只被读进来
        let home = tmp_home("effect");
        save_policy(
            &home,
            &TriagePolicy {
                block_senders: vec!["@spam.com".into()],
                ..Default::default()
            },
        )
        .unwrap();
        let policy = load_policy(&home).unwrap();
        let it = item("ads@spam.com", "限时优惠");
        let (a, rule, _) = deterministic_triage(&it, &policy).expect("黑名单该命中");
        assert_eq!(a, Intervention::Quiet);
        assert_eq!(rule, rules::BLOCKED_SENDER);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn every_decision_has_a_reason() {
        // 理由要进台账。没有理由的判定事后无法解释。
        let stub = StubDecider::succeeding().with_choice("triage", "hold");
        let mut engine =
            crate::decide::DecisionEngine::new(stub, crate::decide::DecisionClass::Interrupt);
        for it in [
            item("a@x.com", "验证码 1234"),
            item("noreply@x.com", "s"),
            item("zhangsan@example.com", "s"),
        ] {
            let d = triage_item(
                &mut engine,
                &it,
                &TriagePolicy::default(),
                &CompanionPolicy::default(),
                &day_ctx(),
                14,
                1_000,
            );
            assert!(!d.reason.is_empty(), "{it:?}");
        }
    }
}
