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
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::time::Duration;

use serde_json::{Value, json};
use yunxi_bot_core::costlog::CallRecord;
use yunxi_bot_core::think::context::ContextBudget;
use yunxi_bot_core::think::session::SessionStore;
use yunxi_bot_core::think::{ModelSpec, ReasoningEffort};
use yunxi_bot_core::tool::runner::{Approval, ApprovalRequest, Approver};
use yunxi_bot_core::{EventKind, Ledger};

use crate::chat_handler::ChatHandler;

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

    // —— 和 `chat` 同一套装配：工具、审批、台账、决策模型 ——
    let mut handler = ChatHandler::new(
        home.clone(),
        "云熙",
        crate::chat_handler::DEFAULT_PERSONA,
        crate::chat_handler::default_rules(),
    );
    if let Ok(tools) = crate::tooling::default_registry(&home) {
        let mut tools = tools;
        let _ = crate::tooling::with_mcp(&mut tools, &home);
        handler = handler.with_tools(tools);
    }
    handler = handler
        .with_approver(Arc::new(Mutex::new(approver)))
        .with_policy(crate::tooling::policy_from_args(args))
        .with_sink(crate::tool_sink_or_warn())
        .with_decider(Arc::new(yunxi_bot_core::decide::LayaDecider::new(
            crate::flag(args, "--endpoint").unwrap_or("http://127.0.0.1:17870/decide"),
        )));

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

    let mut ledger = Ledger::open(crate::ledger_path())?;

    // —— 新建或接最近一个会话 ——
    let mut session_id = new_session_id();
    let mut turns: u32 = 0;
    let mut resumed = false;
    if args.iter().any(|a| a == "--resume")
        && let Some(file) = store.latest()?
    {
        let (layout, mismatched) =
            yunxi_bot_core::think::session::resume_onto(&file, &handler.expected_chat_prefix());
        let _ = mismatched;
        handler.restore_chat_layout(layout, crate::chat_handler::CHAT_SESSION_KEY);
        session_id = file.id;
        turns = file.turns;
        resumed = true;
    }

    // 流式正文直接写协议行（与审批、最终回复同在主线程，写出顺序稳定）。
    handler.set_on_delta(Arc::new(|text: &str| {
        write_event(json!({"type":"delta","text":text}));
    }));

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
                handler.reset_chat_session();
                session_id = new_session_id();
                turns = 0;
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
                            &handler.expected_chat_prefix(),
                        );
                        handler.restore_chat_layout(layout, crate::chat_handler::CHAT_SESSION_KEY);
                        session_id = file.id;
                        turns = file.turns;
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
            "chat" => {
                let text = req["text"].as_str().unwrap_or("").trim().to_string();
                if text.is_empty() {
                    write_event(json!({"type":"error","message":"空消息"}));
                    continue;
                }
                let routing = match handler.route_for(&text, effort) {
                    Ok(r) => r,
                    Err(e) => {
                        write_event(json!({"type":"error","message":format!("路由失败: {e}")}));
                        continue;
                    }
                };
                write_event(
                    json!({"type":"route","model":routing.spec.model,"thinking":routing.thinking}),
                );

                if let Some(c) = crate::chat::compact_now(&mut handler, &budget, &routing.spec) {
                    let _ = ledger.append(
                        EventKind::ContextCompacted,
                        None,
                        json!({
                            "session": session_id,
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
                match handler.chat_turn(&routing, &text, &mut records) {
                    Ok(reply) => {
                        turns += 1;
                        if let Err(e) =
                            crate::chat::persist(&handler, &store, &session_id, turns, 0)
                        {
                            eprintln!("[chat-server] 存盘失败: {e}");
                        }
                        append_display(&home, &session_id, "user", &text);
                        append_display(&home, &session_id, "assistant", &reply);
                        drain_records(&mut ledger, &mut records);
                        write_event(
                            json!({"type":"reply","text":reply,"session":session_id,"turns":turns}),
                        );
                        write_event(sessions_event(&home, &store));
                    }
                    Err(e) => {
                        drain_records(&mut ledger, &mut records);
                        write_event(json!({"type":"error","message":format!("这一轮失败了: {e}")}));
                    }
                }
            }
            other => {
                write_event(json!({"type":"error","message":format!("未知请求类型: {other}")}))
            }
        }
    }
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
    fn truncation_is_char_safe() {
        assert_eq!(truncate("云熙", 10), "云熙");
        let cut = truncate("云熙云熙云熙", 2);
        assert!(cut.starts_with("云熙"));
        assert!(cut.ends_with('…'));
    }
}
