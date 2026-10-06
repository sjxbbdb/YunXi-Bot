//! 本地字符 n-gram 向量。**零依赖**：不引 embedding 服务，也不引向量库。
//!
//! ## 为什么不用真的 embedding 模型
//!
//! 真 embedding（几百维、跑一个模型）语义确实更好，但代价是：
//! 要么引一个模型进来（体积、加载时间、失败面），要么调外部服务
//! （延迟、限流、密钥、还要花钱）。**记忆库是几百到几千条的规模**，
//! 为它付那个代价不值。
//!
//! 字符 n-gram 在中文上表现相当好——中文的词边界本来就模糊，
//! 二元、三元字符组已经能抓住大部分语义线索。"不喜欢被叫亲"和
//! "别叫我亲"共享多个 n-gram，相似度会明显高于"住在杭州"。
//!
//! ## 为什么是"派生数据"——不存进台账
//!
//! 台账是**追加式**的、是系统的唯一事实来源。而向量是**算法的函数**：
//! 算法一改（这里现在就是 v1，一定会改），存进去的那些全成了陈的，
//! 而追加式日志**改不了**——只能再写一批迁移事件，越滚越脏。
//!
//! 所以：**每次从正文现算**。几百条的规模下这是亚毫秒的事，
//! 换来的是"算法可以随便改，没有迁移负担"。
//!
//! ## 有符号哈希
//!
//! 把 n-gram 哈希到桶里会撞。纯加法撞了会**系统性偏高**相似度
//! （不同的文本因为共享碰撞桶而显得像）。用哈希的某一位决定符号、
//! 加减各半，碰撞的期望贡献就抵消掉了——这是 hashing trick 的标准做法。

/// 算法版本。
///
/// 改算法时必须递增。**它的用处只有一个**：让"为什么这次召回变了"
/// 有个可查的锚点。向量本身不落盘，所以不需要迁移——这也是不落盘的好处。
pub const EMBEDDING_VERSION: u32 = 1;

/// 向量维度。
///
/// 256 是权衡：太小（64）碰撞多、区分度不够；太大（4096）算了半天
/// 收益也看不出来。记忆库的规模在几百到几千条，256 足够。
pub const DIMS: usize = 256;

/// 最长取到几元字符组。
///
/// 1 元抓单字（对中文有用，"亲"这个字本身就是信号）；
/// 2 元抓最常见的词；3 元抓短语。再往上收益递减，还会让
/// "同一句话换个说法"变得太不相似。
const MAX_NGRAM: usize = 3;

/// 把一段文本变成一个 L2 归一化的向量。
///
/// **归一化之后余弦相似度就是点积**——省掉每次除模长，
/// 也不用担心长短文本的量纲差异（长文本向量大，不归一化的话
/// 余弦算出来还是对的，但点积就不对了，容易写错）。
pub fn embed(text: &str) -> Vec<f32> {
    let mut v = vec![0.0f32; DIMS];
    let chars: Vec<char> = normalize(text).chars().collect();
    if chars.is_empty() {
        return v;
    }
    for n in 1..=MAX_NGRAM {
        if chars.len() < n {
            break;
        }
        for w in chars.windows(n) {
            let mut buf = String::with_capacity(n * 4);
            for c in w {
                buf.push(*c);
            }
            let h = fnv1a(buf.as_bytes());
            let idx = (h % DIMS as u64) as usize;
            // 用另一个位决定符号：加一半、减一半，碰撞的期望贡献抵消
            let sign = if (h >> 63) & 1 == 0 { 1.0 } else { -1.0 };
            v[idx] += sign;
        }
    }
    l2_normalize(&mut v);
    v
}

/// 词频统计：哪些 n-gram 在语料里到处都是。
///
/// ## 为什么需要它
///
/// 不带 IDF 的字符 n-gram 有个真问题：**常见的字会主导相似度**。
///
/// 测试抓到的实例：记忆库里大部分条目都以「使用者」开头。于是
/// 「使用者住在杭州」和「使用者不喜欢被叫亲」共享了很长的「使用者」，
/// 相似度 0.32；而真正该匹配的「别叫我亲」只共享两个字，只有 0.18。
/// **召回会把不相干的那条排前面。**
///
/// IDF 正是治这个的：一个 n-gram 在越多条记忆里出现，它越不能区分，
/// 权重就该越低。「使用者」出现在几乎每条里 → 权重接近 0；
/// 「青绿」只出现在一条里 → 权重高。
///
/// ## 只在有语料时用
///
/// 记忆库空着、或者只有一两条时，IDF 统计没有意义（一条记忆里
/// 每个 n-gram 的 df 都是 1）。所以 [`embed_plain`] 保留着——
/// 那种场景下用朴素版本，反而更稳。
#[derive(Debug, Clone, Default)]
pub struct Idf {
    df: std::collections::HashMap<u64, u32>,
    total: u32,
}

impl Idf {
    /// 从语料里统计。**调用方传的是所有记忆的正文。**
    pub fn fit<'a>(texts: impl IntoIterator<Item = &'a str>) -> Self {
        let mut df: std::collections::HashMap<u64, u32> = std::collections::HashMap::new();
        let mut total = 0u32;
        for t in texts {
            total += 1;
            let chars: Vec<char> = normalize(t).chars().collect();
            // 同一条里重复出现的 n-gram 只算一次 df——
            // 不然一条啰嗦的记忆会把自己那些词的权重压低
            let mut seen = std::collections::HashSet::new();
            for n in 1..=MAX_NGRAM {
                if chars.len() < n {
                    break;
                }
                for w in chars.windows(n) {
                    let mut buf = String::with_capacity(n * 4);
                    for c in w {
                        buf.push(*c);
                    }
                    let h = fnv1a(buf.as_bytes());
                    if seen.insert(h) {
                        *df.entry(h).or_insert(0) += 1;
                    }
                }
            }
        }
        Self { df, total }
    }

    /// 某个 n-gram 的 IDF。语料为空时返回 1（等于不用 IDF）。
    fn weight(&self, h: u64) -> f32 {
        if self.total == 0 {
            return 1.0;
        }
        let df = *self.df.get(&h).unwrap_or(&0) as f32;
        // 平滑过的 IDF：`ln((N+1)/(df+1)) + 1`。
        // **加 1 是为了不出负权重**——出现在每条里的 n-gram 权重会趋近 1
        // 而不是 0 或负数，那样它仍然保留一点点信息（"这条记忆里有这个词"），
        // 但不足以主导排序。
        ((self.total as f32 + 1.0) / (df + 1.0)).ln() + 1.0
    }
}

/// 带 IDF 权重的向量。
pub fn embed_with_idf(text: &str, idf: &Idf) -> Vec<f32> {
    let mut v = vec![0.0f32; DIMS];
    let chars: Vec<char> = normalize(text).chars().collect();
    if chars.is_empty() {
        return v;
    }
    for n in 1..=MAX_NGRAM {
        if chars.len() < n {
            break;
        }
        for w in chars.windows(n) {
            let mut buf = String::with_capacity(n * 4);
            for c in w {
                buf.push(*c);
            }
            let h = fnv1a(buf.as_bytes());
            let idx = (h % DIMS as u64) as usize;
            let sign = if (h >> 63) & 1 == 0 { 1.0 } else { -1.0 };
            v[idx] += sign * idf.weight(h);
        }
    }
    l2_normalize(&mut v);
    v
}

/// 不带 IDF 的向量。语料太小、统计没意义时用它。
pub fn embed_plain(text: &str) -> Vec<f32> {
    embed(text)
}

/// 余弦相似度。**输入必须是 [`embed`] 出来的归一化向量。**
///
/// 归一化之后就是点积。这里不做除法——做了的话每次比较都多两次开方，
/// 而且掩盖了"传进来的没归一化"这个错误。
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    debug_assert_eq!(a.len(), b.len(), "维度不同的向量不该拿来比");
    a.iter().zip(b.iter()).map(|(x, y)| x * y).sum()
}

/// 归一化文本：小写、**去掉所有空白**。
///
/// ## 为什么空白是"去掉"而不是"折成一个空格"
///
/// 一开始折成了一个空格，结果"住在 杭州"和"住在杭州"算出来差很远
/// （测试抓到了）。原因：折成空格之后 `windows(3)` 拿到的是
/// `在␣杭`，而原文里是 `在杭`——**一个空格把 n-gram 切断了**。
///
/// 而中文里空格本来就不是词边界，人是随手打的。对英文来说去掉空格
/// 会多出一些跨词 n-gram（"ow"、"lwo"），但那些是噪声、量也小，
/// 比"中文里一个空格毁掉整条记忆的召回"划算得多。
///
/// 这个取舍是**面向中文使用者**做的。以后要支持英文为主的语料，
/// 该改成"只在 CJK 与拉丁之间保留分隔"。
fn normalize(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if c.is_whitespace() {
            continue;
        }
        for lc in c.to_lowercase() {
            out.push(lc);
        }
    }
    out
}

fn l2_normalize(v: &mut [f32]) {
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > f32::EPSILON {
        for x in v.iter_mut() {
            *x /= norm;
        }
    }
}

/// FNV-1a。**自己写而不是用 `DefaultHasher`**：
///
/// `DefaultHasher` 的文档明确说"不保证跨 Rust 版本一致"。向量虽然不落盘，
/// 但**测试会断言具体相似度关系**——底层哈希一换，那些断言就会无缘无故
/// 地飘。自己写一个 20 行的、行为钉死的哈希，比依赖标准库的"不保证"省心。
const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h = FNV_OFFSET;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(FNV_PRIME);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_vector_is_normalized() {
        // 归一化之后余弦才是点积。没归一化的话下面那些相似度全不对。
        let v = embed("使用者住在杭州，喜欢简洁的回答");
        let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-4, "模长该是 1，实际 {norm}");
    }

    #[test]
    fn the_same_text_gives_the_same_vector() {
        // **纯函数**：同样的输入永远同样的输出。否则召回结果会飘。
        let a = embed("住在杭州");
        let b = embed("住在杭州");
        assert_eq!(a, b);
    }

    #[test]
    fn empty_text_gives_a_zero_vector_rather_than_panicking() {
        let v = embed("");
        assert_eq!(v.len(), DIMS);
        assert!(v.iter().all(|x| *x == 0.0));
        // 零向量和谁都不相似——而不是"和谁都像"
        assert_eq!(cosine(&v, &embed("随便什么")), 0.0);
    }

    #[test]
    fn whitespace_and_case_do_not_change_much() {
        // "住在 杭州" 和 "住在杭州" 人看起来是同一句，
        // 向量也该几乎一样
        let a = embed("住在杭州");
        let b = embed("住在  杭州");
        assert!(cosine(&a, &b) > 0.95, "空白差异不该有这么大影响");
        // 大小写同理
        let c = embed("Rust 写的");
        let d = embed("rust 写的");
        assert!(cosine(&c, &d) > 0.95);
    }

    /// 一小撮真实形状的记忆：**大部分都以「使用者」开头**——
    /// 这正是 IDF 要对付的那种语料。
    fn corpus() -> Vec<&'static str> {
        vec![
            "使用者最喜欢的颜色是青绿色",
            "使用者不喜欢被叫亲",
            "使用者住在杭州",
            "使用者不吃香菜",
            "使用者用的是 Windows",
            "这个项目用 cargo test 跑测试",
            "上周和客户开了个会",
        ]
    }

    #[test]
    fn without_idf_a_common_prefix_dominates() {
        // **记录一个真实的限制，而不是假装它不存在。**
        //
        // 不带 IDF 时，语料里到处都是的「使用者」会主导相似度：
        // 「使用者住在杭州」和「使用者不喜欢被叫亲」共享很长的公共前缀，
        // 分数反而比真正近义的「别叫我亲」高。**召回会把不相干的排前面。**
        //
        // 这条测试**故意断言这个坏行为**——它是 IDF 存在的理由。
        // 哪天底层换了，这条会挂，而挂的时候正好提醒：IDF 那条路还成不成立。
        let pref = embed_plain("使用者不喜欢被叫亲");
        let near = embed_plain("别叫我亲");
        let far = embed_plain("使用者住在杭州");
        assert!(
            cosine(&pref, &far) > cosine(&pref, &near),
            "这个限制还在的话，公共前缀该压过近义——变了就说明该重新看 IDF 的必要性"
        );
    }

    #[test]
    fn with_idf_the_paraphrase_wins() {
        // **这是整个 RAG 能不能用的核心断言。**
        //
        // 有了 IDF，「使用者」在几乎每条里都出现 → 权重趋近 1（可有可无），
        // 而「叫」「亲」只在少数条里出现 → 权重高。于是真正近义的
        // 「别叫我亲」应当胜出。
        let c = corpus();
        let idf = Idf::fit(c.iter().copied());
        let pref = embed_with_idf("使用者不喜欢被叫亲", &idf);
        let near = embed_with_idf("别叫我亲", &idf);
        let far = embed_with_idf("使用者住在杭州", &idf);
        let s_near = cosine(&pref, &near);
        let s_far = cosine(&pref, &far);
        assert!(
            s_near > s_far,
            "**加了 IDF 之后近义必须胜出**：「别叫我亲」{s_near} 该大于「住在杭州」{s_far}"
        );
    }

    #[test]
    fn idf_agrees_with_plain_when_the_corpus_is_empty() {
        // 语料空着（还没记过任何记忆）时，IDF 统计没意义。
        // 这时必须退化成朴素版本，而不是算出个乱七八糟的东西。
        let idf = Idf::fit(std::iter::empty());
        let a = embed_with_idf("住在杭州", &idf);
        let b = embed_plain("住在杭州");
        assert_eq!(a, b, "没语料时 IDF 版本该和朴素版本一致");
    }

    #[test]
    fn a_query_matches_the_memory_that_answers_it() {
        // 真实场景的形状：使用者问一句话，该把它对应的那条记忆挑出来
        let q = embed("我最喜欢什么颜色");
        let color = embed("使用者最喜欢的颜色是青绿色");
        let food = embed("使用者不吃香菜");
        let city = embed("使用者在杭州工作");
        assert!(
            cosine(&q, &color) > cosine(&q, &food),
            "颜色那条该比香菜那条像"
        );
        assert!(
            cosine(&q, &color) > cosine(&q, &city),
            "颜色那条该比城市那条像"
        );
    }

    #[test]
    fn collisions_do_not_make_everything_look_similar() {
        // **有符号哈希的意义在这里。** 全是加法的话，桶撞多了会让
        // 任意两条文本都有不低的基础相似度，"挑得准"就成了空话。
        let a = embed("完全不相干的一二三");
        let b = embed("另外四个五六个");
        assert!(
            cosine(&a, &b).abs() < 0.3,
            "不相干的文本相似度该接近 0，实际 {}",
            cosine(&a, &b)
        );
    }
}
