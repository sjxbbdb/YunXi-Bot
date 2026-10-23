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
//! {"type":"delta","text":"…"}          // 流式正文，可能多条
//! {"type":"route","model":"deepseek-flash","thinking":false}
//! {"type":"reply","text":"…","session":"…","turns":4}
//! {"type":"error","message":"…"}
//! {"type":"pong"}
//! ```
//!
//! 窗口 → 服务：
//! ```text
//! {"type":"chat","text":"在吗？"}
//! {"type":"new"}
//! {"type":"ping"}
//! ```
//!
//! stdout 只留给协议；日志一律走 stderr，否则窗口会把日志当成消息。

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::{Value, json};
use yunxi_bot_core::costlog::CallRecord;
use yunxi_bot_core::think::ModelSpec;
use yunxi_bot_core::think::ReasoningEffort;
use yunxi_bot_core::think::context::ContextBudget;
use yunxi_bot_core::think::session::SessionStore;
use yunxi_bot_core::{EventKind, Ledger};

use crate::chat_handler::ChatHandler;

/// 启动本地聊天服务。
pub fn run(args: &[String]) -> Result<i32, Box<dyn std::error::Error>> {
    let home = crate::default_home();
    let store = SessionStore::new(&home);
    let effort = match crate::flag(args, "--thinking").unwrap_or("auto") {
        "off" => ReasoningEffort::Never,
        "on" => ReasoningEffort::Always,
        _ => ReasoningEffort::Auto,
    };

    if crate::flag(args, "--endpoint").is_none() {
        crate::ensure_decider_running();
    }

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
        .with_policy(crate::tooling::policy_from_args(args))
        .with_sink(crate::tool_sink_or_warn())
        .with_decider(Arc::new(yunxi_bot_core::decide::LayaDecider::new(
            crate::flag(args, "--endpoint").unwrap_or("http://127.0.0.1:17870/decide"),
        )));

    // 窗口没有"人在终端里答审批"的通道：不挂审批者 = 默认 RefusingApprover。
    // 需要人工批准的动作会被明确拒绝，而不是被悄悄放行。
    // （只读工具按策略照常可用。）

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
        let (layout, mismatch) =
            yunxi_bot_core::think::session::resume_onto(&file, &handler.expected_chat_prefix());
        let _ = mismatch;
        handler.restore_chat_layout(layout, crate::chat_handler::CHAT_SESSION_KEY);
        session_id = file.id;
        turns = file.turns;
        resumed = true;
    }

    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let history = read_display_history(&home, &session_id, 200);
    if !history.is_empty() {
        emit(&mut out, json!({"type":"history","messages":history}));
    }
    emit(
        &mut out,
        json!({"type":"ready","session":session_id,"turns":turns,"resumed":resumed}),
    );

    // 流式正文走 stdout 的协议行；没有 on_delta 的话窗口只能等整段。
    handler.set_on_delta(Arc::new(|text: &str| {
        let stdout = std::io::stdout();
        let mut lock = stdout.lock();
        let _ = writeln!(lock, "{}", json!({"type":"delta","text":text}));
        let _ = lock.flush();
    }));

    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(e) => {
                eprintln!("[chat-server] 读输入失败: {e}");
                break;
            }
        };
        if line.trim().is_empty() {
            continue;
        }
        let req: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                emit(
                    &mut out,
                    json!({"type":"error","message":format!("请求不是合法 JSON: {e}")}),
                );
                continue;
            }
        };
        match req["type"].as_str().unwrap_or("chat") {
            "ping" => emit(&mut out, json!({"type":"pong"})),
            "new" => {
                handler.reset_chat_session();
                across_new_session(&mut ledger, &session_id);
                session_id = new_session_id();
                turns = 0;
                emit(&mut out, json!({"type":"session","session":session_id}));
            }
            "chat" => {
                let text = req["text"].as_str().unwrap_or("").trim();
                if text.is_empty() {
                    emit(&mut out, json!({"type":"error","message":"空消息"}));
                    continue;
                }
                let routing = match handler.route_for(text, effort) {
                    Ok(r) => r,
                    Err(e) => {
                        emit(
                            &mut out,
                            json!({"type":"error","message":format!("路由失败: {e}")}),
                        );
                        continue;
                    }
                };
                emit(
                    &mut out,
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
                match handler.chat_turn(&routing, text, &mut records) {
                    Ok(reply) => {
                        turns += 1;
                        if let Err(e) =
                            crate::chat::persist(&handler, &store, &session_id, turns, 0)
                        {
                            eprintln!("[chat-server] 存盘失败: {e}");
                        }
                        append_display(&home, &session_id, "user", text);
                        append_display(&home, &session_id, "assistant", &reply);
                        drain_records(&mut ledger, &mut records);
                        emit(
                            &mut out,
                            json!({"type":"reply","text":reply,"session":session_id,"turns":turns}),
                        );
                    }
                    Err(e) => {
                        drain_records(&mut ledger, &mut records);
                        emit(
                            &mut out,
                            json!({"type":"error","message":format!("这一轮失败了: {e}")}),
                        );
                    }
                }
            }
            other => emit(
                &mut out,
                json!({"type":"error","message":format!("未知请求类型: {other}")}),
            ),
        }
    }
    Ok(0)
}

/// 窗口显示用历史（只含你/云熙的对话文本，不含工具消息与系统前缀）。
///
/// 存在 `data/chat-ui/<session>.jsonl`：会话布局仍是唯一上下文来源，
/// 这份只是给界面回显用，损坏/缺失都不影响对话。
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

fn emit(out: &mut impl Write, value: Value) {
    if writeln!(out, "{value}").is_err() || out.flush().is_err() {
        eprintln!("[chat-server] 协议写出失败");
    }
}

fn new_session_id() -> String {
    format!("chat-{}", chrono::Local::now().format("%Y%m%d-%H%M%S"))
}

/// 新会话时在台账留一条，便于事后对齐（会话文件本身另存，不进台账）。
fn across_new_session(ledger: &mut Ledger, old_session: &str) {
    let _ = ledger.append(
        EventKind::InfoTriaged,
        None,
        json!({"source":"chat_server","phase":"new_session","previous":old_session}),
    );
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
        yunxi_bot_core::think::session::SessionStore::check_id(&id)
            .expect("chat-server 新建的会话 id 必须能落盘");
    }

    #[test]
    fn protocol_writer_emits_exactly_one_json_line() {
        let mut buffer = Vec::new();
        emit(&mut buffer, json!({"type":"pong"}));
        let text = String::from_utf8(buffer).unwrap();
        assert_eq!(text.lines().count(), 1, "每行一条 JSON，窗口才能按行解析");
        let parsed: Value = serde_json::from_str(text.trim()).unwrap();
        assert_eq!(parsed["type"], "pong");
    }
}

#[cfg(test)]
mod display_history_tests {
    use super::*;

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
}
