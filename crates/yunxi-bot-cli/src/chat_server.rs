//! 原生聊天窗口的本地 stdio 服务。
//!
//! `yunxi-bot chat-server` 由 macOS 原生窗口（`云熙聊天.app`）拉起，
//! 用**逐行 JSON** 通信。它是同一个 [`ChatHandler`] 的另一个前端：
//! 人格、记忆、会话、工具审批、台账都走 `chat` 这条链路——
//! 不另起一条绕开审批的轻量通道。
//!
//! ## 协议（每行一个 JSON 对象）
//!
//! 服务 → 窗口：
//! ```text
//! {"type":"ready","session":"chat-...","turns":3,"resumed":true}
//! {"type":"sessions","items":[{"id":"...","preview":"...","turns":2,"updated_at":...}]}
//! {"type":"history","messages":[{"role":"user","text":"..."}]}
//! {"type":"opened","session":"...","turns":2}
//! {"type":"delta","text":"…"}
//! {"type":"route","model":"deepseek-flash","thinking":false}
//! {"type":"approval","id":1,"tool":"write_file",...}
//! {"type":"approval_expired","id":1}
//! {"type":"reply","text":"…","session":"…","turns":4}
//! {"type":"error","message":"…"}
//! {"type":"pong"}
//! ```
//!
//! 窗口 → 服务：
//! ```text
//! {"type":"chat","text":"在吗？"}
//! {"type":"approval","id":1,"allow":true}
//! {"type":"new"}
//! {"type":"list_sessions"}
//! {"type":"open","session":"chat-..."}
//! {"type":"effort","value":"auto|on|off"}
//! {"type":"ping"}
//! ```
//!
//! stdout 只留给协议；日志一律走 stderr，否则窗口会把日志当成消息。
//!
//! 读线程负责 stdin：审批决定直接交给 [`ApprovalBridge`]（chat_turn 阻塞时
//! 也要能送达），其它请求进 channel 给主循环。这样审批不会被主循环卡死。

use std::io::{BufRead, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::time::Duration;

use serde_json::{Value, json};
use yunxi_bot_core::costlog::{CallRecord, project_costs};
use yunxi_bot_core::decide::Decider;
use yunxi_bot_core::memory::Memory;
use yunxi_bot_core::think::context::ContextBudget;
use yunxi_bot_core::think::router::TaskKind;
use yunxi_bot_core::think::session::SessionStore;
use yunxi_bot_core::think::{ModelSpec, ReasoningEffort};
use yunxi_bot_core::tool::ToolDescription;
use yunxi_bot_core::tool::runner::{Approval, ApprovalRequest, Approver};
use yunxi_bot_core::{EventKind, Ledger};

use crate::chat_handler::ChatHandler;
use crate::model_adapter::ModelAdapter;
use crate::task_classifier::{InputKind, TaskClassifier, strip_command};

/// 由 App 传入 `--auto-local` 时，启动本机陪伴模型并等待健康。
///
/// 只负责“拉起来并等端口”，不替 ChatHandler 做路由；模型缺失/启动失败
/// 只打印原因，随后照常 ready，聊天会给出明确的“本机未启动”错误。
struct LocalModelGuard(Option<std::process::Child>);

impl Drop for LocalModelGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn local_idle_unload_seconds() -> u64 {
    std::env::var("YUNXI_LOCAL_IDLE_UNLOAD")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|v| *v == 0 || *v >= 60)
        .unwrap_or(600)
}

fn local_model_ready() -> bool {
    std::net::TcpStream::connect_timeout(
        &std::net::SocketAddr::from(([127, 0, 0, 1], 17872)),
        std::time::Duration::from_millis(200),
    )
    .is_ok()
}

fn start_local_model(home: &std::path::Path) -> LocalModelGuard {
    if local_model_ready() {
        return LocalModelGuard(None);
    }
    let root = std::env::var("YUNXI_REPO_ROOT")
        .map(std::path::PathBuf::from)
        .ok()
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let python = root.join(".venv-mlx/bin/python");
    let script = root.join("sidecar/local_llm_server.py");
    if !python.is_file() || !script.is_file() {
        eprintln!(
            "[本机模型] 缺少 {} 或 {}；请先运行 安装Mac环境.command --local-model",
            python.display(),
            script.display()
        );
        return LocalModelGuard(None);
    }
    let env_model = std::env::var("YUNXI_COMPANION_MODEL").unwrap_or_default();
    let model = if matches!(env_model.as_str(), "gemma" | "gemma4") {
        "gemma-4-26B-A4B-it-qat-4bit"
    } else {
        "Qwen3.5-9B-4bit"
    };
    let log = std::fs::File::create(home.join("local-model.log"))
        .map(std::process::Stdio::from)
        .unwrap_or_else(|_| std::process::Stdio::null());
    let child = std::process::Command::new(&python)
        .arg(&script)
        .args([
            "--model", model, "--device", "mlx", "--port", "17872", "--warmup",
        ])
        .arg("--idle-unload")
        .arg(local_idle_unload_seconds().to_string())
        .current_dir(&root)
        .env("YUNXI_BOT_HOME", home)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(log)
        .spawn();
    match child {
        Ok(child) => {
            let mut guard = LocalModelGuard(Some(child));
            eprintln!("[本机模型] 正在加载 {model}…");
            for _ in 0..240 {
                if local_model_ready() {
                    eprintln!("[本机模型] 已就绪");
                    return guard;
                }
                std::thread::sleep(std::time::Duration::from_millis(500));
                if guard
                    .0
                    .as_mut()
                    .is_some_and(|c| matches!(c.try_wait(), Ok(Some(_))))
                {
                    break;
                }
            }
            eprintln!("[本机模型] 未在 120 秒内就绪；聊天会显示本机模型未启动，不会上云。");
            guard
        }
        Err(e) => {
            eprintln!("[本机模型] 启动失败：{e}");
            LocalModelGuard(None)
        }
    }
}

/// 启动本地聊天服务。
pub fn run(args: &[String]) -> Result<i32, Box<dyn std::error::Error>> {
    let home = crate::default_home();
    let store = SessionStore::new(&home);
    let mut effort = match crate::flag(args, "--thinking").unwrap_or("auto") {
        "off" => ReasoningEffort::Never,
        "on" => ReasoningEffort::Always,
        _ => ReasoningEffort::Auto,
    };

    if crate::flag(args, "--endpoint").is_none() {
        crate::ensure_decider_running();
    }
    // —— 窗口内审批：读线程把 UI 的决定送回来 ——
    let approvals = ApprovalBridge::new();
    let approver = WebApprover {
        bridge: approvals.clone(),
    };

    // —— 分类器 + 双代理：陪伴（本地） / 工作（云端） ——
    let decider: Arc<dyn Decider> = Arc::new(yunxi_bot_core::decide::LayaDecider::new(
        crate::flag(args, "--endpoint").unwrap_or("http://127.0.0.1:17870/decide"),
    ));
    let classifier = TaskClassifier::new(decider.clone(), crate::ledger_path());
    let companion_adapter = ModelAdapter::companion();
    let work_adapter = ModelAdapter::work();

    let companion = ChatHandler::new(
        home.clone(),
        "云熙",
        crate::chat_handler::DEFAULT_PERSONA,
        crate::chat_handler::default_rules(),
    )
    .with_chat_protocol(yunxi_bot_core::prompts::local_chat_protocol())
    .with_decider(decider.clone());

    let mut tool_descriptions: Vec<ToolDescription> = Vec::new();
    let mut mcp_count = 0usize;
    let mut mcp_problems: Vec<String> = Vec::new();
    // 工作上下文独立在 data/work：人格、画像、台账、向量库都与陪伴隔离。
    let work_home = home.join("work");
    std::fs::create_dir_all(&work_home)?;
    let work_persona_path = work_home.join("persona.md");
    if !work_persona_path.exists() {
        std::fs::write(&work_persona_path, yunxi_bot_core::prompts::WORK_PERSONA)?;
    }
    let mut work = ChatHandler::new_with_context(
        home.clone(),
        work_home.clone(),
        "云熙 · 工作助理",
        yunxi_bot_core::prompts::WORK_PERSONA,
        crate::chat_handler::default_rules(),
    )
    .with_chat_protocol(yunxi_bot_core::prompts::work_protocol())
    .with_decider(decider.clone());
    if let Ok(tools) = crate::tooling::default_registry(&home) {
        let mut tools = tools;
        match crate::tooling::with_mcp(&mut tools, &home) {
            Ok((n, problems)) => {
                mcp_count = n;
                mcp_problems = problems;
            }
            Err(e) => mcp_problems.push(e.to_string()),
        }
        tool_descriptions = tools.describe();
        work = work.with_tools(tools);
    }
    let mut work = work
        .with_approver(Arc::new(Mutex::new(approver)))
        .with_policy(crate::tooling::policy_from_args(args))
        .with_sink(crate::tool_sink_or_warn_at(work_home.join("ledger.jsonl")));
    let mut companion = companion;

    let mut budget = ContextBudget::for_model(ModelSpec::DEEPSEEK_FLASH.model);
    if let Some(w) = crate::flag(args, "--context-window") {
        let w: usize = w.parse()?;
        if w < 512 {
            eprintln!("--context-window 太小了（{w}），至少 512");
            return Ok(2);
        }
        budget.window_tokens = w;
        budget.reserve_for_reply = budget.reserve_for_reply.min(w / 8);
    }
    // 本机 Qwen3.5-9B 的 MLX 侧车按 32k 上下文配置；人格前缀较大，
    // 不能沿用云端的 64k 预算——否则会话不压缩，请求会直接被侧车拒绝。
    let mut local_budget = ContextBudget::for_model(ModelSpec::LOCAL_CHAT.model);
    local_budget.window_tokens = 32768;
    local_budget.reserve_for_reply = 32768 / 8;

    let mut ledger = Ledger::open(crate::ledger_path())?;
    // 工作代理的模型调用/压缩事件写工作台账，和陪伴台账分开。
    let mut work_ledger = Ledger::open(work_home.join("ledger.jsonl"))?;

    // —— 新建或接最近一个会话 ——
    let mut session_id = new_session_id();
    let mut work_session_id = format!("{session_id}-work");
    // 懒加载：App 传 --auto-local 时，第一次聊天才拉起本机模型。
    let mut local_model_guard: Option<LocalModelGuard> = None;
    let mut turns: u32 = 0;
    let mut work_turns: u32 = 0;
    let mut resumed = false;
    if args.iter().any(|a| a == "--resume")
        && let Some(file) = store
            .list()?
            .into_iter()
            .find(|file| !file.id.ends_with("-work"))
    {
        let (layout, mismatched) =
            yunxi_bot_core::think::session::resume_onto(&file, &companion.expected_chat_prefix());
        let _ = mismatched;
        companion.restore_chat_layout(layout, crate::chat_handler::CHAT_SESSION_KEY);
        session_id = file.id;
        work_session_id = format!("{session_id}-work");
        turns = file.turns;
        resumed = true;
    }

    // 流式正文直接写协议行（与审批、最终回复同在主线程，写出顺序稳定）。
    let on_delta: Arc<dyn Fn(&str) + Send + Sync> = Arc::new(|text: &str| {
        write_event(json!({"type":"delta","text":text}));
    });
    companion.set_on_delta(on_delta.clone());
    work.set_on_delta(on_delta);

    let history = read_display_history(&home, &session_id, 200);
    if !history.is_empty() {
        write_event(json!({"type":"history","messages":history}));
    }
    write_event(json!({"type":"ready","session":session_id,"turns":turns,"resumed":resumed}));
    write_event(sessions_event(&home, &store));

    // 读线程：协议请求进 channel；审批决定先进 bridge（chat_turn 阻塞时也要能送达）。
    let (tx, rx) = mpsc::channel::<Value>();
    let bridge_for_reader = approvals.clone();
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        for line in stdin.lock().lines() {
            let Ok(line) = line else { break };
            if line.trim().is_empty() {
                continue;
            }
            let req: Value = match serde_json::from_str(&line) {
                Ok(v) => v,
                Err(e) => {
                    let _ = tx.send(
                        json!({"type":"__bad_json","message":format!("请求不是合法 JSON: {e}")}),
                    );
                    continue;
                }
            };
            if req["type"] == "approval"
                && let (Some(id), Some(allow)) = (req["id"].as_u64(), req["allow"].as_bool())
            {
                bridge_for_reader.resolve(id, allow);
                continue;
            }
            if tx.send(req).is_err() {
                break;
            }
        }
    });

    for req in rx {
        match req["type"].as_str().unwrap_or("chat") {
            "ping" => write_event(json!({"type":"pong"})),
            "__bad_json" => write_event(json!({"type":"error","message":req["message"]})),
            "list_sessions" => write_event(sessions_event(&home, &store)),
            "effort" => {
                effort = match req["value"].as_str().unwrap_or("auto") {
                    "on" => ReasoningEffort::Always,
                    "off" => ReasoningEffort::Never,
                    _ => ReasoningEffort::Auto,
                };
                write_event(json!({"type":"effort","value":effort_label(effort)}));
            }
            "new" => {
                companion.reset_chat_session();
                work.reset_chat_session();
                session_id = new_session_id();
                work_session_id = format!("{session_id}-work");
                turns = 0;
                work_turns = 0;
                write_event(json!({"type":"session","session":session_id}));
                write_event(sessions_event(&home, &store));
            }
            "open" => {
                let Some(id) = req["session"].as_str().filter(|s| !s.is_empty()) else {
                    write_event(json!({"type":"error","message":"缺少 session"}));
                    continue;
                };
                match store.load(id) {
                    Ok(file) => {
                        let (layout, _mismatch) = yunxi_bot_core::think::session::resume_onto(
                            &file,
                            &companion.expected_chat_prefix(),
                        );
                        companion
                            .restore_chat_layout(layout, crate::chat_handler::CHAT_SESSION_KEY);
                        work.reset_chat_session();
                        session_id = file.id;
                        work_session_id = format!("{session_id}-work");
                        turns = file.turns;
                        work_turns = 0;
                        let history = read_display_history(&home, &session_id, 200);
                        let history_missing = history.is_empty();
                        if !history_missing {
                            write_event(json!({"type":"history","messages":history}));
                        }
                        write_event(
                            json!({"type":"opened","session":session_id,"turns":turns,"history_missing":history_missing}),
                        );
                    }
                    Err(e) => {
                        write_event(json!({"type":"error","message":format!("打开会话失败: {e}")}))
                    }
                }
            }
            "capabilities" => write_event(capabilities_event(&CapabilityContext {
                home: &home,
                store: &store,
                ledger: &ledger,
                effort,
                session: &session_id,
                turns,
                tools: &tool_descriptions,
                mcp_count,
                mcp_problems: &mcp_problems,
            })),
            "chat" => {
                let raw = req["text"].as_str().unwrap_or("").trim().to_string();
                if raw.is_empty() {
                    write_event(json!({"type":"error","message":"空消息"}));
                    continue;
                }
                let text = strip_command(&raw);
                if text.is_empty() {
                    write_event(json!({"type":"error","message":"空消息"}));
                    continue;
                }
                let classification = match classifier.classify(&raw) {
                    Ok(c) => c,
                    Err(e) => {
                        write_event(json!({"type":"error","message":format!("分类失败: {e}")}));
                        continue;
                    }
                };
                let is_chat = matches!(classification.kind, InputKind::Chat);
                if is_chat {
                    // 进入聊天才拉起本机模型；任务不会唤醒它。
                    if !local_model_ready() && args.iter().any(|a| a == "--auto-local") {
                        local_model_guard = Some(start_local_model(&home));
                    }
                    if !local_model_ready() {
                        write_event(json!({
                            "type":"error",
                            "message":"本机模型未启动：请先运行「启动本地模型.command」。这一轮没有上云。"
                        }));
                        continue;
                    }
                }
                let routing = if is_chat {
                    match companion.route_for_spec(
                        &text,
                        effort,
                        companion_adapter.spec.clone(),
                        TaskKind::Conversation,
                    ) {
                        Ok(r) => r,
                        Err(e) => {
                            write_event(
                                json!({"type":"error","message":format!("本机路由失败: {e}")}),
                            );
                            continue;
                        }
                    }
                } else {
                    match work.route_for_spec(
                        &text,
                        effort,
                        work_adapter.spec.clone(),
                        TaskKind::Generation,
                    ) {
                        Ok(r) => r,
                        Err(e) => {
                            write_event(
                                json!({"type":"error","message":format!("工作路由失败: {e}")}),
                            );
                            continue;
                        }
                    }
                };
                write_event(json!({
                    "type":"route",
                    "model":routing.spec.model,
                    "thinking":routing.thinking,
                    "mode":if is_chat { "chat" } else { "task" },
                    "agent":if is_chat { companion_adapter.role } else { work_adapter.role },
                    "adapter":if is_chat { companion_adapter.label } else { work_adapter.label },
                    "local":if is_chat { companion_adapter.local } else { work_adapter.local },
                    "tools":if is_chat { companion_adapter.tools } else { work_adapter.tools },
                    "classified":classification.reason,
                    "classified_model":classification.model,
                }));

                let compact = if is_chat {
                    crate::chat::compact_now(&mut companion, &local_budget, &routing.spec)
                } else {
                    crate::chat::compact_now(&mut work, &budget, &routing.spec)
                };
                if let Some(c) = compact {
                    let active_ledger = if is_chat {
                        &mut ledger
                    } else {
                        &mut work_ledger
                    };
                    let _ = active_ledger.append(
                        EventKind::ContextCompacted,
                        None,
                        json!({
                            "session": if is_chat { &session_id } else { &work_session_id },
                            "dropped_messages": c.dropped_messages,
                            "dropped_chars": c.dropped_chars,
                            "dropped_tool_calls": c.dropped_tool_calls,
                            "kept_messages": c.kept_messages,
                            "before_tokens": c.before_tokens,
                            "after_tokens": c.after_tokens,
                            "saved_tokens": c.saved_tokens(),
                            "summarized": c.summarized,
                        }),
                    );
                }

                let mut records = Vec::new();
                let result = if is_chat {
                    companion.chat_turn_with_images(&routing, &text, Vec::new(), &mut records)
                } else {
                    work.chat_turn_with_images(&routing, &text, Vec::new(), &mut records)
                };
                match result {
                    Ok(reply) => {
                        turns += 1;
                        if is_chat {
                            if let Err(e) =
                                crate::chat::persist(&companion, &store, &session_id, turns, 0)
                            {
                                eprintln!("[chat-server] 陪伴会话存盘失败: {e}");
                            }
                        } else {
                            work_turns += 1;
                            if let Err(e) =
                                crate::chat::persist(&work, &store, &work_session_id, work_turns, 0)
                            {
                                eprintln!("[chat-server] 工作会话存盘失败: {e}");
                            }
                        }
                        append_display(&home, &session_id, "user", &text);
                        append_display(&home, &session_id, "assistant", &reply);
                        if is_chat {
                            drain_records(&mut ledger, &mut records);
                        } else {
                            drain_records(&mut work_ledger, &mut records);
                        }
                        write_event(
                            json!({"type":"reply","text":reply,"session":session_id,"turns":turns}),
                        );
                        write_event(sessions_event(&home, &store));
                    }
                    Err(e) => {
                        if is_chat {
                            drain_records(&mut ledger, &mut records);
                        } else {
                            drain_records(&mut work_ledger, &mut records);
                        }
                        write_event(json!({"type":"error","message":format!("这一轮失败了: {e}")}));
                    }
                }
            }
            other => {
                write_event(json!({"type":"error","message":format!("未知请求类型: {other}")}))
            }
        }
    }
    // 让 guard 活到 chat-server 退出；正常退出时回收本机模型子进程。
    drop(local_model_guard);
    Ok(0)
}

// MARK: - 审批桥

struct ApprovalState {
    decisions: std::collections::HashMap<u64, bool>,
}

/// UI 与 chat_turn 之间的审批会合点。等待带超时：窗口关掉/无响应时一律拒绝。
struct ApprovalBridge {
    next: AtomicU64,
    state: Mutex<ApprovalState>,
    ready: Condvar,
}

impl ApprovalBridge {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            next: AtomicU64::new(1),
            state: Mutex::new(ApprovalState {
                decisions: std::collections::HashMap::new(),
            }),
            ready: Condvar::new(),
        })
    }

    fn resolve(&self, id: u64, allow: bool) {
        if let Ok(mut state) = self.state.lock() {
            state.decisions.insert(id, allow);
            self.ready.notify_all();
        }
    }

    fn wait(&self, id: u64, timeout: Duration) -> Option<bool> {
        let deadline = std::time::Instant::now() + timeout;
        let mut state = self.state.lock().ok()?;
        loop {
            if let Some(answer) = state.decisions.remove(&id) {
                return Some(answer);
            }
            let left = deadline.checked_duration_since(std::time::Instant::now())?;
            let (guard, _) = self.ready.wait_timeout(state, left).ok()?;
            state = guard;
        }
    }

    fn next_id(&self) -> u64 {
        self.next.fetch_add(1, Ordering::SeqCst)
    }
}

struct WebApprover {
    bridge: Arc<ApprovalBridge>,
}

impl Approver for WebApprover {
    fn approve(&mut self, req: &ApprovalRequest) -> Approval {
        let id = self.bridge.next_id();
        // 私有工具的参数不往界面送（与终端审批同一条隐私规则）。
        let arguments = if req.private {
            "[私有参数不显示]".to_string()
        } else {
            truncate(&req.arguments, 1600)
        };
        write_event(json!({
            "type":"approval",
            "id":id,
            "tool":req.tool,
            "capability":req.capability.label(),
            "specifier":req.specifier,
            "reason":req.reason,
            "preview":req.preview,
            "arguments":arguments,
            "require_human":req.require_human,
        }));
        match self.bridge.wait(id, Duration::from_secs(300)) {
            Some(true) => Approval::Once,
            Some(false) => Approval::Deny,
            None => {
                write_event(json!({"type":"approval_expired","id":id}));
                Approval::Deny
            }
        }
    }
}

// MARK: - 协议输出与数据

fn write_event(value: Value) {
    let stdout = std::io::stdout();
    let mut lock = stdout.lock();
    let _ = writeln!(lock, "{value}");
    let _ = lock.flush();
}

fn effort_label(effort: ReasoningEffort) -> &'static str {
    match effort {
        ReasoningEffort::Always => "on",
        ReasoningEffort::Never => "off",
        ReasoningEffort::Auto => "auto",
    }
}

fn truncate(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    text.chars().take(limit).collect::<String>() + "…"
}

fn sessions_event(home: &Path, store: &SessionStore) -> Value {
    let items = store
        .list()
        .unwrap_or_default()
        .into_iter()
        .filter(|file| !file.id.ends_with("-work"))
        .map(|file| {
            let history = read_display_history(home, &file.id, 200);
            let preview = history
                .iter()
                .find(|row| row["role"] == "user")
                .and_then(|row| row["text"].as_str().map(str::to_string));
            json!({
                "id": file.id,
                "preview": preview.map(|p| truncate(p.trim(), 60)).unwrap_or_default(),
                "turns": file.turns,
                "updated_at": file.updated_at,
                "has_history": !history.is_empty(),
            })
        })
        .collect::<Vec<_>>();
    json!({"type":"sessions","items":items})
}

// MARK: - 能力快照

fn port_listening(port: u16) -> bool {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    TcpStream::connect_timeout(&addr, Duration::from_millis(150)).is_ok()
}

fn event_kind_name(kind: yunxi_bot_core::EventKind) -> String {
    serde_json::to_value(kind)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| format!("{kind:?}"))
}

fn trigger_label(trigger: &yunxi_bot_core::job::Trigger) -> String {
    match trigger {
        yunxi_bot_core::job::Trigger::Manual => "手动".to_string(),
        yunxi_bot_core::job::Trigger::Every { seconds } => format!("每 {seconds} 秒"),
        yunxi_bot_core::job::Trigger::Cron { expr } => format!("cron {expr}"),
        yunxi_bot_core::job::Trigger::Watch { path } => format!("监听 {path}"),
    }
}

fn job_state_label(state: yunxi_bot_core::job::JobState) -> &'static str {
    use yunxi_bot_core::job::JobState;
    match state {
        JobState::Pending => "待运行",
        JobState::WaitingApproval => "待批准",
        JobState::Running => "运行中",
        JobState::Succeeded => "上次成功",
        JobState::Failed => "上次失败",
        JobState::Disabled => "已停用",
    }
}

struct CapabilityContext<'a> {
    home: &'a Path,
    store: &'a SessionStore,
    ledger: &'a Ledger,
    effort: ReasoningEffort,
    session: &'a str,
    turns: u32,
    tools: &'a [ToolDescription],
    mcp_count: usize,
    mcp_problems: &'a [String],
}

fn capabilities_event(ctx: &CapabilityContext<'_>) -> Value {
    let CapabilityContext {
        home,
        store,
        ledger,
        effort,
        session,
        turns,
        tools,
        mcp_count,
        mcp_problems,
    } = *ctx;
    let now = yunxi_bot_core::now_millis().unwrap_or(0);
    let events = ledger.events();

    // —— 会话与身份 ——
    let sessions = store.list().unwrap_or_default();
    let latest = sessions.first();
    let persona_bytes = std::fs::metadata(home.join("persona.md"))
        .map(|m| m.len())
        .unwrap_or(0);
    let profile_bytes = std::fs::metadata(home.join("profile.md"))
        .map(|m| m.len())
        .unwrap_or(0);

    // —— 工具 ——
    let mut by_capability: std::collections::BTreeMap<String, usize> =
        std::collections::BTreeMap::new();
    for tool in tools {
        *by_capability.entry(tool.capability.clone()).or_insert(0) += 1;
    }
    let tool_items: Vec<Value> = tools
        .iter()
        .map(|tool| {
            json!({
                "name": tool.name,
                "description": truncate(&tool.description, 220),
                "capability": tool.capability,
                "approval": tool.approval,
                "private_io": tool.private_io,
                "fresh_approval": tool.fresh_approval,
            })
        })
        .collect();

    // —— 记忆 ——
    let memory = Memory::from_events(events);
    let all = memory.all_sorted(now);
    let mut memory_counts: std::collections::BTreeMap<&'static str, usize> =
        std::collections::BTreeMap::new();
    for entry in &all {
        *memory_counts.entry(entry.kind.label()).or_insert(0) += 1;
    }
    let mut recent = all.clone();
    recent.sort_by_key(|entry| std::cmp::Reverse(entry.created_at));
    let recent_memory: Vec<Value> = recent
        .iter()
        .take(8)
        .map(|entry| {
            json!({
                "id": entry.id,
                "kind": entry.kind.label(),
                "text": truncate(&entry.text, 160),
                "weight": (entry.weight * 100.0).round() / 100.0,
                "created_at": entry.created_at,
            })
        })
        .collect();
    let pending = yunxi_bot_core::continuity::pending(events, now).unwrap_or_default();
    let pending_items: Vec<Value> = pending
        .iter()
        .take(5)
        .map(|c| {
            json!({
                "id": c.id,
                "kind": c.kind.label(),
                "text": truncate(&c.text, 160),
                "quote": truncate(&c.quote, 120),
                "source_seq": c.source_seq,
            })
        })
        .collect();
    let followups = yunxi_bot_core::continuity::followups(events)
        .map(|f| f.len())
        .unwrap_or(0);
    let knowledge = yunxi_bot_core::retrieval::documents(home)
        .map(|docs| docs.len())
        .unwrap_or(0);

    // —— 任务 ——
    let jobs: Vec<Value> = ledger
        .rebuild()
        .into_values()
        .map(|job| {
            json!({
                "id": job.id.as_str(),
                "name": job.spec.name,
                "command": truncate(&job.spec.command.join(" "), 140),
                "trigger": trigger_label(&job.spec.trigger),
                "state": job_state_label(job.state),
                "irreversible": job.spec.irreversible,
                "last_run_at": job.last_run_at,
                "last_error": job.last_error.as_deref().map(|e| truncate(e, 120)),
            })
        })
        .collect();
    let do_tasks: Vec<Value> = yunxi_bot_core::ledger::task_from_events(events)
        .into_values()
        .map(|task| {
            let (done, failed, skipped) = task.tally();
            json!({
                "id": task.id,
                "goal": truncate(&task.goal, 160),
                "state": task.state.label(),
                "steps": task.steps.len(),
                "done": done,
                "failed": failed,
                "skipped": skipped,
            })
        })
        .collect();

    // —— 台账与成本 ——
    let mut kind_counts: std::collections::BTreeMap<String, usize> =
        std::collections::BTreeMap::new();
    for event in events {
        *kind_counts.entry(event_kind_name(event.kind)).or_insert(0) += 1;
    }
    let mut top_kinds: Vec<(String, usize)> = kind_counts.into_iter().collect();
    top_kinds.sort_by_key(|(_, count)| std::cmp::Reverse(*count));
    let top_kinds: Vec<Value> = top_kinds
        .into_iter()
        .take(12)
        .map(|(kind, count)| json!({"kind": kind, "count": count}))
        .collect();
    let recent_events: Vec<Value> = events
        .iter()
        .rev()
        .take(10)
        .map(|event| {
            json!({
                "seq": event.seq,
                "kind": event_kind_name(event.kind),
                "at": event.at,
            })
        })
        .collect();
    let cost = project_costs(events);

    // —— 系统 ——
    let daemon_lock = std::fs::read_to_string(home.join("daemon.lock"))
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok());
    let daemon_pid = daemon_lock
        .as_ref()
        .and_then(|v| v["pid"].as_u64())
        .map(|pid| pid as u32);
    let daemon_alive = daemon_pid.is_some_and(yunxi_bot_core::instance::is_process_alive);
    let autostart_path = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .map(|p| p.join("Library/LaunchAgents/com.yunxi.bot.agent.plist"));
    let autostart_registered = autostart_path.as_ref().is_some_and(|p| p.exists());
    let models: Vec<String> = std::fs::read_dir(home.join("models"))
        .map(|it| {
            it.filter_map(|e| e.ok())
                .filter(|e| e.path().is_dir())
                .map(|e| e.file_name().to_string_lossy().to_string())
                .collect()
        })
        .unwrap_or_default();
    let mut models = models;
    models.sort();
    let ledger_bytes = std::fs::metadata(home.join("ledger.jsonl"))
        .map(|m| m.len())
        .unwrap_or(0);
    let chat_ui_count = std::fs::read_dir(home.join("chat-ui"))
        .map(|it| {
            it.filter_map(|e| e.ok())
                .filter(|e| e.path().extension().is_some_and(|ext| ext == "jsonl"))
                .count()
        })
        .unwrap_or(0);
    let mail_configured = home.join("mac-mail.json").exists();
    let services = vec![
        json!({"name": "Laya 决策服务", "port": 17870, "listening": port_listening(17870)}),
        json!({"name": "EmbeddingGemma 2", "port": 17873, "listening": port_listening(17873)}),
    ];

    json!({
        "type": "capabilities",
        "snapshot": {
            "generated_at": now,
            "version": env!("CARGO_PKG_VERSION"),
            "home": home.display().to_string(),
            "platform": std::env::consts::OS,
            "bot": {
                "model": ModelSpec::DEEPSEEK_FLASH.model,
                "effort": effort_label(effort),
                "session": session,
                "turns": turns,
                "session_count": sessions.len(),
                "latest_session": latest.map(|s| s.id.clone()),
                "latest_turns": latest.map(|s| s.turns),
            },
            "identity": {
                "persona_bytes": persona_bytes,
                "profile_bytes": profile_bytes,
            },
            "tools": {
                "total": tools.len(),
                "by_capability": by_capability
                    .into_iter()
                    .map(|(label, count)| json!({"label": label, "count": count}))
                    .collect::<Vec<_>>(),
                "items": tool_items,
                "mcp_count": mcp_count,
                "mcp_problems": mcp_problems,
            },
            "memory": {
                "total": memory.len(),
                "by_kind": memory_counts
                    .into_iter()
                    .map(|(label, count)| json!({"label": label, "count": count}))
                    .collect::<Vec<_>>(),
                "recent": recent_memory,
                "pending": pending_items,
                "pending_total": pending.len(),
                "followups": followups,
                "knowledge_documents": knowledge,
            },
            "tasks": {
                "jobs": jobs,
                "do_tasks": do_tasks,
            },
            "ledger": {
                "events": events.len(),
                "recent": recent_events,
                "top_kinds": top_kinds,
                "bytes": ledger_bytes,
            },
            "cost": {
                "calls": cost.calls,
                "prompt_tokens": cost.prompt_tokens,
                "completion_tokens": cost.completion_tokens,
                "cache_hit_tokens": cost.cache_hit_tokens,
                "cache_miss_tokens": cost.cache_miss_tokens,
                "cache_hit_rate": cost.cache_hit_rate(),
                "thinking_calls": cost.thinking_calls,
                "total": cost.total,
                "saved": cost.saved(),
                "currency": "CNY",
            },
            "system": {
                "daemon": {
                    "pid": daemon_pid,
                    "alive": daemon_alive,
                    "lock_present": daemon_lock.is_some(),
                },
                "autostart": {
                    "registered": autostart_registered,
                    "path": autostart_path.map(|p| p.display().to_string()),
                },
                "services": services,
                "models": models,
                "chat_ui_sessions": chat_ui_count,
                "mail_configured": mail_configured,
                "ledger_path": home.join("ledger.jsonl").display().to_string(),
            }
        }
    })
}

// MARK: - 显示历史（只服务界面回显，不进模型上下文）

fn display_log_path(home: &Path, session: &str) -> PathBuf {
    home.join("chat-ui").join(format!("{session}.jsonl"))
}

fn append_display(home: &Path, session: &str, role: &str, text: &str) {
    let path = display_log_path(home, session);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let line = json!({"role": role, "text": text});
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = writeln!(file, "{line}");
    }
}

fn read_display_history(home: &Path, session: &str, limit: usize) -> Vec<Value> {
    let Ok(text) = std::fs::read_to_string(display_log_path(home, session)) else {
        return Vec::new();
    };
    let mut rows: Vec<Value> = text
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    if rows.len() > limit {
        rows = rows.split_off(rows.len() - limit);
    }
    rows
}

fn new_session_id() -> String {
    format!("chat-{}", chrono::Local::now().format("%Y%m%d-%H%M%S"))
}

fn drain_records(ledger: &mut Ledger, records: &mut Vec<CallRecord>) {
    for record in records.drain(..) {
        let _ = ledger.append(EventKind::ModelCalled, None, json!(record));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_ids_are_acceptable_to_the_session_store() {
        let id = new_session_id();
        assert!(id.starts_with("chat-"));
        SessionStore::check_id(&id).expect("chat-server 新建的会话 id 必须能落盘");
    }

    #[test]
    fn display_history_round_trips_and_is_bounded() {
        let home = std::env::temp_dir().join(format!("yunxi-ui-history-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        append_display(&home, "chat-test", "user", "第一句");
        append_display(&home, "chat-test", "assistant", "第二句");
        let rows = read_display_history(&home, "chat-test", 200);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["role"], "user");
        assert_eq!(rows[1]["text"], "第二句");
        let last = read_display_history(&home, "chat-test", 1);
        assert_eq!(last.len(), 1);
        assert_eq!(last[0]["text"], "第二句");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn event_kind_names_match_the_serialized_form() {
        assert_eq!(
            event_kind_name(yunxi_bot_core::EventKind::InfoFetched),
            "info_fetched"
        );
        assert_eq!(
            event_kind_name(yunxi_bot_core::EventKind::ModelCalled),
            "model_called"
        );
    }

    #[test]
    fn trigger_labels_are_human_readable() {
        use yunxi_bot_core::job::Trigger;
        assert_eq!(trigger_label(&Trigger::Manual), "手动");
        assert_eq!(trigger_label(&Trigger::Every { seconds: 60 }), "每 60 秒");
        assert_eq!(
            trigger_label(&Trigger::Cron {
                expr: "0 9 * * *".to_string()
            }),
            "cron 0 9 * * *"
        );
        assert_eq!(
            trigger_label(&Trigger::Watch {
                path: "/tmp/x".to_string()
            }),
            "监听 /tmp/x"
        );
    }

    #[test]
    fn truncation_is_char_safe() {
        assert_eq!(truncate("云熙", 10), "云熙");
        let cut = truncate("云熙云熙云熙", 2);
        assert!(cut.starts_with("云熙"));
        assert!(cut.ends_with('…'));
    }
}
