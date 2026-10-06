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
   - 做不了 → `BLOCKED:` 然后一句话说清缺什么（缺信息、缺权限、缺工具）。
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
    chat_prefix: String,
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
    /// 审批策略。
    policy: ToolPolicy,
    /// 路由。**对话要自己判断"这活复杂不复杂"**——
    /// 任务引擎那条路由是引擎给的，对话这条得自己算。
    router: yunxi_bot_core::think::ModelRouter,
}

impl ChatHandler {
    /// `persona_text` 与 `rules` 来自配置；它们会被拼进稳定前缀。
    pub fn new(home: PathBuf, persona_name: &str, persona_text: &str, rules: Vec<String>) -> Self {
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
        Self {
            home,
            thinkers: BTreeMap::new(),
            sessions: BTreeMap::new(),
            chat_prefix: {
                let base = format!(
                    "{}\n\n{}",
                    build_persona(persona_name, persona_text, &rules),
                    CHAT_SYSTEM
                );
                let rules_text = project_rules.render();
                // **没有规则时不拼空段**：那会平白占掉前缀的 token，
                // 还每轮都一样地占。
                if rules_text.is_empty() {
                    base
                } else {
                    format!("{base}\n\n{rules_text}")
                }
            },
            project_rules,
            persona: build_persona(persona_name, persona_text, &rules),
            tools: ToolRegistry::new(),
            approver: std::sync::Arc::new(std::sync::Mutex::new(RefusingApprover)),
            decider: None,
            // 默认不记录。CLI 一定会用 `with_sink` 覆盖它——
            // 默认值是给"这条链路不该记录"的场景留的显式出口。
            sink: None,
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
        self.converse(
            routing,
            Intent::Chat,
            input.to_string(),
            CHAT_MAX_TOKENS,
            records,
        )
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

    /// 启动时加载到的项目规则。
    pub fn project_rules(&self) -> &yunxi_bot_core::rules::RuleSet {
        &self.project_rules
    }

    /// 当前应该用的对话稳定前缀。
    ///
    /// 载入会话时要拿它和存档里的指纹比。**必须在建会话之前就能算出来**——
    /// 算不出来的话就只能"先建再比"，而建了就会覆盖掉存档里的指纹，
    /// 于是每次都报"前缀变了"。
    pub fn expected_chat_prefix(&self) -> &str {
        self.chat_prefix.as_str()
    }

    /// 给一轮对话路由。
    ///
    /// 和任务引擎那条的区别：**那条的路由是引擎按整条任务算的**，
    /// 这里只能按这一句算。所以任务类型固定为 `Conversation`——
    /// 一个对话轮次本身不做多步推理，复杂度体现在它要用几次工具上。
    pub fn route_for(&self, input: &str, effort: ReasoningEffort) -> Routing {
        use yunxi_bot_core::think::router::{TaskKind, profile_task};
        // 对话还没拆解，所以步骤数是 0——`profile_task` 会按长度粗估。
        let profile = profile_task(input, 0);
        self.router.route(
            input,
            TaskKind::Conversation,
            &profile,
            effort,
            self.decider.as_deref(),
        )
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
        if !self.sessions.contains_key(&key) {
            // 稳定前缀 = 人格 + 该意图的指令。**一次定下，之后不再改。**
            let stable = format!("{}\n\n{}", self.persona, intent.system());
            self.sessions.insert(key.clone(), PromptLayout::new(stable));
        }

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
        let mut runner = ToolRunner::new(&self.tools, self.policy.clone(), &mut *ap, ctx)
            // **把路由的思考决定带进工具循环。** 忘了这一步，"复杂任务开思考"
            // 就只在没有工具的那条路径上成立——而几乎每条路径都有工具。
            .with_thinking(match routing.thinking_field() {
                Thinking::ServerDefault => None,
                other => Some(other),
            });
        if let Some(d) = &self.decider {
            runner = runner.with_decider(&**d);
        }
        // **每一次工具调用都要有人接住。**
        //
        // 不接的话，机器能读使用者的文件、跑命令、抓网页，
        // 而台账里一条记录都没有——那是信任问题，不只是审计问题。
        if let Some(s) = &self.sink {
            runner = runner.with_sink(std::sync::Arc::clone(s));
        }

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

/// 包在客户端外面的两件事：**本地限流等待**与**用量记账**。
///
/// 包成 `Thinker` 而不是散在调用点，是为了让工具循环的每一轮自动享受同样待遇。
/// 放在外面就得每加一个调用点都记得处理一次——而"记得"是靠不住的。
struct MeteredThinker {
    inner: std::sync::Arc<OpenAiThinker>,
    rpm: u32,
    provider: &'static str,
    model: &'static str,
    sink: std::sync::Arc<std::sync::Mutex<Vec<CallRecord>>>,
}

impl Thinker for MeteredThinker {
    fn think(
        &self,
        req: &ThinkRequest,
    ) -> Result<yunxi_bot_core::think::ThinkResponse, ThinkError> {
        let rpm = self.rpm;
        let resp = retry_throttled(
            || self.inner.think(req),
            |wait| {
                eprintln!(
                    "  · 本地限流（{rpm} RPM），等 {} ms 后重发",
                    wait.as_millis()
                );
            },
        )?;

        // 记账：**存原始计数**，金额事后按价格表算
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
        Ok(resp)
    }

    fn model(&self) -> &str {
        self.model
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
        match call() {
            Err(ThinkError::LocalThrottle { wait }) if rounds < MAX_THROTTLE_ROUNDS => {
                let wait = wait.min(MAX_THROTTLE_WAIT);
                if wait.is_zero() {
                    // 等一下也没用，别空转
                    return Err(ThinkError::LocalThrottle { wait });
                }
                on_wait(wait);
                std::thread::sleep(wait);
                rounds += 1;
            }
            other => return other,
        }
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
        let volatile = format!("需要拍板的问题：{}", run.question);
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
        let h = handler();
        let expected = h.expected_chat_prefix().to_string();
        let built = PromptLayout::new(expected.clone());
        assert_eq!(built.fingerprint(), built.fingerprint_for(&expected));
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
        let (_, m) = yunxi_bot_core::think::session::resume_onto(&f, h.expected_chat_prefix());
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
