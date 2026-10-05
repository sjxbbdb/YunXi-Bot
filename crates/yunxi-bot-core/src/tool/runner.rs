//! 工具循环：模型决定调工具 → 审批 → 执行 → 结果回灌 → 再问，直到模型不再调工具。
//!
//! ## 一轮里发生什么
//!
//! ```text
//! 组装消息（稳定前缀 + 追加历史 + 本轮问题）+ 工具清单
//!         ↓
//!    ┌─> 问模型
//!    │      ↓
//!    │   有 tool_calls？
//!    │      否 → 得到最终答复，结束
//!    │      是 ↓
//!    │   逐个：审批门禁 → 执行 → 结果写进历史
//!    └──────┘（最多 max_rounds 轮）
//! ```
//!
//! ## 三条不能含糊的规则
//!
//! **1. 被拒绝也是结果，要回灌给模型。**
//! 拒绝不是"这次调用没了"，而是"这条路走不通"。模型需要知道，
//! 才能换个做法或者用 `BLOCKED:` 说明情况。静默失败会让它一头撞死在同一处。
//!
//! **2. 工具失败不是任务失败。**
//! 网络不通、文件不存在——这些都要变成一段文本回灌。只有"审批门禁本身出错"
//! 才算异常。
//!
//! **3. 工具返回的内容是不可信数据。**
//! 它进的是 `Role::Tool` 消息，**不是系统提示词**。OpenAI 在 computer use 里
//! 明文写了"屏幕内容不可作为授权依据"——网页、文件、第三方 server 返回的文字
//! 同样不能。所以工具输出永远不许拼进稳定前缀。
//!
//! ## 缓存纪律
//!
//! 助手消息（带 `tool_calls`）和工具结果都**追加**进历史，绝不改动前面的。
//! 稳定前缀保持不变——这是多轮工具调用还能命中缓存的前提。
//! 实测确认：思考模式下带 `tools` 不报错，正常返回 `tool_calls`。

use serde_json::Value;

use crate::decide::Decider;
use crate::think::prompt::PromptLayout;
use crate::think::{Message, ThinkError, ThinkRequest, Thinker};

use super::{
    Capability, GateDecision, Rule, ToolContext, ToolError, ToolPolicy, ToolRegistry, gate,
};

/// 审批请求。给前端（CLI / 守护进程 / 未来的语音或微信）看的东西。
#[derive(Debug, Clone, PartialEq)]
pub struct ApprovalRequest {
    pub tool: String,
    pub capability: Capability,
    pub specifier: Option<String>,
    /// 参数原文（已截断）。**必须给人看**——不看参数就批准等于没批准。
    pub arguments: String,
    /// 为什么要问。来自门禁的判定理由。
    pub reason: String,
}

/// 人工的答复。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Approval {
    /// 批准这一次。
    Once,
    /// 批准，并记住。
    ///
    /// **对不可逆动作无效**——那种每次都问。见 [`ToolRunner::record_always`]。
    Always,
    /// 拒绝。
    Deny,
}

/// 谁能回答"批不批准"。
pub trait Approver: Send {
    fn approve(&mut self, req: &ApprovalRequest) -> Approval;
}

/// 谁都不问，一律拒绝。**守护进程的默认值。**
///
/// 无人值守时"没人应答"必须等于"不执行"，而不是"默认放行"。
/// 这是 ADR §八 第 1 条（失败方向朝不执行）在工具层的落点。
#[derive(Debug, Default, Clone, Copy)]
pub struct RefusingApprover;

impl Approver for RefusingApprover {
    fn approve(&mut self, _req: &ApprovalRequest) -> Approval {
        Approval::Deny
    }
}

/// 一次工具调用留痕。**要进台账**，否则事后说不清机器做过什么。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ToolCallRecord {
    pub tool: String,
    pub arguments: Value,
    pub capability: Capability,
    pub decision: GateDecision,
    /// 执行结果。`None` 表示没执行（被拒绝 / 需人工但没人应答）。
    pub output: Option<Result<String, ToolError>>,
}

impl ToolCallRecord {
    /// 这次调用真的动了系统吗。
    pub fn executed(&self) -> bool {
        matches!(self.output, Some(Ok(_)))
    }
}

/// 工具循环的产出。
#[derive(Debug, Clone, PartialEq)]
pub struct ToolRunOutcome {
    /// 模型最终的文本答复。
    pub text: String,
    pub rounds: u32,
    pub calls: Vec<ToolCallRecord>,
    /// 用掉的模型调用次数（供预算扣减）。
    pub model_calls: u32,
}

#[derive(Debug)]
pub enum ToolLoopError {
    /// 模型调用失败。
    Think(ThinkError),
    /// 轮数用尽还没收敛。
    ///
    /// **这是一个真实的失败形态**，不能当成"模型还没说完"。
    /// 它通常意味着工具一直在返回同样的东西、模型在原地打转。
    RoundsExhausted { rounds: u32 },
}

impl std::fmt::Display for ToolLoopError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ToolLoopError::Think(e) => write!(f, "模型调用失败: {e}"),
            ToolLoopError::RoundsExhausted { rounds } => {
                write!(f, "工具循环跑了 {rounds} 轮仍未结束——模型可能在原地打转")
            }
        }
    }
}

impl std::error::Error for ToolLoopError {}

impl From<ThinkError> for ToolLoopError {
    fn from(e: ThinkError) -> Self {
        ToolLoopError::Think(e)
    }
}

/// 工具循环。
pub struct ToolRunner<'a> {
    registry: &'a ToolRegistry,
    /// 拥有而不是借用：`Approval::Always` 要往白名单里写东西。
    policy: ToolPolicy,
    approver: &'a mut dyn Approver,
    decider: Option<&'a dyn Decider>,
    ctx: ToolContext,
    /// 本轮的思考模式。**每一轮请求都要带上它。**
    ///
    /// 第一版这里没有这个字段，于是工具循环发的请求一个都不带思考模式，
    /// 全部落到服务端默认——**"复杂任务开思考"这个决策在工具路径上被静默丢掉了**。
    /// 对 DeepSeek 恰好默认是开，所以行为看起来对；但记录显示"思考调用 0/6"，
    /// 而如果策略是"关"，它照样会思考，输出 token 翻 2.5 倍且没人知道。
    ///
    /// `None` 表示不指定（不发这个字段），**不是"关"**。
    thinking: Option<crate::think::Thinking>,
    /// 一轮对话最多几次工具往返。
    pub max_rounds: u32,
}

/// 默认轮数上限。
///
/// 8 轮足够"查一下再算一下再写一下"这类复合任务。再多通常意味着打转，
/// 而打转的代价是实打实的 token 和配额。
pub const DEFAULT_MAX_ROUNDS: u32 = 8;

impl<'a> ToolRunner<'a> {
    pub fn new(
        registry: &'a ToolRegistry,
        policy: ToolPolicy,
        approver: &'a mut dyn Approver,
        ctx: ToolContext,
    ) -> Self {
        Self {
            registry,
            policy,
            approver,
            decider: None,
            ctx,
            thinking: None,
            max_rounds: DEFAULT_MAX_ROUNDS,
        }
    }

    /// 设定本轮的思考模式。**路由决定的，每个任务一次。**
    pub fn with_thinking(mut self, t: Option<crate::think::Thinking>) -> Self {
        self.thinking = t;
        self
    }

    pub fn with_decider(mut self, decider: &'a dyn Decider) -> Self {
        self.decider = Some(decider);
        self
    }

    pub fn with_max_rounds(mut self, n: u32) -> Self {
        self.max_rounds = n;
        self
    }

    pub fn policy(&self) -> &ToolPolicy {
        &self.policy
    }

    pub fn ctx_mut(&mut self) -> &mut ToolContext {
        &mut self.ctx
    }

    /// 跑完一次带工具的对话。
    ///
    /// `layout` 由调用方持有——它承载稳定前缀，是缓存命中的前提，
    /// 不能每一轮重建。
    pub fn run(
        &mut self,
        thinker: &dyn Thinker,
        layout: &mut PromptLayout,
        max_tokens: u32,
    ) -> Result<ToolRunOutcome, ToolLoopError> {
        let specs = self.registry.specs();
        let mut calls = Vec::new();

        // 轮数就是模型调用次数——一轮一次，没有别的调用点。
        // 用 `round` 本身当计数，不另开一个变量（那样 clippy 会认为它是
        // 显式循环计数器，而且两者一旦漂移就是账目不对）。
        for round in 1..=self.max_rounds {
            let messages = layout.build();
            let mut req = ThinkRequest::new(messages).with_max_tokens(max_tokens);
            // **每一轮都要带思考模式。** 只在第一轮带会让后续轮次静默退回默认，
            // 而"哪一轮思考了"在账单上分不出来。
            if let Some(t) = self.thinking {
                req = req.with_thinking(t);
            }
            if !specs.is_empty() {
                req = req.with_tools(specs.clone());
            }

            let resp = thinker.think(&req)?;

            let reqs = parse_tool_calls(&resp.tool_calls);
            if reqs.is_empty() {
                // 没有工具调用 → 这一轮就是最终答复。
                // 记进历史，让后续轮次的前缀能续上。
                layout.push_raw(Message::assistant(resp.content.clone()));
                return Ok(ToolRunOutcome {
                    text: resp.content,
                    rounds: round,
                    calls,
                    model_calls: round,
                });
            }

            // 助手请求调工具的那条消息必须**原样**进历史：字段少一个服务端就报 400
            layout.push_raw(Message::assistant_tool_calls(resp.tool_calls.clone()));

            for tr in reqs {
                let record = self.execute_one(&tr);
                // **无论成败都要回灌。** 被拒绝、失败、成功——模型都得知道，
                // 否则它会以为工具没被调用过而重复请求同一个。
                let text = render_tool_result(&record);
                calls.push(record);
                layout.push_raw(Message::tool_result(tr.id.clone(), text));
            }
        }

        Err(ToolLoopError::RoundsExhausted {
            rounds: self.max_rounds,
        })
    }

    /// 执行一次工具调用：解析 → 门禁 → 审批 → 执行。
    fn execute_one(&mut self, tr: &ParsedCall) -> ToolCallRecord {
        let Some(tool) = self.registry.get(&tr.name) else {
            // 模型编了一个不存在的工具。这不是异常，报回去让它改。
            return ToolCallRecord {
                tool: tr.name.clone(),
                arguments: tr.arguments.clone(),
                capability: Capability::ReadOnly,
                decision: GateDecision::Deny {
                    reason: format!("没有这个工具: {}", tr.name),
                },
                output: None,
            };
        };

        let cap = tool.capability();
        let spec = tool.specifier(&tr.arguments, &self.ctx);
        let decision = gate(tool, &tr.arguments, &self.policy, &self.ctx, self.decider);

        match &decision {
            GateDecision::Deny { .. } => ToolCallRecord {
                tool: tr.name.clone(),
                arguments: tr.arguments.clone(),
                capability: cap,
                decision,
                output: None,
            },
            GateDecision::Allow { .. } => {
                let out = tool.call(&tr.arguments, &mut self.ctx);
                ToolCallRecord {
                    tool: tr.name.clone(),
                    arguments: tr.arguments.clone(),
                    capability: cap,
                    decision,
                    output: Some(out.map(|o| o.text)),
                }
            }
            GateDecision::Ask { reason } => {
                // 参数原文要给人看：不看参数就批准等于没批准
                let args_head: String = tr.arguments.to_string().chars().take(800).collect();
                let req = ApprovalRequest {
                    tool: tr.name.clone(),
                    capability: cap,
                    specifier: spec.clone(),
                    arguments: args_head,
                    reason: reason.clone(),
                };
                match self.approver.approve(&req) {
                    Approval::Deny => ToolCallRecord {
                        tool: tr.name.clone(),
                        arguments: tr.arguments.clone(),
                        capability: cap,
                        // **要如实记成"人被拒绝了"，不是"没人可应答"。**
                        // 两者给模型的信号完全不同：前者说明有人在看着、
                        // 这条路被明确否掉了；后者只是当下问不到人。
                        // 混在一起会让模型在"有人拒绝"时还以为可以稍后再试。
                        decision: GateDecision::Deny {
                            reason: format!("人工拒绝：{reason}"),
                        },
                        output: None,
                    },
                    verdict => {
                        if verdict == Approval::Always {
                            self.record_always(&tr.name, spec.as_deref(), cap);
                        }
                        let out = tool.call(&tr.arguments, &mut self.ctx);
                        ToolCallRecord {
                            tool: tr.name.clone(),
                            arguments: tr.arguments.clone(),
                            capability: cap,
                            decision: GateDecision::Allow {
                                reason: format!("人工批准（{reason}）"),
                            },
                            output: Some(out.map(|o| o.text)),
                        }
                    }
                }
            }
        }
    }

    /// 记住"总是允许"。
    ///
    /// **不可逆动作不写进白名单。** 那种动作每次都要人点头——
    /// 一次"总是允许"会把它变成永久自动执行，而"永久自动发邮件"
    /// 不是使用者说"总是允许"时脑子里想的东西。
    fn record_always(&mut self, tool: &str, spec: Option<&str>, cap: Capability) {
        if cap.is_irreversible() {
            return;
        }
        let rule = match spec {
            Some(s) => Rule::scoped(tool, s),
            None => Rule::tool(tool),
        };
        if !self.policy.allow.contains(&rule) {
            self.policy.allow.push(rule);
        }
    }
}

/// 解析出来的工具调用。
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}

/// 从 OpenAI 格式的 `tool_calls` 里解析出要调什么。
///
/// **参数是 JSON 字符串而不是对象**（OpenAI 协议如此），解析失败不能当成
/// "没参数"——那会让工具在一个空参数上跑，做出完全不相干的事。
/// 这里把坏参数原样保留，由工具自己报 `BadArgs`。
pub fn parse_tool_calls(raw: &[Value]) -> Vec<ParsedCall> {
    let mut out = Vec::new();
    for c in raw {
        let id = c
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let Some(f) = c.get("function") else { continue };
        let name = f
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        if name.is_empty() {
            continue;
        }
        let arguments = match f.get("arguments") {
            Some(Value::String(s)) => {
                serde_json::from_str(s).unwrap_or_else(|_| serde_json::json!({ "__unparsable": s }))
            }
            Some(v) => v.clone(),
            None => serde_json::json!({}),
        };
        out.push(ParsedCall {
            id,
            name,
            arguments,
        });
    }
    out
}

/// 把一次调用的结局渲染成回灌给模型的文本。
///
/// **三种结局都要说清楚**，因为模型下一步的动作完全取决于它：
/// 成功 → 用结果；失败 → 换条路；被拒绝 → 别重试，另想办法。
pub fn render_tool_result(rec: &ToolCallRecord) -> String {
    match &rec.output {
        Some(Ok(text)) => text.clone(),
        Some(Err(e)) => format!("工具 {} 执行失败：{e}", rec.tool),
        None => match &rec.decision {
            GateDecision::Deny { reason } => format!(
                "工具 {} 被拒绝，不会执行：{reason}。不要重复请求同一个调用，\
                 请换一种做法，或者用 BLOCKED: 说明你缺什么。",
                rec.tool
            ),
            GateDecision::Ask { reason } => format!(
                "工具 {} 需要人工确认，当前无人可应答（{reason}）。\
                 不要重复请求，请用 BLOCKED: 说明你需要什么才能继续。",
                rec.tool
            ),
            GateDecision::Allow { .. } => {
                format!("工具 {} 未产生输出", rec.tool)
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::SandboxMode;
    use crate::think::{Role, Usage};
    use serde_json::json;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// 剧本化工具：记录被调了几次，可按需失败。
    #[derive(Clone)]
    struct ScriptTool {
        name: String,
        cap: Capability,
        spec: Option<String>,
        fail: bool,
        calls: Arc<AtomicUsize>,
    }

    impl ScriptTool {
        fn new(name: &str, cap: Capability) -> Self {
            Self {
                name: name.into(),
                cap,
                spec: None,
                fail: false,
                calls: Arc::new(AtomicUsize::new(0)),
            }
        }
        fn with_spec(mut self, s: &str) -> Self {
            self.spec = Some(s.into());
            self
        }
        fn failing(mut self) -> Self {
            self.fail = true;
            self
        }
        fn count(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    impl super::super::Tool for ScriptTool {
        fn name(&self) -> &str {
            &self.name
        }
        fn description(&self) -> &str {
            "剧本工具"
        }
        fn parameters(&self) -> Value {
            json!({ "type": "object", "properties": {} })
        }
        fn capability(&self) -> Capability {
            self.cap
        }
        fn specifier(&self, _args: &Value, _ctx: &ToolContext) -> Option<String> {
            self.spec.clone()
        }
        fn call(
            &self,
            _args: &Value,
            _ctx: &mut ToolContext,
        ) -> Result<super::super::ToolOutput, ToolError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.fail {
                return Err(ToolError::Failed {
                    detail: "剧本要求失败".into(),
                });
            }
            Ok(super::super::ToolOutput::read("工具产出"))
        }
    }

    /// 剧本化模型：按顺序返回预设响应。
    struct ScriptThinker {
        replies: std::sync::Mutex<Vec<crate::think::ThinkResponse>>,
        seen_requests: std::sync::Mutex<Vec<ThinkRequest>>,
    }

    impl ScriptThinker {
        fn new(replies: Vec<crate::think::ThinkResponse>) -> Self {
            Self {
                replies: std::sync::Mutex::new(replies),
                seen_requests: std::sync::Mutex::new(Vec::new()),
            }
        }
        fn text_reply(text: &str) -> crate::think::ThinkResponse {
            crate::think::ThinkResponse {
                content: text.into(),
                model: "script".into(),
                usage: Usage::default(),
                finish_reason: Some("stop".into()),
                reasoning: None,
                thinking: crate::think::Thinking::Disabled,
                tool_calls: Vec::new(),
            }
        }
        fn tool_reply(calls: Vec<Value>) -> crate::think::ThinkResponse {
            crate::think::ThinkResponse {
                content: String::new(),
                model: "script".into(),
                usage: Usage::default(),
                finish_reason: Some("tool_calls".into()),
                reasoning: None,
                thinking: crate::think::Thinking::Disabled,
                tool_calls: calls,
            }
        }
        fn requests(&self) -> Vec<ThinkRequest> {
            self.seen_requests.lock().unwrap().clone()
        }
    }

    impl Thinker for ScriptThinker {
        fn think(&self, req: &ThinkRequest) -> Result<crate::think::ThinkResponse, ThinkError> {
            self.seen_requests.lock().unwrap().push(req.clone());
            let mut r = self.replies.lock().unwrap();
            if r.is_empty() {
                return Err(ThinkError::Malformed("剧本用完了".into()));
            }
            Ok(r.remove(0))
        }
        fn model(&self) -> &str {
            "script"
        }
    }

    fn call_json(id: &str, name: &str, args: Value) -> Value {
        json!({
            "id": id,
            "type": "function",
            "function": { "name": name, "arguments": args.to_string() }
        })
    }

    fn ctx() -> ToolContext {
        ToolContext::new(std::env::temp_dir(), SandboxMode::WorkspaceWrite)
    }

    fn runner<'a>(
        reg: &'a ToolRegistry,
        policy: ToolPolicy,
        approver: &'a mut dyn Approver,
    ) -> ToolRunner<'a> {
        ToolRunner::new(reg, policy, approver, ctx())
    }

    // ---- 基本循环 ----

    #[test]
    fn a_reply_without_tool_calls_finishes_immediately() {
        let reg = ToolRegistry::new();
        let mut ap = RefusingApprover;
        let thinker = ScriptThinker::new(vec![ScriptThinker::text_reply("直接回答")]);
        let mut layout = PromptLayout::new("S");
        layout.ask("问题");

        let out = runner(&reg, ToolPolicy::default(), &mut ap)
            .run(&thinker, &mut layout, 512)
            .unwrap();
        assert_eq!(out.text, "直接回答");
        assert_eq!(out.rounds, 1);
        assert!(out.calls.is_empty());
        assert_eq!(out.model_calls, 1);
    }

    #[test]
    fn tool_call_is_executed_and_result_fed_back() {
        let mut reg = ToolRegistry::new();
        let tool = ScriptTool::new("read_file", Capability::ReadOnly);
        let counter = tool.clone();
        reg.register(Arc::new(tool)).unwrap();

        let mut ap = RefusingApprover; // 只读在工作区内免问，用不到它
        let thinker = ScriptThinker::new(vec![
            ScriptThinker::tool_reply(vec![call_json(
                "c1",
                "read_file",
                json!({ "path": "x.md" }),
            )]),
            ScriptThinker::text_reply("读到了"),
        ]);
        let mut layout = PromptLayout::new("S");
        layout.ask("读一下 x.md");

        let mut r = runner(&reg, ToolPolicy::default(), &mut ap);
        r.ctx_mut().note_read(std::env::temp_dir());
        let out = r.run(&thinker, &mut layout, 512).unwrap();

        assert_eq!(out.text, "读到了");
        assert_eq!(out.rounds, 2);
        assert_eq!(out.model_calls, 2);
        assert_eq!(counter.count(), 1, "工具该被真的调用一次");
        assert_eq!(out.calls.len(), 1);
        assert!(out.calls[0].executed());

        // 第二次请求里必须能看到工具结果
        let reqs = thinker.requests();
        assert_eq!(reqs.len(), 2);
        let second = &reqs[1];
        assert!(
            second.messages.iter().any(|m| m.role == Role::Tool),
            "工具结果必须以 tool 角色回灌"
        );
        assert!(
            second.messages.iter().any(|m| !m.tool_calls.is_empty()),
            "助手请求工具的那条消息必须原样回传"
        );
        // 工具清单要被带上（否则模型根本不知道有工具）
        assert!(!second.tools.is_empty());
    }

    #[test]
    fn tool_result_carries_the_call_id() {
        // 少 tool_call_id 服务端报 400，所以这条要有测试盯着
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(ScriptTool::new("t", Capability::ReadOnly)))
            .unwrap();
        let mut ap = RefusingApprover;
        let thinker = ScriptThinker::new(vec![
            ScriptThinker::tool_reply(vec![call_json("call-abc", "t", json!({}))]),
            ScriptThinker::text_reply("ok"),
        ]);
        let mut layout = PromptLayout::new("S");
        layout.ask("q");
        runner(&reg, ToolPolicy::default(), &mut ap)
            .run(&thinker, &mut layout, 512)
            .unwrap();

        let reqs = thinker.requests();
        let tool_msg = reqs[1]
            .messages
            .iter()
            .find(|m| m.role == Role::Tool)
            .expect("应有工具消息");
        assert_eq!(tool_msg.tool_call_id.as_deref(), Some("call-abc"));
    }

    #[test]
    fn multiple_tool_calls_in_one_round_all_run() {
        let mut reg = ToolRegistry::new();
        let a = ScriptTool::new("a", Capability::ReadOnly);
        let b = ScriptTool::new("b", Capability::ReadOnly);
        let (ca, cb) = (a.clone(), b.clone());
        reg.register(Arc::new(a)).unwrap();
        reg.register(Arc::new(b)).unwrap();

        let mut ap = RefusingApprover;
        let thinker = ScriptThinker::new(vec![
            ScriptThinker::tool_reply(vec![
                call_json("c1", "a", json!({})),
                call_json("c2", "b", json!({})),
            ]),
            ScriptThinker::text_reply("都做完了"),
        ]);
        let mut layout = PromptLayout::new("S");
        layout.ask("q");
        let out = runner(&reg, ToolPolicy::default(), &mut ap)
            .run(&thinker, &mut layout, 512)
            .unwrap();
        assert_eq!(out.calls.len(), 2);
        assert_eq!(ca.count(), 1);
        assert_eq!(cb.count(), 1);
    }

    #[test]
    fn rounds_are_bounded() {
        // 模型一直在请求工具 → 必须停下来，不能无限烧配额
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(ScriptTool::new("t", Capability::ReadOnly)))
            .unwrap();
        let mut ap = RefusingApprover;
        let replies: Vec<_> = (0..10)
            .map(|_| ScriptThinker::tool_reply(vec![call_json("c", "t", json!({}))]))
            .collect();
        let thinker = ScriptThinker::new(replies);
        let mut layout = PromptLayout::new("S");
        layout.ask("q");

        let err = runner(&reg, ToolPolicy::default(), &mut ap)
            .with_max_rounds(3)
            .run(&thinker, &mut layout, 512)
            .unwrap_err();
        assert!(
            matches!(err, ToolLoopError::RoundsExhausted { rounds: 3 }),
            "{err:?}"
        );
        assert!(err.to_string().contains("打转"), "{err}");
    }

    // ---- 拒绝与失败都要回灌 ----

    #[test]
    fn a_denied_call_is_reported_to_the_model() {
        // 静默失败会让模型一头撞死在同一处
        let mut reg = ToolRegistry::new();
        let tool = ScriptTool::new("danger", Capability::Write);
        let counter = tool.clone();
        reg.register(Arc::new(tool)).unwrap();

        let mut ap = RefusingApprover;
        let thinker = ScriptThinker::new(vec![
            ScriptThinker::tool_reply(vec![call_json("c1", "danger", json!({}))]),
            ScriptThinker::text_reply("好，我换个办法"),
        ]);
        let mut layout = PromptLayout::new("S");
        layout.ask("q");
        let out = runner(&reg, ToolPolicy::default(), &mut ap)
            .run(&thinker, &mut layout, 512)
            .unwrap();

        assert_eq!(counter.count(), 0, "被拒绝的工具不该被执行");
        assert!(!out.calls[0].executed());
        let reqs = thinker.requests();
        let tool_msg = reqs[1]
            .messages
            .iter()
            .find(|m| m.role == Role::Tool)
            .unwrap();
        assert!(tool_msg.content.contains("被拒绝"), "{}", tool_msg.content);
        assert!(
            tool_msg.content.contains("不要重复请求"),
            "要告诉模型下一步别干什么: {}",
            tool_msg.content
        );
    }

    #[test]
    fn a_failing_tool_does_not_abort_the_loop() {
        // 工具失败是正常结果，不是异常
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(
            ScriptTool::new("net", Capability::ReadOnly).failing(),
        ))
        .unwrap();
        let mut ap = RefusingApprover;
        let thinker = ScriptThinker::new(vec![
            ScriptThinker::tool_reply(vec![call_json("c1", "net", json!({}))]),
            ScriptThinker::text_reply("网络不通，我说不清"),
        ]);
        let mut layout = PromptLayout::new("S");
        layout.ask("q");
        let out = runner(&reg, ToolPolicy::default(), &mut ap)
            .run(&thinker, &mut layout, 512)
            .unwrap();

        assert_eq!(out.text, "网络不通，我说不清");
        assert!(out.calls[0].output.as_ref().unwrap().is_err());
        let reqs = thinker.requests();
        let tool_msg = reqs[1]
            .messages
            .iter()
            .find(|m| m.role == Role::Tool)
            .unwrap();
        assert!(
            tool_msg.content.contains("执行失败"),
            "{}",
            tool_msg.content
        );
    }

    #[test]
    fn an_unknown_tool_name_is_reported_not_fatal() {
        // 模型可能会编一个不存在的工具名
        let reg = ToolRegistry::new();
        let mut ap = RefusingApprover;
        let thinker = ScriptThinker::new(vec![
            ScriptThinker::tool_reply(vec![call_json("c1", "no_such_tool", json!({}))]),
            ScriptThinker::text_reply("我记错了工具名"),
        ]);
        let mut layout = PromptLayout::new("S");
        layout.ask("q");
        let out = runner(&reg, ToolPolicy::default(), &mut ap)
            .run(&thinker, &mut layout, 512)
            .unwrap();
        assert_eq!(out.text, "我记错了工具名");
        let reqs = thinker.requests();
        let tool_msg = reqs[1]
            .messages
            .iter()
            .find(|m| m.role == Role::Tool)
            .unwrap();
        assert!(
            tool_msg.content.contains("没有这个工具"),
            "{}",
            tool_msg.content
        );
    }

    // ---- 审批交互 ----

    #[test]
    fn approval_once_executes_without_remembering() {
        struct Yes;
        impl Approver for Yes {
            fn approve(&mut self, _r: &ApprovalRequest) -> Approval {
                Approval::Once
            }
        }
        let mut reg = ToolRegistry::new();
        let tool = ScriptTool::new("w", Capability::Write);
        let counter = tool.clone();
        reg.register(Arc::new(tool)).unwrap();
        let mut ap = Yes;
        let thinker = ScriptThinker::new(vec![
            ScriptThinker::tool_reply(vec![call_json("c1", "w", json!({}))]),
            ScriptThinker::text_reply("写好了"),
        ]);
        let mut layout = PromptLayout::new("S");
        layout.ask("q");
        let mut r = runner(&reg, ToolPolicy::default(), &mut ap);
        r.run(&thinker, &mut layout, 512).unwrap();
        assert_eq!(counter.count(), 1);
        assert!(r.policy().allow.is_empty(), "Once 不该写进白名单");
    }

    #[test]
    fn approval_always_writes_a_scoped_rule() {
        struct Always;
        impl Approver for Always {
            fn approve(&mut self, _r: &ApprovalRequest) -> Approval {
                Approval::Always
            }
        }
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(
            ScriptTool::new("w", Capability::Write).with_spec("D:\\notes"),
        ))
        .unwrap();
        let mut ap = Always;
        let thinker = ScriptThinker::new(vec![
            ScriptThinker::tool_reply(vec![call_json("c1", "w", json!({}))]),
            ScriptThinker::text_reply("ok"),
        ]);
        let mut layout = PromptLayout::new("S");
        layout.ask("q");
        let mut r = runner(&reg, ToolPolicy::default(), &mut ap);
        r.run(&thinker, &mut layout, 512).unwrap();

        let rules = &r.policy().allow;
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].tool, "w");
        assert_eq!(rules[0].specifier.as_deref(), Some("D:\\notes"));
    }

    /// **关键性质：不可逆动作不会被"总是允许"变成永久自动。**
    ///
    /// 一次"总是允许"会把它变成永久自动执行，而"永久自动发邮件"
    /// 不是使用者说"总是允许"时脑子里想的东西。
    #[test]
    fn always_never_remembers_an_irreversible_action() {
        struct Always;
        impl Approver for Always {
            fn approve(&mut self, _r: &ApprovalRequest) -> Approval {
                Approval::Always
            }
        }
        let mut reg = ToolRegistry::new();
        let tool = ScriptTool::new("send_mail", Capability::Outbound);
        let counter = tool.clone();
        reg.register(Arc::new(tool)).unwrap();
        let mut ap = Always;
        let thinker = ScriptThinker::new(vec![
            ScriptThinker::tool_reply(vec![call_json("c1", "send_mail", json!({}))]),
            ScriptThinker::text_reply("发出去了"),
        ]);
        let mut layout = PromptLayout::new("S");
        layout.ask("q");
        let mut r = runner(&reg, ToolPolicy::default(), &mut ap);
        r.run(&thinker, &mut layout, 512).unwrap();

        assert_eq!(counter.count(), 1, "这一次要执行");
        assert!(
            r.policy().allow.is_empty(),
            "不可逆动作不该被写进白名单——下次还得问"
        );
    }

    #[test]
    fn approval_request_shows_the_arguments() {
        // 不看参数就批准等于没批准
        struct Capture(std::sync::Arc<std::sync::Mutex<Vec<ApprovalRequest>>>);
        impl Approver for Capture {
            fn approve(&mut self, r: &ApprovalRequest) -> Approval {
                self.0.lock().unwrap().push(r.clone());
                Approval::Deny
            }
        }
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(ScriptTool::new("w", Capability::Write)))
            .unwrap();
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut ap = Capture(seen.clone());
        let thinker = ScriptThinker::new(vec![
            ScriptThinker::tool_reply(vec![call_json("c1", "w", json!({ "path": "secret.txt" }))]),
            ScriptThinker::text_reply("ok"),
        ]);
        let mut layout = PromptLayout::new("S");
        layout.ask("q");
        runner(&reg, ToolPolicy::default(), &mut ap)
            .run(&thinker, &mut layout, 512)
            .unwrap();

        let reqs = seen.lock().unwrap();
        assert_eq!(reqs.len(), 1);
        assert!(
            reqs[0].arguments.contains("secret.txt"),
            "审批请求必须带上参数: {}",
            reqs[0].arguments
        );
        assert!(!reqs[0].reason.is_empty(), "必须说明为什么要问");
    }

    #[test]
    fn refusing_approver_denies_everything() {
        // 守护进程的默认值：没人应答 = 不执行
        let mut reg = ToolRegistry::new();
        let tool = ScriptTool::new("w", Capability::Write);
        let counter = tool.clone();
        reg.register(Arc::new(tool)).unwrap();
        let mut ap = RefusingApprover;
        let thinker = ScriptThinker::new(vec![
            ScriptThinker::tool_reply(vec![call_json("c1", "w", json!({}))]),
            ScriptThinker::text_reply("ok"),
        ]);
        let mut layout = PromptLayout::new("S");
        layout.ask("q");
        runner(&reg, ToolPolicy::default(), &mut ap)
            .run(&thinker, &mut layout, 512)
            .unwrap();
        assert_eq!(counter.count(), 0);
    }

    #[test]
    fn thinking_mode_is_forwarded_to_every_round() {
        // **回归：工具循环曾经把思考模式整个丢掉。**
        //
        // 表现很隐蔽：对 DeepSeek 服务端默认就是开，所以行为"看起来对"，
        // 但台账记的是"思考调用 0 次"；而如果策略是"关"，它照样会思考，
        // 输出 token 翻 2.5 倍而且没人知道。
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(ScriptTool::new("t", Capability::ReadOnly)))
            .unwrap();
        let mut ap = RefusingApprover;
        let thinker = ScriptThinker::new(vec![
            ScriptThinker::tool_reply(vec![call_json("c1", "t", json!({}))]),
            ScriptThinker::tool_reply(vec![call_json("c2", "t", json!({}))]),
            ScriptThinker::text_reply("done"),
        ]);
        let mut layout = PromptLayout::new("S");
        layout.ask("q");
        runner(&reg, ToolPolicy::default(), &mut ap)
            .with_thinking(Some(crate::think::Thinking::Enabled))
            .run(&thinker, &mut layout, 512)
            .unwrap();

        for (i, r) in thinker.requests().iter().enumerate() {
            assert_eq!(
                r.thinking,
                Some(crate::think::Thinking::Enabled),
                "第 {} 轮丢了思考模式",
                i + 1
            );
        }
    }

    #[test]
    fn disabled_thinking_is_also_forwarded() {
        // 另一半同样重要：决定"关"的时候不能被默认值悄悄打开
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(ScriptTool::new("t", Capability::ReadOnly)))
            .unwrap();
        let mut ap = RefusingApprover;
        let thinker = ScriptThinker::new(vec![ScriptThinker::text_reply("done")]);
        let mut layout = PromptLayout::new("S");
        layout.ask("q");
        runner(&reg, ToolPolicy::default(), &mut ap)
            .with_thinking(Some(crate::think::Thinking::Disabled))
            .run(&thinker, &mut layout, 512)
            .unwrap();
        assert_eq!(
            thinker.requests()[0].thinking,
            Some(crate::think::Thinking::Disabled)
        );
    }

    #[test]
    fn no_thinking_setting_sends_no_field() {
        // `None` 是"不指定"，不是"关"。Agnes 不吃这个字段，给它塞就是错的。
        let reg = ToolRegistry::new();
        let mut ap = RefusingApprover;
        let thinker = ScriptThinker::new(vec![ScriptThinker::text_reply("done")]);
        let mut layout = PromptLayout::new("S");
        layout.ask("q");
        runner(&reg, ToolPolicy::default(), &mut ap)
            .run(&thinker, &mut layout, 512)
            .unwrap();
        assert_eq!(thinker.requests()[0].thinking, None);
    }

    // ---- 缓存纪律 ----

    #[test]
    fn stable_prefix_survives_tool_rounds() {
        // 多轮工具调用还能命中缓存的前提：稳定前缀一次都不许变
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(ScriptTool::new("t", Capability::ReadOnly)))
            .unwrap();
        let mut ap = RefusingApprover;
        let thinker = ScriptThinker::new(vec![
            ScriptThinker::tool_reply(vec![call_json("c1", "t", json!({}))]),
            ScriptThinker::tool_reply(vec![call_json("c2", "t", json!({}))]),
            ScriptThinker::text_reply("done"),
        ]);
        let mut layout = PromptLayout::new("稳定前缀");
        layout.ask("q");
        let before = layout.fingerprint();

        runner(&reg, ToolPolicy::default(), &mut ap)
            .run(&thinker, &mut layout, 512)
            .unwrap();

        assert_eq!(layout.fingerprint(), before, "稳定前缀被改动了，缓存会全废");
        // 每次请求的 system 消息必须逐字节相同
        let reqs = thinker.requests();
        let systems: Vec<&str> = reqs
            .iter()
            .map(|r| r.messages[0].content.as_str())
            .collect();
        for s in &systems {
            assert_eq!(*s, systems[0], "system 消息必须逐字节一致");
        }
    }

    #[test]
    fn history_is_append_only_across_rounds() {
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(ScriptTool::new("t", Capability::ReadOnly)))
            .unwrap();
        let mut ap = RefusingApprover;
        let thinker = ScriptThinker::new(vec![
            ScriptThinker::tool_reply(vec![call_json("c1", "t", json!({}))]),
            ScriptThinker::tool_reply(vec![call_json("c2", "t", json!({}))]),
            ScriptThinker::text_reply("done"),
        ]);
        let mut layout = PromptLayout::new("S");
        layout.ask("q");
        runner(&reg, ToolPolicy::default(), &mut ap)
            .run(&thinker, &mut layout, 512)
            .unwrap();

        let reqs = thinker.requests();
        assert_eq!(reqs.len(), 3);
        // 后一轮的前 k 条必须与上一轮逐字节相同（k = 上一轮长度）
        for w in reqs.windows(2) {
            let (prev, next) = (&w[0], &w[1]);
            for i in 0..prev.messages.len() {
                assert_eq!(
                    prev.messages[i].content, next.messages[i].content,
                    "第 {i} 条被改写了，缓存会失效"
                );
            }
            assert!(next.messages.len() >= prev.messages.len(), "历史只能追加");
        }
    }

    #[test]
    fn tool_output_never_enters_the_stable_prefix() {
        // 工具返回是不可信数据，绝不许拼进系统提示词
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(ScriptTool::new("t", Capability::ReadOnly)))
            .unwrap();
        let mut ap = RefusingApprover;
        let thinker = ScriptThinker::new(vec![
            ScriptThinker::tool_reply(vec![call_json("c1", "t", json!({}))]),
            ScriptThinker::text_reply("done"),
        ]);
        let mut layout = PromptLayout::new("S");
        layout.ask("q");
        runner(&reg, ToolPolicy::default(), &mut ap)
            .run(&thinker, &mut layout, 512)
            .unwrap();

        for r in thinker.requests() {
            assert_eq!(r.messages[0].role, Role::System);
            assert!(
                !r.messages[0].content.contains("工具产出"),
                "工具输出漏进系统提示词了"
            );
        }
    }

    #[test]
    fn volatile_question_is_not_duplicated_after_a_tool_round() {
        // 同一个问题既留在易变段又进历史，会让下一轮前缀对不上
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(ScriptTool::new("t", Capability::ReadOnly)))
            .unwrap();
        let mut ap = RefusingApprover;
        let thinker = ScriptThinker::new(vec![
            ScriptThinker::tool_reply(vec![call_json("c1", "t", json!({}))]),
            ScriptThinker::text_reply("done"),
        ]);
        let mut layout = PromptLayout::new("S");
        layout.ask("唯一的问题");
        runner(&reg, ToolPolicy::default(), &mut ap)
            .run(&thinker, &mut layout, 512)
            .unwrap();

        let reqs = thinker.requests();
        let last = &reqs[reqs.len() - 1];
        let n = last
            .messages
            .iter()
            .filter(|m| m.content == "唯一的问题")
            .count();
        assert_eq!(n, 1, "问题出现了 {n} 次，应该只有一次");
    }

    // ---- 解析 ----

    #[test]
    fn parses_openai_tool_call_shape() {
        let raw = vec![call_json("id-1", "read_file", json!({ "path": "a.md" }))];
        let p = parse_tool_calls(&raw);
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].id, "id-1");
        assert_eq!(p[0].name, "read_file");
        assert_eq!(p[0].arguments["path"], "a.md");
    }

    #[test]
    fn unparsable_arguments_are_preserved_not_dropped() {
        // 当成"没参数"会让工具在空参数上跑，做出完全不相干的事
        let raw = vec![json!({
            "id": "c1",
            "function": { "name": "t", "arguments": "{坏掉的 json" }
        })];
        let p = parse_tool_calls(&raw);
        assert_eq!(p.len(), 1);
        assert!(
            p[0].arguments.get("__unparsable").is_some(),
            "坏参数要原样保留: {:?}",
            p[0].arguments
        );
    }

    #[test]
    fn calls_without_a_name_are_skipped() {
        let raw = vec![
            json!({ "id": "c1", "function": { "arguments": "{}" } }),
            json!({ "id": "c2" }),
        ];
        assert!(parse_tool_calls(&raw).is_empty());
    }

    #[test]
    fn missing_arguments_default_to_empty_object() {
        let raw = vec![json!({ "id": "c1", "function": { "name": "t" } })];
        let p = parse_tool_calls(&raw);
        assert_eq!(p[0].arguments, json!({}));
    }

    // ---- 渲染 ----

    #[test]
    fn render_distinguishes_denied_from_ask_from_failed() {
        // 三种结局下模型下一步的动作完全不同，文本必须能区分
        let base = |decision, output| ToolCallRecord {
            tool: "t".into(),
            arguments: json!({}),
            capability: Capability::Write,
            decision,
            output,
        };
        let denied = render_tool_result(&base(
            GateDecision::Deny {
                reason: "黑名单".into(),
            },
            None,
        ));
        let asked = render_tool_result(&base(
            GateDecision::Ask {
                reason: "要问人".into(),
            },
            None,
        ));
        let failed = render_tool_result(&base(
            GateDecision::Allow {
                reason: "ok".into(),
            },
            Some(Err(ToolError::Failed {
                detail: "超时".into(),
            })),
        ));
        assert!(denied.contains("被拒绝"));
        assert!(asked.contains("无人可应答"));
        assert!(failed.contains("执行失败"));
        assert_ne!(denied, asked);
        assert_ne!(asked, failed);
    }

    #[test]
    fn render_passes_successful_output_through_unchanged() {
        let rec = ToolCallRecord {
            tool: "t".into(),
            arguments: json!({}),
            capability: Capability::ReadOnly,
            decision: GateDecision::Allow {
                reason: "ok".into(),
            },
            output: Some(Ok("原始产出".into())),
        };
        assert_eq!(render_tool_result(&rec), "原始产出");
    }

    #[test]
    fn records_are_ledger_ready() {
        // 每次调用都要能写进台账，否则说不清机器做过什么
        let rec = ToolCallRecord {
            tool: "read_file".into(),
            arguments: json!({ "path": "a.md" }),
            capability: Capability::ReadOnly,
            decision: GateDecision::Allow {
                reason: "只读且在工作区内".into(),
            },
            output: Some(Ok("内容".into())),
        };
        let v = serde_json::to_value(&rec).unwrap();
        assert_eq!(v["tool"], "read_file");
        assert_eq!(v["capability"], "read_only");
        assert!(rec.executed());
    }
}
