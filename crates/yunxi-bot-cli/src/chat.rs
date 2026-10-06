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

/// 压缩时保留最近多少条消息。
///
/// 12 条大约覆盖"最近三四轮对话"——够让模型接得上刚才在说什么，
/// 又不至于让压缩白做（留太多就等于没压）。
pub const KEEP_RECENT_MESSAGES: usize = 12;

/// 用模型做摘要。
///
/// ## 为什么摘要要调模型
///
/// 兜底方案（`DropMarker`）只说"这里少了 40 条"，模型拿不到任何内容——
/// 于是它会重新问你已经说过的事。模型摘要贵一次调用，
/// 但换来的是**压缩之后对话还能接得上**。
///
/// 失败时如实返回 `Err`，由 `PromptLayout::compact` 退回兜底——
/// **压缩本身不能失败**，它被触发时我们已经接近上限了。
pub struct ModelSummarizer {
    thinker: std::sync::Arc<yunxi_bot_core::think::OpenAiThinker>,
}

impl ModelSummarizer {
    pub fn new(thinker: std::sync::Arc<yunxi_bot_core::think::OpenAiThinker>) -> Self {
        Self { thinker }
    }
}

impl yunxi_bot_core::think::context::Summarizer for ModelSummarizer {
    fn summarize(&self, dropped: &[yunxi_bot_core::think::Message]) -> Result<String, String> {
        use yunxi_bot_core::think::{Message, ThinkRequest, Thinker};

        // 把要丢的消息拼成一段文本交给模型。**工具调用要说出工具名**——
        // "调用了 web_search" 比 "调用了某工具" 有用得多。
        let mut body = String::new();
        for m in dropped {
            if !m.tool_calls.is_empty() {
                let names: Vec<String> = m
                    .tool_calls
                    .iter()
                    .filter_map(|tc| {
                        tc.get("function")
                            .and_then(|f| f.get("name"))
                            .and_then(|n| n.as_str())
                            .map(str::to_string)
                    })
                    .collect();
                body.push_str(&format!("[调用了工具：{}]\n", names.join("、")));
            }
            if !m.content.trim().is_empty() {
                body.push_str(&m.content);
                body.push('\n');
            }
        }

        let req = ThinkRequest {
            messages: vec![
                Message::system(
                    "你在压缩一段即将被丢弃的对话历史。\
                     写一段简洁的摘要，保留：① 使用者说过的偏好与约束 \
                     ② 已经做了什么、结果如何 ③ 还没做完的事 \
                     ④ 重要的具体值（路径、文件名、数字）。\
                     丢掉寒暄和重复。直接给摘要，不要开场白。",
                ),
                Message::user(body),
            ],
            max_tokens: Some(800),
            temperature: None,
            thinking: Some(yunxi_bot_core::think::Thinking::Disabled),
            tools: Vec::new(),
        };

        // **摘要也要走限流等待。**
        //
        // 这个是端到端测试抓到的：压缩发生在对话中途，而那时往往
        // 正好卡在 10 RPM 上——绕过等待直接调，摘要必然被限流打掉，
        // 于是每次都退到兜底（有损），使用者看到的是"它怎么不记得了"。
        //
        // D17 已经定过"本地限流不是失败"：等一等再发。摘要没有理由例外。
        let resp = crate::chat_handler::retry_throttled(
            || self.thinker.think(&req),
            |wait| {
                eprintln!("  （摘要调用遇到本地限流，等 {} ms）", wait.as_millis());
            },
        )
        .map_err(|e| e.to_string())?;
        Ok(resp.content)
    }
}

/// 超预算就压。**摘要用模型；失败退回兜底。**
fn compact_now(
    handler: &mut ChatHandler,
    budget: &yunxi_bot_core::think::context::ContextBudget,
) -> Option<yunxi_bot_core::think::context::Compaction> {
    // 用量还没到就别白花一次调用
    let usage = handler.chat_usage()?;
    if !budget.should_compact(&usage) {
        return None;
    }
    // 用哪个模型做摘要：优先便宜的那个（摘要是简单活），
    // 拿不到就退回兜底——**压缩不能失败**。
    let summarizer: Box<dyn yunxi_bot_core::think::context::Summarizer> =
        match handler.summarizer_thinker() {
            Some(t) => Box::new(ModelSummarizer::new(t)),
            None => Box::new(yunxi_bot_core::think::context::DropMarker),
        };
    handler.compact_if_needed(budget, summarizer.as_ref(), KEEP_RECENT_MESSAGES)
}

/// 跑一个交互式会话。
///
/// 返回退出码。
pub fn run(
    handler: &mut ChatHandler,
    store: &SessionStore,
    session_id: String,
    resume: bool,
    effort: ReasoningEffort,
    ledger: Option<&mut yunxi_bot_core::Ledger>,
    budget: yunxi_bot_core::think::context::ContextBudget,
) -> Result<i32, Box<dyn std::error::Error>> {
    let mut ledger = ledger;
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

    // **启动时把规则说清楚。** 不说的话，使用者不知道它读到了什么——
    // 而"它怎么知道这个项目的测试命令"和"它怎么不知道"都要有答案。
    let rs = handler.project_rules();
    if !rs.is_empty() {
        println!("{}", rs.summary());
    }

    if turns == 0 {
        println!("新会话：{session_id}");
        println!("  （输入内容开始；`/help` 看命令，`/exit` 退出）");
    }
    println!();

    // **只有交互式这条链路才流式。**
    //
    // 常驻循环、定时任务、测试都不需要"边生成边显示"——
    // 而它们为它付的代价是：多一条 SSE 解析路径、多一种失败模式。
    //
    // 增量直接写 stdout 并**立刻 flush**：不 flush 的话内容会攒在
    // 缓冲区里，等整段结束才一起出来——那就完全失去了流式的意义
    // （看起来和加这个功能之前一模一样）。
    let streamed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let streamed_flag = streamed.clone();
    handler.set_on_delta(std::sync::Arc::new(move |t: &str| {
        use std::io::Write;
        // 第一段增量到达之前才需要"准备开始输出"，
        // 之后就是纯粹的追加
        streamed_flag.store(true, std::sync::atomic::Ordering::SeqCst);
        print!("{t}");
        let _ = std::io::stdout().flush();
    }));

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
                "rules" => {
                    // **要能查到规则从哪来的。** 模型说"按项目约定应该……"
                    // 时，使用者得能核实那指的是哪一条。
                    let rs = handler.project_rules();
                    if rs.is_empty() {
                        println!("没有找到项目规则文件。");
                        println!();
                        println!("在工作目录（或它的上级）放一份 AGENTS.md，");
                        println!("它会被自动读进稳定前缀——以后不用每次重说。");
                        println!("兼容 CLAUDE.md；两者都在时 AGENTS.md 优先。");
                        println!();
                        println!("停止条件：遇到含 .git 的目录就不再往上找。");
                    } else {
                        // **顺带验证接线。** "规则加载了"和"规则真的进了前缀"
                        // 是两件事——只报前者的话，接线断了也看不出来。
                        // 这个检查是端到端测试逼出来的：规则加载正常、
                        // `/rules` 列得好好的，而模型说"系统提示词里没有这一节"。
                        let prefix = handler.expected_chat_prefix();
                        let first_line = rs.files[0]
                            .text
                            .lines()
                            .find(|l| !l.trim().is_empty())
                            .unwrap_or("");
                        println!(
                            "稳定前缀 {} 字；规则已进前缀：{}",
                            prefix.chars().count(),
                            if !first_line.is_empty() && prefix.contains(first_line) {
                                "是"
                            } else {
                                "**否——接线断了**"
                            }
                        );
                        // **指纹也报出来。**
                        //
                        // 它是"前缀没变"的机器可读表示，而"前缀变了缓存全废"
                        // 是静默的——只表现为账单变贵。报出来之后，
                        // 使用者（和测试）不用花一次模型调用就能验证它。
                        println!("前缀指纹：{:x}", handler.chat_prefix_fingerprint());
                        println!();
                        println!(
                            "加载了 {} 份规则（从远到近，近的对模型影响更大）：",
                            rs.files.len()
                        );
                        for f in &rs.files {
                            println!(
                                "  [{:<4}] {}{}",
                                f.scope.label(),
                                f.path.display(),
                                if f.truncated { "（已截断）" } else { "" }
                            );
                            let first = f.text.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
                            println!("          {}", first.chars().take(60).collect::<String>());
                        }
                    }
                    continue;
                }
                "usage" => {
                    // **把估价和"是不是精确"一起显示。**
                    // 只说一个数字会让人以为那是准确值——而它多数时候是估算。
                    match handler.chat_usage() {
                        Some(u) => {
                            println!(
                                "上下文：约 {} token{}（窗口 {}，{} 时压缩）",
                                u.estimated_tokens(),
                                if u.is_exact() {
                                    "（精确，来自上次响应）"
                                } else {
                                    "（估算）"
                                },
                                budget.window_tokens,
                                budget.trigger_at()
                            );
                            if let Some(a) = u.anchor_tokens {
                                println!("  锚点：{a} token（上一次响应报告的真实值）");
                            } else {
                                println!("  锚点：还没有（纯估算，误差会大一些）");
                            }
                        }
                        None => println!("还没有会话。"),
                    }
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

        // ---- 压缩检查：**在花掉这一轮之前做** ----
        //
        // 放在这里而不是响应之后：响应回来时已经花掉了，
        // 而超限的响应本身就是失败（API 报错）。所以要在发出去之前压。
        if let Some(c) = compact_now(handler, &budget) {
            println!("  （{}）", c.summary());
            if let Some(l) = ledger.as_mut() {
                let _ = l.append(
                    yunxi_bot_core::EventKind::ContextCompacted,
                    None,
                    serde_json::json!({
                        "session": session_id,
                        "dropped_messages": c.dropped_messages,
                        "dropped_chars": c.dropped_chars,
                        "dropped_tool_calls": c.dropped_tool_calls,
                        "kept_messages": c.kept_messages,
                        "before_tokens": c.before_tokens,
                        "after_tokens": c.after_tokens,
                        "saved_tokens": c.saved_tokens(),
                        // **摘要是不是模型写的。** 兜底是有损的，
                        // 事后追查"它怎么不记得了"要靠这一位。
                        "summarized": c.summarized,
                    }),
                );
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
                // **正文已经边生成边打出来了。**
                //
                // 这里只补收尾的换行——再打一遍 `text` 会显示两遍。
                // 而"有没有流出来过"决定怎么补：纯工具调用的一轮
                // 本来就没有正文，硬打一遍空字符串会多出空行。
                println!();
                if !streamed.swap(false, std::sync::atomic::Ordering::SeqCst) {
                    println!("{text}");
                }
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

        // **把这一轮的模型调用写进台账。**
        //
        // 不写的话，对话的成本在台账里是完全隐形的——
        // `yunxi-bot cost` 会少报，缓存命中率也测不到对话这条链路。
        // 而"成本看不见"正是最容易被忽略、也最容易失控的一类问题。
        //
        // 放在 match 之外：**失败的调用也是调用**（它照样花了 token，
        // 有些还花了钱），漏掉失败的那些会让账单对不上。
        if let Some(l) = ledger.as_mut() {
            for r in records.drain(..) {
                let _ = l.append(
                    yunxi_bot_core::EventKind::ModelCalled,
                    None,
                    serde_json::json!(r),
                );
            }
        }

        // **把这一轮的模型调用写进台账。**
        //
        // 不写的话，对话的成本在台账里是完全隐形的——
        // `yunxi-bot cost` 会少报，缓存命中率也测不到对话这条链路。
        // 而"成本看不见"正是最容易被忽略、也最容易失控的一类问题。
        //
        // 放在 match 之外：**失败的调用也是调用**（它照样花了 token，
        // 有些还花了钱），漏掉失败的那些会让账单对不上。
        if let Some(l) = ledger.as_mut() {
            for r in records.drain(..) {
                let _ = l.append(
                    yunxi_bot_core::EventKind::ModelCalled,
                    None,
                    serde_json::json!(r),
                );
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
    println!("  /usage             看上下文用了多少、什么时候会压缩");
    println!("  /rules             看加载了哪些项目规则、从哪来的");
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
