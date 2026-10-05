//! 工具层：模型能对世界做的事，以及"哪些必须先问人"。
//!
//! ## 一个 trait，所有工具从同一个口进来
//!
//! 原生工具（读文件、跑命令、抓网页）和 MCP 工具（第三方 server 暴露的）
//! **注册进同一个注册表**，走同一套审批门禁。执行引擎不需要知道区别。
//!
//! 这也是为什么这一层要先定：MCP 只是"再注册一批工具进来"，
//! 而不是一条平行的链路。
//!
//! ## 审批按**能力类别**判定，不逐个工具写规则
//!
//! 逐个工具写规则是 Cline 官方明确说过的坑（"不使用固定 allowlist"）。
//! 新增一个工具就得记得给它加规则，忘一次就是一个洞。
//!
//! 七个独立来源指向同一条边界——**读可以自动，写必须问人**：
//!
//! | 来源 | 证据 |
//! |---|---|
//! | MCP 规范 | "there SHOULD always be a human in the loop with the ability to deny tool invocations" |
//! | Claude Code | `Read`/`Grep`/`Glob` 权限=否；`Write`/`Edit`/`Bash` 权限=是 |
//! | OpenAI MCP 工具 | **默认每次调用都要求批准** |
//! | OpenAI 连接器 | 邮件/日历干脆只提供只读工具 |
//! | Manus Desktop | "Every command … requires your explicit approval" |
//! | OpenHands | `UNKNOWN` 默认也触发确认 |
//! | OpenAI computer use | 屏幕内容不可作为授权依据 |
//!
//! ## 三层判定，与模型路由同构
//!
//! ```text
//! 1. 确定性规则（不花钱）：能力类别 + 白名单/黑名单 + 路径范围
//!         ↓ 拿不准
//! 2. 本地 Verdict（免费、离线）："这个动作该不该问人？"
//!         ↓ 它弃权
//! 3. 保守兜底：问人（fail closed）
//! ```
//!
//! **顺序不能反**，和路由同一条理由：每次都问模型，等于为了省钱先花钱。
//! OpenHands 那套 `Pattern → LLM → Ensemble 取最高风险` 就是这个结构，
//! 我们直接复用已有的三层，不另造一套。

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::decide::Decider;
use crate::policy::SandboxMode;

pub mod files;
pub mod runner;
pub mod system;
pub mod web;

pub use runner::{
    Approval, ApprovalRequest, Approver, DEFAULT_MAX_ROUNDS, RefusingApprover, ToolCallRecord,
    ToolLoopError, ToolRunOutcome, ToolRunner,
};

/// 工具的能力类别。**审批策略的唯一依据。**
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    /// 只读，无副作用。
    ReadOnly,
    /// 本地写入。
    Write,
    /// 执行外部命令。
    Execute,
    /// 出站网络。
    ///
    /// **单列而不是并进只读**：抓一个网页会泄露请求内容（URL 本身可能就是隐私），
    /// 而且返回的内容是**不可信数据**，会进模型的上下文。这和读本地文件不是一回事。
    /// Claude Code 对 `WebFetch`/`WebSearch` 也都标了"需要权限"。
    Network,
    /// 不可逆，或对外发送（发邮件、删除、付款）。
    ///
    /// **这一类永远需要人工，且不接受模型判定。** 见 [`gate`]。
    Outbound,
}

impl Capability {
    pub fn label(self) -> &'static str {
        match self {
            Capability::ReadOnly => "只读",
            Capability::Write => "写入",
            Capability::Execute => "执行",
            Capability::Network => "网络",
            Capability::Outbound => "不可逆",
        }
    }

    /// 无副作用的能力。**只有这一类在满足路径/域名约束时可以自动放行。**
    pub fn is_inert(self) -> bool {
        matches!(self, Capability::ReadOnly)
    }

    /// 不可逆的能力。**任何情况下都不交给模型判。**
    pub fn is_irreversible(self) -> bool {
        matches!(self, Capability::Outbound)
    }
}

/// 工具执行的结果。
#[derive(Debug, Clone, PartialEq)]
pub struct ToolOutput {
    /// 回灌给模型的文本。
    pub text: String,
    /// 这次调用改变了系统状态吗。**决定审批记忆能用多久。**
    ///
    /// 只读结论可以永久记住；改过状态的动作只记到会话结束——
    /// 因为环境已经不同了，"上次允许过"不能证明"这次也对"。
    pub changed_state: bool,
}

impl ToolOutput {
    pub fn read(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            changed_state: false,
        }
    }
    pub fn changed(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            changed_state: true,
        }
    }
}

/// 工具失败。
///
/// `Serialize` 是为了能进台账：**事后要能查清机器尝试过什么**。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ToolError {
    UnknownTool(String),
    /// 参数不合法。**带上实际收到的参数字符串**，否则没法排查模型给错了什么。
    BadArgs {
        detail: String,
    },
    /// 被策略拒绝。**不重试**——重试一次还是同样的判定。
    Denied {
        reason: String,
    },
    /// 工具自己执行失败了（网络不通、文件不存在…）。
    ///
    /// **这是正常结果，不是异常。** 它会被回灌给模型，让模型换条路走，
    /// 而不是让整个任务失败。
    Failed {
        detail: String,
    },
}

impl std::fmt::Display for ToolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ToolError::UnknownTool(n) => write!(f, "没有这个工具: {n}"),
            ToolError::BadArgs { detail } => write!(f, "参数不合法: {detail}"),
            ToolError::Denied { reason } => write!(f, "被策略拒绝: {reason}"),
            ToolError::Failed { detail } => write!(f, "执行失败: {detail}"),
        }
    }
}

impl std::error::Error for ToolError {}

/// 工具执行上下文。
#[derive(Debug)]
pub struct ToolContext {
    /// 工作目录。只读工具在工作区内免问，出界要问——照抄 Claude Code 的规则。
    pub cwd: PathBuf,
    pub sandbox: SandboxMode,
    /// **本次会话已经读过的文件。** `edit_file` 用它实现"先读后写"。
    ///
    /// 这条规则从 Claude Code 抄来：编辑前必须在当前对话读过该文件。
    /// 它能挡掉一大批"模型凭印象改文件"的事故。
    pub read_files: BTreeSet<PathBuf>,
}

impl ToolContext {
    pub fn new(cwd: impl Into<PathBuf>, sandbox: SandboxMode) -> Self {
        Self {
            cwd: cwd.into(),
            sandbox,
            read_files: BTreeSet::new(),
        }
    }

    /// 记下读过的文件。
    pub fn note_read(&mut self, path: impl Into<PathBuf>) {
        self.read_files.insert(path.into());
    }

    /// 这个路径读过吗。
    pub fn has_read(&self, path: &Path) -> bool {
        self.read_files.contains(path)
    }

    /// 路径是否落在工作目录内。
    ///
    /// 用**规范化后的前缀比较**，不是字符串包含——字符串包含会把
    /// `C:\work-evil` 判成在 `C:\work` 里面。
    pub fn inside_cwd(&self, path: &Path) -> bool {
        let base = normalize_existing(&self.cwd);
        let p = if path.is_absolute() {
            normalize_existing(path)
        } else {
            normalize_existing(&self.cwd.join(path))
        };
        p.starts_with(&base)
    }

    /// 把相对路径解析成绝对路径（不要求存在）。
    pub fn resolve(&self, path: &Path) -> PathBuf {
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.cwd.join(path)
        }
    }
}

/// 尽量规范化：能拿 canonicalize 就拿，拿不到就退回归一化 `..` 与 `.`。
///
/// ## 两个必须处理的坑（都是测试抓出来的）
///
/// 1. **不能直接 `canonicalize`**：要检查的路径往往还不存在（比如准备新建的文件），
///    这时 canonicalize 报错。所以要留一条手工归一化的退路。
///
/// 2. **Windows 上 `canonicalize` 返回 `\\?\` 前缀的"扩展长度路径"**，
///    而手工归一化不会。两边形式不一致时 `starts_with` 恒为假——
///    表现是"工作区内的只读访问每次都要审批"：功能没坏，但**规则悄悄失效了**。
///    所以最后统一剥掉前缀。这条在 Linux 上永远测不出来。
fn normalize_existing(path: &Path) -> PathBuf {
    let raw = match path.canonicalize() {
        Ok(c) => c,
        Err(_) => manual_normalize(path),
    };
    strip_extended_prefix(raw)
}

/// 手工消解 `.` 与 `..`，不碰文件系统。
fn manual_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// 剥掉 Windows 扩展长度路径前缀，让两种归一化形式可比。
fn strip_extended_prefix(p: PathBuf) -> PathBuf {
    let s = p.to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{rest}"));
    }
    if let Some(rest) = s.strip_prefix(r"\\?\") {
        return PathBuf::from(rest);
    }
    p
}

/// 一个工具。
pub trait Tool: Send + Sync {
    /// 模型看到的名字。**MCP 工具的格式是 `mcp__<server>__<tool>`**，
    /// 带出处前缀是为了两个 server 同名时不会静默串台。
    fn name(&self) -> &str;

    /// 给模型看的一句话说明。写清"什么时候该用它"，不只是"它是什么"。
    fn description(&self) -> &str;

    /// 参数的 JSON Schema（OpenAI function-calling 格式）。
    fn parameters(&self) -> serde_json::Value;

    fn capability(&self) -> Capability;

    /// 审批规则的**粒度**：文件工具给路径，网络工具给域名，命令工具给命令动词。
    ///
    /// 有了它，"总是允许读 D:\notes 下的文件"才写得出来。
    /// 返回 `None` 表示这个工具没有可归类的粒度——**那就每次都问**，
    /// 因为记不下规则就等于没有规则。
    ///
    /// ## 为什么需要 `ctx`
    ///
    /// 粒度必须是**工具实际会操作的那个路径**，而不是模型给的那串字符。
    /// 模型给 `notes\a.md`，工具会把它解析成 `<cwd>\notes\a.md`——
    /// 如果 specifier 返回前者，就会出两个问题：
    ///
    /// 1. **规则静默失效**：使用者写 `--allow read_file:D:\notes`，
    ///    而模型给的是相对路径 → 前缀对不上 → 每次都要问。
    ///    使用者会以为"我明明写了规则怎么还问"，然后去怀疑整个审批机制。
    /// 2. **更糟的一种**：审批提示上显示的是 `notes\a.md`，
    ///    而工具真正动的是 `<cwd>\notes\a.md`——**人在批准一个自己没看清的东西**。
    ///    这是"不看参数就批准等于没批准"的上一层。
    ///
    /// 所以签名里必须有 `ctx`。第一版没有，是第一版设计错了。
    fn specifier(&self, _args: &serde_json::Value, _ctx: &ToolContext) -> Option<String> {
        None
    }

    /// 执行。**实现里不要做审批判断**——那是 [`gate`] 的职责，
    /// 混在一起会出现"某个工具自己忘了检查"的洞。
    fn call(
        &self,
        args: &serde_json::Value,
        ctx: &mut ToolContext,
    ) -> Result<ToolOutput, ToolError>;
}

/// 工具注册表。
///
/// 存 `Arc<dyn Tool>` 而不是 `Box`：子集（受限场景只给一部分工具）只需要
/// 克隆 Arc，不必给 trait object 手写 `clone_box`。
#[derive(Default, Clone)]
pub struct ToolRegistry {
    tools: Vec<std::sync::Arc<dyn Tool>>,
}

impl std::fmt::Debug for ToolRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolRegistry")
            .field("tools", &self.names())
            .finish()
    }
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// 注册。**重名直接拒绝**，不覆盖——覆盖会让"为什么这个工具不见了"
    /// 变成一场考古。
    pub fn register(&mut self, tool: std::sync::Arc<dyn Tool>) -> Result<(), ToolError> {
        if self.get(tool.name()).is_some() {
            return Err(ToolError::BadArgs {
                detail: format!("工具名重复: {}", tool.name()),
            });
        }
        self.tools.push(tool);
        Ok(())
    }

    pub fn get(&self, name: &str) -> Option<&dyn Tool> {
        self.tools
            .iter()
            .find(|t| t.name() == name)
            .map(|t| t.as_ref())
    }

    pub fn names(&self) -> Vec<&str> {
        self.tools.iter().map(|t| t.name()).collect()
    }

    pub fn len(&self) -> usize {
        self.tools.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    /// 全部工具的能力类别。给审批预览用。
    pub fn capabilities(&self) -> Vec<(&str, Capability)> {
        self.tools
            .iter()
            .map(|t| (t.name(), t.capability()))
            .collect()
    }

    /// 渲染成 OpenAI 兼容的 `tools` 字段。
    ///
    /// **顺序必须稳定**（按注册顺序），因为这段 json 会进请求体。
    /// 每次调用顺序不同会让前缀缓存作废——MCP 那批尤其要注意，
    /// 不要把 `HashMap` 的迭代顺序漏进来。
    pub fn specs(&self) -> Vec<serde_json::Value> {
        self.tools
            .iter()
            .map(|t| {
                serde_json::json!({
                    "type": "function",
                    "function": {
                        "name": t.name(),
                        "description": t.description(),
                        "parameters": t.parameters(),
                    }
                })
            })
            .collect()
    }

    /// 只要这些名字的工具（子代理/受限场景用）。
    pub fn subset(&self, names: &[&str]) -> Self {
        Self {
            tools: self
                .tools
                .iter()
                .filter(|t| names.contains(&t.name()))
                .cloned()
                .collect(),
        }
    }
}

/// 一条审批规则。**规则是"工具 + 粒度"**，不是"工具"。
///
/// `specifier` 为 `None` 表示"这个工具的全部调用"。
/// 想写"总是允许读 `D:\notes` 下的文件"，就把路径放进 specifier。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Rule {
    pub tool: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub specifier: Option<String>,
}

impl Rule {
    pub fn tool(name: impl Into<String>) -> Self {
        Self {
            tool: name.into(),
            specifier: None,
        }
    }
    pub fn scoped(name: impl Into<String>, spec: impl Into<String>) -> Self {
        Self {
            tool: name.into(),
            specifier: Some(spec.into()),
        }
    }

    /// 这条规则覆盖这次调用吗。
    ///
    /// 粒度用**边界前缀匹配**，不是字符串前缀匹配。这个区别是安全相关的：
    ///
    /// ```text
    /// 规则 https://docs.rs   字符串前缀会命中 https://docs.rs.evil.example/   ← 洞
    /// 规则 D:\notes          字符串前缀会命中 D:\notes-evil\                 ← 同一个洞
    /// ```
    ///
    /// 也就是说使用者写"总是允许抓 docs.rs"，结果放行了攻击者的域名。
    /// 所以匹配要求 rule 之后**紧跟着一个分隔符**（或整串相等）。
    ///
    /// 这个洞是 web-writer 在自己的模块注释里报上来的——**工具实现者比接口
    /// 作者更容易发现边界规则的漏洞**，因为它就在他每天要写 specifier 的地方。
    pub fn matches(&self, tool: &str, specifier: Option<&str>) -> bool {
        if self.tool != tool {
            return false;
        }
        match (&self.specifier, specifier) {
            (None, _) => true,
            (Some(_), None) => false,
            (Some(rule), Some(actual)) => boundary_match(rule, actual),
        }
    }
}

/// 分隔符：路径与 URL 都算。**两类 specifier 共用一套规则**，
/// 因为它们要防的是同一个东西——"前缀相同但其实是别的主体"。
const SEPARATORS: [char; 2] = ['/', '\\'];

/// 带边界的前缀匹配。
///
/// `rule` 是 `actual` 的前缀**并且**后面紧跟分隔符（或完全相等）才算命中。
pub fn boundary_match(rule: &str, actual: &str) -> bool {
    if actual == rule {
        return true;
    }
    let Some(rest) = actual.strip_prefix(rule) else {
        return false;
    };
    // 规则自己已经以分隔符结尾 → 命中的就是那个目录/路径本身
    if rule.ends_with(SEPARATORS) {
        return true;
    }
    rest.starts_with(SEPARATORS)
}

/// 审批策略。Default 是手写的，见下方说明。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolPolicy {
    /// 预批准。
    #[serde(default)]
    pub allow: Vec<Rule>,
    /// 拒绝。**优先级高于 allow**——deny 先判，这点和 Cursor 一致。
    #[serde(default)]
    pub deny: Vec<Rule>,
    /// 只读工具在工作区内是否免问。默认 `true`（照抄 Claude Code）。
    #[serde(default = "default_true")]
    pub inert_inside_cwd_is_free: bool,
}

fn default_true() -> bool {
    true
}

/// **`Default` 是手写的，不是 derive 的。**
///
/// serde 的 `default = "default_true"` 只影响反序列化，而 `#[derive(Default)]`
/// 会给 `false`——两者不一致时，从配置文件读进来的策略与代码里
/// `ToolPolicy::default()` 造出来的策略**行为不同**。测试抓到了这个。
impl Default for ToolPolicy {
    fn default() -> Self {
        Self {
            allow: Vec::new(),
            deny: Vec::new(),
            inert_inside_cwd_is_free: true,
        }
    }
}

impl ToolPolicy {
    /// 空策略：**什么都要问**。最安全的起点。
    pub fn deny_all() -> Self {
        Self {
            allow: Vec::new(),
            deny: Vec::new(),
            inert_inside_cwd_is_free: false,
        }
    }
}

/// 审批判定。
///
/// `Serialize` 是为了能进台账：**审批决定必须留痕**，否则事后无法解释
/// "为什么这个动作被放行了"——而那是安全审计里最需要回答的问题。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum GateDecision {
    Allow {
        reason: String,
    },
    Ask {
        reason: String,
    },
    /// 直接拒绝。**不降级、不问人、不给模型再试的机会。**
    Deny {
        reason: String,
    },
}

impl GateDecision {
    pub fn is_allow(&self) -> bool {
        matches!(self, GateDecision::Allow { .. })
    }
    pub fn is_deny(&self) -> bool {
        matches!(self, GateDecision::Deny { .. })
    }
    pub fn reason(&self) -> &str {
        match self {
            GateDecision::Allow { reason }
            | GateDecision::Ask { reason }
            | GateDecision::Deny { reason } => reason,
        }
    }
}

/// 审批门禁。**三层，从便宜到贵。**
///
/// `decider` 为 `None` 表示本地决策模型不可用——此时只会走第一层和兜底，
/// **不会因此变宽松**。
pub fn gate(
    tool: &dyn Tool,
    args: &serde_json::Value,
    policy: &ToolPolicy,
    ctx: &ToolContext,
    decider: Option<&dyn Decider>,
) -> GateDecision {
    let name = tool.name();
    let spec = tool.specifier(args, ctx);
    let cap = tool.capability();

    // ---- 第一层：确定性规则。不花钱、不耗时。 ----

    // deny 优先。黑名单命中就是终点。
    if let Some(r) = policy
        .deny
        .iter()
        .find(|r| r.matches(name, spec.as_deref()))
    {
        return GateDecision::Deny {
            reason: format!(
                "命中拒绝规则 {name}({})",
                r.specifier.as_deref().unwrap_or("*")
            ),
        };
    }

    // 不可逆动作：**命中白名单才放行，否则一律问人，且不问模型。**
    //
    // 这里刻意不接受 `decider` 的判定。ADR §八 第 2 条写的是"不可逆动作必须
    // 人工批准，且批准不参与降级"——让一个模型去决定"这次删除不用问人"，
    // 正是那条规则要禁止的事。
    if cap.is_irreversible() {
        if let Some(r) = policy
            .allow
            .iter()
            .find(|r| r.matches(name, spec.as_deref()))
        {
            return GateDecision::Allow {
                reason: format!(
                    "不可逆动作，但命中显式预批准 {name}({})",
                    r.specifier.as_deref().unwrap_or("*")
                ),
            };
        }
        return GateDecision::Ask {
            reason: format!("{}动作必须人工确认（不接受模型判定）", cap.label()),
        };
    }

    if let Some(r) = policy
        .allow
        .iter()
        .find(|r| r.matches(name, spec.as_deref()))
    {
        return GateDecision::Allow {
            reason: format!(
                "命中预批准 {name}({})",
                r.specifier.as_deref().unwrap_or("*")
            ),
        };
    }

    // 只读免问。照抄 Claude Code 的规则：`Read`/`Grep`/`Glob` 在工作区内不问。
    //
    // 两种情况都放行：
    // - **有粒度**（文件工具）：粒度在工作区内 → 免问；出界 → 继续往下走。
    // - **没有粒度**（比如 `now` 读系统时钟）：没有"界"可越，本身就是纯本地无副作用。
    //   要求它每次审批是荒谬的摩擦，而且不会带来任何安全性——
    //   声明成 `ReadOnly` 的工具按定义就没有副作用可言。
    if cap.is_inert() && policy.inert_inside_cwd_is_free {
        match spec.as_deref() {
            None => {
                return GateDecision::Allow {
                    reason: "纯只读，无副作用".to_string(),
                };
            }
            Some(p) => {
                if ctx.inside_cwd(Path::new(p)) {
                    return GateDecision::Allow {
                        reason: format!("只读且在工作区内: {p}"),
                    };
                }
                // 出界：继续往下走，让本地决策模型或人来看
            }
        }
    }

    // ---- 第二层：本地决策模型。免费、离线、约 15ms ----
    if let Some(decision) = ask_local(decider, tool, args, spec.as_deref(), ctx) {
        return decision;
    }

    // ---- 第三层：保守兜底。**问人。** ----
    GateDecision::Ask {
        reason: format!(
            "{}能力，{}；信号不足，按最保守处理",
            cap.label(),
            match &spec {
                Some(s) => format!("范围 {s}"),
                None => "无可归类粒度".to_string(),
            }
        ),
    }
}

/// 问本地决策模型"这个动作该不该问人"。
///
/// 任何异常（连不上、弃权、返回未知选项）都返回 `None` 由调用方兜底。
/// **兜底是"问人"，不是"放行"**——这一层失败只会让事情更保守。
fn ask_local(
    decider: Option<&dyn Decider>,
    tool: &dyn Tool,
    args: &serde_json::Value,
    spec: Option<&str>,
    ctx: &ToolContext,
) -> Option<GateDecision> {
    let decider = decider?;
    use crate::decide::{DecisionRequest, Question};

    let question = Question::choice(
        "needs_human",
        "这个动作该不该先问过使用者？",
        &[
            ("auto", "无需询问：它没有副作用，或者副作用完全在预期范围内"),
            ("ask", "需要询问：它会改变系统状态、或把信息发到外部"),
        ],
    );

    // state 要带足够判断的信息。**参数原文要截断**——工具有可能带很长的内容。
    let args_head: String = args.to_string().chars().take(600).collect();
    let state = serde_json::json!({
        "tool": tool.name(),
        "capability": tool.capability().label(),
        "scope": spec,
        "arguments": args_head,
        "cwd": ctx.cwd.to_string_lossy(),
        "inside_cwd": spec.map(|s| ctx.inside_cwd(Path::new(s))),
    });

    let req = DecisionRequest::new(state, vec![question]);
    let result = decider.decide(&req).ok()?;
    match result.choice("needs_human") {
        Some("auto") => Some(GateDecision::Allow {
            reason: format!("本地决策模型判定无需询问（{}）", result.model),
        }),
        Some("ask") => Some(GateDecision::Ask {
            reason: format!("本地决策模型判定需要询问（{}）", result.model),
        }),
        // 未知选项、缺答案 —— 都算弃权，交给兜底
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 测试用工具：能力与粒度都可配。
    #[derive(Clone)]
    struct FakeTool {
        name: String,
        cap: Capability,
        spec_kind: SpecKind,
        fail: bool,
    }

    #[derive(Clone)]
    enum SpecKind {
        Path,
        Domain,
        None,
    }

    impl FakeTool {
        fn new(name: &str, cap: Capability) -> Self {
            Self {
                name: name.into(),
                cap,
                spec_kind: SpecKind::None,
                fail: false,
            }
        }
        fn with_spec(mut self, k: SpecKind) -> Self {
            self.spec_kind = k;
            self
        }
        fn failing(mut self) -> Self {
            self.fail = true;
            self
        }
    }

    impl Tool for FakeTool {
        fn name(&self) -> &str {
            &self.name
        }
        fn description(&self) -> &str {
            "测试工具"
        }
        fn parameters(&self) -> serde_json::Value {
            json!({ "type": "object", "properties": {} })
        }
        fn capability(&self) -> Capability {
            self.cap
        }
        fn specifier(&self, args: &serde_json::Value, _ctx: &ToolContext) -> Option<String> {
            match self.spec_kind {
                SpecKind::Path => args.get("path").and_then(|v| v.as_str()).map(String::from),
                SpecKind::Domain => args.get("url").and_then(|v| v.as_str()).map(String::from),
                SpecKind::None => None,
            }
        }
        fn call(
            &self,
            _args: &serde_json::Value,
            _ctx: &mut ToolContext,
        ) -> Result<ToolOutput, ToolError> {
            if self.fail {
                return Err(ToolError::Failed {
                    detail: "测试失败".into(),
                });
            }
            Ok(ToolOutput::read("ok"))
        }
    }

    fn ctx() -> ToolContext {
        ToolContext::new(std::env::temp_dir(), SandboxMode::WorkspaceWrite)
    }

    // ---- 能力分类 ----

    #[test]
    fn only_readonly_is_inert() {
        assert!(Capability::ReadOnly.is_inert());
        for c in [
            Capability::Write,
            Capability::Execute,
            Capability::Network,
            Capability::Outbound,
        ] {
            assert!(!c.is_inert(), "{c:?} 不该被当成无副作用");
        }
    }

    #[test]
    fn network_is_not_readonly() {
        // 抓网页会泄露 URL，而且返回内容是不可信数据，会进模型上下文。
        // 把它并进只读会让所有网络访问自动放行。
        assert!(!Capability::Network.is_inert());
    }

    #[test]
    fn only_outbound_is_irreversible() {
        assert!(Capability::Outbound.is_irreversible());
        for c in [
            Capability::ReadOnly,
            Capability::Write,
            Capability::Execute,
            Capability::Network,
        ] {
            assert!(!c.is_irreversible());
        }
    }

    // ---- 注册表 ----

    #[test]
    fn duplicate_tool_names_are_rejected() {
        // 覆盖会让"为什么这个工具不见了"变成考古
        let mut r = ToolRegistry::new();
        r.register(std::sync::Arc::new(FakeTool::new(
            "a",
            Capability::ReadOnly,
        )))
        .unwrap();
        let err = r.register(std::sync::Arc::new(FakeTool::new(
            "a",
            Capability::ReadOnly,
        )));
        assert!(matches!(err, Err(ToolError::BadArgs { .. })), "{err:?}");
        assert_eq!(r.len(), 1);
    }

    #[test]
    fn specs_are_openai_shaped_and_order_stable() {
        let mut r = ToolRegistry::new();
        r.register(std::sync::Arc::new(FakeTool::new(
            "b",
            Capability::ReadOnly,
        )))
        .unwrap();
        r.register(std::sync::Arc::new(FakeTool::new(
            "a",
            Capability::ReadOnly,
        )))
        .unwrap();
        let s = r.specs();
        assert_eq!(s.len(), 2);
        assert_eq!(s[0]["type"], "function");
        assert_eq!(s[0]["function"]["name"], "b");
        // 顺序按注册顺序，两次调用必须逐字节相同——这段 json 会进请求体
        assert_eq!(
            serde_json::to_string(&r.specs()).unwrap(),
            serde_json::to_string(&r.specs()).unwrap()
        );
    }

    #[test]
    fn subset_keeps_only_named_tools() {
        let mut r = ToolRegistry::new();
        for n in ["a", "b", "c"] {
            r.register(std::sync::Arc::new(FakeTool::new(n, Capability::ReadOnly)))
                .unwrap();
        }
        let sub = r.subset(&["a", "c"]);
        assert_eq!(sub.names(), vec!["a", "c"]);
    }

    // ---- 审批门禁：第一层 ----

    #[test]
    fn deny_beats_allow() {
        // deny 优先，这点和 Cursor 一致
        let t = FakeTool::new("x", Capability::Write);
        let policy = ToolPolicy {
            allow: vec![Rule::tool("x")],
            deny: vec![Rule::tool("x")],
            ..Default::default()
        };
        let d = gate(&t, &json!({}), &policy, &ctx(), None);
        assert!(d.is_deny(), "{d:?}");
    }

    #[test]
    fn irreversible_always_asks_even_with_a_decider() {
        // 关键性质：**不可逆动作不接受模型判定。**
        // 就算本地决策模型说"无需询问"，也必须问人。
        use crate::decide::StubDecider;
        let t = FakeTool::new("send_mail", Capability::Outbound);
        let stub = StubDecider::succeeding().with_choice("needs_human", "auto");
        let d = gate(&t, &json!({}), &ToolPolicy::default(), &ctx(), Some(&stub));
        assert!(
            matches!(d, GateDecision::Ask { .. }),
            "不可逆动作必须问人: {d:?}"
        );
        assert!(d.reason().contains("不接受模型判定"), "{d:?}");
        assert_eq!(stub.calls(), 0, "不可逆动作不该去问模型");
    }

    #[test]
    fn irreversible_can_be_preapproved_only_explicitly() {
        let t = FakeTool::new("send_mail", Capability::Outbound);
        let policy = ToolPolicy {
            allow: vec![Rule::tool("send_mail")],
            ..Default::default()
        };
        let d = gate(&t, &json!({}), &policy, &ctx(), None);
        assert!(d.is_allow(), "显式预批准应生效: {d:?}");
        assert!(d.reason().contains("不可逆"));
    }

    #[test]
    fn scoped_allow_uses_prefix_matching() {
        let t = FakeTool::new("read_file", Capability::ReadOnly).with_spec(SpecKind::Path);
        let policy = ToolPolicy {
            allow: vec![Rule::scoped("read_file", "D:\\notes")],
            deny: Vec::new(),
            inert_inside_cwd_is_free: false,
        };
        assert!(
            gate(
                &t,
                &json!({ "path": "D:\\notes\\a.md" }),
                &policy,
                &ctx(),
                None
            )
            .is_allow()
        );
        // 前缀不匹配就不算命中
        assert!(
            !gate(
                &t,
                &json!({ "path": "D:\\other\\a.md" }),
                &policy,
                &ctx(),
                None
            )
            .is_allow()
        );
    }

    /// **安全回归：前缀相同但不是同一个主体。**
    ///
    /// 字符串前缀匹配会让规则 `https://docs.rs` 命中
    /// `https://docs.rs.evil.example/`——使用者写"总是允许抓 docs.rs"，
    /// 结果放行了攻击者的域名。路径上是同一个洞（`D:\notes` vs `D:\notes-evil`）。
    ///
    /// 这个洞是 web-writer 在自己的模块注释里报上来的：工具实现者比接口作者
    /// 更容易发现边界规则的漏洞，因为它就在他每天要写 specifier 的地方。
    #[test]
    fn rule_matching_requires_a_boundary_not_just_a_prefix() {
        // 域名：点号后面的东西是另一个域
        assert!(!boundary_match(
            "https://docs.rs",
            "https://docs.rs.evil.example/x"
        ));
        // 路径：连字符后面的东西是另一个目录
        assert!(!boundary_match("D:\\notes", "D:\\notes-evil\\a.md"));
        // 正常的前缀仍要命中
        assert!(boundary_match("https://docs.rs", "https://docs.rs"));
        assert!(boundary_match("https://docs.rs", "https://docs.rs/rmcp"));
        assert!(boundary_match("D:\\notes", "D:\\notes\\a.md"));
        // 规则自己带分隔符也算
        assert!(boundary_match("D:\\notes\\", "D:\\notes\\a.md"));
        assert!(boundary_match("https://docs.rs/", "https://docs.rs/rmcp"));
        // 不相关的不命中
        assert!(!boundary_match("https://docs.rs", "https://rust-lang.org"));
    }

    #[test]
    fn the_domain_hole_is_closed_end_to_end() {
        // 同一件事从 gate 走一遍，确保不是只修了辅助函数
        let t = FakeTool::new("web_fetch", Capability::Network).with_spec(SpecKind::Domain);
        let policy = ToolPolicy {
            allow: vec![Rule::scoped("web_fetch", "https://docs.rs")],
            deny: Vec::new(),
            inert_inside_cwd_is_free: false,
        };
        assert!(
            gate(
                &t,
                &json!({ "url": "https://docs.rs/rmcp" }),
                &policy,
                &ctx(),
                None
            )
            .is_allow()
        );
        assert!(
            !gate(
                &t,
                &json!({ "url": "https://docs.rs.evil.example/steal" }),
                &policy,
                &ctx(),
                None
            )
            .is_allow(),
            "前缀相同的攻击者域名被放行了"
        );
    }

    #[test]
    fn a_shorter_rule_does_not_swallow_a_longer_sibling() {
        // 规则写工具名的一部分不该命中另一个工具
        let r = Rule::tool("read");
        assert!(!r.matches("read_file", None));
        assert!(r.matches("read", None));
    }

    #[test]
    fn rule_without_specifier_covers_everything() {
        let r = Rule::tool("x");
        assert!(r.matches("x", Some("anything")));
        assert!(r.matches("x", None));
        assert!(!r.matches("y", None));
    }

    #[test]
    fn network_tools_can_be_scoped_by_domain() {
        // 网络的粒度是域名：想写"总是允许抓 docs.rs"就要靠它。
        // Claude Code 的 WebFetch 也是按域名记规则的。
        let t = FakeTool::new("web_fetch", Capability::Network).with_spec(SpecKind::Domain);
        let policy = ToolPolicy {
            allow: vec![Rule::scoped("web_fetch", "https://docs.rs")],
            deny: Vec::new(),
            inert_inside_cwd_is_free: false,
        };
        assert!(
            gate(
                &t,
                &json!({ "url": "https://docs.rs/rmcp/latest/rmcp/" }),
                &policy,
                &ctx(),
                None
            )
            .is_allow()
        );
        assert!(
            !gate(
                &t,
                &json!({ "url": "https://evil.example.com/" }),
                &policy,
                &ctx(),
                None
            )
            .is_allow()
        );
    }

    #[test]
    fn network_inside_cwd_is_still_not_free() {
        // 网络没有"在工作区内"这个概念。就算 url 恰好是个本地路径，
        // 也不该因为落在工作区就免问——那会把任意 URL 都放行。
        let t = FakeTool::new("web_fetch", Capability::Network).with_spec(SpecKind::Domain);
        let dir = std::env::temp_dir();
        let c = ToolContext::new(&dir, SandboxMode::WorkspaceWrite);
        let d = gate(
            &t,
            &json!({ "url": dir.join("x").to_string_lossy() }),
            &policy_free(),
            &c,
            None,
        );
        assert!(!d.is_allow(), "网络访问不该因为路径在工作区内就免问: {d:?}");
    }

    #[test]
    fn rule_with_specifier_needs_a_specifier_to_match() {
        // 规则写了粒度，但这次调用没有粒度 → 不算命中。
        // 反过来会让"只允许读这个目录"变成"允许这个工具的一切"
        let r = Rule::scoped("x", "D:\\notes");
        assert!(!r.matches("x", None));
    }

    // ---- 审批门禁：只读免问 ----

    #[test]
    fn readonly_inside_cwd_is_free() {
        let dir = std::env::temp_dir();
        let mut c = ToolContext::new(&dir, SandboxMode::WorkspaceWrite);
        c.note_read(&dir);
        let t = FakeTool::new("read_file", Capability::ReadOnly).with_spec(SpecKind::Path);
        let policy = ToolPolicy::default();
        let d = gate(
            &t,
            &json!({ "path": dir.join("a.md").to_string_lossy() }),
            &policy,
            &c,
            None,
        );
        assert!(d.is_allow(), "工作区内只读应免问: {d:?}");
    }

    #[test]
    fn readonly_outside_cwd_is_not_free() {
        let dir = std::env::temp_dir().join("yunxi-cwd-test");
        let c = ToolContext::new(&dir, SandboxMode::WorkspaceWrite);
        let t = FakeTool::new("read_file", Capability::ReadOnly).with_spec(SpecKind::Path);
        let d = gate(
            &t,
            &json!({ "path": "C:\\Windows\\System32\\config\\SAM" }),
            &policy_free(),
            &c,
            None,
        );
        assert!(!d.is_allow(), "工作区外只读不该免问: {d:?}");
    }

    fn policy_free() -> ToolPolicy {
        ToolPolicy {
            allow: Vec::new(),
            deny: Vec::new(),
            inert_inside_cwd_is_free: true,
        }
    }

    #[test]
    fn readonly_without_a_path_is_free() {
        // 比如 `now` 读系统时钟：没有"界"可越，纯本地无副作用。
        // 要求它每次审批是荒谬的摩擦，而且换不来任何安全性。
        let t = FakeTool::new("now", Capability::ReadOnly);
        let d = gate(&t, &json!({}), &ToolPolicy::default(), &ctx(), None);
        assert!(d.is_allow(), "无路径的只读工具应免问: {d:?}");
    }

    #[test]
    fn readonly_without_a_path_still_asks_under_deny_all() {
        // 但 deny_all 策略下什么都问——这是"最保守起点"该有的样子
        let t = FakeTool::new("now", Capability::ReadOnly);
        let d = gate(&t, &json!({}), &ToolPolicy::deny_all(), &ctx(), None);
        assert!(!d.is_allow(), "{d:?}");
    }

    #[test]
    fn write_is_never_free_even_inside_cwd() {
        let dir = std::env::temp_dir();
        let c = ToolContext::new(&dir, SandboxMode::WorkspaceWrite);
        let t = FakeTool::new("write_file", Capability::Write).with_spec(SpecKind::Path);
        let d = gate(
            &t,
            &json!({ "path": dir.join("a.md").to_string_lossy() }),
            &policy_free(),
            &c,
            None,
        );
        assert!(!d.is_allow(), "写入在工作区内也要问: {d:?}");
    }

    // ---- 审批门禁：第二、三层 ----

    #[test]
    fn local_decider_can_auto_approve() {
        use crate::decide::StubDecider;
        let t = FakeTool::new("run_command", Capability::Execute);
        let stub = StubDecider::succeeding().with_choice("needs_human", "auto");
        let d = gate(&t, &json!({}), &ToolPolicy::default(), &ctx(), Some(&stub));
        assert!(d.is_allow(), "{d:?}");
        assert_eq!(stub.calls(), 1);
    }

    #[test]
    fn local_decider_asking_wins_over_fallback() {
        use crate::decide::StubDecider;
        let t = FakeTool::new("run_command", Capability::Execute);
        let stub = StubDecider::succeeding().with_choice("needs_human", "ask");
        let d = gate(&t, &json!({}), &ToolPolicy::default(), &ctx(), Some(&stub));
        assert!(matches!(d, GateDecision::Ask { .. }), "{d:?}");
    }

    #[test]
    fn decider_failure_falls_back_to_asking_a_human() {
        // **兜底是"问人"，不是"放行"。** 这一层失败只会让事情更保守。
        use crate::decide::StubDecider;
        let t = FakeTool::new("run_command", Capability::Execute);
        let stub = StubDecider::failing("sidecar 没起来");
        let d = gate(&t, &json!({}), &ToolPolicy::default(), &ctx(), Some(&stub));
        assert!(matches!(d, GateDecision::Ask { .. }), "{d:?}");
    }

    #[test]
    fn unknown_choice_is_treated_as_abstention() {
        use crate::decide::StubDecider;
        let t = FakeTool::new("run_command", Capability::Execute);
        let stub = StubDecider::succeeding().with_choice("needs_human", "nonsense");
        let d = gate(&t, &json!({}), &ToolPolicy::default(), &ctx(), Some(&stub));
        assert!(matches!(d, GateDecision::Ask { .. }), "{d:?}");
    }

    #[test]
    fn no_decider_still_asks() {
        let t = FakeTool::new("run_command", Capability::Execute);
        let d = gate(&t, &json!({}), &ToolPolicy::default(), &ctx(), None);
        assert!(matches!(d, GateDecision::Ask { .. }), "{d:?}");
        assert!(d.reason().contains("最保守"));
    }

    #[test]
    fn deny_all_policy_never_auto_approves() {
        // 空策略是最安全的起点：什么都要问
        let policy = ToolPolicy::deny_all();
        for cap in [
            Capability::ReadOnly,
            Capability::Write,
            Capability::Execute,
            Capability::Network,
            Capability::Outbound,
        ] {
            let t = FakeTool::new("t", cap).with_spec(SpecKind::Path);
            let d = gate(
                &t,
                &json!({ "path": std::env::temp_dir().join("x").to_string_lossy() }),
                &policy,
                &ctx(),
                None,
            );
            assert!(!d.is_allow(), "{cap:?} 在 deny_all 下不该放行: {d:?}");
        }
    }

    #[test]
    fn every_gate_decision_has_a_reason() {
        // 理由要进台账。没有理由的审批决定事后无法解释。
        let t = FakeTool::new("x", Capability::Write);
        for policy in [ToolPolicy::default(), ToolPolicy::deny_all()] {
            let d = gate(&t, &json!({}), &policy, &ctx(), None);
            assert!(!d.reason().is_empty(), "{d:?}");
        }
    }

    // ---- 路径边界 ----

    #[test]
    fn inside_cwd_does_not_use_string_prefix() {
        // 字符串包含会把 C:\work-evil 判成在 C:\work 里
        let base = std::env::temp_dir().join("yunxi-base");
        let c = ToolContext::new(&base, SandboxMode::WorkspaceWrite);
        let sibling = std::env::temp_dir().join("yunxi-base-evil");
        assert!(
            !c.inside_cwd(&sibling),
            "同前缀的兄弟目录不该被当成工作区内"
        );
    }

    #[test]
    fn inside_cwd_handles_parent_traversal() {
        let base = std::env::temp_dir().join("yunxi-base");
        let c = ToolContext::new(&base, SandboxMode::WorkspaceWrite);
        // 试图用 .. 走出工作区
        let escape = base.join("..").join("elsewhere");
        assert!(!c.inside_cwd(&escape), ".. 不该能走出工作区");
    }

    #[test]
    fn relative_paths_resolve_against_cwd() {
        let base = std::env::temp_dir().join("yunxi-base");
        let c = ToolContext::new(&base, SandboxMode::WorkspaceWrite);
        assert!(c.inside_cwd(Path::new("a/b.md")));
        assert_eq!(c.resolve(Path::new("a.md")), base.join("a.md"));
    }

    /// 回归：Windows 上 `canonicalize()` 返回 `\\?\` 前缀，手工归一化不返回。
    ///
    /// 两者形式不一致时 `starts_with` 恒为假，表现是**工作区内的只读访问
    /// 每次都要审批**——功能没坏，但规则悄悄失效了。这条在 Linux 上测不出来。
    #[test]
    fn windows_extended_path_prefix_does_not_break_boundary_check() {
        // 一个真实存在的目录（canonicalize 会成功，因而带 `\\?\`）
        let base = std::env::temp_dir();
        // 一个**不存在**的路径（canonicalize 会失败，因而走手工归一化）
        let not_yet = base.join("yunxi-not-yet-created.md");
        assert!(!not_yet.exists(), "这条测试需要一个不存在的路径");

        assert!(
            !manual_normalize(&base)
                .to_string_lossy()
                .starts_with(r"\\?\"),
            "手工归一化不该产生扩展长度前缀"
        );

        let c = ToolContext::new(&base, SandboxMode::WorkspaceWrite);
        assert!(
            c.inside_cwd(&not_yet),
            "不存在的路径落在工作区内也要判对（这正是 write_file 的场景）"
        );
    }

    #[test]
    fn extended_prefix_stripping_handles_unc() {
        // UNC 路径的形式是 \\?\UNC\server\share，剥掉后要还原成 \\server\share
        assert_eq!(
            strip_extended_prefix(PathBuf::from(r"\\?\UNC\srv\share\a.md")),
            PathBuf::from(r"\\srv\share\a.md")
        );
        assert_eq!(
            strip_extended_prefix(PathBuf::from(r"\\?\D:\x\a.md")),
            PathBuf::from(r"D:\x\a.md")
        );
        // 普通路径原样返回
        assert_eq!(
            strip_extended_prefix(PathBuf::from(r"D:\x\a.md")),
            PathBuf::from(r"D:\x\a.md")
        );
    }

    #[test]
    fn note_read_and_has_read_round_trip() {
        // edit_file 靠它实现"先读后写"
        let mut c = ctx();
        let p = c.cwd.join("a.md");
        assert!(!c.has_read(&p));
        c.note_read(&p);
        assert!(c.has_read(&p));
    }

    // ---- 错误 ----

    #[test]
    fn tool_failure_is_a_normal_result_not_an_abort() {
        // 工具失败要能被回灌给模型，让模型换条路——而不是让任务失败
        let t = FakeTool::new("x", Capability::ReadOnly).failing();
        let err = t.call(&json!({}), &mut ctx()).unwrap_err();
        assert!(matches!(err, ToolError::Failed { .. }));
        assert!(err.to_string().contains("测试失败"));
    }

    #[test]
    fn error_messages_are_distinguishable() {
        // 这四种失败的处理方式完全不同：重试/换路/放弃/报错
        let errs = [
            ToolError::UnknownTool("x".into()),
            ToolError::BadArgs {
                detail: "缺 path".into(),
            },
            ToolError::Denied {
                reason: "黑名单".into(),
            },
            ToolError::Failed {
                detail: "超时".into(),
            },
        ];
        let msgs: Vec<String> = errs.iter().map(|e| e.to_string()).collect();
        for m in &msgs {
            assert!(!m.is_empty());
        }
        let uniq: BTreeSet<&String> = msgs.iter().collect();
        assert_eq!(uniq.len(), msgs.len(), "四种错误的说明不该重复");
    }
}
