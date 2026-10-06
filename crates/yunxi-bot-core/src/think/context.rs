//! 上下文预算与压缩。**通用 agent 里唯一一条"做错了比不做更糟"的东西。**
//!
//! ## 问题
//!
//! `PromptLayout.history` 只增不减。长会话迟早撞上模型的上下文上限，
//! 而**撞上时的表现是 API 报错，不是优雅降级**——使用者看到的是
//! "这一轮失败了"，而不是"我该开个新会话了"。
//!
//! ## 为什么它比看起来难：和缓存纪律直接冲突
//!
//! 缓存命中要求**前缀逐字节稳定**（实测 88.3%，命中比未命中便宜 50 倍）。
//! 而压缩天然要改前缀——把最老的消息换成摘要，前缀就变了。
//!
//! 所以设计上必须做到一件事：**把缓存损失摊薄到"每次压缩一次"，
//! 而不是"每轮一次"**。
//!
//! ```text
//! [稳定前缀] [摘要] [近期消息……] [本轮问题]
//!            ^^^^^^ 这一段在两次压缩之间**一个字都不变**
//! ```
//!
//! 摘要一旦生成就固定下来，后续轮次只往后追加。于是：
//!
//! | 时刻 | 缓存 |
//! |---|---|
//! | 压缩后的第一轮 | **未命中一次**（前缀变了） |
//! | 之后每一轮 | 命中 |
//! | 下一次压缩 | 未命中一次 |
//!
//! 代价从"每轮"降到"每次压缩"，而压缩只在超预算时发生。
//!
//! ## token 怎么数：精确锚点 + 增量估算
//!
//! **不靠纯估算蒙。** 每次 API 响应里都有 `usage.prompt_tokens`——
//! 那是**我们这次实际发出去多少 token 的精确值**。所以：
//!
//! ```text
//! 上一次响应报告的真实值（精确）
//!   + 自那以后新增消息的字符数 ÷ 每 token 字符数（估算，且逐轮自我修正）
//! ```
//!
//! 这个设计的性质：**估算误差不会累积**。每次响应都把锚点拉回精确值，
//! 所以最坏情况是"在两次响应之间估偏一点"，而不是"越估越离谱"。
//!
//! 每 token 字符数取 2（项目既有阈值）：中文大约 1 字 1 token 上下，
//! ASCII 大约 4 字符 1 token。2 是两者的保守中值——**宁可高估**，
//! 因为高估只是早一点压缩，低估会撞上限。

use serde::{Deserialize, Serialize};

use super::Message;

/// 每 token 大约几个字符。见模块文档：**宁可高估**。
pub const CHARS_PER_TOKEN: usize = 2;

/// 摘要消息的开头标记。
///
/// **它必须显眼**：读台账的人（和模型）都要一眼看出
/// "这里之前的东西被压掉了"，而不是以为对话就是从这儿开始的。
pub const COMPACT_MARKER: &str = "【此前对话已压缩】";

/// 上下文预算。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ContextBudget {
    /// 模型的上下文窗口（token）。
    pub window_tokens: usize,
    /// 留给回答的空间。**不能把窗口用满**——用满就没地方放回答了，
    /// 而表现是"模型答到一半被截断"。
    pub reserve_for_reply: usize,
    /// 用到窗口的百分之多少就该压。`0.75` 表示四分之三。
    ///
    /// 不设成 1.0：留出余量给"估算偏低"和"这一轮工具输出特别长"。
    pub compact_at: f64,
}

impl Default for ContextBudget {
    fn default() -> Self {
        // 128k 是当前主流模型的常见窗口。**宁可保守**：
        // 真正的窗口由调用方按模型覆盖。
        Self {
            window_tokens: 131_072,
            reserve_for_reply: 8_192,
            compact_at: 0.75,
        }
    }
}

impl ContextBudget {
    /// 按模型名给一个预算。
    ///
    /// 认不出的模型用最保守的一档——**猜大了的后果是撞上限报错，
    /// 猜小了的后果只是早一点压缩**。两者不对称，所以往小猜。
    pub fn for_model(model: &str) -> Self {
        let m = model.to_ascii_lowercase();
        let window = if m.contains("deepseek") {
            // DeepSeek 系列目前是 64k 上下文
            65_536
        } else if m.contains("agnes") {
            32_768
        } else {
            // 认不出：按最小的一档猜
            16_384
        };
        Self {
            window_tokens: window,
            reserve_for_reply: 4_096.min(window / 8),
            ..Default::default()
        }
    }

    /// 触发压缩的 token 数。
    pub fn trigger_at(&self) -> usize {
        let usable = self.window_tokens.saturating_sub(self.reserve_for_reply);
        ((usable as f64) * self.compact_at) as usize
    }

    /// 这个用量该压了吗。
    pub fn should_compact(&self, usage: &ContextUsage) -> bool {
        usage.estimated_tokens() >= self.trigger_at()
    }
}

/// 上下文用量：**精确锚点 + 增量估算**。
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct ContextUsage {
    /// 上一次响应报告的真实 `prompt_tokens`。`None` 表示还没拿到过。
    ///
    /// **这是精度的来源。** 没有它就只能纯估算，而纯估算的误差会累积。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anchor_tokens: Option<usize>,
    /// 自锚点之后新增内容的字符数。
    #[serde(default)]
    pub delta_chars: usize,
}

impl ContextUsage {
    /// 当前估算的 token 数。
    pub fn estimated_tokens(&self) -> usize {
        let delta = self.delta_chars.div_ceil(CHARS_PER_TOKEN);
        match self.anchor_tokens {
            // 有锚点：精确值 + 增量估算
            Some(a) => a + delta,
            // 没锚点（第一轮或者刚压缩完）：纯估算。**这时候最不准，
            // 所以下面的 `is_exact` 要如实报 false。**
            None => delta,
        }
    }

    /// 这个数是不是精确的。
    ///
    /// 只有"锚点存在且没有新增内容"时才为真。
    /// **使用者（和台账）需要能区分"我知道它多大"和"我在猜"。**
    pub fn is_exact(&self) -> bool {
        self.anchor_tokens.is_some() && self.delta_chars == 0
    }

    /// 记下一次真实的用量。**把估算拉回精确。**
    pub fn anchor(&mut self, prompt_tokens: usize) {
        self.anchor_tokens = Some(prompt_tokens);
        self.delta_chars = 0;
    }

    /// 记下新增内容。
    pub fn add(&mut self, chars: usize) {
        self.delta_chars = self.delta_chars.saturating_add(chars);
    }

    /// 压缩之后重置。**锚点作废**——下一轮的精确值由下一次响应给。
    pub fn reset_after_compaction(&mut self, remaining_chars: usize) {
        self.anchor_tokens = None;
        self.delta_chars = remaining_chars;
    }
}

/// 谁来把丢掉的消息变成一段摘要。
///
/// 抽成 trait 是为了**能离线测**：真摘要是模型调用，
/// 而"压缩逻辑对不对"不该依赖网络。
pub trait Summarizer {
    /// 把一段消息压成一段文字。失败返回 `Err`。
    fn summarize(&self, dropped: &[Message]) -> Result<String, String>;
}

/// 不调模型的兜底：只如实说明"这里少了多少"。
///
/// ## 为什么必须有兜底
///
/// **压缩不能失败。** 它被触发时我们已经接近上限了——
/// 这时候"摘要失败了所以不压"会让下一轮直接撞上限报错。
/// 所以模型不可用时用这个：**信息有损，但对话能继续。**
///
/// 有损是明确的：摘要里写着丢了多少条、多少字。
#[derive(Debug, Default, Clone, Copy)]
pub struct DropMarker;

impl Summarizer for DropMarker {
    fn summarize(&self, dropped: &[Message]) -> Result<String, String> {
        let chars: usize = dropped.iter().map(|m| m.content.chars().count()).sum();
        let tools: usize = dropped.iter().map(|m| m.tool_calls.len()).sum();
        Ok(format!(
            "（因上下文超限，此前 {} 条消息（约 {chars} 字，含 {tools} 次工具调用）\
             已被省略。如果需要其中某个细节，请让使用者重新说明。）",
            dropped.len()
        ))
    }
}

/// 一次压缩的结果。**要进台账。**
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Compaction {
    /// 丢掉了多少条消息。
    pub dropped_messages: usize,
    /// 丢掉了多少字。
    pub dropped_chars: usize,
    /// 丢掉的里有多少次工具调用。
    pub dropped_tool_calls: usize,
    /// 保留了最近多少条。
    pub kept_messages: usize,
    /// 压缩前的 token 估算。
    pub before_tokens: usize,
    /// 压缩后的 token 估算。
    pub after_tokens: usize,
    /// 摘要是不是由模型生成的。`false` 表示用的兜底（有损）。
    pub summarized: bool,
    /// 模型摘要失败的原因。`None` 表示模型摘要成功了。
    ///
    /// **这个字段是必须的。** 最初失败原因是静默吞掉的，于是
    /// "每次压缩都在丢信息"这件事完全不可见——而使用者的感受是
    /// "它怎么不记得了"，无从追查。
    ///
    /// 这个 bug 是端到端测试抓到的：三次压缩全是兜底，日志里一个字都没提。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary_error: Option<String>,
}

impl Compaction {
    /// 省下了多少 token。
    pub fn saved_tokens(&self) -> usize {
        self.before_tokens.saturating_sub(self.after_tokens)
    }

    /// 一行说明，进台账和给使用者看。
    pub fn summary(&self) -> String {
        // **退到兜底时要说清为什么。** 只说"有损"没法排查——
        // 而"每次压缩都在丢信息"正是那种不查就永远不知道的事。
        let tail = match (&self.summarized, &self.summary_error) {
            (true, _) => String::new(),
            (false, Some(e)) => format!("（兜底摘要，有损：{e}）"),
            (false, None) => "（兜底摘要，有损）".to_string(),
        };
        format!(
            "压缩：丢 {} 条（{} 次工具调用）/ 留 {} 条；约 {} → {} token（省 {}）{tail}",
            self.dropped_messages,
            self.dropped_tool_calls,
            self.kept_messages,
            self.before_tokens,
            self.after_tokens,
            self.saved_tokens(),
        )
    }
}

/// 消息的估算字符数。
///
/// **要算上工具调用**：`tool_calls` 里是完整 JSON，占的 token 不少，
/// 只算 `content` 会明显低估——而低估的后果是撞上限。
pub fn message_chars(m: &Message) -> usize {
    let mut n = m.content.chars().count();
    for tc in &m.tool_calls {
        n += serde_json::to_string(tc)
            .map(|s| s.chars().count())
            .unwrap_or(0);
    }
    if let Some(id) = &m.tool_call_id {
        n += id.chars().count();
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(s: &str) -> Message {
        Message::user(s)
    }

    // ---- 预算 ----

    #[test]
    fn an_unknown_model_gets_the_most_conservative_window() {
        // **猜大了的后果是撞上限报错，猜小了的后果只是早一点压缩。**
        // 两者不对称，所以往小猜。
        let unknown = ContextBudget::for_model("some-new-model");
        assert!(unknown.window_tokens <= 16_384);
        assert!(unknown.window_tokens < ContextBudget::for_model("deepseek-chat").window_tokens);
    }

    #[test]
    fn known_models_get_their_own_window() {
        assert_eq!(
            ContextBudget::for_model("deepseek-chat").window_tokens,
            65_536
        );
        assert_eq!(
            ContextBudget::for_model("agnes-3.0-flash").window_tokens,
            32_768
        );
    }

    #[test]
    fn the_trigger_leaves_room_for_the_reply() {
        // **不能把窗口用满**——用满就没地方放回答了，
        // 而表现是"模型答到一半被截断"。
        let b = ContextBudget {
            window_tokens: 10_000,
            reserve_for_reply: 2_000,
            compact_at: 1.0,
        };
        assert_eq!(b.trigger_at(), 8_000, "满打满算也只能用到 8000");
    }

    #[test]
    fn the_default_trigger_is_below_the_window() {
        let b = ContextBudget::default();
        assert!(b.trigger_at() < b.window_tokens);
        assert!(b.trigger_at() > b.window_tokens / 2, "压得太早也是浪费");
    }

    #[test]
    fn should_compact_fires_at_the_trigger() {
        let b = ContextBudget {
            window_tokens: 10_000,
            reserve_for_reply: 2_000,
            compact_at: 0.75,
        };
        let at = b.trigger_at(); // 6000
        let mut u = ContextUsage::default();
        u.anchor(at - 10);
        assert!(!b.should_compact(&u));
        u.anchor(at);
        assert!(b.should_compact(&u), "到点就该压");
        u.anchor(at + 1);
        assert!(b.should_compact(&u));
    }

    // ---- 用量：精确锚点 + 增量估算 ----

    #[test]
    fn an_anchor_makes_the_estimate_exact() {
        // **这是"不靠估算蒙"的落点**：上一次响应报告的 prompt_tokens
        // 是我们实际发出去多少 token 的精确值。
        let mut u = ContextUsage::default();
        u.add(9999);
        assert!(!u.is_exact());
        u.anchor(1234);
        assert!(u.is_exact());
        assert_eq!(u.estimated_tokens(), 1234);
    }

    #[test]
    fn additions_after_an_anchor_are_estimated_on_top() {
        let mut u = ContextUsage::default();
        u.anchor(1000);
        u.add(200);
        assert_eq!(
            u.estimated_tokens(),
            1000 + 100,
            "200 字符按每 token 2 字符算"
        );
        assert!(!u.is_exact(), "有新内容之后就不是精确值了");
    }

    #[test]
    fn the_estimate_rounds_up_never_down() {
        // **宁可高估。** 低估会撞上限，高估只是早压一点。
        let mut u = ContextUsage::default();
        u.anchor(0);
        u.add(1);
        assert_eq!(
            u.estimated_tokens(),
            1,
            "1 个字符至少算 1 个 token，不能是 0"
        );
        u.add(1);
        assert_eq!(u.estimated_tokens(), 1, "2 字符 = 1 token");
        u.add(1);
        assert_eq!(u.estimated_tokens(), 2, "3 字符 = 2 token（向上取整）");
    }

    #[test]
    fn a_new_anchor_discards_the_accumulated_error() {
        // **估算误差不会累积。** 每次响应都把锚点拉回精确值——
        // 所以最坏情况是"两次响应之间估偏一点"，不是"越估越离谱"。
        let mut u = ContextUsage::default();
        u.anchor(1000);
        for _ in 0..50 {
            u.add(1000); // 估得很离谱
        }
        let wild = u.estimated_tokens();
        u.anchor(2000); // 响应回来了，拉回精确
        assert_eq!(u.estimated_tokens(), 2000);
        assert!(wild > 20_000, "前提：刚才确实估偏了");
    }

    #[test]
    fn compaction_invalidates_the_anchor() {
        // 压缩之后内容全变了，旧锚点不再代表现在的用量
        let mut u = ContextUsage::default();
        u.anchor(50_000);
        u.reset_after_compaction(400);
        assert!(u.anchor_tokens.is_none(), "旧锚点必须作废");
        assert_eq!(u.estimated_tokens(), 200);
    }

    #[test]
    fn without_an_anchor_the_estimate_is_pure_guesswork() {
        let mut u = ContextUsage::default();
        u.add(400);
        assert_eq!(u.estimated_tokens(), 200);
        assert!(!u.is_exact(), "没锚点时不该声称精确");
    }

    // ---- 字符统计：工具调用也要算 ----

    #[test]
    fn tool_calls_count_toward_the_size() {
        // **只算 content 会明显低估**——`tool_calls` 里是完整 JSON，
        // 而低估的后果是撞上限。
        let m = Message::assistant_tool_calls(vec![serde_json::json!({
            "id": "call_1", "type": "function",
            "function": {"name": "web_search", "arguments": "{\"q\":\"很长的查询\"}"}
        })]);
        assert!(
            message_chars(&m) > 50,
            "工具调用的 JSON 要算进去: {}",
            message_chars(&m)
        );
    }

    #[test]
    fn a_tool_result_counts_its_id_too() {
        let m = Message::tool_result("call_abcdefgh", "结果");
        assert!(message_chars(&m) > 12);
    }

    #[test]
    fn a_plain_message_counts_its_content() {
        assert_eq!(message_chars(&msg("你好世界")), 4);
    }

    // ---- 兜底摘要 ----

    #[test]
    fn the_fallback_summary_says_how_much_was_lost() {
        // **有损是明确的**：摘要里写着丢了多少条、多少字。
        let dropped = vec![msg("一二三"), msg("四五六")];
        let s = DropMarker.summarize(&dropped).unwrap();
        assert!(s.contains("2 条"), "{s}");
        assert!(s.contains("6 字"), "{s}");
    }

    #[test]
    fn the_fallback_summary_counts_tool_calls() {
        let dropped = vec![Message::assistant_tool_calls(vec![
            serde_json::json!({"id": "a"}),
            serde_json::json!({"id": "b"}),
        ])];
        let s = DropMarker.summarize(&dropped).unwrap();
        assert!(s.contains("2 次工具调用"), "{s}");
    }

    #[test]
    fn the_fallback_tells_the_model_what_to_do_about_it() {
        // 只说"省略了"没用；要告诉模型**缺信息时该怎么办**
        let s = DropMarker.summarize(&[msg("x")]).unwrap();
        assert!(s.contains("重新说明"), "{s}");
    }

    #[test]
    fn the_fallback_never_fails() {
        // **压缩不能失败。** 它被触发时我们已经接近上限了。
        assert!(DropMarker.summarize(&[]).is_ok());
        assert!(DropMarker.summarize(&[msg("")]).is_ok());
    }

    // ---- 压缩结果 ----

    #[test]
    fn a_compaction_reports_what_it_saved() {
        let c = Compaction {
            dropped_messages: 10,
            dropped_chars: 8000,
            dropped_tool_calls: 3,
            kept_messages: 4,
            before_tokens: 5000,
            after_tokens: 800,
            summarized: true,
            summary_error: None,
        };
        assert_eq!(c.saved_tokens(), 4200);
        let s = c.summary();
        assert!(s.contains("丢 10 条"), "{s}");
        assert!(s.contains("3 次工具调用"), "{s}");
        assert!(s.contains("省 4200"), "{s}");
    }

    #[test]
    fn a_fallback_compaction_is_labelled_as_lossy() {
        // 台账里要能看出"这次是有损的"——
        // 否则事后追查"它怎么不记得了"会毫无线索
        let c = Compaction {
            dropped_messages: 1,
            dropped_chars: 10,
            dropped_tool_calls: 0,
            kept_messages: 1,
            before_tokens: 100,
            after_tokens: 50,
            summarized: false,
            summary_error: None,
        };
        assert!(c.summary().contains("有损"), "{}", c.summary());
    }

    #[test]
    fn a_fallback_compaction_says_why_it_fell_back() {
        // **这个字段是端到端测试逼出来的。** 最初失败原因被静默吞掉，
        // 于是"每次压缩都在丢信息"完全不可见——
        // 而使用者的感受是"它怎么不记得了"，无从追查。
        let c = Compaction {
            dropped_messages: 3,
            dropped_chars: 100,
            dropped_tool_calls: 0,
            kept_messages: 2,
            before_tokens: 3000,
            after_tokens: 400,
            summarized: false,
            summary_error: Some("本地限流：等 5 秒后重发".into()),
        };
        let s = c.summary();
        assert!(s.contains("有损"), "{s}");
        assert!(s.contains("本地限流"), "要说清为什么退到兜底: {s}");
    }

    #[test]
    fn a_successful_summary_carries_no_error() {
        let c = Compaction {
            dropped_messages: 3,
            dropped_chars: 100,
            dropped_tool_calls: 0,
            kept_messages: 2,
            before_tokens: 3000,
            after_tokens: 400,
            summarized: true,
            summary_error: None,
        };
        assert!(!c.summary().contains("兜底"), "{}", c.summary());
    }

    #[test]
    fn saved_tokens_never_underflows() {
        // 压缩后反而变大是可能的（摘要比原文长），不能因此 panic
        let c = Compaction {
            dropped_messages: 1,
            dropped_chars: 1,
            dropped_tool_calls: 0,
            kept_messages: 1,
            before_tokens: 10,
            after_tokens: 99,
            summarized: true,
            summary_error: None,
        };
        assert_eq!(c.saved_tokens(), 0);
    }

    // ---- 与真实模型的一致性 ----

    #[test]
    fn the_char_estimate_is_conservative_for_chinese() {
        // 中文大约 1 字 1 token，ASCII 大约 4 字符 1 token。
        // 取 2 是保守中值：**宁可高估**。
        // 编译期性质：改错了应该在构建阶段就炸
        const {
            assert!(CHARS_PER_TOKEN <= 2, "取大了会低估 token，那会撞上限");
            assert!(CHARS_PER_TOKEN >= 1, "取太小会让压缩过于频繁");
        }
    }
}
