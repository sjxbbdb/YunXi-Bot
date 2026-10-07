//! 接入真实模型的 [`TaskHandler`] 实现。
//!
//! ## 三套独立的会话，不是一个
//!
//! 拆解、执行步骤、生成决策选项**各用一套系统提示词**，因此各有一个
//! [`PromptLayout`]。看起来是重复，实际上是缓存正确性的要求：
//!
//! DeepSeek 的缓存前缀必须**完整匹配**。把三种意图混在一个会话里，
//! 系统提示词就得在三者之间来回变，于是每次切换都把前缀作废。
//! 分开之后，每个布局各自线性增长，前缀始终是"上一次的那个前缀 + 追加内容"。
//!
//! ## 按 provider 分开记历史，不是按任务
//!
//! 同一个任务里，简单步骤走 Agnes、复杂步骤走 DeepSeek（这是使用者的要求）。
//! 但**两家的前缀缓存互不相通**，所以每个 provider 各记一份历史：
//! 切回某一家时，它看到的前缀和上次一样。
//!
//! 这也正好满足"一个任务执行中途不换模型"——中途不换，但不同步骤可以选不同模型。

use std::collections::BTreeMap;
use std::path::PathBuf;

use yunxi_bot_core::costlog::CallRecord;
use yunxi_bot_core::decide::Decider;
use yunxi_bot_core::task::TaskError;
use yunxi_bot_core::task::engine::{
    OPTIONS_MAX_TOKENS, ObserveRequest, PLAN_MAX_TOKENS, PlanRequest, STEP_MAX_TOKENS, StepRequest,
    TaskHandler,
};
use yunxi_bot_core::think::prompt::{PromptLayout, build_persona};
use yunxi_bot_core::think::{
    ModelSpec, OpenAiThinker, ReasoningEffort, Routing, ThinkError, ThinkRequest, Thinker,
    ThinkerConfig, Thinking,
};
use yunxi_bot_core::tool::{Approver, RefusingApprover, ToolPolicy, ToolRegistry, ToolRunner};

/// 拆解用的系统提示词。**必须是纯函数**——掺进时间或任务 id 就会毁掉前缀缓存。
pub const PLANNER_SYSTEM: &str = "\
你在把一个目标拆成可执行的步骤。

输出要求（严格遵守）：
1. 只输出一个 JSON 对象，不要任何解释文字、不要 markdown 代码块之外的散文。
2. 形如：{\"steps\":[{\"id\":\"s1\",\"instruction\":\"...\",\"depends_on\":[],\"kind\":\"lookup\"}]}
3. id 用 s1、s2、s3 这样的短标识，**唯一**。
4. depends_on 是前置步骤的 id 数组；能并行的步骤不要互相依赖。
5. kind 从这几个里选：conversation / lookup / generation / edit / analysis / planning / execution。
   - 需要分析、比较、诊断、推理的步骤填 analysis；需要拆解排方案的填 planning。
   - 查一个事实填 lookup；起草内容填 generation；改已有内容填 edit；跑命令填 execution。
   - **kind 会影响这一步用哪个模型、要不要开思考，所以要认真填。**
6. 步骤要具体到「能单独完成并产出结果」，不要写「继续处理」这类空话。
7. 如果这一步本身是\"需要人来拍板的选择\"，把 instruction 写成
   `decide: 要判断的问题`，不要写别的。";

/// 执行单步用的系统提示词。
pub const STEP_SYSTEM: &str = "\
你在执行一个更大目标里的一步。

要求：
1. **第一行必须以 `OK:` 或 `BLOCKED:` 开头**，后面可以直接跟内容。
   - 做成了 → `OK:` 然后给出这一步的产出。
     **「我查了，结论是没有」也算做成了。** 比如「项目里没有测试」、
     「这个文件不存在」、「没找到相关配置」——**那是一个结论，
     不是一次失败。** 这一步要的就是一个答案，而你给出了答案。
   - 真做不了 → `BLOCKED:` 然后一句话说清**再给点什么才能继续**
     （缺信息、缺权限、缺工具、需要人拍板）。
     **只有「再给我点东西我就能做」才用这个。**

     **这两者分错的代价很大**：`BLOCKED:` 会让**依赖这一步的后续步骤
     全部跳过**、整个任务变成「卡住」。而「没有测试」这种结论
     本来不该打断整条链——它恰恰是后面步骤需要知道的事。
   - **需要授权的事，直接去做，让审批层去判；不要在文字里自己拒绝。**
     你没能力判断「这次运行有没有开审批」——审批层知道
     （它可能已经被 `--yes` 授权、可能有交互通道，而且它会把每一次
     批准与否**记进台账**）。你在文字里说「这需要使用者点头」，
     等于**绕过了那整套机制**：本该被批准的事变成整条链停住，
     而台账上什么也没有。
     真机上抓到过：建一个测试文件、`--yes` 已经授权了，模型却回
     `BLOCKED:` 说不能自作主张创建新文件，于是任务卡住、文件没建。
     **让它去调工具，拒绝由审批层拒绝。**
2. 只做这一步，不要顺手做别的步骤，也不要复述整个计划。
3. 直接给出这一步的产出本身。要写文件就给内容，要答问题就给答案。
4. 简洁。需要几行就几行，不要为了显得完整而灌水。
5. **做不了就说做不了，不要编一个看起来像结果的东西。**
   编出来的结果会让整条链路在错误的前提上继续往下跑。";

/// 通用对话用的系统提示词。
///
/// ## 它和另外三个的区别
///
/// `PLANNER`/`STEP`/`OPTIONS` 是**任务引擎的内部角色**——它们各自只干一件事，
/// 而且被引擎约束着（输出格式、只做这一步）。`CHAT` 是**面对使用者的那一个**：
/// 没有外部编排，它自己决定要不要用工具、要不要拆步骤。
///
/// 所以这里的约束方向和那三个相反：那边是"收窄"（只做这一步），
/// 这边是"放开但要诚实"。
pub const CHAT_SYSTEM: &str = "\
你是云熙，一个通用 agent。使用者直接和你说话，你自己决定怎么做。

## 怎么用工具

- **能查就别猜。** 涉及具体事实（文件内容、当前时间、搜索结果）时用工具，
  不要凭记忆编。你不知道这个项目里有什么文件，去 list_dir 看。
- **能一步做完就别拆。** 简单的事直接做，不要为了显得有条理而先列计划。
- **一次可以要多个工具。** 几个互不依赖的读取就一起要，省得来回等。
- 做事之前先说一句你要做什么（一句话）。使用者需要知道你在干什么。

## 怎么说话

- **直接给结果，不要复述过程。** 使用者看到工具调用，不需要你再说一遍。
- 简洁。需要几行就几行，不要为了显得完整而灌水。
- 中文回答（除非使用者用别的语言）。

## 三条硬要求

1. **做不了就说做不了。** 缺信息、缺权限、工具失败了——如实说，
   并说清缺什么。**不要编一个看起来像结果的东西。**
   编出来的结果会让整条链路在错误的前提上继续往下跑。
2. **改了东西要说改了哪。** 用了 write_file / edit_file 之后，
   一句话说清动了哪个文件、动了什么。
3. **不确定就说不确定。** 猜的时候要标明是猜的。";

/// 对话会话的固定 key。见 `converse` 里的理由。
pub const CHAT_SESSION_KEY: &str = "chat";

/// 一轮对话的输出上限。
///
/// 比"执行一步"大：对话要能写出完整解释，而单步通常只是一段产出。
pub const CHAT_MAX_TOKENS: u32 = 4096;

/// 生成决策选项用的系统提示词。
pub const OPTIONS_SYSTEM: &str = "\
你在为一个需要拍板的步骤列出可选做法。

输出要求（严格遵守）：
1. 只输出一个 JSON 对象：{\"question\":\"要决定什么\",\"options\":[\"做法一\",\"做法二\"]}
2. options 给 2 到 6 个**互相区别**的做法，每个不超过 40 字。
3. 每个选项要说清\"做什么\"，不要写\"方案A\"这种没有信息的占位符。
4. 不要替使用者选，只列出来。选择由另一个决策模型做。";

/// 会话标识：意图 + provider。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct SessionKey {
    intent: Intent,
    provider: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Intent {
    /// 面对使用者的通用对话。**没有外部编排——它自己决定怎么做。**
    Chat,
    Plan,
    Step,
    Options,
}

impl Intent {
    fn system(self) -> String {
        let base = match self {
            Intent::Chat => CHAT_SYSTEM,
            Intent::Plan => PLANNER_SYSTEM,
            Intent::Step => STEP_SYSTEM,
            Intent::Options => OPTIONS_SYSTEM,
        };
        base.to_string()
    }
}

/// 真实模型 handler。
pub struct ChatHandler {
    home: PathBuf,
    /// 已构造的客户端。按 provider 缓存，避免每步都重新读密钥文件。
    ///
    /// 存 `Arc` 是为了**取出来时不再借住 `self`**：工具循环同时要可变借
    /// `sessions` 和不可变借 `registry`，再挂着一个 `&self` 的客户端借用
    /// 就会打架。克隆一个 Arc 让借用关系干净。
    thinkers: BTreeMap<String, std::sync::Arc<OpenAiThinker>>,
    /// 会话布局。key 是（意图，provider）。
    sessions: BTreeMap<SessionKey, PromptLayout>,
    /// 人格 + 硬规则拼成的稳定块。**只在这里存一份。**
    persona: String,
    /// 对话会话的稳定前缀。**提前算好**——载入会话时要拿它和存档里的
    /// 指纹比，而"先建会话再比"会覆盖掉存档里的指纹。
    /// 常驻记忆段（构造时定下）。**单独存着**，因为它要拼进稳定前缀，
    /// 而稳定前缀只能有一份来源——两份必然漂移（见 `stable_prefix`）。
    memory_block: String,
    /// 使用者自己写的画像（构造时读一次）。同样进稳定前缀。
    profile_block: String,
    /// 启动时读到的项目规则。**留着是为了能报出来源**——
    /// 模型说"按项目约定应该……"时，使用者得能查到那指的是哪一条。
    project_rules: yunxi_bot_core::rules::RuleSet,
    /// 可用的工具。空表示这条链路没有工具（行为与加工具之前完全一致）。
    tools: ToolRegistry,
    /// 谁能回答"批不批准"。默认谁都不问、一律拒绝。
    approver: std::sync::Arc<std::sync::Mutex<dyn Approver>>,
    /// 审批门禁第二层用的本地决策模型。
    decider: Option<std::sync::Arc<dyn Decider>>,
    /// 工具调用的去处。`None` 表示这条链路不记录工具调用。
    ///
    /// **不设就是不记录**，而 CLI 一定会设——
    /// 机器能改使用者的文件却不留记录，是 ADR D11 明令禁止的。
    sink: Option<std::sync::Arc<dyn yunxi_bot_core::tool::ToolCallSink>>,
    /// 正文增量的去处。`None` 表示不流式。
    ///
    /// **只有面对使用者的那条链路才设它**——常驻循环和测试不需要
    /// "边生成边显示"，也不该为它付代价。
    on_delta: Option<yunxi_bot_core::tool::runner::DeltaSink>,
    /// 审批策略。
    policy: ToolPolicy,
    /// 路由。**对话要自己判断"这活复杂不复杂"**——
    /// 任务引擎那条路由是引擎给的，对话这条得自己算。
    router: yunxi_bot_core::think::ModelRouter,
}

/// 拼"常驻记忆段"——从台账里读记忆，选出关于"使用者是谁"的那些。
///
/// ## 台账打不开就不拼，而不是让对话起不来
///
/// 记忆是**增强**，不是对话能不能进行的前提。台账坏了的话，
/// 一个没有记忆的助理远好过一个起不来的助理。
///
/// 但要**说一声**——"这次没有记忆"和使用者本来就没记过，
/// 是两件不同的事，混在一起会让人以为记忆丢了。
fn resident_memory_block(home: &std::path::Path) -> String {
    use yunxi_bot_core::memory::Memory;
    let path = home.join("ledger.jsonl");
    let Ok(ledger) = yunxi_bot_core::ledger::Ledger::open(&path) else {
        if path.exists() {
            eprintln!("提示：台账打不开，这次对话看不到记忆。");
        }
        return String::new();
    };
    let mem = Memory::from_events(ledger.events());
    let entries = mem.resident(yunxi_bot_core::think::prompt::RESIDENT_PER_KIND);
    let version = mem.resident_version(yunxi_bot_core::think::prompt::RESIDENT_PER_KIND);
    yunxi_bot_core::think::prompt::build_memory_block(&entries, &version)
}

/// 一回合的诊断数据。
///
/// **打包成一个结构而不是八个位置参数**：八个参数里把 `lexical_n` 和
/// `semantic_n` 传反了，编译器一句话都不会说，而输出看起来一切正常。
/// 具名字段让这种错写不出来。
struct TurnDiag<'a> {
    input_chars: usize,
    need: yunxi_bot_core::recall_gate::MemoryNeed,
    /// 门控的结论是**决策模型**给的吗（还是关键词直接判的）。
    gate_by_model: bool,
    lexical_n: usize,
    semantic_n: usize,
    both_n: usize,
    echoed: usize,
    selected: Option<&'a [String]>,
}

impl ChatHandler {
    /// `persona_text` 与 `rules` 来自配置；它们会被拼进稳定前缀。
    ///
    /// ## 人格文件优先
    ///
    /// 传进来的 `persona_name` / `persona_text` 现在是**内置默认**，
    /// 只在该文件不存在时用。真正生效的是 `<数据目录>/persona.md`。
    ///
    /// **为什么在这里读而不是在调用方读**：有 5 处构造点，每一处都
    /// 自己去读一遍的话，迟早有一处忘了读——而那一处的表现是
    /// **"改了人格但那条链路没变"**，最难查。放在唯一入口里，
    /// "所有链路用同一个人格"就成了结构上的保证。
    pub fn new(home: PathBuf, persona_name: &str, persona_text: &str, rules: Vec<String>) -> Self {
        let loaded = yunxi_bot_core::persona::load(&home, persona_name, persona_text);
        let persona_name = loaded.name.as_str();
        let persona_text = loaded.text.as_str();
        // **项目规则在这里读一次，就定下来。**
        //
        // 它进稳定前缀，而前缀每轮变的话缓存全废——所以不能
        // "每次拼前缀时现读"。中途 cwd 变了导致的差异由
        // `PrefixMismatch` 处理（保留历史、换新前缀、如实报告）。
        //
        // 找规则用 cwd：使用者关心的是"我现在在哪个项目里工作"，
        // 而不是数据目录在哪。
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let project_rules = yunxi_bot_core::rules::RuleSet::discover(&cwd, &home);
        // **常驻记忆在构造时读一次，就定下来。**
        //
        // 和项目规则同理：它进稳定前缀，而前缀每轮变的话缓存全废。
        // 所以不能"每轮现读记忆"——那样每记一条新记忆，
        // 整个会话的缓存都要从头重来一次。
        //
        // 代价是：**同一个进程里新记的记忆，这一轮会话看不见**。
        // 这是可接受的（记记忆本来就该是"下一次对话生效"），
        // 而且比"每轮前缀都变、缓存永远命中不了"好得多。
        let memory_block = resident_memory_block(&home);
        // **使用者自己写的那份**。和记忆不是一回事：画像是他主动写的
        // 自我描述（权威），记忆是助理攒的观察（可能有错）。
        //
        // 它在构造时读一次就定下来——和人格、规则、常驻记忆同理：
        // 进稳定前缀的东西每轮现读的话，前缀一直在变、缓存全废。
        let profile_block = yunxi_bot_core::profile::load_and_render(&home);
        Self {
            home,
            thinkers: BTreeMap::new(),
            sessions: BTreeMap::new(),
            memory_block,
            profile_block,
            project_rules,
            persona: build_persona(persona_name, persona_text, &rules),
            tools: ToolRegistry::new(),
            approver: std::sync::Arc::new(std::sync::Mutex::new(RefusingApprover)),
            decider: None,
            // 默认不记录。CLI 一定会用 `with_sink` 覆盖它——
            // 默认值是给"这条链路不该记录"的场景留的显式出口。
            sink: None,
            on_delta: None,
            policy: ToolPolicy::default(),
            router: yunxi_bot_core::think::ModelRouter::default(),
        }
    }

    /// 挂上工具。
    pub fn with_tools(mut self, tools: ToolRegistry) -> Self {
        self.tools = tools;
        self
    }

    /// 挂上审批者。**不挂就是 [`RefusingApprover`]**——没人应答等于不执行。
    pub fn with_approver(
        mut self,
        approver: std::sync::Arc<std::sync::Mutex<dyn Approver>>,
    ) -> Self {
        self.approver = approver;
        self
    }

    /// 挂上本地决策模型（审批门禁第二层）。
    pub fn with_decider(mut self, decider: std::sync::Arc<dyn Decider>) -> Self {
        self.decider = Some(decider);
        self
    }

    /// 设定正文增量的去处。**只有交互式那条链路才该设它。**
    pub fn set_on_delta(&mut self, f: yunxi_bot_core::tool::runner::DeltaSink) {
        self.on_delta = Some(f);
    }

    /// 设定工具调用的去处。
    ///
    /// **不设就是不记录**，而 CLI 一定会设——
    /// 机器能改使用者的文件却不留记录，是 ADR D11 明令禁止的。
    pub fn with_sink(
        mut self,
        sink: std::sync::Arc<dyn yunxi_bot_core::tool::ToolCallSink>,
    ) -> Self {
        self.sink = Some(sink);
        self
    }

    pub fn with_policy(mut self, policy: ToolPolicy) -> Self {
        self.policy = policy;
        self
    }

    /// 三套系统提示词 + 人格，也就是三个会话各自的**稳定前缀**。
    ///
    /// 打出来是为了让"前缀是否逐字节稳定"这件事可核对——缓存命中与否
    /// 完全取决于它，而这是使用者唯一能亲眼验证的地方。
    pub fn stable_prefixes(&self) -> Vec<(&'static str, String)> {
        [
            ("拆解", Intent::Plan),
            ("执行", Intent::Step),
            ("选项", Intent::Options),
        ]
        .into_iter()
        .map(|(name, i)| (name, format!("{}\n\n{}", self.persona, i.system())))
        .collect()
    }

    /// 取（或造）某个 provider 的客户端。
    ///
    /// 造不出来就是配置问题（缺密钥），**直接报错而不是换一个模型偷偷跑**——
    /// 悄悄降级会让"为什么这次答案变差了"永远查不清。
    fn thinker(&mut self, spec: &ModelSpec) -> Result<std::sync::Arc<OpenAiThinker>, TaskError> {
        if !self.thinkers.contains_key(spec.provider) {
            let cfg = match spec.provider {
                "deepseek" => ThinkerConfig::deepseek(),
                _ => ThinkerConfig::agnes(),
            };
            let t = OpenAiThinker::from_home(&self.home, cfg).map_err(|e| {
                TaskError::Core(yunxi_bot_core::CoreError::Ledger(format!(
                    "无法初始化 {} 的客户端（{}）: {e}",
                    spec.provider, spec.key_hint
                )))
            })?;
            self.thinkers
                .insert(spec.provider.to_string(), std::sync::Arc::new(t));
        }
        self.thinkers
            .get(spec.provider)
            .cloned()
            .ok_or_else(|| TaskError::Core(yunxi_bot_core::CoreError::Ledger("客户端丢失".into())))
    }

    /// 一轮通用对话。**这是通用 agent 的核心入口。**
    ///
    /// ## 和 `plan`/`step` 的区别
    ///
    /// 那两条是**任务引擎的内部步骤**：提示词、预算、收尾时机都由引擎决定。
    /// 这一条是**面对使用者的**：它自己决定要不要用工具、什么时候收尾。
    ///
    /// 但底下走的是**同一个 `converse`**——工具循环、审批门禁、台账留痕、
    /// 缓存前缀全都不变。**另起一条轻量路径的话，那条路迟早会绕开审批。**
    pub fn chat_turn(
        &mut self,
        routing: &Routing,
        input: &str,
        records: &mut Vec<CallRecord>,
    ) -> Result<String, TaskError> {
        let volatile = self.with_recalled_memory(input);
        self.converse(routing, Intent::Chat, volatile, CHAT_MAX_TOKENS, records)
    }

    /// 一回合的**脱敏**召回诊断。
    ///
    /// ## 为什么要有它
    ///
    /// 研究文档 §4.7 点出了两个现存问题："**抽取命中率不清楚、没有诊断**"
    /// 和"**召回 0 命中不知原因**"。而这两个问题在真机上我都撞到过：
    ///
    /// - D76 那次"任务跑完了但文件没改"——**它那一步判了什么、召回了什么，
    ///   一个字都没留下**，所以到现在还没查清
    /// - D82 的门控判了什么，也只能从行为反推
    ///
    /// **先有观测，才谈得上查。**
    ///
    /// ## 默认不打印，而且不打印正文
    ///
    /// 用环境变量 `YUNXI_BOT_MEMORY_DEBUG=1` 开关。两个理由：
    ///
    /// 1. **每轮都打会淹没对话**——它是查问题用的，不是日常输出
    /// 2. **它含私人内容**。默认打的话会进终端回滚、进日志、进截图。
    ///    所以这里**只打 id 和类别，不打正文**——id 足够回答
    ///    "是哪一条"，而正文要看该去 `yunxi-bot memory`
    ///
    /// 这个区分不是洁癖：**日志里出现过的私人信息就收不回来了**，
    /// 而排查绝大多数时候只需要知道"是哪一条"。
    fn diag(d: TurnDiag<'_>) {
        if std::env::var("YUNXI_BOT_MEMORY_DEBUG").is_err() {
            return;
        }
        // 输入本身也可能含私人内容，所以只报长度和门控结果
        eprintln!("[记忆] 这轮问题 {} 字", d.input_chars);
        eprintln!(
            "[记忆] memory_decision: {}   （由{}判定）",
            d.need.label(),
            if d.gate_by_model {
                "决策模型"
            } else {
                "关键词"
            }
        );
        eprintln!(
            "[记忆] lexical_candidates {} / dense_candidates {} / 两路都中 {}",
            d.lexical_n, d.semantic_n, d.both_n
        );
        eprintln!("[记忆] 因已在上下文里而不重复注入 {} 条", d.echoed);
        match d.selected {
            Some(ids) if !ids.is_empty() => {
                eprintln!("[记忆] selected_ids: {}", ids.join(", "));
                eprintln!("[记忆] （只看正文：yunxi-bot memory --search <词>）");
            }
            _ => eprintln!("[记忆] selected_ids: 无（fallback: no_match）"),
        }
    }

    /// 拿不准时问决策模型："这句话要不要去查关于他的记忆。"
    ///
    /// 返回 `None` 表示**问不出来**——没有决策器、调用失败、或者模型
    /// 给了个听不懂的选项。三种情况都由调用方退回关键词的判断。
    ///
    /// **认不出就说认不出**，不硬猜：硬猜一个比不猜更坏，
    /// 因为它看起来像有依据，事后没法归因。
    fn gate_with_model(&self, input: &str) -> Option<yunxi_bot_core::recall_gate::MemoryNeed> {
        use yunxi_bot_core::decide::DecisionRequest;
        let d = self.decider.as_deref()?;
        // **只喂判断所需的证据。** 把整段上下文倒进去既浪费调用，
        // 也让"它凭什么这么判"变得不可复核。
        let req = DecisionRequest::new(
            serde_json::json!({ "使用者这一句": input }),
            yunxi_bot_core::recall_gate::gate_questions(),
        );
        let res = d.decide(&req).ok()?;
        let answer = res.answer(yunxi_bot_core::recall_gate::GATE_QUESTION)?;
        yunxi_bot_core::recall_gate::need_from_choice(answer.choice.as_deref())
    }

    /// 给这一句话配上相关的往事，拼成易变尾。
    ///
    /// ## 为什么是易变尾而不是前缀
    ///
    /// 动态召回的产出**每轮都不一样**（跟着这句话走）。放进前缀的话
    /// 前缀每轮都变，缓存永远命中不了——那是这个项目反复踩的坑。
    /// 放尾部则不享受缓存，但也**不破坏**任何已有的缓存。
    ///
    /// 而记忆的**常驻层**（事实 / 偏好）走的是另一条路：进稳定前缀、
    /// 带版本号。两层分工的依据是**变化频率**：常驻层几个月才变一次，
    /// 动态层每句话都在变。
    ///
    /// ## 每轮重新读台账，而不是用构造时那份
    ///
    /// 常驻段必须在构造时定下来（它进前缀）。而动态段**应该**看到最新的
    /// 记忆——同一轮对话里刚 `remember` 的东西，下一句就该能想起来。
    fn with_recalled_memory(&mut self, input: &str) -> String {
        use yunxi_bot_core::memory::Memory;
        use yunxi_bot_core::think::prompt::{
            RECALL_BUDGET_CHARS, RECALL_LIMIT, build_recall_block,
        };

        // **已经不重复注入了。**
        //
        // `record_reply` 会把 volatile 取走塞进历史——所以前几轮注入的
        // 记忆段现在冻在历史里、并且被缓存。这一轮若又召回同一条，
        // 就是同一件事在上下文里出现两次：白占 token，还可能让模型
        // 以为"这事被强调过"。
        //
        // 参考实现（Miyu 的 `retain_unseen_association`）管这个叫
        // "已经在可见历史里的内容不再重复注入"。
        //
        // **我一开始写的是疲劳计数**（同一条召回超过 5 次就按对数降权）——
        // 那是在给自己造出来的问题打补丁：真正要判的是"它现在在不在
        // 上下文里"，而计数只是它的粗糙代理，还会在新会话里把该召回的
        // 那条按下去。**查实际存在与否，比统计次数准。**
        let key = SessionKey {
            intent: Intent::Chat,
            provider: CHAT_SESSION_KEY.to_string(),
        };

        // **先过门控：这一轮到底有没有记忆需求。**
        //
        // 在这之前每轮都无条件召回。后果两头：白花几毫秒；更坏的是
        // 翻出来的东西看着相关其实是噪声，会把回答带偏。
        // 参考架构 §4.2 的原话是"避免每轮盲目注入"。
        //
        // 门控是**确定性**的（关键词表），不调模型——它要是也得调模型，
        // 省下来的调用还不够付它的；而且调模型的门控没法稳定测试，
        // "召回结果为什么变了"就说不清了。
        let need = yunxi_bot_core::recall_gate::gate(input);
        // **拿不准才问决策模型。**
        //
        // 关键词先跑：常见问法（"我住在哪"）一毫秒就能判准，
        // 而且**确定**——同一句话永远同一个结果。
        //
        // 只有它判成 `Mixed`（拿不准）时才问模型。真机上的例子：
        // 「量子色动力学的重整化群方程」没有疑问句式、也没有个人指代，
        // 关键词只能保守地判成"可能要用记忆"，于是白召回一趟。
        // **"这是不是通用知识"靠字面判不出来。**
        //
        // 模型失败/超时/给出听不懂的选项时**退回关键词的判断**——
        // 门控是优化，不是对话能不能进行的前提。
        let from_keyword = need;
        let need = if need == yunxi_bot_core::recall_gate::MemoryNeed::Mixed {
            self.gate_with_model(input).unwrap_or(need)
        } else {
            need
        };
        let gate_by_model = need != from_keyword;
        if !need.needs_recall() {
            // **不召回也要留痕。** "它怎么没去查记忆"和"查了没找到"
            // 是两件事，只记后者的话前者永远说不清。
            Self::diag(TurnDiag {
                input_chars: input.chars().count(),
                need,
                gate_by_model,
                lexical_n: 0,
                semantic_n: 0,
                both_n: 0,
                echoed: 0,
                selected: None,
            });
            return input.to_string();
        }

        // 台账读不到就当没有记忆：**记忆是增强，不是对话能不能进行的前提。**
        let Ok(ledger) = yunxi_bot_core::ledger::Ledger::open(self.home.join("ledger.jsonl"))
        else {
            return input.to_string();
        };
        let mem = Memory::from_events(ledger.events());
        let Ok(now) = yunxi_bot_core::now_millis() else {
            return input.to_string();
        };
        // 疲劳参数留着但**不再用了**——`recall_for_prompt` 的签名里它是
        // "会话内计数"，这里传空表；要恢复那套只需把它换成真实计数。
        // **归一逻辑只有一份**（`yunxi_bot_core::memory::normalize_scope`）。
        // 这里和 `remember` 必须算出同一个串，否则那条记忆永远召不回。
        let cwd_scope: Option<String> = std::env::current_dir()
            .ok()
            .map(|d| yunxi_bot_core::memory::normalize_scope(&d));
        let hits = mem.recall_for_prompt(
            input,
            now,
            RECALL_LIMIT,
            RECALL_BUDGET_CHARS,
            &std::collections::HashMap::new(),
            // **当前工作目录。** 用来筛工作目录类记忆——
            // 在 B 项目里不该召回 A 项目的约定。
            // 拿不到就传 None：宁可少给一条，也不要用错目录的约定。
            cwd_scope.as_deref(),
        );
        use yunxi_bot_core::memory::{RecallRoute, RecallVia};
        let lexical_n = hits
            .iter()
            .filter(|h| h.via == RecallVia::Lexical || h.via == RecallVia::Both)
            .count();
        let semantic_n = hits
            .iter()
            .filter(|h| h.via == RecallVia::Semantic || h.via == RecallVia::Both)
            .count();
        let both_n = hits.iter().filter(|h| h.via == RecallVia::Both).count();
        let echoed = hits
            .iter()
            .filter(|h| h.route == RecallRoute::Picked)
            .filter(|h| {
                self.sessions
                    .get(&key)
                    .is_some_and(|l| l.history_mentions(&h.entry.text))
            })
            .count();

        let picked: Vec<&yunxi_bot_core::memory::MemoryEntry> = hits
            .iter()
            .filter(|h| h.route == RecallRoute::Picked)
            // **已经在历史里的不再注入。**
            .filter(|h| {
                self.sessions
                    .get(&key)
                    .is_none_or(|l| !l.history_mentions(&h.entry.text))
            })
            .map(|h| h.entry)
            .collect();

        let ids: Vec<String> = picked
            .iter()
            .map(|e| format!("{}:{}", e.kind.state_key(), e.id))
            .collect();
        Self::diag(TurnDiag {
            input_chars: input.chars().count(),
            need,
            gate_by_model,
            lexical_n,
            semantic_n,
            both_n,
            echoed,
            selected: Some(&ids),
        });

        if picked.is_empty() {
            return input.to_string();
        }
        let block = build_recall_block(&picked);
        if block.is_empty() {
            return input.to_string();
        }
        // 记忆在前、问题在后：**问题在最后一行是标准做法**，
        // 而且这样问题的位置每轮都一样。
        format!("{block}\n{input}")
    }

    /// 把当前对话会话的布局取出来（落盘用）。
    pub fn chat_layout(&self) -> Option<&PromptLayout> {
        self.sessions
            .iter()
            .find(|(k, _)| k.intent == Intent::Chat)
            .map(|(_, v)| v)
    }

    /// 把落盘的布局装回来（`--resume` 用）。
    pub fn restore_chat_layout(&mut self, layout: PromptLayout, provider: &str) {
        self.sessions.insert(
            SessionKey {
                intent: Intent::Chat,
                provider: provider.to_string(),
            },
            layout,
        );
    }

    /// 保证这个 key 对应的会话存在，返回它。
    ///
    /// 抽出来是为了**可测**：`converse` 要模型才能跑，而"会话用的前缀
    /// 到底是不是 `expected_chat_prefix`"这件事不该只有跑一次真对话
    /// 才能验。以前没抽，于是那条测试只能用"自己跟自己比"糊过去——**恒真**。
    fn ensure_session(
        &mut self,
        key: &SessionKey,
        intent: Intent,
    ) -> &yunxi_bot_core::think::prompt::PromptLayout {
        if !self.sessions.contains_key(key) {
            // 稳定前缀 = 人格 + 记忆 + 该意图的指令（+ 项目规则）。
            // **一次定下，之后不再改。** 唯一的来源是 `stable_prefix`。
            let stable = self.stable_prefix(intent);
            self.sessions.insert(key.clone(), PromptLayout::new(stable));
        }
        // 上面那个分支保证它存在
        self.sessions
            .get(key)
            .expect("刚插入过或者本来就有，取不到说明有 bug")
    }

    /// 某个意图的**稳定前缀**。这是唯一的来源。
    ///
    /// ## 为什么要有这个方法（以前是两个地方各拼一份）
    ///
    /// 以前有两份：构造时拼的 `chat_prefix`（给指纹用）和 `converse` 里
    /// 现场拼的那份（真正进请求的）。**它们不一致**：
    ///
    /// - `chat_prefix` = 人格 + CHAT_SYSTEM + **项目规则**
    /// - `converse`   = 人格 + CHAT_SYSTEM            ← **没有项目规则**
    ///
    /// 后果有两个，第二个更坏：
    ///
    /// 1. **项目规则根本没进对话请求。** 在一个放了 `AGENTS.md` 的目录里
    ///    对话，那些约定完全不生效——而 `yunxi-bot rules` 和指纹都说
    ///    "规则加载了"。真机验过：一条"每句话结尾加喵"的规矩，对话里
    ///    一个字都没喵。
    ///
    /// 2. **载入会话时指纹必然对不上**，每次 `--resume` 都报"前缀变了"。
    ///    那是个**假警报**，会让人去查一个不存在的问题。
    ///
    /// 而那条本该拦住它的测试（`the_expected_prefix_matches_what_a_session_would_use`）
    /// 是**空的**：它拿一个串和它自己比，恒真。这一条现在是真比了。
    ///
    /// ## 记忆放在哪
    ///
    /// 记忆段跟在人格后面、指令前面。**不进 `intent.system()`**：
    /// 那是意图的固定指令，记忆是随使用者变的，混在一起以后
    /// 谁也说不清"这段到底是代码写的还是记出来的"。
    ///
    /// 项目规则只给 `Chat`：任务那三条有自己的指令，项目约定对它们
    /// 是另一回事，混进去会让提示词和内容对不上。
    fn stable_prefix(&self, intent: Intent) -> String {
        let mut out = format!("{}\n\n{}", self.persona, intent.system());
        if intent == Intent::Chat {
            // **没有内容就不拼空段**：那会平白占掉前缀的 token，
            // 还每轮都一样地占。
            //
            // 顺序是**人格 → 画像 → 记忆 → 项目规则**：先"我是谁"，
            // 再"你是谁"，再"我记得什么"，最后"这个项目的约定"。
            // 从最稳定到最具体，读起来顺，也符合注意力从远到近的分布。
            if !self.profile_block.is_empty() {
                out.push_str("\n\n");
                out.push_str(&self.profile_block);
            }
            if !self.memory_block.is_empty() {
                out.push_str("\n\n");
                out.push_str(&self.memory_block);
            }
            let rules_text = self.project_rules.render();
            if !rules_text.is_empty() {
                out.push_str("\n\n");
                out.push_str(&rules_text);
            }
        }
        out
    }

    /// 对话稳定前缀的指纹。
    ///
    /// **它是"前缀没变"的机器可读表示。** 报出来之后，使用者
    /// （和测试）不用花一次模型调用来验证跨启动的一致性。
    pub fn chat_prefix_fingerprint(&self) -> u64 {
        // 和 `PromptLayout::fingerprint` 用同一套算法——不然两个数字
        // 对不上，而那种对不上会被误读成"前缀变了"
        let l = yunxi_bot_core::think::prompt::PromptLayout::new(self.stable_prefix(Intent::Chat));
        l.fingerprint()
    }

    /// 启动时加载到的项目规则。
    pub fn project_rules(&self) -> &yunxi_bot_core::rules::RuleSet {
        &self.project_rules
    }

    /// 当前应该用的对话稳定前缀。
    ///
    /// 载入会话时要拿它和存档里的指纹比。**必须在建会话之前就能算出来**——
    /// 算不出来的话就只能"先建再比"，而建了就会覆盖掉存档里的指纹，
    /// 于是每次都报"前缀变了"。
    pub fn expected_chat_prefix(&self) -> String {
        self.stable_prefix(Intent::Chat)
    }

    /// 给一轮对话路由。
    ///
    /// 和任务引擎那条的区别：**那条的路由是引擎按整条任务算的**，
    /// 这里只能按这一句算。
    ///
    /// ## 任务类型现在是**问出来的**，不是写死的
    ///
    /// 原来这里固定传 `TaskKind::Conversation`，理由是"一个对话轮次
    /// 本身不做多步推理"。**那个理由不成立**——用户在对话里说
    /// "帮我把这个文件改一下"就是一件事，而写死 `Conversation`
    /// 会把它永远按闲聊处理、永远走免费的 Agnes，**而那件事根本办不成**。
    ///
    /// 现在：**每次输入问决策模型一次「这是任务还是闲聊」**（只问一次，
    /// 结果用在整轮路由里），判成任务再走词表细化成具体类型。
    ///
    /// 决策模型不可用时**回落闲聊**：那是免费的一边，也最不容易误花钱。
    /// 真想让它干活的人会用 `do`，那条路不受这里影响。
    pub fn route_for(&self, input: &str, effort: ReasoningEffort) -> Routing {
        use yunxi_bot_core::think::router::{TaskKind, profile_task};
        // 对话还没拆解，所以步骤数是 0——`profile_task` 会按长度粗估。
        let profile = profile_task(input, 0);

        // **问一次。** `route()` 自己不再问任何人（D118 之后它谁也不问），
        // 所以不会出现"一次输入问两三次"。
        let verdict = self
            .decider
            .as_deref()
            .and_then(|d| self.router.classify_input(d, input));
        let is_task = verdict.unwrap_or(false);

        // **分类要留痕。**
        //
        // 这是项目自己的原则——`Routing` 的文档原话是"**理由必须进台账**，
        // 否则事后无法解释为什么花了钱"。而分类恰恰是决定"花钱还是免费"
        // 的那一步：判成任务走 DeepSeek（收费），判成闲聊走 Agnes（免费）。
        //
        // 不留痕的话，事后翻台账查不出三件事：
        // 1. 某一句被判成了哪一类
        // 2. 它是不是因为**决策模型挂了**才回的"闲聊"
        //    （`verdict == None` 就是那个信号，`degraded` 记的就是它）
        // 3. 分类一共让它多花了多少钱
        //
        // **第 2 条最要紧**：D104/D106 两次静默降级都是这个形状——
        // 功能还在，只是退化了，而退化之后看起来和正常一模一样。
        //
        // 台账**按需开**，和上面 `with_recalled_memory` 同一个写法——
        // `ChatHandler` 不常驻一个台账句柄（那会让它的生命周期
        // 和调用方纠缠在一起）。开不出来就算了：**留痕失败不该挡住对话。**
        if let Ok(mut ledger) = yunxi_bot_core::ledger::Ledger::open(self.home.join("ledger.jsonl"))
        {
            let _ = yunxi_bot_core::decide::record_decision(
                &mut ledger,
                yunxi_bot_core::decide::DecisionClass::Classify,
                // **拿不到答案就是降级**——不是"它说是闲聊"。
                // 这两件事在行为上一样（都走免费端点），但在台账上必须分得开。
                verdict.is_none(),
                if is_task { "task" } else { "chat" },
                "问决策模型：这句话是要我办事，还是随口聊聊",
                Some("verdict-small"),
                &["input_kind".to_string()],
            );
        }
        // 是任务就细化成具体类型；词表认不出来时按"生成"兜
        // （**那仍然是任务**，只是不知道是哪一类）。
        let kind = if is_task {
            TaskKind::classify(input).unwrap_or(TaskKind::Generation)
        } else {
            TaskKind::Conversation
        };

        self.router
            .route(input, kind, &profile, effort, self.decider.as_deref())
    }

    /// 丢掉对话历史，开一个新的。**不删任何落盘的东西。**
    ///
    /// "想重来"不等于"想把刚才那段扔掉"——删是不可逆的，
    /// 而落盘那份还在，用 `chat list` 就能找回来。
    pub fn reset_chat_session(&mut self) {
        self.sessions.retain(|k, _| k.intent != Intent::Chat);
    }

    /// 拿一个客户端来做摘要。
    ///
    /// **优先便宜的那个**：摘要是简单活，用贵模型是浪费。
    /// 拿不到就返回 `None`，由调用方退回兜底——压缩不能失败。
    pub fn summarizer_thinker(&mut self) -> Option<std::sync::Arc<OpenAiThinker>> {
        for spec in [ModelSpec::AGNES_FLASH, ModelSpec::DEEPSEEK_FLASH] {
            if let Ok(t) = self.thinker(&spec) {
                return Some(t);
            }
        }
        None
    }

    /// 上下文用量。`None` 表示这个会话还不存在。
    pub fn chat_usage(&self) -> Option<yunxi_bot_core::think::context::ContextUsage> {
        self.chat_layout().map(|l| l.usage())
    }

    /// 超出预算就压缩。返回压缩记录，`None` 表示没压。
    ///
    /// **压缩是一次性的、有意的前缀变更。** 代价是那一次缓存未命中，
    /// 换来的是不撞上下文上限。设计细节见
    /// [`yunxi_bot_core::think::context`] 的模块文档。
    pub fn compact_if_needed(
        &mut self,
        budget: &yunxi_bot_core::think::context::ContextBudget,
        summarizer: &dyn yunxi_bot_core::think::context::Summarizer,
        keep_recent: usize,
    ) -> Option<yunxi_bot_core::think::context::Compaction> {
        let key = self
            .sessions
            .iter()
            .find(|(k, _)| k.intent == Intent::Chat)
            .map(|(k, _)| k.clone())?;
        self.sessions
            .get_mut(&key)?
            .compact(summarizer, budget, keep_recent)
    }

    /// 一次带会话的调用。返回模型正文。
    ///
    /// 内部走的是**工具循环**：没挂工具时它等价于一次普通调用
    /// （模型不调工具 → 第一轮就结束），所以不需要两条代码路径。
    fn converse(
        &mut self,
        routing: &Routing,
        intent: Intent,
        volatile: String,
        max_tokens: u32,
        records: &mut Vec<CallRecord>,
    ) -> Result<String, TaskError> {
        // **对话用固定的会话键。**
        //
        // 使用者的对话是**一条**，不该因为这一轮路由到了另一个模型就断开——
        // 那会让"刚才说的那个文件"突然变成对不上话的悬空指代。
        //
        // 而任务引擎那三条按 provider 分是对的：它们是不同的角色，
        // 交叉着用同一个历史反而会让提示词和内容对不上。
        let key = SessionKey {
            intent,
            provider: match intent {
                Intent::Chat => CHAT_SESSION_KEY.to_string(),
                _ => routing.spec.provider.to_string(),
            },
        };
        self.ensure_session(&key, intent);

        let spec = routing.spec.clone();
        let inner = self.thinker(&spec)?;
        // 限流等待与用量记账都包在 thinker 里，于是**每一轮工具调用都享受同样的待遇**。
        // 把它们放在外面就得每加一个调用点都记得处理一次，迟早漏。
        let meter = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let metered = MeteredThinker {
            inner,
            rpm: spec.rpm,
            provider: spec.provider,
            model: spec.model,
            sink: meter.clone(),
        };

        let approver = self.approver.clone();
        let mut ap = approver.lock().map_err(|_| {
            TaskError::Core(yunxi_bot_core::CoreError::Ledger(
                "审批者被毒化（上一次回答时 panic 了）".into(),
            ))
        })?;

        // 借用关系：`sessions` 可变借、`tools` 不可变借——两个不同字段，
        // 同一个函数体里可以共存。客户端已经克隆成 Arc，不再借住 self。
        let layout = self
            .sessions
            .get_mut(&key)
            .ok_or_else(|| TaskError::Core(yunxi_bot_core::CoreError::Ledger("会话丢失".into())))?;
        layout.ask(volatile);

        let ctx = yunxi_bot_core::tool::ToolContext::new(
            std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            yunxi_bot_core::policy::SandboxMode::WorkspaceWrite,
        );
        // **纯生成型的意图不给工具。**
        //
        // 判据是"这一步的产出是文本，还是动作"：
        //
        // - `Plan` 产出一份步骤清单，`Options` 产出一组候选做法——
        //   两者都不该自己动手。它们要的信息，拆出来的步骤会去取。
        // - `Step` 和 `Chat` 是要干活的，必须有工具。
        //
        // 真机上踩过：规划步的提示词写着"只输出一个 JSON 对象"，
        // 可工具循环照样把 10 个工具递过去了——模型于是跑去 `read_file`、
        // `list_dir`，**自己开始干活而不是拆解**，转了 8 轮撞上轮数上限，
        // 整个任务连拆解都没完成。
        //
        // **给不出去的权限就不要给。** 递一个用不上的工具，
        // 换来的只有"模型拿它去绕路"。
        let no_tools = yunxi_bot_core::tool::ToolRegistry::new();
        let tools = match intent {
            Intent::Plan | Intent::Options => &no_tools,
            Intent::Step | Intent::Chat => &self.tools,
        };
        let mut runner = ToolRunner::new(tools, self.policy.clone(), &mut *ap, ctx)
            // **把路由的思考决定带进工具循环。** 忘了这一步，"复杂任务开思考"
            // 就只在没有工具的那条路径上成立——而几乎每条路径都有工具。
            .with_thinking(match routing.thinking_field() {
                Thinking::ServerDefault => None,
                other => Some(other),
            });
        if let Some(d) = &self.decider {
            runner = runner.with_decider(&**d);
        }
        if let Some(f) = &self.on_delta {
            runner = runner.with_on_delta(f.clone());
        }
        // **每一次工具调用都要有人接住。**
        //
        // 不接的话，机器能读使用者的文件、跑命令、抓网页，
        // 而台账里一条记录都没有——那是信任问题，不只是审计问题。
        if let Some(s) = &self.sink {
            runner = runner.with_sink(std::sync::Arc::clone(s));
        }

        // **思考模式的输出预算是"思考 + 正文"的总和。**
        //
        // `max_tokens` 不是正文的额度——思考过程从同一个池子里扣。
        // 所以思考开着时不加余量，正文就会被**从中间截断**，
        // 而截断的表现极具误导性（残缺的 JSON 被当成"没有 steps 字段"）。
        //
        // 放在这里而不是各个调用点：**一处覆盖所有意图**，
        // 也就不会出现"某个意图忘了加"。
        let max_tokens = if routing.thinking {
            max_tokens.saturating_add(yunxi_bot_core::task::engine::THINKING_OUTPUT_HEADROOM)
        } else {
            max_tokens
        };

        let outcome = runner.run(&metered, layout, max_tokens).map_err(|e| {
            TaskError::Core(yunxi_bot_core::CoreError::Ledger(format!(
                "工具循环失败: {e}"
            )))
        })?;

        // 把这一轮所有模型调用的用量一次性交出去
        //
        // **同时拿最大的一次 `prompt_tokens` 当上下文用量的锚点。**
        // 那是"我们这次实际发出去多少 token"的**精确值**——
        // 有了它，字符估算法就只是两次响应之间的临时补丁，误差不会累积。
        //
        // 取最大而不是最后一次：工具循环里提示词逐轮变长，最后一次通常最大；
        // 取最大是保守的，而**保守在这里是安全的**（高估只是早一点压缩，
        // 低估会撞上限）。
        let mut anchor = None;
        if let Ok(mut m) = meter.lock() {
            anchor = m.iter().map(|r| r.usage.prompt_tokens).max();
            records.append(&mut m);
        }
        if let (Some(a), Some(l)) = (anchor, self.sessions.get_mut(&key)) {
            l.anchor_usage(a as usize);
        }
        // "总是允许"的规则要能留到后面的调用——它是使用者的决定，不该每轮重问
        self.policy = runner.policy().clone();

        Ok(outcome.text)
    }
}

/// 包在客户端外面的两件事：**客户端限流等待**与**用量记账**。
///
/// ## "本地/客户端限流"是什么意思
///
/// **不是"本地模型被限流"。** 本地跑的那个决策模型（Verdict）
/// 走的是 `decide/` 那一套，**它没有任何限流器**。
///
/// 这里限的是**远端调用**：Agnes 免费档只有 10 RPM，DeepSeek 是 60 RPM
/// （`think/agnes.rs:100` / `:119`）。**每个端点各有一个限流器，额度各自独立**
/// ——所以 Agnes 的 10 不会把 DeepSeek 的 60 拖下来。
///
/// 之所以要在客户端自己压着发：**撞到服务端再退避比主动排队贵得多**
/// （一次 429 要重发整条请求，还可能带上思考模式的输出预算）。
///
/// 包成 `Thinker` 而不是散在调用点，是为了让工具循环的每一轮自动享受同样待遇。
/// 放在外面就得每加一个调用点都记得处理一次——而"记得"是靠不住的。
struct MeteredThinker {
    /// **trait object 而不是具体类型。**
    ///
    /// 用 `Arc<OpenAiThinker>` 的话，"这个 wrapper 有没有转发
    /// `think_stream`"就**没法用替身测**——而那个 bug 正是漏转发造成的。
    /// 不能测的地方就是会长 bug 的地方。
    inner: std::sync::Arc<dyn Thinker>,
    rpm: u32,
    provider: &'static str,
    model: &'static str,
    sink: std::sync::Arc<std::sync::Mutex<Vec<CallRecord>>>,
}

impl Thinker for MeteredThinker {
    /// **必须转发 `think_stream`。**
    ///
    /// 这个 wrapper 包在主链路外面（负责限流等待和用量记账）。
    /// 它一开始只实现了 `think`，于是 `think_stream` 落到 trait 的默认实现上：
    /// **等整段答完，再一次性交出去**——流式在外层被整个吞掉了。
    ///
    /// 而这个 bug 极难发现：功能"能用"（正文照样出来、工具照样调），
    /// 只是不再逐字显示。**是端到端的时间证据抓到的**——
    /// 首字节和结尾的时间戳一模一样，而直接打 API 的探针显示
    /// provider 那边确实在流（304 个 chunk 跨 2.6 秒）。
    ///
    /// 教训：**中间层只要漏转发一个方法，就会静默改变行为。**
    fn think_stream(
        &self,
        req: &ThinkRequest,
        on_delta: &mut dyn FnMut(&str),
    ) -> Result<yunxi_bot_core::think::ThinkResponse, ThinkError> {
        let rpm = self.rpm;
        let resp = retry_throttled(
            || self.inner.think_stream(req, on_delta),
            |wait| {
                eprintln!(
                    "  · 客户端限流（{rpm} RPM，为免撞端点额度自己压的），等 {} ms 后重发",
                    wait.as_millis()
                );
            },
        )?;
        self.record(req, &resp);
        Ok(resp)
    }

    fn think(
        &self,
        req: &ThinkRequest,
    ) -> Result<yunxi_bot_core::think::ThinkResponse, ThinkError> {
        let rpm = self.rpm;
        let resp = retry_throttled(
            || self.inner.think(req),
            |wait| {
                eprintln!(
                    "  · 客户端限流（{rpm} RPM，为免撞端点额度自己压的），等 {} ms 后重发",
                    wait.as_millis()
                );
            },
        )?;
        self.record(req, &resp);
        Ok(resp)
    }

    fn model(&self) -> &str {
        self.model
    }
}

impl MeteredThinker {
    /// 记账：**存原始计数**，金额事后按价格表算。
    ///
    /// 抽出来是因为流式和非流式**两条路径都要记**——
    /// 只在一条上记会让成本少报一半，而那种少报不会报错。
    fn record(&self, req: &ThinkRequest, resp: &yunxi_bot_core::think::ThinkResponse) {
        if let Ok(mut s) = self.sink.lock() {
            // 只把**显式开的**记成思考。
            //
            // 记录成 `None`（不指定）时服务端可能仍然思考——对 DeepSeek 默认就是开。
            // 所以"没指定"记成 false 是不准确的，但记成 true 更不准确：
            // 前者低估、后者虚报。选择低估，因为**台账宁可少记也不要虚报**，
            // 而且调用方（工具循环）现在总会显式指定，这条退路只在不指定的旧路径上生效。
            let did_think = req.thinking == Some(Thinking::Enabled);
            s.push(
                CallRecord::new(
                    self.provider,
                    self.model,
                    yunxi_bot_core::costlog::peak_now(),
                )
                .with_thinking(did_think)
                .with_usage(resp.usage),
            );
        }
    }
}

/// 等待上限：比它更长就该让调用方决定（可能是配额真的用完了）。
pub const MAX_THROTTLE_WAIT: std::time::Duration = std::time::Duration::from_secs(30);
/// 最多等几轮。等完还不行就如实报错，别让常驻进程无限阻塞。
pub const MAX_THROTTLE_ROUNDS: u32 = 4;

/// 本地限流重试。**"现在不行"不是"不行"。**
///
/// 这一条是被真实运行逼出来的：Agnes 免费档 10 RPM，一次任务连着几个步骤必然
/// 撞上限流。第一版把 `LocalThrottle` 当普通错误往上抛，于是引擎把它算作
/// "步骤失败"、消耗重试次数，最后一步都跑不完——而**实际上什么错都没有，
/// 只是需要等几秒**。
///
/// 抽成接受闭包的纯函数是为了它能被测试：这个 bug 值一条回归测试，
/// 而它不该需要网络才能测。
pub fn retry_throttled<F, N>(
    mut call: F,
    mut on_wait: N,
) -> Result<yunxi_bot_core::think::ThinkResponse, ThinkError>
where
    F: FnMut() -> Result<yunxi_bot_core::think::ThinkResponse, ThinkError>,
    N: FnMut(std::time::Duration),
{
    let mut rounds = 0u32;
    loop {
        let err = match call() {
            Ok(r) => return Ok(r),
            Err(e) => e,
        };
        if rounds >= MAX_THROTTLE_ROUNDS || !err.is_retryable() {
            return Err(err);
        }
        let wait = match &err {
            // 本地限流器说等多久就等多久
            ThinkError::LocalThrottle { wait } => *wait,
            // **服务端 429 也要等。**
            //
            // 这是 D17 那条教训第三次以新面孔出现：本地限流等，
            // 服务端的 429 却直接判失败——而两者是同一件事。
            // 表现是连着跑几个脚本就开始"这一轮失败了"，
            // 看起来像产品坏了，其实只是需要等几秒。
            //
            // 服务端给了 `Retry-After` 就听它的；没给就按轮次退避。
            ThinkError::RateLimited { retry_after, .. } => retry_after
                .unwrap_or_else(|| std::time::Duration::from_millis(1500 * (rounds as u64 + 1))),
            // 网络抖动和服务端 5xx：短退避
            ThinkError::Transient { .. } | ThinkError::Network(_) => {
                std::time::Duration::from_millis(800 * (rounds as u64 + 1))
            }
            // `is_retryable` 说不会走到这里
            _ => return Err(err),
        };
        let wait = wait.min(MAX_THROTTLE_WAIT);
        if wait.is_zero() {
            // 等一下也没用，别空转
            return Err(err);
        }
        on_wait(wait);
        std::thread::sleep(wait);
        rounds += 1;
    }
}

impl TaskHandler for ChatHandler {
    fn plan(
        &mut self,
        routing: &Routing,
        run: &PlanRequest,
        records: &mut Vec<CallRecord>,
    ) -> Result<String, TaskError> {
        // 用户消息只放目标与上限。**人格和格式要求都在稳定前缀里**，
        // 所以不同任务之间这一段逐字节相同。
        let volatile = format!("目标：{}\n\n最多拆成 {} 步。", run.goal, run.max_steps);
        self.converse(routing, Intent::Plan, volatile, PLAN_MAX_TOKENS, records)
    }

    fn execute_step(
        &mut self,
        routing: &Routing,
        run: &StepRequest,
        records: &mut Vec<CallRecord>,
    ) -> Result<String, TaskError> {
        let mut volatile = format!("总目标：{}\n\n这一步要做：{}", run.goal, run.instruction);
        if !run.inputs.is_empty() {
            volatile.push_str("\n\n前置步骤的结果：");
            for (id, out) in &run.inputs {
                volatile.push_str(&format!("\n[{id}] {out}"));
            }
        }
        self.converse(routing, Intent::Step, volatile, STEP_MAX_TOKENS, records)
    }

    fn observe(
        &mut self,
        routing: &Routing,
        run: &ObserveRequest,
        records: &mut Vec<CallRecord>,
    ) -> Result<String, TaskError> {
        // **决策点必须看到证据。**
        //
        // 在这之前这里只有一句话（那一步的指令）：`goal` 是空的、
        // 前置步骤的结果一个都没带。于是生成选项的模型只能凭那句话编，
        // 而 Verdict（双编码器，按选项文本打分）**更是一点证据都拿不到**。
        //
        // 真机后果：决策步选了「门槛按 > 判定」，而前面的分析步骤已经
        // 写着 README 明说「满减的边界条件写错了」。执行步骤当场发现
        // 矛盾、拒绝照做，任务卡住。**那不是它拍错，是没给它材料。**
        let mut volatile = format!("总目标：{}\n\n需要拍板的问题：{}", run.goal, run.question);
        if !run.inputs.is_empty() {
            volatile.push_str("\n\n判断所需的证据（前置步骤的结果）：");
            for (id, out) in &run.inputs {
                volatile.push_str(&format!("\n[{id}] {out}"));
            }
        }
        self.converse(
            routing,
            Intent::Options,
            volatile,
            OPTIONS_MAX_TOKENS,
            records,
        )
    }
}

/// 默认人格。等使用者给出自己的版本就能覆盖。
/// 内置的助理名字。
///
/// **它只是"还没建过 `persona.md` 时的起点"。** 改名字请改那个文件——
/// 改这里要重编译，那就不是"你的人格"了。
pub const DEFAULT_PERSONA_NAME: &str = "云熙";

pub const DEFAULT_PERSONA: &str = "\
你是常驻在用户自己电脑上的个人助理。说话克制、直接、不客套，不堆感叹号。\
不确定的事就说不确定，不要用漂亮的措辞掩盖没把握。";

/// 默认硬规则。**这几条是不可违背的边界**，不是建议。
pub fn default_rules() -> Vec<String> {
    vec![
        "不可逆的动作（删除、覆盖、发送、付款）一律先停下来问，不要自己决定。".to_string(),
        "不确定就说不确定，绝不用编造的内容填空。".to_string(),
        "能一句话说清就不要写三段。".to_string(),
    ]
}

/// 干跑用的 handler：不碰网络，把请求原样记下来。
///
/// `yunxi-bot do --dry-run` 用它，让使用者**在花钱之前**看到会怎么拆、会怎么路由。
pub struct DryRunHandler {
    /// 网络被替换成的固定拆解结果。故意只给一步，避免假装知道真实拆解。
    pub canned_plan: String,
    pub seen: Vec<String>,
}

impl Default for DryRunHandler {
    fn default() -> Self {
        Self {
            canned_plan: serde_json::json!({
                "steps": [{
                    "id": "s1",
                    "instruction": "（干跑：真实运行时会在这里拆出具体步骤）",
                    "depends_on": [],
                    "kind": "generation"
                }]
            })
            .to_string(),
            seen: Vec::new(),
        }
    }
}

impl TaskHandler for DryRunHandler {
    fn plan(
        &mut self,
        routing: &Routing,
        run: &PlanRequest,
        _records: &mut Vec<CallRecord>,
    ) -> Result<String, TaskError> {
        self.seen
            .push(format!("拆解 → {}｜{}", routing.summary(), run.goal));
        Ok(self.canned_plan.clone())
    }

    fn execute_step(
        &mut self,
        routing: &Routing,
        run: &StepRequest,
        _records: &mut Vec<CallRecord>,
    ) -> Result<String, TaskError> {
        self.seen.push(format!(
            "步骤 {} → {}｜{}",
            run.step_id,
            routing.summary(),
            run.instruction
        ));
        Ok(format!("（干跑）{} 的产出", run.step_id))
    }

    fn observe(
        &mut self,
        routing: &Routing,
        run: &ObserveRequest,
        _records: &mut Vec<CallRecord>,
    ) -> Result<String, TaskError> {
        self.seen
            .push(format!("决策点 → {}｜{}", routing.summary(), run.question));
        Ok(serde_json::json!({
            "question": run.question,
            "options": ["（干跑）选项甲", "（干跑）选项乙"]
        })
        .to_string())
    }
}

/// 供 CLI 显示：这个路由会开思考吗、走谁。
pub fn route_line(routing: &Routing, effort: ReasoningEffort) -> String {
    format!(
        "{}｜思考{}｜{}",
        routing.summary(),
        match routing.thinking_field() {
            Thinking::ServerDefault => "不适用",
            Thinking::Enabled => "开",
            Thinking::Disabled => "关",
        },
        effort.label()
    )
}

#[cfg(test)]
mod chat_session_tests {
    use super::*;

    fn handler() -> ChatHandler {
        ChatHandler::new(
            std::path::PathBuf::from("."),
            "云熙",
            DEFAULT_PERSONA,
            default_rules(),
        )
    }

    #[test]
    fn loaded_rules_actually_reach_the_stable_prefix() {
        // **"加载了"和"进了前缀"是两件事。**
        //
        // 调试时踩过：规则加载正常、`/rules` 列得好好的，而模型说
        // "系统提示词里没有这一节"。那次差一点去查模型，其实是接线断了。
        //
        // 这条断言不依赖具体环境（有没有 AGENTS.md 都成立）：
        // **只要加载到了规则，它就必须出现在前缀里。**
        let h = handler();
        let rs = h.project_rules();
        if rs.is_empty() {
            return; // 这个目录没有规则，无从断言——不是失败
        }
        let prefix = h.expected_chat_prefix();
        for f in &rs.files {
            let first = f.text.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
            assert!(
                prefix.contains(first),
                "规则 {} 加载了却没进前缀——接线断了",
                f.path.display()
            );
        }
    }

    #[test]
    fn the_prefix_has_no_empty_rules_section_when_nothing_was_found() {
        // 没有规则时不拼空段：那会平白占掉前缀的 token，还每轮都一样地占
        let h = handler();
        if h.project_rules().is_empty() {
            assert!(
                !h.expected_chat_prefix().contains("# 项目约定"),
                "没有规则却拼了个空标题"
            );
        }
    }

    #[test]
    fn the_prefix_fingerprint_is_available_without_a_model_call() {
        // **确定性断言不该挂在概率性的东西上。**
        //
        // 端到端一开始是读会话文件里的指纹——而那个文件只在
        // "有过成功的一轮"之后才写。模型一失败（限流、超时），
        // 指纹就是 None，被误报成"前缀不一致"。
        //
        // 所以指纹要能不花一次模型调用就拿到。
        let h = handler();
        let a = h.chat_prefix_fingerprint();
        let b = h.chat_prefix_fingerprint();
        assert_eq!(a, b, "同一次进程里两次算出来必须一样");
    }

    #[test]
    fn the_reported_fingerprint_matches_what_a_session_would_record() {
        // **两个数字对不上会被误读成"前缀变了"。**
        // 上报的指纹和 `PromptLayout` 落盘的那个必须是同一套算法。
        //
        // 名字说的是"和会话**会记录**的那个一致"，所以这里就真的去建一个
        // 会话、拿它自己算出来的指纹比——**而不是拿期望前缀再建一个
        // layout 跟自己比**。后者只验了"指纹算法一致"，
        // 而上一版就是这么写的：名字管着一个更强的承诺，做的事少一截。
        let mut h = handler();
        let key = SessionKey {
            intent: Intent::Chat,
            provider: "p".into(),
        };
        let recorded = h.ensure_session(&key, Intent::Chat).fingerprint();
        assert_eq!(
            h.chat_prefix_fingerprint(),
            recorded,
            "上报的指纹和会话实际记录的不是同一个——载入时会误报'前缀变了'"
        );
    }

    #[test]
    fn the_chat_prefix_is_stable_across_calls() {
        // **这是缓存命中的前提。** 前缀一变，缓存全废——
        // 而那种失效是静默的，只表现为账单变贵。
        let h = handler();
        let a = h.expected_chat_prefix().to_string();
        let b = h.expected_chat_prefix().to_string();
        assert_eq!(a, b);
    }

    #[test]
    fn the_chat_prefix_contains_the_persona_and_the_chat_rules() {
        let h = handler();
        let p = h.expected_chat_prefix();
        assert!(p.contains("云熙"), "人格该在里面");
        assert!(p.contains("通用 agent"), "对话指令该在里面");
    }

    #[test]
    fn two_handlers_built_the_same_way_have_the_same_prefix() {
        // 否则每次启动都是新前缀，跨进程的缓存永远命不中
        assert_eq!(
            handler().expected_chat_prefix(),
            handler().expected_chat_prefix()
        );
    }

    #[test]
    fn the_expected_prefix_matches_what_a_session_would_use() {
        // **载入会话时要拿它和存档里的指纹比。**
        // 如果这里算出来的和 `converse` 建会话时用的不是同一份，
        // 就会每次都报"前缀变了"——而那是假警报，会让人去查一个不存在的问题。
        //
        // ## 这条测试原来是空的
        //
        // 原来它这么写：
        //
        // ```ignore
        // let expected = h.expected_chat_prefix().to_string();
        // let built = PromptLayout::new(expected.clone());
        // assert_eq!(built.fingerprint(), built.fingerprint_for(&expected));
        // ```
        //
        // **它拿一个串和它自己比，恒真。** 它从来没有和 `converse` 真正
        // 建会话时用的那份比过——所以两份前缀漂移了很久它都没吭声，
        // 而漂移的后果是**项目规则根本没进对话请求**（真机上验过：
        // 一条"每句话结尾加喵"的规矩，对话里一个字都没喵）。
        //
        // 现在真比：把会话建出来，拿它的头一条消息和期望的前缀比。
        let mut h = handler();
        let expected = h.expected_chat_prefix();

        let key = SessionKey {
            intent: Intent::Chat,
            provider: "p".into(),
        };
        // `head()` 是**历史**的第一条（稳定前缀在下标 0，历史从 1 开始），
        // 所以这里得走 `build()`。这个偏移很容易看错。
        let built = h.ensure_session(&key, Intent::Chat).build();
        let Some(actual) = built.first() else {
            panic!("建出来的会话该有一条稳定前缀消息");
        };
        assert_eq!(
            actual.content, expected,
            "**期望的前缀和会话里真正用的不是同一份。**\n这会同时导致两件事：\n  - 项目规则/记忆段没进请求\n  - 每次 resume 都报'前缀变了'（假警报）"
        );
    }

    #[test]
    fn reset_clears_only_the_chat_session() {
        // 任务引擎那三个会话是**另一个角色**，不该被"清空对话"波及
        let mut h = handler();
        h.restore_chat_layout(PromptLayout::new("对话前缀"), CHAT_SESSION_KEY);
        h.sessions.insert(
            SessionKey {
                intent: Intent::Plan,
                provider: "p".into(),
            },
            PromptLayout::new("计划前缀"),
        );
        assert!(h.chat_layout().is_some());

        h.reset_chat_session();
        assert!(h.chat_layout().is_none(), "对话该被清掉");
        assert!(
            h.sessions.iter().any(|(k, _)| k.intent == Intent::Plan),
            "计划会话不该被动到"
        );
    }

    #[test]
    fn restore_then_read_round_trips() {
        let mut h = handler();
        let mut l = PromptLayout::new(h.expected_chat_prefix().to_string());
        l.ask("问题");
        l.record_reply("回答");
        let fp = l.fingerprint();
        h.restore_chat_layout(l, CHAT_SESSION_KEY);

        let got = h.chat_layout().expect("该装回来了");
        assert_eq!(got.history_len(), 2);
        assert_eq!(got.fingerprint(), fp);
    }

    #[test]
    fn resuming_onto_the_same_prefix_reports_no_change() {
        // **不能假报变化。** 假警报会让人去查一个不存在的问题。
        let h = handler();
        let mut l = PromptLayout::new(h.expected_chat_prefix().to_string());
        l.ask("q");
        l.record_reply("a");
        let f = yunxi_bot_core::think::session::SessionFile {
            yunxi_bot_session: yunxi_bot_core::think::session::SESSION_FORMAT_VERSION,
            id: "t".into(),
            created_at: 0,
            updated_at: 0,
            turns: 1,
            fingerprint: l.fingerprint(),
            layout: l,
        };
        let (_, m) = yunxi_bot_core::think::session::resume_onto(&f, &h.expected_chat_prefix());
        assert_eq!(m, yunxi_bot_core::think::session::PrefixMismatch::Same);
    }

    #[test]
    fn a_router_is_available_for_chat_turns() {
        // 对话要自己判断"这活复杂不复杂"——任务引擎那条路由是引擎给的
        let h = handler();
        let r = h.route_for("你好", yunxi_bot_core::think::ReasoningEffort::Auto);
        assert_eq!(r.spec.provider, "agnes", "闲聊不该走贵的那条");
    }

    #[test]
    fn chat_is_a_distinct_intent_with_its_own_prompt() {
        // 四个意图的提示词不能撞（撞了就等于没有角色区分）
        let prompts = [
            Intent::Chat.system(),
            Intent::Plan.system(),
            Intent::Step.system(),
            Intent::Options.system(),
        ];
        for i in 0..prompts.len() {
            for j in (i + 1)..prompts.len() {
                assert_ne!(prompts[i], prompts[j], "第 {i} 和第 {j} 个提示词撞了");
            }
        }
    }

    #[test]
    fn the_chat_prompt_requires_honesty_about_failure() {
        // **编一个看起来像结果的东西会让整条链路在错误前提上继续跑。**
        let s = CHAT_SYSTEM;
        assert!(s.contains("做不了就说做不了"), "缺了最重要的一条");
        assert!(s.contains("不要编"), "要说清编造的后果");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use yunxi_bot_core::think::{ThinkError, ThinkResponse};

    fn ok() -> ThinkResponse {
        ThinkResponse {
            content: "成了".into(),
            model: "stub".into(),
            usage: Default::default(),
            finish_reason: Some("stop".into()),
            reasoning: None,
            thinking: Thinking::Disabled,
            tool_calls: Vec::new(),
        }
    }

    /// 回归：**本地限流不是失败**。
    ///
    /// 真实运行里 Agnes 免费档 10 RPM，一个任务连着几步必然撞限流。
    /// 第一版把它当普通错误抛上去，引擎就把它算成"步骤失败"、消耗重试次数，
    /// 最后一步都跑不完——而实际上只需要等几秒。
    /// 一个成功的假响应。测试不该需要网络。
    fn ok_response() -> ThinkResponse {
        ThinkResponse {
            content: "好了".into(),
            model: "m".into(),
            usage: Default::default(),
            finish_reason: None,
            reasoning: None,
            thinking: yunxi_bot_core::think::Thinking::Disabled,
            tool_calls: Vec::new(),
        }
    }

    use yunxi_bot_core::think::{Message, Usage};

    /// 一个"能流式"的 thinker：把回复拆成三片。
    struct ChunkedThinker {
        stream_calls: std::sync::atomic::AtomicUsize,
    }

    impl Thinker for ChunkedThinker {
        fn think(&self, _req: &ThinkRequest) -> Result<ThinkResponse, ThinkError> {
            // **wrapper 漏转发 `think_stream` 时就会走到这里**——
            // 而这里刻意不分片，于是测试能看出区别
            Ok(ThinkResponse {
                content: "整段".into(),
                model: "m".into(),
                usage: Usage::default(),
                finish_reason: None,
                reasoning: None,
                thinking: Thinking::Disabled,
                tool_calls: Vec::new(),
            })
        }

        fn think_stream(
            &self,
            _req: &ThinkRequest,
            on_delta: &mut dyn FnMut(&str),
        ) -> Result<ThinkResponse, ThinkError> {
            self.stream_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            for part in ["一", "二", "三"] {
                on_delta(part);
            }
            Ok(ThinkResponse {
                content: "一二三".into(),
                model: "m".into(),
                usage: Usage::default(),
                finish_reason: None,
                reasoning: None,
                thinking: Thinking::Disabled,
                tool_calls: Vec::new(),
            })
        }

        fn model(&self) -> &str {
            "chunked"
        }
    }

    fn metered_for_test(
        inner: std::sync::Arc<ChunkedThinker>,
        sink: std::sync::Arc<std::sync::Mutex<Vec<CallRecord>>>,
    ) -> MeteredThinker {
        MeteredThinker {
            inner,
            rpm: 0,
            provider: "p",
            model: "m",
            sink,
        }
    }

    #[test]
    fn the_metered_wrapper_forwards_streaming() {
        // **这个 bug 是端到端的时间证据抓到的。**
        //
        // wrapper 漏转发 `think_stream` 的话，它会落到 trait 的默认实现：
        // 等整段答完再一次性交出去——功能"能用"（正文照样出来、
        // 工具照样调），只是不再逐字显示。这种"能用但不对"最难发现。
        //
        // 实测差别：修之前正文 2 批到达、首末差 0.00s；
        // 修之后 209 批、首末差 3.56s。
        let calls = std::sync::Arc::new(ChunkedThinker {
            stream_calls: std::sync::atomic::AtomicUsize::new(0),
        });
        let sink = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let w = metered_for_test(calls.clone(), sink);

        let mut parts = Vec::new();
        let resp = w
            .think_stream(&ThinkRequest::new(vec![Message::user("hi")]), &mut |t| {
                parts.push(t.to_string())
            })
            .unwrap();

        assert_eq!(
            calls.stream_calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "没转发——内层根本没走流式"
        );
        assert_eq!(parts, vec!["一", "二", "三"], "增量该原样传出来");
        assert_eq!(resp.content, "一二三");
    }

    #[test]
    fn the_wrapper_still_records_usage_when_streaming() {
        // 转发流式**不能把记账丢了**——那样成本就少报，
        // 而"少报"不会报错，只会让账单和实际对不上
        let calls = std::sync::Arc::new(ChunkedThinker {
            stream_calls: std::sync::atomic::AtomicUsize::new(0),
        });
        let sink = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let w = metered_for_test(calls, sink.clone());
        w.think_stream(&ThinkRequest::new(vec![Message::user("hi")]), &mut |_| {})
            .unwrap();
        assert_eq!(sink.lock().unwrap().len(), 1, "流式这一次也该记上");
    }

    #[test]
    fn a_server_side_429_is_retried_not_failed() {
        // **这是 D17 那条教训第三次以新面孔出现。**
        //
        // 本地限流等得好好的，服务端的 429 却直接判失败——
        // 而两者是同一件事。表现是连着跑几个脚本就开始"这一轮失败了"，
        // 看起来像产品坏了，其实只是需要等几秒。
        let mut calls = 0;
        let r = retry_throttled(
            || {
                calls += 1;
                if calls < 3 {
                    Err(ThinkError::RateLimited {
                        retry_after: Some(Duration::from_millis(1)),
                        detail: String::new(),
                    })
                } else {
                    Ok(ok_response())
                }
            },
            |_| {},
        );
        assert!(r.is_ok(), "429 该被等过去: {r:?}");
        assert_eq!(calls, 3);
    }

    #[test]
    fn a_429_without_retry_after_still_backs_off() {
        // 服务端没给 `Retry-After` 时不能立刻重发——那只会再撞一次
        let mut waits = Vec::new();
        let mut calls = 0;
        let _ = retry_throttled(
            || {
                calls += 1;
                if calls < 2 {
                    Err(ThinkError::RateLimited {
                        retry_after: None,
                        detail: String::new(),
                    })
                } else {
                    Err(ThinkError::Auth("停".into()))
                }
            },
            |w| waits.push(w),
        );
        assert_eq!(waits.len(), 1, "该等一次");
        assert!(!waits[0].is_zero(), "退避不能是 0");
    }

    #[test]
    fn a_non_retryable_error_is_not_retried() {
        // **重试认证错误只是浪费时间和配额。**
        let mut calls = 0;
        let r = retry_throttled(
            || {
                calls += 1;
                Err::<ThinkResponse, _>(ThinkError::Auth("密钥不对".into()))
            },
            |_| {},
        );
        assert!(r.is_err());
        assert_eq!(calls, 1, "不该重试");
    }

    #[test]
    fn transient_errors_are_retried() {
        let mut calls = 0;
        let r = retry_throttled(
            || {
                calls += 1;
                if calls < 2 {
                    Err(ThinkError::Transient {
                        status: Some(503),
                        detail: String::new(),
                    })
                } else {
                    Ok(ok_response())
                }
            },
            |_| {},
        );
        assert!(r.is_ok(), "5xx 该被等过去");
        assert_eq!(calls, 2);
    }

    #[test]
    fn local_throttle_is_waited_out_not_failed() {
        let mut calls = 0;
        let mut waits = Vec::new();
        let r = retry_throttled(
            || {
                calls += 1;
                if calls == 1 {
                    Err(ThinkError::LocalThrottle {
                        wait: Duration::from_millis(1),
                    })
                } else {
                    Ok(ok())
                }
            },
            |w| waits.push(w),
        );
        assert!(r.is_ok(), "等一会儿之后应该成功: {r:?}");
        assert_eq!(calls, 2, "应该重发一次");
        assert_eq!(waits.len(), 1, "应该只等一次");
    }

    #[test]
    fn throttle_waiting_gives_up_instead_of_hanging_forever() {
        // 常驻进程不能无限阻塞：等够轮数就如实报错
        let mut calls = 0;
        let r = retry_throttled(
            || {
                calls += 1;
                Err(ThinkError::LocalThrottle {
                    wait: Duration::from_millis(1),
                })
            },
            |_| {},
        );
        assert!(matches!(r, Err(ThinkError::LocalThrottle { .. })), "{r:?}");
        assert_eq!(calls, MAX_THROTTLE_ROUNDS + 1, "等满轮数后要放弃");
    }

    #[test]
    fn zero_wait_does_not_spin() {
        // 等待时间为 0 时不能空转——那会变成一个吃 CPU 的死循环
        let mut calls = 0;
        let r = retry_throttled(
            || {
                calls += 1;
                Err(ThinkError::LocalThrottle {
                    wait: Duration::ZERO,
                })
            },
            |_| panic!("不该报告等待"),
        );
        assert!(matches!(r, Err(ThinkError::LocalThrottle { .. })));
        assert_eq!(calls, 1, "0 等待应立刻放弃，不是重试");
    }

    #[test]
    fn other_errors_pass_straight_through() {
        // 密钥错误重试多少次都没用，必须原样上报
        let mut calls = 0;
        let r = retry_throttled(
            || {
                calls += 1;
                Err(ThinkError::Auth("密钥无效".into()))
            },
            |_| {},
        );
        assert!(matches!(r, Err(ThinkError::Auth(_))), "{r:?}");
        assert_eq!(calls, 1, "认证错误不该重试");
    }

    #[test]
    fn long_waits_are_capped() {
        // 单次等待有上限，避免一个超长等待把常驻进程卡死
        assert!(MAX_THROTTLE_WAIT <= Duration::from_secs(60));
    }

    #[test]
    fn persona_and_rules_land_in_the_stable_prefix() {
        // 稳定前缀是缓存命中的前提，人格与硬规则必须真的在里面
        let h = ChatHandler::new(
            std::path::PathBuf::from("."),
            "云熙",
            DEFAULT_PERSONA,
            default_rules(),
        );
        let prefixes = h.stable_prefixes();
        assert_eq!(prefixes.len(), 3, "拆解 / 执行 / 选项 各一套");
        for (name, p) in &prefixes {
            assert!(p.contains("云熙"), "{name} 前缀里应有人格名");
            assert!(p.contains("不可逆"), "{name} 前缀里应有硬规则");
        }
        // 三套必须互不相同，否则就是复用错了会话
        assert_ne!(prefixes[0].1, prefixes[1].1);
        assert_ne!(prefixes[1].1, prefixes[2].1);
    }

    #[test]
    fn stable_prefixes_are_byte_identical_across_instances() {
        // 换一个实例前缀必须逐字节相同——否则每次重启都会让缓存全废
        let mk = || {
            ChatHandler::new(
                std::path::PathBuf::from("."),
                "云熙",
                DEFAULT_PERSONA,
                default_rules(),
            )
            .stable_prefixes()
        };
        assert_eq!(mk(), mk());
    }

    #[test]
    fn default_rules_are_not_empty() {
        // 硬规则是边界，空列表意味着边界没了
        assert!(!default_rules().is_empty());
        assert!(default_rules().iter().any(|r| r.contains("不可逆")));
    }
}
