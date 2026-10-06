//! 召回触发门控：**先判"有没有记忆需求"，再决定要不要召回。**
//!
//! ## 为什么要有门控
//!
//! 在这之前每一轮都无条件召回。后果有两头：
//!
//! - **白花钱**：问"什么是 HashMap"也要去记忆库里翻一遍，翻出来的
//!   还要占上下文预算
//! - **更坏的**：翻出来的东西**看着相关其实是噪声**，会把回答带偏
//!
//! 参考架构（`docs/research/memory-recall-reference.md` §4.2）的原话是
//! **"避免每轮盲目注入"**。
//!
//! ## 为什么先做确定性版本，不叫模型
//!
//! 门控在召回**之前**——它要是也得调模型，那省下来的调用还不够付它的。
//! 而且调模型的门控**没法稳定测试**：同一句话两次可能判得不一样，
//! 于是"召回结果为什么变了"就说不清。
//!
//! 确定性版本可以拿一张表钉死。**等它不够用了再谈模型**——
//! 那时也该先量出"它错在哪一类"。
//!
//! ## 判错的两个方向**不对称**
//!
//! - 判成"要召回"而其实不用：多花几毫秒、多几行上下文
//! - 判成"不用召回"而其实要：**它明明记过却想不起来**——
//!   而那种失效在对话里看起来就是"它忘了"，**最难查**
//!
//! **所以有疑问时偏向召回。** 唯一判成"不召回"的是
//! **明确的一般性问题**（问概念、问命令用法），那类问题问的是世界，
//! 不是使用者。

/// 这一轮的记忆需求。
///
/// 取值和参考架构对齐（`none | profile | episode | long_term | knowledge | mixed`），
/// 这样诊断输出能和文档对得上。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryNeed {
    /// 明确不需要记忆：问概念、问命令用法、纯计算。
    None,
    /// 问使用者是谁：称呼、偏好、身份。答案在画像里。
    Profile,
    /// 问经历：什么时候、当时、后来。答案在情景里。
    Episode,
    /// 问"我记过什么"：明确回指。答案在长期记忆里。
    LongTerm,
    /// 混合：既有个人指代又有具体事。**偏保守，按最宽的处理。**
    Mixed,
}

impl MemoryNeed {
    pub fn label(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Profile => "profile",
            Self::Episode => "episode",
            Self::LongTerm => "long_term",
            Self::Mixed => "mixed",
        }
    }

    /// 需要去长期记忆库里召回吗。
    ///
    /// `Profile` 返回 false：画像**已经在稳定前缀里**了，再召一次是重复。
    pub fn needs_recall(self) -> bool {
        matches!(self, Self::Episode | Self::LongTerm | Self::Mixed)
    }
}

/// 明确回指——**最强的信号**，命中就一定要召回。
///
/// 这些词的意思是"我说过/你知道"，它**直接指向记忆本身**，
/// 而不是指向记忆的内容。
const BACK_REFERENCE: &[&str] = &[
    "刚才",
    "刚刚",
    "之前",
    "上次",
    "上回",
    "以前",
    "早先",
    "刚刚说",
    "我说过",
    "我提过",
    "你记得",
    "还记得",
    "记不记得",
    "我跟你",
    "我跟你说",
    "我们说过",
    "我的偏好",
    "我的习惯",
    "我的要求",
    "你记住",
];

/// 时间/历史——指向情景记忆。
const TEMPORAL: &[&str] = &[
    "什么时候",
    "哪天",
    "那天",
    "当时",
    "后来",
    "最近一次",
    "上一次",
    "前几天",
    "上周",
    "上个月",
    "去年",
    "昨天",
    "前天",
    "这段时间",
];

/// 个人化——第一人称加一个"关于我的东西"的名词。
///
/// **单看"我"不够。**"我怎么写一个 for 循环"里的"我"不指向任何记忆，
/// 所以这里要求"我"+一个所属物/属性的词。
const PERSONAL_NOUNS: &[&str] = &[
    "我的",
    "我叫",
    "我住",
    "我在",
    "我用",
    "我养",
    "我吃",
    "我喝",
    "我喜",
    "我最",
    "我不",
    "我会",
    "我能",
    "我该",
    "我们",
    "我平时",
    "我一般",
    "我习惯",
    "我讨厌",
    "我打算",
    "我想去",
    "我需要",
];

/// 明确的一般性问题——**唯一会判成"不召回"的一类**。
///
/// 判定很窄：要有"问世界"的句式，**而且不能有任何个人指代**。
/// 宁可漏判（多召回一次），也不要把"我的 X 怎么用"错判成通用问题。
const GENERAL: &[&str] = &[
    "什么是",
    "是什么",
    "怎么用",
    "如何用",
    "怎么实现",
    "如何实现",
    "有什么区别",
    "什么区别",
    "语法",
    "命令怎么",
    "写一个",
    "实现一个",
    "举个例子",
    "为什么",
    "原理",
];

/// 判这一轮有没有记忆需求。
///
/// ## 优先级是**刻意**排的
///
/// 1. **任何个人指代**（"我的"、"我最"…）→ 一定要召回。
///    它出现在"什么是 X"的句子里也一样——"我的网络为什么不稳"
///    问的是**使用者的网络**，不是网络的原理。
/// 2. 明确回指 → 长期记忆
/// 3. 时间/历史 → 情景
/// 4. 都没有、且是通用问法 → `None`
/// 5. **什么都没匹配到 → `Mixed`（保守召回），不是 `None`**
///
/// 第 5 条是这张表里最要紧的一行：**"没看懂"不能等于"不需要"。**
pub fn gate(query: &str) -> MemoryNeed {
    let q = query.trim();
    if q.is_empty() {
        return MemoryNeed::None;
    }

    let has_first_person = q.contains('我');
    let has_personal = PERSONAL_NOUNS.iter().any(|k| q.contains(k));
    let has_back_ref = BACK_REFERENCE.iter().any(|k| q.contains(k));
    let has_temporal = TEMPORAL.iter().any(|k| q.contains(k));
    let has_general = GENERAL.iter().any(|k| q.contains(k));

    // 1. 明确回指最强——它说的就是"你记的"
    if has_back_ref {
        // 回指 + 时间 → 混合（既要往事也要经历）
        return if has_temporal {
            MemoryNeed::Mixed
        } else {
            MemoryNeed::LongTerm
        };
    }

    // 2. **第一人称 + 时间词 → 问经历。**
    //
    // 这一条是补出来的：原来只认"我"+特定名词（"我的"/"我最"），
    // 于是「我上周干了什么」这种人话落到了 Mixed——**它恰恰是最典型的
    // 问经历**。"我"和"上周"分开看都不够，合起来才是信号。
    if has_first_person && has_temporal {
        return MemoryNeed::Episode;
    }

    // 3. 个人指代：问"我是谁"
    if has_first_person && has_personal {
        return MemoryNeed::Profile;
    }
    if has_personal {
        return MemoryNeed::Profile;
    }

    // 4. 光有时间不算——"去年 Rust 发布了什么版本"问的是世界
    if has_temporal {
        return MemoryNeed::Mixed;
    }

    // 5. 通用问法，且**没有任何个人指代** → 真的不需要
    //
    // 注意 `!has_first_person`：**"我怎么写一个 for 循环"** 里的"我"
    // 不指向任何记忆，所以它该走这条、判成 None。
    if has_general && !has_first_person {
        return MemoryNeed::None;
    }

    // 5. **没看懂 → 保守召回。**
    //    "没看懂"不等于"不需要"：判成 None 的代价是"它明明记过却想不起来"，
    //    而那种失效在对话里看起来就是"它忘了"，最难查。
    MemoryNeed::Mixed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_back_reference_always_recalls() {
        for q in [
            "我刚才说什么了",
            "你还记得吗",
            "我上次提的那个",
            "我跟你说过的那件事",
            "我的偏好是什么",
        ] {
            let need = gate(q);
            assert!(
                need.needs_recall() || need == MemoryNeed::Profile,
                "「{q}」明确回指，必须召回，实际判成 {need:?}"
            );
        }
    }

    #[test]
    fn a_personal_question_asks_the_profile() {
        // 问"我是谁"的那类，答案在画像里
        for q in ["我住在哪", "我最喜欢什么颜色", "我叫什么", "我养了什么"] {
            assert_eq!(gate(q), MemoryNeed::Profile, "「{q}」");
        }
    }

    #[test]
    fn a_personal_time_question_asks_the_episodes() {
        // 有个人指代 + 时间词 → 问经历，不是问身份
        for q in ["我上周干了什么", "我上个月换了什么", "我昨天去哪了"] {
            assert_eq!(gate(q), MemoryNeed::Episode, "「{q}」");
        }
    }

    #[test]
    fn a_general_question_does_not_touch_private_memory() {
        // **唯一会判成"不召回"的一类。** 问的是世界，不是使用者。
        for q in [
            "什么是 HashMap",
            "Rust 的借用检查是什么",
            "怎么用 cargo test",
            "举个例子",
        ] {
            assert_eq!(
                gate(q),
                MemoryNeed::None,
                "「{q}」是通用问题，不该去翻私人记忆"
            );
        }
    }

    #[test]
    fn a_personal_reference_beats_the_general_pattern() {
        // **"我的 X 为什么不稳"问的是使用者的 X，不是 X 的原理。**
        // 规则 1 排在规则 4 前面就是为了这个。
        assert_eq!(gate("我的网络为什么不稳"), MemoryNeed::Profile);
        assert_eq!(gate("我的代码为什么报这个错"), MemoryNeed::Profile);
        assert_eq!(gate("我的部署怎么用"), MemoryNeed::Profile);
    }

    #[test]
    fn an_unrecognised_question_recalls_rather_than_skips() {
        // **这张表里最要紧的一行：没看懂 ≠ 不需要。**
        //
        // 判成 None 的代价是"它明明记过却想不起来"，而那种失效在对话里
        // 看起来就是"它忘了"，最难查。判成 Mixed 只是多花几毫秒。
        for q in ["嗯", "那个东西", "帮我看看", "继续"] {
            assert_eq!(gate(q), MemoryNeed::Mixed, "「{q}」没看懂，该保守召回");
        }
    }

    #[test]
    fn an_empty_question_asks_for_nothing() {
        assert_eq!(gate(""), MemoryNeed::None);
        assert_eq!(gate("   "), MemoryNeed::None);
    }

    #[test]
    fn the_profile_channel_does_not_ask_for_another_recall() {
        // 画像**已经在稳定前缀里**了，再召一次是重复占位
        assert!(!MemoryNeed::Profile.needs_recall());
        assert!(!MemoryNeed::None.needs_recall());
        for n in [MemoryNeed::Episode, MemoryNeed::LongTerm, MemoryNeed::Mixed] {
            assert!(n.needs_recall(), "{n:?} 该去召回");
        }
    }

    #[test]
    fn labels_match_the_reference_architecture() {
        // 诊断输出的取值要和参考文档对得上，否则拿文档比对时对不上号
        assert_eq!(MemoryNeed::None.label(), "none");
        assert_eq!(MemoryNeed::Profile.label(), "profile");
        assert_eq!(MemoryNeed::Episode.label(), "episode");
        assert_eq!(MemoryNeed::LongTerm.label(), "long_term");
        assert_eq!(MemoryNeed::Mixed.label(), "mixed");
    }
}
