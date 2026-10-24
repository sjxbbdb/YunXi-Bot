//! 每一步分别确定模型与思考模式：聊天和助理任务均走 DeepSeek；本地只保留决策与检索辅助模型。
//! 显式 provider 可限定模型；本地健康检查不会切换到远端。

use serde::{Deserialize, Serialize};

use super::client::ThinkerConfig;
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

    /// "任务还是闲聊"的判据。**和 [`Self::criteria`] 是两套不同的东西**：
    /// 那套问"需要多少推理"（只在确定是任务之后才有意义），
    /// 这套问"到底要不要办事"。
    ///
    /// 措辞刻意写得具体：**使用者不会说"请执行一个任务"**，
    /// 他会说"帮我把这个改一下"、"查一下"、"跑一遍"。
    /// 判据要认得出那些说法。
    pub fn input_kind_criteria() -> [(&'static str, &'static str); 2] {
        [
            (
                "task",
                "有事要办：要查什么、要改什么、要跑什么、要写什么、要分析什么，或者要我做点什么。哪怕只有一句话、听起来很随口，只要落点是\"办成一件事\"，就是这一类",
            ),
            (
                "chat",
                "只是闲聊：打招呼、聊感受、随口说一句、问我在不在。不需要我动手做任何事，回一句话就够了只是引用台词、描述别人的命令而明确不要求执行时，也是闲聊。",
            ),
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
    /// 每个端点独立的请求限流。
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
    /// 其他平台保留的轻量聊天端点；任务由 DeepSeek 处理。
    pub const LOCAL_QWEN: Self = Self {
        tier: Tier::Cheap,
        provider: "local",
        // **`/v1` 结尾**：`OpenAiThinker` 会自己拼 `/chat/completions`。
        base_url: "http://127.0.0.1:17872/v1",
        model: "Qwen3-4B-Instruct-2507",
        price: PriceTable::LOCAL,
        // **本地没有端点限流**，所以这个数只用来算"两次调用之间
        // 至少隔多久"。填 600 而不是 0：0 在限流器里通常意味着
        // "不允许任何调用"，而不是"不限"——**那是两种相反的语义，
        // 猜错的方向是本地模型一次也用不上。**
        rpm: 600,
        // 本地 sidecar 不吃 `thinking` 字段，和 Agnes 同理。
        thinking: Thinking::ServerDefault,
        key_hint: "本地模型不需要密钥（先起 sidecar/local_llm_server.py）",
    };

    /// Apple Silicon 已验收的量化聊天模型；思考开关仍由每次请求决定。
    pub const LOCAL_GEMMA: Self = Self {
        model: "gemma-4-26B-A4B-it-qat-4bit",
        thinking: Thinking::Enabled,
        ..Self::LOCAL_QWEN
    };

    /// Mac 纯聊天槽位，复用已下载并实测的低内存量化模型。
    pub const LOCAL_CHAT: Self = Self {
        model: "Qwen3.5-9B-4bit",
        thinking: Thinking::Enabled,
        ..Self::LOCAL_QWEN
    };

    #[cfg(target_os = "macos")]
    pub const LOCAL: Self = Self::LOCAL_CHAT;
    #[cfg(not(target_os = "macos"))]
    pub const LOCAL: Self = Self::LOCAL_QWEN;

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

    /// 这个端点吃不吃思考模式的字段。不支持时不发送。
    pub fn accepts_thinking(&self) -> bool {
        !matches!(self.thinking, Thinking::ServerDefault)
    }

    /// 端点参数只从 spec 获取，避免路由标签与实际请求目的地不一致。
    pub fn thinker_config(&self) -> ThinkerConfig {
        let mut cfg = match self.provider {
            // 本地侧车：不读密钥文件，而且只认回环地址
            "local" => ThinkerConfig::local(self.base_url, self.model),
            "deepseek" => ThinkerConfig::deepseek(),
            // 自定义 OpenAI 兼容云端：密钥放 secrets/openai.key，base_url/model 由适配器提供。
            "openai-compatible" => ThinkerConfig {
                key_source: super::client::KeySource::File("openai.key"),
                ..ThinkerConfig::deepseek()
            },
            // 未知 provider 不借用其他服务的凭证；远端会被回环守卫拒绝。
            _ => ThinkerConfig {
                key_source: super::client::KeySource::Loopback,
                ..ThinkerConfig::local(self.base_url, self.model)
            },
        };
        cfg.base_url = self.base_url.to_string();
        cfg.model = self.model.to_string();
        cfg.rpm = self.rpm;
        cfg.thinking = self.thinking;
        cfg
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
    /// 端点不支持思考时返回 [`Thinking::ServerDefault`]——**绝不硬塞**。
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
        Self::new(vec![ModelSpec::DEEPSEEK_FLASH], Tier::Deep)
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
        _task_text: &str,
        kind: TaskKind,
        profile: &TaskProfile,
        effort: ReasoningEffort,
        _decider: Option<&dyn Decider>,
    ) -> Routing {
        let calls = profile.estimated_calls();

        let (tier, reason) = if kind == TaskKind::Conversation {
            (
                Tier::Cheap,
                "聊天模式：直接回复，不开放任务工具".to_string(),
            )
        } else {
            (
                Tier::Deep,
                format!(
                    "档位=深度：助理任务走 DeepSeek（deepseek-flash），预计 {calls} 次调用（{} 字 / {} 步）",
                    profile.prompt_chars, profile.step_count
                ),
            )
        };
        let mut reason = reason;

        let mut spec = self
            .spec(tier)
            .cloned()
            .unwrap_or(ModelSpec::DEEPSEEK_FLASH);
        if self.specs.len() == 1 {
            reason = format!("限定模型：{}（{}）", spec.provider, spec.model);
        }

        // ---- 两个轴在这里交汇：需要推理，但没有能推理的端点 ----
        //
        // **这是测试逼出来的一条。** 一个"分析这部分内容"的步骤只有一句话，
        // 按调用次数算就是轻量档 → Agnes。但 Agnes 的思考模式是"服务端默认"
        // （它不吃 thinking 字段），于是**这一步永远拿不到思考**——
        // 而它恰恰是唯一真正需要思考的一步。
        //
        // 判据顺序很关键：**调用次数决定"要不要换不排队的模型"，
        // 需要推理决定"能不能省这次钱"**。后者是硬约束，前者是优化。
        if kind.needs_reasoning()
            && !spec.accepts_thinking()
            && effort != ReasoningEffort::Never
            && let Some(better) = self
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

        // ---- 思考轴：由任务类型决定 ----
        // 按任务所需档位决定思考，显式换成便宜的推理模型不能把思考关掉。
        let mut think = effort.applies(kind, tier);
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
            // **路由不再问决策模型了**——分类那一步才是它该管的。
            // 所以这个位恒为 false；留着是因为它对调用方仍有意义：
            // "这次档位是不是决策模型定的"。答案是"不是，是分类定的"。
            used_decider: false,
            estimate,
        }
    }

    /// 问本地决策模型：要几次调用。任何异常都返回 `None` 由调用方兜底——
    /// **路由失败不能把任务卡住**。
    /// 问决策模型"这个任务大概要几次调用"，用来折算档位。
    ///
    /// **路由现在不调它了**——档位由 `kind` 直接定（聊天与任务→DeepSeek）。留着是因为它仍然是"向本地决策模型问一个数值判断"
    /// 的完整范例，删掉的话连它那套"弃权怎么处理"的测试也一起没了。
    ///
    /// 哪天要做"按任务复杂度动态选档"，从它接着写。
    pub fn ask_call_count(&self, decider: &dyn Decider, task_text: &str) -> Option<Tier> {
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

    /// 问决策模型一个**二元**问题：这句话是要我办事，还是随口聊聊。
    ///
    /// ## 为什么单开一个问题，而不是复用 `ask_kind`
    ///
    /// `ask_kind` 问的是"**需要多少推理**"（shallow / drafting / deep），
    /// 它的选项里**根本没有"闲聊"**——所以它答不了这个问题。
    /// 硬套的话，"闲聊"只会被归成"不用推理"，于是走到查找档，
    /// **那还是当成了任务**。
    ///
    /// 分类建议本轮回复模式；聊天与任务都交给 DeepSeek，工具供给不因短答被撤销。
    /// 模型判断不能替代工具策略或人工审批。
    ///
    /// ## 只问一次
    ///
    /// 调用方在一次输入的开头问一次，拿到结果后**在整轮里复用**。
    /// 路由本身不再问（D118 之后它谁也不问了），所以不会重复。
    /// 返回分类与实际模型名供审计；`None` 表示弃权或不可用，由调用方兜底。
    pub fn classify_input(&self, decider: &dyn Decider, text: &str) -> Option<(bool, String)> {
        use crate::decide::{DecisionRequest, Question};

        let question = Question::choice(
            "input_kind",
            crate::prompts::decisions::INPUT,
            &TaskKind::input_kind_criteria(),
        );
        let req = DecisionRequest::new(serde_json::json!({ "input": text }), vec![question]);
        let result = decider.decide(&req).ok()?;
        match result.choice("input_kind") {
            Some("task") => Some((true, result.model)),
            Some("chat") => Some((false, result.model)),
            // 弃权或给了不认识的选项——**不猜**，交给调用方兜
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
    #[test]
    fn every_task_kind_uses_deepseek_even_with_one_short_step() {
        use super::*;
        for kind in [
            TaskKind::Lookup,
            TaskKind::Generation,
            TaskKind::Edit,
            TaskKind::Execution,
            TaskKind::Analysis,
            TaskKind::Planning,
        ] {
            for effort in [ReasoningEffort::Auto, ReasoningEffort::Never] {
                let route = ModelRouter::default().route(
                    "做一下",
                    kind,
                    &profile_task("做一下", 1),
                    effort,
                    None,
                );
                assert_eq!(route.spec.provider, "deepseek", "{kind:?} / {effort:?}");
            }
        }
    }
    #[test]
    fn active_models_and_default_client_do_not_require_agnes() {
        let router = super::ModelRouter::default();
        assert!(router.specs().iter().all(|s| s.provider != "agnes"));
        let cfg = super::ThinkerConfig::default();
        assert_eq!(cfg.model, super::ModelSpec::DEEPSEEK_FLASH.model);
        assert_eq!(cfg.base_url, super::ModelSpec::DEEPSEEK_FLASH.base_url);
        assert_eq!(
            cfg.key_source,
            crate::think::client::KeySource::File("deepseek.key")
        );
    }
    use super::*;
    use crate::decide::StubDecider;
    use crate::think::client::{KeySource, OpenAiThinker, is_loopback_url};
    use std::path::Path;

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
    fn a_real_task_goes_to_deepseek_without_asking_the_decider() {
        // **这是用户定的规则**：真实任务走 DeepSeek，陪伴聊天走 Agnes。
        //
        // 它取代了原来三条测试（`ambiguous_falls_through_to_local_decider`、
        // `decider_failure_falls_back_to_the_free_tier`、
        // `unknown_choice_is_treated_as_abstention`）——那三条测的是
        // "决策模型决定档位"这个机制，而**路由现在根本不问它了**。
        //
        // **注意这条测试盯的两件事**：
        // 1. 档位是 `Deep`（DeepSeek），不是 Standard（Agnes）
        // 2. **决策模型一次都没被调用**——分类那一步才是它该管的
        let router = ModelRouter::default();
        let stub = StubDecider::succeeding().with_choice("call_count", "many");
        let r = router.route(
            "整理这周的邮件",
            TaskKind::Lookup,
            &profile(500, 2),
            ReasoningEffort::Auto,
            Some(&stub),
        );
        assert_eq!(r.tier, Tier::Deep, "真实任务该走 DeepSeek");
        assert_eq!(r.spec.provider, "deepseek");
        assert!(
            !r.used_decider,
            "路由不该问决策模型——分类那一步已经说清是任务还是陪伴了"
        );
        assert_eq!(stub.calls(), 0, "一次都不该问");
    }

    #[test]
    fn the_decider_being_down_does_not_change_task_routing() {
        // 决策模型挂掉**不影响路由**——因为路由不问它。
        // 这条取代了 `decider_failure_falls_back_to_the_free_tier`：
        // 那个"回落"逻辑本身已经不存在了。
        let router = ModelRouter::default();
        let stub = StubDecider::failing("连接被拒绝");
        let r = router.route(
            "x",
            TaskKind::Lookup,
            &profile(500, 2),
            ReasoningEffort::Auto,
            Some(&stub),
        );
        assert_eq!(r.tier, Tier::Deep, "决策模型挂了，任务照样走 DeepSeek");
        assert_eq!(stub.calls(), 0);
    }

    #[test]
    fn conversation_uses_deepseek_without_auto_thinking() {
        let router = ModelRouter::default();
        let r = router.route(
            "聊聊天",
            TaskKind::Conversation,
            &profile(20, 1),
            ReasoningEffort::Auto,
            None,
        );
        assert_eq!(r.spec.provider, "deepseek");
        assert!(!r.thinking);
    }

    #[test]
    fn the_decider_can_say_an_input_is_a_task() {
        // **这是用户要的那条**：每次会话输入，问一次决策模型
        // "这是任务还是闲聊"。
        //
        // 在这之前 chat 那条路把 `TaskKind::Conversation` **写死**了——
        // 用户在对话里说"帮我把这个文件改一下"，它也被当闲聊，
        // 永远走免费的 Agnes，**而那件事根本办不成**。
        let router = ModelRouter::default();
        let stub = StubDecider::succeeding().with_choice("input_kind", "task");
        assert_eq!(
            router.classify_input(&stub, "帮我把这个文件改一下"),
            Some((true, "stub".into())),
            "要办事的话该判成任务"
        );
        assert_eq!(stub.calls(), 1, "**只问一次**");
    }

    #[test]
    fn the_decider_can_say_an_input_is_just_chat() {
        // 另一半：闲聊走免费的那边。**它占日常交互的绝大多数**，
        // 判错这一边的代价是天天在花钱。
        let router = ModelRouter::default();
        let stub = StubDecider::succeeding().with_choice("input_kind", "chat");
        assert_eq!(
            router.classify_input(&stub, "今天天气不错"),
            Some((false, "stub".into()))
        );
        assert_eq!(stub.calls(), 1);
    }

    #[test]
    fn an_unrecognised_choice_is_not_guessed() {
        // **不猜。** 决策模型给了个不认识的选项，返回 None 让调用方兜——
        // 猜错的方向要么是白花钱，要么是该办的事没办。
        let router = ModelRouter::default();
        let stub = StubDecider::succeeding().with_choice("input_kind", "nonsense");
        assert_eq!(router.classify_input(&stub, "随便说点什么"), None);
    }

    #[test]
    fn an_unavailable_decider_yields_no_opinion() {
        // 决策模型挂了 → None。调用方（chat）会**回落闲聊**：
        // 免费的一边，也最不容易误花钱。
        let router = ModelRouter::default();
        let stub = StubDecider::failing("连接被拒绝");
        assert_eq!(router.classify_input(&stub, "帮我把这个改一下"), None);
    }

    #[test]
    fn the_two_question_sets_are_different() {
        // **这两套判据不能混。** `criteria()` 问"需要多少推理"，
        // 它的选项里**根本没有"闲聊"**——硬套的话闲聊会被归成
        // "不用推理"，于是走到查找档，**那还是当成了任务**。
        let reasoning: Vec<&str> = TaskKind::criteria().iter().map(|(k, _)| *k).collect();
        let input_kind: Vec<&str> = TaskKind::input_kind_criteria()
            .iter()
            .map(|(k, _)| *k)
            .collect();
        assert!(reasoning.contains(&"deep"), "推理那套问的是深度");
        assert!(
            input_kind.contains(&"chat"),
            "这一套必须能表达'闲聊'——那是它存在的理由"
        );
        assert_ne!(reasoning, input_kind, "两套判据不该是同一套");
    }

    #[test]
    fn default_chain_contains_only_deepseek() {
        let router = ModelRouter::default();
        assert_eq!(router.spec(Tier::Standard).unwrap().provider, "deepseek");
        assert_eq!(router.specs().len(), 1);
        let deep = router.spec(Tier::Deep).unwrap();
        assert_eq!(deep.provider, "deepseek");
        assert_eq!(deep.model, "deepseek-flash", "模型名不能写错");
    }

    #[test]
    fn every_slot_actually_calls_the_endpoint_it_names() {
        // **这条就是那个 bug 的回归测试。**
        //
        // 症状：路由选的是本地槽位（终端打印 `Qwen3-4B-Instruct-2507·不思考`，
        // 台账里 `provider` 也是 `local`），可请求发到了 Agnes 的服务器——
        // 因为调用方按 provider 手写客户端配置，`"local"` 掉进了 `_ =>` 兜底。
        // **没有任何一处显示能看出这件事：名字、台账、标签全是对的。**
        //
        // 判据直接盯最终的 `ThinkerConfig`：客户端连的地址和它报的模型，
        // 必须就是这条 spec 写的那两条。中间不允许再存一份手抄的数据。
        //
        // 遍历 `router.specs()` 而不是手写三个常量：**手写的清单恰好测不出
        // "新加的槽位没接上"**，而漏接新槽位就是这个 bug 的形状
        // （`local` 正是当时新加的那一个）。
        for spec in ModelRouter::default().specs() {
            let cfg = spec.thinker_config();
            assert_eq!(
                cfg.base_url, spec.base_url,
                "provider `{}` 的客户端连的是 {}，但 spec 说的是 {}——\
                 路由按 spec 报名字、客户端按 config 发请求，这两份一旦不同，\
                 界面和台账都会指着另一个模型说是它答的",
                spec.provider, cfg.base_url, spec.base_url
            );
            assert_eq!(
                cfg.model, spec.model,
                "provider `{}` 的模型名漂了：客户端要用 {}，spec 说的是 {}",
                spec.provider, cfg.model, spec.model
            );
            // 限流和思考能力同理，也是 spec 的属性：它们要是各说各话，
            // "这个端点能不能开思考"就会有两个答案。
            assert_eq!(
                cfg.rpm, spec.rpm,
                "provider `{}` 的 RPM 没跟着 spec 走：{} vs {}",
                spec.provider, cfg.rpm, spec.rpm
            );
            assert_eq!(
                cfg.thinking, spec.thinking,
                "provider `{}` 的思考能力没跟着 spec 走：{:?} vs {:?}",
                spec.provider, cfg.thinking, spec.thinking
            );
        }
    }

    #[test]
    fn the_local_slot_is_keyless_and_only_ever_points_at_loopback() {
        // 本地槽位没有密钥文件可读，所以它必须落在"不需要密钥"那一类；
        // 而这一类**只在回环地址上成立**（守卫在 `client::from_home`）。
        // 两件事要一起断言：只测"能构造出来"的话，
        // 哪天有人把它指到远端，这条测试反而会替那个改动放行。
        let spec = ModelSpec::LOCAL;
        let cfg = spec.thinker_config();
        assert_eq!(
            cfg.key_source,
            KeySource::Loopback,
            "本地槽位不该去读密钥文件——它根本没有"
        );
        assert!(
            is_loopback_url(&cfg.base_url),
            "本地槽位的 base_url 必须是回环地址，否则无密钥客户端连构造都会被拒: {}",
            cfg.base_url
        );
        // 真从构造入口走一遍：home 里不需要存在任何文件。
        OpenAiThinker::from_home(Path::new("C:/没有这个目录"), cfg)
            .expect("本地槽位必须能无密钥构造出来");
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
        let router = ModelRouter::new(vec![ModelSpec::LOCAL_QWEN], Tier::Cheap);
        let s = router.spec(Tier::Deep).expect("应回落到默认档");
        assert_eq!(s.provider, "local");
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

    #[cfg(target_os = "macos")]
    #[test]
    fn cloud_chat_honors_explicit_thinking() {
        let router = ModelRouter::default();
        for (effort, expected) in [
            (ReasoningEffort::Auto, false),
            (ReasoningEffort::Always, true),
            (ReasoningEffort::Never, false),
        ] {
            let routing = router.route(
                "你好",
                TaskKind::Conversation,
                &profile_task("你好", 1),
                effort,
                None,
            );
            assert_eq!(routing.spec.model, "deepseek-flash");
            assert_eq!(routing.thinking, expected);
            assert_eq!(routing.thinking_field(), Thinking::from_decision(expected));
        }
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
    fn an_unsupported_endpoint_never_gets_a_thinking_field() {
        // Agnes 不吃 thinking 字段，就不能给它塞
        assert_eq!(ModelSpec::LOCAL_QWEN.thinking, Thinking::ServerDefault);
        assert_eq!(ModelSpec::LOCAL_QWEN.thinking.body_field(), None);
        assert!(!ModelSpec::LOCAL_QWEN.accepts_thinking());
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
        let router = ModelRouter::new(vec![ModelSpec::LOCAL_QWEN], Tier::Cheap);
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
        assert!(r.reason.contains("deepseek"), "要解释模型: {}", r.reason);
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
