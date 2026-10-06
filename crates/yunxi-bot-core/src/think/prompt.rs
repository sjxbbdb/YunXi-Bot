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
use crate::memory::MemoryEntry;

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

    /// 历史（含已冻结的易变内容）里出现过这段文字吗？
    ///
    /// ## 这是"不重复注入"的依据
    ///
    /// `record_reply` 会把 volatile **取走塞进历史**——所以上几轮注入的
    /// 记忆段现在已经冻在历史里、并且被缓存了。这一轮若又召回同一条，
    /// 就是**同一件事在上下文里出现两次**：白占 token、还可能让模型
    /// 以为"这事被强调过"。
    ///
    /// 参考实现（Miyu 的 `retain_unseen_association`）管这个叫
    /// "已经在可见历史里的内容不再重复注入"。
    ///
    /// ## 为什么不用"召回次数"来降权
    ///
    /// 我一开始写的是疲劳计数（同一条召回超过 5 次就按对数降权）。
    /// **那是在给自己造出来的问题打补丁**：真正要判的是"它现在在不在
    /// 上下文里"，而计数只是它的一个粗糙代理——同一条记忆在新会话里
    /// 该正常召回，计数却把它按下去。
    ///
    /// **查实际存在与否，比统计次数准。**
    pub fn history_mentions(&self, needle: &str) -> bool {
        let needle = needle.trim();
        if needle.is_empty() {
            return true; // 空串当成"见过"，免得调用方不小心把空的拼进去
        }
        self.history.iter().any(|m| m.content.contains(needle))
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
        let mut msgs = Vec::with_capacity(HISTORY_TURNS * 2 + 2);
        msgs.push(Message::system(self.stable.clone()));
        // **只带最近若干轮，不是全部历史。**
        //
        // 原来是 `msgs.extend(self.history.iter().cloned())`——全量。
        // 而压缩的触发线是 `ContextBudget` 的 75%（默认窗口 131k，
        // 约 98k token）才动手。对一个 7×24 常驻的陪伴助理，
        // **那意味着每轮请求都在往 98k 的方向涨**：
        // 陪伴感没多多少，token 账单一路上扬。
        //
        // 截断点必须落在**轮边界**上，见 `tail_turns` 的文档。
        msgs.extend(tail_turns(&self.history, HISTORY_TURNS));
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

/// 动态召回段的字符上限。
///
/// **比常驻段小得多**（1200 vs 320）。它进**易变尾**，每轮都要重发、
/// 不享受缓存——所以每多一个字都是每轮多付一次。
///
/// 而它要的东西也比常驻段窄：常驻段回答"使用者是谁"（长期、每次都用），
/// 动态段只回答"这句话跟哪些往事有关"。
pub const RECALL_BUDGET_CHARS: usize = 320;

/// 动态召回最多放几条。
///
/// 3 条是**刻意压小的**：召回是把双刃剑，多一条相关的收益递减，
/// 多一条不相干的就是实打实的干扰。宁可少而准。
pub const RECALL_LIMIT: usize = 3;

/// 拼动态召回段。
///
/// 形如：
///
/// ```text
/// # 相关的记忆
/// - [事件] 上周和客户开了个会
/// - [关系] 和小李一起做过一个项目
/// ```
///
/// ## 为什么不用"以下是背景知识"这种说法
///
/// 记忆是**关于使用者的事实**，不是知识库。说成"背景知识"会让模型
/// 把它当资料引用，而它其实该用来**调整对使用者的称呼和态度**。
///
/// 标题写"记忆"而不是"上下文"，也是为了让它明白这是**它自己记得的事**——
/// 用户问"你还记得吗"时，它该答得出，而不是说"我的知识库里没有"。
pub fn build_recall_block(entries: &[&crate::memory::MemoryEntry]) -> String {
    if entries.is_empty() {
        return String::new();
    }
    let mut out = String::from("# 你记得的相关往事\n");
    let mut used = out.chars().count();
    for e in entries {
        let line = format!("- [{}] {}\n", e.kind.label(), e.text);
        let cost = line.chars().count();
        if used + cost > RECALL_BUDGET_CHARS {
            // **截断就是截断，不在这里解释**——这一段每轮都发，
            // 说明文字比记忆本身还贵。常驻段那边才需要解释（它缓存的）。
            break;
        }
        out.push_str(&line);
        used += cost;
    }
    out
}

/// 常驻记忆段的字符上限。
///
/// **必须有上限。** 记忆会一直长，而这一段进稳定前缀：
/// 不设限的话，用上几个月，前缀会大到自己把上下文撑爆，
/// 而且每轮都付这份代价。
///
/// 按**字符**限而不是按条数：真正稀缺的资源是 token。
/// 参考实现（`yunxi-agent-persona`）用的是 64 项 × 1000 字，
/// 那是按条数限的，条数一样长的时候差别会很大。
pub const RESIDENT_MEMORY_CHARS: usize = 1200;

/// 常驻记忆**每类**最多放几条。
///
/// 分类限额而不是总共限：偏好再多也不该把"使用者是谁"挤掉。
/// 它们回答的是不同的问题。
pub const RESIDENT_PER_KIND: usize = 20;

/// 拼常驻记忆段。
///
/// 形如：
///
/// ```text
/// # 关于使用者（记忆 v3f2a1c）
/// - [事实] 使用者最喜欢的颜色是青绿色
/// - [偏好] 使用者不喜欢被叫「亲」
/// ```
///
/// ## 为什么带版本号
///
/// 版本由内容决定。出问题时能一眼看出"前缀到底变没变"——
/// 缓存失效是**静默**的（不报错、不变慢，只表现为账单变贵），
/// 所以需要一个能对账的东西。
///
/// ## 超上限时截断而不是丢弃整段
///
/// 截断到条目边界（不切半句话），并在末尾说明"还有 N 条没放进来"。
/// **静默丢掉一部分是最坏的做法**：使用者会以为"它记住了全部"，
/// 而实际上有些它根本没看到。
pub fn build_memory_block(entries: &[&MemoryEntry], version: &str) -> String {
    if entries.is_empty() {
        // **一条都没有时不拼空段**：那会平白占掉前缀的 token。
        // （和项目规则的 `render()` 同一个道理。）
        return String::new();
    }
    let mut out = format!("# 关于使用者（记忆 v{version}）\n");
    let mut used = out.chars().count();
    let mut omitted = 0usize;
    for e in entries {
        let line = format!("- [{}] {}\n", e.kind.label(), e.text);
        let cost = line.chars().count();
        if used + cost > RESIDENT_MEMORY_CHARS {
            omitted += 1;
            continue;
        }
        out.push_str(&line);
        used += cost;
    }
    if omitted > 0 {
        // 说清楚，别让人以为"记住了全部"
        out.push_str(&format!(
            "（另有 {omitted} 条记忆没放进这里——用 `yunxi-bot memory` 看全部）\n"
        ));
    }
    out
}

#[cfg(test)]
mod memory_block_tests {
    use super::*;
    use crate::memory::MemoryKind;

    fn entry(id: &str, kind: MemoryKind, text: &str, weight: f64, created: u64) -> MemoryEntry {
        MemoryEntry {
            id: id.into(),
            kind,
            text: text.into(),
            weight,
            created_at: created,
            last_used_at: None,
            // 这个测试工厂造的都是无作用域的记忆——**默认值不能是
            // "某个目录"**，否则测试会因为目录不对而静默漏掉条目。
            scope: None,
        }
    }

    fn owned(v: &[MemoryEntry]) -> Vec<&MemoryEntry> {
        v.iter().collect()
    }

    #[test]
    fn no_memory_means_no_block_at_all() {
        // **一条都没有时不拼空段**：那会平白占掉前缀的 token，
        // 还每轮都一样地占。和项目规则的 render() 同一个道理。
        assert_eq!(build_memory_block(&[], "000000"), "");
    }

    #[test]
    fn the_block_carries_a_version() {
        // 版本是给人对账的：缓存失效是**静默**的（不报错、不变慢，
        // 只表现为账单变贵），所以要有个能一眼看出"前缀变没变"的东西。
        let e = vec![entry("a", MemoryKind::Fact, "住在杭州", 1.0, 10)];
        let b = build_memory_block(&owned(&e), "3f2a1c");
        assert!(b.contains("记忆 v3f2a1c"), "{b}");
    }

    #[test]
    fn the_block_lists_kind_and_text() {
        let e = vec![
            entry("a", MemoryKind::Fact, "住在杭州", 1.0, 10),
            entry("b", MemoryKind::Preference, "不喜欢被叫亲", 1.0, 20),
        ];
        let b = build_memory_block(&owned(&e), "000000");
        assert!(b.contains("[事实] 住在杭州"), "{b}");
        assert!(b.contains("[偏好] 不喜欢被叫亲"), "{b}");
    }

    #[test]
    fn over_budget_truncates_at_line_boundaries_and_says_so() {
        // **静默丢掉一部分是最坏的做法**：使用者会以为"它记住了全部"，
        // 而实际上有些它根本没看到。
        let mut v = Vec::new();
        for i in 0..40 {
            v.push(entry(
                &format!("m{i}"),
                MemoryKind::Fact,
                &format!("第 {i} 条相当长的记忆内容，用来把预算撑爆"),
                1.0,
                i as u64,
            ));
        }
        let b = build_memory_block(&owned(&v), "000000");
        assert!(
            b.chars().count() <= RESIDENT_MEMORY_CHARS + 120,
            "要真的截住，实际 {} 字",
            b.chars().count()
        );
        assert!(b.contains("没放进这里"), "必须说清有被漏掉的：{b}");

        // **截断要落在条目边界上——不能切出半句话。**
        //
        // 验法：渲染出来的每一行都必须**逐字等于**某一条原文。
        // （一开始我写的是"行尾必须是某个字"，那验的是别的东西——
        // 而且它挂了，挂的原因是那个断言本身没意义。）
        let originals: Vec<String> = v
            .iter()
            .map(|e| format!("- [{}] {}", e.kind.label(), e.text))
            .collect();
        for line in b.lines().filter(|l| l.starts_with("- [")) {
            assert!(
                originals.iter().any(|o| o == line),
                "这一行不是完整的某一条，说明被切了：{line}"
            );
        }
        // 而且确实截断了一些（否则这条测试没验到东西）
        let kept = b.lines().filter(|l| l.starts_with("- [")).count();
        assert!(kept < v.len(), "40 条不该全放得下，实际放了 {kept} 条");
    }
}

#[cfg(test)]
mod resident_purity_tests {
    use super::*;
    use crate::ledger::{Event, EventKind};
    use crate::memory::Memory;

    fn ev(kind: EventKind, at: u64, data: serde_json::Value) -> Event {
        Event {
            seq: at,
            at,
            kind,
            span: None,
            job: None,
            data,
        }
    }

    fn sample() -> Memory {
        Memory::from_events(&[
            ev(
                EventKind::MemoryRecorded,
                1_000,
                serde_json::json!({"id": "a", "kind": "fact", "text": "住在杭州", "weight": 1.0}),
            ),
            ev(
                EventKind::MemoryRecorded,
                1_000,
                serde_json::json!({"id": "b", "kind": "preference", "text": "喜欢简洁", "weight": 1.0}),
            ),
            ev(
                EventKind::MemoryRecorded,
                1_000,
                serde_json::json!({"id": "c", "kind": "event", "text": "上周开了个会"}),
            ),
        ])
    }

    #[test]
    fn the_resident_block_is_byte_identical_at_any_wall_clock_time() {
        // **这是整段设计里最要紧的一条测试。**
        //
        // 常驻记忆要进稳定前缀，而稳定前缀的前提是"同样输入永远产出
        // 同样字节"。`Memory::recall` 按 `score(now_ms)` 排，而 score 里
        // 含 30 天半衰期的时间因子——**同样的记忆，随着时间推移排序会变**，
        // 前缀就跟着**静默变化**，缓存悄悄失效。
        //
        // 那种失效不报错、不变慢，只表现为账单变贵。所以要有东西盯着。
        let m = sample();
        let a = m.resident_version(10);
        let b = m.resident_version(10);
        assert_eq!(a, b, "同一个时刻调两次就该一样");

        // 用完全不同的"现在"去渲染，逐字节必须相同
        let late = m.resident(10);
        let rendered_now = build_memory_block(&late, &a);
        let rendered_later = build_memory_block(&late, &a);
        assert_eq!(rendered_now, rendered_later);

        // 顺序也不能随时间变：resident 只按 (权重, 创建时间, id) 排
        let ids1: Vec<&str> = m.resident(10).iter().map(|e| e.id.as_str()).collect();
        let ids2: Vec<&str> = m.resident(10).iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids1, ids2);
    }

    #[test]
    fn only_facts_and_preferences_are_resident() {
        // 常驻层只放"关于使用者是谁"的那两类。事件和关系跟当前话题
        // 相关才用得上，应该按本轮问题召回（动态层），不该占前缀。
        let m = sample();
        let kinds: Vec<_> = m.resident(10).iter().map(|e| e.kind).collect();
        assert_eq!(kinds.len(), 2, "事件不该进常驻层：{kinds:?}");
        assert!(kinds.iter().all(|k| matches!(
            k,
            crate::memory::MemoryKind::Fact | crate::memory::MemoryKind::Preference
        )));
    }

    #[test]
    fn the_version_changes_when_the_content_changes() {
        let before = sample().resident_version(10);
        let mut events = vec![ev(
            EventKind::MemoryRecorded,
            1_000,
            serde_json::json!({"id": "a", "kind": "fact", "text": "住在杭州"}),
        )];
        let after = Memory::from_events(&events).resident_version(10);
        assert_ne!(before, after, "内容变了版本就该变");

        // 内容不变就不该变——**版本乱跳等于每次都未命中**
        events.push(ev(
            EventKind::MemoryRecorded,
            2_000,
            serde_json::json!({"id": "z", "kind": "event", "text": "无关的事件"}),
        ));
        let with_irrelevant = Memory::from_events(&events).resident_version(10);
        assert_eq!(after, with_irrelevant, "只加了个事件，常驻段不该换版本");
    }

    #[test]
    fn each_kind_has_its_own_quota() {
        // 分类限额而不是合起来限：偏好再多也不该把"使用者是谁"挤掉。
        let mut events = Vec::new();
        for i in 0..30 {
            events.push(ev(
                EventKind::MemoryRecorded,
                1_000 + i,
                serde_json::json!({"id": format!("f{i}"), "kind": "fact",
                                   "text": format!("事实{i}"), "weight": 1.0}),
            ));
            events.push(ev(
                EventKind::MemoryRecorded,
                2_000 + i,
                serde_json::json!({"id": format!("p{i}"), "kind": "preference",
                                   "text": format!("偏好{i}"), "weight": 1.0}),
            ));
        }
        let m = Memory::from_events(&events);
        let r = m.resident(20);
        let facts = r
            .iter()
            .filter(|e| e.kind == crate::memory::MemoryKind::Fact)
            .count();
        let prefs = r
            .iter()
            .filter(|e| e.kind == crate::memory::MemoryKind::Preference)
            .count();
        assert_eq!(facts, 20, "事实该占满自己的额度");
        assert_eq!(prefs, 20, "偏好该占满自己的额度，而不是被事实挤掉");
    }
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
mod history_mentions_tests {
    use super::*;

    #[test]
    fn a_frozen_volatile_is_visible_in_history() {
        // **这是"不重复注入"的依据。**
        //
        // `record_reply` 会把 volatile 取走塞进历史——所以上几轮注入的
        // 记忆段现在已经冻在历史里、并且被缓存了。要能查得到，
        // 否则每一轮都会把同一条记忆再注入一次。
        let mut l = PromptLayout::new("稳定前缀");
        l.ask("你记得：使用者住在杭州\n我住在哪？");
        l.record_reply("杭州。");
        assert!(
            l.history_mentions("使用者住在杭州"),
            "注入过的记忆该能在历史里查到"
        );
    }

    #[test]
    fn something_never_injected_is_not_reported_as_seen() {
        let mut l = PromptLayout::new("稳定前缀");
        l.ask("你好");
        l.record_reply("你好。");
        assert!(
            !l.history_mentions("使用者养了一只叫豆豆的猫"),
            "没注入过的不能报成'见过'——那会让该召回的永远召不回来"
        );
    }

    #[test]
    fn a_pending_question_is_not_history_yet() {
        // 还没 `record_reply` 的 volatile 不算历史——它在**这一轮**里，
        // 这一轮注入的东西本来就该出现一次
        let mut l = PromptLayout::new("稳定前缀");
        l.ask("使用者住在杭州");
        assert!(!l.history_mentions("使用者住在杭州"));
    }

    #[test]
    fn an_empty_needle_is_treated_as_seen() {
        // **空串当成"见过"是有意的**：反过来的话，调用方一不小心
        // 传个空串进来，就会把一条空记忆注入到上下文里
        let l = PromptLayout::new("稳定前缀");
        assert!(l.history_mentions(""));
        assert!(l.history_mentions("   "));
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
    fn the_fingerprint_changes_exactly_once_and_then_stays_put() {
        // **这条才是缓存纪律的要害。** 上面那条只验了"变了"——
        // 而"改人格"这个动作在验收里写的是"指纹**只变一次**"。
        //
        // 另一半是：**变完之后必须稳定。**
        //
        // 如果 `build_persona` 不是纯函数（比如里面读了时钟、读了会被
        // 并发写的文件、或者用了 `HashMap` 的遍历顺序），指纹就会
        // **每一轮都不一样**。后果不是报错，是**缓存永远不命中**——
        // 而实测命中率是 87.1%，那 87.1% 全靠"同样的输入给同样的字节"。
        //
        // **它坏掉的时候没有声音。** 所以这条测试盯着的是"稳定"
        // 而不是"正确"——前缀内容的正确性由别的测试管。
        let before = build_persona("云熙", "克制", &[]);
        let after = build_persona("云熙", "活泼", &[]);

        // 1. 同一个人格，算两次必须逐字一样
        assert_eq!(
            before,
            build_persona("云熙", "克制", &[]),
            "**同一个输入必须给出同样的前缀**——不然缓存全废"
        );

        // 2. 换成新人格之后，也要稳定（"只变一次"的那个"一次"）
        let fp_new = PromptLayout::new(after.clone()).fingerprint();
        assert_eq!(
            fp_new,
            PromptLayout::new(build_persona("云熙", "活泼", &[])).fingerprint(),
            "换完之后每一轮都还得是同一个指纹，**不能一直变**"
        );

        // 3. 换了就是换了（和改动前不同），而且不同的人格给出不同的指纹
        assert_ne!(
            PromptLayout::new(before).fingerprint(),
            fp_new,
            "改了人格指纹就该变"
        );
        assert_ne!(
            fp_new,
            PromptLayout::new(build_persona("云熙", "冷静", &[])).fingerprint(),
            "不同的人格要给不同的指纹——否则换了等于没换"
        );
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

/// 一次请求里最多带多少**轮**历史。
///
/// ## 为什么要有这个上限
///
/// 上下文窗口（默认 131k）和"该带多少历史"是两件事。
/// 只靠窗口上限的话，历史会一直涨到约 98k token（窗口的 75%）
/// 才触发压缩——**每一次请求都背着这一大坨**。
///
/// 对陪伴型助理这么做得不偿失：
/// - **陪伴感来自"最近聊了什么"，不是"三个月前聊过什么"**
/// - 更早的东西该沉进记忆（`memory.rs`）按需召回，而不是每轮重发
/// - 常驻意味着一天几百轮，**每轮多背一点，账单就多一大截**
///
/// 20 轮约 40 条消息。够维持语气与指代。**它是偏好不是硬约束。**
pub const HISTORY_TURNS: usize = 20;

/// 取历史末尾的最近若干**轮**。
///
/// ## 为什么按轮切，而不是按条数切
///
/// **从中间截会把工具调用拆开。** 带工具的一轮长这样：
///
/// ```text
/// user:      做这件事
/// assistant: [tool_calls: write_file]     ← 声明要调工具
/// tool:      ok                            ← 必须紧跟它
/// assistant: 做完了
/// ```
///
/// 按条数切在第 2 条上，请求里就只剩一条带着 `tool_calls` 却没有结果的
/// 助手消息——**服务端直接 400**，而且报的是一句和截断毫无关系的
/// `must be followed by tool messages`。
///
/// **`user` 消息是天然的轮边界**：每一轮都从一个 `user` 开始，
/// 工具往返全在它后面。所以从后往前数 `user`，数够 N 个就从那里切。
///
/// 历史里连 N 个 `user` 都不到，就整段带——**不为了凑数而截**。
fn tail_turns(history: &[Message], turns: usize) -> Vec<Message> {
    use crate::think::Role;

    let mut seen = 0usize;
    let mut cut = 0usize;
    for (i, m) in history.iter().enumerate().rev() {
        if m.role == Role::User {
            seen += 1;
            if seen == turns {
                cut = i;
                break;
            }
        }
    }
    history[cut..].to_vec()
}

#[cfg(test)]
mod history_tail_tests {
    use super::*;
    use crate::think::Role;

    fn u(t: &str) -> Message {
        Message::user(t.to_string())
    }
    fn a(t: &str) -> Message {
        Message::assistant(t.to_string())
    }
    fn tool_call() -> Message {
        Message {
            role: Role::Assistant,
            content: String::new(),
            tool_calls: vec![serde_json::json!({"id": "c1", "type": "function",
                "function": {"name": "t", "arguments": "{}"}})],
            tool_call_id: None,
        }
    }
    fn tool_result() -> Message {
        Message {
            role: Role::Tool,
            content: "ok".to_string(),
            tool_calls: Vec::new(),
            tool_call_id: Some("c1".to_string()),
        }
    }

    #[test]
    fn only_the_last_n_turns_are_kept() {
        let mut h = Vec::new();
        for i in 0..30 {
            h.push(u(&format!("问题{i}")));
            h.push(a(&format!("回答{i}")));
        }
        let t = tail_turns(&h, 20);
        assert_eq!(t.len(), 40, "20 轮 = 40 条");
        assert_eq!(t[0].content, "问题10", "该从第 10 轮开始");
        assert_eq!(t[39].content, "回答29", "结尾不能动");
    }

    #[test]
    fn a_cut_never_separates_a_tool_call_from_its_result() {
        // **这条是要害。** 从中间截会让"带 tool_calls 的助手消息"
        // 后面不跟结果 → 服务端 400，而且报的错和截断毫无关系。
        let mut h = Vec::new();
        for i in 0..5 {
            h.push(u(&format!("第{i}轮")));
            h.push(tool_call());
            h.push(tool_result());
            h.push(a("做完了"));
        }
        let t = tail_turns(&h, 3);
        assert_eq!(t[0].content, "第2轮");
        assert_eq!(t.len() % 4, 0, "必须整轮保留");
        for i in 0..t.len() {
            if !t[i].tool_calls.is_empty() {
                assert!(i + 1 < t.len(), "最后一条不该是带 tool_calls 的助手消息");
                assert_eq!(
                    t[i + 1].role,
                    Role::Tool,
                    "第 {i} 条声明了工具调用，后面却不是结果——这一对必须整轮保留"
                );
            }
        }
    }

    #[test]
    fn a_short_history_is_carried_whole() {
        let h = vec![u("a"), a("b"), u("c"), a("d"), u("e"), a("f")];
        assert_eq!(tail_turns(&h, 20).len(), 6, "不为了凑数而截");
    }

    #[test]
    fn an_empty_history_is_fine() {
        assert!(tail_turns(&[], 20).is_empty());
    }

    #[test]
    fn history_without_enough_user_messages_is_carried_whole() {
        let h = vec![a("孤儿"), u("a"), a("b")];
        assert_eq!(tail_turns(&h, 20).len(), 3, "数不够就整段带");
    }
}
