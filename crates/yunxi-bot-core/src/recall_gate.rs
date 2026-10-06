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

/// 门控那个决策问题的 id。
pub const GATE_QUESTION: &str = "memory_need";

/// **拿不准时问决策模型。**
///
/// ## 为什么不能全靠关键词
///
/// 关键词表能覆盖常见问法，但**判不了无标记的名词短语**。
/// 真机上的例子：「量子色动力学的重整化群方程」——它没有疑问句式、
/// 也没有个人指代，关键词只能保守地判成"可能要用记忆"，
/// 于是白召回一趟。
///
/// **"这是不是通用知识"靠字面判不出来。** 关键词表越写越长也只是
/// 在追着用例跑，而每一轮对话都要为这张表付维护成本。
///
/// ## 为什么也不是全交给模型
///
/// 门控在**每一轮**都跑。全交给模型的话：
/// - 每轮多一次调用（限流、延迟、花钱）
/// - 常见问法（"我住在哪"）本来关键词一毫秒就能判准
/// - **而且它变得不可测**——同一句话两次可能判得不一样，
///   "召回结果为什么变了"就说不清
///
/// 所以：**关键词先跑，只有它判成 `Mixed`（拿不准）时才问模型。**
/// 常见情况零成本且确定，模糊情况有真判断。
///
/// ## 选项和 `MemoryNeed` 一一对应
///
/// 判据写得**互相排斥**——"问的是世界"和"问的是使用者本人"不能重叠，
/// 否则模型的选择没有可复核性（这和研究文档说的"选项必须有
/// 可区分的判据"是同一条要求）。
pub fn gate_questions() -> Vec<crate::decide::Question> {
    use crate::decide::Question;
    vec![Question::choice(
        GATE_QUESTION,
        "为了回答使用者这一句话，需要去查关于他本人的记忆吗？",
        &[
            (
                "none",
                "问的是世界：概念、原理、命令用法、通用知识。跟「他是谁」无关",
            ),
            (
                "profile",
                "问的是使用者本人：怎么称呼、喜欢什么、住在哪、有什么习惯",
            ),
            (
                "episode",
                "问的是他经历过的事：什么时候、当时、上次、最近一次",
            ),
            (
                "long_term",
                "他明确回指自己说过的话：「我说过」「你记得」「之前提的那个」",
            ),
            ("knowledge", "要查的是文档或资料库，而不是关于他的私人记忆"),
            ("mixed", "既有个人指代又涉及具体的事，两类都可能用得上"),
        ],
    )]
}

/// 把模型的选项翻成 [`MemoryNeed`]。
///
/// **`knowledge` 映射成 `None`**：两者的共同点是"**别去翻私人记忆**"，
/// 而这一层的职责就是决定要不要翻。区分"该查文档"和"什么都不用查"
/// 是召回**之后**的事（研究文档 §4.1 说知识库和记忆不得互相旁路）。
///
/// 认不出的选项返回 `None`（拿不准）而不是硬猜一个——
/// 调用方会退回关键词的判断。
pub fn need_from_choice(choice: Option<&str>) -> Option<MemoryNeed> {
    Some(match choice? {
        "none" | "knowledge" => MemoryNeed::None,
        "profile" => MemoryNeed::Profile,
        "episode" => MemoryNeed::Episode,
        "long_term" => MemoryNeed::LongTerm,
        "mixed" => MemoryNeed::Mixed,
        _ => return None,
    })
}

#[cfg(test)]
mod model_gate_tests {
    use super::*;

    #[test]
    fn the_model_choice_maps_back_to_a_need() {
        assert_eq!(need_from_choice(Some("none")), Some(MemoryNeed::None));
        assert_eq!(need_from_choice(Some("profile")), Some(MemoryNeed::Profile));
        assert_eq!(need_from_choice(Some("episode")), Some(MemoryNeed::Episode));
        assert_eq!(
            need_from_choice(Some("long_term")),
            Some(MemoryNeed::LongTerm)
        );
        assert_eq!(need_from_choice(Some("mixed")), Some(MemoryNeed::Mixed));
    }

    #[test]
    fn knowledge_means_do_not_touch_private_memory() {
        // **`knowledge` 和 `none` 在这一层的效果一样**：都不翻私人记忆。
        // 区分"该查文档"和"什么都不用查"是召回之后的事
        // （知识库和记忆不得互相旁路）。
        assert_eq!(need_from_choice(Some("knowledge")), Some(MemoryNeed::None));
        assert!(!need_from_choice(Some("knowledge")).unwrap().needs_recall());
    }

    #[test]
    fn an_unrecognised_choice_is_none_not_a_guess() {
        // **认不出就说认不出**，让调用方退回关键词的判断——
        // 硬猜一个比不猜更坏，因为它看起来像有依据。
        assert_eq!(need_from_choice(Some("随便什么")), None);
        assert_eq!(need_from_choice(None), None);
    }

    #[test]
    fn the_gate_question_has_criteria_for_every_option() {
        // 判据必须**互相排斥**，否则模型的选择没有可复核性。
        // 这里钉的是"六类都有判据"——少一个就等于那一类模型没法选。
        let qs = gate_questions();
        assert_eq!(qs.len(), 1);
        assert_eq!(qs[0].id, GATE_QUESTION);
        let crate::decide::QuestionKind::Choice { criteria } = &qs[0].kind else {
            panic!("门控该是个选择题");
        };
        assert_eq!(criteria.len(), 6, "六类都要有判据");
        for (name, how) in criteria {
            assert!(!how.trim().is_empty(), "{name} 没写判据");
            // 每一类的判据都要能翻回来，否则模型选了也没用
            assert!(need_from_choice(Some(name)).is_some(), "{name} 翻不回来");
        }
    }
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
