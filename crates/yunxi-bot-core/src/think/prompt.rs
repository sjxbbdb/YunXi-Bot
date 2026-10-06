//! 缓存友好的提示词组装。
//!
//! ## 这一层存在的唯一理由：让前缀命中缓存
//!
//! DeepSeek 的缓存文档里有个反直觉的例子（[原文](https://api-docs.deepseek.com/zh-cn/guides/kv_cache)）：
//!
//! ```text
//! 第一次请求: A + B          → 落盘前缀单元 "A+B"
//! 第二次请求: A + C          → ✗ 不命中！因为 "A+C" 无法完整匹配 "A+B"
//!                              （此时系统才识别出公共前缀 A 并落盘）
//! 第三次请求: A + D          → ✓ 命中 "A"
//! ```
//!
//! **结论：在提示词中间插入任何变化内容，整段前缀就作废。**
//! 这不是"效率降低一点"，是把已经花的缓存构建成本全部浪费掉。
//!
//! 所以本模块把提示词强制拆成三段，顺序不可变：
//!
//! ```text
//! ┌─────────────────────────────────────┐
//! │ 稳定段：系统提示词 + 人格 + 工具定义  │  ← 字节级稳定，跨调用完全相同
//! ├─────────────────────────────────────┤
//! │ 追加段：已完成步骤的历史              │  ← 只追加，绝不重写前面的
//! ├─────────────────────────────────────┤
//! │ 易变段：当前要问的问题                │  ← 每次不同，放最后
//! └─────────────────────────────────────┘
//! ```
//!
//! [`PromptLayout::fingerprint`] 让"前缀是否稳定"变成**可断言的东西**，
//! 而不是一句写在文档里的叮嘱。见模块测试。

use super::Message;
use super::context::{
    COMPACT_MARKER, Compaction, ContextBudget, ContextUsage, Summarizer, message_chars,
};

/// 三段式提示词布局。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PromptLayout {
    /// 稳定前缀：系统提示词 + 人格 + 工具定义。**跨调用必须字节一致。**
    stable: String,
    /// 追加式历史。只允许往后加，不允许改前面的。
    ///
    /// **唯一的例外是压缩**——它会把最老的一段换成一条摘要消息。
    /// 那是**一次性的、有意的**前缀变更：代价是那一次缓存未命中，
    /// 换来的是不撞上下文上限。见 [`crate::think::context`] 的模块文档。
    history: Vec<Message>,
    /// 易变尾部：当前要问的问题。
    volatile: String,
    /// 上下文用量：精确锚点 + 增量估算。
    #[serde(default)]
    usage: ContextUsage,
}

/// 找一个**不会劈开工具调用序列**的切割点。
///
/// 从 `want` 往前退，直到 `history[cut]` 不是 `Role::Tool`。
/// 退到 0 说明整个历史都在一个工具序列里——那就压不了，
/// 调用方要如实返回"没压成"，而不是硬切。
///
/// ## 为什么不能硬切
///
/// 丢掉一条带 `tool_calls` 的助手消息、却留下它的工具结果，
/// 服务端会报 400，错误信息指向"缺少对应的 tool_call_id"——
/// 看起来像工具出了问题，其实是切错了地方。
/// **切错的代价（对话直接坏掉）比超限的代价（报错）更糟。**
fn safe_cut(history: &[Message], want: usize) -> usize {
    if history.is_empty() {
        return 0;
    }
    // **最多切到"只剩最后一条"。**
    //
    // 允许切满（`cut == len`）有两个问题：一是数组越界（`history[cut]`），
    // 二是语义上把整段历史清空——那不叫压缩，叫失忆。
    let mut cut = want.min(history.len() - 1);
    while cut > 0 && history[cut].role == super::Role::Tool {
        cut -= 1;
    }
    cut
}

impl PromptLayout {
    /// 用稳定前缀开一个布局。
    pub fn new(stable: impl Into<String>) -> Self {
        Self {
            stable: stable.into(),
            history: Vec::new(),
            volatile: String::new(),
            usage: ContextUsage::default(),
        }
    }

    /// 稳定前缀。
    pub fn stable(&self) -> &str {
        &self.stable
    }

    /// 记下"刚才那个问题的回答"。
    ///
    /// **它会消费掉待问的问题**（把 volatile 取走塞进历史），所以不可能出现
    /// "同一个问题既留在 volatile 又进了历史"。
    ///
    /// 早先的 API 是 `push_exchange(question, answer)`，问题要由调用方再传一遍——
    /// 实测那会让同一个问题出现两次，而且下一轮的前缀与上一轮对不上，缓存直接失效。
    /// **API 应该让正确的用法成为唯一顺手的用法。**
    pub fn record_reply(&mut self, assistant: impl Into<String>) {
        if self.volatile.is_empty() {
            return;
        }
        let question = std::mem::take(&mut self.volatile);
        let q = Message::user(question);
        let a = Message::assistant(assistant);
        self.usage.add(message_chars(&q) + message_chars(&a));
        self.history.push(q);
        self.history.push(a);
    }

    /// 设置本轮要问的问题。可以反复覆盖——它本来就是易变的。
    pub fn ask(&mut self, question: impl Into<String>) {
        self.volatile = question.into();
    }

    pub fn history_len(&self) -> usize {
        self.history.len()
    }

    /// 稳定前缀的指纹。
    ///
    /// 用它断言"前缀没变"。**前缀一变，缓存全废**——所以这件事值得有个
    /// 机器可检查的表示，而不是靠人记得。
    pub fn fingerprint(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        self.stable.hash(&mut h);
        h.finish()
    }

    /// 换掉稳定前缀。**历史保留。**
    ///
    /// 用在"载入会话时前缀变了"的场景（比如刚加载了项目的 `AGENTS.md`）。
    /// 历史仍然是有效的对话记录，丢它是过度的。
    pub fn replace_stable(&mut self, stable: impl Into<String>) {
        self.stable = stable.into();
    }

    /// 某个候选前缀的指纹。
    ///
    /// 载入会话时要拿"当前前缀"和"存档里的指纹"比，
    /// 而当前前缀还没进布局——所以需要一个不改状态的算法。
    pub fn fingerprint_for(&self, candidate: &str) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        candidate.hash(&mut h);
        h.finish()
    }

    /// 拼成发给模型的消息列表。
    pub fn build(&self) -> Vec<Message> {
        let mut msgs = Vec::with_capacity(self.history.len() + 2);
        msgs.push(Message::system(self.stable.clone()));
        msgs.extend(self.history.iter().cloned());
        if !self.volatile.is_empty() {
            msgs.push(Message::user(self.volatile.clone()));
        }
        msgs
    }

    /// 往历史里追加一条**原样**的消息（工具调用往返用）。
    ///
    /// 和 [`Self::record_reply`] 的区别：那个是"一问一答"的便捷写法，
    /// 这个是"我自己管这一轮的消息序列"。工具调用需要后者——
    /// 助手消息带 `tool_calls`、随后每条结果带 `tool_call_id`，
    /// 顺序和字段都不能被改写。
    ///
    /// **它消费掉待问的问题**（如果有），这样工具往返不会把同一句话留在
    /// 易变段又进历史——那会让下一轮的前缀对不上，缓存直接失效。
    pub fn push_raw(&mut self, msg: Message) {
        if !self.volatile.is_empty() {
            let q = std::mem::take(&mut self.volatile);
            let m = Message::user(q);
            self.usage.add(message_chars(&m));
            self.history.push(m);
        }
        self.usage.add(message_chars(&msg));
        self.history.push(msg);
    }

    /// 前缀的字符长度，用于估算 token 与成本。
    pub fn stable_len(&self) -> usize {
        self.stable.chars().count()
    }

    // ---- 上下文用量与压缩 ----

    /// 当前的上下文用量。
    pub fn usage(&self) -> ContextUsage {
        self.usage
    }

    /// 历史的第一条。压缩之后它就是那条摘要。
    ///
    /// 单独给一个访问器是因为 `build()[0]` 是**系统消息**（稳定前缀），
    /// 历史从下标 1 开始——这个偏移很容易看错，而看错的表现是
    /// "摘要明明写了却说没有"。
    pub fn head(&self) -> Option<&Message> {
        self.history.first()
    }

    /// 记下一次 API 响应报告的真实 `prompt_tokens`。
    ///
    /// **这是精度的来源。** 它把我们实际发出去多少 token 的精确值拉回来，
    /// 于是估算误差不会累积。
    pub fn anchor_usage(&mut self, prompt_tokens: usize) {
        self.usage.anchor(prompt_tokens);
    }

    /// 超出预算时压缩最老的一段。返回 `None` 表示还没到该压的时候。
    ///
    /// ## 两个必须做对的地方
    ///
    /// **一、切割点不能劈开工具调用序列。**
    ///
    /// 丢掉一条带 `tool_calls` 的助手消息、却留下它的工具结果，
    /// 服务端会报 400——而错误信息指的是"缺少对应的 tool_call_id"，
    /// 看起来像是工具出了问题，其实是我们切错了地方。
    /// 所以切割点要从目标位置**往前退到第一个不是 `Role::Tool` 的消息**。
    ///
    /// **二、摘要要调模型，但模型不可用时不能失败。**
    ///
    /// 压缩被触发时我们已经接近上限了——这时候"摘要失败了所以不压"
    /// 会让下一轮直接撞上限报错。所以失败就退回 [`context::DropMarker`]：
    /// **信息有损，但对话能继续**，而且有损是写在台账里的。
    pub fn compact(
        &mut self,
        summarizer: &dyn Summarizer,
        budget: &ContextBudget,
        keep_recent: usize,
    ) -> Option<Compaction> {
        if !budget.should_compact(&self.usage) {
            return None;
        }

        // **`keep_recent` 是偏好，不是下限。**
        //
        // 最初的写法是 `if history.len() <= keep_recent { return None }`——
        // 于是"消息不多但每条很大"的会话（6 轮长内容 = 12 条）永远压不了，
        // 然后直接撞上限报错。**而那正是压缩要防的事。**
        //
        // 这个 bug 是端到端测试抓到的：`/usage` 显示 3212 token、
        // 触发线 984，却始终没压。
        //
        // 现在：超预算就必须让出空间。同时**至少砍掉一半**——
        // 只砍一两条在"超了三倍"时毫无意义，下一轮又要压一次，
        // 而每次压缩都意味着一次缓存未命中。
        let len = self.history.len();
        if len < 2 {
            return None;
        }
        // **至少留住最后一轮交换（2 条）。**
        // 少于这个数就等于让模型失去眼前的上下文——那比超限更糟，
        // 因为它会开始问你已经说过的事。
        let keep = keep_recent.min((len / 2).max(2));
        let want_cut = len - keep;
        let cut = safe_cut(&self.history, want_cut);
        if cut == 0 {
            // 整个历史都在一个工具序列里——压不了。
            // **不强行压**：切错位置的代价是 400，比超限更糟。
            return None;
        }

        let before_tokens = self.usage.estimated_tokens();
        let dropped: Vec<Message> = self.history.drain(..cut).collect();
        let dropped_chars: usize = dropped.iter().map(message_chars).sum();
        let dropped_tool_calls: usize = dropped.iter().map(|m| m.tool_calls.len()).sum();

        // 兜底摘要。**压缩不能失败**，所以无论模型那边出什么事，
        // 这里都要产出一段可用的文字。
        let fallback = |err: Option<String>| -> (String, bool, Option<String>) {
            let text = super::context::DropMarker
                .summarize(&dropped)
                .unwrap_or_else(|_| format!("{COMPACT_MARKER}（此前内容已省略）"));
            (text, false, err)
        };
        let (summary, summarized, summary_error) = match summarizer.summarize(&dropped) {
            Ok(s) if !s.trim().is_empty() => (s, true, None),
            // 空摘要等于把历史扔了却不留任何说明——那比兜底更糟
            Ok(_) => fallback(Some("模型返回了空摘要".into())),
            // **失败原因必须留下来**：静默退兜底会让"每次压缩都在丢信息"
            // 完全不可见，而使用者的感受是"它怎么不记得了"，无从追查。
            Err(e) => fallback(Some(e)),
        };

        let head = Message::user(format!("{COMPACT_MARKER}\n{summary}"));
        let head_chars = message_chars(&head);
        self.history.insert(0, head);

        // 剩余内容：摘要 + 保住的历史。**锚点作废**——
        // 下一轮的精确值由下一次响应给。
        let remaining: usize = self.history.iter().map(message_chars).sum();
        self.usage.reset_after_compaction(remaining);

        let c = Compaction {
            dropped_messages: dropped.len(),
            dropped_chars,
            dropped_tool_calls,
            kept_messages: self.history.len() - 1,
            before_tokens,
            after_tokens: self.usage.estimated_tokens(),
            summarized,
            summary_error,
        };
        let _ = head_chars;
        Some(c)
    }
}

/// 人格 + 系统提示词的组装。
///
/// **它必须是纯函数**：同样的输入永远产出同样的字节。任何掺进去的
/// "当前时间"、"本次运行 id" 都会让缓存前缀失效——那些东西属于易变段。
pub fn build_persona(name: &str, persona: &str, rules: &[String]) -> String {
    let mut out = String::new();
    out.push_str("# 身份\n");
    out.push_str(name);
    out.push_str("\n\n# 人格\n");
    out.push_str(persona);
    if !rules.is_empty() {
        out.push_str("\n\n# 硬规则\n");
        for (i, r) in rules.iter().enumerate() {
            out.push_str(&format!("{}. {}\n", i + 1, r));
        }
    }
    out
}

#[cfg(test)]
mod compaction_tests {
    use super::*;
    use crate::think::Role;
    use crate::think::context::{ContextBudget, DropMarker};

    /// 一个只在小预算下才触发的预算。
    fn tiny_budget() -> ContextBudget {
        ContextBudget {
            window_tokens: 200,
            reserve_for_reply: 0,
            compact_at: 0.5,
        }
    }

    fn layout_with_turns(n: usize, chars: usize) -> PromptLayout {
        let mut l = PromptLayout::new("稳定前缀");
        for i in 0..n {
            l.ask(format!("问题 {i} {}", "字".repeat(chars)));
            l.record_reply(format!("回答 {i} {}", "字".repeat(chars)));
        }
        l
    }

    // ---- 触发条件 ----

    #[test]
    fn nothing_happens_below_the_budget() {
        let mut l = layout_with_turns(1, 10);
        assert!(l.compact(&DropMarker, &tiny_budget(), 2).is_none());
    }

    #[test]
    fn a_short_but_huge_history_still_compacts() {
        // **这个 bug 是端到端测试抓到的。**
        //
        // 6 轮长内容 = 12 条消息，正好等于 `KEEP_RECENT`。
        // 而最初的写法是 `len <= keep_recent 就不压`——于是
        // "消息不多但每条很大"的会话永远压不了，然后直接撞上限报错。
        // **而那正是压缩要防的事。**
        let mut l = layout_with_turns(6, 2000); // 12 条，每条很大
        let c = l
            .compact(&DropMarker, &tiny_budget(), 12)
            .expect("超出预算就必须压——消息条数少不是不压的理由");
        assert!(c.saved_tokens() > 0);
    }

    #[test]
    fn compaction_cuts_at_least_half_when_the_message_count_is_low() {
        // 只砍一两条在"超了三倍"时毫无意义：下一轮又要压一次，
        // 而每次压缩都意味着一次缓存未命中。
        let mut l = layout_with_turns(6, 2000);
        let c = l.compact(&DropMarker, &tiny_budget(), 12).unwrap();
        assert!(
            c.dropped_messages >= 4,
            "至少该砍掉一半（12 条里的 6 条），实际砍了 {}",
            c.dropped_messages
        );
    }

    #[test]
    fn a_preferred_keep_count_is_honoured_when_there_is_enough_history() {
        // 历史够长时，`keep_recent` 该被尊重——否则每次都砍掉一半，
        // 模型很快就会忘记刚才在说什么
        let mut l = layout_with_turns(100, 50);
        let c = l.compact(&DropMarker, &tiny_budget(), 12).unwrap();
        assert!(
            c.kept_messages >= 12,
            "该留住 12 条，实际留了 {}",
            c.kept_messages
        );
    }

    #[test]
    fn compaction_fires_when_over_budget() {
        let mut l = layout_with_turns(30, 100);
        let c = l.compact(&DropMarker, &tiny_budget(), 4).expect("该压了");
        assert!(c.dropped_messages > 0);
        assert!(c.saved_tokens() > 0, "压了却一点没省: {c:?}");
    }

    #[test]
    fn a_single_exchange_is_never_compacted() {
        // **至少留住最后一轮交换。** 压掉它等于让模型失去眼前的上下文——
        // 那比超限更糟，因为它会开始问你已经说过的事。
        let mut l = layout_with_turns(1, 5000);
        assert!(
            l.compact(&DropMarker, &tiny_budget(), 4).is_none(),
            "只有一轮交换时没有可压的"
        );
    }

    // ---- 硬约束一：不劈开工具调用序列 ----

    #[test]
    fn a_cut_never_leaves_an_orphan_tool_result() {
        // **丢掉请求工具的助手消息、却留下工具结果，服务端会报 400。**
        // 而错误信息指向"缺少 tool_call_id"——看起来像工具坏了，
        // 其实是切错了地方。
        let mut l = PromptLayout::new("p");
        for i in 0..10 {
            l.ask(format!("问题 {i}"));
            l.push_raw(Message::assistant_tool_calls(vec![serde_json::json!({
                "id": format!("call_{i}"), "type": "function",
                "function": {"name": "now", "arguments": "{}"}
            })]));
            l.push_raw(Message::tool_result(format!("call_{i}"), "结果"));
            l.record_reply("说完了");
        }
        l.compact(&DropMarker, &tiny_budget(), 3);

        // **不变量：每一条工具结果都能找到它对应的助手请求。**
        let h = l.build();
        for (i, m) in h.iter().enumerate() {
            if m.role != Role::Tool {
                continue;
            }
            let id = m.tool_call_id.as_deref().expect("工具结果必须有 id");
            let found = h[..i].iter().any(|prev| {
                prev.tool_calls
                    .iter()
                    .any(|tc| tc.get("id").and_then(|v| v.as_str()) == Some(id))
            });
            assert!(found, "第 {i} 条工具结果成了孤儿（{id}）：压缩切错了位置");
        }
    }

    #[test]
    fn a_cut_lands_on_a_boundary_not_inside_a_tool_run() {
        let history = vec![
            Message::user("u0"),
            Message::assistant_tool_calls(vec![serde_json::json!({"id": "a"})]),
            Message::tool_result("a", "r"),
            Message::assistant("a0"),
            Message::user("u1"),
        ];
        // 想切 3 条，但第 3 条是工具结果 —— 要退到 2
        let cut = safe_cut(&history, 3);
        // **要断言的是不变量，不是一个具体数字。**
        // 切在 3 也是对的：工具请求 [1] 和它的结果 [2] 一起被丢掉，
        // 没有孤儿。写死数字会把正确的实现判成错的。
        assert_ne!(history[cut].role, Role::Tool, "切点不能落在工具结果上");
    }

    #[test]
    fn a_history_that_is_one_tool_run_cannot_be_cut() {
        // 退到 0 说明整段都在一个工具序列里 —— **不硬切**。
        // 切错的代价（对话直接坏掉）比超限的代价（报错）更糟。
        let history = vec![
            Message::assistant_tool_calls(vec![serde_json::json!({"id": "a"})]),
            Message::tool_result("a", "r"),
        ];
        assert_eq!(safe_cut(&history, 2), 0);
    }

    #[test]
    fn a_normal_boundary_is_used_as_is() {
        let history = vec![Message::user("u0"), Message::assistant("a0")];
        assert_eq!(safe_cut(&history, 1), 1);
    }

    // ---- 硬约束二：压缩后前缀稳定 ----

    #[test]
    fn the_prefix_is_byte_stable_between_compactions() {
        // **这是缓存纪律的核心。**
        //
        // 压缩一次 → 未命中一次；之后每轮都要命中。
        // 如果摘要每轮都重算，前缀就每轮都变，缓存全废——
        // 而那种失效是静默的，只表现为账单变贵。
        let mut l = layout_with_turns(30, 100);
        l.compact(&DropMarker, &tiny_budget(), 4).expect("该压了");

        // 压缩之后前缀长这样：稳定前缀 + 摘要
        let after_compact: Vec<Message> = l.build();
        let head_after = after_compact[0..2.min(after_compact.len())].to_vec();

        // 再聊几轮（不该再触发压缩）
        let mut l2 = l.clone();
        l2.ask("新问题");
        l2.record_reply("新回答");
        let after_more: Vec<Message> = l2.build();

        assert_eq!(
            &after_more[0..head_after.len()],
            &head_after[..],
            "追加消息不该改动前缀——否则每轮都缓存未命中"
        );
    }

    #[test]
    fn the_summary_message_is_visibly_marked() {
        // 读台账的人（和模型）都要一眼看出"这里之前的东西被压掉了"，
        // 而不是以为对话就是从这儿开始的
        let mut l = layout_with_turns(30, 100);
        l.compact(&DropMarker, &tiny_budget(), 4);
        assert!(
            l.head().unwrap().content.contains(COMPACT_MARKER),
            "摘要要带标记: {}",
            l.head().unwrap().content
        );
        // 顺带确认 `build()` 的结构：第 0 条是系统消息（稳定前缀），
        // 摘要从第 1 条开始——**这个偏移很容易看错**
        assert_eq!(l.build()[0].role, Role::System);
        assert!(l.build()[1].content.contains(COMPACT_MARKER));
    }

    #[test]
    fn compacting_twice_does_not_double_up_markers_at_the_front() {
        // 第二次压缩会把上一次的摘要一起压掉——那是对的（它也是历史），
        // 但结果里不该出现两条并列的摘要头
        let mut l = layout_with_turns(40, 200);
        l.compact(&DropMarker, &tiny_budget(), 4);
        // 再塞很多，逼出第二次
        for i in 0..40 {
            l.ask(format!("追加 {i} {}", "字".repeat(200)));
            l.record_reply("好");
        }
        l.compact(&DropMarker, &tiny_budget(), 4);
        let heads = l
            .build()
            .iter()
            .take(3)
            .filter(|m| m.content.contains(COMPACT_MARKER))
            .count();
        assert_eq!(heads, 1, "开头不该堆多个摘要");
    }

    // ---- 用量记账 ----

    #[test]
    fn appending_updates_the_usage_estimate() {
        let mut l = PromptLayout::new("p");
        let before = l.usage().estimated_tokens();
        l.ask("很长的问题".repeat(50));
        l.record_reply("很长的回答".repeat(50));
        assert!(l.usage().estimated_tokens() > before);
    }

    #[test]
    fn an_anchor_makes_the_usage_exact() {
        let mut l = PromptLayout::new("p");
        l.ask("q");
        l.anchor_usage(1234);
        assert!(l.usage().is_exact());
        assert_eq!(l.usage().estimated_tokens(), 1234);
    }

    #[test]
    fn compaction_invalidates_the_anchor() {
        // 压缩之后内容全变了，旧锚点不再代表现在的用量
        let mut l = layout_with_turns(30, 100);
        l.anchor_usage(999_999);
        l.compact(&DropMarker, &tiny_budget(), 4);
        assert!(l.usage().anchor_tokens.is_none(), "旧锚点必须作废");
        assert!(
            l.usage().estimated_tokens() < 999_999,
            "压缩之后估出来的应该小得多"
        );
    }

    #[test]
    fn tool_calls_count_toward_the_trigger() {
        // 工具调用的 JSON 占的 token 不少，只算 content 会低估，
        // 而低估的后果是撞上限
        let mut a = PromptLayout::new("p");
        a.ask("q");
        a.record_reply("a");
        let mut b = PromptLayout::new("p");
        b.ask("q");
        b.push_raw(Message::assistant_tool_calls(vec![serde_json::json!({
            "id": "call_1", "type": "function",
            "function": {"name": "web_search", "arguments": "{\"q\":\"很长的查询字符串\"}"}
        })]));
        assert!(b.usage().estimated_tokens() > a.usage().estimated_tokens());
        let _ = a;
    }

    // ---- 摘要失败要兜底 ----

    struct AlwaysFails;
    impl Summarizer for AlwaysFails {
        fn summarize(&self, _: &[Message]) -> Result<String, String> {
            Err("模型不可用".into())
        }
    }

    struct ReturnsEmpty;
    impl Summarizer for ReturnsEmpty {
        fn summarize(&self, _: &[Message]) -> Result<String, String> {
            Ok("   ".into())
        }
    }

    #[test]
    fn a_failing_summarizer_still_compacts() {
        // **压缩不能失败。** 它被触发时我们已经接近上限了——
        // "摘要失败所以不压"会让下一轮直接撞上限报错。
        let mut l = layout_with_turns(30, 100);
        let c = l
            .compact(&AlwaysFails, &tiny_budget(), 4)
            .expect("必须压成");
        assert!(!c.summarized, "要如实标出用的是兜底");
        assert!(c.saved_tokens() > 0);
    }

    #[test]
    fn an_empty_summary_is_treated_as_failure() {
        // 空摘要等于把历史扔了却不留任何说明——那比兜底更糟
        let mut l = layout_with_turns(30, 100);
        let c = l
            .compact(&ReturnsEmpty, &tiny_budget(), 4)
            .expect("必须压成");
        assert!(!c.summarized);
        assert!(l.head().unwrap().content.contains(COMPACT_MARKER));
    }

    #[test]
    fn the_fallback_still_says_how_much_was_lost() {
        let mut l = layout_with_turns(30, 100);
        l.compact(&AlwaysFails, &tiny_budget(), 4);
        let head = &l.head().unwrap().content;
        assert!(head.contains("消息"), "要说清丢了多少: {head}");
    }

    // ---- 压缩之后对话还能继续 ----

    #[test]
    fn a_compacted_layout_can_keep_growing() {
        let mut l = layout_with_turns(30, 100);
        l.compact(&DropMarker, &tiny_budget(), 4);
        let after = l.history_len();
        l.ask("压完之后的新问题");
        l.record_reply("新回答");
        assert_eq!(l.history_len(), after + 2, "压缩之后还要能正常追加");
    }

    #[test]
    fn compaction_is_recorded_with_readable_numbers() {
        let mut l = layout_with_turns(30, 100);
        let c = l.compact(&DropMarker, &tiny_budget(), 4).unwrap();
        let s = c.summary();
        assert!(s.contains("压缩"), "{s}");
        assert!(s.contains("token"), "{s}");
    }
}

#[cfg(test)]
mod tests {
    use super::super::Role;
    use super::*;

    fn persona() -> String {
        build_persona(
            "云熙",
            "你是常驻在用户电脑上的助理，说话克制、不客套。",
            &["不可逆动作必须人工批准".into(), "不确定就不打扰".into()],
        )
    }

    #[test]
    fn stable_prefix_survives_history_growth() {
        // 这是缓存命中率的守门测试：历史增长时，稳定前缀的指纹**不得变化**
        let mut p = PromptLayout::new(persona());
        let f0 = p.fingerprint();

        p.ask("第一步做什么？");
        let f1 = p.fingerprint();
        assert_eq!(f0, f1, "改易变段不该影响稳定前缀");

        p.record_reply("先备份");
        let f2 = p.fingerprint();
        assert_eq!(f0, f2, "追加历史不该影响稳定前缀");

        p.ask("第二步做什么？");
        assert_eq!(f0, p.fingerprint(), "再问一次也不该影响稳定前缀");
    }

    #[test]
    fn persona_is_a_pure_function() {
        // 掺进任何"当前时间/运行 id"都会让缓存前缀失效，
        // 所以人格组装必须是纯函数——同样输入永远同样字节
        let a = persona();
        let b = persona();
        assert_eq!(a, b);
        assert_eq!(
            PromptLayout::new(a).fingerprint(),
            PromptLayout::new(b).fingerprint()
        );
    }

    #[test]
    fn layout_order_is_stable_then_history_then_volatile() {
        let mut p = PromptLayout::new("STABLE");
        p.ask("U1");
        p.record_reply("A1");
        p.ask("Q2");
        let msgs = p.build();

        assert_eq!(msgs.len(), 4);
        assert_eq!(msgs[0].role, Role::System);
        assert_eq!(msgs[0].content, "STABLE");
        assert_eq!(msgs[1].content, "U1");
        assert_eq!(msgs[2].content, "A1");
        assert_eq!(msgs[3].role, Role::User);
        assert_eq!(msgs[3].content, "Q2", "易变内容必须在最后");
    }

    #[test]
    fn history_is_append_only_in_effect() {
        // 模拟两轮：第一轮的**非易变部分**必须逐字保留在第二轮里。
        //
        // 注意只比到倒数第二条——最后一条是"本轮要问的问题"，它本来就该变。
        // 最初的版本把整段都比了，于是测试失败；失败本身是对的，
        // 但它同时暴露了当时 API 的一个真问题（见 record_reply 的文档）。
        let mut p = PromptLayout::new("S");
        p.ask("Q1");
        p.record_reply("A1");
        let first = p.build();
        assert_eq!(
            first.len(),
            3,
            "System + Q1 + A1（待问已被 record_reply 消费）"
        );

        p.ask("Q2");
        let second = p.build();

        let prefix = first.len();
        for i in 0..prefix {
            assert_eq!(
                first[i].content, second[i].content,
                "第 {i} 条被改写了，缓存会失效"
            );
        }
        assert_eq!(second.last().unwrap().content, "Q2");
    }

    #[test]
    fn record_reply_consumes_the_pending_question() {
        // 这个 API 的设计目的：让"同一个问题出现两次"不可能发生
        let mut p = PromptLayout::new("S");
        p.ask("Q1");
        p.record_reply("A1");

        let users: Vec<String> = p
            .build()
            .into_iter()
            .filter(|m| m.role == Role::User)
            .map(|m| m.content)
            .collect();
        assert_eq!(
            users,
            vec!["Q1".to_string()],
            "问题只该出现一次（在历史里）"
        );
        assert_eq!(p.build().len(), 3, "待问已被清空，不该多出空消息");
    }

    #[test]
    fn record_reply_without_a_pending_question_is_a_noop() {
        let mut p = PromptLayout::new("S");
        p.record_reply("凭空来的回答");
        assert_eq!(p.history_len(), 0, "没有待问的问题时不该往历史里塞东西");
    }

    #[test]
    fn changing_the_persona_does_change_the_fingerprint() {
        // 反例：人格变了前缀就该变（否则测试本身是假的）
        let a = PromptLayout::new(build_persona("云熙", "克制", &[])).fingerprint();
        let b = PromptLayout::new(build_persona("云熙", "活泼", &[])).fingerprint();
        assert_ne!(a, b);
    }

    #[test]
    fn rules_are_numbered_deterministically() {
        let p = persona();
        assert!(p.contains("1. 不可逆动作必须人工批准"));
        assert!(p.contains("2. 不确定就不打扰"));
    }

    #[test]
    fn empty_volatile_is_omitted() {
        let p = PromptLayout::new("S");
        assert_eq!(p.build().len(), 1, "没问问题时不该多出一条空消息");
    }
}
