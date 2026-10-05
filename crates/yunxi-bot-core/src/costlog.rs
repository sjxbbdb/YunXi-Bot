//! 模型调用的成本台账。
//!
//! ## 为什么先记账、后算钱
//!
//! 每次调用都把**回执里的原始计数**（prompt / completion / 缓存命中）写进台账，
//! 单价和时段只影响"怎么算钱"，不影响"花了多少 token"。
//!
//! 这样做有两个好处：
//!
//! 1. **单价变了可以重算历史。** 把价格表当参数而不是把金额写死进台账，
//!    换价之后重放事件就能得到新的金额。
//! 2. **缓存命中率是可审计的。** 没有缓存字段和缓存全不命中，在只看金额时
//!    长得一样；分开存就分得清。
//!
//! ## 一个必须诚实的地方
//!
//! 服务端没回缓存字段时，[`Cost::has_cache_data`] 是 `false`。
//! 此时我们**不知道**缓存命中情况，不能假设"没命中"，也不能假设"命中了"。
//! 汇总里把它单独列出来，而不是混进"未命中"。

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::ledger::{Event, EventKind, Ledger, LedgerError};
use crate::think::cost::{Cost, PriceTable, Usage, is_peak_hour};

/// 一次模型调用的记录。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CallRecord {
    pub provider: String,
    pub model: String,
    /// 这次调用属于哪个任务（没有就空）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step: Option<String>,
    /// 这次调用是不是在高峰时段。**存下来**，否则事后算钱会用到今天的时段。
    pub peak: bool,
    /// 是否开了思考模式。
    #[serde(default)]
    pub thinking: bool,
    pub usage: Usage,
}

impl CallRecord {
    pub fn new(provider: impl Into<String>, model: impl Into<String>, peak: bool) -> Self {
        Self {
            provider: provider.into(),
            model: model.into(),
            task: None,
            step: None,
            peak,
            thinking: false,
            usage: Usage::default(),
        }
    }

    pub fn for_step(mut self, task: &str, step: &str) -> Self {
        self.task = Some(task.to_string());
        self.step = Some(step.to_string());
        self
    }

    pub fn with_usage(mut self, usage: Usage) -> Self {
        self.usage = usage;
        self
    }

    pub fn with_thinking(mut self, thinking: bool) -> Self {
        self.thinking = thinking;
        self
    }

    /// 按价格表复盘这次调用花了多少。
    pub fn cost(&self, price: &PriceTable) -> Cost {
        self.usage.cost(price, self.peak)
    }
}

/// 汇总。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CostReport {
    pub calls: usize,
    /// 按 provider 分开的价格表。
    pub total: f64,
    /// 如果缓存完全不命中，会花多少。用来量化缓存到底省了多少。
    pub without_cache: f64,
    /// 有多少次调用服务端没回缓存字段。
    pub calls_without_cache_data: usize,
    pub cache_hit_tokens: u64,
    pub cache_miss_tokens: u64,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    /// 其中开思考的调用次数。
    pub thinking_calls: usize,
}

impl CostReport {
    pub fn saved(&self) -> f64 {
        (self.without_cache - self.total).max(0.0)
    }

    /// 缓存命中率。**没有缓存数据时返回 `None`**，不是 0——
    /// 两者含义不同：一个是"不知道"，一个是"确实没命中"。
    pub fn cache_hit_rate(&self) -> Option<f64> {
        let total = self.cache_hit_tokens + self.cache_miss_tokens;
        if total == 0 {
            return None;
        }
        Some(self.cache_hit_tokens as f64 / total as f64)
    }
}

/// 一次调用该按哪个价格表算。**按 provider 查，查不到就用 None。**
pub fn price_for(provider: &str) -> Option<PriceTable> {
    match provider {
        "agnes" => Some(PriceTable::AGNES_30_FLASH),
        "deepseek" => Some(PriceTable::DEEPSEEK_FLASH),
        _ => None,
    }
}

/// 成本记录的落点。
///
/// 抽成 trait 是因为执行引擎只需要"能把记录交出去"，不需要知道落的是台账还是
/// 内存。这样引擎的测试可以完全不碰文件系统。
pub trait CostSink {
    fn record(&mut self, rec: &CallRecord) -> Result<(), LedgerError>;
}

impl CostSink for Ledger {
    fn record(&mut self, rec: &CallRecord) -> Result<(), LedgerError> {
        record_call(self, rec).map(|_| ())
    }
}

/// 把调用记录追加进台账。
pub fn record_call(ledger: &mut Ledger, rec: &CallRecord) -> Result<Event, LedgerError> {
    ledger.append(EventKind::ModelCalled, None, json!(rec))
}

/// 从事件流重算成本。
///
/// **认不出 provider 的记录会被统计进 `calls` 但计 0 元**——宁可金额偏小，
/// 也不要拿一个错的价格表算出一个看似精确的数字。
pub fn project_costs(events: &[Event]) -> CostReport {
    let mut r = CostReport::default();
    for e in events {
        if e.kind != EventKind::ModelCalled {
            continue;
        }
        let Ok(rec) = serde_json::from_value::<CallRecord>(e.data.clone()) else {
            continue;
        };
        r.calls += 1;
        r.prompt_tokens += rec.usage.prompt_tokens;
        r.completion_tokens += rec.usage.completion_tokens;
        r.cache_hit_tokens += rec.usage.cache_hit_tokens;
        r.cache_miss_tokens += rec.usage.cache_miss_tokens;
        if rec.thinking {
            r.thinking_calls += 1;
        }
        if !rec.usage.has_cache_data() {
            r.calls_without_cache_data += 1;
        }
        if let Some(price) = price_for(&rec.provider) {
            let c = rec.cost(&price);
            r.total += c.total();
            r.without_cache += c.without_cache(&price, rec.peak);
        }
    }
    r
}

/// 现在是不是高峰时段。常驻 Agent 可以把重任务排到空闲时段省一半。
pub fn peak_now() -> bool {
    use chrono::{Datelike, Local, Timelike};
    let now = Local::now();
    is_peak_hour(now.weekday(), now.hour(), now.minute())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(seq: u64, data: serde_json::Value) -> Event {
        Event {
            seq,
            at: seq,
            kind: EventKind::ModelCalled,
            span: None,
            job: None,
            data,
        }
    }

    fn rec_json(provider: &str, hit: u64, miss: u64, peak: bool) -> serde_json::Value {
        json!({
            "provider": provider,
            "model": "m",
            "peak": peak,
            "thinking": false,
            "usage": {
                "prompt_tokens": hit + miss,
                "completion_tokens": 100,
                "total_tokens": hit + miss + 100,
                "prompt_cache_hit_tokens": hit,
                "prompt_cache_miss_tokens": miss,
            }
        })
    }

    #[test]
    fn empty_events_give_a_zero_report() {
        let r = project_costs(&[]);
        assert_eq!(r.calls, 0);
        assert_eq!(r.total, 0.0);
        assert_eq!(r.cache_hit_rate(), None, "没有数据就是不知道，不是 0");
    }

    #[test]
    fn deepseek_cache_hits_are_cheaper_than_misses() {
        let hit = project_costs(&[ev(1, rec_json("deepseek", 1000, 0, false))]);
        let miss = project_costs(&[ev(1, rec_json("deepseek", 0, 1000, false))]);
        assert!(
            hit.total < miss.total,
            "命中应更便宜: {} vs {}",
            hit.total,
            miss.total
        );
        assert!(hit.saved() > 0.0);
        assert_eq!(hit.cache_hit_rate(), Some(1.0));
        assert_eq!(miss.cache_hit_rate(), Some(0.0));
    }

    #[test]
    fn unknown_provider_counts_as_a_call_but_costs_nothing() {
        // 宁可金额偏小，也不要拿错的价格表算出一个看似精确的数字
        let r = project_costs(&[ev(1, rec_json("some-new-vendor", 100, 100, false))]);
        assert_eq!(r.calls, 1);
        assert_eq!(r.total, 0.0);
        assert_eq!(price_for("some-new-vendor"), None);
    }

    #[test]
    fn provider_prices_are_not_shared() {
        // Agnes 是 10 倍缓存折扣，DeepSeek 是 50 倍——不能共用常数
        let a = price_for("agnes").unwrap();
        let d = price_for("deepseek").unwrap();
        assert!((a.input_idle / a.cache_hit_idle - 10.0).abs() < 1e-9);
        assert!((d.input_idle / d.cache_hit_idle - 50.0).abs() < 1e-9);
    }

    #[test]
    fn calls_missing_cache_data_are_flagged_not_assumed() {
        // 服务端没回缓存字段时，不能假设命中也不能假设未命中
        let mut r = rec_json("agnes", 0, 0, false);
        r["usage"] = json!({ "prompt_tokens": 50, "completion_tokens": 10, "total_tokens": 60 });
        let report = project_costs(&[ev(1, r)]);
        assert_eq!(report.calls_without_cache_data, 1);
        assert_eq!(report.cache_hit_rate(), None);
    }

    #[test]
    fn peak_costs_more_than_idle_for_the_same_tokens() {
        let peak = project_costs(&[ev(1, rec_json("deepseek", 0, 1000, true))]);
        let idle = project_costs(&[ev(1, rec_json("deepseek", 0, 1000, false))]);
        assert!(peak.total > idle.total);
        assert!(
            (idle.total * 2.0 - peak.total).abs() < 1e-9,
            "空闲应正好一半"
        );
    }

    #[test]
    fn peak_flag_is_stored_not_recomputed() {
        // 时段写进记录里，否则事后算钱会用到"今天"的时段
        let rec = CallRecord::new("deepseek", "deepseek-flash", true);
        let v = serde_json::to_value(&rec).unwrap();
        assert_eq!(v["peak"], json!(true));
    }

    #[test]
    fn thinking_calls_are_counted_separately() {
        let mut r = rec_json("deepseek", 0, 100, false);
        r["thinking"] = json!(true);
        let report = project_costs(&[ev(1, r)]);
        assert_eq!(report.thinking_calls, 1);
    }

    #[test]
    fn malformed_records_do_not_panic() {
        // 台账是外部可编辑的文本文件，坏行不能让汇总崩掉
        let report = project_costs(&[ev(1, json!({ "provider": 123 }))]);
        assert_eq!(report.calls, 0);
    }

    #[test]
    fn report_totals_accumulate_across_calls() {
        let report = project_costs(&[
            ev(1, rec_json("agnes", 0, 100, false)),
            ev(2, rec_json("deepseek", 500, 500, true)),
        ]);
        assert_eq!(report.calls, 2);
        // 第一次 prompt = 0 命中 + 100 未命中 = 100；第二次 = 500 + 500 = 1000
        assert_eq!(report.prompt_tokens, 1100);
        assert_eq!(report.completion_tokens, 200);
        assert_eq!(report.cache_hit_tokens, 500);
        assert_eq!(report.cache_miss_tokens, 600);
    }

    #[test]
    fn call_record_keeps_task_attribution() {
        let rec = CallRecord::new("agnes", "agnes-3.0-flash", false).for_step("t1", "s2");
        assert_eq!(rec.task.as_deref(), Some("t1"));
        assert_eq!(rec.step.as_deref(), Some("s2"));
    }
}
