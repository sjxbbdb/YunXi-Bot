//! 交互式会话：**通用 agent 的核心体验**。
//!
//! ## 为什么必须有这个
//!
//! 在此之前只有一次性命令：
//!
//! ```text
//! yunxi-bot do "改一下 X"     ← 跑完就退
//! yunxi-bot do "再改一下 Y"   ← 完全不知道上一句
//! ```
//!
//! 那不是对话，是"发命令—等结果"。而通用 agent 的价值有一大半在
//! **上下文延续**上：你说了"这个文件"，下一句的"它"才有指代对象。
//!
//! ## 三条设计约束
//!
//! 1. **走同一个 `converse`。** 工具循环、审批门禁、台账留痕、缓存前缀
//!    全都不变。**另起一条轻量路径的话，那条路迟早会绕开审批。**
//!
//! 2. **每轮落盘。** 不是退出时才存——那意味着一次崩溃（或 Ctrl+C）
//!    丢掉整场对话。而"丢失"在这里尤其难接受：你刚解释完背景。
//!
//! 3. **审批照常问人。** REPL 里有人在，所以用真审批者。
//!    答 `n` 就是拒绝，和被拒绝的工具一样走 `GateDecision::Deny`。
//!
//! ## 斜杠命令
//!
//! 以 `/` 开头的不进模型。理由很简单：**如果交给模型，它就得猜
//! "退出"是什么意思**，而猜错的表现是它试图调用一个不存在的工具。

use std::io::{BufRead, Write};

use yunxi_bot_core::think::ReasoningEffort;
use yunxi_bot_core::think::session::{
    PrefixMismatch, SESSION_FORMAT_VERSION, SessionFile, SessionStore,
};

use crate::chat_handler::ChatHandler;

/// 一个会话最多留多少轮。
///
/// **这不是上下文预算**（那是下一步要做的），只是一个防止会话文件无限增长
/// 的上限。超了从最老的开始丢，并如实报告——静默丢会让使用者
/// 在很久以后发现"它怎么不记得了"，而那时已经无从追查。
pub const MAX_TURNS: usize = 200;

/// 跑一个交互式会话。
///
/// 返回退出码。
pub fn run(
    handler: &mut ChatHandler,
    store: &SessionStore,
    session_id: String,
    resume: bool,
    effort: ReasoningEffort,
) -> Result<i32, Box<dyn std::error::Error>> {
    let stdin = std::io::stdin();
    let mut turns: u32 = 0;

    // ---- 载入或新建 ----
    let mut resumed_turns = 0u32;
    if resume {
        match store.load(&session_id) {
            Ok(f) => {
                // 前缀核对：变了就**保留历史、换新前缀、如实报告**
                let (layout, mismatch) =
                    yunxi_bot_core::think::session::resume_onto(&f, handler.expected_chat_prefix());
                handler.restore_chat_layout(layout, crate::chat_handler::CHAT_SESSION_KEY);
                resumed_turns = f.turns;
                match mismatch {
                    PrefixMismatch::Same => {
                        println!("接着上次聊（{} 轮）。", f.turns);
                    }
                    PrefixMismatch::Changed { was, now } => {
                        // **不能静默。** 使用者需要知道"换了前缀"，
                        // 否则会以为缓存还在命中，然后对账单感到困惑。
                        println!("接着上次聊（{} 轮）。", f.turns);
                        println!(
                            "  注意：稳定前缀变了（{:x} → {:x}），历史保留但缓存会重建一次。",
                            was, now
                        );
                        println!(
                            "  这通常意味着人格、规则或工具定义变了（比如刚加载了 AGENTS.md）。"
                        );
                    }
                }
                turns = f.turns;
            }
            Err(e) => {
                if resume {
                    // `--resume` 明确要了某个会话却找不到 —— **报错而不是静默新建**。
                    // 静默新建会让使用者以为接上了，然后对着一个空上下文说话。
                    eprintln!("{e}");
                    eprintln!("用 `yunxi-bot chat list` 看有哪些会话。");
                    return Ok(2);
                }
            }
        }
    }

    if turns == 0 {
        println!("新会话：{session_id}");
        println!("  （输入内容开始；`/help` 看命令，`/exit` 退出）");
    }
    println!();

    let mut records = Vec::new();
    let outcome = loop {
        print!("› ");
        let _ = std::io::stdout().flush();

        let mut line = String::new();
        match stdin.lock().read_line(&mut line) {
            Ok(0) => break Ok(()), // EOF（管道关了 / Ctrl+Z）
            Ok(_) => {}
            Err(e) => {
                eprintln!("读输入失败: {e}");
                break Ok(());
            }
        }
        let input = line.trim();
        if input.is_empty() {
            continue;
        }

        // ---- 斜杠命令：不进模型 ----
        if let Some(cmd) = input.strip_prefix('/') {
            let (name, _rest) = match cmd.split_once(char::is_whitespace) {
                Some((a, b)) => (a, b.trim()),
                None => (cmd, ""),
            };
            match name {
                "exit" | "quit" | "q" => break Ok(()),
                "help" | "h" => {
                    print_help();
                    continue;
                }
                "save" => {
                    match persist(handler, store, &session_id, turns, resumed_turns) {
                        Ok(()) => println!(
                            "已存到 {}",
                            store.dir().join(format!("{session_id}.json")).display()
                        ),
                        Err(e) => eprintln!("存失败: {e}"),
                    }
                    continue;
                }
                "clear" => {
                    // 开一个新会话，**不删旧的**——删是不可逆的，而"想重来"
                    // 不等于"想把刚才那段扔掉"。
                    handler.reset_chat_session();
                    turns = 0;
                    resumed_turns = 0;
                    println!("已开新会话（旧的还在，用 `chat list` 能看到）。");
                    continue;
                }
                "history" => {
                    println!("本会话 {turns} 轮。");
                    if let Some(l) = handler.chat_layout() {
                        println!("  历史消息 {} 条。", l.history_len());
                    }
                    continue;
                }
                other => {
                    eprintln!("未知命令 `/{other}`。`/help` 看有哪些。");
                    continue;
                }
            }
        }

        // ---- 一轮对话 ----
        let routing = handler.route_for(input, effort);
        println!(
            "  [{}·{}]",
            routing.spec.model,
            if routing.thinking {
                "思考"
            } else {
                "不思考"
            }
        );

        match handler.chat_turn(&routing, input, &mut records) {
            Ok(text) => {
                println!();
                println!("{text}");
                println!();
                turns += 1;

                // **每轮落盘。** 退出时才存的话，一次崩溃就丢掉整场对话——
                // 而"丢失"在这里尤其难接受：你刚解释完背景。
                if let Err(e) = persist(handler, store, &session_id, turns, resumed_turns) {
                    eprintln!("  （警告：这一轮没能存盘：{e}）");
                }
                if turns as usize >= MAX_TURNS {
                    println!(
                        "  提示：本会话已达 {MAX_TURNS} 轮上限，最老的几轮不再保留。\
                         \n  `/save` 后 `/clear` 开新的会更清爽。"
                    );
                }
            }
            Err(e) => {
                // **一轮失败不该结束整场对话。** 网络抖一下、
                // 模型返回 400——换个说法再试就是了。
                eprintln!("这一轮失败了: {e}");
                eprintln!("（会话还在，直接说下一句就行。）");
            }
        }
        let _ = resumed_turns;
    };

    // ---- 退出时再存一次（把最后的轮次数写准）----
    if turns > 0 {
        match persist(handler, store, &session_id, turns, resumed_turns) {
            Ok(()) => println!("会话已存：{session_id}（{turns} 轮）"),
            Err(e) => eprintln!("退出时存盘失败: {e}"),
        }
        println!("接着聊：yunxi-bot chat --resume {session_id}");
    }
    outcome.map(|_| 0)
}

/// 落盘。
fn persist(
    handler: &ChatHandler,
    store: &SessionStore,
    id: &str,
    turns: u32,
    _resumed: u32,
) -> Result<(), String> {
    let Some(layout) = handler.chat_layout() else {
        // 一轮都没聊过——不写空会话。写了会让 `chat list` 里多出一堆
        // 点进去什么都没有的条目。
        return Ok(());
    };
    let now = yunxi_bot_core::now_millis().map_err(|e| e.to_string())?;
    let created = store.load(id).map(|f| f.created_at).unwrap_or(now);
    let file = SessionFile {
        yunxi_bot_session: SESSION_FORMAT_VERSION,
        id: id.to_string(),
        created_at: created,
        updated_at: now,
        turns: turns.min(MAX_TURNS as u32),
        fingerprint: layout.fingerprint(),
        layout: layout.clone(),
    };
    store.save(&file).map_err(|e| e.to_string())
}

fn print_help() {
    println!("命令：");
    println!("  /help              看这个");
    println!("  /save              立刻存盘（其实每轮都自动存）");
    println!("  /history           看本会话有多少轮");
    println!("  /clear             开一个新会话（旧的保留）");
    println!("  /exit              退出（等价于 Ctrl+D）");
    println!();
    println!("其他任何输入都会进模型。");
}

/// `yunxi-bot chat list` —— 列出已有会话。
pub fn list_sessions(store: &SessionStore) -> Result<i32, Box<dyn std::error::Error>> {
    let all = store.list().map_err(|e| e.to_string())?;
    if all.is_empty() {
        println!("还没有任何会话。");
        println!("开始一个：yunxi-bot chat");
        return Ok(0);
    }
    println!("共 {} 个会话（最近的在最前）：", all.len());
    println!();
    for f in &all {
        let when = chrono::DateTime::from_timestamp_millis(f.updated_at as i64)
            .map(|t| {
                use chrono::TimeZone;
                chrono::Local
                    .from_utc_datetime(&t.naive_utc())
                    .format("%m-%d %H:%M")
                    .to_string()
            })
            .unwrap_or_else(|| "?".into());
        println!("  {:<28} {:>4} 轮   {}", f.id, f.turns, when);
    }
    println!();
    println!("接着聊：yunxi-bot chat --resume <id>");
    Ok(0)
}
