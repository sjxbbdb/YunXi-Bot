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
    Plan,
    Step,
    Options,
}

impl Intent {
    fn system(self) -> String {
        let base = match self {
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
}

impl ChatHandler {
    /// `persona_text` 与 `rules` 来自配置；它们会被拼进稳定前缀。
    pub fn new(home: PathBuf, persona_name: &str, persona_text: &str, rules: Vec<String>) -> Self {
        Self {
            home,
            thinkers: BTreeMap::new(),
            sessions: BTreeMap::new(),
            persona: build_persona(persona_name, persona_text, &rules),
            tools: ToolRegistry::new(),
            approver: std::sync::Arc::new(std::sync::Mutex::new(RefusingApprover)),
            decider: None,
            // 默认不记录。CLI 一定会用 `with_sink` 覆盖它——
            // 默认值是给"这条链路不该记录"的场景留的显式出口。
            sink: None,
            policy: ToolPolicy::default(),
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
        let key = SessionKey {
            intent,
            provider: routing.spec.provider.to_string(),
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
        if let Ok(mut m) = meter.lock() {
            records.append(&mut m);
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
