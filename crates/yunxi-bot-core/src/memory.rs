//! 记忆层：为决策层提供可用的 state。
//!
//! ## 为什么记忆不能推迟到最后做
//!
//! ADR §2.2：**陪伴的温度来自「连续性」，不是来自「判断」。** 决策模型只能决定
//! "要不要介入"，但**凭什么亲近**是记忆与关系状态的产物。没有连续记忆，
//! 它的关心会很假——因为每次判断都缺少"我们之间的历史"这个输入。
//!
//! ## 设计
//!
//! 沿用台账的事件溯源，不另起存储：记忆的增删改都是台账事件，状态由投影得出。
//! 这样记忆天然可审计、可回溯、可重建。
//!
//! ## 与决策层的接口
//!
//! [`Memory::build_decision_state`] 负责把记忆**裁剪**成决策模型要的 state。
//! ADR §5.4 明确要求：只喂判断所需的证据，不放"服务知道的一切"，
//! 且**密钥与无关字段一律不进 state**。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::ledger::Event;

/// 记忆类别。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryKind {
    /// 关于使用者的事实（职业、习惯、正在做的事）。
    Fact,
    /// 明确表达的偏好（不喜欢被打扰的时段、沟通风格）。
    Preference,
    /// 关系状态（亲近程度、共同经历）。
    Relationship,
    /// 值得记住的事件。
    Event,
    /// **关于某个工作目录的记忆**（这个项目的约定、这个仓库的坑）。
    ///
    /// ## 为什么单独一类
    ///
    /// 因为它**有作用域**：`Fact` 是关于"这个人"的，在哪儿都成立；
    /// 而"这个项目用 pytest 不用 unittest"只在**那个目录下**成立。
    ///
    /// 混在一起会出两种错：
    /// - 在 B 项目里召回 A 项目的约定 → **按错的前提干活**
    /// - A 项目的路径进了常驻层 → 常驻层是**稳定前缀**，而 cwd
    ///   每次运行都可能变，**前缀一变缓存全废**
    ///
    /// 所以这一类的规则不一样：**不进常驻层**（`resident` 只取
    /// `Fact | Preference`，自动成立），**召回时按 cwd 筛**。
    Workspace,
}

impl MemoryKind {
    pub fn label(self) -> &'static str {
        match self {
            MemoryKind::Fact => "事实",
            MemoryKind::Preference => "偏好",
            MemoryKind::Relationship => "关系",
            MemoryKind::Event => "事件",
            MemoryKind::Workspace => "工作目录",
        }
    }

    /// 在 state 里的字段名。
    pub fn state_key(self) -> &'static str {
        match self {
            MemoryKind::Fact => "facts",
            MemoryKind::Preference => "preferences",
            MemoryKind::Relationship => "relationship",
            MemoryKind::Event => "recent_events",
            // **和 facts/preferences 分开**：它们进常驻层（稳定前缀），
            // 而工作目录记忆只在当前目录命中的那一轮进易变段。
            MemoryKind::Workspace => "workspace_notes",
        }
    }
}

/// 一条记忆。
/// 召回的最低相似度。低于它的当成"不相干"。
///
/// **这个数是量出来的，不是拍的。** 一开始定 0.15，结果好几条相关的
/// 查询直接返回空——因为 IDF 加权 + L2 归一化之后，余弦的整体量级
/// 比朴素版本小得多。
///
/// 宁可多召回一条不相干的，也别漏掉相关的：漏掉的表现是
/// "它明明记过却想不起来"，那比多一行噪声难查得多。
pub const RECALL_MIN_SCORE: f32 = 0.20;

/// 一条候选记忆在召回里的结局。
///
/// **"为什么没召回某条"必须能回答。** 参考实现
/// （`yunxi-agent-persona` 的 `MemoryRecallRoute`）也是这么分的——
/// 只返回选中项的话，排查时第一个问题就答不上来。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecallRoute {
    /// 选中了，会进提示词。
    Picked,
    /// 相似度太低，跟这句话不相干。
    DroppedUnrelated,
    /// 条数或字符预算用完了。
    DroppedBudget,
}

impl RecallRoute {
    pub fn label(self) -> &'static str {
        match self {
            Self::Picked => "召回",
            Self::DroppedUnrelated => "不相干",
            Self::DroppedBudget => "超出预算",
        }
    }
}

/// 这一条是从哪一路来的。
///
/// **可观测性要求**（研究文档 §4.7）：事后要能回答"它为什么被选中"。
/// 只报"选中了"不够——"词面命中"和"语义相似"是两种不同的证据，
/// 调不准的时候得知道该动哪一路。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecallVia {
    /// 词面和语义都认为相关——**最强的证据**。
    Both,
    /// 只有词面命中（精确的字对上了）。
    Lexical,
    /// 只有语义相似（意思像，但字不一样）。
    Semantic,
    /// 两路都没进（不相干）。
    None,
}

impl RecallVia {
    pub fn label(self) -> &'static str {
        match self {
            Self::Both => "词面+语义",
            Self::Lexical => "词面",
            Self::Semantic => "语义",
            Self::None => "-",
        }
    }
}

/// 一条召回候选。
#[derive(Debug)]
pub struct RecallHit<'a> {
    pub entry: &'a MemoryEntry,
    /// RRF 融合分。**不是相似度**——它只反映名次，不可当阈值用。
    pub score: f32,
    pub route: RecallRoute,
    pub via: RecallVia,
}

/// 被反复召回时的降权。
///
/// ## 为什么需要它
///
/// 参考实现（Miyu）里有个真实记录：一条"华硕 s2idle"的记忆被召回了
/// **238 次**——因为召回会加强它（`weight` 涨、`last_used_at` 刷新），
/// 而更强的它更容易被再次召回。**这是个正反馈，会自己滚起来。**
///
/// ## 为什么用对数
///
/// 前几次召回几乎无感（`ln(1+1)=0.69`、`ln(1+3)=1.39`），
/// 惩罚只在"远超同侪"时才显形。**线性惩罚会把正常的第二次召回
/// 也砍一半**，那太狠了。
///
/// ## 计数只在会话内
///
/// 每次召回都往台账写一条的话，台账会被召回记录淹没——而台账是
/// **审计线**，不该被这种东西占满。所以计数由调用方在会话内维护。
/// 跨会话的霸屏由 `weight` 和近因去管。
fn fatigue_penalty(times_recalled: Option<u32>) -> f32 {
    /// 在这个次数以内**完全不罚**。
    ///
    /// 一开始写的是 `1/(1+ln(1+n))`，结果召回**一次**就砍掉 41%
    /// （测试抓到了）——那不叫"前几次几乎无感"，那叫"召回过就该让位"。
    /// 正常的反复引用会被它误伤。
    const FREE_RECALLS: f32 = 5.0;
    let n = times_recalled.unwrap_or(0) as f32;
    if n <= FREE_RECALLS {
        return 1.0;
    }
    // 超过免费额度之后才按对数压：`ln(n/FREE)` 在 n 刚过线时接近 0，
    // 所以曲线是**连续**的，不会在第六次突然掉一截
    1.0 / (1.0 + (n / FREE_RECALLS).ln())
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryEntry {
    pub id: String,
    pub kind: MemoryKind,
    /// 记忆正文。**不得包含密钥。**
    pub text: String,
    /// 权重。被反复用到会增强，长期不用会衰减。
    pub weight: f64,
    pub created_at: u64,
    pub last_used_at: Option<u64>,
    /// 这条记忆属于哪个工作目录。**只有 `Workspace` 类会填它**——
    /// 关于"这个人"的记忆不该跟着目录走。
    ///
    /// 存的是**规范化后的目录路径**，不是哈希。不用指纹是为了可排查：
    /// 台账里出现一个哈希，事后没人能看出它指哪个目录，
    /// 而"这条记忆为什么没被召回"恰恰是最常要回答的问题。
    ///
    /// `None` = **无作用域**，在哪儿都能召回。
    pub scope: Option<String>,
}

impl MemoryEntry {
    /// 排序用的有效分数：权重优先，同权重看新近度。
    fn score(&self, now_ms: u64) -> f64 {
        let age_days = (now_ms.saturating_sub(self.last_used_at.unwrap_or(self.created_at)) as f64)
            / 86_400_000.0;
        // 半衰期 30 天
        let recency = 0.5_f64.powf(age_days / 30.0);
        self.weight * (0.5 + 0.5 * recency)
    }
}

/// 记忆库。由台账事件投影得出。
#[derive(Debug, Default, Clone)]
pub struct Memory {
    entries: BTreeMap<String, MemoryEntry>,
}

/// 把工作目录归一成记忆作用域用的字符串。
///
/// ## 为什么必须只有一份
///
/// **写入（`remember`）和召回（`chat`）必须用完全一样的归一。**
/// 不一致的表现是"**记了但它从来不提**"——**它不报错，只是静默地不匹配**，
/// 而那是最难查的一种。
///
/// 这个 session 里已经因为"两份来源对不上"栽过好几次（D62 最典型：
/// 两条守卫各自检查了一份前缀，两条都没检查真正发出去的那份）。
/// 所以这段逻辑**只写一份**，两边都调它。
///
/// ## 归一方式
///
/// - 统一分隔符为 `\`（Windows 上 `current_dir()` 给的是 `\`，
///   但别的来源可能给 `/`）
/// - 去掉末尾分隔符
///
/// **不用 `Path::canonicalize`**：它会解析符号链接、而且**要求目录存在**。
/// 而这个函数要能在"目录已经不在了"的情况下照样算出同一个值——
/// 否则**昨天记的今天就召不回了**。
///
/// 边界：整个路径都是分隔符时（`/`），归一整之后会是空串，
/// 而空串会让"无作用域"和"根目录"混在一起。所以那种情况保留一个分隔符。
pub fn normalize_scope(path: &std::path::Path) -> String {
    let s = path.to_string_lossy().replace('/', "\\");
    let trimmed = s.trim_end_matches('\\');
    if trimmed.is_empty() {
        "\\".to_string()
    } else {
        trimmed.to_string()
    }
}

/// 记忆事件的数据形状。
#[derive(Debug, Clone, Serialize, Deserialize)]
struct RecordedPayload {
    kind: MemoryKind,
    text: String,
    #[serde(default = "one")]
    weight: f64,
    /// 这条记忆属于哪个工作目录。只有 `Workspace` 类会用。
    ///
    /// **`default` 是必须的**：加这个字段之前写下的台账里没有它，
    /// 而那些台账要能继续读——**记忆是长期资产，不能因为加了一个字段
    /// 就让旧记录读不出来。**
    #[serde(default)]
    scope: Option<String>,
}

fn one() -> f64 {
    1.0
}

impl Memory {
    /// 从台账事件投影出记忆库。
    ///
    /// 只认 `memory_*` 事件；其余一律忽略——这与权限状态投影同一套语义。
    pub fn from_events(events: &[Event]) -> Self {
        use crate::ledger::EventKind;
        let mut mem = Self::default();

        for e in events {
            match e.kind {
                EventKind::MemoryRecorded => {
                    let Ok(p) = serde_json::from_value::<RecordedPayload>(e.data.clone()) else {
                        continue;
                    };
                    let id = e
                        .data
                        .get("id")
                        .and_then(|v| v.as_str())
                        .unwrap_or(&e.job.clone().unwrap_or_default())
                        .to_string();
                    if id.is_empty() {
                        continue;
                    }
                    mem.entries.insert(
                        id.clone(),
                        MemoryEntry {
                            id,
                            kind: p.kind,
                            text: p.text,
                            weight: p.weight.clamp(0.0, 10.0),
                            created_at: e.at,
                            last_used_at: None,
                            // 只有 `Workspace` 类带作用域。别的类即使事件里
                            // 写了 scope 也丢掉——关于"这个人"的记忆
                            // 不该跟着目录走。
                            scope: if p.kind == MemoryKind::Workspace {
                                p.scope.clone()
                            } else {
                                None
                            },
                        },
                    );
                }
                EventKind::MemoryReinforced => {
                    if let Some(id) = e.data.get("id").and_then(|v| v.as_str())
                        && let Some(entry) = mem.entries.get_mut(id)
                    {
                        let delta = e.data.get("delta").and_then(|v| v.as_f64()).unwrap_or(0.2);
                        entry.weight = (entry.weight + delta).clamp(0.0, 10.0);
                        entry.last_used_at = Some(e.at);
                    }
                }
                EventKind::MemoryForgotten => {
                    if let Some(id) = e.data.get("id").and_then(|v| v.as_str()) {
                        mem.entries.remove(id);
                    }
                }
                _ => {}
            }
        }
        mem
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn get(&self, id: &str) -> Option<&MemoryEntry> {
        self.entries.get(id)
    }

    /// 全部记忆，**按类别分组、组内按有效分数降序**。
    ///
    /// ## 为什么按分数排而不是按时间
    ///
    /// 使用者打开"它记住了什么"时想知道的是**它最看重什么**，而不是
    /// "最近记了什么"。而分数里已经含了权重和近因（30 天半衰期），
    /// 所以按分数排等于"既看重要度也看新鲜度"。
    ///
    /// 分类别是为了**能核对分层对不对**——比如"这条明明是偏好，
    /// 怎么记成事实了"。
    pub fn all_sorted(&self, now_ms: u64) -> Vec<&MemoryEntry> {
        let mut v: Vec<&MemoryEntry> = self.entries.values().collect();
        v.sort_by(|a, b| {
            a.kind.cmp(&b.kind).then_with(|| {
                b.score(now_ms)
                    .partial_cmp(&a.score(now_ms))
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
        });
        v
    }

    /// 按一句话召回：这是 RAG 的那一半。
    ///
    /// ## 和 `recall()` 的分工
    ///
    /// - [`Memory::recall`] 按类别 + 分数取，**是给决策层看的**
    /// - 这个按**语义相似度**取，**是给对话模型看的**
    ///
    /// ## 允许用时钟——因为它的产出不进稳定前缀
    ///
    /// 常驻段必须是纯函数（见 [`Memory::resident`]），因为它进稳定前缀、
    /// 一变缓存全废。而这个函数的产出进**易变尾**（每轮的问题旁边），
    /// 本来就不缓存，所以用新近度没关系。
    ///
    /// **这个区分要守住**：哪天有人想把动态召回的产出挪进前缀，
    /// 就会踩到"前缀随时间静默变化"那个坑。
    ///
    /// ## 每一条的结局都要说清
    ///
    /// 返回**全部候选**连同各自的 [`RecallRoute`]，而不只是选中的那些。
    /// 因为"它怎么没想起那条"是排查时第一个会问的问题，
    /// 而只返回选中项的话这个问题没法回答。
    pub fn recall_for_prompt(
        &self,
        query: &str,
        now_ms: u64,
        limit: usize,
        budget_chars: usize,
        fatigue: &std::collections::HashMap<String, u32>,
        // **当前工作目录**，用来筛 `Workspace` 记忆。
        //
        // 筛选放在召回这一刻，而不是"记的时候"：记忆跨目录共存，
        // 记的时候筛就得为每个目录重写一遍记忆。
        //
        // `None` = 不知道当前目录。这时**所有带作用域的
        // `Workspace` 记忆都不给**——宁可少给一条，
        // 也不要在一个未知目录里用别的目录的约定。
        // （Rust 不允许给函数参数写 `///` 文档注释，所以这里是 `//`。）
        scope: Option<&str>,
    ) -> Vec<RecallHit<'_>> {
        use crate::embedding::{CHANNEL_DEPTH, RRF_K, embed_with_idf, lexical_overlap, rrf_fuse};

        // 语料 = 全部记忆的正文。IDF 要它来算"哪些字到处都是"。
        let idf = crate::embedding::Idf::fit(self.entries.values().map(|e| e.text.as_str()));

        // 定序的载体：`self.entries` 是 BTreeMap，遍历顺序按 id，
        // **是确定的**——所以下标能稳定地代表某一条。
        let items: Vec<&MemoryEntry> = self
            .entries
            .values()
            .filter(|e| match (e.kind, e.scope.as_deref()) {
                // 工作目录记忆：**只有当前目录对得上才给**。
                // 没有作用域的 `Workspace` 记忆是坏数据（记的时候一定会填），
                // 不给——它不知道该在哪个目录生效。
                (MemoryKind::Workspace, Some(owner)) => scope == Some(owner),
                (MemoryKind::Workspace, None) => false,
                // 其他类**无作用域**：事实、偏好、关系、事件在哪儿都成立
                _ => true,
            })
            .collect();

        // ---- 路 1：语义 ----
        let qv = embed_with_idf(query, &idf);
        let mut semantic: Vec<(usize, f32)> = items
            .iter()
            .enumerate()
            .map(|(i, e)| {
                let ev = embed_with_idf(&e.text, &idf);
                (i, crate::embedding::cosine(&qv, &ev).max(0.0))
            })
            // **要有相关性下限。** 重写成两路时这里一度写成 `> 0.0`，
            // 结果是"任何中文文本之间都有一点余弦"——不相干的问题
            // 也能召回一堆。下限是量出来的（见 RECALL_MIN_SCORE 的文档）。
            .filter(|(_, sc)| *sc >= RECALL_MIN_SCORE)
            .collect();
        semantic.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.0.cmp(&b.0))
        });
        semantic.truncate(CHANNEL_DEPTH);

        // ---- 路 2：词面 ----
        //
        // **加这一路是为了"精确的字对上了"这种情况。**
        // 语义通道对精确的词不敏感：问 `DISCOUNT_THRESHOLD` 长什么样时，
        // 一个泛化的"讲满减逻辑"可能把真正含这个标识符的那条挤掉。
        let mut lexical: Vec<(usize, f32)> = items
            .iter()
            .enumerate()
            .map(|(i, e)| (i, lexical_overlap(query, &e.text)))
            .filter(|(_, sc)| *sc > 0.0)
            .collect();
        lexical.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.0.cmp(&b.0))
        });
        lexical.truncate(CHANNEL_DEPTH);

        // ---- 融合 ----
        //
        // **词面传在前面**：RRF 的稳定排序让它在平局时赢，
        // 于是"精确命中不被泛化向量挤掉"这条要求自动成立。
        let lex_rank: Vec<usize> = lexical.iter().map(|(i, _)| *i).collect();
        let sem_rank: Vec<usize> = semantic.iter().map(|(i, _)| *i).collect();
        let fused = rrf_fuse(&[lex_rank, sem_rank], RRF_K);

        let in_lex = |i: usize| lexical.iter().any(|(j, _)| *j == i);
        let in_sem = |i: usize| semantic.iter().any(|(j, _)| *j == i);

        let mut out = Vec::with_capacity(items.len());
        let mut used = 0usize;
        let mut taken = 0usize;
        let mut pushed: std::collections::HashSet<usize> = std::collections::HashSet::new();

        for (idx, fused_score) in &fused {
            let e = items[*idx];
            let via = match (in_lex(*idx), in_sem(*idx)) {
                (true, true) => RecallVia::Both,
                (true, false) => RecallVia::Lexical,
                (false, true) => RecallVia::Semantic,
                (false, false) => RecallVia::None,
            };
            // 按类别权重和疲劳修正**排序分**。RRF 分本身只用名次，
            // 这里乘上业务修正——概念上对上研究文档 §4.4 的那个式子。
            let adjusted = *fused_score as f32
                * self.dynamic_weight(e, now_ms)
                * fatigue_penalty(fatigue.get(&e.id).copied());
            let route = if taken >= limit || used + e.text.chars().count() > budget_chars {
                RecallRoute::DroppedBudget
            } else {
                used += e.text.chars().count();
                taken += 1;
                RecallRoute::Picked
            };
            pushed.insert(*idx);
            out.push(RecallHit {
                entry: e,
                score: adjusted,
                route,
                via,
            });
        }

        // ---- 两路都没进的那些，也要出现在结果里 ----
        //
        // **"它怎么没想起那条"是排查时第一个会问的问题。**
        // 只返回选中项的话这个问题答不上来（研究文档 §4.7 的原话：
        // "召回 0 命中不知原因"）。
        for (i, e) in items.iter().enumerate() {
            if pushed.contains(&i) {
                continue;
            }
            out.push(RecallHit {
                entry: e,
                score: 0.0,
                route: RecallRoute::DroppedUnrelated,
                via: RecallVia::None,
            });
        }

        // 选中的排前面（按修正后的分降序），没选中的按 id 兜底定序
        out.sort_by(|a, b| {
            let pa = (a.route != RecallRoute::Picked) as u8;
            let pb = (b.route != RecallRoute::Picked) as u8;
            pa.cmp(&pb)
                .then_with(|| {
                    b.score
                        .partial_cmp(&a.score)
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
                .then_with(|| a.entry.id.cmp(&b.entry.id))
        });
        out
    }

    /// 各层在**动态召回**里的基础权重。
    ///
    /// 常驻层（事实 / 偏好）**稍微**压低一点：它们已经进稳定前缀了，
    /// 再在尾部出现一次是重复占位。
    ///
    /// **但只能是"稍微"。** 一开始压到 0.35、而关系层给到 1.2，
    /// 结果测试里"我住在哪"召回了「妈妈住在南京」（关系类）而不是
    /// 「住在杭州」（事实类）——**同样相关的两条，凭类别分了胜负**，
    /// 而类别跟"这条是不是答案"根本没关系。
    ///
    /// 类别的意义是"重复占位要不要避免"，不是"哪条更对"。
    /// 所以差距要小到**不足以翻盘**。
    ///
    /// 而事件 / 关系只有跟当前话题相关时才用得上——所以它们靠相似度
    /// 说话，基础权重给足。
    ///
    /// 收 `now_ms` 而不是读时钟：**这个函数的输出不该依赖"什么时候调用"**，
    /// 由调用方把时间递进来，这样测试能钉死一个时刻去断言。
    fn dynamic_weight(&self, e: &MemoryEntry, now_ms: u64) -> f32 {
        let by_kind = match e.kind {
            // 已经在前缀里了，稍微让一让，但不足以翻盘
            MemoryKind::Fact | MemoryKind::Preference => 0.9,
            MemoryKind::Relationship | MemoryKind::Event => 1.0,
            // 工作目录记忆**只在相关时才该出现**——它讲的是这个目录的事，
            // 换个话题就不该占位。给低基础权重，靠相似度把它抬上来。
            MemoryKind::Workspace => 0.8,
        };
        // 近因：90 天半衰期。**比常驻层那个 30 天宽**——
        // 常驻层要的是"长期是谁"，动态层要的是"最近相关"
        let age_days =
            (now_ms.saturating_sub(e.last_used_at.unwrap_or(e.created_at)) as f64) / 86_400_000.0;
        let recency = 0.5_f64.powf(age_days / 90.0) as f32;
        by_kind * (0.6 + 0.4 * recency)
    }

    /// **常驻**记忆：关于"使用者是谁"的那几类，按稳定顺序取前若干条。
    ///
    /// ## 这个函数必须是纯函数——这是缓存纪律要求的
    ///
    /// 常驻记忆要进**稳定前缀**，而稳定前缀的前提是"同样输入永远产出
    /// 同样字节"（见 `build_persona` 的文档）。
    ///
    /// **所以这里不能用 [`Memory::recall`]。** 那个按 `score(now_ms)` 排，
    /// 而 score 里含 30 天半衰期的时间因子——**同样的记忆，随着时间推移
    /// 排序会变**，于是前缀**静默变化**、缓存悄悄失效。
    /// 那种失效不报错、不变慢，只表现为账单变贵。
    ///
    /// 这里只按 `(权重降序, 创建时间降序, id)` 排——**全是事件里定死的值，
    /// 跟"现在几点"无关**。给同一串事件，任何时候调用都得到同样的结果。
    ///
    /// ## 为什么分常驻和动态
    ///
    /// - **常驻**（Fact / Preference）：关于"使用者是谁"。每次对话都用得上，
    ///   而且变化很慢——所以进稳定前缀，被缓存。
    /// - **动态**（Event / Relationship）：跟当前话题相关才用得上，
    ///   应该按本轮问题召回、放易变尾。
    ///
    /// 全塞前缀的话前缀会随记忆无限变大；全放尾部的话每轮都要重发。
    pub fn resident(&self, per_kind_limit: usize) -> Vec<&MemoryEntry> {
        let mut v: Vec<&MemoryEntry> = self
            .entries
            .values()
            .filter(|e| matches!(e.kind, MemoryKind::Fact | MemoryKind::Preference))
            .collect();
        v.sort_by(|a, b| {
            // 权重高的先；同权重新的先；再同就按 id——
            // **最后这个兜底不能省**，否则同权重的两条顺序不定，
            // 前缀就跟着不定
            b.weight
                .partial_cmp(&a.weight)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| b.created_at.cmp(&a.created_at))
                .then_with(|| a.id.cmp(&b.id))
        });
        // 每类各留前 N 条，而不是合起来取前 N——
        // 偏好再多也不该把"使用者是谁"挤掉
        let mut seen_fact = 0usize;
        let mut seen_pref = 0usize;
        v.retain(|e| match e.kind {
            MemoryKind::Fact => {
                seen_fact += 1;
                seen_fact <= per_kind_limit
            }
            _ => {
                seen_pref += 1;
                seen_pref <= per_kind_limit
            }
        });
        v
    }

    /// 常驻记忆段的**版本号**：内容决定的短哈希。
    ///
    /// 内容是哪些、顺序如何，全都体现在这个串里。所以：
    /// - 版本没变 → 前缀没变 → **缓存该命中**
    /// - 版本变了 → 就是这次换了记忆，**一次未命中是预期内的**
    ///
    /// 它的价值是**可诊断**：出问题时能一眼看出"前缀到底变没变"，
    /// 而不是对着一堆字节猜。
    pub fn resident_version(&self, per_kind_limit: usize) -> String {
        // 用 DefaultHasher 而不是引新依赖：短哈希够用，
        // 它只是给人看和给日志比对的，不是密码学用途
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        for e in self.resident(per_kind_limit) {
            e.id.hash(&mut h);
            e.text.hash(&mut h);
            // 权重取整参与哈希：免得浮点尾数让版本号无谓地跳
            ((e.weight * 100.0).round() as i64).hash(&mut h);
        }
        format!("{:06x}", h.finish() & 0xff_ffff)
    }

    /// 按正文子串找。**大小写不敏感。**
    ///
    /// 用子串而不是别的匹配方式：使用者要找的是"我记过关于 X 的东西"，
    /// 那是个很粗的筛子，不该要求他记得原话。
    pub fn search(&self, needle: &str) -> Vec<&MemoryEntry> {
        let n = needle.trim().to_lowercase();
        if n.is_empty() {
            return Vec::new();
        }
        self.entries
            .values()
            .filter(|e| e.text.to_lowercase().contains(&n))
            .collect()
    }

    /// 按类别召回，按有效分数降序。
    pub fn recall(&self, kind: MemoryKind, now_ms: u64, limit: usize) -> Vec<&MemoryEntry> {
        let mut v: Vec<&MemoryEntry> = self.entries.values().filter(|e| e.kind == kind).collect();
        v.sort_by(|a, b| {
            b.score(now_ms)
                .partial_cmp(&a.score(now_ms))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        v.truncate(limit);
        v
    }

    /// 构造决策模型的 state。
    ///
    /// **这是"只喂判断所需的证据"的落点。** 具体做法：
    ///
    /// - 每类记忆只取权重最高的若干条，不是全量倾倒；
    /// - 关系状态压成一个阶段标签，而不是把原始记录都塞进去；
    /// - 情境字段只放**判断真正需要知道的**（是否安静时段、多久没互动、待处理量）；
    /// - **不做任何密钥或原文透传**。
    pub fn build_decision_state(
        &self,
        ctx: &Situation,
        now_ms: u64,
        per_kind_limit: usize,
    ) -> serde_json::Value {
        let mut state = serde_json::Map::new();

        for kind in [
            MemoryKind::Fact,
            MemoryKind::Preference,
            MemoryKind::Relationship,
            MemoryKind::Event,
        ] {
            let items: Vec<&str> = self
                .recall(kind, now_ms, per_kind_limit)
                .iter()
                .map(|e| e.text.as_str())
                .collect();
            if !items.is_empty() {
                state.insert(kind.state_key().into(), serde_json::json!(items));
            }
        }

        // 情境字段
        state.insert("quiet_hours".into(), serde_json::json!(ctx.quiet_hours));
        state.insert(
            "minutes_since_last_interaction".into(),
            serde_json::json!(ctx.minutes_since_last_interaction),
        );
        state.insert("unread_events".into(), serde_json::json!(ctx.unread_events));
        state.insert(
            "recent_failures".into(),
            serde_json::json!(ctx.recent_failures),
        );
        state.insert(
            "interventions_today".into(),
            serde_json::json!(ctx.interventions_today),
        );
        state.insert(
            "relationship_stage".into(),
            serde_json::json!(ctx.relationship_stage),
        );

        serde_json::Value::Object(state)
    }
}

/// 决策所需的情境。刻意保持小：判断用不到的一律不放。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Situation {
    /// 是否处于使用者设定的安静时段。
    pub quiet_hours: bool,
    /// 距离上次互动过了多少分钟。
    pub minutes_since_last_interaction: u64,
    /// 待处理事件数。
    pub unread_events: usize,
    /// 近期连续失败数。
    pub recent_failures: u32,
    /// 今天已经打扰过几次。
    pub interventions_today: u32,
    /// 关系阶段（由记忆派生后的标签，不是原始记录）。
    pub relationship_stage: String,
}

impl Default for Situation {
    fn default() -> Self {
        Self {
            quiet_hours: false,
            minutes_since_last_interaction: 0,
            unread_events: 0,
            recent_failures: 0,
            interventions_today: 0,
            relationship_stage: "初期".into(),
        }
    }
}

#[cfg(test)]
mod listing_tests {
    use super::*;
    use crate::ledger::{Event, EventKind};

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

    fn three() -> Memory {
        Memory::from_events(&[
            ev(
                EventKind::MemoryRecorded,
                1_000,
                serde_json::json!({"id": "a", "kind": "event", "text": "开过一次会"}),
            ),
            ev(
                EventKind::MemoryRecorded,
                1_000,
                serde_json::json!({"id": "b", "kind": "fact", "text": "住在杭州"}),
            ),
            ev(
                EventKind::MemoryRecorded,
                1_000,
                serde_json::json!({"id": "c", "kind": "preference", "text": "喜欢简洁的回答"}),
            ),
        ])
    }

    #[test]
    fn all_sorted_groups_by_kind() {
        // **按类别分组是为了能核对分层对不对**——比如"这条明明是偏好，
        // 怎么记成事实了"。分组之后一眼看得出来。
        let m = three();
        let all = m.all_sorted(2_000);
        assert_eq!(all.len(), 3);
        // MemoryKind 的声明顺序：Fact < Preference < Relationship < Event
        let kinds: Vec<MemoryKind> = all.iter().map(|e| e.kind).collect();
        assert_eq!(
            kinds,
            vec![MemoryKind::Fact, MemoryKind::Preference, MemoryKind::Event],
            "同类的要挨在一起"
        );
    }

    #[test]
    fn all_sorted_puts_the_heavier_one_first_within_a_kind() {
        // 组内按分数：使用者想知道"它最看重什么"，
        // 而不是"最近记了什么"。分数里已经含了权重和近因。
        let m = Memory::from_events(&[
            ev(
                EventKind::MemoryRecorded,
                1_000,
                serde_json::json!({"id": "low", "kind": "fact", "text": "轻的", "weight": 0.2}),
            ),
            ev(
                EventKind::MemoryRecorded,
                1_000,
                serde_json::json!({"id": "high", "kind": "fact", "text": "重的", "weight": 5.0}),
            ),
        ]);
        let all = m.all_sorted(2_000);
        assert_eq!(all[0].id, "high", "权重高的排前面");
        assert_eq!(all[1].id, "low");
    }

    #[test]
    fn search_matches_a_substring() {
        // 用子串：使用者要找的是"我记过关于 X 的东西"，
        // 那是个很粗的筛子，不该要求他记得原话。
        let m = three();
        let hits = m.search("杭州");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, "b");
    }

    #[test]
    fn search_is_case_insensitive() {
        let m = Memory::from_events(&[ev(
            EventKind::MemoryRecorded,
            1_000,
            serde_json::json!({"id": "a", "kind": "fact", "text": "用 Rust 写的"}),
        )]);
        assert_eq!(m.search("rust").len(), 1, "小写也该找到");
        assert_eq!(m.search("RUST").len(), 1, "大写也该找到");
    }

    #[test]
    fn an_empty_needle_finds_nothing_rather_than_everything() {
        // **空串返回全部是个陷阱**：调用方一不小心就会把整本记忆倒出来。
        // 什么都不返回更安全，而且调用方本来就该先判空。
        let m = three();
        assert!(m.search("").is_empty());
        assert!(m.search("   ").is_empty());
    }

    #[test]
    fn a_forgotten_memory_disappears_from_listing_and_search() {
        // **忘记要真的从列表和搜索里消失**，而不只是台账里多一条事件。
        let mut m = three();
        assert_eq!(m.all_sorted(2_000).len(), 3);
        assert_eq!(m.search("杭州").len(), 1);

        m = Memory::from_events(&[
            ev(
                EventKind::MemoryRecorded,
                1_000,
                serde_json::json!({"id": "b", "kind": "fact", "text": "住在杭州"}),
            ),
            ev(
                EventKind::MemoryForgotten,
                2_000,
                serde_json::json!({"id": "b"}),
            ),
        ]);
        assert!(m.all_sorted(3_000).is_empty(), "忘了就不该还在列表里");
        assert!(m.search("杭州").is_empty(), "忘了就不该还能搜到");
    }
}

#[cfg(test)]
mod recall_tests {
    use super::*;
    use crate::ledger::{Event, EventKind};
    use std::collections::HashMap;

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

    /// 一批**互不相干**的记忆。每条盯一件不同的事——
    /// 这正是"召回挑得准"要面对的形状。
    fn twenty() -> Vec<(&'static str, &'static str)> {
        vec![
            ("喜欢青绿色", "preference"),
            ("不喜欢被叫亲", "preference"),
            ("住在杭州", "fact"),
            ("不吃香菜", "fact"),
            ("用的是 Windows", "fact"),
            ("养了一只叫豆豆的猫", "fact"),
            ("每天七点起床", "fact"),
            ("在做一个叫云熙的项目", "fact"),
            ("喜欢喝美式不加糖", "preference"),
            ("讨厌开会", "preference"),
            ("上一个项目是云熙机器人", "event"),
            ("上周和客户开了个会", "event"),
            ("昨天把测试跑通了", "event"),
            ("前天读了本关于记忆的书", "event"),
            ("上个月换了台笔记本", "event"),
            ("周末去爬了山", "event"),
            ("同事叫小李", "relationship"),
            ("和小李一起做过一个项目", "relationship"),
            ("妈妈住在南京", "relationship"),
            ("和爸爸关系很好", "relationship"),
        ]
    }

    fn mem_of(items: &[(&str, &str)]) -> Memory {
        let events: Vec<Event> = items
            .iter()
            .enumerate()
            .map(|(i, (text, kind))| {
                ev(
                    EventKind::MemoryRecorded,
                    1_000 + i as u64,
                    serde_json::json!({"id": format!("m{i}"), "kind": kind, "text": text}),
                )
            })
            .collect();
        Memory::from_events(&events)
    }

    fn no_fatigue() -> HashMap<String, u32> {
        HashMap::new()
    }

    #[test]
    fn recall_picks_the_right_memory_for_a_question() {
        // **这是 RAG 的验收核心：20 条不相干的记忆里挑得准。**
        //
        // ## 为什么这里问的都是"事件"而不是"事实"
        //
        // 一开始我拿"我养了什么宠物"「我喝咖啡有什么讲究」去测，挂了。
        // 查下去发现**测错了对象**：
        //
        // - 那两条问的是**事实**，而事实**已经全在常驻前缀里**了
        //   （`resident()` 把它们都放进了稳定段）。它们根本不需要
        //   靠动态召回去找。
        // - 而且「宠物」和「猫」、「咖啡」和「美式」之间**没有任何
        //   共同的字**——字符 n-gram 跨不过这种语义跳跃，这是方法的
        //   固有限制，不是 bug。
        //
        // **动态召回真正服务的是事件和关系**：它们条目多、跟话题强相关、
        // 不可能全塞进前缀。所以验收就该拿它们测。
        let items = twenty();
        let m = mem_of(&items);
        let cases = [
            ("上周和客户干了什么", "客户"),
            ("上个月我换过什么设备", "笔记本"),
            ("前天读了什么", "书"),
            ("周末去哪了", "爬山"),
            ("和小李一起做过什么", "小李"),
        ];
        let mut hit = 0;
        for (q, want) in cases {
            let r = m.recall_for_prompt(q, 10_000, 3, 400, &no_fatigue(), None);
            let picked: Vec<&str> = r
                .iter()
                .filter(|h| h.route == RecallRoute::Picked)
                .map(|h| h.entry.text.as_str())
                .collect();
            let ok = picked.iter().any(|t| t.contains(want));
            println!(
                "  「{q}」→ {picked:?}  期望含「{want}」  {}",
                if ok { "✓" } else { "✗" }
            );
            if ok {
                hit += 1;
            }
        }
        // 20 条互不相干的记忆里挑 3 条，命中 5/5 才算"挑得准"。
        // 定 4/5 是留一点余量——字符 n-gram 不是真 embedding，
        // 对"你该怎么称呼我"这种绕的说法本来就不强。
        println!("  召回命中率 {hit}/5");
        assert!(hit >= 4, "20 条里挑得准是这条的验收标准，只对了 {hit}/5");
    }

    #[test]
    fn a_semantic_leap_cannot_be_bridged_by_characters() {
        // **如实记下方法的固有限制，而不是假装它不存在。**
        //
        // 例子要挑**真的没有共同字**的：
        //
        // - 「宠物」vs「猫」——零共同字 ✓
        // - 「咖啡」vs「美式」——零共同字 ✓
        //
        // 一开始我举的是「我养了什么宠物」vs「养了一只叫豆豆的猫」，
        // 结果相似度 0.13 **过了门槛**——因为两句里有共同的「养了」。
        // 那个例子不成立，是我挑得不准，不是限制不存在。
        //
        // 为什么这不致命：**这类问题问的是事实，而事实已经在常驻前缀里了。**
        // 模型看得到「养了一只叫豆豆的猫」，直接就能答，不需要召回。
        let q = crate::embedding::embed_plain("宠物");
        let m = crate::embedding::embed_plain("猫");
        let s = crate::embedding::cosine(&q, &m);
        assert!(
            s < RECALL_MIN_SCORE,
            "这个限制还在的话，跨语义的相似度该低于门槛——实际 {s}。\
             如果它变高了，说明换了算法，这条和上面的取舍该重新看"
        );
    }

    #[test]
    fn the_budget_is_respected_and_overspill_is_labelled() {
        // 预算用完的那些必须**标成 DroppedBudget**，不能默默不返回——
        // "为什么没召回某条"要能回答。
        let items = twenty();
        let m = mem_of(&items);
        // **`limit` 压到 1 是为了让"溢出"必然发生。**
        //
        // 原来用的是 2，而相关性下限提高之后够相关的条目也少了，
        // 凑不出溢出的场景——于是这条测试挂在了"没有 DroppedBudget"上。
        // 那**不是断言放宽，是场景失效**：要验的机制是"溢出被标出来"，
        // 就该保证一定溢出。
        let r = m.recall_for_prompt(
            "颜色 猫 咖啡 杭州 香菜",
            10_000,
            1,
            400,
            &no_fatigue(),
            None,
        );
        let picked = r.iter().filter(|h| h.route == RecallRoute::Picked).count();
        assert!(picked <= 1, "条数上限是 1，实际选了 {picked}");
        assert!(
            r.iter().any(|h| h.route == RecallRoute::DroppedBudget),
            "超预算的必须带原因，而不是凭空消失"
        );
        // 所有候选都要在返回里（不管选中还是没选中）
        assert_eq!(r.len(), items.len(), "全部候选都要返回，好回答'为什么没它'");
    }

    #[test]
    fn an_unrelated_question_drops_everything_with_a_reason() {
        let items = twenty();
        let m = mem_of(&items);
        let r = m.recall_for_prompt(
            "量子色动力学的重整化群方程",
            10_000,
            5,
            400,
            &no_fatigue(),
            None,
        );
        let picked = r.iter().filter(|h| h.route == RecallRoute::Picked).count();
        assert_eq!(picked, 0, "跟记忆库完全不相干的问题不该硬塞记忆进去");
        assert!(r.iter().all(|h| h.route == RecallRoute::DroppedUnrelated));
    }

    #[test]
    fn a_repeatedly_recalled_memory_stops_dominating() {
        // **验收项：过度召回不霸屏。**
        //
        // 参考实现里有个真实记录：一条记忆被召回 238 次——因为召回会
        // 加强它，而更强的它更容易被再召回，正反馈自己滚起来。
        let items = twenty();
        let m = mem_of(&items);
        let q = "我住在哪";

        let fresh = m.recall_for_prompt(q, 10_000, 3, 400, &no_fatigue(), None);
        let top_fresh = fresh
            .iter()
            .find(|h| h.route == RecallRoute::Picked)
            .map(|h| h.entry.id.clone())
            .expect("该有命中的");

        // 同一条已经被召回 200 次
        let mut tired = HashMap::new();
        tired.insert(top_fresh.clone(), 200u32);
        let after = m.recall_for_prompt(q, 10_000, 3, 400, &tired, None);

        // 它的分数必须明显掉下来（对数惩罚：1/(1+ln(201)) ≈ 0.158）
        let s_fresh = fresh
            .iter()
            .find(|h| h.entry.id == top_fresh)
            .unwrap()
            .score;
        let s_tired = after
            .iter()
            .find(|h| h.entry.id == top_fresh)
            .unwrap()
            .score;
        println!("  被召回 200 次之后：{s_fresh:.3} → {s_tired:.3}");
        assert!(
            s_tired < s_fresh * 0.3,
            "反复召回该被明显压下去：{s_fresh} → {s_tired}"
        );
    }

    #[test]
    fn a_few_recalls_barely_matter() {
        // **对数惩罚的意义**：前几次几乎无感。线性惩罚会把正常的
        // 第二次召回也砍一半，那太狠了。
        assert!((fatigue_penalty(None) - 1.0).abs() < 1e-6);
        assert!((fatigue_penalty(Some(0)) - 1.0).abs() < 1e-6);
        assert!(fatigue_penalty(Some(1)) > 0.7, "召回一次几乎不该有惩罚");
        assert!(fatigue_penalty(Some(3)) > 0.6, "三次也该还很轻");
        // 200 次时约 0.21——**那是 79% 的削减**，够"明显"了。
        // 一开始写的是 < 0.2，就差 0.013 挂着；那是阈值拍得太紧，
        // 不是行为不对。
        assert!(fatigue_penalty(Some(200)) < 0.25, "两百次该有大幅削减");
        // 必须单调递减，否则"越召回越高"就不是惩罚了
        let mut last = 2.0f32;
        for n in 0..500u32 {
            let p = fatigue_penalty(Some(n));
            assert!(p <= last, "召回次数多了惩罚只能更重，不能更轻");
            last = p;
        }
    }

    #[test]
    fn the_resident_layers_are_deprioritised_in_dynamic_recall() {
        // 事实/偏好已经进稳定前缀了，动态召回里再出现一次就是白占预算。
        // 但这只该是**压低**，不该是"永不召回"——这句话真的在问它时，
        // 相似度会把它顶上来。
        // **用完全相同的正文**，这样相似度一模一样，唯一变量就是类别。
        // 一开始用的是两条不同的正文，于是"事实分低"到底是因为类别
        // 还是因为本来就不像，根本分不出来——测试挂了我还在猜。
        let m = mem_of(&[("住在杭州", "fact"), ("住在杭州", "event")]);
        let r = m.recall_for_prompt("杭州", 10_000, 5, 400, &no_fatigue(), None);
        let fact = r.iter().find(|h| h.entry.kind == MemoryKind::Fact).unwrap();
        let event = r
            .iter()
            .find(|h| h.entry.kind == MemoryKind::Event)
            .unwrap();
        assert!(
            fact.score < event.score,
            "同样的相似度下，常驻层该排在事件后面（{:.3} vs {:.3}）",
            fact.score,
            event.score
        );
        // 但它仍然被召回了——"压低"不是"排除"
        assert_eq!(fact.route, RecallRoute::Picked);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::{Event, EventKind};
    use serde_json::json;

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

    fn sample_memory() -> Memory {
        Memory::from_events(&[
            ev(
                EventKind::MemoryRecorded,
                1_000,
                json!({"id": "f1", "kind": "fact", "text": "在准备 AI 岗位面试"}),
            ),
            ev(
                EventKind::MemoryRecorded,
                2_000,
                json!({"id": "p1", "kind": "preference", "text": "深夜不喜欢被打扰"}),
            ),
            ev(
                EventKind::MemoryRecorded,
                3_000,
                json!({"id": "r1", "kind": "relationship", "text": "一起把简历改完了"}),
            ),
            ev(EventKind::MemoryForgotten, 4_000, json!({"id": "f1"})),
        ])
    }

    #[test]
    fn projects_record_and_forget() {
        let m = sample_memory();
        assert_eq!(m.len(), 2, "f1 应已被遗忘");
        assert!(m.get("f1").is_none());
        assert!(m.get("p1").is_some());
    }

    #[test]
    fn reinforce_raises_weight_and_marks_used() {
        let mut events = vec![ev(
            EventKind::MemoryRecorded,
            1_000,
            json!({"id": "p1", "kind": "preference", "text": "x"}),
        )];
        let before = Memory::from_events(&events).get("p1").unwrap().weight;
        events.push(ev(
            EventKind::MemoryReinforced,
            2_000,
            json!({"id": "p1", "delta": 1.5}),
        ));
        let m = Memory::from_events(&events);
        let after = m.get("p1").unwrap();
        assert!(after.weight > before);
        assert_eq!(after.last_used_at, Some(2_000));
    }

    #[test]
    fn recall_filters_by_kind_and_orders_by_weight() {
        let m = Memory::from_events(&[
            ev(
                EventKind::MemoryRecorded,
                1_000,
                json!({"id": "p1", "kind": "preference", "text": "低权重", "weight": 0.5}),
            ),
            ev(
                EventKind::MemoryRecorded,
                2_000,
                json!({"id": "p2", "kind": "preference", "text": "高权重", "weight": 5.0}),
            ),
            ev(
                EventKind::MemoryRecorded,
                3_000,
                json!({"id": "f1", "kind": "fact", "text": "别类"}),
            ),
        ]);
        let got = m.recall(MemoryKind::Preference, 4_000, 10);
        assert_eq!(got.len(), 2, "只应召回偏好类");
        assert_eq!(got[0].text, "高权重");
    }

    #[test]
    fn recall_respects_limit() {
        let m = Memory::from_events(&[
            ev(
                EventKind::MemoryRecorded,
                1,
                json!({"id": "a", "kind": "fact", "text": "a"}),
            ),
            ev(
                EventKind::MemoryRecorded,
                2,
                json!({"id": "b", "kind": "fact", "text": "b"}),
            ),
            ev(
                EventKind::MemoryRecorded,
                3,
                json!({"id": "c", "kind": "fact", "text": "c"}),
            ),
        ]);
        assert_eq!(m.recall(MemoryKind::Fact, 10, 2).len(), 2);
    }

    #[test]
    fn decision_state_is_trimmed_not_dumped() {
        // 造 30 条事实，per_kind_limit=3，state 里只应有 3 条
        let mut events = Vec::new();
        for i in 0..30 {
            events.push(ev(
                EventKind::MemoryRecorded,
                i as u64 + 1,
                json!({"id": format!("f{i}"), "kind": "fact", "text": format!("事实 {i}")}),
            ));
        }
        let m = Memory::from_events(&events);
        let state = m.build_decision_state(&Situation::default(), 1_000_000, 3);
        let facts = state["facts"].as_array().unwrap();
        assert_eq!(facts.len(), 3, "state 必须裁剪，不能全量倾倒");
    }

    #[test]
    fn decision_state_carries_situation_fields() {
        let m = sample_memory();
        let ctx = Situation {
            quiet_hours: true,
            minutes_since_last_interaction: 360,
            unread_events: 5,
            recent_failures: 1,
            interventions_today: 2,
            relationship_stage: "熟悉".into(),
        };
        let state = m.build_decision_state(&ctx, 1_000_000, 5);
        assert_eq!(state["quiet_hours"], json!(true));
        assert_eq!(state["minutes_since_last_interaction"], json!(360));
        assert_eq!(state["relationship_stage"], json!("熟悉"));
    }

    #[test]
    fn unknown_event_kinds_are_ignored() {
        let m = Memory::from_events(&[
            ev(EventKind::JobCreated, 1, json!({"spec": {}})),
            ev(
                EventKind::MemoryRecorded,
                2,
                json!({"id": "ok", "kind": "fact", "text": "t"}),
            ),
        ]);
        assert_eq!(m.len(), 1);
    }

    #[test]
    fn malformed_memory_payload_is_skipped_not_panicking() {
        let m = Memory::from_events(&[
            ev(EventKind::MemoryRecorded, 1, json!({"kind": "fact"})), // 缺 text
            ev(EventKind::MemoryRecorded, 2, json!({"id": "x"})),      // 缺 kind/text
        ]);
        assert_eq!(m.len(), 0, "坏数据应跳过而不是崩");
    }
}

#[cfg(test)]
mod workspace_scope_tests {
    use super::*;
    use crate::ledger::{Event, EventKind};

    fn ev(at: u64, data: serde_json::Value) -> Event {
        Event {
            seq: at,
            at,
            kind: EventKind::MemoryRecorded,
            job: None,
            span: None,
            data,
        }
    }

    /// 一条"这个项目用 pytest"的工作目录记忆，外加一条无作用域的事实。
    fn two() -> Memory {
        Memory::from_events(&[
            ev(
                1_000,
                serde_json::json!({
                    "id": "w1", "kind": "workspace",
                    "text": "这个项目用 pytest 不用 unittest",
                    "scope": "C:\\proj\\a"
                }),
            ),
            ev(
                1_001,
                serde_json::json!({"id": "f1", "kind": "fact", "text": "使用者住在杭州"}),
            ),
        ])
    }

    fn ids(hits: &[RecallHit<'_>]) -> Vec<String> {
        hits.iter().map(|h| h.entry.id.clone()).collect()
    }

    fn no_fatigue() -> std::collections::HashMap<String, u32> {
        std::collections::HashMap::new()
    }

    #[test]
    fn a_workspace_memory_is_recalled_in_its_own_directory() {
        let m = two();
        let hits = m.recall_for_prompt(
            "这个项目用 pytest 还是 unittest",
            10_000,
            5,
            400,
            &no_fatigue(),
            Some("C:\\proj\\a"),
        );
        assert!(
            ids(&hits).contains(&"w1".to_string()),
            "**在它自己的目录里必须能召回**：{:?}",
            ids(&hits)
        );
    }

    #[test]
    fn a_workspace_memory_is_not_recalled_in_another_directory() {
        // **这是这条验收的核心。**
        //
        // A 项目的约定在 B 项目里被召回，等于**按错的前提干活**——
        // 而它会以一种非常像"它记得我"的姿态出现，人不容易察觉。
        let m = two();
        let hits = m.recall_for_prompt(
            "这个项目用 pytest 还是 unittest",
            10_000,
            5,
            400,
            &no_fatigue(),
            Some("C:\\proj\\b"),
        );
        assert!(
            !ids(&hits).contains(&"w1".to_string()),
            "**换个目录就不该给**：{:?}",
            ids(&hits)
        );
    }

    #[test]
    fn a_workspace_memory_is_not_recalled_when_the_directory_is_unknown() {
        // 拿不到 cwd 的时候（测试、非交互调用）**宁可少给一条**，
        // 也不要用一个不知道哪个目录的约定。
        let m = two();
        let hits = m.recall_for_prompt(
            "这个项目用 pytest 还是 unittest",
            10_000,
            5,
            400,
            &no_fatigue(),
            None,
        );
        assert!(
            !ids(&hits).contains(&"w1".to_string()),
            "不知道当前目录时不该给带作用域的记忆：{:?}",
            ids(&hits)
        );
    }

    #[test]
    fn an_unscoped_memory_is_recalled_anywhere() {
        // **反过来的那一半**：事实、偏好这类没有作用域的记忆
        // 在哪个目录都该能召回。筛得太狠会让"它不记得我了"。
        let m = two();
        for scope in [Some("C:\\proj\\a"), Some("C:\\proj\\b"), None] {
            let hits = m.recall_for_prompt("使用者住在哪", 10_000, 5, 400, &no_fatigue(), scope);
            assert!(
                ids(&hits).contains(&"f1".to_string()),
                "无作用域的记忆在 {scope:?} 下也该给：{:?}",
                ids(&hits)
            );
        }
    }

    #[test]
    fn workspace_memories_never_enter_the_resident_layer() {
        // **常驻层是稳定前缀**，而 cwd 每次运行都可能变。
        // 工作目录记忆一旦进去，前缀就跟着目录变——**缓存全废**。
        let m = two();
        let resident: Vec<String> = m.resident(20).iter().map(|e| e.id.clone()).collect();
        assert!(
            !resident.contains(&"w1".to_string()),
            "工作目录记忆不该进常驻层：{resident:?}"
        );
        // 事实类还是要在里面——别把正常的也筛掉了
        assert!(resident.contains(&"f1".to_string()), "{resident:?}");
    }

    #[test]
    fn a_workspace_memory_without_a_scope_is_not_given() {
        // 没有作用域的 `Workspace` 记忆是坏数据（记的时候一定会填）。
        // **它不知道该在哪个目录生效**，所以哪个目录都不给。
        let m = Memory::from_events(&[ev(
            1_000,
            serde_json::json!({
                "id": "bad", "kind": "workspace", "text": "这个项目用 pytest"
            }),
        )]);
        let hits = m.recall_for_prompt(
            "这个项目用 pytest",
            10_000,
            5,
            400,
            &no_fatigue(),
            Some("C:\\proj\\a"),
        );
        assert!(ids(&hits).is_empty(), "坏数据不该被召回：{:?}", ids(&hits));
    }
}

#[cfg(test)]
mod normalize_scope_tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn different_spellings_of_the_same_directory_agree() {
        // **这个性质才是要紧的那个。**
        //
        // 写的时候用 `C:\proj\a`、召回的时候用 `C:/proj/a/`——
        // 归一之后必须是同一个串，否则那条记忆**永远召不回，
        // 而且不报任何错**。
        let a = normalize_scope(Path::new("C:\\proj\\a"));
        let b = normalize_scope(Path::new("C:/proj/a/"));
        assert_eq!(a, b, "同一个目录的两种写法必须归一成同一个串");
    }

    #[test]
    fn a_trailing_separator_is_dropped() {
        assert_eq!(normalize_scope(Path::new("C:\\proj\\a\\")), "C:\\proj\\a");
        assert_eq!(normalize_scope(Path::new("C:\\proj\\a")), "C:\\proj\\a");
    }

    #[test]
    fn forward_slashes_become_backslashes() {
        assert_eq!(normalize_scope(Path::new("C:/proj/a")), "C:\\proj\\a");
    }

    #[test]
    fn a_bare_root_does_not_collapse_to_empty() {
        // **空串会和"无作用域"撞车。**
        // `scope: None` 的语义是"在哪儿都能召回"，而空串如果被当成
        // `Some("")`，它就跟根目录混在一起了。
        assert_eq!(normalize_scope(Path::new("/")), "\\");
        assert!(!normalize_scope(Path::new("/")).is_empty());
    }

    #[test]
    fn different_directories_stay_different() {
        // 反方向：归一不能把不同的目录并成一个——
        // 那会让 A 项目的约定在 B 项目里生效。
        assert_ne!(
            normalize_scope(Path::new("C:\\proj\\a")),
            normalize_scope(Path::new("C:\\proj\\b"))
        );
        // 前缀关系也不能并
        assert_ne!(
            normalize_scope(Path::new("C:\\proj")),
            normalize_scope(Path::new("C:\\proj\\a"))
        );
    }
}
