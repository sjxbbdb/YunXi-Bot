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
}

impl MemoryKind {
    pub fn label(self) -> &'static str {
        match self {
            MemoryKind::Fact => "事实",
            MemoryKind::Preference => "偏好",
            MemoryKind::Relationship => "关系",
            MemoryKind::Event => "事件",
        }
    }

    /// 在 state 里的字段名。
    pub fn state_key(self) -> &'static str {
        match self {
            MemoryKind::Fact => "facts",
            MemoryKind::Preference => "preferences",
            MemoryKind::Relationship => "relationship",
            MemoryKind::Event => "recent_events",
        }
    }
}

/// 一条记忆。
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

/// 记忆事件的数据形状。
#[derive(Debug, Clone, Serialize, Deserialize)]
struct RecordedPayload {
    kind: MemoryKind,
    text: String,
    #[serde(default = "one")]
    weight: f64,
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
