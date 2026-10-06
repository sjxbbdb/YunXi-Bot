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

/// 三段式提示词布局。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PromptLayout {
    /// 稳定前缀：系统提示词 + 人格 + 工具定义。**跨调用必须字节一致。**
    stable: String,
    /// 追加式历史。只允许往后加，不允许改前面的。
    history: Vec<Message>,
    /// 易变尾部：当前要问的问题。
    volatile: String,
}

impl PromptLayout {
    /// 用稳定前缀开一个布局。
    pub fn new(stable: impl Into<String>) -> Self {
        Self {
            stable: stable.into(),
            history: Vec::new(),
            volatile: String::new(),
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
        self.history.push(Message::user(question));
        self.history.push(Message::assistant(assistant));
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
            self.history.push(Message::user(q));
        }
        self.history.push(msg);
    }

    /// 前缀的字符长度，用于估算 token 与成本。
    pub fn stable_len(&self) -> usize {
        self.stable.chars().count()
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
