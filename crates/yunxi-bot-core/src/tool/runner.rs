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

use std::collections::BTreeSet;
use std::path::PathBuf;

use super::{
    Capability, GateDecision, Rule, Tool, ToolContext, ToolError, ToolOutput, ToolPolicy,
    ToolRegistry, gate,
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
    /// **这次调用会造成什么后果。** 见 [`Tool::preview`]。
    ///
    /// `None` 表示这个工具没提供预览（读取类工具本来就没有"后果"可言）。
    /// 那时审批框只能显示参数——而那是**参数，不是后果**，
    /// 所以"没有预览"本身就是一件该让人知道的事。
    pub preview: Option<String>,
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
    /// 开始执行的时刻（Unix 毫秒）。没执行时为 0。
    ///
    /// **加这个是因为"并行"需要可证。** 本地文件读得太快，
    /// 光看总耗时看不出并行的痕迹；而两个调用的
    /// `[started, started+duration]` 区间重叠，就是并行最直接的证据。
    ///
    /// 顺带的好处：能看到哪个工具慢。
    #[serde(default)]
    pub started_at_ms: u64,
    /// 执行耗时（毫秒）。没执行时为 0。
    #[serde(default)]
    pub duration_ms: u64,
}

impl ToolCallRecord {
    /// 这次调用真的动了系统吗。
    pub fn executed(&self) -> bool {
        matches!(self.output, Some(Ok(_)))
    }
}

/// 工具调用的去处。**每一次调用都要有人接住。**
///
/// ## 为什么是一个 trait 而不是直接给 ledger
///
/// 工具层不该依赖台账的类型——它只需要"有地方记"这件事。
/// 这样测试可以塞一个内存记录器，而 CLI 塞一个写台账的实现。
///
/// ## 为什么 `&self` 而不是 `&mut self`
///
/// 因为工具循环同时持有很多可变借用（会话、注册表）。要求 `&mut` 会让
/// sink 和它们打架。实现方自己解决内部可变性（CLI 用 `Mutex<Ledger>`）。
///
/// ## 记录失败怎么办
///
/// **吞掉，但不静默。** 签名不给 `Result`：一次记录失败不该让工具循环
/// 中断（那会为了留痕而拒绝干活）。实现方应该把失败打到 stderr——
/// 台账写不进去是需要人知道的事，但它不是工具调用失败。
pub trait ToolCallSink: Send + Sync {
    fn record(&self, call: &ToolCallRecord);
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
    /// **模型在反复做同一个动作。**
    ///
    /// 真机上抓到过（D100）：一次运行调了 40 次工具、最后 12 次里 11 次是
    /// `write_file`，把 19 行的文件撑到 **49 行**，直到撞上 8 轮上限才停。
    ///
    /// ## 为什么单独一个变体，而不是并进 `RoundsExhausted`
    ///
    /// **两者给模型的信息完全不同**：
    ///
    /// - "轮数用完了"——它对这个毫无办法，因为它不知道自己做错了什么
    /// - "**你在重复同一个动作**"——它至少能换个做法
    ///
    /// 而更实际的是：**这种事本来就不该磨到轮数上限**。
    /// 磨到上限意味着白花了 5 轮的钱和时间。
    Repeating { tool: String, times: u32 },
}

impl std::fmt::Display for ToolLoopError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ToolLoopError::Think(e) => write!(f, "模型调用失败: {e}"),
            ToolLoopError::RoundsExhausted { rounds } => {
                write!(f, "工具循环跑了 {rounds} 轮仍未结束——模型可能在原地打转")
            }
            ToolLoopError::Repeating { tool, times } => write!(
                f,
                "连续 {times} 次调用都是同一个动作（{tool}），参数也一样——\
                 这是在原地打转，不是在做新的事。换个做法，或者说明为什么做不下去。"
            ),
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
    /// 工具调用的去处。`None` 表示不记录——**测试与纯计算场景的显式选择**。
    sink: Option<std::sync::Arc<dyn ToolCallSink>>,
    /// 正文增量的去处。`None` 表示不流式（一次性拿到完整正文）。
    ///
    /// **它只收正文，不收工具调用。** 工具调用的增量是参数 JSON 的碎片，
    /// 打出来是噪声，而且它们本来就不是给人读的。
    on_delta: Option<DeltaSink>,
    /// 一轮对话最多几次工具往返。
    pub max_rounds: u32,
}

/// 正文增量的去处。
///
/// 抽成别名有两个理由：一是这个类型写在签名里太长了，
/// 二是**"要把增量送到哪"这件事值得有个名字**——
/// 它和 `ToolCallSink`（工具调用的去处）是同一层的东西。
pub type DeltaSink = std::sync::Arc<dyn Fn(&str) + Send + Sync>;

/// 一次工具调用的**判定结果**。
///
/// ## 为什么要把判定和执行拆开
///
/// 判定阶段（门禁 + 问人）**必须串行**：
///
/// - 审批者是一个 `&mut`，同时问它没有意义
/// - 更重要的是那是人的现实——**同时弹三个审批框，使用者根本不知道
///   自己在批哪一条**
///
/// 执行阶段才可以并行，而且只有只读的可以（见 [`Decided::parallelizable`]）。
enum Decided<'a> {
    /// 已经有结论，不用执行（工具不存在、被拒绝、没人应答）。
    Settled(ToolCallRecord),
    /// 批准了，待执行。
    Run {
        tool: &'a dyn Tool,
        call: ParsedCall,
        capability: Capability,
        reason: String,
    },
}

impl Decided<'_> {
    /// 这个调用能不能和旁边的调用**同时**跑。
    ///
    /// ## 只并行只读，理由有三条
    ///
    /// 1. **并行写同一个文件是灾难。** 两个 `write_file` 同时改一个文件，
    ///    结果取决于调度——而那种不确定性事后完全无法复现。
    /// 2. **有副作用的调用之间有隐式依赖。** 模型可能在一次里既建目录又写文件，
    ///    并行就会让写文件先跑而失败。
    /// 3. **`Network` 也不并行**：搜索和抓取有各自的限流，并发打过去只会
    ///    更快撞上配额——这和任务层"限流器是共享状态"是同一条理由。
    ///
    /// 只读没有这些问题：它们不改变任何状态，跑多少次、什么顺序都不影响结果。
    fn parallelizable(&self) -> bool {
        matches!(
            self,
            Decided::Run {
                capability: Capability::ReadOnly,
                ..
            }
        )
    }
}

/// 默认轮数上限。
///
/// 8 轮足够"查一下再算一下再写一下"这类复合任务。再多通常意味着打转，
/// 而打转的代价是实打实的 token 和配额。
/// 连续多少次一模一样的调用就判定为"原地打转"。
///
/// 定 3 是权衡：
/// - **2 次太紧**——正常流程里"读一遍、改一下、再读一遍确认"是合理的，
///   而"读同一个文件两次"完全可能是对的
/// - **4 次太松**——真机上那次是 11 次连写同一个文件，
///   等到第 4 次才拦也已经白花了三轮
///
/// 3 次的意思是：**同一个工具、同样的参数、连着来三遍**。
/// 那不像是在做事。
pub const REPEAT_LIMIT: usize = 3;

/// 末尾是不是连着 N 次一模一样的调用。
///
/// **比的是"工具名 + 参数"的完整文本**，不是模糊相似——
/// 模糊相似会把"读 a.rs、读 b.rs、读 c.rs"这种正常的批量读判成重复。
/// 要拦的是**字面上一模一样**那种。
///
/// 返回 `(工具名, 连续次数)`。
fn detect_repeat(seen: &[(String, String)], limit: usize) -> Option<(String, u32)> {
    if seen.len() < limit {
        return None;
    }
    let tail = &seen[seen.len() - limit..];
    let first = &tail[0];
    if tail.iter().all(|c| c == first) {
        // 把**真正连续**的次数报出来（可能不止 limit 次）
        let mut n = 0usize;
        for c in seen.iter().rev() {
            if c == first {
                n += 1;
            } else {
                break;
            }
        }
        return Some((first.0.clone(), n as u32));
    }
    None
}

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
            sink: None,
            on_delta: None,
            max_rounds: DEFAULT_MAX_ROUNDS,
        }
    }

    /// 设定正文增量的去处。
    ///
    /// **不设就是不流式**——行为和加这个功能之前完全一致
    /// （一次性拿到完整正文）。常驻循环、测试、无人值守场景
    /// 都不需要流式，也不该为它付代价。
    pub fn with_on_delta(mut self, f: DeltaSink) -> Self {
        self.on_delta = Some(f);
        self
    }

    /// 设定工具调用的去处。
    ///
    /// **不设就是不记录**——那必须是一个显式的选择，而不是默认行为。
    /// 默认静默会让"这次为什么没有工具记录"变成一个没有答案的问题。
    pub fn with_sink(mut self, sink: std::sync::Arc<dyn ToolCallSink>) -> Self {
        self.sink = Some(sink);
        self
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
        // 这个运行里请求过的每一次工具调用（工具名 + 参数）。
        // 用来发现"原地打转"——见 `detect_repeat`。
        let mut seen: Vec<(String, String)> = Vec::new();

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

            // **走流式入口。** 不支持的 Thinker 会退化成一次性返回
            // （`Thinker::think_stream` 的默认实现），所以这里不需要判断。
            let resp = {
                let cb = self.on_delta.clone();
                let mut emit = |t: &str| {
                    if let Some(f) = &cb {
                        f(t);
                    }
                };
                thinker.think_stream(&req, &mut emit)?
            };

            let reqs = parse_tool_calls(&resp.tool_calls);

            // **先看它是不是在重复，再去执行。**
            //
            // 放在执行**之前**：重复的那些调用没必要再跑一遍——
            // 跑了也只是再拿一次同样的结果，然后下一轮再来。
            for tr in &reqs {
                seen.push((tr.name.clone(), tr.arguments.to_string()));
            }
            if let Some((tool, times)) = detect_repeat(&seen, REPEAT_LIMIT) {
                return Err(ToolLoopError::Repeating { tool, times });
            }
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

            // 助手请求调工具的那条消息必须**原样**进历史：字段少一个服务端就报 400。
            //
            // **但"原样"有个前提：它得是合法的。** 真机上抓到的 400
            // 完整报文是：
            //
            // ```text
            // Assistant tool call <id>.arguments must be valid JSON.
            // ```
            //
            // 根因：**模型产出的 `arguments` 不是合法 JSON**——最常见的
            // 是被输出预算截断在参数中间（`{"path": "src/bill`）。
            // 而我们把它**原样**塞进历史，下一轮服务端校验历史时
            // **整条请求被拒**。一条坏的工具调用，废掉后面所有的回合。
            //
            // **这和本项目反复踩的"截断"是同一个根**：被截断的东西
            // 不能当成完整的用，也不能原样传下去。
            layout.push_raw(Message::assistant_tool_calls(sanitize_tool_calls(
                &resp.tool_calls,
            )));

            // **先全部判定（串行），再批量执行（只读的并行）。**
            //
            // 判定必须串行：审批是 `&mut`，而且同时弹三个审批框
            // 使用者根本不知道自己在批哪一条。
            let decided: Vec<Decided<'a>> = reqs.iter().map(|tr| self.decide_one(tr)).collect();
            let records = self.execute_batch(decided);

            for (tr, record) in reqs.into_iter().zip(records) {
                // **每一次调用都交给 sink。**
                //
                // 放在回灌模型之前、放进 `calls` 之前——因为那两步之后
                // 还有可能出错（回灌本身失败、整轮被放弃），而
                // **已经发生过的系统动作不能因为后续步骤失败就不留痕**。
                //
                // 这也是"工具调用不落台账"那个缺口的修正点：在此之前
                // `ToolRunOutcome.calls` 生产出来就被丢掉，于是机器能读
                // 使用者的文件、跑命令、抓网页，而台账里一条记录都没有
                // ——破了 ADR D11「台账是唯一事实来源」。
                if let Some(sink) = self.sink.as_ref() {
                    sink.record(&record);
                }
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

    /// 判定一次调用：解析 → 门禁 → 审批。**不执行。**
    ///
    /// ## 为什么判定和执行要分开
    ///
    /// 判定阶段（门禁 + 问人）**必须串行**：
    ///
    /// - 审批者是一个 `&mut`，同时问它没有意义
    /// - 更重要的是那是人的现实——**同时弹三个审批框，使用者根本不知道
    ///   自己在批哪一条**
    ///
    /// 执行阶段才可以并行，而且只有只读的可以（见 [`Decided::parallelizable`]）。
    fn decide_one(&mut self, tr: &ParsedCall) -> Decided<'a> {
        let Some(tool) = self.registry.get(&tr.name) else {
            // 模型编了一个不存在的工具。这不是异常，报回去让它改。
            //
            // **空名字单独说**：那不是"模型编了个工具名"，而是**响应本身不完整**
            // （分片没拼全，或者服务端给了个畸形的 tool_call）。
            // 两种情况给模型的信号不一样——前者让它换个工具名，
            // 后者让它重说一遍。
            let reason = if tr.name.is_empty() {
                "这条工具调用没带工具名（响应不完整），无法执行".to_string()
            } else {
                format!("没有这个工具: {}", tr.name)
            };
            return Decided::Settled(ToolCallRecord {
                tool: tr.name.clone(),
                arguments: tr.arguments.clone(),
                capability: Capability::ReadOnly,
                decision: GateDecision::Deny { reason },
                output: None,
                started_at_ms: 0,
                duration_ms: 0,
            });
        };

        let cap = tool.capability();
        let spec = tool.specifier(&tr.arguments, &self.ctx);
        let decision = gate(tool, &tr.arguments, &self.policy, &self.ctx, self.decider);

        match decision {
            GateDecision::Deny { .. } => Decided::Settled(ToolCallRecord {
                tool: tr.name.clone(),
                arguments: tr.arguments.clone(),
                capability: cap,
                decision,
                output: None,
                started_at_ms: 0,
                duration_ms: 0,
            }),
            GateDecision::Allow { reason } => Decided::Run {
                tool,
                call: tr.clone(),
                capability: cap,
                reason,
            },
            GateDecision::Ask { reason } => {
                // 参数原文要给人看：不看参数就批准等于没批准
                let args_head: String = tr.arguments.to_string().chars().take(800).collect();
                let req = ApprovalRequest {
                    tool: tr.name.clone(),
                    capability: cap,
                    specifier: spec.clone(),
                    arguments: args_head,
                    reason: reason.clone(),
                    // **在真正执行之前算，而且必须只读。**
                    // 算不出来（读不了、参数不全）就给 `None`——
                    // 那时审批框会如实说"没提供预览"，
                    // 而不是让人以为参数 JSON 就是后果。
                    preview: tool.preview(&tr.arguments, &self.ctx),
                };
                match self.approver.approve(&req) {
                    Approval::Deny => Decided::Settled(ToolCallRecord {
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
                        started_at_ms: 0,
                        duration_ms: 0,
                    }),
                    verdict => {
                        if verdict == Approval::Always {
                            self.record_always(&tr.name, spec.as_deref(), cap);
                        }
                        Decided::Run {
                            tool,
                            call: tr.clone(),
                            capability: cap,
                            reason: format!("人工批准（{reason}）"),
                        }
                    }
                }
            }
        }
    }

    /// 执行一次已经判定过的调用（串行路径）。
    fn finish_one(&mut self, d: Decided<'a>) -> ToolCallRecord {
        match d {
            Decided::Settled(r) => r,
            Decided::Run {
                tool,
                call,
                capability,
                reason,
            } => {
                let started = crate::now_millis().unwrap_or(0);
                let out = tool.call(&call.arguments, &mut self.ctx);
                let duration = crate::now_millis()
                    .unwrap_or(started)
                    .saturating_sub(started);
                ToolCallRecord {
                    tool: call.name.clone(),
                    arguments: call.arguments.clone(),
                    capability,
                    decision: GateDecision::Allow { reason },
                    output: Some(out.map(|o| o.text)),
                    started_at_ms: started,
                    duration_ms: duration,
                }
            }
        }
    }

    /// 按批执行：**连续的只读调用并行，其余串行。**
    ///
    /// 顺序不能乱——工具结果要按模型请求的顺序回灌，否则它会以为
    /// 结果和调用对不上。
    fn execute_batch(&mut self, decided: Vec<Decided<'a>>) -> Vec<ToolCallRecord> {
        let mut out = Vec::with_capacity(decided.len());
        let mut iter = decided.into_iter().peekable();
        while let Some(d) = iter.next() {
            if !d.parallelizable() {
                out.push(self.finish_one(d));
                continue;
            }
            // 起一段连续的只读调用
            let mut batch = vec![d];
            while iter.peek().is_some_and(Decided::parallelizable) {
                batch.push(iter.next().expect("peek 说过有"));
            }
            if batch.len() >= 2 {
                out.extend(self.run_parallel(batch));
            } else {
                // 只有一个——并行没有收益，还要多付线程开销
                for one in batch {
                    out.push(self.finish_one(one));
                }
            }
        }
        out
    }

    /// 一段只读调用并行跑。
    ///
    /// 用 `std::thread::scope` 而不是 spawn 出去 join：
    /// **作用域线程能借用栈上的东西**，而 spawn 要求 `'static`——
    /// 那会逼着把工具和参数都拷一遍。
    fn run_parallel(&mut self, batch: Vec<Decided<'a>>) -> Vec<ToolCallRecord> {
        let base = self.ctx.clone();
        type ParallelResult = (Result<ToolOutput, ToolError>, BTreeSet<PathBuf>, u64, u64);
        let results: Vec<ParallelResult> = std::thread::scope(|s| {
            let handles: Vec<_> = batch
                .iter()
                .map(|d| {
                    let Decided::Run { tool, call, .. } = d else {
                        // `parallelizable` 保证了这里只可能是 Run
                        unreachable!("只读批次里不该有已定案项")
                    };
                    // **每个线程一份自己的 ctx。** 共享一个 `&mut` 就没法并行了；
                    // 跑完把 `read_files` 并回来，"先读后写"仍然成立。
                    let mut ctx = base.clone();
                    s.spawn(move || {
                        // **在每个线程里各量各的开始时刻。**
                        // 在外面量只能得到一个总区间，而"谁和谁重叠"
                        // 正是要证明的东西。
                        let started = crate::now_millis().unwrap_or(0);
                        let out = tool.call(&call.arguments, &mut ctx);
                        let dur = crate::now_millis()
                            .unwrap_or(started)
                            .saturating_sub(started);
                        (out, ctx.read_files, started, dur)
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|h| match h.join() {
                    Ok(v) => v,
                    // 工具实现 panic 了：**不能把整个循环带走**——
                    // 那会让一次坏工具杀掉整场对话。
                    Err(_) => (
                        Err(ToolError::Failed {
                            detail: "工具执行时 panic 了".to_string(),
                        }),
                        BTreeSet::new(),
                        0,
                        0,
                    ),
                })
                .collect()
        });

        let mut out = Vec::with_capacity(batch.len());
        for (d, (result, read, started, duration)) in batch.into_iter().zip(results) {
            let Decided::Run {
                call,
                capability,
                reason,
                ..
            } = d
            else {
                unreachable!("只读批次里不该有已定案项")
            };
            // 把并行读到的文件并回主 ctx —— "先读后写"这条规则
            // 不能因为走了并行路径就失效
            self.ctx.read_files.extend(read);
            out.push(ToolCallRecord {
                tool: call.name.clone(),
                arguments: call.arguments.clone(),
                capability,
                decision: GateDecision::Allow { reason },
                output: Some(result.map(|o| o.text)),
                started_at_ms: started,
                duration_ms: duration,
            });
        }
        out
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
/// 把工具调用里的 `arguments` 修成合法 JSON 字符串。
///
/// ## 为什么必须有这一步
///
/// OpenAI 协议要求 `tool_calls[].function.arguments` 是一个
/// **JSON 字符串**。模型偶尔会给出不合法的——最常见的是被输出预算
/// 截断在参数中间：
///
/// ```text
/// {"path": "src/bill
/// ```
///
/// 而这条消息**要原样回灌进历史**（协议要求助手消息带 tool_calls，
/// 后面跟对应的 tool 结果）。**一条坏的调用会让下一轮整条请求
/// 被服务端拒掉**——报的还是一句看不懂的
/// `Assistant tool call <id>.arguments must be valid JSON.`
///
/// ## 为什么修成 `{}` 而不是丢掉
///
/// - **丢掉会破坏条数对应**：助手说调了 N 个、我们只回 N-1 个结果，
///   服务端同样报 400（D50 就是栽在这个上）
/// - 修成 `{}` 之后工具会报"参数坏了"，**模型收到一条明确的反馈**，
///   可以重试
///
/// 所以这是"**带着说明失败**"，不是"悄悄改掉"。
///
/// **参数本来就合法的原样保留**——只在坏的时候动它。
fn sanitize_tool_calls(raw: &[Value]) -> Vec<Value> {
    raw.iter()
        .map(|c| {
            let mut c = c.clone();
            let Some(f) = c.get_mut("function") else {
                return c;
            };
            let Some(args) = f.get_mut("arguments") else {
                return c;
            };
            if let Value::String(text) = args
                && serde_json::from_str::<Value>(text).is_err()
            {
                *args = Value::String("{}".to_string());
            }
            c
        })
        .collect()
}

pub fn parse_tool_calls(raw: &[Value]) -> Vec<ParsedCall> {
    let mut out = Vec::new();
    for c in raw {
        let id = c
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let Some(f) = c.get("function") else {
            // **没有 function 也要收下。**
            //
            // 助手消息里已经带着这条 `tool_calls` 了——**丢掉它会让
            // "助手说调了 N 个、我们只回 N-1 个结果"**，服务端于是报
            // 400（"assistant message with tool_calls must be followed by..."），
            // 而那个报错完全看不出是这里丢的。
            //
            // 收下来，让它走"没有这个工具"那条路：模型会收到一条说明，
            // 数量也就对上了。
            out.push(ParsedCall {
                id,
                name: String::new(),
                arguments: serde_json::json!({}),
            });
            continue;
        };
        let name = f
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
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
mod tool_call_shape_tests {
    use super::*;
    use serde_json::json;

    /// 一个最小工具，只为让注册表非空。
    struct EchoTool;
    impl Tool for EchoTool {
        fn name(&self) -> &str {
            "echo"
        }
        fn description(&self) -> &str {
            "回显"
        }
        fn parameters(&self) -> serde_json::Value {
            json!({"type": "object"})
        }
        fn capability(&self) -> Capability {
            Capability::ReadOnly
        }
        fn call(
            &self,
            _a: &serde_json::Value,
            _c: &mut ToolContext,
        ) -> Result<ToolOutput, ToolError> {
            Ok(ToolOutput::read(""))
        }
    }

    struct AlwaysAllow;
    impl Approver for AlwaysAllow {
        fn approve(&mut self, _req: &ApprovalRequest) -> Approval {
            Approval::Once
        }
    }

    #[test]
    fn a_call_without_a_name_is_kept_not_dropped() {
        // **这是那个间歇性 400 的成因。**
        //
        // 助手消息里已经带着这条 `tool_calls` 了。丢掉它会让
        // "助手说调了 N 个、我们只回 N-1 个结果"——服务端于是报
        // 400（assistant message with tool_calls must be followed by...），
        // 而那个报错**完全看不出是这里丢的**。
        let raw = vec![
            json!({"id": "c1", "type": "function",
                   "function": {"name": "read_file", "arguments": "{}"}}),
            json!({"id": "c2", "type": "function",
                   "function": {"arguments": "{}"}}), // 没有 name
        ];
        let calls = parse_tool_calls(&raw);
        assert_eq!(
            calls.len(),
            2,
            "数量必须和助手消息里的 tool_calls 对齐，否则必然 400"
        );
        assert_eq!(calls[1].id, "c2", "id 要保留，回灌时要用");
        assert!(calls[1].name.is_empty());
    }

    #[test]
    fn a_call_without_a_function_object_is_kept_too() {
        let raw = vec![json!({"id": "c1", "type": "function"})];
        let calls = parse_tool_calls(&raw);
        assert_eq!(calls.len(), 1, "没有 function 也要收下，数量不能少");
        assert_eq!(calls[0].id, "c1");
    }

    #[test]
    fn a_normal_call_still_parses_fully() {
        // 别为了容错把正常路径弄坏
        let raw = vec![json!({"id": "c1", "type": "function",
                              "function": {"name": "read_file",
                                           "arguments": "{\"path\":\"a.md\"}"}})];
        let calls = parse_tool_calls(&raw);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "read_file");
        assert_eq!(calls[0].arguments["path"], "a.md");
    }

    #[test]
    fn an_empty_name_says_the_response_was_incomplete() {
        // 空名字和"编了个工具名"要给模型不同的信号：
        // 前者让它重说一遍，后者让它换个工具名。
        let mut reg = ToolRegistry::new();
        reg.register(std::sync::Arc::new(EchoTool)).unwrap();
        let mut policy = ToolPolicy::default();
        policy.allow.push(Rule {
            tool: "*".into(),
            specifier: None,
        });
        let mut approver = AlwaysAllow;
        let mut runner = ToolRunner::new(
            &reg,
            policy,
            &mut approver,
            ToolContext::new(
                std::env::temp_dir(),
                crate::policy::SandboxMode::WorkspaceWrite,
            ),
        );
        let broken = ParsedCall {
            id: "c1".into(),
            name: String::new(),
            arguments: json!({}),
        };
        let decided = runner.decide_one(&broken);
        let rec = runner.finish_one(decided);
        match &rec.decision {
            GateDecision::Deny { reason } => {
                assert!(reason.contains("没带工具名"), "{reason}");
                assert!(reason.contains("不完整"), "要说清是响应的问题: {reason}");
            }
            other => panic!("空工具名该被拒: {other:?}"),
        }
        assert!(!rec.executed(), "没执行的不该被算成执行了");
    }

    #[test]
    fn an_unknown_name_still_says_there_is_no_such_tool() {
        let mut reg = ToolRegistry::new();
        reg.register(std::sync::Arc::new(EchoTool)).unwrap();
        let policy = ToolPolicy::default();
        let mut approver = AlwaysAllow;
        let mut runner = ToolRunner::new(
            &reg,
            policy,
            &mut approver,
            ToolContext::new(
                std::env::temp_dir(),
                crate::policy::SandboxMode::WorkspaceWrite,
            ),
        );
        let bogus = ParsedCall {
            id: "c1".into(),
            name: "no_such_tool".into(),
            arguments: json!({}),
        };
        let decided = runner.decide_one(&bogus);
        let rec = runner.finish_one(decided);
        match &rec.decision {
            GateDecision::Deny { reason } => assert!(reason.contains("没有这个工具"), "{reason}"),
            other => panic!("{other:?}"),
        }
    }
}

#[cfg(test)]
mod parallel_tests {
    use super::*;
    use crate::policy::SandboxMode;
    use serde_json::json;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    /// 一个**只读**工具：记下同时在跑的峰值，并睡一会儿。
    struct SlowRead {
        name: &'static str,
        concurrent: Arc<AtomicUsize>,
        peak: Arc<AtomicUsize>,
        sleep_ms: u64,
    }

    impl Tool for SlowRead {
        fn name(&self) -> &str {
            self.name
        }
        fn description(&self) -> &str {
            "慢速只读工具"
        }
        fn parameters(&self) -> serde_json::Value {
            json!({"type": "object"})
        }
        fn capability(&self) -> Capability {
            Capability::ReadOnly
        }
        fn call(
            &self,
            _args: &serde_json::Value,
            _ctx: &mut ToolContext,
        ) -> Result<ToolOutput, ToolError> {
            let now = self.concurrent.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(now, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(self.sleep_ms));
            self.concurrent.fetch_sub(1, Ordering::SeqCst);
            Ok(ToolOutput::read(format!("{} 读完", self.name)))
        }
    }

    /// 一个**有副作用**的工具：同样统计并发峰值。
    struct SlowWrite {
        concurrent: Arc<AtomicUsize>,
        peak: Arc<AtomicUsize>,
        sleep_ms: u64,
    }

    impl Tool for SlowWrite {
        fn name(&self) -> &str {
            "slow_write"
        }
        fn description(&self) -> &str {
            "慢速写入工具"
        }
        fn parameters(&self) -> serde_json::Value {
            json!({"type": "object"})
        }
        fn capability(&self) -> Capability {
            Capability::Write
        }
        fn call(
            &self,
            _args: &serde_json::Value,
            _ctx: &mut ToolContext,
        ) -> Result<ToolOutput, ToolError> {
            let now = self.concurrent.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(now, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(self.sleep_ms));
            self.concurrent.fetch_sub(1, Ordering::SeqCst);
            Ok(ToolOutput::changed("写完了"))
        }
    }

    /// 给纯判定测试用的占位工具。
    struct StubTool;
    impl Tool for StubTool {
        fn name(&self) -> &str {
            "stub"
        }
        fn description(&self) -> &str {
            "占位"
        }
        fn parameters(&self) -> serde_json::Value {
            json!({"type": "object"})
        }
        fn capability(&self) -> Capability {
            Capability::ReadOnly
        }
        fn call(
            &self,
            _a: &serde_json::Value,
            _c: &mut ToolContext,
        ) -> Result<ToolOutput, ToolError> {
            Ok(ToolOutput::read(""))
        }
    }

    struct AlwaysAllow;
    impl Approver for AlwaysAllow {
        fn approve(&mut self, _req: &ApprovalRequest) -> Approval {
            Approval::Once
        }
    }

    fn ctx() -> ToolContext {
        ToolContext::new(std::env::temp_dir(), SandboxMode::WorkspaceWrite)
    }

    fn pcall(name: &str) -> ParsedCall {
        ParsedCall {
            id: "c1".into(),
            name: name.into(),
            arguments: json!({}),
        }
    }

    fn open_policy() -> ToolPolicy {
        let mut p = ToolPolicy::default();
        p.allow.push(Rule {
            tool: "*".into(),
            specifier: None,
        });
        p
    }

    // ---- 不变量：只有只读能并行 ----

    #[test]
    fn only_read_only_calls_are_parallelizable() {
        // **这是整个特性的核心不变量。**
        let stub = StubTool;
        for cap in [
            Capability::Write,
            Capability::Execute,
            Capability::Network,
            Capability::Outbound,
            Capability::Unknown,
        ] {
            let d = Decided::Run {
                tool: &stub,
                call: pcall("x"),
                capability: cap,
                reason: String::new(),
            };
            assert!(!d.parallelizable(), "{cap:?} 不该被并行");
        }
        let ok = Decided::Run {
            tool: &stub,
            call: pcall("x"),
            capability: Capability::ReadOnly,
            reason: String::new(),
        };
        assert!(ok.parallelizable());
    }

    #[test]
    fn a_settled_decision_is_not_parallelizable() {
        let d = Decided::Settled(ToolCallRecord {
            tool: "x".into(),
            arguments: json!({}),
            capability: Capability::ReadOnly,
            decision: GateDecision::Deny {
                reason: "拒绝".into(),
            },
            output: None,
            started_at_ms: 0,
            duration_ms: 0,
        });
        assert!(!d.parallelizable());
    }

    // ---- 真并行 ----

    #[test]
    fn three_read_only_calls_actually_run_concurrently() {
        // **耗时证据 + 并发峰值证据。** 只看总耗时会"调度恰好快"就误判，
        // 所以两边都要看。
        let concurrent = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let mut reg = ToolRegistry::new();
        for n in ["r1", "r2", "r3"] {
            reg.register(Arc::new(SlowRead {
                name: n,
                concurrent: concurrent.clone(),
                peak: peak.clone(),
                sleep_ms: 300,
            }))
            .unwrap();
        }
        let mut approver = AlwaysAllow;
        let mut runner = ToolRunner::new(&reg, open_policy(), &mut approver, ctx());

        let decided: Vec<Decided> = ["r1", "r2", "r3"]
            .iter()
            .map(|n| Decided::Run {
                tool: reg.get(n).unwrap(),
                call: pcall(n),
                capability: Capability::ReadOnly,
                reason: String::new(),
            })
            .collect();

        let started = Instant::now();
        let out = runner.execute_batch(decided);
        let elapsed = started.elapsed();

        assert_eq!(out.len(), 3);
        assert_eq!(
            peak.load(Ordering::SeqCst),
            3,
            "三个只读没有同时跑——峰值并发 {}",
            peak.load(Ordering::SeqCst)
        );
        assert!(
            elapsed < Duration::from_millis(600),
            "耗时 {elapsed:?}，看起来还是串行的（串行要 900ms）"
        );
    }

    #[test]
    fn write_calls_never_run_concurrently() {
        // **并行写同一个文件是灾难。** 结果取决于调度，
        // 而那种不确定性事后完全无法复现。
        let concurrent = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(SlowWrite {
            concurrent: concurrent.clone(),
            peak: peak.clone(),
            sleep_ms: 50,
        }))
        .unwrap();
        let mut approver = AlwaysAllow;
        let mut runner = ToolRunner::new(&reg, open_policy(), &mut approver, ctx());

        let decided: Vec<Decided> = (0..3)
            .map(|_| Decided::Run {
                tool: reg.get("slow_write").unwrap(),
                call: pcall("slow_write"),
                capability: Capability::Write,
                reason: String::new(),
            })
            .collect();
        let out = runner.execute_batch(decided);

        assert_eq!(out.len(), 3);
        assert_eq!(
            peak.load(Ordering::SeqCst),
            1,
            "写入并发跑了——峰值 {}",
            peak.load(Ordering::SeqCst)
        );
    }

    #[test]
    fn a_mixed_batch_keeps_order() {
        // 顺序不能乱：工具结果要按模型请求的顺序回灌，
        // 否则它会以为结果和调用对不上
        let conc = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let mut reg = ToolRegistry::new();
        for n in ["r1", "r2"] {
            reg.register(Arc::new(SlowRead {
                name: n,
                concurrent: conc.clone(),
                peak: peak.clone(),
                sleep_ms: 30,
            }))
            .unwrap();
        }
        reg.register(Arc::new(SlowWrite {
            concurrent: conc.clone(),
            peak: peak.clone(),
            sleep_ms: 30,
        }))
        .unwrap();
        let mut approver = AlwaysAllow;
        let mut runner = ToolRunner::new(&reg, open_policy(), &mut approver, ctx());

        let seq = ["r1", "r2", "slow_write", "r1"];
        let decided: Vec<Decided> = seq
            .iter()
            .map(|n| {
                let t = reg.get(n).unwrap();
                Decided::Run {
                    tool: t,
                    call: pcall(n),
                    capability: t.capability(),
                    reason: String::new(),
                }
            })
            .collect();
        let out = runner.execute_batch(decided);

        let names: Vec<&str> = out.iter().map(|r| r.tool.as_str()).collect();
        assert_eq!(names, seq, "结果顺序必须和请求顺序一致");
    }

    #[test]
    fn a_lone_read_only_call_does_not_pay_thread_overhead() {
        let conc = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(SlowRead {
            name: "r1",
            concurrent: conc.clone(),
            peak: peak.clone(),
            sleep_ms: 10,
        }))
        .unwrap();
        let mut approver = AlwaysAllow;
        let mut runner = ToolRunner::new(&reg, open_policy(), &mut approver, ctx());

        let out = runner.execute_batch(vec![Decided::Run {
            tool: reg.get("r1").unwrap(),
            call: pcall("r1"),
            capability: Capability::ReadOnly,
            reason: String::new(),
        }]);
        assert_eq!(out.len(), 1);
        assert_eq!(peak.load(Ordering::SeqCst), 1);
    }

    // ---- 并行读到的文件要并回主 ctx ----

    #[test]
    fn files_read_in_parallel_are_recorded_for_read_before_write() {
        // **"先读后写"不能因为走了并行路径就失效。**
        //
        // 并行时每个线程一份自己的 ctx（不然没法并行），跑完要把
        // `read_files` 并回来——漏了的话，模型读完再改会被拒，
        // 而报错说"还没读过"，看起来就像它没读。
        struct ReadingTool;
        impl Tool for ReadingTool {
            fn name(&self) -> &str {
                "reading"
            }
            fn description(&self) -> &str {
                "读一个文件并记下来"
            }
            fn parameters(&self) -> serde_json::Value {
                json!({"type": "object"})
            }
            fn capability(&self) -> Capability {
                Capability::ReadOnly
            }
            fn call(
                &self,
                args: &serde_json::Value,
                ctx: &mut ToolContext,
            ) -> Result<ToolOutput, ToolError> {
                let p = std::path::PathBuf::from(args["path"].as_str().unwrap_or("a"));
                ctx.note_read(p);
                Ok(ToolOutput::read("ok"))
            }
        }

        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(ReadingTool)).unwrap();
        let mut approver = AlwaysAllow;
        let mut runner = ToolRunner::new(&reg, open_policy(), &mut approver, ctx());

        let decided: Vec<Decided> = ["f1", "f2", "f3"]
            .iter()
            .map(|p| Decided::Run {
                tool: reg.get("reading").unwrap(),
                call: ParsedCall {
                    id: "c".into(),
                    name: "reading".into(),
                    arguments: json!({"path": p}),
                },
                capability: Capability::ReadOnly,
                reason: String::new(),
            })
            .collect();
        runner.execute_batch(decided);

        for p in ["f1", "f2", "f3"] {
            assert!(
                runner.ctx.has_read(std::path::Path::new(p)),
                "{p} 没被并回主 ctx——并行路径漏了 read_files"
            );
        }
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
    fn identical_calls_stop_before_the_round_limit() {
        // **真机上抓到的形状**（D100）：一次运行调了 40 次工具、
        // 最后 12 次里 11 次是 `write_file`，把 19 行的文件撑到 49 行，
        // **直到撞上 8 轮上限才停**。
        //
        // "磨到上限"意味着白花了五轮的钱和时间。这条测试盯的是
        // **提前停**——而且停的理由要说清楚。
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
            .with_max_rounds(8)
            .run(&thinker, &mut layout, 512)
            .unwrap_err();
        match &err {
            ToolLoopError::Repeating { tool, times } => {
                assert_eq!(tool, "t");
                assert!(*times >= 3, "该报出真实的连续次数：{times}");
            }
            other => panic!("一模一样地连着调，该判成打转而不是磨到轮数上限：{other:?}"),
        }
        // **消息要能让模型换个做法。**
        // "轮数用完了"对它毫无信息量——它不知道自己做错了什么。
        let msg = err.to_string();
        assert!(msg.contains("同一个动作"), "{msg}");
        assert!(msg.contains("换个做法"), "{msg}");
    }

    #[test]
    fn rounds_are_bounded() {
        // 模型一直在请求工具 → 必须停下来，不能无限烧配额
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(ScriptTool::new("t", Capability::ReadOnly)))
            .unwrap();
        let mut ap = RefusingApprover;
        // **每一轮调用的参数要不一样。**
        //
        // 原来这里是 10 次一模一样的调用，于是加"重复检测"之后
        // 它会在第 3 轮就被 `Repeating` 拦下——**而这条测试要验的是
        // "轮数有上限"，不是"重复被抓"**。
        // 两件事都要有人验，但不该混在一条里：混着的话，
        // 重复检测一上线，轮数上限就再也没人验了。
        let replies: Vec<_> = (0..10)
            .map(|i| ScriptThinker::tool_reply(vec![call_json("c", "t", json!({ "i": i }))]))
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
    fn calls_without_a_name_are_kept_so_the_counts_match() {
        // **这条测试原来断言的是相反的（"没名字的就跳过"），而那个行为
        // 正是真机上那个间歇性 400 的成因。**
        //
        // 助手消息是**原样**带着全部 `tool_calls` 进历史的
        // （`layout.push_raw(Message::assistant_tool_calls(resp.tool_calls))`）。
        // 而协议要求**每一条** `tool_call_id` 都有对应的 `tool` 消息回灌。
        //
        // 跳掉没名字的那些，就变成"助手说调了 2 个、我们只回 1 个结果"——
        // 服务端报 400（"assistant message with tool_calls must be followed by..."），
        // **而那个报错完全看不出是这里丢的**。
        //
        // 现在收下来，让它走"没有这个工具"那条路：模型收到一条解释，
        // 消息条数也就对上了。**不执行它，但要回答它。**
        let raw = vec![
            json!({ "id": "c1", "function": { "arguments": "{}" } }),
            json!({ "id": "c2" }),
        ];
        let p = parse_tool_calls(&raw);
        assert_eq!(
            p.len(),
            2,
            "数量必须和助手消息里的 tool_calls 对齐，否则必然 400"
        );
        assert!(p[0].name.is_empty(), "没名字就如实留空，不要编一个");
        assert_eq!(p[1].id, "c2", "id 要保留——回灌时靠它对应");
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
            started_at_ms: 0,
            duration_ms: 0,
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
            started_at_ms: 0,
            duration_ms: 0,
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
            started_at_ms: 0,
            duration_ms: 0,
        };
        let v = serde_json::to_value(&rec).unwrap();
        assert_eq!(v["tool"], "read_file");
        assert_eq!(v["capability"], "read_only");
        assert!(rec.executed());
    }
}

#[cfg(test)]
mod sanitize_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_truncated_arguments_string_is_repaired() {
        // **真机上抓到的原文**：
        // `Assistant tool call <id>.arguments must be valid JSON.`
        // 根因是模型把参数截断在了中间。
        let raw = vec![json!({
            "id": "c1", "type": "function",
            "function": { "name": "edit_file", "arguments": "{\"path\": \"src/bill" }
        })];
        let out = sanitize_tool_calls(&raw);
        assert_eq!(
            out[0]["function"]["arguments"],
            json!("{}"),
            "坏的参数该被修成合法的空对象"
        );
        // **id 和 name 不能动**——回灌时靠 id 对应结果
        assert_eq!(out[0]["id"], json!("c1"));
        assert_eq!(out[0]["function"]["name"], json!("edit_file"));
    }

    #[test]
    fn valid_arguments_are_left_alone() {
        // **只在坏的时候动它。** 好的参数改掉就等于篡改模型的意图。
        let raw = vec![json!({
            "id": "c1", "type": "function",
            "function": { "name": "read_file", "arguments": "{\"path\":\"a.md\"}" }
        })];
        let out = sanitize_tool_calls(&raw);
        assert_eq!(
            out[0]["function"]["arguments"],
            json!("{\"path\":\"a.md\"}")
        );
    }

    #[test]
    fn the_call_count_never_changes() {
        // **条数必须对上**——助手说调了 N 个，就得回 N 个结果，
        // 少一个服务端照样 400（D50 那次的教训）。
        let raw = vec![
            json!({"id": "c1", "function": {"name": "a", "arguments": "{坏"}}),
            json!({"id": "c2", "function": {"name": "b", "arguments": "{}"}}),
            json!({"id": "c3"}),
        ];
        assert_eq!(sanitize_tool_calls(&raw).len(), 3, "一个都不能少");
    }

    #[test]
    fn a_call_without_a_function_does_not_panic() {
        let raw = vec![json!({"id": "c1"})];
        assert_eq!(sanitize_tool_calls(&raw).len(), 1);
    }
}

#[cfg(test)]
mod repeat_tests {
    use super::*;

    fn call(name: &str, args: &str) -> (String, String) {
        (name.to_string(), args.to_string())
    }

    #[test]
    fn three_identical_calls_in_a_row_is_spinning() {
        // **真机上抓到的形状**：连着 11 次写同一个文件。
        let seen = vec![
            call("write_file", "{\"path\":\"b.py\"}"),
            call("write_file", "{\"path\":\"b.py\"}"),
            call("write_file", "{\"path\":\"b.py\"}"),
        ];
        let (tool, times) = detect_repeat(&seen, REPEAT_LIMIT).expect("该判成打转");
        assert_eq!(tool, "write_file");
        assert_eq!(times, 3);
    }

    #[test]
    fn a_longer_streak_reports_the_real_count() {
        // 报"3 次"而实际连了 11 次，会让人低估问题的规模
        let mut seen = Vec::new();
        for _ in 0..11 {
            seen.push(call("write_file", "{}"));
        }
        assert_eq!(
            detect_repeat(&seen, REPEAT_LIMIT),
            Some(("write_file".into(), 11))
        );
    }

    #[test]
    fn reading_different_files_is_not_spinning() {
        // **模糊相似会把正常的批量读判成重复。** 比的是完整参数文本。
        let seen = vec![
            call("read_file", "{\"path\":\"a.rs\"}"),
            call("read_file", "{\"path\":\"b.rs\"}"),
            call("read_file", "{\"path\":\"c.rs\"}"),
        ];
        assert_eq!(detect_repeat(&seen, REPEAT_LIMIT), None);
    }

    #[test]
    fn two_identical_calls_are_still_allowed() {
        // **2 次太紧**——"读一遍、改一下、再读一遍确认"是合理的，
        // 而"读同一个文件两次"完全可能是对的。
        let seen = vec![
            call("read_file", "{\"path\":\"a.rs\"}"),
            call("read_file", "{\"path\":\"a.rs\"}"),
        ];
        assert_eq!(detect_repeat(&seen, REPEAT_LIMIT), None);
    }

    #[test]
    fn a_break_in_the_streak_resets_it() {
        // **只看末尾**。中间重复过、但已经换过做法了，就不算还在打转。
        let seen = vec![
            call("write_file", "{}"),
            call("write_file", "{}"),
            call("read_file", "{}"),
            call("write_file", "{}"),
            call("write_file", "{}"),
        ];
        assert_eq!(
            detect_repeat(&seen, REPEAT_LIMIT),
            None,
            "末尾只有 2 次连续"
        );
    }

    #[test]
    fn the_same_tool_with_different_arguments_is_not_repeating() {
        let seen = vec![
            call("write_file", "{\"path\":\"a\"}"),
            call("write_file", "{\"path\":\"b\"}"),
            call("write_file", "{\"path\":\"c\"}"),
        ];
        assert_eq!(detect_repeat(&seen, REPEAT_LIMIT), None);
    }
}
