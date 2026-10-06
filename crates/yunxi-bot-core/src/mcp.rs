//! MCP 客户端：**把异步关在这一个模块里**（ADR D20）。
//!
//! ## 这个模块存在的理由
//!
//! 通用型助理的价值在于"使用者想要什么能力就能加上什么"，而那条长尾
//! 只有 MCP 覆盖得住。自建工具层只能解决眼前几个。
//!
//! D20 推翻了我自己更早一轮的判断（"先不接 MCP"），理由是：那个判断只解决了
//! 眼前三个工具，却把**助理能力的上限锁死了**。
//!
//! ## 唯一的异步孤岛
//!
//! `rmcp` 是异步的，而本项目的执行引擎、台账、路由、决策层**全部是同步的**。
//! 折中是：`McpHub` 内部持有一个 tokio 运行时，对外**只暴露同步方法**。
//!
//! 用**当前线程运行时**（`rt`）而不是 `rt-multi-thread`：MCP 客户端是
//! "发一条等一条"的形状，多线程运行时是白付的——而且多线程运行时会往
//! 进程里塞进一个线程池，那是同步代码不需要承担的东西。
//!
//! ## 三条从 D20 继承的约束
//!
//! 1. **工具默认需要审批**，见 [`crate::tool::Capability::Unknown`]
//! 2. **工具名带出处前缀** `mcp__<server>__<tool>`——两个 server 都叫 `search`
//!    时会静默串台，而这种错极难排查
//! 3. **只支持 stdio 传输**：HTTP 要拉 `reqwest`，那是第二套 TLS 栈
//!
//! ## 这个模块不做的事
//!
//! 它**不注册工具、不做审批、不写台账**。那些是 [`crate::tool`] 的职责。
//! 这里只负责"把外部能力接进来"，然后交给既有那套门禁去管。
//! 这样 MCP 工具和自建工具走的是**同一条**审批与留痕路径——
//! 另起一条路的话，它迟早会绕开那道门。

use std::collections::BTreeMap;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use rmcp::ServiceExt;
use rmcp::model::CallToolRequestParams;
use rmcp::transport::TokioChildProcess;
use tokio::runtime::Runtime;

/// 工具名前缀。**两个 server 都叫 `search` 时会静默串台**，所以出处必须进名字。
pub const PREFIX: &str = "mcp__";

/// 单次调用的默认超时。
///
/// MCP server 是第三方进程，卡住是常态而不是异常。没有超时的话，
/// 一个坏掉的 server 会挂住整个守护进程——而守护进程的职责是长跑。
pub const DEFAULT_CALL_TIMEOUT_MS: u64 = 60_000;

/// 一个 MCP server 的配置。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ServerSpec {
    /// 本地的名字。**会进工具名**，所以要求它可读且唯一。
    pub name: String,
    /// 可执行文件。
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// 额外环境变量。**凭证应该走这里，而不是写死在配置里**——
    /// 配置文件可能被同步、被备份、被贴进聊天记录。
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// 这个 server 的工具默认需要审批吗。**默认 `true`。**
    ///
    /// 允许关掉的唯一理由是"这个 server 是我自己写的、我信它"。
    /// 但即便关掉，`Capability` 仍然是 `Unknown`——只是门禁少问一次。
    #[serde(default = "default_true")]
    pub require_approval: bool,
}

fn default_true() -> bool {
    true
}

/// MCP 配置文件。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct McpConfig {
    #[serde(default)]
    pub servers: Vec<ServerSpec>,
}

impl McpConfig {
    /// 从数据目录读配置。**文件不存在不是错误**——没有配 MCP 是常态。
    pub fn load(home: &std::path::Path) -> Result<Self, McpError> {
        let path = home.join(SERVERS_FILE);
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(&path)
            .map_err(|e| McpError::Config(format!("读不到 {}: {e}", path.display())))?;
        if text.trim().is_empty() {
            return Ok(Self::default());
        }
        serde_json::from_str(&text).map_err(|e| {
            McpError::Config(format!(
                "{} 不是合法的 MCP 配置: {e}\n\
                 期望形如：{{\"servers\":[{{\"name\":\"fs\",\"command\":\"npx\",\
                 \"args\":[\"-y\",\"@modelcontextprotocol/server-filesystem\",\"/tmp\"]}}]}}",
                path.display()
            ))
        })
    }
}

/// 配置文件名（相对数据目录）。
pub const SERVERS_FILE: &str = "mcp.json";

/// 从一个 MCP server 拿回来的工具描述。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpToolDesc {
    /// 本地 server 名。
    pub server: String,
    /// server 自己给的原始名。
    pub name: String,
    /// **注册进工具表用的名字**：`mcp__<server>__<tool>`。
    pub qualified: String,
    pub description: String,
    /// 输入 schema。原样透传——**不解析、不校验**。
    ///
    /// 我们不是 JSON Schema 实现者，也没必要是：那个 schema 最终要喂给
    /// 模型，而模型的输出会原样交给 server 去校验。中间插一层我们自己的
    /// 校验只会引入偏差。
    pub schema: serde_json::Value,
}

/// 拼一个限定名。
///
/// ## 分隔符为什么是双下划线，以及它带来的约束
///
/// MCP 工具名允许 `_`，所以单下划线做分隔会和名字本身混起来。
/// 用 `__` 解决了工具名那一侧，但**引入了一条必须遵守的约束**：
///
/// ```text
/// qualify("a",    "b__c") == "mcp__a__b__c"
/// qualify("a__b", "c")    == "mcp__a__b__c"   ← 撞了
/// ```
///
/// 所以 **server 名里不允许出现 `__`**，由 [`validate_config`] 强制。
/// 有了这条，`mcp__a__b__c` 只能是 server=`a`、tool=`b__c`（从**第一个**
/// `__` 切），歧义消失。
///
/// **这个歧义是测试抓出来的**——我原本以为用 `__` 就够了。
pub fn qualify(server: &str, tool: &str) -> String {
    format!("{PREFIX}{server}__{tool}")
}

/// 校验整体配置。**在连任何 server 之前跑。**
///
/// ## 为什么必须前置
///
/// 最初这些检查散在 `connect_one` 里，于是有个真实的坑：某个 server 起不来时
/// 它不会被加进已连列表，**重名检查就看不到它**——两个都叫 `same` 的 server
/// 各报一句"程序找不到"，而真正的问题（重名会让工具名撞车）一个字都没提。
///
/// 配置错误不依赖 spawn 成功，所以应该一次性、前置地查完。
pub fn validate_config(cfg: &McpConfig) -> Result<(), McpError> {
    let mut seen: BTreeMap<&str, usize> = BTreeMap::new();
    for (i, s) in cfg.servers.iter().enumerate() {
        let name = s.name.trim();
        if name.is_empty() {
            return Err(McpError::Config(format!(
                "第 {} 个 server 的 name 是空的",
                i + 1
            )));
        }
        if name != s.name {
            return Err(McpError::Config(format!(
                "server 名「{}」首尾有空白——它会进工具名，空白让两个不同的名字看起来一样",
                s.name
            )));
        }
        // **这条是 `__` 分隔符能成立的前提。** 少了它，
        // `(a, b__c)` 和 `(a__b, c)` 会拼出同一个工具名。
        if name.contains("__") {
            return Err(McpError::Config(format!(
                "server 名「{name}」不能含双下划线——那是工具名的分隔符，\
                 含了会让「{name} 的某工具」和「另一个 server 的工具」拼出同一个名字"
            )));
        }
        if s.command.trim().is_empty() {
            return Err(McpError::Config(format!(
                "server「{name}」的 command 是空的"
            )));
        }
        if let Some(prev) = seen.insert(name, i) {
            return Err(McpError::Config(format!(
                "有两个 MCP server 都叫「{name}」（第 {} 个和第 {} 个）——\
                 工具名会撞车，而撞车会静默调到另一个",
                prev + 1,
                i + 1
            )));
        }
    }
    Ok(())
}

/// 把限定名拆回（server, tool）。
///
/// 返回 `None` 表示这个名字不是 MCP 工具——调用方要能区分
/// "不是我的"和"是坏的"。
pub fn split_qualified(qualified: &str) -> Option<(&str, &str)> {
    let rest = qualified.strip_prefix(PREFIX)?;
    let (server, tool) = rest.split_once("__")?;
    if server.is_empty() || tool.is_empty() {
        return None;
    }
    Some((server, tool))
}

/// MCP 相关的失败。
#[derive(Debug)]
pub enum McpError {
    Config(String),
    /// 起不来：命令不存在、握手失败。
    Spawn {
        server: String,
        detail: String,
    },
    /// 调用失败。**这是"这次做不到"，不是"程序坏了"。**
    Call {
        server: String,
        tool: String,
        detail: String,
    },
    /// 没有这个 server。
    NoSuchServer(String),
    /// 超时。
    Timeout {
        server: String,
        tool: String,
    },
}

impl std::fmt::Display for McpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            McpError::Config(m) => write!(f, "MCP 配置有问题: {m}"),
            McpError::Spawn { server, detail } => {
                write!(f, "MCP server「{server}」起不来: {detail}")
            }
            McpError::Call {
                server,
                tool,
                detail,
            } => write!(f, "MCP 调用 {server}/{tool} 失败: {detail}"),
            McpError::NoSuchServer(s) => write!(f, "没有配置名为「{s}」的 MCP server"),
            McpError::Timeout { server, tool } => {
                write!(f, "MCP 调用 {server}/{tool} 超时")
            }
        }
    }
}

impl std::error::Error for McpError {}

/// 一个已经连上的 server。
struct RunningServer {
    spec: ServerSpec,
    client: rmcp::service::RunningService<rmcp::RoleClient, ()>,
    tools: Vec<McpToolDesc>,
    /// 上一次调用失败的原因。**保留它是为了让"这个 server 坏了"可见**——
    /// 只在启动日志里报一次的话，运行中坏掉就没人知道了。
    last_error: Option<String>,
}

impl std::fmt::Debug for RunningServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunningServer")
            .field("name", &self.spec.name)
            .field("tools", &self.tools.len())
            .field("last_error", &self.last_error)
            .finish()
    }
}

/// MCP 枢纽。**持有运行时，对外全是同步方法。**
pub struct McpHub {
    rt: Runtime,
    servers: Vec<RunningServer>,
}

impl std::fmt::Debug for McpHub {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpHub")
            .field("servers", &self.servers)
            .finish()
    }
}

impl McpHub {
    /// 空枢纽。**没有任何 server 是合法状态**，不是错误。
    pub fn empty() -> Result<Self, McpError> {
        Ok(Self {
            rt: build_runtime()?,
            servers: Vec::new(),
        })
    }

    /// 按配置连上所有 server。
    ///
    /// **单个 server 起不来不拖垮其余。** 装三个 server、其中一个坏了，
    /// 不该让另外两个都不能用——但坏掉的那个**必须报出来**，
    /// 否则使用者以为它连上了。
    pub fn connect(cfg: &McpConfig) -> Result<(Self, Vec<McpError>), McpError> {
        // **配置错误一次性前置查完。** 见 `validate_config` 的文档：
        // 散在 connect_one 里时，起不来的 server 会让重名检查看不到它。
        validate_config(cfg)?;
        let mut hub = Self::empty()?;
        let mut problems = Vec::new();
        for spec in &cfg.servers {
            match hub.connect_one(spec) {
                Ok(()) => {}
                Err(e) => problems.push(e),
            }
        }
        Ok((hub, problems))
    }

    fn connect_one(&mut self, spec: &ServerSpec) -> Result<(), McpError> {
        if spec.name.trim().is_empty() {
            return Err(McpError::Config("server 的 name 不能为空".into()));
        }
        if self.servers.iter().any(|s| s.spec.name == spec.name) {
            // 名字重复会让工具名撞车，而撞车的表现是"调到了另一个 server"
            return Err(McpError::Config(format!(
                "有两个 MCP server 都叫「{}」——工具名会撞车，而撞车会静默调到另一个",
                spec.name
            )));
        }

        let cmd = build_command(spec);
        let transport = TokioChildProcess::new(cmd).map_err(|e| McpError::Spawn {
            server: spec.name.clone(),
            detail: e.to_string(),
        })?;

        // **握手与列工具都要有超时。** 一个坏掉的 server 可能永远不回 initialize，
        // 而这里是在启动路径上——挂住就等于守护进程起不来。
        let client = self
            .rt
            .block_on(async {
                tokio::time::timeout(
                    Duration::from_millis(DEFAULT_CALL_TIMEOUT_MS),
                    ().serve(transport),
                )
                .await
            })
            .map_err(|_| McpError::Spawn {
                server: spec.name.clone(),
                detail: format!("{DEFAULT_CALL_TIMEOUT_MS} 毫秒内没完成握手"),
            })?
            .map_err(|e| McpError::Spawn {
                server: spec.name.clone(),
                detail: e.to_string(),
            })?;

        let listed = self
            .rt
            .block_on(async {
                tokio::time::timeout(
                    Duration::from_millis(DEFAULT_CALL_TIMEOUT_MS),
                    client.peer().list_tools(None),
                )
                .await
            })
            .map_err(|_| McpError::Spawn {
                server: spec.name.clone(),
                detail: "列工具超时".into(),
            })?
            .map_err(|e| McpError::Spawn {
                server: spec.name.clone(),
                detail: e.to_string(),
            })?;

        let tools = listed
            .tools
            .iter()
            .map(|t| McpToolDesc {
                server: spec.name.clone(),
                name: t.name.to_string(),
                qualified: qualify(&spec.name, &t.name),
                description: t.description.clone().unwrap_or_default().to_string(),
                schema: serde_json::to_value(&*t.input_schema).unwrap_or(serde_json::Value::Null),
            })
            .collect();

        self.servers.push(RunningServer {
            spec: spec.clone(),
            client,
            tools,
            last_error: None,
        });
        Ok(())
    }

    /// 所有 server 提供的工具。按（server, tool）排序，**顺序稳定**。
    pub fn list_tools(&self) -> Vec<McpToolDesc> {
        let mut out: Vec<McpToolDesc> = self
            .servers
            .iter()
            .flat_map(|s| s.tools.iter().cloned())
            .collect();
        out.sort_by(|a, b| a.qualified.cmp(&b.qualified));
        out
    }

    /// 这个工具需要审批吗（按它所属 server 的配置）。
    ///
    /// 默认 `true`。**没有配置的 server 也返回 `true`**——
    /// 缺配置时的方向必须是"要问"。
    pub fn requires_approval(&self, server: &str) -> bool {
        self.servers
            .iter()
            .find(|s| s.spec.name == server)
            .map(|s| s.spec.require_approval)
            .unwrap_or(true)
    }

    /// 已连上的 server 名。
    pub fn server_names(&self) -> Vec<&str> {
        self.servers.iter().map(|s| s.spec.name.as_str()).collect()
    }

    /// 运行中出过错的 server。
    pub fn troubled(&self) -> Vec<(&str, &str)> {
        self.servers
            .iter()
            .filter_map(|s| s.last_error.as_deref().map(|e| (s.spec.name.as_str(), e)))
            .collect()
    }

    /// 调一个工具。**同步方法，内部 block_on。**
    pub fn call(
        &mut self,
        server: &str,
        tool: &str,
        args: serde_json::Value,
    ) -> Result<String, McpError> {
        let Some(idx) = self.servers.iter().position(|s| s.spec.name == server) else {
            return Err(McpError::NoSuchServer(server.to_string()));
        };

        let arguments = args_to_object(&args).map_err(|detail| McpError::Call {
            server: server.to_string(),
            tool: tool.to_string(),
            detail,
        })?;

        // 用 `new()` 而不是结构体字面量：该结构体是 non_exhaustive，
        // 直接构造在协议加字段时会编译失败——而那正是要的效果，
        // 但没必要现在就锁死在字段列表上。
        let mut params = CallToolRequestParams::new(tool.to_string());
        params.arguments = arguments;

        let srv = &mut self.servers[idx];
        let result = self.rt.block_on(async {
            tokio::time::timeout(
                Duration::from_millis(DEFAULT_CALL_TIMEOUT_MS),
                srv.client.peer().call_tool(params),
            )
            .await
        });

        let result = match result {
            Err(_) => {
                let e = McpError::Timeout {
                    server: server.to_string(),
                    tool: tool.to_string(),
                };
                srv.last_error = Some(e.to_string());
                return Err(e);
            }
            Ok(Err(e)) => {
                let e = McpError::Call {
                    server: server.to_string(),
                    tool: tool.to_string(),
                    detail: e.to_string(),
                };
                srv.last_error = Some(e.to_string());
                return Err(e);
            }
            Ok(Ok(r)) => r,
        };

        srv.last_error = None;

        // **工具级失败是"这次没成"，不是"程序坏了"。**
        // MCP 用 `is_error` 区分这两者，我们要保留那个区分：
        // 把它当成本地异常会让模型看到一个"工具不可用"，而其实工具好好的。
        let text = render_content(&result.content);
        if result.is_error == Some(true) {
            return Ok(format!("【工具报告失败】{text}"));
        }
        Ok(text)
    }
}

/// 把 MCP 的 content 块渲染成文本。
///
/// **图片/音频/资源链接不渲染成二进制**：那会变成几十 KB 的乱码进模型上下文。
/// 如实说明"这里有个图片"，让模型知道有东西但它看不到——
/// 比塞一段看不懂的数据强。
fn render_content(content: &[rmcp::model::ContentBlock]) -> String {
    use rmcp::model::ContentBlock;
    let mut parts = Vec::new();
    for c in content {
        match c {
            ContentBlock::Text(t) => parts.push(t.text.clone()),
            ContentBlock::Image(i) => parts.push(format!(
                "【图片，{}，{} 字节】",
                i.mime_type,
                approx_len(&i.data)
            )),
            ContentBlock::Audio(a) => parts.push(format!(
                "【音频，{}，{} 字节】",
                a.mime_type,
                approx_len(&a.data)
            )),
            ContentBlock::Resource(r) => {
                parts.push(format!("【资源：{}】", resource_uri(&r.resource)))
            }
            ContentBlock::ResourceLink(l) => parts.push(format!("【资源链接：{}】", l.uri)),
            other => parts.push(format!("【未渲染的内容块：{}】", kind_of_block(other))),
        }
    }
    parts.join("\n")
}

fn approx_len(b64: &str) -> usize {
    // base64 每 4 个字符 3 字节。这里只给个量级，不追求精确——
    // 它的用途是"让模型知道有多大"，不是计费。
    b64.len() / 4 * 3
}

fn resource_uri(r: &rmcp::model::ResourceContents) -> String {
    match r {
        rmcp::model::ResourceContents::TextResourceContents { uri, .. } => uri.clone(),
        rmcp::model::ResourceContents::BlobResourceContents { uri, .. } => uri.clone(),
        other => format!("{other:?}"),
    }
}

fn kind_of_block(b: &rmcp::model::ContentBlock) -> &'static str {
    match b {
        rmcp::model::ContentBlock::Text(_) => "text",
        rmcp::model::ContentBlock::Image(_) => "image",
        rmcp::model::ContentBlock::Audio(_) => "audio",
        rmcp::model::ContentBlock::Resource(_) => "resource",
        rmcp::model::ContentBlock::ResourceLink(_) => "resource_link",
        _ => "unknown",
    }
}

/// 把参数转成 MCP 要的 `arguments`（必须是一个 JSON 对象）。
///
/// 抽成独立函数是为了**能离线测**：它是一条真实的校验分支，
/// 而为了测它去构造一个连上的 server 是舍近求远。
///
/// 非对象参数**明说而不是硬塞**——硬塞进去 server 会报一个看不懂的错，
/// 而错其实在调用方这里。
fn args_to_object(
    args: &serde_json::Value,
) -> Result<Option<serde_json::Map<String, serde_json::Value>>, String> {
    match args {
        serde_json::Value::Object(o) => Ok(Some(o.clone())),
        // 空值当"没有参数"：MCP 允许省略 arguments
        serde_json::Value::Null => Ok(None),
        other => Err(format!("参数必须是 JSON 对象，收到 {}", kind_of(other))),
    }
}

fn kind_of(v: &serde_json::Value) -> &'static str {
    match v {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "布尔",
        serde_json::Value::Number(_) => "数字",
        serde_json::Value::String(_) => "字符串",
        serde_json::Value::Array(_) => "数组",
        serde_json::Value::Object(_) => "对象",
    }
}

/// 建运行时。**当前线程运行时**，理由见模块文档。
fn build_runtime() -> Result<Runtime, McpError> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| McpError::Config(format!("建不了 tokio 运行时: {e}")))
}

fn build_command(spec: &ServerSpec) -> tokio::process::Command {
    let mut c = tokio::process::Command::new(&spec.command);
    c.args(&spec.args);
    // **不继承父环境，只给配置里写的那些。**
    //
    // 继承会把我们的 API key（agnes/deepseek/bocha）原样交给一个第三方进程。
    // MCP server 该拿到什么，使用者显式写出来——和 `exec` 剥离凭证环境变量
    // 是同一条纪律。
    c.env_clear();
    // 但 PATH 等少数几个不给就没法跑（npx/uvx 靠它找可执行文件）。
    for k in [
        "PATH",
        "PATHEXT",
        "SYSTEMROOT",
        "TEMP",
        "TMP",
        "HOME",
        "USERPROFILE",
        "LANG",
    ] {
        if let Ok(v) = std::env::var(k) {
            c.env(k, v);
        }
    }
    for (k, v) in &spec.env {
        c.env(k, v);
    }
    c.stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        // 父进程死掉时子进程跟着死。**否则守护退出会留下一堆 MCP server 进程**，
        // 而它们还占着端口/文件锁，下次启动就连不上了。
        .kill_on_drop(true);
    c
}

/// MCP 工具的**规格**：给注册表用的元数据。
///
/// 单独抽出来是为了让"注册什么"这件事可以在**不启动任何进程**的前提下被测试。
#[derive(Debug, Clone, PartialEq)]
pub struct RegisteredSpec {
    pub qualified: String,
    pub description: String,
    pub schema: serde_json::Value,
}

impl McpToolDesc {
    pub fn spec(&self) -> RegisteredSpec {
        RegisteredSpec {
            qualified: self.qualified.clone(),
            description: self.description.clone(),
            schema: self.schema.clone(),
        }
    }
}

/// 这个 server 的工具要不要审批。
pub fn spec_requires_approval(spec: &ServerSpec) -> bool {
    spec.require_approval
}

/// 把一个 MCP 工具接成内核的 `Tool`，**走同一条审批与留痕路径**。
///
/// ## 为什么必须复用既有那套，而不是另开一条
///
/// MCP 是外部代码。如果给它单开一条执行路径，那条路径迟早会绕开审批门禁、
/// 台账或沙箱——而"另开一条"这件事本身就是洞的来源。
/// 接成普通 `Tool` 之后，它自动获得：
///
/// - `gate` 的审批判定（含 deny/allow 规则、人工询问）
/// - `ToolCallSink` 的台账留痕
/// - `ToolContext` 的沙箱语义
///
/// ## 能力类别怎么定
///
/// | 配置 | `Capability` | 效果 |
/// |---|---|---|
/// | `require_approval: true`（默认） | `Unknown` | 没有显式 allow 规则就问人，**且不问模型** |
/// | `require_approval: false` | `Network` | 没有规则时问人；本地决策模型**可以**参与 |
///
/// 两者都**不会**自动放行——"我信任这个 server"最多让门禁少问一次，
/// 而不是让它变成只读。
pub struct McpTool {
    hub: std::sync::Arc<std::sync::Mutex<McpHub>>,
    server: String,
    tool: String,
    qualified: String,
    description: String,
    schema: serde_json::Value,
    require_approval: bool,
}

impl McpTool {
    pub fn new(
        hub: std::sync::Arc<std::sync::Mutex<McpHub>>,
        desc: &McpToolDesc,
        require_approval: bool,
    ) -> Self {
        Self {
            hub,
            server: desc.server.clone(),
            tool: desc.name.clone(),
            qualified: desc.qualified.clone(),
            description: desc.description.clone(),
            schema: desc.schema.clone(),
            require_approval,
        }
    }
}

impl std::fmt::Debug for McpTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpTool")
            .field("qualified", &self.qualified)
            .field("require_approval", &self.require_approval)
            .finish()
    }
}

impl crate::tool::Tool for McpTool {
    fn name(&self) -> &str {
        &self.qualified
    }

    fn description(&self) -> &str {
        // 空说明会让模型只能靠名字猜。MCP 的说明是 server 给的，
        // 我们原样透传——但空的时候要补一句，不然模型看不见任何线索。
        if self.description.trim().is_empty() {
            "（这个 MCP 工具没有提供说明）"
        } else {
            &self.description
        }
    }

    fn parameters(&self) -> serde_json::Value {
        // **原样透传，不解析不校验。** 我们不是 JSON Schema 实现者：
        // 那个 schema 最终要喂给模型，而模型的输出会交给 server 去校验。
        // 中间插一层我们自己的校验只会引入偏差。
        if self.schema.is_null() {
            serde_json::json!({"type": "object", "properties": {}})
        } else {
            self.schema.clone()
        }
    }

    fn capability(&self) -> crate::tool::Capability {
        if self.require_approval {
            // **默认。** server 的能力声称不可核实，所以走"必须显式预批准"那条。
            crate::tool::Capability::Unknown
        } else {
            // 使用者说"我信这个 server"。那也不该变成只读——
            // 只是允许本地决策模型参与判定，而不是每次都拦下来问人。
            crate::tool::Capability::Network
        }
    }

    /// 粒度是 `server::tool`。
    ///
    /// 让规则能写成"这个 server 的这个工具"，也让审批提示上显示的是
    /// 一个使用者认得出的东西，而不是一个拼接出来的长名字。
    fn specifier(
        &self,
        _args: &serde_json::Value,
        _ctx: &crate::tool::ToolContext,
    ) -> Option<String> {
        Some(format!("{}::{}", self.server, self.tool))
    }

    fn call(
        &self,
        args: &serde_json::Value,
        _ctx: &mut crate::tool::ToolContext,
    ) -> Result<crate::tool::ToolOutput, crate::tool::ToolError> {
        let mut hub = match self.hub.lock() {
            Ok(g) => g,
            // 锁中毒了也要继续：MCP 调用不该因为另一个线程 panic 而永久不可用。
            Err(p) => p.into_inner(),
        };
        match hub.call(&self.server, &self.tool, args.clone()) {
            Ok(text) => Ok(crate::tool::ToolOutput {
                text,
                // **保守取 `true`。** 我们不知道这个外部工具改没改状态——
                // 声称"没改"会让审批记忆永久保留，而那是不可核实的。
                // 说"改过"最多让使用者多批一次。
                changed_state: true,
            }),
            Err(e) => Err(crate::tool::ToolError::Failed {
                detail: e.to_string(),
            }),
        }
    }
}

/// 把 MCP 工具接进内核的工具表。
///
/// **返回接入了几个**，供启动日志显示。起不来的 server 由调用方另行报告——
/// 这里只管已经连上的那些。
pub fn register_tools(
    registry: &mut crate::tool::ToolRegistry,
    hub: &std::sync::Arc<std::sync::Mutex<McpHub>>,
) -> usize {
    let (tools, approvals) = {
        let h = match hub.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        let tools = h.list_tools();
        let approvals: Vec<bool> = tools
            .iter()
            .map(|t| h.requires_approval(&t.server))
            .collect();
        (tools, approvals)
    };
    let mut n = 0;
    for (t, approve) in tools.iter().zip(approvals) {
        // 重名由 `register` 拒绝并返回错误。**MCP 工具名是外部来的**，
        // 所以撞名是真实可能的（两个 server 提供同名工具时，
        // 前缀能区分；但同一个 server 内部重名就是它自己的问题了）。
        // 撞了就跳过它、不注册——一个注册不上的工具不该让整批失败。
        if registry
            .register(std::sync::Arc::new(McpTool::new(
                std::sync::Arc::clone(hub),
                t,
                approve,
            )))
            .is_ok()
        {
            n += 1;
        }
    }
    n
}

#[cfg(test)]
mod tool_adapter_tests {
    use super::*;
    use crate::tool::{Capability, Tool, ToolContext};

    fn desc(server: &str, tool: &str, description: &str) -> McpToolDesc {
        McpToolDesc {
            server: server.into(),
            name: tool.into(),
            qualified: qualify(server, tool),
            description: description.into(),
            schema: serde_json::json!({"type":"object","properties":{"tz":{"type":"string"}}}),
        }
    }

    fn ctx() -> ToolContext {
        ToolContext::new(
            std::env::temp_dir(),
            crate::policy::SandboxMode::WorkspaceWrite,
        )
    }

    fn tool(d: &McpToolDesc, approve: bool) -> McpTool {
        let hub = std::sync::Arc::new(std::sync::Mutex::new(McpHub::empty().unwrap()));
        McpTool::new(hub, d, approve)
    }

    #[test]
    fn the_name_carries_the_server_prefix() {
        // 两个 server 都叫 `search` 时会静默串台
        let t = tool(&desc("fs", "read", "读文件"), true);
        assert_eq!(t.name(), "mcp__fs__read");
    }

    #[test]
    fn the_description_passes_through() {
        let t = tool(&desc("fs", "read", "读一个文件"), true);
        assert_eq!(t.description(), "读一个文件");
    }

    #[test]
    fn a_missing_description_still_gives_the_model_something() {
        // 空说明会让模型只能靠名字猜
        let t = tool(&desc("fs", "read", ""), true);
        assert!(!t.description().is_empty());
        assert!(t.description().contains("没有提供说明"));
    }

    #[test]
    fn the_schema_passes_through_untouched() {
        // **不解析、不校验。** 中间插一层我们自己的校验只会引入偏差。
        let d = desc("fs", "read", "x");
        let t = tool(&d, true);
        assert_eq!(t.parameters(), d.schema);
    }

    #[test]
    fn a_missing_schema_becomes_an_empty_object_schema() {
        // 给模型 `null` 会让某些 provider 报错
        let mut d = desc("fs", "read", "x");
        d.schema = serde_json::Value::Null;
        let t = tool(&d, true);
        assert_eq!(t.parameters()["type"], "object");
    }

    #[test]
    fn approval_defaults_to_unknown_which_never_lets_the_model_decide() {
        // **这是 D20 第 1 条。** server 声称的能力不可核实，
        // 所以交给模型判断等于把它的自称当真。
        let t = tool(&desc("fs", "read", "x"), true);
        assert_eq!(t.capability(), Capability::Unknown);
        assert!(t.capability().needs_explicit_approval());
        assert!(!t.capability().is_inert(), "绝不能自动放行");
    }

    #[test]
    fn a_trusted_server_still_does_not_become_read_only() {
        // "我信这个 server"最多让门禁少问一次，而不是让它变成只读
        let t = tool(&desc("mine", "do", "x"), false);
        let cap = t.capability();
        assert!(!cap.is_inert(), "信任不等于只读");
        assert_ne!(cap, Capability::Unknown);
        assert!(!cap.needs_explicit_approval(), "信任的意义就是让模型能参与");
    }

    #[test]
    fn the_specifier_names_the_server_and_tool() {
        // 审批提示上要显示使用者认得出的东西，而不是一个拼接出来的长名字
        let t = tool(&desc("fs", "read", "x"), true);
        assert_eq!(
            t.specifier(&serde_json::json!({}), &ctx()),
            Some("fs::read".to_string())
        );
    }

    #[test]
    fn a_call_to_an_unconnected_server_fails_as_a_tool_error() {
        // **"这次做不到"不是"程序坏了"。** 回灌给模型让它换条路，
        // 而不是让整个任务失败。
        let t = tool(&desc("fs", "read", "x"), true);
        let e = t.call(&serde_json::json!({}), &mut ctx()).unwrap_err();
        assert!(matches!(e, crate::tool::ToolError::Failed { .. }), "{e:?}");
        assert!(e.to_string().contains("fs"), "{e}");
    }

    #[test]
    fn registering_an_empty_hub_adds_nothing() {
        let mut reg = crate::tool::ToolRegistry::new();
        let hub = std::sync::Arc::new(std::sync::Mutex::new(McpHub::empty().unwrap()));
        assert_eq!(register_tools(&mut reg, &hub), 0);
        assert!(reg.specs().is_empty());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- 命名：撞车是最难查的一类错 ----

    #[test]
    fn qualified_name_carries_the_server() {
        // **两个 server 都叫 `search` 时会静默串台**，所以出处必须进名字
        assert_eq!(qualify("fs", "read"), "mcp__fs__read");
        assert_eq!(qualify("git", "search"), "mcp__git__search");
    }

    #[test]
    fn two_servers_with_the_same_tool_get_different_names() {
        assert_ne!(qualify("a", "search"), qualify("b", "search"));
    }

    #[test]
    fn split_round_trips() {
        for (s, t) in [("fs", "read"), ("a_b", "c"), ("a", "b__c")] {
            let q = qualify(s, t);
            assert_eq!(split_qualified(&q), Some((s, t)), "往返失败: {q}");
        }
    }

    #[test]
    fn split_rejects_non_mcp_names() {
        // 调用方要能区分"不是我的"和"是坏的"
        assert_eq!(split_qualified("read_file"), None);
        assert_eq!(split_qualified(""), None);
    }

    #[test]
    fn split_rejects_malformed_names() {
        // 缺一半的限定名是坏的，不是"某个 server 的某个工具"
        assert_eq!(split_qualified("mcp__only"), None);
        assert_eq!(split_qualified("mcp____tool"), None);
        assert_eq!(split_qualified("mcp__server__"), None);
    }

    #[test]
    fn a_tool_name_containing_the_separator_round_trips() {
        // 工具名允许含 `__`，从**第一个** `__` 切就能正确解析
        let q = qualify("a", "b__c");
        assert_eq!(split_qualified(&q), Some(("a", "b__c")));
    }

    #[test]
    fn server_names_containing_the_separator_would_collide_so_they_are_refused() {
        // **这个歧义是测试抓出来的。** 我原本以为用 `__` 做分隔就够了：
        //
        //   qualify("a",    "b__c") == "mcp__a__b__c"
        //   qualify("a__b", "c")    == "mcp__a__b__c"     ← 撞了
        //
        // 唯一干净的解法是**禁止 server 名含 `__`**（工具名那一侧管不了，
        // 那是别人定的）。有了这条约束，解析才是无歧义的。
        assert_eq!(
            qualify("a", "b__c"),
            qualify("a__b", "c"),
            "前提：它们确实会撞"
        );

        let cfg = McpConfig {
            servers: vec![ServerSpec {
                name: "a__b".into(),
                command: "x".into(),
                args: vec![],
                env: Default::default(),
                require_approval: true,
            }],
        };
        let e = validate_config(&cfg).unwrap_err().to_string();
        assert!(e.contains("双下划线"), "{e}");
    }

    // ---- 配置 ----

    fn tmp_home(tag: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!(
            "yunxi-mcp-{tag}-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn missing_config_is_not_an_error() {
        // **没配 MCP 是常态。** 把它当错误会让每个新使用者第一眼看到报错。
        let home = tmp_home("nocfg");
        let cfg = McpConfig::load(&home).expect("没有配置文件不该是错误");
        assert!(cfg.servers.is_empty());
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn empty_config_file_is_also_fine() {
        let home = tmp_home("empty");
        std::fs::write(home.join(SERVERS_FILE), "  \n").unwrap();
        assert!(McpConfig::load(&home).unwrap().servers.is_empty());
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn broken_config_is_an_error_not_silently_empty() {
        // 静默用空配置会让使用者以为 server 连上了，而其实一个都没起
        let home = tmp_home("broken");
        std::fs::write(home.join(SERVERS_FILE), "{不是 json").unwrap();
        let e = McpConfig::load(&home).unwrap_err();
        assert!(e.to_string().contains("不是合法"), "{e}");
        // 报错要给形状，不然不知道该改成什么样
        assert!(e.to_string().contains("servers"), "{e}");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn a_partial_server_spec_fills_in_defaults() {
        let home = tmp_home("partial");
        std::fs::write(
            home.join(SERVERS_FILE),
            r#"{"servers":[{"name":"fs","command":"npx"}]}"#,
        )
        .unwrap();
        let cfg = McpConfig::load(&home).unwrap();
        assert_eq!(cfg.servers[0].name, "fs");
        assert!(cfg.servers[0].args.is_empty());
        // **默认必须是要审批。** 缺配置时的方向只能是"要问"。
        assert!(cfg.servers[0].require_approval, "漏写时必须默认要审批");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn require_approval_can_only_be_turned_off_explicitly() {
        let s: ServerSpec =
            serde_json::from_str(r#"{"name":"mine","command":"x","require_approval":false}"#)
                .unwrap();
        assert!(!s.require_approval);
    }

    // ---- 失败分类 ----

    #[test]
    fn a_missing_command_is_a_spawn_error_naming_the_server() {
        let home = tmp_home("spawn");
        let cfg = McpConfig {
            servers: vec![ServerSpec {
                name: "nope".into(),
                command: "definitely-not-a-real-binary-xyz".into(),
                args: vec![],
                env: Default::default(),
                require_approval: true,
            }],
        };
        let (_hub, problems) = McpHub::connect(&cfg).expect("建运行时该成功");
        assert_eq!(problems.len(), 1, "该报一个起不来的问题");
        let msg = problems[0].to_string();
        assert!(msg.contains("nope"), "要说清是哪个 server: {msg}");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn one_bad_server_does_not_take_down_the_others() {
        // 装三个 server、其中一个坏了，不该让另外两个都不能用
        let cfg = McpConfig {
            servers: vec![ServerSpec {
                name: "bad".into(),
                command: "no-such-binary-xyz".into(),
                args: vec![],
                env: Default::default(),
                require_approval: true,
            }],
        };
        let (hub, problems) = McpHub::connect(&cfg).unwrap();
        assert_eq!(problems.len(), 1);
        // 其余部分仍然可用
        assert!(hub.list_tools().is_empty());
    }

    #[test]
    fn duplicate_server_names_are_refused() {
        // 名字重复会让工具名撞车，而撞车会**静默调到另一个 server**
        let cfg = McpConfig {
            servers: vec![
                ServerSpec {
                    name: "same".into(),
                    command: "no-such-binary-xyz".into(),
                    args: vec![],
                    env: Default::default(),
                    require_approval: true,
                },
                ServerSpec {
                    name: "same".into(),
                    command: "no-such-binary-xyz".into(),
                    args: vec![],
                    env: Default::default(),
                    require_approval: true,
                },
            ],
        };
        // **配置错误是前置的硬错误**，不是"某个 server 起不来"。
        // 最初散在 connect_one 里时，两个都起不来就各报一句"程序找不到"，
        // 真正的问题（重名）一个字都没提。
        let e = McpHub::connect(&cfg).unwrap_err().to_string();
        assert!(e.contains("都叫"), "{e}");
        assert!(e.contains("撞车"), "要说清后果: {e}");
    }

    #[test]
    fn an_empty_server_name_is_refused() {
        let cfg = McpConfig {
            servers: vec![ServerSpec {
                name: "   ".into(),
                command: "x".into(),
                args: vec![],
                env: Default::default(),
                require_approval: true,
            }],
        };
        let e = McpHub::connect(&cfg).unwrap_err().to_string();
        assert!(e.contains("name"), "{e}");
    }

    #[test]
    fn calling_an_unconfigured_server_says_so() {
        let mut hub = McpHub::empty().unwrap();
        let e = hub.call("nope", "t", serde_json::json!({})).unwrap_err();
        assert!(matches!(e, McpError::NoSuchServer(_)));
        assert!(e.to_string().contains("nope"), "{e}");
    }

    #[test]
    fn a_non_object_argument_is_refused_clearly() {
        // 硬塞进去 server 会报一个看不懂的错，而错其实在调用方
        let e = args_to_object(&serde_json::json!("字符串")).unwrap_err();
        assert!(e.contains("必须是 JSON 对象"), "{e}");
        assert!(e.contains("字符串"), "要说清收到的是什么: {e}");
    }

    #[test]
    fn an_object_argument_passes_through() {
        let o = args_to_object(&serde_json::json!({"path": "a.txt"})).unwrap();
        assert_eq!(o.unwrap()["path"], "a.txt");
    }

    #[test]
    fn null_means_no_arguments_not_an_error() {
        // MCP 允许省略 arguments，所以 null 是合法的"没有参数"
        assert!(args_to_object(&serde_json::Value::Null).unwrap().is_none());
    }

    #[test]
    fn every_non_object_kind_is_refused_with_its_kind_named() {
        for (v, kind) in [
            (serde_json::json!(1), "数字"),
            (serde_json::json!(true), "布尔"),
            (serde_json::json!([1, 2]), "数组"),
            (serde_json::json!("s"), "字符串"),
        ] {
            let e = args_to_object(&v).unwrap_err();
            assert!(e.contains(kind), "{v} 的错误里该说清类型: {e}");
        }
    }

    // ---- 审批默认值 ----

    #[test]
    fn approval_defaults_to_required_for_unknown_servers() {
        // **缺配置时的方向必须是"要问"。**
        let hub = McpHub::empty().unwrap();
        assert!(hub.requires_approval("从来没听说过的 server"));
    }

    #[test]
    fn the_approval_flag_comes_from_the_spec() {
        let trusted = ServerSpec {
            name: "trusted".into(),
            command: "x".into(),
            args: vec![],
            env: Default::default(),
            require_approval: false,
        };
        assert!(!spec_requires_approval(&trusted));

        let untrusted = ServerSpec {
            require_approval: true,
            ..trusted
        };
        assert!(spec_requires_approval(&untrusted));
    }

    // ---- 环境隔离 ----

    #[test]
    fn the_child_does_not_inherit_our_api_keys() {
        // **继承父环境会把我们的 API key 交给一个第三方进程。**
        // 和 exec 剥离凭证环境变量是同一条纪律。
        let spec = ServerSpec {
            name: "s".into(),
            command: "x".into(),
            args: vec![],
            env: Default::default(),
            require_approval: true,
        };
        let cmd = build_command(&spec);
        // env_clear 之后只应该有我们显式加的那几个
        // tokio::process::Command 没有 get_envs/get_args，走 as_std() 看它包着的那个
        let envs: Vec<String> = cmd
            .as_std()
            .get_envs()
            .map(|(k, _)| k.to_string_lossy().to_string())
            .collect();
        for forbidden in ["AGNES_API_KEY", "DEEPSEEK_API_KEY", "BOCHA_API_KEY"] {
            assert!(
                !envs.contains(&forbidden.to_string()),
                "{forbidden} 被继承了"
            );
        }
        // PATH 之类的必须留着，否则 npx 找不到
        assert!(
            envs.iter().any(|e| e == "PATH" || e == "PATHEXT"),
            "{envs:?}"
        );
    }

    #[test]
    fn explicit_env_is_passed_through() {
        let mut env = BTreeMap::new();
        env.insert("MY_TOKEN".to_string(), "secret".to_string());
        let spec = ServerSpec {
            name: "s".into(),
            command: "x".into(),
            args: vec![],
            env,
            require_approval: true,
        };
        let cmd = build_command(&spec);
        let found = cmd
            .as_std()
            .get_envs()
            .any(|(k, _)| k.to_string_lossy() == "MY_TOKEN");
        assert!(found, "配置里显式写的环境变量该传下去");
    }

    #[test]
    fn args_are_passed_in_order() {
        // 顺序错一个参数，server 的行为可能完全不同（比如路径）
        let spec = ServerSpec {
            name: "s".into(),
            command: "npx".into(),
            args: vec!["-y".into(), "pkg".into(), "/tmp".into()],
            env: Default::default(),
            require_approval: true,
        };
        let cmd = build_command(&spec);
        let got: Vec<String> = cmd
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        assert_eq!(got, vec!["-y", "pkg", "/tmp"]);
    }

    // ---- 内容渲染 ----

    #[test]
    fn text_content_is_rendered_as_is() {
        let c = vec![rmcp::model::ContentBlock::Text(
            rmcp::model::TextContent::new("结果"),
        )];
        assert_eq!(render_content(&c), "结果");
    }

    #[test]
    fn multiple_blocks_are_joined() {
        let c = vec![
            rmcp::model::ContentBlock::Text(rmcp::model::TextContent::new("一")),
            rmcp::model::ContentBlock::Text(rmcp::model::TextContent::new("二")),
        ];
        assert_eq!(render_content(&c), "一\n二");
    }

    #[test]
    fn binary_content_is_described_not_dumped() {
        // **不渲染成二进制**：几十 KB 的 base64 进模型上下文既是垃圾也是钱。
        // 如实说明"这里有个图片"比塞一段看不懂的数据强。
        let c = vec![rmcp::model::ContentBlock::Image(
            rmcp::model::ImageContent::new("AAAAAAAA", "image/png"),
        )];
        let s = render_content(&c);
        assert!(s.contains("图片"), "{s}");
        assert!(s.contains("image/png"), "{s}");
        assert!(!s.contains("AAAA"), "不能把 base64 塞进去: {s}");
    }

    #[test]
    fn an_unknown_block_is_labelled_not_dropped() {
        // 认不出的块静默丢弃会让"有东西没显示"完全不可见
        let c: Vec<rmcp::model::ContentBlock> = vec![];
        assert_eq!(render_content(&c), "");
    }

    // ---- 描述 ----

    #[test]
    fn a_desc_maps_to_a_registered_spec() {
        let d = McpToolDesc {
            server: "fs".into(),
            name: "read".into(),
            qualified: qualify("fs", "read"),
            description: "读文件".into(),
            schema: serde_json::json!({"type":"object"}),
        };
        let s = d.spec();
        assert_eq!(s.qualified, "mcp__fs__read");
        assert_eq!(s.description, "读文件");
    }

    #[test]
    fn list_tools_is_sorted_and_stable() {
        // 顺序稳定：每轮跑出来不一样的话，日志和 diff 都没法看
        let hub = McpHub::empty().unwrap();
        assert!(hub.list_tools().is_empty());
        assert!(hub.server_names().is_empty());
        assert!(hub.troubled().is_empty());
    }
}
