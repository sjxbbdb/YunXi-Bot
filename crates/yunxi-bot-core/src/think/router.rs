//! 模型选择器：按**预计需要多少次调用**在模型间路由。
//!
//! ## 判据是"调用次数"，不是"推理深度"
//!
//! 这一点值得写清楚，因为它反直觉：多轮任务换 DeepSeek，**主要是在解 RPM 瓶颈，
//! 不是在图更强的推理**。
//!
//! | | Agnes 3.0 Flash | DeepSeek Flash |
//! |---|---|---|
//! | RPM | **10**（免费档） | 并发 2500，基本无等待 |
//! | 6 次调用要多久 | ≈ 36 秒（光是等） | ≈ 几秒 |
//! | 价格 | 当前免费 | 空闲 ¥1/M 输入、¥4/M 输出 |
//! | 缓存折扣 | 10 倍 | **50 倍** |
//!
//! 所以：**少调用走 Agnes（免费且够快），多调用走 DeepSeek（不然光等就把任务拖死）。**
//!
//! ## 三层判断，从便宜到贵
//!
//! ```text
//! 1. 确定性信号（不花钱、不耗时）
//!      任务长度 / 显式枚举 / 串联词 / 代码
//!      → 能定就定
//!              ↓ 拿不准
//! 2. 本地 Verdict（免费、约 15ms、离线）
//!      一个 choice 问题："这个任务预计需要几步？"
//!              ↓ 它也弃权
//! 3. 保守兜底：Agnes（免费的那个）
//! ```
//!
//! **顺序不能反。** 每次都问模型判复杂度，等于为了省钱先花钱。
//!
//! ## 只做一次决策
//!
//! 路由在**任务开始时决定一次**，整个任务用同一个模型。除了简单，这也和缓存
//! 一致：中途换模型会把已构建的前缀缓存全部作废。

use serde::{Deserialize, Serialize};

use super::cost::PriceTable;
use crate::decide::Decider;

/// 能力档位。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    /// 轻量：一两次调用就完事。走免费的那个。
    Cheap,
    /// 标准：默认档。
    Standard,
    /// 深度：需要多次调用的多轮任务。
    Deep,
}

impl Tier {
    pub fn label(self) -> &'static str {
        match self {
            Tier::Cheap => "轻量",
            Tier::Standard => "标准",
            Tier::Deep => "深度",
        }
    }
}

/// DeepSeek 的思考模式。
///
/// **默认是开的，而且这是成本大头。** 实测同一个问题：
///
/// | 模式 | 输出 token | 其中思考 |
/// |---|---|---|
/// | 默认（开，effort=high） | 62 | 55 |
/// | 关掉 | **14** | 0 |
/// | effort=low | 63 | 56 |
///
/// 也就是说：**关掉思考能省约 77% 的输出 token**，而 `effort=low` 对这种
/// 简单问题几乎不起作用——唯一有效的杠杆就是开/关。
///
/// 另外注意：**思考模式下 `temperature` 不生效**（官方文档：设了不报错，但也不生效）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Thinking {
    /// 不传该字段，由服务端默认（对 DeepSeek 就是**开**）。
    ServerDefault,
    Enabled,
    Disabled,
}

impl Thinking {
    /// 需要往请求体里塞的字段。`None` 表示不传。
    pub fn body_field(self) -> Option<serde_json::Value> {
        match self {
            Thinking::ServerDefault => None,
            Thinking::Enabled => Some(serde_json::json!({ "type": "enabled" })),
            Thinking::Disabled => Some(serde_json::json!({ "type": "disabled" })),
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Thinking::ServerDefault => "服务端默认",
            Thinking::Enabled => "开",
            Thinking::Disabled => "关",
        }
    }
}

/// 一个模型端点的完整描述。
#[derive(Debug, Clone, PartialEq)]
pub struct ModelSpec {
    pub tier: Tier,
    /// 用于台账归因的短名。
    pub provider: &'static str,
    pub base_url: &'static str,
    pub model: &'static str,
    pub price: PriceTable,
    /// 每分钟请求数上限。Agnes 免费档是 10。
    pub rpm: u32,
    /// 该档默认的思考模式。
    pub thinking: Thinking,
    /// 密钥位置提示。
    pub key_hint: &'static str,
}

impl ModelSpec {
    /// Agnes 3.0 Flash：轻量/默认档。512K 上下文、支持工具调用、当前免费。
    ///
    /// 选它做默认的理由：**免费 + 配额够用**。10 RPM 对一两次调用绰绰有余。
    pub const AGNES_FLASH: Self = Self {
        tier: Tier::Standard,
        provider: "agnes",
        base_url: "https://api.agnes-ai.cn/v1",
        model: "agnes-3.0-flash",
        price: PriceTable::AGNES_30_FLASH,
        rpm: 10,
        thinking: Thinking::ServerDefault,
        key_hint: "secrets/agnes.key 或 YUNXI_BOT_AGNES_KEY",
    };

    /// DeepSeek Flash：深度档。用于**需要多次调用**的多轮任务。
    ///
    /// 模型名是 `deepseek-flash`（DeepSeek-V4.1-Flash）。
    /// 默认**关思考**——多轮任务里每步都思考会把输出 token 翻几倍，
    /// 而多轮任务的价值在于"能跑完"，不在于每步都想得很深。
    /// 需要深度思考的步骤单独开。
    pub const DEEPSEEK_FLASH: Self = Self {
        tier: Tier::Deep,
        provider: "deepseek",
        base_url: "https://api.deepseek.com/v1",
        model: "deepseek-flash",
        price: PriceTable::DEEPSEEK_FLASH,
        rpm: 60,
        thinking: Thinking::Disabled,
        key_hint: "secrets/deepseek.key 或 YUNXI_BOT_DEEPSEEK_KEY",
    };
}

/// 阈值。**集中在一处**，便于按实际账单调整。
pub mod thresholds {
    /// 超过这个调用次数就换 DeepSeek。
    ///
    /// 依据：Agnes 免费档 10 RPM ≈ 每次间隔 6 秒。4 次调用要等约 18 秒，
    /// 还在"能接受"的范围；再多就该换到不排队的那个了。
    pub const MULTI_CALL: usize = 4;
    /// 短任务判定阈值（字符）。
    pub const SHORT_PROMPT_CHARS: usize = 200;
}

/// 任务画像。**只描述"要干多少活"，不描述"有多难"**。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TaskProfile {
    pub prompt_chars: usize,
    /// 已知或预估的步骤数。还没拆解时为 0。
    pub step_count: usize,
    /// 任务里是否显式列了多条（"1. … 2. …"或"、"分隔的多个动作）。
    pub explicit_multi: bool,
    pub has_code: bool,
}

impl TaskProfile {
    /// 预计需要的模型调用次数。**这是路由的主要依据。**
    ///
    /// 拆解本身算 1 次；每个步骤至少 1 次（生成内容或生成命令）；
    /// 每个决策点还要 1 次生成选项。没有步骤数时用任务长度粗估。
    pub fn estimated_calls(&self) -> usize {
        let steps = if self.step_count > 0 {
            self.step_count
        } else {
            // 没拆解前粗估：每 300 字算一步，至少 1 步
            (self.prompt_chars / 300).max(1)
        };
        // 拆解 1 次 + 每步 1 次 + 决策点按步数的一半估
        1 + steps + steps / 2
    }

    /// 确定性判断。拿不准返回 `None`。
    ///
    /// **`None` 不是失败**——它是"该问下一步了"的信号。
    pub fn heuristic_tier(&self) -> Option<Tier> {
        let calls = self.estimated_calls();
        if calls > thresholds::MULTI_CALL {
            return Some(Tier::Deep);
        }
        // 明确的小任务
        if self.prompt_chars > 0
            && self.prompt_chars < thresholds::SHORT_PROMPT_CHARS
            && self.step_count <= 1
            && !self.explicit_multi
            && !self.has_code
        {
            return Some(Tier::Cheap);
        }
        None
    }
}

/// 一次路由决定。**理由必须进台账**，否则事后无法解释为什么花了钱。
#[derive(Debug, Clone, PartialEq)]
pub struct Routing {
    pub tier: Tier,
    pub reason: String,
    /// 预计调用次数。
    pub estimated_calls: usize,
    /// 是否由本地决策模型参与判断。
    pub used_decider: bool,
}

/// 从任务文本识别"多条动作"信号。
pub fn detect_explicit_multi(text: &str) -> bool {
    // 显式枚举：1. / 2. / 一、/ 二、
    let has_numbering = text.contains("1.")
        || text.contains("1、")
        || text.contains("一、")
        || text.contains("2.")
        || text.contains("首先");
    // 串联词
    const CHAIN: [&str; 6] = ["然后", "接着", "之后", "再", "并且", "同时"];
    let has_chain = CHAIN.iter().any(|m| text.contains(m));
    has_numbering || has_chain
}

/// 从文本识别代码信号。
pub fn detect_code(text: &str) -> bool {
    const TOKENS: [&str; 8] = ["```", "代码", "脚本", "函数", "编译", "报错", "bug", ".rs"];
    TOKENS.iter().any(|t| text.contains(t))
}

/// 问本地决策模型时用的判据。
pub fn call_count_criteria() -> [(&'static str, &'static str); 3] {
    [
        ("few", "一两次调用就能完成，不需要来回多轮"),
        ("several", "需要四五次调用，中间有几轮来回"),
        ("many", "需要很多轮调用，要拆成多个步骤逐步完成"),
    ]
}

/// 模型选择器。
#[derive(Debug, Clone)]
pub struct ModelRouter {
    specs: Vec<ModelSpec>,
    default_tier: Tier,
}

impl Default for ModelRouter {
    fn default() -> Self {
        Self {
            specs: vec![ModelSpec::AGNES_FLASH, ModelSpec::DEEPSEEK_FLASH],
            // 拿不准时走免费的那个
            default_tier: Tier::Standard,
        }
    }
}

impl ModelRouter {
    pub fn new(specs: Vec<ModelSpec>, default_tier: Tier) -> Self {
        Self {
            specs,
            default_tier,
        }
    }

    pub fn specs(&self) -> &[ModelSpec] {
        &self.specs
    }

    /// 取某一档的模型。该档没配就回落到默认档——**路由不该因为缺配置而失败**。
    pub fn spec(&self, tier: Tier) -> Option<&ModelSpec> {
        self.specs
            .iter()
            .find(|s| s.tier == tier)
            .or_else(|| self.specs.iter().find(|s| s.tier == self.default_tier))
            .or_else(|| self.specs.first())
    }

    /// 路由。**整个任务只调一次。**
    pub fn route(
        &self,
        profile: &TaskProfile,
        decider: Option<&dyn Decider>,
        task_text: &str,
    ) -> Routing {
        let calls = profile.estimated_calls();

        // 第一层：确定性信号
        if let Some(tier) = profile.heuristic_tier() {
            return Routing {
                tier,
                reason: format!(
                    "确定性信号：预计 {calls} 次调用（{} 字 / {} 步 / 多条={} / 代码={}）",
                    profile.prompt_chars,
                    profile.step_count,
                    profile.explicit_multi,
                    profile.has_code
                ),
                estimated_calls: calls,
                used_decider: false,
            };
        }

        // 第二层：本地决策模型
        if let Some(decider) = decider {
            if let Some(tier) = self.ask_local(decider, task_text) {
                return Routing {
                    tier,
                    reason: "本地决策模型判定调用次数".into(),
                    estimated_calls: calls,
                    used_decider: true,
                };
            }
        }

        // 第三层：兜底走免费的那个
        Routing {
            tier: self.default_tier,
            reason: "信号不足且本地判断弃权，回落默认档（免费）".into(),
            estimated_calls: calls,
            used_decider: false,
        }
    }

    /// 问本地决策模型。任何异常都返回 `None` 由调用方兜底——
    /// **路由失败不能把任务卡住**。
    fn ask_local(&self, decider: &dyn Decider, task_text: &str) -> Option<Tier> {
        use crate::decide::{DecisionRequest, Question};

        let question = Question::choice(
            "call_count",
            "完成这个任务预计需要多少次模型调用？",
            &call_count_criteria(),
        );
        let req = DecisionRequest::new(serde_json::json!({ "task": task_text }), vec![question]);
        let result = decider.decide(&req).ok()?;
        match result.choice("call_count") {
            Some("few") => Some(Tier::Cheap),
            Some("several") => Some(Tier::Standard),
            Some("many") => Some(Tier::Deep),
            _ => None,
        }
    }
}

/// 从任务文本算出画像。
pub fn profile_task(task: &str, step_count: usize) -> TaskProfile {
    TaskProfile {
        prompt_chars: task.chars().count(),
        step_count,
        explicit_multi: detect_explicit_multi(task),
        has_code: detect_code(task),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decide::StubDecider;
    use crate::think::cost::is_peak_hour;

    #[test]
    fn multi_call_tasks_go_to_deepseek() {
        // 这是路由的核心判据：调用次数多就换不排队的那个
        let p = TaskProfile {
            prompt_chars: 100,
            step_count: 8,
            ..Default::default()
        };
        assert!(p.estimated_calls() > thresholds::MULTI_CALL);
        assert_eq!(p.heuristic_tier(), Some(Tier::Deep));
    }

    #[test]
    fn few_call_tasks_stay_on_the_free_one() {
        let p = TaskProfile {
            prompt_chars: 30,
            step_count: 1,
            ..Default::default()
        };
        assert!(p.estimated_calls() <= thresholds::MULTI_CALL);
        assert_eq!(p.heuristic_tier(), Some(Tier::Cheap));
    }

    #[test]
    fn chained_instructions_count_as_multiple_steps() {
        let p = profile_task("先备份数据库，然后把日志打包，再发我邮箱", 0);
        assert!(p.explicit_multi);
        // 没拆解时按长度粗估，但"多条"信号会阻止它被判成轻量
        assert_ne!(p.heuristic_tier(), Some(Tier::Cheap));
    }

    #[test]
    fn estimated_calls_scales_with_steps() {
        let one = TaskProfile {
            step_count: 1,
            ..Default::default()
        };
        let ten = TaskProfile {
            step_count: 10,
            ..Default::default()
        };
        assert!(ten.estimated_calls() > one.estimated_calls());
        // 1 + 步骤 + 步骤/2
        assert_eq!(ten.estimated_calls(), 1 + 10 + 5);
    }

    #[test]
    fn heuristic_path_never_consults_the_decider() {
        // 每次都问模型判复杂度，等于为了省钱先花钱
        let router = ModelRouter::default();
        let stub = StubDecider::succeeding().with_choice("call_count", "many");
        let p = TaskProfile {
            prompt_chars: 30,
            step_count: 1,
            ..Default::default()
        };
        let r = router.route(&p, Some(&stub), "买杯咖啡");
        assert!(!r.used_decider);
        assert_eq!(stub.calls(), 0, "确定性信号能定就不该问模型");
    }

    #[test]
    fn ambiguous_falls_through_to_local_decider() {
        let router = ModelRouter::default();
        let stub = StubDecider::succeeding().with_choice("call_count", "many");
        let p = TaskProfile {
            prompt_chars: 500,
            step_count: 2,
            ..Default::default()
        };
        assert_eq!(p.heuristic_tier(), None, "这个画像应判不出来");
        let r = router.route(&p, Some(&stub), "整理这周的邮件");
        assert_eq!(r.tier, Tier::Deep);
        assert!(r.used_decider);
        assert_eq!(stub.calls(), 1);
    }

    #[test]
    fn decider_failure_falls_back_to_the_free_tier() {
        let router = ModelRouter::default();
        let stub = StubDecider::failing("连接被拒绝");
        let p = TaskProfile {
            prompt_chars: 500,
            step_count: 2,
            ..Default::default()
        };
        let r = router.route(&p, Some(&stub), "x");
        assert_eq!(r.tier, Tier::Standard, "判断失败应回落免费档");
        assert!(
            r.reason.contains("免费"),
            "理由要说清回落到了哪: {}",
            r.reason
        );
    }

    #[test]
    fn unknown_choice_is_treated_as_abstention() {
        let router = ModelRouter::default();
        let stub = StubDecider::succeeding().with_choice("call_count", "nonsense");
        let p = TaskProfile {
            prompt_chars: 500,
            step_count: 2,
            ..Default::default()
        };
        let r = router.route(&p, Some(&stub), "x");
        assert_eq!(r.tier, Tier::Standard, "未知选项不该被当成某一档");
    }

    #[test]
    fn default_chain_is_agnes_plus_deepseek() {
        let router = ModelRouter::default();
        assert_eq!(router.spec(Tier::Standard).unwrap().provider, "agnes");
        let deep = router.spec(Tier::Deep).unwrap();
        assert_eq!(deep.provider, "deepseek");
        assert_eq!(deep.model, "deepseek-flash", "模型名不能写错");
    }

    #[test]
    fn deepseek_cache_discount_is_fifty_times_not_ten() {
        // 我最初按 Agnes 的 10% 类推，写成了 10 倍——**是错的**
        let p = PriceTable::DEEPSEEK_FLASH;
        assert!((p.input_idle / p.cache_hit_idle - 50.0).abs() < 1e-9);
        // Agnes 才是 10 倍
        let a = PriceTable::AGNES_30_FLASH;
        assert!((a.input_idle / a.cache_hit_idle - 10.0).abs() < 1e-9);
    }

    #[test]
    fn deepseek_thinking_is_off_by_default_in_our_config() {
        // 实测：开思考时输出 token 是关掉时的 4 倍多
        assert_eq!(ModelSpec::DEEPSEEK_FLASH.thinking, Thinking::Disabled);
        assert_eq!(
            ModelSpec::DEEPSEEK_FLASH.thinking.body_field(),
            Some(serde_json::json!({"type": "disabled"}))
        );
    }

    #[test]
    fn server_default_sends_no_thinking_field() {
        // Agnes 不吃 thinking 字段，就不能给它塞
        assert_eq!(ModelSpec::AGNES_FLASH.thinking.body_field(), None);
    }

    #[test]
    fn peak_and_idle_pricing_differs_by_half() {
        let p = PriceTable::DEEPSEEK_FLASH;
        assert!((p.idle_discount() - 0.5).abs() < 1e-9, "空闲时段应便宜一半");
        assert!(p.rates_at(true).peak);
        assert!(p.rates_at(false).input < p.rates_at(true).input);
    }

    #[test]
    fn peak_hours_are_beijing_weekday_windows() {
        use chrono::Weekday;
        assert!(is_peak_hour(Weekday::Mon, 10, 0), "周一 10 点应算高峰");
        assert!(is_peak_hour(Weekday::Fri, 15, 0));
        assert!(!is_peak_hour(Weekday::Mon, 12, 30), "午休不算高峰");
        assert!(!is_peak_hour(Weekday::Mon, 19, 0));
        assert!(!is_peak_hour(Weekday::Sat, 10, 0), "周末全天空闲");
        assert!(!is_peak_hour(Weekday::Sun, 15, 0));
    }

    #[test]
    fn routing_reason_is_always_present() {
        let router = ModelRouter::default();
        for p in [
            TaskProfile {
                prompt_chars: 30,
                step_count: 1,
                ..Default::default()
            },
            TaskProfile {
                step_count: 20,
                ..Default::default()
            },
            TaskProfile {
                prompt_chars: 500,
                step_count: 2,
                ..Default::default()
            },
        ] {
            let r = router.route(&p, None, "x");
            assert!(!r.reason.is_empty(), "路由理由必须可入账");
            assert!(r.estimated_calls >= 1);
        }
    }

    #[test]
    fn missing_tier_falls_back_to_default() {
        let router = ModelRouter::new(vec![ModelSpec::AGNES_FLASH], Tier::Standard);
        let s = router.spec(Tier::Deep).expect("应回落到默认档");
        assert_eq!(s.provider, "agnes");
    }
}
