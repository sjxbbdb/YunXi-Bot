//! 模型选择器：每个任务决定一次「用哪个模型 + 要不要思考」。
//!
//! ## 两个轴，不要混在一起
//!
//! | 轴 | 决定什么 | 依据 |
//! |---|---|---|
//! | **任务类型**（[`TaskKind`]） | **要不要思考** | 这活需要多少推理 |
//! | **调用次数**（[`TaskProfile`]） | **用哪个模型** | RPM 瓶颈 |
//!
//! 我最初把后者当成全部判据，理由写的是"多轮任务换 DeepSeek 主要在解 RPM 瓶颈，
//! 不是在图更强推理"。那句话只解释得通**模型**的选择，解释不通**思考模式**的
//! 选择——于是我把思考模式全局关掉了，这是错的。
//!
//! ### 实测：关思考省 60% token，但答案短 2.5 倍
//!
//! 同一个需要推理的问题（三货架四层六箱，每天出 17 箱）：
//!
//! | 模式 | 输出 token | 思考字符 | 答案字符 | 耗时 |
//! |---|---|---|---|---|
//! | 默认（不传字段）= 开 | 855 | 1037 | 218 | 4.2s |
//! | 显式 `enabled` | 815 | 941 | 253 | 3.9s |
//! | 显式 `disabled` | **340** | 0 | **558** | 2.1s |
//!
//! 省下的 60% 输出 token **正是推理本身**。所以正确的做法不是"全局关掉省钱"，
//! 而是**只在不需要推理的任务上关**——省掉的本来就是浪费。
//!
//! 另实测：**思考模式 + 工具调用不冲突**（`finish=tool_calls`，正常返回
//! `reasoning_content`）。这是任务执行框架能用思考模式的前提。
//!
//! ## 模型轴：判据是调用次数，不是推理深度
//!
//! | | Agnes 3.0 Flash | DeepSeek Flash |
//! |---|---|---|
//! | RPM | **10**（免费档） | 并发 2500，基本无等待 |
//! | 6 次调用要多久 | ≈ 36 秒（光是等） | ≈ 几秒 |
//! | 价格 | 当前免费 | 空闲 ¥1/M 输入、¥4/M 输出 |
//! | 缓存折扣 | 10 倍 | **50 倍** |
//!
//! **少调用走 Agnes（免费且够快），多调用走 DeepSeek（不然光等就把任务拖死）。**
//!
//! ## 三层判断，从便宜到贵
//!
//! ```text
//! 1. 确定性信号（不花钱、不耗时）
//!      任务长度 / 显式枚举 / 串联词 / 代码 / 推理词
//!      → 能定就定
//!              ↓ 拿不准
//! 2. 本地 Verdict（免费、约 15ms、离线）
//!      两个 choice 问题："要几次调用？""要多少推理？"
//!              ↓ 它也弃权
//! 3. 保守兜底：Agnes + 开思考
//! ```
//!
//! **顺序不能反。** 每次都问模型判复杂度，等于为了省钱先花钱。
//!
//! ## 每个任务只做一次决策
//!
//! 路由在**任务开始时决定一次**，这个任务全程用同一个模型 + 同一个思考模式。
//! 除了简单，这也和缓存一致：中途换模型会把已构建的前缀缓存全部作废。
//! 拆解出来的**每个子任务各自路由一次**——那是"每个任务"，不是"任务中途换"。

use serde::{Deserialize, Serialize};

use super::cost::{Cost, PriceTable, Usage, is_peak_hour};
use crate::decide::Decider;

/// 能力档位。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    /// 轻量：一两次调用就完事。走免费的那个。
    Cheap,
    /// 标准：默认档。
    Standard,
    /// 深度：需要多次调用、或需要推理的多轮任务。
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

/// 任务类型。**决定要不要开思考**。
///
/// 关键约束：**`kind` 是任务的属性，不是模型的属性。** 同一个任务在不同模型上
/// 是同一个 kind；是"这个活需要多少推理"决定思考模式，不是"这个模型能不能思考"。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskKind {
    /// 寒暄、确认、报个状态。不需要推理。
    Conversation,
    /// 查一个事实、取一个值、转述一条记录。
    Lookup,
    /// 起草一段文字、写一封邮件、生成一段代码。
    Generation,
    /// 改一个已有的东西（文件、配置、数据）。
    Edit,
    /// 分析、比较、诊断、算一笔账。**需要推理。**
    Analysis,
    /// 拆解目标、排步骤、定方案。**需要推理。**
    Planning,
    /// 跑一条命令、调一个工具、执行一个动作。
    Execution,
}

impl TaskKind {
    pub fn label(self) -> &'static str {
        match self {
            TaskKind::Conversation => "对话",
            TaskKind::Lookup => "查找",
            TaskKind::Generation => "生成",
            TaskKind::Edit => "修改",
            TaskKind::Analysis => "分析",
            TaskKind::Planning => "规划",
            TaskKind::Execution => "执行",
        }
    }

    /// 这个类型要不要思考。**这是思考模式的唯一依据。**
    pub fn needs_reasoning(self) -> bool {
        matches!(self, TaskKind::Analysis | TaskKind::Planning)
    }

    /// 确定性分类。拿不准返回 `None`——**`None` 是"该问下一步了"的信号**。
    ///
    /// 顺序即优先级：**推理信号最强，先判**。一个带"对比一下"的短句是分析任务，
    /// 不该因为"短"被当成对话。
    pub fn classify(text: &str) -> Option<Self> {
        if is_complex_reasoning(text) {
            return Some(TaskKind::Analysis);
        }
        if detect_planning(text) {
            return Some(TaskKind::Planning);
        }
        if detect_execution(text) {
            return Some(TaskKind::Execution);
        }
        if detect_edit(text) {
            return Some(TaskKind::Edit);
        }
        if detect_code(text) {
            // 写代码 vs 改代码：有"新写/实现"信号算生成
            const NEW: [&str; 5] = ["写一个", "写个", "实现一个", "新写", "从零"];
            if NEW.iter().any(|t| text.contains(t)) {
                return Some(TaskKind::Generation);
            }
            return Some(TaskKind::Edit);
        }
        if detect_generation(text) {
            return Some(TaskKind::Generation);
        }
        if detect_lookup(text) {
            return Some(TaskKind::Lookup);
        }
        // 短、无信号 → 对话。注意这条**必须放最后**。
        let n = text.trim().chars().count();
        if n > 0 && n < thresholds::SHORT_PROMPT_CHARS {
            return Some(TaskKind::Conversation);
        }
        None
    }

    /// 问本地决策模型时用的判据。
    pub fn criteria() -> [(&'static str, &'static str); 3] {
        [
            ("shallow", "不需要推理：查一下、转述一下、回一句话就能办"),
            ("drafting", "基本不用推理：起草或修改内容，照着要求写就行"),
            ("deep", "需要认真推理：分析、比较、诊断、拆解、排方案"),
        ]
    }

    /// 从决策模型的选项还原类型。
    pub fn from_choice(choice: &str) -> Option<Self> {
        match choice {
            "shallow" => Some(TaskKind::Lookup),
            "drafting" => Some(TaskKind::Generation),
            "deep" => Some(TaskKind::Analysis),
            _ => None,
        }
    }
}

/// 思考强度。**默认值就是 [`ReasoningEffort::Auto`]**——不由单个模型常量决定。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningEffort {
    /// 按任务类型决定。**默认。**
    #[default]
    Auto,
    /// 强制开。
    Always,
    /// 强制关。只该用在已知不需要推理的批量操作上。
    Never,
}

impl ReasoningEffort {
    /// 这个任务类型在这一档上要不要开思考。
    ///
    /// **只有 [`Tier::Deep`] 才开。** 免费档不值得为思考花等待时间和 token 预算，
    /// 而且轻量档的任务类型本来就不需要推理。强制 `Always` 可以覆盖这条——
    /// 但那是使用者的显式选择，不是默认行为。
    pub fn applies(self, kind: TaskKind, tier: Tier) -> bool {
        match self {
            ReasoningEffort::Always => true,
            ReasoningEffort::Never => false,
            ReasoningEffort::Auto => tier == Tier::Deep && kind.needs_reasoning(),
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            ReasoningEffort::Auto => "按任务类型",
            ReasoningEffort::Always => "强制开",
            ReasoningEffort::Never => "强制关",
        }
    }
}

/// DeepSeek 的思考模式字段。
///
/// **两个模式的实测代价见模块文档**：开 = 855 输出 token / 4.2s，关 = 340 / 2.1s。
///
/// 另外注意：**思考模式下 `temperature` 不生效**（官方文档：设了不报错，但也不生效）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Thinking {
    /// 不传该字段，由服务端默认。**对 DeepSeek 默认就是开。**
    ServerDefault,
    Enabled,
    Disabled,
}

impl Thinking {
    /// 从"要不要思考"得到线上字段。`None` 表示不传。
    pub fn from_decision(think: bool) -> Self {
        if think {
            Thinking::Enabled
        } else {
            Thinking::Disabled
        }
    }

    /// 需要往请求体里塞的字段。
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
    /// 该端点的**能力**上限：它支不支持思考模式。
    ///
    /// **注意这不是策略。** 策略由 [`ReasoningEffort`] 逐任务决定；
    /// 这里只回答"这个端点吃不吃 `thinking` 字段"。
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
        // Agnes 不吃 thinking 字段，传了是错的行为
        thinking: Thinking::ServerDefault,
        key_hint: "secrets/agnes.key 或 YUNXI_BOT_AGNES_KEY",
    };

    /// DeepSeek Flash：深度档。用于**需要多次调用、或需要推理**的任务。
    ///
    /// 模型名是 `deepseek-flash`（DeepSeek-V4.1-Flash）。
    ///
    /// `thinking` 字段在这里是 [`Thinking::Enabled`]——**表示这个端点支持思考**，
    /// 而不是"每个任务都开"。实际发什么由 [`ReasoningEffort::applies`] 逐任务算。
    pub const DEEPSEEK_FLASH: Self = Self {
        tier: Tier::Deep,
        provider: "deepseek",
        base_url: "https://api.deepseek.com/v1",
        model: "deepseek-flash",
        price: PriceTable::DEEPSEEK_FLASH,
        rpm: 60,
        thinking: Thinking::Enabled,
        key_hint: "secrets/deepseek.key 或 YUNXI_BOT_DEEPSEEK_KEY",
    };

    /// 这个端点吃不吃思考模式的字段。Agnes 不吃，就不该给它塞。
    pub fn accepts_thinking(&self) -> bool {
        !matches!(self.thinking, Thinking::ServerDefault)
    }
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
    /// 估算用：1 个 token 大约多少个字符（中文）。
    pub const CHARS_PER_TOKEN: usize = 2;
    /// 估算用：系统提示词 + 人格 + 工具描述的大致字符数。
    pub const OVERHEAD_CHARS: usize = 1200;
    /// 估算用：回答长度（字符）。
    pub const ANSWER_CHARS: usize = 400;
    /// 开思考时的输出放大倍数（实测 855/340 ≈ 2.5）。
    pub const THINKING_OUTPUT_MULTIPLIER: f64 = 2.5;
}

/// 任务画像。**只描述"要干多少活"，不描述"有多难"**——难度归 [`TaskKind`]。
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
    /// 预计需要的模型调用次数。**这是模型轴的主要依据。**
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

    /// 确定性判断模型档位。拿不准返回 `None`。
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
    /// 任务类型（决定思考模式的依据）。
    pub kind: TaskKind,
    /// 最终选中的模型。
    pub spec: ModelSpec,
    /// 最终要不要开思考。**由任务类型决定，不由模型常量决定。**
    pub thinking: bool,
    /// 理由链：模型档位为什么这么定 + 思考模式为什么这么定。
    pub reason: String,
    /// 预计调用次数。
    pub estimated_calls: usize,
    /// 是否由本地决策模型参与判断。
    pub used_decider: bool,
    /// 估算用量（以字数为单位）。**只用于预估，真实用量以回执为准。**
    pub estimate: TokenEstimate,
}

impl Routing {
    /// 本次实际要发出去的思考模式字段。
    ///
    /// 端点不支持思考（Agnes）时返回 [`Thinking::ServerDefault`]——**绝不硬塞**。
    pub fn thinking_field(&self) -> Thinking {
        if !self.spec.accepts_thinking() {
            return Thinking::ServerDefault;
        }
        Thinking::from_decision(self.thinking)
    }

    /// 按给定价格估算这次任务要花多少。
    ///
    /// **不加缓存**：第一次调用前缀必然是冷的。缓存收益是后续调用的事，
    /// 而后续调用命中与否取决于提示词布局，不该在这里假装。
    pub fn estimated_cost(&self, peak: bool) -> Cost {
        let usage = self.estimate.usage(self.thinking);
        usage.cost(&self.spec.price, peak)
    }

    /// 一句话说清"选了谁、开不开思考、为什么"。
    pub fn summary(&self) -> String {
        format!(
            "{}/{} · {} · 思考{} · 约 {} 次调用",
            self.spec.provider,
            self.spec.model,
            self.kind.label(),
            if self.thinking_field() == Thinking::ServerDefault {
                "不适用"
            } else if self.thinking {
                "开"
            } else {
                "关"
            },
            self.estimated_calls
        )
    }
}

/// 用量估算。**故意用字符数而不是假装精确的 token 数**。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TokenEstimate {
    pub prompt_chars: usize,
    pub answer_chars: usize,
    pub calls: usize,
    /// 估算时假设前缀是否已热。
    pub cached: bool,
}

impl TokenEstimate {
    /// 折算成 token 用量。
    pub fn usage(&self, thinking: bool) -> Usage {
        let per = thresholds::CHARS_PER_TOKEN;
        let prompt_tokens = (self.prompt_chars / per) as u64;
        let answer = (self.answer_chars / per) as u64;
        let out = if thinking {
            (answer as f64 * thresholds::THINKING_OUTPUT_MULTIPLIER) as u64
        } else {
            answer
        };
        let mut usage = Usage {
            prompt_tokens,
            completion_tokens: out,
            total_tokens: prompt_tokens + out,
            cache_hit_tokens: 0,
            cache_miss_tokens: prompt_tokens,
        };
        if self.cached {
            usage.cache_hit_tokens = prompt_tokens;
            usage.cache_miss_tokens = 0;
        }
        usage
    }
}

/// 从任务文本识别"需要认真推理"的信号。
///
/// **这一类信号优先级最高**：一个带"对比两个方案"的短句是分析任务，
/// 不该因为字少被当成寒暄。
pub fn is_complex_reasoning(text: &str) -> bool {
    const CUES: [&str; 15] = [
        "分析",
        "对比",
        "比较",
        "评估",
        "权衡",
        "诊断",
        "排查",
        "推理",
        "为什么",
        "为何",
        "判断",
        "可行性",
        "利弊",
        "方案",
        "预算多少",
    ];
    CUES.iter().any(|c| text.contains(c))
}

/// 识别"规划"类信号：拆解、排期、流程、步骤方案。
pub fn detect_planning(text: &str) -> bool {
    const CUES: [&str; 8] = [
        "拆解",
        "拆成",
        "排期",
        "规划",
        "路线图",
        "分期",
        "分几个阶段",
        "先后顺序",
    ];
    CUES.iter().any(|c| text.contains(c))
}

/// 识别"执行"类信号：跑命令、调工具。**与"生成"的区别是它要落地。**
pub fn detect_execution(text: &str) -> bool {
    const CUES: [&str; 10] = [
        "运行",
        "执行",
        "跑一下",
        "跑个",
        "启动",
        "部署",
        "安装",
        "重启",
        "调用工具",
        "发出去",
    ];
    CUES.iter().any(|c| text.contains(c))
}

/// 识别"修改"类信号：改已有的东西。
pub fn detect_edit(text: &str) -> bool {
    const CUES: [&str; 8] = [
        "修改",
        "改成",
        "改一下",
        "删掉",
        "删除",
        "替换",
        "更新一下",
        "重命名",
    ];
    CUES.iter().any(|c| text.contains(c))
}

/// 识别"生成"类信号：起草一段新内容。
///
/// **这条是我漏掉的。** 测试里"帮我写一封回信"被判成了对话——因为整条分类链
/// 里根本没有"写东西"这一类，于是所有起草任务都掉进最后的"短句→对话"兜底。
/// 起草和寒暄的区别不是长度，是**要不要产出内容**。
pub fn detect_generation(text: &str) -> bool {
    const CUES: [&str; 12] = [
        "写一封",
        "写个",
        "写一个",
        "写一段",
        "写一份",
        "起草",
        "拟定",
        "拟一份",
        "生成",
        "润色",
        "扩写",
        "缩写",
    ];
    CUES.iter().any(|c| text.contains(c))
}

/// 识别"查找"类信号：取一个事实、查一条记录。
///
/// 两条并行的判据：**祈使式**（"查一下 / 搜索 / 多少钱"）和
/// **疑问式**（含"哪/几/多少/有没有/吗"这类疑问成分）。只留前一条会把
/// "查一下快递到哪了"漏成对话。
pub fn detect_lookup(text: &str) -> bool {
    const CUES: [&str; 8] = [
        "查一下",
        "查查",
        "搜索",
        "找一下",
        "看看有没有",
        "多少钱",
        "是什么",
        "在哪",
    ];
    if CUES.iter().any(|c| text.contains(c)) {
        return true;
    }
    // 疑问式：整句是个问句，答案是一个事实。
    //
    // 早先只有祈使式（"查一下…"），于是"查一下快递到哪了"被判成对话——
    // 分类错了，模型选择和思考模式就跟着错。
    //
    // 不含光秃秃的"什么"：那会把"这是什么"这类开放提问也拉进来。
    const QUESTION_CUES: [&str; 6] = ["哪", "几", "多少", "有没有", "吗", "谁"];
    QUESTION_CUES.iter().any(|c| text.contains(c))
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

/// 问本地决策模型时用的判据（模型轴）。
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
    /// 有没有端点支持思考。没有的话就不该声称"思考模式按任务类型决定"。
    reasoning_capable: bool,
}

impl Default for ModelRouter {
    fn default() -> Self {
        Self::new(
            vec![ModelSpec::AGNES_FLASH, ModelSpec::DEEPSEEK_FLASH],
            // 拿不准时走免费的那个
            Tier::Standard,
        )
    }
}

impl ModelRouter {
    pub fn new(specs: Vec<ModelSpec>, default_tier: Tier) -> Self {
        let reasoning_capable = specs.iter().any(ModelSpec::accepts_thinking);
        Self {
            specs,
            default_tier,
            reasoning_capable,
        }
    }

    pub fn specs(&self) -> &[ModelSpec] {
        &self.specs
    }

    /// 配置里有没有支持思考的端点。
    pub fn reasoning_capable(&self) -> bool {
        self.reasoning_capable
    }

    /// 取某一档的模型。该档没配就回落到默认档——**路由不该因为缺配置而失败**。
    pub fn spec(&self, tier: Tier) -> Option<&ModelSpec> {
        self.specs
            .iter()
            .find(|s| s.tier == tier)
            .or_else(|| self.specs.iter().find(|s| s.tier == self.default_tier))
            .or_else(|| self.specs.first())
    }

    /// 路由。**每个任务只调一次。**
    ///
    /// `kind` 由调用方给出（通常是 [`TaskKind::classify`]，拿不准再走这一步的
    /// 本地决策层）。把 `kind` 作为参数而不是内部再判一次，是为了让
    /// **"任务类型是在任务执行层定的"** 这件事在类型上就成立。
    pub fn route(
        &self,
        task_text: &str,
        kind: TaskKind,
        profile: &TaskProfile,
        effort: ReasoningEffort,
        decider: Option<&dyn Decider>,
    ) -> Routing {
        let calls = profile.estimated_calls();

        // ---- 模型轴：三层，从便宜到贵 ----
        //
        // 这里先按**调用次数**定档，但后面还要被思考轴否决一次，见下。
        let (tier, mut reason, used_decider) = match profile.heuristic_tier() {
            Some(tier) => (
                tier,
                format!(
                    "档位由确定性信号定：预计 {calls} 次调用（{} 字 / {} 步 / 多条={} / 代码={}）",
                    profile.prompt_chars,
                    profile.step_count,
                    profile.explicit_multi,
                    profile.has_code
                ),
                false,
            ),
            None => match decider.and_then(|d| self.ask_call_count(d, task_text)) {
                Some(tier) => (tier, "档位由本地决策模型判定调用次数".to_string(), true),
                None => (
                    self.default_tier,
                    "档位信号不足且本地判断弃权，回落默认档（免费）".to_string(),
                    false,
                ),
            },
        };

        let mut spec = self.spec(tier).cloned().unwrap_or(ModelSpec::AGNES_FLASH);

        // ---- 两个轴在这里交汇：需要推理，但没有能推理的端点 ----
        //
        // **这是测试逼出来的一条。** 一个"分析这部分内容"的步骤只有一句话，
        // 按调用次数算就是轻量档 → Agnes。但 Agnes 的思考模式是"服务端默认"
        // （它不吃 thinking 字段），于是**这一步永远拿不到思考**——
        // 而它恰恰是唯一真正需要思考的一步。
        //
        // 判据顺序很关键：**调用次数决定"要不要换不排队的模型"，
        // 需要推理决定"能不能省这次钱"**。后者是硬约束，前者是优化。
        if kind.needs_reasoning() && !spec.accepts_thinking() && effort != ReasoningEffort::Never {
            if let Some(better) = self
                .specs
                .iter()
                .filter(|s| s.accepts_thinking())
                .min_by_key(|s| s.tier)
            {
                reason.push_str(&format!(
                    "；这一步需要推理，但 {} 不支持思考模式，改用 {}",
                    spec.provider, better.provider
                ));
                spec = better.clone();
            }
        }

        // ---- 思考轴：由任务类型决定 ----
        let mut think = effort.applies(kind, spec.tier);
        if think && !spec.accepts_thinking() {
            // 选了不支持思考的端点却要思考 → 说清楚并降级，不静默
            reason.push_str(&format!(
                "；{} 不支持思考模式，已降级为不开思考",
                spec.provider
            ));
            think = false;
        }
        reason.push_str(&match effort {
            ReasoningEffort::Auto => format!(
                "；思考模式按任务类型定：{} {}",
                kind.label(),
                if kind.needs_reasoning() {
                    "需要推理"
                } else {
                    "不需要推理"
                }
            ),
            other => format!("；思考模式由使用者指定：{}", other.label()),
        });

        let estimate = small_task_estimate(profile);
        Routing {
            tier: spec.tier,
            kind,
            spec,
            thinking: think,
            reason,
            estimated_calls: calls,
            used_decider,
            estimate,
        }
    }

    /// 问本地决策模型：要几次调用。任何异常都返回 `None` 由调用方兜底——
    /// **路由失败不能把任务卡住**。
    fn ask_call_count(&self, decider: &dyn Decider, task_text: &str) -> Option<Tier> {
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

    /// 问本地决策模型：这个任务需要多少推理。
    ///
    /// 只在 [`TaskKind::classify`] 拿不准、且**真的需要区分**时才值得问：
    /// 如果档位已经落到不支持思考的端点上，问也是白问。
    pub fn ask_kind(&self, decider: &dyn Decider, task_text: &str) -> Option<TaskKind> {
        use crate::decide::{DecisionRequest, Question};

        let question =
            Question::choice("task_kind", "这个任务需要多少推理？", &TaskKind::criteria());
        let req = DecisionRequest::new(serde_json::json!({ "task": task_text }), vec![question]);
        let result = decider.decide(&req).ok()?;
        result.choice("task_kind").and_then(TaskKind::from_choice)
    }
}

/// 按字数粗估用量。**故意粗糙**：精确数字要从回执里读，估算是用来做选择的。
pub fn small_task_estimate(profile: &TaskProfile) -> TokenEstimate {
    TokenEstimate {
        prompt_chars: thresholds::OVERHEAD_CHARS + profile.prompt_chars,
        answer_chars: thresholds::ANSWER_CHARS,
        calls: profile.estimated_calls(),
        cached: false,
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

/// 现在是不是 DeepSeek 的高峰时段。**常驻 Agent 可以把重任务排到空闲时段省一半。**
pub fn is_peak_now(now: chrono::DateTime<chrono::Local>) -> bool {
    use chrono::{Datelike, Timelike};
    is_peak_hour(now.weekday(), now.hour(), now.minute())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decide::StubDecider;

    fn profile(chars: usize, steps: usize) -> TaskProfile {
        TaskProfile {
            prompt_chars: chars,
            step_count: steps,
            ..Default::default()
        }
    }

    // ---------- 模型轴：调用次数 ----------

    #[test]
    fn multi_call_tasks_go_to_deepseek() {
        // 这是模型轴的核心判据：调用次数多就换不排队的那个
        let p = profile(100, 8);
        assert!(p.estimated_calls() > thresholds::MULTI_CALL);
        assert_eq!(p.heuristic_tier(), Some(Tier::Deep));
    }

    #[test]
    fn few_call_tasks_stay_on_the_free_one() {
        let p = profile(30, 1);
        assert!(p.estimated_calls() <= thresholds::MULTI_CALL);
        assert_eq!(p.heuristic_tier(), Some(Tier::Cheap));
    }

    #[test]
    fn chained_instructions_count_as_multiple_steps() {
        let p = profile_task("先备份数据库，然后把日志打包，再发我邮箱", 0);
        assert!(p.explicit_multi);
        assert_ne!(p.heuristic_tier(), Some(Tier::Cheap));
    }

    #[test]
    fn estimated_calls_scales_with_steps() {
        let one = profile(0, 1);
        let ten = profile(0, 10);
        assert!(ten.estimated_calls() > one.estimated_calls());
        assert_eq!(ten.estimated_calls(), 1 + 10 + 5);
    }

    #[test]
    fn heuristic_path_never_consults_the_decider() {
        // 每次都问模型判复杂度，等于为了省钱先花钱
        let router = ModelRouter::default();
        let stub = StubDecider::succeeding().with_choice("call_count", "many");
        let r = router.route(
            "买杯咖啡",
            TaskKind::Conversation,
            &profile(30, 1),
            ReasoningEffort::Auto,
            Some(&stub),
        );
        assert!(!r.used_decider);
        assert_eq!(stub.calls(), 0, "确定性信号能定就不该问模型");
    }

    #[test]
    fn ambiguous_falls_through_to_local_decider() {
        let router = ModelRouter::default();
        let stub = StubDecider::succeeding().with_choice("call_count", "many");
        let p = profile(500, 2);
        assert_eq!(p.heuristic_tier(), None, "这个画像应判不出来");
        let r = router.route(
            "整理这周的邮件",
            TaskKind::Lookup,
            &p,
            ReasoningEffort::Auto,
            Some(&stub),
        );
        assert_eq!(r.tier, Tier::Deep);
        assert!(r.used_decider);
        assert_eq!(stub.calls(), 1);
    }

    #[test]
    fn decider_failure_falls_back_to_the_free_tier() {
        let router = ModelRouter::default();
        let stub = StubDecider::failing("连接被拒绝");
        let r = router.route(
            "x",
            TaskKind::Lookup,
            &profile(500, 2),
            ReasoningEffort::Auto,
            Some(&stub),
        );
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
        let r = router.route(
            "x",
            TaskKind::Lookup,
            &profile(500, 2),
            ReasoningEffort::Auto,
            Some(&stub),
        );
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
        let a = PriceTable::AGNES_30_FLASH;
        assert!((a.input_idle / a.cache_hit_idle - 10.0).abs() < 1e-9);
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
        for p in [profile(30, 1), profile(0, 20), profile(500, 2)] {
            let r = router.route("x", TaskKind::Lookup, &p, ReasoningEffort::Auto, None);
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

    // ---------- 思考轴：任务类型 ----------

    #[test]
    fn complex_tasks_get_deepseek_with_thinking_on() {
        // 使用者的要求：复杂任务走 DeepSeek，**并且开着思考**——
        // 关了怎么处理复杂任务
        let router = ModelRouter::default();
        let task = "分析这两个方案的利弊，评估哪个更可行";
        let kind = TaskKind::classify(task).expect("应能确定性判成分析");
        assert_eq!(kind, TaskKind::Analysis);
        assert!(kind.needs_reasoning());

        // 多次调用 → 深度档；需要推理 → 思考开
        let r = router.route(
            task,
            kind,
            &profile_task(task, 6),
            ReasoningEffort::Auto,
            None,
        );
        assert_eq!(r.tier, Tier::Deep);
        assert_eq!(r.spec.provider, "deepseek");
        assert!(r.thinking, "复杂任务的思考模式必须是开的");
        assert_eq!(r.thinking_field(), Thinking::Enabled);
    }

    #[test]
    fn simple_tasks_go_to_agnes_without_thinking() {
        let router = ModelRouter::default();
        let task = "查一下明天几点开会";
        let kind = TaskKind::classify(task).expect("查找类应能判出来");
        assert_eq!(kind, TaskKind::Lookup);
        assert!(!kind.needs_reasoning());

        let r = router.route(
            task,
            kind,
            &profile_task(task, 1),
            ReasoningEffort::Auto,
            None,
        );
        assert_eq!(r.spec.provider, "agnes");
        assert!(!r.thinking, "查找类不该开思考");
        // Agnes 不吃 thinking 字段
        assert_eq!(r.thinking_field(), Thinking::ServerDefault);
    }

    #[test]
    fn deepseek_default_is_not_disabled_thinking() {
        // 我上一版把 DeepSeek 的默认思考模式写成了 Disabled，理由是省钱。
        // 那会顺手把复杂任务的能力砍掉——实测关掉后答案短 2.5 倍。
        assert_ne!(
            ModelSpec::DEEPSEEK_FLASH.thinking,
            Thinking::Disabled,
            "端点默认不能是关思考"
        );
        assert!(
            ModelSpec::DEEPSEEK_FLASH.accepts_thinking(),
            "DeepSeek 必须被认定支持思考"
        );
    }

    #[test]
    fn agnes_never_gets_a_thinking_field() {
        // Agnes 不吃 thinking 字段，就不能给它塞
        assert_eq!(ModelSpec::AGNES_FLASH.thinking, Thinking::ServerDefault);
        assert_eq!(ModelSpec::AGNES_FLASH.thinking.body_field(), None);
        assert!(!ModelSpec::AGNES_FLASH.accepts_thinking());
    }

    #[test]
    fn reasoning_only_applies_on_the_deep_tier() {
        // 免费档不值得为思考花等待时间和 token 预算
        assert!(!ReasoningEffort::Auto.applies(TaskKind::Analysis, Tier::Standard));
        assert!(!ReasoningEffort::Auto.applies(TaskKind::Analysis, Tier::Cheap));
        assert!(ReasoningEffort::Auto.applies(TaskKind::Analysis, Tier::Deep));
        // 深度档上的简单任务也不开
        assert!(!ReasoningEffort::Auto.applies(TaskKind::Lookup, Tier::Deep));
    }

    #[test]
    fn override_can_force_thinking_either_way() {
        assert!(ReasoningEffort::Always.applies(TaskKind::Lookup, Tier::Standard));
        assert!(!ReasoningEffort::Never.applies(TaskKind::Analysis, Tier::Deep));
    }

    #[test]
    fn unsupported_endpoint_downgrades_loudly() {
        // 选了不支持思考的端点却要思考：说清楚，不静默
        let router = ModelRouter::new(vec![ModelSpec::AGNES_FLASH], Tier::Standard);
        let r = router.route(
            "分析一下",
            TaskKind::Analysis,
            &profile(30, 1),
            ReasoningEffort::Always,
            None,
        );
        assert!(!r.thinking, "端点不支持就得降级");
        assert!(
            r.reason.contains("不支持思考"),
            "降级理由必须写进台账: {}",
            r.reason
        );
        assert!(!router.reasoning_capable());
    }

    #[test]
    fn routing_reason_covers_both_axes() {
        // 理由必须同时解释"为什么这个模型"和"为什么这个思考模式"
        let router = ModelRouter::default();
        let r = router.route(
            "分析这两个方案的利弊",
            TaskKind::Analysis,
            &profile(100, 6),
            ReasoningEffort::Auto,
            None,
        );
        assert!(r.reason.contains("档位"), "要解释档位: {}", r.reason);
        assert!(
            r.reason.contains("思考模式"),
            "要解释思考模式: {}",
            r.reason
        );
    }

    // ---------- 任务类型分类 ----------

    #[test]
    fn reasoning_cues_win_over_brevity() {
        // 短句但带推理信号 → 分析，不能因为字少被当成寒暄
        assert_eq!(
            TaskKind::classify("对比一下这两个"),
            Some(TaskKind::Analysis)
        );
        assert_eq!(TaskKind::classify("为什么慢"), Some(TaskKind::Analysis));
    }

    #[test]
    fn generation_is_detected_not_fallen_through_to_chat() {
        // 起草任务的差别不是长度，是"要不要产出内容"。
        // 漏掉这一类会让所有"写一封…"掉进"短句→对话"兜底。
        assert_eq!(
            TaskKind::classify("帮我写一封回信"),
            Some(TaskKind::Generation)
        );
        assert_eq!(
            TaskKind::classify("起草一段说明"),
            Some(TaskKind::Generation)
        );
        assert_eq!(TaskKind::classify("润色这段话"), Some(TaskKind::Generation));
        assert!(!TaskKind::Generation.needs_reasoning());
    }

    #[test]
    fn classification_covers_each_kind() {
        assert_eq!(
            TaskKind::classify("查一下快递到哪了"),
            Some(TaskKind::Lookup)
        );
        assert_eq!(
            TaskKind::classify("帮我写一封回信"),
            Some(TaskKind::Generation)
        );
        assert_eq!(
            TaskKind::classify("把配置里的端口改成 8080"),
            Some(TaskKind::Edit)
        );
        assert_eq!(
            TaskKind::classify("分析一下这段日志为什么报错"),
            Some(TaskKind::Analysis)
        );
        assert_eq!(
            TaskKind::classify("把这个项目拆成几个阶段"),
            Some(TaskKind::Planning)
        );
        assert_eq!(
            TaskKind::classify("运行一下测试"),
            Some(TaskKind::Execution)
        );
        assert_eq!(TaskKind::classify("早"), Some(TaskKind::Conversation));
    }

    #[test]
    fn long_ambiguous_text_abstains() {
        // 长且没有信号 → 交给本地决策模型，不要硬猜
        let long = "嗯".repeat(300);
        assert_eq!(TaskKind::classify(&long), None);
    }

    #[test]
    fn decider_can_answer_the_kind_question() {
        let router = ModelRouter::default();
        let stub = StubDecider::succeeding().with_choice("task_kind", "deep");
        assert_eq!(router.ask_kind(&stub, "随便什么"), Some(TaskKind::Analysis));
        let bad = StubDecider::succeeding().with_choice("task_kind", "nonsense");
        assert_eq!(router.ask_kind(&bad, "x"), None, "未知选项算弃权");
    }

    // ---------- 成本估算 ----------

    #[test]
    fn thinking_estimate_is_higher_than_not_thinking() {
        let e = TokenEstimate {
            prompt_chars: 1200,
            answer_chars: 400,
            calls: 1,
            cached: false,
        };
        assert!(e.usage(true).completion_tokens > e.usage(false).completion_tokens);
        // 实测 855/340 ≈ 2.5
        let ratio =
            e.usage(true).completion_tokens as f64 / e.usage(false).completion_tokens as f64;
        assert!((ratio - thresholds::THINKING_OUTPUT_MULTIPLIER).abs() < 1e-9);
    }

    #[test]
    fn estimate_never_claims_cache_savings() {
        // 第一次调用的前缀必然是冷的，估算里不能假装命中
        let e = TokenEstimate {
            prompt_chars: 1200,
            answer_chars: 400,
            calls: 1,
            cached: false,
        };
        let u = e.usage(true);
        assert_eq!(u.cache_hit_tokens, 0);
        assert_eq!(u.cache_miss_tokens, u.prompt_tokens);
    }

    #[test]
    fn deep_task_estimate_shows_idle_is_cheaper() {
        let router = ModelRouter::default();
        let r = router.route(
            "分析这两个方案的利弊，评估可行性",
            TaskKind::Analysis,
            &profile(200, 6),
            ReasoningEffort::Auto,
            None,
        );
        let peak = r.estimated_cost(true).total();
        let idle = r.estimated_cost(false).total();
        assert!(idle < peak, "空闲时段应更便宜: {idle} vs {peak}");
        assert!(peak > 0.0);
    }

    #[test]
    fn summary_mentions_provider_kind_and_thinking() {
        let router = ModelRouter::default();
        let r = router.route(
            "分析一下",
            TaskKind::Analysis,
            &profile(100, 6),
            ReasoningEffort::Auto,
            None,
        );
        let s = r.summary();
        assert!(s.contains("deepseek"), "{s}");
        assert!(s.contains("分析"), "{s}");
        assert!(s.contains("思考开"), "{s}");
    }
}
