//! 成本记账：把 token 用量换算成钱，并暴露缓存命中率。
//!
//! ## 为什么要单独做这一层
//!
//! 缓存命中的单价和未命中差 **10 倍**（DeepSeek 与 Agnes 都是：
//! 命中价 = 普通输入价的 10%）。这意味着"省不省钱"几乎完全取决于
//! **提示词前缀有没有保持字节稳定**——而不是取决于用了哪个模型。
//!
//! 不记账就看不见这件事：同样的任务，命中率从 0% 提到 80%，成本差好几倍，
//! 但在日志里只有一个"成功"。

use serde::{Deserialize, Serialize};

/// 一个模型的价目表（单位：元 / 百万 token）。
///
/// **高峰与空闲分开存**：DeepSeek 空闲时段是高峰价的**一半**，而常驻 Agent
/// 完全可以把重任务排到空闲时段——那是白捡的 50%。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PriceTable {
    pub cache_hit_peak: f64,
    pub cache_hit_idle: f64,
    pub input_peak: f64,
    pub input_idle: f64,
    pub output_peak: f64,
    pub output_idle: f64,
    /// 是否当前免费（促销）。免费时仍然记账——**将来收费时才知道花了多少**。
    pub free: bool,
    /// 价格来源与核对日期，便于日后复核。
    pub source: &'static str,
}

/// 某一时刻生效的三档单价。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rates {
    pub cache_hit: f64,
    pub input: f64,
    pub output: f64,
    pub peak: bool,
}

impl PriceTable {
    /// DeepSeek `deepseek-flash`（DeepSeek-V4.1-Flash）官方刊例价。
    ///
    /// 来源：<https://api-docs.deepseek.com/zh-cn/quick_start/pricing/>（2026-10 核对）
    ///
    /// ⚠️ **缓存命中与未命中差 50 倍**（0.02 vs 1）。这意味着省钱的杠杆
    /// 几乎完全在"提示词前缀有没有保持字节稳定"上，而不在选哪个模型上。
    pub const DEEPSEEK_FLASH: Self = Self {
        cache_hit_peak: 0.04,
        cache_hit_idle: 0.02,
        input_peak: 2.0,
        input_idle: 1.0,
        output_peak: 8.0,
        output_idle: 4.0,
        free: false,
        source: "deepseek-flash 刊例价 2026-10",
    };

    /// Agnes 3.0 Flash 刊例价（现价 ¥0，保留刊例价以便估算）。
    ///
    /// 来源：<https://wiki.agnes-ai.cn/zh-Hans/docs/pricing>（2026-10 核对）
    /// Agnes 的缓存折扣是 **10%**（10 倍），与 DeepSeek 的 50 倍不同。
    /// 本地模型：**不产生 API 费用**。
    ///
    /// 全零而不是"照 Agnes 记 0.35"——Agnes 那档虽然 `free: true`，
    /// 但刊例价是真的（哪天收费了，改 `free` 一个字就行）。而本地模型
    /// **压根没有刊例价**，硬填一个数会让成本报表凭空多出一列假数字。
    ///
    /// 电费和显卡折旧不进这个表：那是"要不要跑本地模型"的决策成本，
    /// 不是"这一句话花了多少"的边际成本。**两笔账混在一起就都算不清了。**
    pub const LOCAL: Self = Self {
        cache_hit_peak: 0.0,
        cache_hit_idle: 0.0,
        input_peak: 0.0,
        input_idle: 0.0,
        output_peak: 0.0,
        output_idle: 0.0,
        free: true,
        source: "本地模型（sidecar/local_llm_server.py）：无 API 费用",
    };

    pub const AGNES_30_FLASH: Self = Self {
        cache_hit_peak: 0.035,
        cache_hit_idle: 0.035,
        input_peak: 0.35,
        input_idle: 0.35,
        output_peak: 1.0,
        output_idle: 1.0,
        free: true,
        source: "agnes-3.0-flash 刊例价 2026-10",
    };

    /// 该时刻生效的单价。
    pub fn rates_at(&self, peak: bool) -> Rates {
        if peak {
            Rates {
                cache_hit: self.cache_hit_peak,
                input: self.input_peak,
                output: self.output_peak,
                peak: true,
            }
        } else {
            Rates {
                cache_hit: self.cache_hit_idle,
                input: self.input_idle,
                output: self.output_idle,
                peak: false,
            }
        }
    }

    /// 空闲时段相对高峰省下的比例。
    pub fn idle_discount(&self) -> f64 {
        if self.input_peak <= 0.0 {
            return 0.0;
        }
        1.0 - self.input_idle / self.input_peak
    }
}

/// DeepSeek 高峰时段判定。
///
/// 规则（官方文档）：北京时间**周一至周五** 9:00–12:00 与 14:00–18:00，
/// **不含中国法定节假日**；其余时段（含周末与节假日全天）均为空闲。
///
/// ⚠️ **不处理法定节假日**——那需要一张年历表。缺失的后果是：节假日会按
/// 高峰价估算，也就是**高估成本**。这是刻意选的方向：宁可高估花费，
/// 也不要虚报节省。
pub fn is_peak_hour(weekday: chrono::Weekday, hour: u32, minute: u32) -> bool {
    use chrono::Weekday;
    if matches!(weekday, Weekday::Sat | Weekday::Sun) {
        return false;
    }
    let t = hour * 60 + minute;
    (9 * 60..12 * 60).contains(&t) || (14 * 60..18 * 60).contains(&t)
}

/// 一次调用的 token 用量。
///
/// 注意 `prompt_tokens` 与 `cache_hit_tokens + cache_miss_tokens` 的关系
/// **各家不一致**，所以这里两个都记，不假设谁等于谁。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub prompt_tokens: u64,
    #[serde(default)]
    pub completion_tokens: u64,
    #[serde(default)]
    pub total_tokens: u64,
    /// DeepSeek：`usage.prompt_cache_hit_tokens`
    #[serde(default, alias = "prompt_cache_hit_tokens")]
    pub cache_hit_tokens: u64,
    /// DeepSeek：`usage.prompt_cache_miss_tokens`
    #[serde(default, alias = "prompt_cache_miss_tokens")]
    pub cache_miss_tokens: u64,
}

impl Usage {
    /// 缓存命中率。没有输入 token 时返回 `None`（而不是 0，避免误读）。
    pub fn cache_hit_rate(&self) -> Option<f64> {
        let total = self.cache_hit_tokens + self.cache_miss_tokens;
        if total == 0 {
            return None;
        }
        Some(self.cache_hit_tokens as f64 / total as f64)
    }

    /// 报告里能不能看出缓存数据。DeepSeek 会给，Agnes 目前不给。
    pub fn has_cache_data(&self) -> bool {
        self.cache_hit_tokens + self.cache_miss_tokens > 0
    }

    /// 输入计费的 token 数：优先用命中+未命中，缺失时退回 `prompt_tokens`。
    fn billable_input(&self) -> (u64, u64) {
        if self.has_cache_data() {
            (self.cache_hit_tokens, self.cache_miss_tokens)
        } else {
            // 没有缓存数据时，全部按未命中计——**宁可高估成本，也不虚报节省**
            (0, self.prompt_tokens)
        }
    }

    /// 按当时生效的单价算这次调用花了多少钱（元）。
    ///
    /// `peak` 决定用高峰价还是空闲价。**拿不准就传 `true`（高峰）**——
    /// 宁可高估成本，也不虚报节省。
    pub fn cost(&self, price: &PriceTable, peak: bool) -> Cost {
        let r = price.rates_at(peak);
        let (hit, miss) = self.billable_input();
        let m = 1_000_000.0;
        Cost {
            cache_hit: hit as f64 * r.cache_hit / m,
            input: miss as f64 * r.input / m,
            output: self.completion_tokens as f64 * r.output / m,
            cache_hit_tokens: hit,
            cache_miss_tokens: miss,
            output_tokens: self.completion_tokens,
            has_cache_data: self.has_cache_data(),
        }
    }
}

/// 一次调用的成本拆解（元）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Cost {
    pub cache_hit: f64,
    pub input: f64,
    pub output: f64,
    pub cache_hit_tokens: u64,
    pub cache_miss_tokens: u64,
    pub output_tokens: u64,
    /// 这次响应里**是否真的带回了缓存字段**。alse 时下面的 cache_miss_tokens
    /// 是回退估算（按全部未命中计），不是服务端报的数。
    pub has_cache_data: bool,
}

impl Cost {
    pub fn total(&self) -> f64 {
        self.cache_hit + self.input + self.output
    }

    /// 假如**全部未命中**要花多少。用来量化"缓存省了多少钱"。
    pub fn without_cache(&self, price: &PriceTable, peak: bool) -> f64 {
        let r = price.rates_at(peak);
        let m = 1_000_000.0;
        let all_input = (self.cache_hit_tokens + self.cache_miss_tokens) as f64;
        all_input * r.input / m + self.output_tokens as f64 * r.output / m
    }

    /// 缓存省下的金额。
    pub fn saved(&self, price: &PriceTable, peak: bool) -> f64 {
        (self.without_cache(price, peak) - self.total()).max(0.0)
    }

    /// 一行摘要，进台账用。
    pub fn summary(&self, price: &PriceTable, peak: bool) -> String {
        let period = if peak { "高峰" } else { "空闲" };
        // 用「响应里到底有没有缓存字段」判断，而不是看 token 数——
        // 没有缓存字段时 cache_miss_tokens 是回退估算，拿它说"省了钱"是假话。
        if self.has_cache_data {
            format!(
                "命中 {}/{} tok（省 ¥{:.6}）{period}合计 ¥{:.6}",
                self.cache_hit_tokens,
                self.cache_hit_tokens + self.cache_miss_tokens,
                self.saved(price, peak),
                self.total()
            )
        } else {
            format!(
                "输入 {} tok（无缓存数据，按未命中计）{period}合计 ¥{:.6}",
                self.cache_miss_tokens,
                self.total()
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(hit: u64, miss: u64, out: u64) -> Usage {
        Usage {
            prompt_tokens: hit + miss,
            completion_tokens: out,
            total_tokens: hit + miss + out,
            cache_hit_tokens: hit,
            cache_miss_tokens: miss,
        }
    }

    #[test]
    fn parses_deepseek_cache_field_names() {
        // 字段名来自 DeepSeek 文档与实测响应：
        //   usage.prompt_cache_hit_tokens / usage.prompt_cache_miss_tokens
        let json = r#"{
            "prompt_tokens": 100,
            "completion_tokens": 20,
            "total_tokens": 120,
            "prompt_cache_hit_tokens": 80,
            "prompt_cache_miss_tokens": 20
        }"#;
        let u: Usage = serde_json::from_str(json).expect("应能解析");
        assert_eq!(u.cache_hit_tokens, 80);
        assert_eq!(u.cache_miss_tokens, 20);
        assert_eq!(u.cache_hit_rate(), Some(0.8));
        assert!(u.has_cache_data());
    }

    #[test]
    fn parses_real_deepseek_response_shape() {
        // 实测响应还带 prompt_tokens_details / reasoning_tokens，
        // 解析不能因为它们而失败
        let json = r#"{
            "prompt_tokens": 44,
            "completion_tokens": 32,
            "total_tokens": 76,
            "prompt_tokens_details": {"cached_tokens": 0},
            "completion_tokens_details": {"reasoning_tokens": 32},
            "prompt_cache_hit_tokens": 0,
            "prompt_cache_miss_tokens": 44
        }"#;
        let u: Usage = serde_json::from_str(json).expect("真实响应应能解析");
        assert_eq!(u.cache_miss_tokens, 44);
        assert_eq!(u.cache_hit_rate(), Some(0.0));
        assert!(u.has_cache_data(), "命中 0 也是有效数据，和'没有数据'不同");
    }

    #[test]
    fn parses_response_without_cache_fields() {
        // Agnes 当前不返回缓存字段，不能因此解析失败
        let json = r#"{"prompt_tokens": 97, "completion_tokens": 9, "total_tokens": 106}"#;
        let u: Usage = serde_json::from_str(json).expect("缺缓存字段也应能解析");
        assert_eq!(u.prompt_tokens, 97);
        assert!(!u.has_cache_data());
        assert_eq!(
            u.cache_hit_rate(),
            None,
            "没有数据应返回 None 而不是假装 0%"
        );
    }

    #[test]
    fn missing_cache_data_bills_everything_as_miss() {
        // 没有缓存数据时**宁可高估成本**，不虚报节省
        let u = Usage {
            prompt_tokens: 1000,
            completion_tokens: 100,
            total_tokens: 1100,
            ..Default::default()
        };
        let c = u.cost(&PriceTable::DEEPSEEK_FLASH, true);
        assert_eq!(c.cache_hit_tokens, 0);
        assert_eq!(c.cache_miss_tokens, 1000, "应全部按未命中计");
        assert_eq!(c.saved(&PriceTable::DEEPSEEK_FLASH, true), 0.0);
    }

    #[test]
    fn deepseek_cache_discount_is_fifty_times() {
        // ⚠️ 我最初按 Agnes 的 10% 类推，写成 10 倍——查了官方定价才发现是 50 倍
        let p = PriceTable::DEEPSEEK_FLASH;
        assert!((p.input_idle / p.cache_hit_idle - 50.0).abs() < 1e-9);
        assert!((p.input_peak / p.cache_hit_peak - 50.0).abs() < 1e-9);
    }

    #[test]
    fn agnes_cache_discount_is_ten_times() {
        // Agnes 的折扣是真的 10 倍，和 DeepSeek 不一样——不能混用一个常数
        let p = PriceTable::AGNES_30_FLASH;
        assert!((p.input_idle / p.cache_hit_idle - 10.0).abs() < 1e-9);
    }

    #[test]
    fn quantifies_what_the_cache_saved() {
        let p = PriceTable::DEEPSEEK_FLASH;
        // 空闲价：1M 未命中 = ¥1，1M 命中 = ¥0.02，省 ¥0.98
        let hit = usage(1_000_000, 0, 0).cost(&p, false);
        let miss = usage(0, 1_000_000, 0).cost(&p, false);
        assert!(
            (miss.input - 1.0).abs() < 1e-9,
            "未命中 100 万 tok 空闲价 = ¥1"
        );
        assert!(
            (hit.cache_hit - 0.02).abs() < 1e-9,
            "命中 100 万 tok = ¥0.02"
        );
        assert!((hit.saved(&p, false) - 0.98).abs() < 1e-9);
    }

    #[test]
    fn idle_hours_are_half_price() {
        let p = PriceTable::DEEPSEEK_FLASH;
        assert!((p.idle_discount() - 0.5).abs() < 1e-9);
        let peak = usage(0, 1_000_000, 0).cost(&p, true);
        let idle = usage(0, 1_000_000, 0).cost(&p, false);
        assert!(idle.total() < peak.total(), "空闲时段应更便宜");
        assert!((peak.total() / idle.total() - 2.0).abs() < 1e-9);
    }

    #[test]
    fn zero_input_has_no_hit_rate_rather_than_zero_percent() {
        let u = Usage::default();
        assert_eq!(u.cache_hit_rate(), None);
    }

    #[test]
    fn summary_is_readable_and_names_the_period() {
        let p = PriceTable::DEEPSEEK_FLASH;
        let peak = usage(800, 200, 50).cost(&p, true).summary(&p, true);
        assert!(peak.contains("命中 800/1000"), "{peak}");
        assert!(peak.contains("省 ¥"), "{peak}");
        assert!(peak.contains("高峰"), "{peak}");

        let idle = usage(800, 200, 50).cost(&p, false).summary(&p, false);
        assert!(idle.contains("空闲"), "{idle}");
    }

    #[test]
    fn summary_says_when_there_is_no_cache_data() {
        let p = PriceTable::DEEPSEEK_FLASH;
        let u = Usage {
            prompt_tokens: 500,
            completion_tokens: 10,
            total_tokens: 510,
            ..Default::default()
        };
        let s = u.cost(&p, true).summary(&p, true);
        assert!(s.contains("无缓存数据"), "{s}");
        assert!(!s.contains("省 ¥"), "没有缓存数据就不该声称省了钱: {s}");
    }

    #[test]
    fn price_tables_carry_their_source() {
        // 价格会变，所以每条都记来源——日后核对时才知道该去哪查
        assert!(PriceTable::DEEPSEEK_FLASH.source.contains("deepseek"));
        assert!(PriceTable::AGNES_30_FLASH.source.contains("agnes"));
    }
}
