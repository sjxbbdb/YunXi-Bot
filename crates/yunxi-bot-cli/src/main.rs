//! YunXi Bot 命令行入口。
//!
//! 当前里程碑：**可运行的调度原型**——任务能按触发条件自动执行、结果落台账、
//! 需批准的任务被拦下、崩溃残留被回收、状态跨进程存活。
//!
//! 外加**任务执行框架**（`do` / `tasks` / `resume`）：人类给一个目标，
//! 系统拆解、逐步执行、决策点交给本地决策模型，全程留痕。
//!
//! 以及**工具层**：模型能读文件、跑命令、抓网页，每个动作都过审批门禁。

mod approval;
mod chat;
mod chat_handler;
mod tool_sink;
mod tooling;

use std::path::PathBuf;
use std::thread;
use std::time::Duration;

use yunxi_bot_core::decide::Decider;
use yunxi_bot_core::job::{JobId, JobSpec, JobState, Trigger};
use yunxi_bot_core::ledger::{EventKind, Ledger};
use yunxi_bot_core::task::engine::{LedgerTaskStore, TaskStore};
use yunxi_bot_core::think::{ModelSpec, ReasoningEffort, TaskKind, TaskProfile};
use yunxi_bot_core::{TickOptions, now_millis, policy, tick};

/// 数据目录。**委托给内核那一份**，不在这里重算。
///
/// 这里原本自己算了一遍（`YUNXI_BOT_HOME` → `%LOCALAPPDATA%\YunXiBot` →
/// `~/.yunxi-bot`），而 sidecar 在 Python 里又算了一遍。两边算法看起来一样，
/// 但只要有一处漏了对齐，表现就是最难查的那类错：sidecar 说"没配置"，
/// 而使用者的配置文件明明就在那儿。
///
/// 端到端测试抓到过一次真实的路径不一致——所以现在只有一份定义。
/// 建工具调用 sink。**打不开台账时要说清楚，但不拒绝干活。**
///
/// 理由：为了留痕而拒绝服务是本末倒置。但也不能静默——"这次没有工具记录"
/// 是使用者需要知道的事，所以打到 stderr。
fn tool_sink_or_warn() -> std::sync::Arc<dyn yunxi_bot_core::tool::ToolCallSink> {
    match tool_sink::LedgerToolSink::open(ledger_path()) {
        Ok(s) => std::sync::Arc::new(s),
        Err(e) => {
            eprintln!("警告：工具调用写不进台账（{e}）——改为打到标准错误");
            std::sync::Arc::new(tool_sink::StderrToolSink)
        }
    }
}
fn default_home() -> PathBuf {
    yunxi_bot_core::default_home()
}

fn ledger_path() -> PathBuf {
    default_home().join("ledger.jsonl")
}

/// 单实例锁文件路径。与台账同目录，便于一起备份/清理。
fn lock_path() -> PathBuf {
    default_home().join("daemon.lock")
}

const USAGE: &str = "\
YunXi Bot —— 陪伴型通用常驻 Agent 助理

用法:
  yunxi-bot add <名称> [选项] -- <命令...>   创建任务
      --every <秒>          固定间隔触发
      --cron \"分 时 日 月 周\"   cron 触发（本地时间）
      --watch <路径>        文件变化触发
      --irreversible        标记不可逆：每次运行都需人工批准
      --cwd <目录>          工作目录
      --timeout <毫秒>      单次执行超时
      --max-attempts <n>    最大尝试次数（默认 1）

  yunxi-bot tick              跑一个调度回合后退出
  yunxi-bot daemon [选项]     常驻循环
      --interval <毫秒>     每轮间隔（默认 5000）
      --max-ticks <n>       跑够 n 轮就停（便于演示，默认无限）
      --require-os-isolation  要求 OS 级写入隔离，拿不到就拒绝执行

  yunxi-bot install-autostart    注册当前用户登录时自启
  yunxi-bot uninstall-autostart  取消自启
  yunxi-bot autostart-status     查看自启注册状态

  yunxi-bot isolation-check      隔离自检：真起受限子进程验证写入隔离是否生效
  yunxi-bot think <提示词>     调一次思考模型（Agnes），验证连通性与用量
  yunxi-bot agent [选项]      跑一个完整的 Agent 回合（判断 + 表达）
      --hour <0-23>         假定当前小时（用于观察安静时段的影响）
      --no-think            只判断不表达（不消耗远端配额）
  yunxi-bot remember <内容>   记一条记忆
      --kind <类别>         fact / preference / relationship / event（默认 fact）
  yunxi-bot journal [-n N]    查看最近的决策记录（默认 10）
  yunxi-bot supervise [选项]   监督模式：daemon 异常退出时自动重启
      --interval <毫秒>     传给 daemon 的轮间隔（默认 5000）
      --max-restarts <n>    放弃前最多重启几次（默认 10）

  yunxi-bot run <id>          立即执行一次（忽略触发条件）
  yunxi-bot list              列出全部任务

  —— 任务执行框架（给目标，不是给命令）——
  yunxi-bot do \"<目标>\"       拆解目标、逐步执行、完成一个做一个
      --budget <n>          模型调用次数上限（默认 20）
      --max-steps <n>       拆解出的步骤上限（默认 12）
      --dry-run             不调模型，只展示会怎么拆分与路由（不花钱）
      --show-prompt         打印三段稳定前缀（核对缓存前提，不花钱）
      --yes                 自动批准全部工具调用（危险，只给自动化用）
      --allow <工具[:粒度]>  预批准，可重复。例：--allow read_file:D:\\notes
      --deny <工具[:粒度]>   拒绝，优先级高于 --allow，可重复
      --provider <名>       强制全程用一个模型：agnes / deepseek
      --thinking <模式>      auto（默认，按任务类型）/ on / off
      --id <名字>           指定任务 id（默认自动生成）
  yunxi-bot notify \"<标题>\" [\"<正文>\"]  发一条桌面通知（同时验证通知出口通不通）
      --tag <标签>          替换同类通知而不是堆积
      --console             强制走控制台出口（验证出口可换）
  yunxi-bot check [选项]      看信息源 -> 判断 -> 该通知你的就通知你（落台账）
      --limit <n>           判定几封（默认 20）
      --dry-run             只判定不真发通知（台账照记）
      --no-notify           完全不出通知，只看判定结果
      --console             通知打到终端
  yunxi-bot feedback [选项]   表个态：--last --never / --read / --ignored
  yunxi-bot mail [选项]       看未读邮件（只读，不会标成已读）
      --limit <n>           取几封（默认 20）
      --read <uid>          看某封的正文
      --port <n>            sidecar 端口（默认 17871）
  yunxi-bot chat [选项]       交互式会话（通用 agent 的入口）
      --resume [--id <名字>]  接着上次聊；不给 --id 就接最近那个
      chat list             看有哪些会话
      --thinking auto|on|off 思考模式
      --context-window <n>   上下文窗口（token，默认按模型定）
  yunxi-bot mcp list          连上配置的 MCP server 并列出它的工具
  yunxi-bot mcp call <s> <t>  调一个 MCP 工具（--args '{...}'）
  yunxi-bot tools             列出工具及其能力类别（不需要模型）
  yunxi-bot tasks             列出执行框架里的任务及其步骤
  yunxi-bot tasks <id>        看一个任务的步骤明细
  yunxi-bot resume <id>       续跑一个停在半路的任务

  yunxi-bot approve <id>      批准待批准任务
  yunxi-bot log [-n N]        显示最近 N 条台账事件（默认 20）
  yunxi-bot status            数据目录与台账概况
  yunxi-bot cost [选项]       模型调用花销（按台账重算）
      --calls <n>           显示最近 n 次调用明细（默认不显示）
  yunxi-bot policy            列出权限预设
  yunxi-bot decide [选项]     演示决策层
      --demo                只打印六类降级方向表（不需要模型）
      --endpoint <URL>      决策模型 sidecar 地址
                            （默认 http://127.0.0.1:17870/decide）

  yunxi-bot companion [选项]  演示陪伴层：模型判断 + 约束收紧
      --demo                用桩模型演示约束如何把「开口」压下来（不需要真模型）
      --endpoint <URL>      真实调用决策模型

  yunxi-bot help              显示本帮助
";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    // 隐藏模式：隔离自检的 canary 子进程。
    // 它由受限令牌启动，只做一件事——分别尝试写「允许」与「禁止」的路径。
    // 父进程检查的是**文件是否真的被创建**，而不是本进程的自我报告：
    // 报告可以撒谎，文件系统状态不会。
    #[cfg(windows)]
    if args.first().map(String::as_str) == Some("__canary-write") {
        // 把每一步的真实结果打到 stdout：父进程靠"文件是否真的存在"判定隔离，
        // 这里的结果用来诊断**为什么**失败。静默忽略错误会让排查无从下手。
        for p in &args[1..] {
            match std::fs::write(p, b"canary") {
                Ok(()) => println!("OK   {p}"),
                Err(e) => println!("FAIL {p} -> {e} (kind={:?})", e.kind()),
            }
        }
        std::process::exit(0);
    }

    let code = match run(&args) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("错误: {e}");
            1
        }
    };
    std::process::exit(code);
}

fn run(args: &[String]) -> Result<i32, Box<dyn std::error::Error>> {
    let Some(cmd) = args.first().map(String::as_str) else {
        print!("{USAGE}");
        return Ok(0);
    };
    let rest = &args[1..];
    match cmd {
        "help" | "-h" | "--help" => {
            print!("{USAGE}");
            Ok(0)
        }
        "status" => cmd_status(),
        "list" => cmd_list(),
        "add" => cmd_add(rest),
        "approve" => cmd_approve(rest),
        "policy" => cmd_policy(),
        "decide" => cmd_decide(rest),
        "companion" => cmd_companion(rest),
        "isolation-check" => cmd_isolation_check(),
        "think" => cmd_think(rest),
        "agent" => cmd_agent(rest),
        "remember" => cmd_remember(rest),
        "journal" => cmd_journal(rest),
        "install-autostart" => cmd_autostart(AutostartAction::Install),
        "uninstall-autostart" => cmd_autostart(AutostartAction::Uninstall),
        "autostart-status" => cmd_autostart(AutostartAction::Status),
        "tick" => cmd_tick(),
        "daemon" => cmd_daemon(rest),
        "supervise" => cmd_supervise(rest),
        "run" => cmd_run(rest),
        "do" => cmd_do(rest),
        "tasks" => cmd_tasks(rest),
        "tools" => cmd_tools(rest),
        "notify" => cmd_notify(rest),
        "mail" => cmd_mail(rest),
        "chat" => cmd_chat(rest),
        "check" => cmd_check(rest),
        "mcp" => cmd_mcp(rest),
        "feedback" => cmd_feedback(rest),
        "resume" => cmd_resume(rest),
        "cost" => cmd_cost(rest),
        "log" => cmd_log(rest),
        other => {
            eprintln!("未知命令: {other}\n");
            print!("{USAGE}");
            Ok(2)
        }
    }
}

// ——————————————————— 展示辅助 ———————————————————

fn state_tag(s: JobState) -> &'static str {
    match s {
        JobState::Pending => "待运行",
        JobState::WaitingApproval => "待批准",
        JobState::Running => "运行中",
        JobState::Succeeded => "上次成功",
        JobState::Failed => "上次失败",
        JobState::Disabled => "已停用",
    }
}

fn trigger_label(t: &Trigger) -> String {
    match t {
        Trigger::Manual => "手动".into(),
        Trigger::Every { seconds } => format!("每 {seconds}s"),
        Trigger::Cron { expr } => format!("cron({expr})"),
        Trigger::Watch { path } => format!("监听({path})"),
    }
}

// ——————————————————— 命令实现 ———————————————————

fn cmd_status() -> Result<i32, Box<dyn std::error::Error>> {
    let l = Ledger::open(ledger_path())?;
    let jobs = l.rebuild();
    println!("数据目录 : {}", default_home().display());
    println!("台账文件 : {}", l.path().display());
    println!("事件总数 : {}", l.len());
    println!("任务总数 : {}", jobs.len());
    if !jobs.is_empty() {
        let mut counts: std::collections::BTreeMap<&'static str, usize> = Default::default();
        for j in jobs.values() {
            *counts.entry(state_tag(j.state)).or_default() += 1;
        }
        println!("\n状态分布:");
        for (k, v) in counts {
            println!("  {k:<8} {v}");
        }
    }
    Ok(0)
}

fn cmd_list() -> Result<i32, Box<dyn std::error::Error>> {
    let l = Ledger::open(ledger_path())?;
    let jobs = l.rebuild();
    if jobs.is_empty() {
        println!("（暂无任务）");
        return Ok(0);
    }
    for (id, j) in &jobs {
        let flag = if j.spec.irreversible {
            " ⚠不可逆"
        } else {
            ""
        };
        println!(
            "{:<18} {:<8} {:<14} {:<22}{}  尝试 {}/{}  批准 {}",
            id,
            state_tag(j.state),
            j.spec.name,
            trigger_label(&j.spec.trigger),
            flag,
            j.attempts,
            j.spec.max_attempts,
            j.approvals
        );
        if let Some(e) = &j.last_error {
            println!("{:>18}   └ 上次错误: {e}", "");
        }
    }
    Ok(0)
}

fn flag<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .map(String::as_str)
}

fn cmd_add(args: &[String]) -> Result<i32, Box<dyn std::error::Error>> {
    let Some(name) = args.first().filter(|s| !s.starts_with("--")) else {
        eprintln!("缺少任务名称。");
        return Ok(2);
    };
    let Some(sep) = args.iter().position(|a| a == "--") else {
        eprintln!("缺少命令：请在 `--` 之后给出要执行的命令。");
        return Ok(2);
    };
    let command: Vec<String> = args[sep + 1..].to_vec();
    if command.is_empty() {
        eprintln!("`--` 之后没有命令。");
        return Ok(2);
    }

    let trigger = if let Some(s) = flag(args, "--every") {
        Trigger::Every {
            seconds: s.parse()?,
        }
    } else if let Some(e) = flag(args, "--cron") {
        if yunxi_bot_core::trigger::parse_cron(e).is_none() {
            eprintln!("cron 表达式非法: {e:?}（需要 5 个字段：分 时 日 月 周）");
            return Ok(2);
        }
        Trigger::Cron { expr: e.into() }
    } else if let Some(p) = flag(args, "--watch") {
        Trigger::Watch { path: p.into() }
    } else {
        Trigger::Manual
    };

    let spec = JobSpec {
        name: name.clone(),
        command,
        cwd: flag(args, "--cwd").unwrap_or(".").into(),
        trigger,
        irreversible: args.iter().any(|a| a == "--irreversible"),
        max_attempts: flag(args, "--max-attempts")
            .map(str::parse)
            .transpose()?
            .unwrap_or(1),
        timeout_ms: flag(args, "--timeout")
            .map(str::parse)
            .transpose()?
            .unwrap_or(60_000),
    };

    let id = JobId::new(format!(
        "{:x}{:04x}",
        now_millis()?,
        std::process::id() & 0xffff
    ));
    let mut l = Ledger::open(ledger_path())?;
    l.append(
        EventKind::JobCreated,
        Some(&id),
        serde_json::json!({ "spec": spec }),
    )?;

    println!("已创建任务 {id}");
    println!("  名称 : {}", spec.name);
    println!("  触发 : {}", trigger_label(&spec.trigger));
    println!("  命令 : {}", spec.command.join(" "));
    if spec.irreversible {
        println!("  ⚠ 已标记不可逆：每次运行前都需要 `approve`");
    }
    Ok(0)
}

fn cmd_approve(args: &[String]) -> Result<i32, Box<dyn std::error::Error>> {
    let Some(id) = args.first() else {
        eprintln!("缺少任务 id。");
        return Ok(2);
    };
    let mut l = Ledger::open(ledger_path())?;
    let jid = JobId::new(id.clone());
    if !l.rebuild().contains_key(&jid) {
        eprintln!("找不到任务 {id}");
        return Ok(2);
    }
    l.append(
        EventKind::JobApproved,
        Some(&jid),
        serde_json::json!({"by":"cli"}),
    )?;
    println!("已批准 {id}");
    Ok(0)
}

fn cmd_tick() -> Result<i32, Box<dyn std::error::Error>> {
    let mut l = Ledger::open(ledger_path())?;
    let r = tick(&mut l, &TickOptions::default())?;
    print_report(&r);
    Ok(0)
}

fn print_report(r: &yunxi_bot_core::TickReport) {
    if r.is_quiet() {
        println!("本轮无动静（检查 {} 个任务）", r.checked);
        return;
    }
    println!(
        "检查 {checked}｜到期 {due}｜单飞跳过 {skip}｜待批准 {appr}｜成功 {ok}｜失败 {bad}｜残留回收 {stale}",
        checked = r.checked,
        due = r.due,
        skip = r.single_flight_skipped,
        appr = r.needs_approval,
        ok = r.succeeded,
        bad = r.failed,
        stale = r.stale_recovered,
    );
}

/// 任务回合：**daemon 自己把没人管的任务推进下去。**
///
/// ## 这是 goal 开头点名的那条断链
///
/// > 任务卡住后**没有任何东西自动推进它**
///
/// 在此之前任务只能由 `yunxi-bot do` 同步跑完；进程被杀、或跑到
/// 「等人工」之后，唯一的推进方式是人在终端敲 `resume`。
///
/// ## 两条边界都写在调度层（`task::schedule`）
///
/// 1. **不碰 `AwaitingHuman`** —— 自动续跑它等于自动批准。
///    这类任务只会被**报出来**（下面那段 `attention_needed`），
///    而不是替他做决定。
/// 2. **不碰刚刚还在动的任务** —— 使用者在终端里跑时守护也在看同一本台账，
///    没有这道闸两边会同时执行同一批步骤（而步骤可能有副作用）。
///
/// ## 无人值守 = 不获得新权限
///
/// 这里构造的审批者是 `RefusingApprover`：**凡是需要批准的动作一律拒绝**。
/// 守护进程拿不到 stdin，用 `StdinApprover` 会直接卡死；而"自动批准"
/// 更不可接受——那等于让一个无人看管的进程自己给自己发权限。
///
/// 所以守护能做的事 = 使用者在 `--allow` 里预先写明的那些。
/// 其余一律失败并留痕——**失败是正确结果，不是缺陷**。
fn run_task_round<D: yunxi_bot_core::decide::Decider>(
    ledger: &mut Ledger,
    router: &yunxi_bot_core::think::ModelRouter,
    decider: &D,
    handler: &mut chat_handler::ChatHandler,
    notifier: &dyn yunxi_bot_core::notify::Notifier,
    cfg: &TaskRound,
    tick_no: u64,
) {
    use yunxi_bot_core::task::engine::{Engine, LedgerTaskStore};
    use yunxi_bot_core::task::schedule::{AutoAdvance, attention_needed, auto_advance_candidates};

    let TaskRound {
        idle_ms,
        budget,
        dry_run,
    } = *cfg;

    let Ok(now) = now_millis() else {
        eprintln!("[{tick_no:>3}] 任务回合跳过：取不到当前时间");
        return;
    };

    // **必须重读台账，不能信 `events()`。**
    //
    // `events()` 是**开台账那一刻的快照**。任务引擎在另一本台账上写状态，
    // 而守护手里这本永远看不到那些写入——于是每轮都按最初那份快照决策，
    // 一个已经 Stalled 的任务会被无限重试。
    //
    // 这个 bug 是被端到端测试抓到的：日志里同一个任务每轮都"卡住"一次。
    if let Err(e) = ledger.reload() {
        println!("[{tick_no:>3}] 任务：读不到台账（{e}）");
        return;
    }
    let events = ledger.events().to_vec();
    let tasks = yunxi_bot_core::ledger::task_from_events(&events);

    // —— 需要人的那些：报出来 ——
    //
    // **自动推进不该碰它们，但不该瞒着使用者。**
    // 这两类任务在台账里静静躺着，而使用者不会主动去查。
    for (t, why) in attention_needed(&tasks) {
        // 同一个任务只提醒一次。反复报同一件事会让噪音淹没真信号。
        let already = events.iter().any(|e| {
            e.kind == EventKind::TaskAttentionNotified
                && e.data.get("task").and_then(|v| v.as_str()) == Some(t.id.as_str())
        });
        if already {
            continue;
        }
        let n = yunxi_bot_core::notify::Notification::new(
            "有任务在等你",
            format!("「{}」{}", truncate_chars(&t.goal, 40), why),
        )
        .with_tag(format!("task.{}", t.id))
        .with_urgency(yunxi_bot_core::notify::Urgency::Normal);

        let d = if dry_run {
            yunxi_bot_core::notify::Delivery::Blocked {
                reason: "演练模式：没有真的发通知".into(),
            }
        } else {
            notifier.notify(&n)
        };
        println!("[{tick_no:>3}] 任务：{} {} —— {}", t.id, why, d.label());
        let _ = ledger.append(
            EventKind::TaskAttentionNotified,
            None,
            serde_json::json!({
                "task": t.id,
                "state": format!("{:?}", t.state),
                "why": why,
                "result": d.label(),
                "confirmed": d.can_claim_user_notified(),
                "dry_run": dry_run,
            }),
        );
    }

    // —— 能推进的那些：推进 ——
    let candidates = auto_advance_candidates(
        &tasks,
        &events,
        AutoAdvance {
            idle_ms,
            now_ms: now,
        },
    );
    if candidates.is_empty() {
        return;
    }

    let mut store = LedgerTaskStore::new(match Ledger::open(ledger.path()) {
        Ok(l) => l,
        Err(e) => {
            println!("[{tick_no:>3}] 任务：打不开台账（{e}）");
            return;
        }
    });

    for id in candidates {
        // 每个任务单独跑一轮。**一个失败不影响下一个**——
        // 一个坏任务不该让整批停摆。
        let out = {
            let mut engine = Engine::new(router, decider, handler, &mut store, budget);
            engine.run(&id)
        };
        match out {
            Ok(o) => println!(
                "[{tick_no:>3}] 任务 {}：{}（{}，用了 {} 次模型调用）",
                id,
                o.task.state.label(),
                truncate_chars(&o.task.goal, 30),
                o.used_model_calls
            ),
            Err(e) => {
                // **失败要说出来，但不打断守护。** 一个任务反复失败时，
                // 使用者需要看到它，而不是让守护悄悄退出。
                println!("[{tick_no:>3}] 任务 {id} 推进失败：{e}");
            }
        }
    }
}

/// 任务回合的固定配置。
#[derive(Clone, Copy)]
struct TaskRound {
    idle_ms: u64,
    budget: yunxi_bot_core::task::Budget,
    dry_run: bool,
}

/// 助理回合的固定配置。启动时定好，每轮不变。
///
/// 打包成一个结构不是为了少敲几个字，而是因为**这五项是一组**：
/// 它们共同定义"看哪个源、怎么通知、按谁的规则"。
/// 散成五个参数时，调用点很容易把 `limit` 和 `dry_run` 传反——
/// 而那样的错误不会编译失败，只会让行为悄悄不对。
struct AssistantRound<'a> {
    source: &'a yunxi_bot_core::info::mail::MailSource,
    notifier: &'a dyn yunxi_bot_core::notify::Notifier,
    home: &'a std::path::Path,
    limit: usize,
    dry_run: bool,
}

/// 助理回合：**daemon 自己看一遍信息源，有事才叫你。**
///
/// ## 这是"常驻"两个字的落点
///
/// 在此之前，整条助理链路只能靠人手敲 `yunxi-bot check` 触发——
/// 那不叫常驻，那叫"你问我答"。这一轮让它在**你不问的时候也看着**。
///
/// ## 四件必须做对的事
///
/// 1. **失败不能杀死守护进程。** 邮件服务器抖一下、sidecar 没起来，
///    都不该让一个跑了三天的常驻进程退出。报一行、下一轮再试。
/// 2. **信息源挂了要能自己恢复。** 这一条是端到端测试逼出来的：
///    最初的写法在**启动时**探一次健康，探不到就把通道永久关掉——
///    于是"sidecar 还没起来"变成了"这个守护再也不看邮件了"。
///    对一个要跑几天的进程，那是错的。现在每轮都探，
///    **只在状态变化时打印**（好→坏、坏→好），既不刷屏也不会永久失能。
/// 3. **不重复打扰。** 这是 `run_pass` 里 `already_seen` 的活——
///    手动跑一次重复通知只是烦一下，常驻跑就是反复弹到你读掉为止。
/// 4. **无事发生时不刷屏。** 每 5 分钟打一行"没有新邮件"，
///    代价不是难看，是**你会开始不看它**——然后真正重要的那行也被漏掉。
fn run_assistant_round(
    ledger: &mut Ledger,
    engine: &mut yunxi_bot_core::decide::DecisionEngine<impl yunxi_bot_core::decide::Decider>,
    cfg: &AssistantRound<'_>,
    tick_no: u64,
    health_ok: &mut Option<bool>,
) {
    use chrono::Timelike;
    use yunxi_bot_core::assistant::{PassInput, run_pass};
    use yunxi_bot_core::companion::CompanionPolicy;
    use yunxi_bot_core::info::InfoSource;
    use yunxi_bot_core::memory::Situation;
    use yunxi_bot_core::triage::load_policy;

    let AssistantRound {
        source,
        notifier,
        home,
        limit,
        dry_run,
    } = *cfg;

    // **每轮都探一次信息源。** 见上面第 2 条：启动时探一次就永久关掉
    // 是错的——sidecar 可能只是还没起来。
    let now_healthy = source.health().is_ok();
    match (*health_ok, now_healthy) {
        (Some(false), true) => {
            println!("[{tick_no:>3}] 信件：信息源恢复了，继续看着");
        }
        (Some(true), false) => {
            // 报出来但**不停**：下一轮还会再试。运行中挂掉比启动时没起来更值得说。
            if let Err(e) = source.health() {
                println!("[{tick_no:>3}] 信件：信息源断了（{e}），会继续重试");
            }
        }
        (None, false) => {
            // 首次就没探到。说一次，然后安静重试。
            if let Err(e) = source.health() {
                println!("[{tick_no:>3}] 信件：暂时用不了（{e}）——会继续重试");
            }
        }
        _ => {}
    }
    *health_ok = Some(now_healthy);
    if !now_healthy {
        return;
    }

    // **每轮重读策略。** 常驻进程不该要求重启才能生效——
    // 你说一句"以后别烦我"，下一轮就该算数。文件很小，读一次不值一提。
    let policy = match load_policy(home) {
        Ok(p) => p,
        Err(e) => {
            // 策略坏了**不能退回默认值继续跑**——那会让你以为规则生效了。
            // 但也不能杀进程：打印出来，这一轮按默认（更保守方向不成立，
            // 所以只报错不猜），下一轮再试。
            println!("[{tick_no:>3}] 助理：策略文件有问题，本轮跳过（{e}）");
            return;
        }
    };

    let Ok(now) = now_millis() else {
        eprintln!("[{tick_no:>3}] 助理回合跳过：取不到当前时间");
        return;
    };

    // **今天已经打扰过几次要每轮重算。** 写死 0 的话，
    // 每日上限这道闸就永远不会合上——使用者会被无限打扰。
    let ctx = Situation {
        relationship_stage: "初期".into(),
        minutes_since_last_interaction: 999,
        interventions_today: interventions_today(ledger, now),
        ..Default::default()
    };

    let mut input = PassInput {
        source,
        engine,
        notifier,
        policy,
        companion: CompanionPolicy::default(),
        ctx,
        local_hour: chrono::Local::now().hour() as u8,
        now_ms: now,
        limit,
        dry_run,
    };

    match run_pass(ledger, &mut input) {
        Ok(r) => {
            // **无事发生就不吭声。** 见上面第 3 条。
            if r.is_quiet() && r.held == 0 && r.silent == 0 {
                return;
            }
            println!("[{tick_no:>3}] 助理：{}", r.summary());
            if !r.is_complete() {
                // **"没看全"必须说出来。** 一个助理说"没有新邮件"而其实
                // 有 3 封没读到，比它不说更糟——你会因此不再自己去看。
                println!(
                    "[{tick_no:>3}]       注意：这一轮没看全，有 {} 条取不到",
                    r.skipped
                );
            }
        }
        Err(e) => {
            // 失败不杀进程：邮件服务器抖一下不该让跑了几天的守护退出。
            println!("[{tick_no:>3}] 助理：这一轮没取到信息（{e}）");
        }
    }
}

/// 今天（本地日）已经打扰过几次。**从台账数，不另存计数器。**
///
/// 另存一个计数器就有了两个真相，而它们迟早不一致——那时"今天到底打扰了
/// 几次"就没有答案了。台账是唯一事实来源（ADR D11）。
///
/// 判据是**确认送达**的通知，以及"已交给系统"的那些。
/// 演练模式不算——它没到你眼前，不该占用你的每日额度。
fn interventions_today(ledger: &Ledger, now_ms: u64) -> u32 {
    use yunxi_bot_core::EventKind;
    // 本地日的起点：往回退到当天 00:00
    let day_start = {
        use chrono::{Local, TimeZone};
        let now = Local.timestamp_millis_opt(now_ms as i64).single();
        match now {
            Some(t) => {
                let midnight = t
                    .date_naive()
                    .and_hms_opt(0, 0, 0)
                    .unwrap_or_else(|| t.naive_local());
                Local
                    .from_local_datetime(&midnight)
                    .single()
                    .map(|x| x.timestamp_millis() as u64)
                    .unwrap_or(now_ms)
            }
            None => now_ms,
        }
    };

    ledger
        .events()
        .iter()
        .filter(|e| e.kind == EventKind::NoticeSent && e.at >= day_start)
        .filter(|e| e.data.get("dry_run").and_then(|v| v.as_bool()) != Some(true))
        .count() as u32
}

fn cmd_daemon(args: &[String]) -> Result<i32, Box<dyn std::error::Error>> {
    let interval: u64 = flag(args, "--interval")
        .map(str::parse)
        .transpose()?
        .unwrap_or(5000);
    let max_ticks: u64 = flag(args, "--max-ticks")
        .map(str::parse)
        .transpose()?
        .unwrap_or(0);
    // 助理巡览的间隔（秒）。**0 表示关闭。**
    //
    // 按时间而不是按轮数：轮数会和 --interval 耦合，
    // `--interval 1000` 配"每 5 轮"就是每 5 秒去登一次 IMAP——
    // 那不是勤快，那是在撞服务端的连接频率限制。
    let assistant_secs: u64 = flag(args, "--assistant-interval")
        .map(str::parse)
        .transpose()?
        .unwrap_or(300);
    let assistant_dry: bool = args.iter().any(|a| a == "--assistant-dry-run");
    // 任务推进的间隔（秒）。**0 表示关闭。**
    let task_secs: u64 = flag(args, "--task-interval")
        .map(str::parse)
        .transpose()?
        .unwrap_or(60);
    // 任务安静多久之后才允许守护接手。默认 120 秒——
    // 比任何一次正常的模型往返都长，又短到"进程被杀了"能合理地被接上。
    let task_idle_secs: u64 = flag(args, "--task-idle")
        .map(str::parse)
        .transpose()?
        .unwrap_or(120);
    let task_dry: bool = args.iter().any(|a| a == "--assistant-dry-run");
    let mail_port: u16 = flag(args, "--port")
        .map(str::parse)
        .transpose()?
        .unwrap_or(yunxi_bot_core::info::mail::DEFAULT_PORT);

    let opts = TickOptions {
        require_os_isolation: args.iter().any(|a| a == "--require-os-isolation"),
        ..Default::default()
    };

    let mut l = Ledger::open(ledger_path())?;

    // 单实例锁：两个守护同时调度同一批任务会导致重复执行。
    // 锁在进程退出（含 Ctrl+C）时由析构自动释放。
    let _lock = match yunxi_bot_core::instance::InstanceLock::acquire(lock_path()) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("拒绝启动：{e}");
            return Ok(2);
        }
    };

    println!("YunXi Bot 守护启动");
    println!("  台账   : {}", l.path().display());
    println!(
        "  锁     : {} (pid {})",
        _lock.path().display(),
        _lock.pid()
    );
    println!("  间隔   : {interval} ms");
    println!(
        "  隔离   : {}",
        if opts.require_os_isolation {
            "要求 OS 级写入隔离（拿不到将拒绝执行）"
        } else {
            "进程隔离"
        }
    );
    if max_ticks > 0 {
        println!("  轮数   : {max_ticks}（跑完即停）");
    }

    // —— Agent 循环 ——
    //
    // 只有显式开启才跑。理由：判断要调用本地决策模型，而**没装 Laya 时它会
    // 每次都降级**——那是正确行为（fail-closed 不打扰），但会在日志里刷屏。
    // 让它成为一个显式选择，比默认打开再解释噪音要好。
    let agent_on = args.iter().any(|a| a == "--agent");
    let judge_every: u64 = flag(args, "--judge-every")
        .map(str::parse)
        .transpose()?
        .unwrap_or(12);
    let agent_home = default_home();

    // **决策引擎在两个功能之间共用。**
    //
    // 陪伴回合和助理回合都需要它，而且都是 Interrupt 类、降级方向都是不打扰。
    // 所以只要有**任一**功能开着就得建——最初这里只看 `agent_on`，
    // 结果是"只开助理不开陪伴"时引擎是 None，助理回合被静默跳过。
    // 那个 bug 是端到端测试抓到的：启动日志说"信件：每 2 秒看一次"，
    // 然后两秒一轮跑了 25 秒，一封邮件都没发现。
    let need_engine = agent_on || assistant_secs > 0;

    let mut engine = if need_engine {
        use yunxi_bot_core::agent::agent_engine;
        use yunxi_bot_core::decide::LayaDecider;
        let endpoint = flag(args, "--endpoint").unwrap_or("http://127.0.0.1:17870/decide");
        if agent_on {
            println!("  Agent  : 开启（每 {judge_every} 轮判断一次）");
        }
        println!("  判断   : {endpoint}（本地）");
        Some(agent_engine(LayaDecider::new(endpoint)))
    } else {
        None
    };
    let thinker = if agent_on {
        match yunxi_bot_core::think::agnes::OpenAiThinker::from_home(
            &agent_home,
            ThinkerConfig::agnes(),
        ) {
            Ok(t) => {
                use yunxi_bot_core::think::Thinker as _;
                println!("  表达   : {}（{} RPM）", t.model(), t.config().rpm);
                Some(t)
            }
            Err(_) => {
                println!("  表达   : 不可用（没配密钥）——仍会判断与记账，但开不了口");
                None
            }
        }
    } else {
        None
    };
    println!();

    // —— 助理巡览的组件 ——
    //
    // 与"陪伴回合"是两件事：
    //   陪伴回合：看**内部状态**（记忆、关系），决定要不要主动开口
    //   助理回合：看**外部信息**（邮件），决定要不要把事告诉你
    //
    // 两者共用同一个决策引擎——都是 Interrupt 类，降级方向都是不打扰。
    let assistant_source: Option<yunxi_bot_core::info::mail::MailSource> = if assistant_secs > 0 {
        use yunxi_bot_core::info::InfoSource as _;
        let src = yunxi_bot_core::info::mail::MailSource::new(mail_port);
        // **启动时探一次只为把话说清楚，不决定要不要用它。**
        //
        // 最初这里写的是"探不到就设成 None，永不启用"——端到端测试立刻
        // 抓到了它的后果：sidecar 只是还没起来，而守护进程从此再也不看邮件。
        // 对一个要跑几天的进程，任何"启动时的一次失败决定长期行为"都是错的。
        match src.health() {
            Ok(()) => {
                println!(
                    "  信件   : 每 {} 秒看一次{}",
                    assistant_secs,
                    src.describe()
                        .map(|w| format!("（{w}）"))
                        .unwrap_or_default()
                );
            }
            Err(e) => {
                // 说清楚它**还会再试**，不然使用者会以为这条通道废了
                println!(
                    "  信件   : 每 {} 秒试一次（现在还连不上：{e}）",
                    assistant_secs
                );
            }
        }
        Some(src)
    } else {
        println!("  信件   : 关闭（--assistant-interval 0）");
        None
    };

    let assistant_notifier: Box<dyn yunxi_bot_core::notify::Notifier> =
        if args.iter().any(|a| a == "--console") {
            Box::new(yunxi_bot_core::notify::ConsoleNotifier)
        } else {
            #[cfg(windows)]
            {
                Box::new(yunxi_bot_core::notify_windows::WindowsToast::new())
            }
            #[cfg(not(windows))]
            {
                Box::new(yunxi_bot_core::notify::NullNotifier)
            }
        };

    let assistant_limit: usize = flag(args, "--limit")
        .map(str::parse)
        .transpose()?
        .unwrap_or(20);

    if assistant_source.is_some() {
        println!(
            "  通知   : {}{}",
            assistant_notifier.name(),
            if assistant_dry {
                "（演练：只判定不真发）"
            } else {
                ""
            }
        );
    }
    println!();

    // —— 任务推进的组件 ——
    //
    // 与助理回合的分工：
    //   助理回合：看**外部信息**（邮件），决定要不要把事告诉你
    //   任务回合：看**内部待办**（任务），把没人管的推进下去
    //
    // **守护的审批者一律是"拒绝"。** 见 `run_task_round` 的文档：
    // 无人值守不该获得新权限，守护能做的 = 使用者在 --allow 里预先写明的。
    let mut task_handler: Option<chat_handler::ChatHandler> = if task_secs > 0 {
        let tools = tooling::default_registry(&agent_home)?;
        println!(
            "  任务   : 每 {} 秒看一眼待办（安静 {} 秒以上的才接手）",
            task_secs, task_idle_secs
        );
        println!("          审批者：一律拒绝（无人值守不获得新权限；能做的 = --allow 里写明的）");
        Some(
            chat_handler::ChatHandler::new(
                agent_home.clone(),
                // 人格名与 cmd_do 保持一致——同一个助理，不该因为
                // 谁来跑而叫不同的名字。
                "云熙",
                chat_handler::DEFAULT_PERSONA,
                chat_handler::default_rules(),
            )
            .with_tools(tools)
            .with_approver(std::sync::Arc::new(std::sync::Mutex::new(
                yunxi_bot_core::tool::RefusingApprover,
            )))
            .with_policy(tooling::policy_from_args(args))
            .with_sink(tool_sink_or_warn())
            .with_decider(std::sync::Arc::new(
                yunxi_bot_core::decide::LayaDecider::new(
                    flag(args, "--endpoint").unwrap_or("http://127.0.0.1:17870/decide"),
                ),
            )),
        )
    } else {
        println!("  任务   : 关闭（--task-interval 0）");
        None
    };

    let task_cfg = TaskRound {
        idle_ms: task_idle_secs * 1000,
        budget: yunxi_bot_core::task::Budget::default(),
        dry_run: task_dry,
    };
    let task_notifier: Box<dyn yunxi_bot_core::notify::Notifier> =
        if args.iter().any(|a| a == "--console") {
            Box::new(yunxi_bot_core::notify::ConsoleNotifier)
        } else {
            #[cfg(windows)]
            {
                Box::new(yunxi_bot_core::notify_windows::WindowsToast::new())
            }
            #[cfg(not(windows))]
            {
                Box::new(yunxi_bot_core::notify::NullNotifier)
            }
        };
    let task_router = yunxi_bot_core::think::ModelRouter::default();
    let task_decider = yunxi_bot_core::decide::LayaDecider::new(
        flag(args, "--endpoint").unwrap_or("http://127.0.0.1:17870/decide"),
    );

    // 上一次任务回合的时刻。同样按时间不按轮数——
    // 理由和助理巡览一样（轮数会和 --interval 耦合）。
    let mut last_task_ms: u64 = 0;

    let mut n = 0u64;
    let mut consecutive_errors = 0u32;
    // 上一次助理巡览的时刻。用**时间**而不是轮数：
    // 轮数会和 --interval 耦合，那样 --interval 1000 配"每 5 轮"
    // 就成了每 5 秒登一次 IMAP——不是勤快，是在撞服务端的连接频率限制。
    let mut last_assistant_ms: u64 = 0;
    // 上一轮信息源健不健康。**只在状态变化时打印**——
    // 每轮都打"连不上"会让日志变成噪音，而噪音的代价是你不再看它。
    let mut assistant_health: Option<bool> = None;
    loop {
        n += 1;
        match tick(&mut l, &opts) {
            Ok(r) => {
                consecutive_errors = 0;
                if !r.is_quiet() {
                    print!("[{n:>3}] ");
                    print_report(&r);
                }
            }
            Err(e) => {
                // 单轮失败不能杀死常驻进程；但连续失败要显式升级为熔断，
                // 避免一个坏掉的台账让进程空转刷屏。
                consecutive_errors += 1;
                eprintln!("[{n:>3}] 本轮失败（连续 {consecutive_errors} 次）: {e}");
                if consecutive_errors >= 5 {
                    eprintln!("连续失败达到 5 次，守护退出以免空转。请检查台账与权限。");
                    return Ok(1);
                }
            }
        }

        // —— Agent 判断回合 ——
        //
        // 放在调度之后：先让任务跑完，判断才有新的事实可看。
        if let Some(engine) = engine.as_mut() {
            if judge_every > 0 && n.is_multiple_of(judge_every) {
                run_agent_round(&mut l, engine, thinker.as_ref(), &agent_home, n);
            }

            // —— 助理巡览 ——
            //
            // **这一段就是"常驻"两个字的落点。** 在此之前整条助理链路
            // 只能靠人手敲 `check` 触发——那不叫常驻，叫"你问我答"。
            if let Some(src) = assistant_source.as_ref() {
                let now = now_millis().unwrap_or(0);
                let due = last_assistant_ms == 0
                    || now.saturating_sub(last_assistant_ms) >= assistant_secs * 1000;
                if due {
                    last_assistant_ms = now;
                    let cfg = AssistantRound {
                        source: src,
                        notifier: assistant_notifier.as_ref(),
                        home: &agent_home,
                        limit: assistant_limit,
                        dry_run: assistant_dry,
                    };
                    run_assistant_round(&mut l, engine, &cfg, n, &mut assistant_health);
                }
            }
        }

        // —— 任务回合 ——
        //
        // **这一段是 goal 开头那条断链的落点**：任务卡住后没有任何东西
        // 自动推进它。现在守护每 `--task-interval` 秒看一眼待办，
        // 把安静够久、又不需要人介入的那些推进下去。
        //
        // 放在助理回合之后：先看外面进来了什么，再看自己欠着什么。
        if let Some(h) = task_handler.as_mut() {
            let now = now_millis().unwrap_or(0);
            let due = last_task_ms == 0 || now.saturating_sub(last_task_ms) >= task_secs * 1000;
            if due {
                last_task_ms = now;
                run_task_round(
                    &mut l,
                    &task_router,
                    &task_decider,
                    h,
                    task_notifier.as_ref(),
                    &task_cfg,
                    n,
                );
            }
        }

        if max_ticks > 0 && n >= max_ticks {
            break;
        }
        thread::sleep(Duration::from_millis(interval));
    }

    println!("\n守护已停止（共 {n} 轮）");
    Ok(0)
}

/// 守护进程里的一个 Agent 判断回合。
///
/// **判断失败不能杀死常驻进程**：Agent 说不出话，任务该跑还得跑。
/// 但失败必须可见——静默吞掉会让"Agent 为什么不理我"无从排查。
fn run_agent_round<D, T>(
    ledger: &mut Ledger,
    engine: &mut yunxi_bot_core::decide::DecisionEngine<D>,
    thinker: Option<&T>,
    home: &std::path::Path,
    tick_no: u64,
) where
    D: yunxi_bot_core::decide::Decider,
    T: yunxi_bot_core::think::Thinker,
{
    use chrono::Timelike;
    use yunxi_bot_core::agent::{CycleInput, run_cycle};
    use yunxi_bot_core::companion::CompanionPolicy;
    use yunxi_bot_core::memory::Situation;

    let Ok(now_ms) = yunxi_bot_core::now_millis() else {
        eprintln!("[{tick_no:>3}] Agent 回合跳过：取不到当前时间");
        return;
    };

    let policy = CompanionPolicy::default();
    let input = CycleInput {
        policy: &policy,
        local_hour: chrono::Local::now().hour() as u8,
        now_ms,
        base: Situation {
            relationship_stage: "初期".into(),
            ..Default::default()
        },
    };

    let _ = home;
    match run_cycle(ledger, engine, thinker, &input) {
        Ok(out) => {
            if let Some(text) = &out.spoken {
                println!("[{tick_no:>3}] ◆ 它开口了: {text}");
            } else if out.throttled {
                println!("[{tick_no:>3}] Agent：本地限流，本轮未调用远端模型");
            } else if out.decision.degraded {
                // 降级会每轮都发生（Laya 没跑），只在首次提示一次
                static WARNED: std::sync::Once = std::sync::Once::new();
                WARNED.call_once(|| {
                    println!(
                        "[{tick_no:>3}] Agent：判断模型不可用，按 fail-closed 保持静默。\
                         \n      本地决策模型未启动时这是预期行为（不会打扰你）。"
                    );
                });
            } else {
                // **每个回合都要有痕迹。** 早先版本这条路径是静默的，
                // 于是"判断正常但选择不说"看起来跟"Agent 没在跑"完全一样。
                println!(
                    "[{tick_no:>3}] Agent：判断为「{}」，本轮不打扰",
                    out.decision.action.label()
                );
            }
        }
        Err(e) => {
            eprintln!("[{tick_no:>3}] Agent 回合失败（不影响任务调度）: {e}");
        }
    }
}

/// 自启动管理动作。
#[derive(Debug, Clone, Copy)]
enum AutostartAction {
    Install,
    Uninstall,
    Status,
}

/// 自启文件名（放在当前用户的「启动」文件夹里）。
#[cfg(windows)]
const AUTOSTART_FILE: &str = "YunXiBot.vbs";

/// 当前用户的「启动」文件夹路径。
#[cfg(windows)]
fn startup_dir() -> Result<PathBuf, Box<dyn std::error::Error>> {
    let appdata =
        std::env::var("APPDATA").map_err(|_| "环境变量 APPDATA 缺失，无法定位启动文件夹")?;
    Ok(PathBuf::from(appdata)
        .join("Microsoft")
        .join("Windows")
        .join("Start Menu")
        .join("Programs")
        .join("Startup"))
}

/// 开机自启：在**当前用户的「启动」文件夹**放一个 VBS 启动器。
///
/// 为什么不用计划任务：`schtasks /SC ONLOGON` 需要管理员权限，普通用户会被
/// 拒绝（实测 `ERROR: Access is denied.`）。启动文件夹是 per-user 的，
/// 零权限、零依赖，符合本项目"数据在 %LOCALAPPDATA%、不碰系统目录"的落地方式。
///
/// 为什么用 VBS：`.cmd` 会弹出控制台窗口。VBS 的 `Run(..., 0, False)` 以
/// **隐藏窗口**方式拉起常驻进程，用户登录后不会看到一个黑框。
fn cmd_autostart(action: AutostartAction) -> Result<i32, Box<dyn std::error::Error>> {
    #[cfg(not(windows))]
    {
        let _ = action;
        eprintln!("当前平台未实现自启注册。");
        eprintln!(
            "Linux 可自行加一个 systemd user unit 或 ~/.config/autostart 桌面项，\
             指向 `yunxi-bot daemon`。"
        );
        Ok(2)
    }

    #[cfg(windows)]
    {
        let dir = startup_dir()?;
        let file = dir.join(AUTOSTART_FILE);

        match action {
            AutostartAction::Install => {
                std::fs::create_dir_all(&dir)?;
                let exe = std::env::current_exe()?;

                // 0 = 隐藏窗口，False = 不等待。
                // 启动 `supervise` 而非 `daemon`：前者在 daemon 异常退出时会
                // 自动重启，这才是"常驻"应有的形态。
                let script = format!(
                    "' YunXi Bot 常驻守护 —— 由 `yunxi-bot install-autostart` 生成\r\n\
                     ' 删除本文件即可取消自启。\r\n\
                     CreateObject(\"WScript.Shell\").Run \"\"\"{exe}\"\" supervise\", 0, False\r\n",
                    exe = exe.display()
                );
                std::fs::write(&file, script)?;

                println!("已注册自启：当前用户登录时后台启动");
                println!("  启动器 : {}", file.display());
                println!("  目标   : {} supervise（异常退出自动重启）", exe.display());
                println!("\n取消：yunxi-bot uninstall-autostart");
                Ok(0)
            }
            AutostartAction::Uninstall => {
                if file.exists() {
                    std::fs::remove_file(&file)?;
                    println!("已取消自启（已删除 {}）", file.display());
                    Ok(0)
                } else {
                    println!("自启未注册（{} 不存在）", file.display());
                    Ok(1)
                }
            }
            AutostartAction::Status => {
                if file.exists() {
                    println!("自启已注册：");
                    println!("  {}", file.display());
                    let body = std::fs::read_to_string(&file).unwrap_or_default();
                    for line in body.lines().filter(|l| !l.trim().is_empty()) {
                        println!("  | {}", line.trim());
                    }
                    Ok(0)
                } else {
                    println!("自启未注册（{} 不存在）", file.display());
                    Ok(1)
                }
            }
        }
    }
}

/// 监督模式：daemon 异常退出时自动重启。
///
/// 为什么需要它：常驻的价值建立在"它一直在"之上。`daemon` 自身已经能扛住
/// 单轮失败（连续 5 次才退出），但**进程级崩溃**（内存耗尽、未捕获的 panic、
/// 外部强杀）需要更外层的看护。
///
/// 三条设计要点：
///
/// 1. **只重启异常退出**：退出码 0 表示守护主动收工（例如 `--max-ticks` 跑完），
///    这种情况重启会造成无意义的循环。
/// 2. **指数退避**：连续崩溃说明环境有问题，立刻重启只会刷屏。间隔逐次翻倍。
/// 3. **有上限**：重启超过上限就放弃并退出，把问题暴露给人，
///    而不是永远假装一切正常。
fn cmd_supervise(args: &[String]) -> Result<i32, Box<dyn std::error::Error>> {
    use std::process::Command;

    let interval = flag(args, "--interval").unwrap_or("5000");
    let max_restarts: u32 = flag(args, "--max-restarts")
        .map(str::parse)
        .transpose()?
        .unwrap_or(10);

    let exe = std::env::current_exe()?;
    println!("监督模式启动");
    println!("  子进程 : {} daemon --interval {interval}", exe.display());
    println!("  上限   : 连续异常退出 {max_restarts} 次后放弃");
    println!();

    let mut restarts = 0u32;
    loop {
        let status = Command::new(&exe)
            .args(["daemon", "--interval", interval])
            .status()?;

        if status.success() {
            println!("\n子进程正常退出（退出码 0），监督结束。");
            return Ok(0);
        }

        let code = status.code().unwrap_or(-1);
        restarts += 1;

        if restarts > max_restarts {
            eprintln!("\n子进程连续异常退出 {restarts} 次（最后一次退出码 {code}），已达上限。");
            eprintln!("停止重启——继续重试只会掩盖问题。请检查台账与运行环境。");
            return Ok(1);
        }

        // 指数退避，封顶 60 秒：连续崩溃时不刷屏，也不无限等待
        let backoff = (1u64 << restarts.min(6)).min(60);
        eprintln!(
            "子进程异常退出（退出码 {code}），{backoff}s 后重启（第 {restarts}/{max_restarts} 次）"
        );
        thread::sleep(Duration::from_secs(backoff));
    }
}

/// 隔离自检：真起一个 canary 子进程，验证写入隔离是否**真的**生效。
///
/// 它存在的理由：隔离的实现里有太多地方可以"错了但不报错"，而那会产出
/// **看起来实现了隔离、实际毫无限制**的代码。声称必须由实测背书。
#[cfg(windows)]
fn cmd_isolation_check() -> Result<i32, Box<dyn std::error::Error>> {
    use yunxi_bot_core::exec;
    use yunxi_bot_core::win_token;

    // 授权区必须是**低完整性**目录：低完整性子进程写不进中完整性对象，
    // 所以"允许写"的位置要用系统预置的 AppData\LocalLow。
    let low_root =
        win_token::low_integrity_root().ok_or("取不到用户目录，无法定位低完整性可写区")?;
    let base = low_root.join(format!("YunXiBot-isocheck-{}", std::process::id()));
    let allowed = base.join("allowed");
    // 反例放在中完整性的 TEMP：低完整性子进程**不应该**写得进去
    let medium = std::env::temp_dir().join(format!("yunxi-isocheck-{}", std::process::id()));
    let denied = medium.join("denied");
    std::fs::create_dir_all(&denied)?;

    println!(
        "平台报告可用隔离级别 : {}",
        exec::available_isolation().describe()
    );
    println!("授权区（低完整性）   : {}", allowed.display());
    println!("禁止区（中完整性）   : {}", denied.display());
    println!();
    println!("正在运行写入隔离开自检（会真的起一个受限子进程）...");

    let result = win_token::verify_write_isolation(&allowed, &denied);
    let _ = std::fs::remove_dir_all(&base);
    let _ = std::fs::remove_dir_all(&medium);

    match result {
        Ok(report) => {
            println!();
            println!("授权路径写入 : {}", report.allowed_written);
            println!("未授权路径写入: {}", report.denied_written);
            println!(
                "子进程退出码 : {} (0x{:08X})",
                report.exit_code, report.exit_code as u32
            );
            if !report.stdout.trim().is_empty() {
                println!("stdout: {}", report.stdout.trim());
            }
            if !report.stderr.trim().is_empty() {
                println!("stderr: {}", report.stderr.trim());
            }
            println!();
            println!("判定: {}", report.explain());
            println!();
            if report.is_effective() {
                println!("写入隔离已通过自检。");
                Ok(0)
            } else {
                println!(
                    "结论：**不声称提供写入隔离。** `--require-os-isolation` 会继续拒绝执行，"
                );
                println!("      这是失败契约在工作，不是缺陷。");
                Ok(1)
            }
        }
        Err(e) => {
            println!("\n自检无法完成：{e}");
            println!("结论：不声称提供写入隔离。");
            Ok(1)
        }
    }
}

#[cfg(not(windows))]
fn cmd_isolation_check() -> Result<i32, Box<dyn std::error::Error>> {
    println!("当前平台不适用（写入隔离是 Windows 专有机制）。");
    println!(
        "平台报告可用隔离级别: {}",
        yunxi_bot_core::exec::available_isolation().describe()
    );
    Ok(2)
}

/// 调一次思考模型，验证连通性与用量。
///
/// 这个命令存在的意义：**接入层的真伪只能靠真实调用验证**。
/// 单测能证明错误映射、限流、密钥不泄露，但证明不了"这台机器此刻真的能连上
/// Agnes 并拿到回复"。
use yunxi_bot_core::think::ThinkerConfig;

fn cmd_think(args: &[String]) -> Result<i32, Box<dyn std::error::Error>> {
    use yunxi_bot_core::think::{Message, ThinkRequest, Thinker, agnes::OpenAiThinker};

    let positional: Vec<String> = args
        .iter()
        .enumerate()
        .filter(|(i, a)| {
            !a.starts_with("--")
                && !(*i > 0 && args.get(i - 1).map(String::as_str) == Some("--provider"))
        })
        .map(|(_, a)| a.clone())
        .collect();
    let prompt: String = if positional.is_empty() {
        "用一句话说明你是谁。".to_string()
    } else {
        positional.join(" ")
    };

    let home = default_home();
    // --provider 让同一套代码在 Agnes 与 DeepSeek 之间切换。
    // 两者都是 OpenAI 兼容，差别只在 base_url / model / 密钥文件 / 思考模式。
    let config = match flag(args, "--provider").unwrap_or("agnes") {
        "agnes" => ThinkerConfig::agnes(),
        "deepseek" => ThinkerConfig::deepseek(),
        other => {
            eprintln!("未知 provider {other}，可用: agnes / deepseek");
            return Ok(2);
        }
    };
    let thinker = match OpenAiThinker::from_home(&home, config) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("无法初始化思考模型: {e}");
            eprintln!();
            eprintln!(
                "密钥应放在: {}",
                home.join("secrets").join("agnes.key").display()
            );
            eprintln!("或用环境变量 YUNXI_BOT_AGNES_KEY 覆盖。");
            return Ok(2);
        }
    };

    println!("模型      : {}", thinker.model());
    println!("Base URL  : {}", thinker.config().base_url);
    println!(
        "限流      : {} RPM（两次调用最小间隔 {:?}）",
        thinker.config().rpm,
        thinker.min_interval()
    );
    println!("超时      : {:?}", thinker.config().timeout);
    println!("思考模式  : {}", thinker.config().thinking.label());
    println!("提示词    : {prompt}");
    println!();

    let req = ThinkRequest::new(vec![
        Message::system("你是一个个人助理程序的连通性测试端点。回答简洁。"),
        Message::user(prompt),
    ])
    .with_max_tokens(256)
    .with_temperature(0.3);

    let started = std::time::Instant::now();
    match thinker.think(&req) {
        Ok(resp) => {
            println!("耗时     : {} ms", started.elapsed().as_millis());
            println!("模型回执 : {}", resp.model);
            println!(
                "结束原因 : {}",
                resp.finish_reason.unwrap_or_else(|| "-".into())
            );
            println!(
                "用量     : prompt {} / completion {} / total {}",
                resp.usage.prompt_tokens, resp.usage.completion_tokens, resp.usage.total_tokens
            );
            println!();
            println!("--- 回复 ---");
            println!("{}", resp.content.trim());
            Ok(0)
        }
        Err(e) => {
            println!("耗时     : {} ms", started.elapsed().as_millis());
            eprintln!("调用失败 : {e}");
            eprintln!(
                "可重试   : {}",
                if e.is_retryable() {
                    "是"
                } else {
                    "否（重试无用）"
                }
            );
            Ok(1)
        }
    }
}

/// 跑一个完整的 Agent 回合：投影记忆 → 本地判断 → 约束收紧 → （必要时）远端表达。
fn cmd_agent(args: &[String]) -> Result<i32, Box<dyn std::error::Error>> {
    use yunxi_bot_core::agent::{CycleInput, agent_engine, run_cycle};
    use yunxi_bot_core::companion::CompanionPolicy;
    use yunxi_bot_core::decide::LayaDecider;
    use yunxi_bot_core::memory::Situation;

    let no_think = args.iter().any(|a| a == "--no-think");
    let hour: u8 = flag(args, "--hour")
        .map(str::parse)
        .transpose()?
        .unwrap_or_else(|| {
            use chrono::Timelike;
            chrono::Local::now().hour() as u8
        });

    let mut ledger = Ledger::open(ledger_path())?;
    let policy = CompanionPolicy::default();

    // 想看 Agent 到底知道什么，就先把它看到的提示词打出来。
    // 判断"为什么它不说话"时，这一行比任何日志都有用。
    if args.iter().any(|a| a == "--show-prompt") {
        use yunxi_bot_core::agent::{compose_prompt, derive_situation};
        use yunxi_bot_core::memory::Memory;
        let events = ledger.events().to_vec();
        let now_ms = yunxi_bot_core::now_millis()?;
        let ctx = derive_situation(&events, now_ms);
        let memory = Memory::from_events(&events);
        println!("===== 它看到的提示词 =====");
        println!("{}", compose_prompt(&ctx, &memory, &events, now_ms));
        println!("==========================");
        println!();
    }

    // 判断走**本地** Laya。它没跑的话会降级成 fail-closed（不打扰）——
    // 这是设计，不是故障，但要让人看得见。
    let endpoint = flag(args, "--endpoint").unwrap_or("http://127.0.0.1:17870/decide");
    let decider = LayaDecider::new(endpoint);
    let mut engine = agent_engine(decider);

    let input = CycleInput {
        policy: &policy,
        local_hour: hour,
        now_ms: yunxi_bot_core::now_millis()?,
        base: Situation {
            relationship_stage: "初期".into(),
            ..Default::default()
        },
    };

    println!("判断模型 : {endpoint}（本地）");
    println!(
        "当前小时 : {hour}   安静时段: {}:00–{}:00",
        policy.quiet_start_hour, policy.quiet_end_hour
    );
    println!("表达模型 : {}", if no_think { "（跳过）" } else { "Agnes" });
    println!();

    // 思考层是可选的：没配密钥也能跑判断
    let home = default_home();
    let thinker = if no_think {
        None
    } else {
        yunxi_bot_core::think::agnes::OpenAiThinker::from_home(&home, ThinkerConfig::agnes()).ok()
    };
    if !no_think && thinker.is_none() {
        println!("提示：没有可用的思考模型（密钥缺失），本轮只判断不表达。");
        println!(
            "      密钥位置: {}",
            home.join("secrets").join("agnes.key").display()
        );
        println!();
    }

    match thinker.as_ref() {
        Some(t) => {
            let out = run_cycle(&mut ledger, &mut engine, Some(t), &input)?;
            print_cycle(&out);
        }
        None => {
            let out = run_cycle::<_, yunxi_bot_core::think::agnes::StubThinker>(
                &mut ledger,
                &mut engine,
                None,
                &input,
            )?;
            print_cycle(&out);
        }
    }
    Ok(0)
}

fn print_cycle(out: &yunxi_bot_core::agent::CycleOutcome) {
    println!("最终动作 : {}", out.decision.action.label());
    if out.decision.degraded {
        println!("已降级   : 是（打扰类降级方向 fail-closed，不会打扰你）");
    }
    println!("依据     : {}", out.decision.reason);
    println!("台账边界 : span#{}", out.span_id);
    println!();
    match (&out.spoken, out.throttled) {
        (Some(text), _) => {
            println!("--- 它开口了 ---");
            println!("{text}");
        }
        (None, true) => println!("（被本地限流挡下，本轮没去打远端模型，也没说话）"),
        (None, false) => println!("（本轮选择不开口）"),
    }
}

/// 记一条记忆。
fn cmd_remember(args: &[String]) -> Result<i32, Box<dyn std::error::Error>> {
    use yunxi_bot_core::memory::MemoryKind;

    let kind_str = flag(args, "--kind").unwrap_or("fact");
    let kind = match kind_str {
        "fact" => MemoryKind::Fact,
        "preference" => MemoryKind::Preference,
        "relationship" => MemoryKind::Relationship,
        "event" => MemoryKind::Event,
        other => {
            eprintln!("未知类别 {other}，可用: fact / preference / relationship / event");
            return Ok(2);
        }
    };

    let text: Vec<String> = args
        .iter()
        .enumerate()
        .filter(|(i, a)| {
            *a != "--kind" && !(*i > 0 && args.get(i - 1).map(String::as_str) == Some("--kind"))
        })
        .map(|(_, a)| a.clone())
        .collect();
    let text = text.join(" ").trim().to_string();
    if text.is_empty() {
        eprintln!("缺少内容。用法: yunxi-bot remember <内容> [--kind fact]");
        return Ok(2);
    }

    let mut ledger = Ledger::open(ledger_path())?;
    let id = format!("m{}", yunxi_bot_core::now_millis()?);
    ledger.append(
        yunxi_bot_core::EventKind::MemoryRecorded,
        None,
        serde_json::json!({ "id": id, "kind": kind, "text": text }),
    )?;

    println!("已记住 [{}] {}", kind.label(), text);
    println!("  编号 : {id}");
    println!("  台账 : {}", ledger.path().display());
    Ok(0)
}

/// 查看最近的决策记录。
fn cmd_journal(args: &[String]) -> Result<i32, Box<dyn std::error::Error>> {
    use yunxi_bot_core::EventKind;

    let n: usize = flag(args, "-n")
        .or_else(|| flag(args, "--limit"))
        .map(str::parse)
        .transpose()?
        .unwrap_or(10);

    let ledger = Ledger::open(ledger_path())?;
    let decisions: Vec<_> = ledger
        .events()
        .iter()
        .filter(|e| e.kind == EventKind::DecisionDecided)
        .rev()
        .take(n)
        .collect();

    if decisions.is_empty() {
        println!("还没有任何决策记录。");
        println!("跑一次 `yunxi-bot agent` 或让 daemon 带 --agent 运行就会产生。");
        return Ok(0);
    }

    println!("最近 {} 条决策记录：\n", decisions.len());
    for e in decisions {
        let when = chrono::DateTime::from_timestamp_millis(e.at as i64)
            .map(|d| {
                use chrono::TimeZone;
                chrono::Local
                    .from_utc_datetime(&d.naive_utc())
                    .format("%m-%d %H:%M:%S")
                    .to_string()
            })
            .unwrap_or_else(|| e.at.to_string());
        let degraded = e
            .data
            .get("degraded")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let action = e.data.get("action").and_then(|v| v.as_str()).unwrap_or("-");
        let reason = e.data.get("reason").and_then(|v| v.as_str()).unwrap_or("");
        println!(
            "  {when}  {action}{}",
            if degraded { "  [降级]" } else { "" }
        );
        if !reason.is_empty() {
            println!("            {reason}");
        }
    }
    Ok(0)
}

fn cmd_run(args: &[String]) -> Result<i32, Box<dyn std::error::Error>> {
    let Some(id) = args.first() else {
        eprintln!("缺少任务 id。");
        return Ok(2);
    };
    let jid = JobId::new(id.clone());
    let mut l = Ledger::open(ledger_path())?;
    let jobs = l.rebuild();
    let Some(job) = jobs.get(&jid) else {
        eprintln!("找不到任务 {id}");
        return Ok(2);
    };

    if job.requires_approval() {
        eprintln!("任务 {id} 需要先 `approve`（不可逆动作每次运行都要重新批准）");
        return Ok(3);
    }

    l.append(
        EventKind::JobStarted,
        Some(&jid),
        serde_json::json!({ "command": job.spec.command, "by": "cli-run" }),
    )?;

    let outcome = yunxi_bot_core::exec::run_command(
        &job.spec.command,
        &yunxi_bot_core::exec::ExecOptions {
            cwd: job.spec.cwd.clone().into(),
            timeout_ms: job.spec.timeout_ms,
            isolation: yunxi_bot_core::exec::IsolationRequirement::ProcessOnly,
            memory_limit_bytes: None,
            max_processes: None,
        },
    )?;

    if outcome.succeeded() {
        l.append(
            EventKind::JobSucceeded,
            Some(&jid),
            serde_json::json!({
                "exit_code": outcome.exit_code,
                "duration_ms": outcome.duration_ms,
                "isolation": outcome.isolation.describe(),
                "stdout": outcome.stdout,
            }),
        )?;
        println!("执行成功（{} ms）", outcome.duration_ms);
        if !outcome.stdout.trim().is_empty() {
            println!("--- stdout ---\n{}", outcome.stdout.trim_end());
        }
        Ok(0)
    } else {
        let err = if outcome.timed_out {
            format!("超时（{}ms）后被强制结束", job.spec.timeout_ms)
        } else {
            outcome.stderr.clone()
        };
        l.append(
            EventKind::JobFailed,
            Some(&jid),
            serde_json::json!({ "exit_code": outcome.exit_code, "error": err }),
        )?;
        eprintln!("执行失败（退出码 {}）: {err}", outcome.exit_code);
        Ok(1)
    }
}

// ——————————————————— 任务执行框架 ———————————————————

/// 任务 id 生成：时间戳 + 目标前几个字。
///
/// 不用随机数：**同一秒内的两次提交应该看得见冲突**，而不是悄悄覆盖。
/// 撞了就加后缀，见 [`unique_task_id`]。
fn make_task_id(goal: &str) -> String {
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
    let head: String = goal
        .chars()
        .filter(|c| !c.is_whitespace())
        .take(6)
        .collect();
    format!("do-{stamp}-{head}")
}

fn unique_task_id(store: &yunxi_bot_core::task::TaskSet, base: &str) -> String {
    if !store.contains_key(base) {
        return base.to_string();
    }
    for i in 2..1000 {
        let cand = format!("{base}-{i}");
        if !store.contains_key(&cand) {
            return cand;
        }
    }
    base.to_string()
}

/// 从 `--thinking` 解析思考策略。
fn parse_effort(args: &[String]) -> Result<ReasoningEffort, Box<dyn std::error::Error>> {
    Ok(match flag(args, "--thinking").unwrap_or("auto") {
        "auto" => ReasoningEffort::Auto,
        "on" | "always" => ReasoningEffort::Always,
        "off" | "never" => ReasoningEffort::Never,
        other => {
            eprintln!("未知思考模式 {other}，可用: auto / on / off");
            return Err("参数错误".into());
        }
    })
}

/// 打开台账 + 决策层。**决策层连不上要显式说出来**，不能静默降级。
fn open_decider(args: &[String]) -> (Ledger, yunxi_bot_core::decide::LayaDecider, bool) {
    let endpoint = flag(args, "--endpoint").unwrap_or("http://127.0.0.1:17870/decide");
    let decider = yunxi_bot_core::decide::LayaDecider::new(endpoint);
    let ledger = Ledger::open(ledger_path()).expect("无法打开台账");
    let alive = decider
        .decide(&yunxi_bot_core::decide::DecisionRequest::new(
            serde_json::json!({}),
            vec![],
        ))
        .is_ok();
    (ledger, decider, alive)
}

fn cmd_do(args: &[String]) -> Result<i32, Box<dyn std::error::Error>> {
    use yunxi_bot_core::task::Budget;
    use yunxi_bot_core::task::engine::{Engine, LedgerTaskStore, create_task, summarize};
    use yunxi_bot_core::think::ModelRouter;

    let goal: String = args
        .iter()
        .enumerate()
        .filter(|(i, a)| !a.starts_with("--") && !(*i > 0 && args[i - 1].starts_with("--")))
        .map(|(_, a)| a.clone())
        .collect::<Vec<_>>()
        .join(" ");
    if goal.trim().is_empty() {
        eprintln!("缺少目标。用法: yunxi-bot do \"把这件事办了\"");
        return Ok(2);
    }

    let budget = Budget {
        max_model_calls: flag(args, "--budget")
            .map(str::parse)
            .transpose()?
            .unwrap_or(20),
        max_steps: flag(args, "--max-steps")
            .map(str::parse)
            .transpose()?
            .unwrap_or(12),
        ..Default::default()
    };
    let effort = parse_effort(args)?;
    let dry_run = args.iter().any(|a| a == "--dry-run");

    // 缓存命中的前提是"前缀逐字节不变"，而这是使用者唯一能亲眼核对的地方。
    if args.iter().any(|a| a == "--show-prompt") {
        let h = chat_handler::ChatHandler::new(
            default_home(),
            "云熙",
            chat_handler::DEFAULT_PERSONA,
            chat_handler::default_rules(),
        );
        println!("—— 三个会话各自的稳定前缀（跨调用必须逐字节相同）——");
        for (name, p) in h.stable_prefixes() {
            println!("\n===== {name}（{} 字符）=====", p.chars().count());
            println!("{p}");
        }
        println!("\n易变的部分（目标、当前步骤、前置结果）不进这段前缀，");
        println!("历史只往后追加、不改前面——这样下一次调用才能命中缓存。");
        return Ok(0);
    }

    let router = ModelRouter::default();
    // `--provider` 强制全程用一个模型。做法是**换掉整张档位表**，
    // 而不是在路由里加特例——路由只有一套判据，别为调试口子fork 一份逻辑。
    let router = match flag(args, "--provider") {
        None => router,
        Some(p) => {
            let spec = match p {
                "deepseek" => ModelSpec::DEEPSEEK_FLASH,
                "agnes" => ModelSpec::AGNES_FLASH,
                other => {
                    eprintln!("未知 provider {other}，可用: agnes / deepseek");
                    return Ok(2);
                }
            };
            let tier = spec.tier;
            println!("注意      : --provider 强制全程用 {p}，已关闭自动选择");
            ModelRouter::new(vec![spec], tier)
        }
    };
    let (ledger, decider, decider_alive) = open_decider(args);

    // ---- 先把"会怎么走"说清楚，再花钱 ----
    //
    // 注意这里要报的是**拆解那一步**的路由，不是"目标整体"的路由：
    // 真正先发生的是拆解，而拆解固定按 Planning 走。早先我打的是目标整体的
    // 路由，结果和实际执行的第一跳对不上——预览骗人比没有预览更糟。
    let plan_kind = TaskKind::Planning;
    let plan_profile = TaskProfile {
        prompt_chars: goal.chars().count(),
        step_count: 3,
        explicit_multi: yunxi_bot_core::think::detect_explicit_multi(&goal),
        has_code: yunxi_bot_core::think::detect_code(&goal),
    };
    let preview = router.route(&goal, plan_kind, &plan_profile, effort, None);

    println!("目标      : {goal}");
    println!("拆解路由  : {}", chat_handler::route_line(&preview, effort));
    println!("拆解理由  : {}", preview.reason);
    println!("步骤路由  : 拆出来的每步各自判——按步骤类型决定用哪个模型、要不要开思考");
    println!(
        "预算      : 最多 {} 次模型调用 / {} 个步骤",
        budget.max_model_calls, budget.max_steps
    );
    let peak = yunxi_bot_core::costlog::peak_now();
    println!(
        "拆解估算  : ¥{:.6}（{}时段，按未命中缓存计；全部步骤另算）",
        preview.estimated_cost(peak).total(),
        if peak { "高峰" } else { "空闲" }
    );
    if !decider_alive {
        println!();
        println!("⚠ 本地决策模型连不上：需要它拍板的步骤会全部升级为人工介入。");
        println!("  启动 sidecar（python sidecar/verdict_server.py）后重跑即可。");
    }
    if dry_run {
        println!("（--dry-run：不调用任何模型）");
    }
    println!();

    let mut store = LedgerTaskStore::new(ledger);
    let tasks = store.project();
    let task_id = match flag(args, "--id") {
        Some(id) => id.to_string(),
        None => unique_task_id(&tasks, &make_task_id(&goal)),
    };
    create_task(&mut store, &task_id, &goal)?;

    let outcome = if dry_run {
        let mut handler = chat_handler::DryRunHandler::default();
        let out = Engine::new(&router, &decider, &mut handler, &mut store, budget)
            .with_effort(effort)
            .run(&task_id)?;
        println!("—— 干跑推演 ——");
        for line in &handler.seen {
            println!("  {line}");
        }
        println!();
        out
    } else {
        let mut handler = chat_handler::ChatHandler::new(
            default_home(),
            "云熙",
            chat_handler::DEFAULT_PERSONA,
            chat_handler::default_rules(),
        );
        // 工具 + 审批。**这是模型第一次真的能对世界动手。**
        let tools =
            tooling::default_registry(&default_home()).map_err(|e| format!("工具注册失败: {e}"))?;
        println!("工具      : {} 个（`yunxi-bot tools` 看清单）", tools.len());
        handler = handler
            .with_tools(tools)
            .with_approver(tooling::approver_from_args(args))
            .with_policy(tooling::policy_from_args(args))
            // **每一次工具调用都要落台账。**
            //
            // 在此之前 `ToolRunOutcome.calls` 生产出来就被丢掉——
            // 机器能读使用者的文件、跑命令、抓网页，而台账里一条记录都没有。
            // 对一个"能替你动手"的助理，那是信任问题，不只是审计问题：
            // 事后没法回答"它到底动过什么"。
            .with_sink(tool_sink_or_warn())
            .with_decider(std::sync::Arc::new(
                yunxi_bot_core::decide::LayaDecider::new(
                    flag(args, "--endpoint").unwrap_or("http://127.0.0.1:17870/decide"),
                ),
            ));
        println!();
        Engine::new(&router, &decider, &mut handler, &mut store, budget)
            .with_effort(effort)
            .run(&task_id)?
    };

    println!("任务 id   : {task_id}（用 `yunxi-bot tasks {task_id}` 看明细）");
    println!();
    print!("{}", summarize(&outcome.task));
    println!();
    println!("本次模型调用 : {} 次", outcome.used_model_calls);
    let (done, failed, skipped) = outcome.task.tally();
    match &outcome.advance {
        yunxi_bot_core::task::Advance::Finished => {
            // **"全部终态"不等于"都成功"。** 只报"完成"会让失败的步骤被读成
            // 已经办好了——这是最危险的一种报告方式，所以这里必须分开说。
            if failed == 0 && skipped == 0 {
                println!("结果      : 完成（{done} 步全部成功）");
                Ok(0)
            } else {
                println!("结果      : 完成，但有 {failed} 步失败、{skipped} 步被跳过");
                println!("           成功的是 {done} 步。失败原因见上面的 ✗ 行。");
                Ok(1)
            }
        }
        yunxi_bot_core::task::Advance::Waiting { reason } => {
            println!("结果      : 停下等人");
            println!("原因      : {reason}");
            println!();
            println!("处理完之后用 `yunxi-bot resume {task_id}` 续跑。");
            Ok(3)
        }
        yunxi_bot_core::task::Advance::Progress { settled, total, .. } => {
            println!("结果      : 中途返回（{settled}/{total}）—— 这不该发生，请报告");
            Ok(1)
        }
    }
}

/// 交互式会话。**通用 agent 的核心入口。**
///
/// ## 为什么它值得一条单独的路径
///
/// 别的命令都是"做一件事就退"。这一条要**活着**——因为对话的价值在延续：
/// 你说了"这个文件"，下一句的"它"才有指代对象。
///
/// ## 它没有另起一条轻量链路
///
/// 底下走的是同一个 `converse`：工具循环、审批门禁、台账留痕、
/// 缓存前缀全都照旧。**另开一条的话，那条路迟早会绕开审批。**
fn cmd_chat(args: &[String]) -> Result<i32, Box<dyn std::error::Error>> {
    use yunxi_bot_core::think::ReasoningEffort;
    use yunxi_bot_core::think::session::SessionStore;

    let home = default_home();
    let store = SessionStore::new(&home);

    // `chat list`：看有哪些会话
    if args.first().map(String::as_str) == Some("list") {
        return chat::list_sessions(&store);
    }

    let resume = args.iter().any(|a| a == "--resume");
    let session_id = match flag(args, "--id") {
        Some(id) => id.to_string(),
        None => {
            // 不给 id 时：`--resume` 接最近那个，否则按时间新建一个。
            if resume {
                match store.latest().map_err(|e| e.to_string())? {
                    Some(f) => f.id,
                    None => {
                        eprintln!("还没有任何会话可以接着聊。");
                        eprintln!("直接跑 `yunxi-bot chat` 开一个新的。");
                        return Ok(2);
                    }
                }
            } else {
                let ts = chrono::Local::now().format("%Y%m%d-%H%M%S");
                format!("chat-{ts}")
            }
        }
    };
    if let Err(e) = SessionStore::check_id(&session_id) {
        eprintln!("{e}");
        return Ok(2);
    }

    let effort = match flag(args, "--thinking").unwrap_or("auto") {
        "off" => ReasoningEffort::Never,
        "on" => ReasoningEffort::Always,
        _ => ReasoningEffort::Auto,
    };

    // —— 和 `do` 同一套装配：工具、审批、台账、决策模型 ——
    let mut handler = chat_handler::ChatHandler::new(
        home.clone(),
        "云熙",
        chat_handler::DEFAULT_PERSONA,
        chat_handler::default_rules(),
    );
    if let Ok(tools) = tooling::default_registry(&home) {
        let mut tools = tools;
        match tooling::with_mcp(&mut tools, &home) {
            Ok((n, problems)) => {
                if n > 0 {
                    println!("（{n} 个 MCP 工具已接入）");
                }
                for p in problems {
                    eprintln!("  ✗ MCP: {p}");
                }
            }
            Err(e) => eprintln!("  ✗ MCP: {e}"),
        }
        handler = handler.with_tools(tools);
    }

    // **REPL 里有人在，所以用真审批者。**
    // 答 `n` 就是拒绝，走的是和别处一样的 `GateDecision::Deny`。
    handler = handler
        .with_approver(tooling::approver_from_args(args))
        .with_policy(tooling::policy_from_args(args))
        .with_sink(tool_sink_or_warn())
        .with_decider(std::sync::Arc::new(
            yunxi_bot_core::decide::LayaDecider::new(
                flag(args, "--endpoint").unwrap_or("http://127.0.0.1:17870/decide"),
            ),
        ));

    // 台账：压缩要留痕（"它怎么不记得了"的答案在这里）
    let mut ledger = Ledger::open(ledger_path())?;
    // 预算按模型定。这里先按默认的对话模型给——真正的窗口由 router 选中的模型决定，
    // 而那个在每一轮才知道；用最保守的一档起步，宁可早压。
    // `--context-window` 让使用者能自己调，也让端到端验证能在合理的
    // 时间内触发压缩（不然要聊几十轮才到 32k）。
    let mut budget = yunxi_bot_core::think::context::ContextBudget::for_model("agnes");
    if let Some(w) = flag(args, "--context-window") {
        let w: usize = w.parse()?;
        if w < 512 {
            eprintln!("--context-window 太小了（{w}），至少 512");
            return Ok(2);
        }
        budget.window_tokens = w;
        // 预留跟着窗口缩，否则小窗口下永远触发不了压缩
        budget.reserve_for_reply = budget.reserve_for_reply.min(w / 8);
    }

    chat::run(
        &mut handler,
        &store,
        session_id,
        resume,
        effort,
        Some(&mut ledger),
        budget,
    )
}

/// MCP：列出外部 server 的工具，或者调一个。
///
/// ## 为什么这个命令必须存在
///
/// MCP 工具**默认需要审批**（`Capability::Unknown`——server 声称的能力
/// 不可核实）。所以使用者加一个 server 之后，第一件想做的事是
/// **看它到底提供了什么**，第二件是**在写审批规则之前先试一次**。
/// 没有这两个入口，加 server 就成了一件盲盒。
///
/// ## 它和 `tools` 的分工
///
/// - `tools`：本进程注册了哪些工具（MCP 的也在里面，标着"能力未知"）
/// - `mcp list`：连上 server，看它**声称**提供什么
///
/// 两者不一样是有意的：前者是"我能调什么"，后者是"它说它能什么"。
fn cmd_mcp(args: &[String]) -> Result<i32, Box<dyn std::error::Error>> {
    use yunxi_bot_core::mcp::{McpConfig, McpHub, SERVERS_FILE};

    let sub = args.first().map(String::as_str).unwrap_or("");
    let cfg_path = default_home().join(SERVERS_FILE);

    let cfg = match McpConfig::load(&default_home()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            return Ok(2);
        }
    };

    if cfg.servers.is_empty() {
        println!("没有配置 MCP server。");
        println!();
        println!("配置文件: {}", cfg_path.display());
        println!("形如：");
        println!("  {{");
        println!("    \"servers\": [");
        println!("      {{ \"name\": \"time\", \"command\": \"uvx\",");
        println!("        \"args\": [\"mcp-server-time\"] }}");
        println!("    ]");
        println!("  }}");
        println!();
        println!("注意：server 名不能含双下划线（那是工具名的分隔符）。");
        println!("装的 server 提供的工具默认**需要审批**——那是刻意的：");
        println!("装一个第三方 server 等于把它的能力引进你的机器，这个决定该有人签字。");
        return Ok(if sub == "list" { 0 } else { 2 });
    }

    print!("连接 {} 个 MCP server…", cfg.servers.len());
    use std::io::Write as _;
    let _ = std::io::stdout().flush();

    let (mut hub, problems) = match McpHub::connect(&cfg) {
        Ok(v) => v,
        Err(e) => {
            println!();
            eprintln!("{e}");
            return Ok(2);
        }
    };
    println!(" 完成");
    for p in &problems {
        // **起不来的必须说出来。** 否则使用者以为它连上了，
        // 而"工具少了一个"会被归因到别处。
        eprintln!("  ✗ {p}");
    }
    let names = hub.server_names();
    println!(
        "已连上: {}",
        if names.is_empty() {
            "（无）".to_string()
        } else {
            names.join(", ")
        }
    );

    match sub {
        "list" => {
            let tools = hub.list_tools();
            println!();
            println!("共 {} 个工具：", tools.len());
            for t in &tools {
                println!();
                println!("  {}", t.qualified);
                println!("      来源  : {}（原名 {}）", t.server, t.name);
                if !t.description.is_empty() {
                    println!("      说明  : {}", truncate_chars(&t.description, 90));
                }
                println!("      能力  : 能力未知（需审批）");
            }
            if tools.is_empty() {
                println!("（这个 server 没提供任何工具）");
            }
            Ok(0)
        }
        "call" => {
            let pos: Vec<&String> = args
                .iter()
                .skip(1)
                .filter(|a| !a.starts_with("--"))
                .collect();
            let (Some(server), Some(tool)) = (pos.first(), pos.get(1)) else {
                eprintln!("用法: yunxi-bot mcp call <server> <tool> [--args '<json>']");
                return Ok(2);
            };
            let raw = flag(args, "--args").unwrap_or("{}");
            let parsed: serde_json::Value = match serde_json::from_str(raw) {
                Ok(v) => v,
                Err(e) => {
                    eprintln!("--args 不是合法 JSON: {e}");
                    return Ok(2);
                }
            };

            println!();
            println!("调用 {server} / {tool} …");
            match hub.call(server, tool, parsed) {
                Ok(text) => {
                    println!();
                    println!("{text}");
                    Ok(0)
                }
                Err(e) => {
                    // **"这次做不到"不是"程序坏了"。** 如实报出来，
                    // 让调用方能换条路，而不是当成崩溃。
                    eprintln!("调用失败: {e}");
                    Ok(1)
                }
            }
        }
        other => {
            eprintln!("未知子命令「{other}」");
            eprintln!("用法: yunxi-bot mcp list");
            eprintln!("      yunxi-bot mcp call <server> <tool> [--args '<json>']");
            Ok(2)
        }
    }
}

/// 发一条桌面通知。**同时是"通知出口"的验证入口。**
///
/// 它存在的理由不只是"方便测"：使用者需要一条能**自己确认出口通不通**的命令。
/// 一个助理说"我通知你了"，你得有办法核实——否则你只能信它。
///
/// 输出里会明确区分「已确认送达」和「已交给系统但未确认可见」，
/// 因为**勿扰/专注助手开着时，通知仍然会进通知中心，只是不弹横幅**。
/// 把这两种情况说成同一件事，就是虚报。
fn cmd_notify(args: &[String]) -> Result<i32, Box<dyn std::error::Error>> {
    use yunxi_bot_core::notify::{ConsoleNotifier, Notification, Notifier, Urgency};

    let positional: Vec<&String> = args.iter().filter(|a| !a.starts_with("--")).collect();
    let Some(title) = positional.first() else {
        eprintln!("用法: yunxi-bot notify \"<标题>\" [\"<正文>\"] [--tag <标签>] [--console]");
        eprintln!();
        eprintln!("标题和正文都会显示在通知上。--tag 用于替换同类通知（不再堆积）。");
        return Ok(2);
    };
    let body = positional.get(1).map(|s| s.as_str()).unwrap_or("");

    let urgency = match flag(args, "--urgency").unwrap_or("normal") {
        "low" => Urgency::Low,
        "normal" => Urgency::Normal,
        "high" => Urgency::High,
        other => {
            eprintln!("未知紧急程度 {other}，可用: low / normal / high");
            return Ok(2);
        }
    };

    let mut n = Notification::new((*title).clone(), body).with_urgency(urgency);
    if let Some(t) = flag(args, "--tag") {
        n = n.with_tag(t);
    }

    // `--console` 强制走控制台出口：在没有通知能力的环境里（CI、远程会话），
    // 这一条是唯一能确认的出口。它也顺带证明"出口可换"这件事是真的。
    let backend: Box<dyn Notifier> = if args.iter().any(|a| a == "--console") {
        Box::new(ConsoleNotifier)
    } else {
        #[cfg(windows)]
        {
            Box::new(yunxi_bot_core::notify_windows::WindowsToast::new())
        }
        #[cfg(not(windows))]
        {
            Box::new(yunxi_bot_core::notify::NullNotifier)
        }
    };

    println!("出口      : {}", backend.name());
    let d = backend.notify(&n);
    println!("结果      : {}", d.label());
    println!("详情      : {}", d.detail());
    println!();
    if d.can_claim_user_notified() {
        println!("可以这样说：通知你了。");
        Ok(0)
    } else if d.left_the_process() {
        println!("只能这样说：通知已经交给系统，但**无法确认你看见**。");
        println!("（勿扰/专注助手开着时就是这种情况：进了通知中心，不弹横幅。）");
        Ok(3)
    } else {
        println!("**没有送达。** 调用方必须换一条出口，不能当作已通知。");
        Ok(1)
    }
}

/// 对一条通知表个态。**"这类以后别烦我"就靠这个。**
///
/// ## 两件事同时做，缺一不可
///
/// | 落到哪 | 有什么用 |
/// |---|---|
/// | 台账 | 事后能说清这条规则**是谁在什么时候为什么加的** |
/// | 策略文件 | 下一次判断**真的会用它** |
///
/// 只写台账不改策略 = 记了不做。只改策略不写台账 = 做了但说不清来由，
/// 三个月后你会盯着 `block_senders` 里一条 `@x.com` 想不起为什么。
///
/// ## 规则建在"当时那条"上，不重新去取
///
/// 从台账里找回系统判定时看到的那条记录。不重新联网取有两个理由，
/// 第二个更重要：不用联网（sidecar 没起来时"以后别烦我"仍该生效——
/// 那正是最想说这句话的时刻）；以及**重新取有可能取到同一 UID 的更新版本**，
/// 于是规则建在了另一条信息上。
fn cmd_feedback(args: &[String]) -> Result<i32, Box<dyn std::error::Error>> {
    use yunxi_bot_core::feedback::{
        Feedback, Scope, apply_never, feedback_event_data, find_item, last_notified,
    };
    use yunxi_bot_core::triage::{POLICY_FILE, load_policy, save_policy};
    use yunxi_bot_core::{EventKind, Ledger};

    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("用法: yunxi-bot feedback [--last | --id <uid>] [选项]");
        println!();
        println!("  --last              对最近一条真的通知过你的表态（最常用）");
        println!("  --id <uid>          对指定的一条表态");
        println!("  --never             这类以后别烦我（会写进策略文件）");
        println!("  --read              看过了，有用（只记台账，不改策略）");
        println!("  --ignored           看到了没管（只记台账，不改策略）");
        println!("  --domain            范围扩大到整个域名（默认只屏蔽这个发件人）");
        println!("  --note \"<说明>\"     附一句话，一并记进台账");
        return Ok(0);
    }

    let never = args.iter().any(|a| a == "--never");
    let read = args.iter().any(|a| a == "--read");
    let ignored = args.iter().any(|a| a == "--ignored");
    let picked = [never, read, ignored].iter().filter(|x| **x).count();
    if picked == 0 {
        eprintln!("要说清是什么意思：--never（以后别烦我）/ --read（有用）/ --ignored（没管）");
        return Ok(2);
    }
    if picked > 1 {
        eprintln!("--never / --read / --ignored 只能选一个");
        return Ok(2);
    }
    let feedback = if never {
        Feedback::Never
    } else if read {
        Feedback::Read
    } else {
        Feedback::Ignored
    };

    let scope = if args.iter().any(|a| a == "--domain") {
        Scope::Domain
    } else {
        Scope::Sender
    };

    let mut ledger = Ledger::open(ledger_path())?;
    let events = ledger.events().to_vec();

    // 找回"当时那条"
    let item = if args.iter().any(|a| a == "--last") {
        match last_notified(&events) {
            Some(i) => i,
            None => {
                eprintln!("台账里没有找到「真的通知过你」的记录。");
                eprintln!("（演练模式的不算——那条没到你眼前。）");
                eprintln!("先跑一轮 yunxi-bot check，或者用 --id 指定。");
                return Ok(1);
            }
        }
    } else if let Some(id) = flag(args, "--id") {
        match find_item(&events, id) {
            Some(i) => i,
            None => {
                eprintln!("台账里没有 id={id} 的判定记录。");
                eprintln!("（只能对系统判过的信息表态——它得先看过。）");
                return Ok(1);
            }
        }
    } else {
        eprintln!("要说清对哪一条表态：--last 或 --id <uid>");
        return Ok(2);
    };

    println!("针对      : {} 的「{}」", item.from_addr, item.subject);

    let note = flag(args, "--note").unwrap_or("");

    // ---- 改策略（只有 --never 会改）----
    let mut rule_added: Option<String> = None;
    if feedback.changes_policy() {
        let policy = match load_policy(&default_home()) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("策略文件有问题，先修它: {e}");
                return Ok(2);
            }
        };
        let (next, added) = match apply_never(&policy, &item, scope) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("没法据此建规则: {e}");
                return Ok(1);
            }
        };
        match &added {
            Some(rule) => {
                save_policy(&default_home(), &next)?;
                println!("范围      : {}", scope.label());
                println!("新规则    : 拉黑 {rule}");
                println!(
                    "策略文件  : {}（现在 {} 条黑名单）",
                    default_home().join(POLICY_FILE).display(),
                    next.block_senders.len()
                );
                rule_added = added;
            }
            None => {
                println!("范围      : {}", scope.label());
                println!("（这条规则已经有了，没重复添加——同一句话不该把策略文件撑大）");
            }
        }
    } else {
        println!("说明      : {}（只记台账，不改策略）", feedback.label());
        println!("（忽略一条不等于永远不想看这一类——所以它不动策略。）");
    }

    // ---- 落台账 ----
    ledger.append(
        EventKind::FeedbackRecorded,
        None,
        feedback_event_data(&item, feedback, Some(scope), rule_added.as_deref(), note),
    )?;
    println!("台账      : {}", ledger_path().display());
    Ok(0)
}

/// 看一眼信息源，判断，**该通知你的就通知你**，全程落台账。
///
/// 这是"助理"这条链路的第一个完整形态：
///
/// ```text
/// 取未读邮件 → 逐条判打扰 → 值得的弹通知 → 全部写台账
/// ```
///
/// ## 三个必须做对的地方
///
/// 1. **只有 Speak 才弹通知。** Hold 是"攒着"——攒着的东西弹通知，
///    安静时段就白设了。
/// 2. **投递结果如实报。** 勿扰模式下通知只进通知中心，那就说"未确认可见"，
///    不说"已通知"。
/// 3. **每条判定都落台账，包括"决定不通知"的那些。** 使用者问
///    "为什么这封没告诉我"时，答案必须在台账里，不能靠重跑一遍猜。
fn cmd_check(args: &[String]) -> Result<i32, Box<dyn std::error::Error>> {
    use yunxi_bot_core::Ledger;
    use yunxi_bot_core::assistant::{PassInput, run_pass};
    use yunxi_bot_core::companion::{CompanionPolicy, companion_engine};
    use yunxi_bot_core::info::InfoSource;
    use yunxi_bot_core::info::mail::{MailSource, credentials_path};
    use yunxi_bot_core::memory::Situation;
    use yunxi_bot_core::notify::Notifier;
    use yunxi_bot_core::triage::load_policy;

    let port: u16 = flag(args, "--port")
        .map(str::parse)
        .transpose()?
        .unwrap_or(yunxi_bot_core::info::mail::DEFAULT_PORT);
    let limit: usize = flag(args, "--limit")
        .map(str::parse)
        .transpose()?
        .unwrap_or(20);
    let dry_run = args.iter().any(|a| a == "--dry-run");
    let no_notify = args.iter().any(|a| a == "--no-notify");

    let src = MailSource::new(port);
    if let Err(e) = src.health() {
        eprintln!("信息源不可用: {e}");
        eprintln!("配置文件: {}", credentials_path(&default_home()).display());
        return Ok(1);
    }

    // 策略从数据目录读。**文件不存在用默认值**（第一次跑是正常的），
    // 但读到了却解析不了就报错——静默用默认值会让使用者以为自己写的规则生效了。
    let triage_policy = match load_policy(&default_home()) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("策略文件有问题: {e}");
            return Ok(2);
        }
    };

    let notifier: Box<dyn Notifier> = if no_notify {
        Box::new(yunxi_bot_core::notify::NullNotifier)
    } else if args.iter().any(|a| a == "--console") {
        Box::new(yunxi_bot_core::notify::ConsoleNotifier)
    } else {
        #[cfg(windows)]
        {
            Box::new(yunxi_bot_core::notify_windows::WindowsToast::new())
        }
        #[cfg(not(windows))]
        {
            Box::new(yunxi_bot_core::notify::NullNotifier)
        }
    };

    // 判定引擎：优先本地 Verdict。连不上就是降级——而降级方向是不打扰，
    // 所以"决策模型没起来"不会导致乱通知。
    let (_, decider, _) = open_decider(args);
    let mut engine = companion_engine(decider);

    let now = now_millis()?;
    use chrono::Timelike;
    let mut input = PassInput {
        source: &src,
        engine: &mut engine,
        notifier: notifier.as_ref(),
        policy: triage_policy,
        companion: CompanionPolicy::default(),
        ctx: Situation {
            relationship_stage: "初期".into(),
            // 命令行是使用者主动问的，不该被"距上次互动"挡住
            minutes_since_last_interaction: 999,
            ..Default::default()
        },
        local_hour: chrono::Local::now().hour() as u8,
        now_ms: now,
        limit,
        dry_run,
    };

    let mut ledger = Ledger::open(ledger_path())?;
    let r = match run_pass(&mut ledger, &mut input) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("取信息失败: {e}");
            return Ok(1);
        }
    };

    println!("信息源    : {}", src.name());
    if let Some(who) = src.describe() {
        println!("账号      : {who}");
    }
    if !input.policy.allow_senders.is_empty() || !input.policy.block_senders.is_empty() {
        println!(
            "策略      : 白名单 {} 条 · 黑名单 {} 条（{}）",
            input.policy.allow_senders.len(),
            input.policy.block_senders.len(),
            default_home()
                .join(yunxi_bot_core::triage::POLICY_FILE)
                .display()
        );
    }
    println!("未读总数  : {} 封", r.total_unseen);
    println!("本次判定  : {} 封", r.fetched);
    if r.skipped > 0 {
        println!("取不到    : {} 封（这些没被看到）", r.skipped);
    }
    println!();

    // **逐条回放这一轮的判定**，让使用者能核对每一项决定。
    // 从台账读回来而不是在内存里传：这样回放的就是**真正记下来的东西**，
    // 不是"我以为记下来的东西"。
    let start_seq = ledger.events().len() as u64;
    for ev in ledger.events() {
        if ev.kind != yunxi_bot_core::EventKind::InfoTriaged {
            continue;
        }
        let mark = match ev.data["action"].as_str().unwrap_or("") {
            "Speak" => "通知",
            "Hold" => "攒着",
            _ => "不提",
        };
        println!(
            "[{mark}] {:<22} {}",
            truncate_chars(ev.data["from"].as_str().unwrap_or(""), 20),
            truncate_chars(ev.data["subject"].as_str().unwrap_or(""), 36)
        );
        println!("        {}", ev.data["reason"].as_str().unwrap_or(""));
    }
    let _ = start_seq;

    println!();
    println!(
        "通知 {} 条 · 攒着 {} 条 · 不提 {} 条",
        r.notified, r.held, r.silent
    );
    if r.already_seen > 0 {
        println!("已经告诉过你 {} 条（不重复打扰）", r.already_seen);
    }
    println!("台账: {}", ledger_path().display());
    if dry_run {
        println!("（演练模式：没有真的发通知，但台账照记）");
    }
    Ok(0)
}
/// sidecar 没起来 / 起来了但没配邮箱 / 配好了但取不到。
/// 第三种会转述 sidecar 的原话（比如"QQ 用的是授权码不是登录密码"）。
fn cmd_mail(args: &[String]) -> Result<i32, Box<dyn std::error::Error>> {
    use yunxi_bot_core::info::InfoSource;
    use yunxi_bot_core::info::mail::{MailSource, credentials_path};

    let port: u16 = flag(args, "--port")
        .map(str::parse)
        .transpose()?
        .unwrap_or(yunxi_bot_core::info::mail::DEFAULT_PORT);
    let src = MailSource::new(port);

    // `--read <uid>` 看一封的正文
    if let Some(uid) = flag(args, "--read") {
        match src.read(uid) {
            Ok(body) => {
                println!("UID {uid} 的正文（前 1200 字）：");
                println!();
                if body.trim().is_empty() {
                    println!("（正文是空的——可能是纯 HTML 邮件或只有附件）");
                } else {
                    println!("{body}");
                }
                return Ok(0);
            }
            Err(e) => {
                eprintln!("读取失败: {e}");
                return Ok(1);
            }
        }
    }

    // 先报健康状态：把"没配"和"连不上"分开说清
    let cfg_path = credentials_path(&default_home());
    match src.health() {
        Ok(()) => {
            // **说清读的是哪个邮箱。** 使用者看到"未读 12 封"时得能确认
            // 读的是对的地方——一个报数字却不说从哪读的助理没法核实。
            match src.describe() {
                Some(who) => println!("sidecar   : 正常（端口 {port}，账号 {who}）"),
                None => println!("sidecar   : 正常（端口 {port}）"),
            }
        }
        Err(e) => {
            eprintln!("不可用    : {e}");
            eprintln!("配置文件  : {}", cfg_path.display());
            eprintln!();
            eprintln!("需要这样一份 JSON（IMAP 密码填邮箱的**授权码**，不是登录密码）：");
            eprintln!("  {{");
            eprintln!("    \"imap_host\": \"imap.qq.com\",");
            eprintln!("    \"imap_port\": 993,");
            eprintln!("    \"username\": \"you@qq.com\",");
            eprintln!("    \"password\": \"授权码\"");
            eprintln!("  }}");
            return Ok(1);
        }
    }

    let limit: usize = flag(args, "--limit")
        .map(str::parse)
        .transpose()?
        .unwrap_or(20);
    let batch = match src.fetch(limit) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("取未读失败: {e}");
            return Ok(1);
        }
    };

    println!("未读总数  : {} 封", batch.total_unseen);
    println!("本次取到  : {} 封", batch.items.len());
    if batch.skipped > 0 {
        // **"没看全"必须说出来**，否则使用者以为这就是全部
        println!("取不到    : {} 封（这些没被看到）", batch.skipped);
    }
    println!();

    if batch.items.is_empty() {
        println!("（没有未读）");
        return Ok(0);
    }

    for it in &batch.items {
        // 打扰判定会用的几个信号，这里显出来好让使用者自己核对
        let tag = if it.direct {
            "直接"
        } else if it.looks_bulk() {
            "群发"
        } else {
            "抄送"
        };
        println!(
            "[{tag}] {:<24} {}",
            truncate_chars(it.display_from(), 22),
            truncate_chars(it.display_subject(), 40)
        );
        println!("        uid={} 收件人={}", it.id, it.recipient_count);
        if !it.preview.trim().is_empty() {
            println!(
                "        {}",
                truncate_chars(&it.preview.replace('\n', " "), 70)
            );
        }
    }
    println!();
    println!("看某封正文: yunxi-bot mail --read <uid>");
    println!("（这一层是只读的：不会把你的邮件标成已读）");
    Ok(0)
}

/// 按字符截断。按字节切会落在汉字中间。
fn truncate_chars(s: &str, n: usize) -> String {
    let t = s.trim();
    if t.chars().count() <= n {
        return t.to_string();
    }
    format!("{}…", t.chars().take(n).collect::<String>())
}

/// 列出工具及其能力类别。**不需要模型，不花钱。**
///
/// 这个命令存在的理由：使用者要写 `--allow` 规则，就得知道工具叫什么、
/// 粒度是什么、默认会不会问。让人去翻源码是把成本推给使用者。
fn cmd_tools(_args: &[String]) -> Result<i32, Box<dyn std::error::Error>> {
    use yunxi_bot_core::tool::Capability;

    let mut r =
        tooling::default_registry(&default_home()).map_err(|e| format!("工具注册失败: {e}"))?;
    // **MCP 工具也要出现在这里。** 它是"我能调什么"的一部分——
    // 只在 `mcp list` 里看得到的话，使用者写审批规则时会漏掉它们。
    let mcp = tooling::with_mcp(&mut r, &default_home());
    match &mcp {
        Ok((n, problems)) => {
            if *n > 0 {
                println!("（另有 {n} 个 MCP 工具，能力类别为「能力未知」——需显式规则才放行）");
                println!();
            }
            for p in problems {
                eprintln!("  ✗ MCP: {p}");
            }
        }
        Err(e) => eprintln!("  ✗ MCP 配置有问题: {e}"),
    }
    println!("共 {} 个工具：", r.len());
    println!();
    println!("{:<14} {:<8} 默认是否免问", "工具", "能力");
    for (name, cap) in r.capabilities() {
        let free = match cap {
            Capability::ReadOnly => "是（只读）",
            _ => "否（要问人）",
        };
        println!("{name:<14} {:<8} {free}", cap.label());
    }
    println!();
    println!("能力类别的含义：");
    println!("  只读    无副作用。工作区内的读自动放行。");
    println!("  写入    改文件。每次要问，除非写了 --allow。");
    println!("  执行    跑命令。每次要问，除非写了 --allow。");
    println!("  网络    出站请求。单列而不并进只读——它会泄露 URL，");
    println!("          返回的内容还是不可信数据，会进模型上下文。");
    println!("  不可逆  对外发送/删除这类。**永远问人**，不接受模型判定，");
    println!("          且「总是允许」对它无效。");
    println!();
    println!("写规则：--allow <工具>[:粒度]，例如");
    println!("  --allow read_file:D:\\notes      总是允许读这个目录");
    println!("  --allow run_command:git         总是允许跑 git");
    println!("  --allow web_fetch:https://docs.rs  总是允许抓这个站");
    Ok(0)
}

fn cmd_tasks(args: &[String]) -> Result<i32, Box<dyn std::error::Error>> {
    use yunxi_bot_core::task::engine::step_table;
    let store = LedgerTaskStore::new(Ledger::open(ledger_path())?);
    let tasks = store.project();

    if let Some(id) = args.first().filter(|s| !s.starts_with("--")) {
        let Some(t) = tasks.get(id) else {
            eprintln!("找不到任务 {id}");
            return Ok(2);
        };
        println!("任务      : {}", t.id);
        println!("目标      : {}", t.goal);
        println!("状态      : {}", t.state.label());
        let (done, failed, skipped) = t.tally();
        println!(
            "进度      : 完成 {done} / 失败 {failed} / 跳过 {skipped} / 共 {}",
            t.steps.len()
        );
        println!();
        println!("{:<6} {:<8} {:<10} 指令", "步骤", "状态", "依赖");
        for (id, state, deps) in step_table(t) {
            let instr = t
                .step(&id)
                .map(|s| s.instruction.clone())
                .unwrap_or_default();
            let head: String = instr.chars().take(46).collect();
            println!("{id:<6} {state:<8} {deps:<10} {head}");
        }
        // 路由留痕：解释"为什么花了这笔钱"
        let routed: Vec<&yunxi_bot_core::task::Step> =
            t.steps.iter().filter(|s| s.routing.is_some()).collect();
        if !routed.is_empty() {
            println!();
            println!("路由留痕:");
            for s in routed {
                let r = s.routing.as_ref().expect("上面刚判过");
                println!(
                    "  {} → {}/{} 思考{}",
                    s.id,
                    r.provider,
                    r.model,
                    if r.thinking { "开" } else { "关" }
                );
                println!("      {}", r.reason);
            }
        }
        // 结果与失败原因**都要显示**，只显示成功的会让人以为做完了
        let with_result: Vec<&yunxi_bot_core::task::Step> =
            t.steps.iter().filter(|s| s.result.is_some()).collect();
        if !with_result.is_empty() {
            println!();
            println!("结果:");
            for s in with_result {
                let r = s.result.as_ref().expect("上面刚判过");
                let head: String = r.chars().take(200).collect();
                println!("  [{}] {}", s.id, head);
            }
        }
        return Ok(0);
    }

    if tasks.is_empty() {
        println!("（还没有用 `do` 提交过任务）");
        return Ok(0);
    }
    println!("{:<34} {:<8} {:<16} 目标", "任务 id", "状态", "进度");
    for (id, t) in &tasks {
        let (done, failed, skipped) = t.tally();
        let progress = format!(
            "{done}/{}{}{}",
            t.steps.len(),
            if failed > 0 {
                format!(" 失败{failed}")
            } else {
                String::new()
            },
            if skipped > 0 {
                format!(" 跳过{skipped}")
            } else {
                String::new()
            }
        );
        let goal: String = t.goal.chars().take(30).collect();
        println!("{id:<34} {:<8} {progress:<16} {goal}", t.state.label());
    }
    Ok(0)
}

fn cmd_resume(args: &[String]) -> Result<i32, Box<dyn std::error::Error>> {
    use yunxi_bot_core::task::engine::{Engine, LedgerTaskStore, summarize};
    use yunxi_bot_core::task::{Budget, TaskState};
    use yunxi_bot_core::think::ModelRouter;

    let Some(id) = args.first().filter(|s| !s.starts_with("--")) else {
        eprintln!("缺少任务 id。用法: yunxi-bot resume <id>");
        return Ok(2);
    };

    let router = ModelRouter::default();
    let (_, decider, _) = open_decider(args);
    let mut store = LedgerTaskStore::new(Ledger::open(ledger_path())?);
    let tasks = store.project();
    let Some(t) = tasks.get(id) else {
        eprintln!("找不到任务 {id}");
        return Ok(2);
    };
    if t.state.is_terminal() {
        println!("任务 {id} 已经是{}，不需要续跑。", t.state.label());
        return Ok(0);
    }
    if t.state == TaskState::AwaitingHuman {
        // **先把人的答案落进台账，再把状态放回执行中。**
        //
        // 顺序不能反：状态一变成 running，引擎下一轮就会去看
        // "这一步有没有人答过"。先放状态、后写答案的话，
        // 那一次仍然会走到决策模型，又弃权一次。
        //
        // 而在此之前根本没有这一步——`resume` 只改状态，
        // **没有地方接人的决定**，于是同一个 decide 步骤
        // 反复问、反复弃权，**死循环**。
        if let Some(answer) = flag(args, "--answer") {
            // 答案要落到**具体哪一步**上。人可能是在回答任务里
            // 第一个待决的决策步骤——把它找出来。
            let target = t
                .steps
                .iter()
                .find(|st| {
                    st.state != yunxi_bot_core::task::StepState::Succeeded
                        && st
                            .instruction
                            .trim_start()
                            .starts_with(yunxi_bot_core::task::engine::DECIDE_PREFIX)
                })
                .map(|st| st.id.clone());
            match target {
                Some(step_id) => {
                    store.write(
                        id,
                        EventKind::HumanAnswered,
                        serde_json::json!({
                            "task": id,
                            "step": step_id,
                            "answer": answer,
                        }),
                    )?;
                    // **还要把这一步放回待执行。**
                    //
                    // 只记答案不够：这个步骤很可能已经因为"反复弃权"
                    // 把尝试次数用光、成了 Failed——而引擎不会再选它。
                    // 于是答案记下了，任务却仍然卡着。
                    //
                    // 人给了答案 = **这一步重新开始**，所以连重试次数一起重置。
                    if let Some(st) = t.steps.iter().find(|x| x.id == step_id) {
                        store.write(
                            id,
                            EventKind::StepPending,
                            serde_json::json!({
                                "task": id,
                                "step": st.id,
                                "instruction": st.instruction,
                                "kind": st.kind,
                                "depends_on": st.depends_on,
                            }),
                        )?;
                    }
                    println!("已记下你对步骤 {step_id} 的决定：{answer}");
                }
                None => {
                    // **不静默丢弃。** 人以为答案交上去了，而系统没用它——
                    // 那比报错更糟：他会等一个永远不会发生的结果。
                    eprintln!("这个任务现在没有待决的决策步骤，--answer 没被用上。");
                    eprintln!("用 `yunxi-bot tasks {id}` 看看它在等什么。");
                    return Ok(2);
                }
            }
        }
        // 人处理完了，把状态放回执行中，剩下的交给引擎
        store.write(
            id,
            EventKind::TaskStateChanged,
            serde_json::json!({ "task": id, "state": "running" }),
        )?;
        println!(
            "已把任务 {id} 从「{}」放回执行中。",
            TaskState::AwaitingHuman.label()
        );
        if flag(args, "--answer").is_none() {
            println!("（没给 --answer：如果它在等一个决策，会再问一次。）");
        }
    }

    let budget = Budget {
        max_model_calls: flag(args, "--budget")
            .map(str::parse)
            .transpose()?
            .unwrap_or(20),
        max_steps: t.steps.len().max(12) as u32,
        ..Default::default()
    };
    let effort = parse_effort(args)?;

    let mut handler = chat_handler::ChatHandler::new(
        default_home(),
        "云熙",
        chat_handler::DEFAULT_PERSONA,
        chat_handler::default_rules(),
    );
    // 续跑时也要挂上同一套工具与策略——否则"同一个任务在 do 和 resume 里
    // 行为不同"，那种 bug 极难查
    let tools =
        tooling::default_registry(&default_home()).map_err(|e| format!("工具注册失败: {e}"))?;
    handler = handler
        .with_tools(tools)
        .with_approver(tooling::approver_from_args(args))
        .with_policy(tooling::policy_from_args(args))
        // **每一次工具调用都要落台账**（见 `tool_sink` 模块文档）
        .with_sink(tool_sink_or_warn())
        .with_decider(std::sync::Arc::new(
            yunxi_bot_core::decide::LayaDecider::new(
                flag(args, "--endpoint").unwrap_or("http://127.0.0.1:17870/decide"),
            ),
        ));
    let outcome = {
        let mut e =
            Engine::new(&router, &decider, &mut handler, &mut store, budget).with_effort(effort);
        e.run(id)?
    };
    print!("{}", summarize(&outcome.task));
    println!("\n本次模型调用 : {} 次", outcome.used_model_calls);
    let (done, failed, skipped) = outcome.task.tally();
    match &outcome.advance {
        yunxi_bot_core::task::Advance::Finished => {
            if failed == 0 && skipped == 0 {
                println!("结果      : 完成（{done} 步全部成功）");
                Ok(0)
            } else {
                println!("结果      : 完成，但有 {failed} 步失败、{skipped} 步被跳过");
                Ok(1)
            }
        }
        yunxi_bot_core::task::Advance::Waiting { reason } => {
            println!("仍在等人 : {reason}");
            Ok(3)
        }
        yunxi_bot_core::task::Advance::Progress { .. } => Ok(1),
    }
}

fn cmd_cost(args: &[String]) -> Result<i32, Box<dyn std::error::Error>> {
    use yunxi_bot_core::costlog::{price_for, project_costs};
    let l = Ledger::open(ledger_path())?;
    let r = project_costs(l.events());

    if r.calls == 0 {
        println!("（台账里还没有模型调用记录）");
        return Ok(0);
    }
    println!("调用次数  : {}", r.calls);
    println!(
        "token     : 输入 {}（命中 {} / 未命中 {}）/ 输出 {}",
        r.prompt_tokens, r.cache_hit_tokens, r.cache_miss_tokens, r.completion_tokens
    );
    if let Some(rate) = r.cache_hit_rate() {
        println!("缓存命中率: {:.1}%", rate * 100.0);
    } else {
        println!("缓存命中率: 未知（服务端没回缓存字段）");
    }
    if r.calls_without_cache_data > 0 {
        println!(
            "           其中 {} 次没有缓存数据——这些按未命中计费，实际可能更省",
            r.calls_without_cache_data
        );
    }
    println!("思考调用  : {}/{} 次", r.thinking_calls, r.calls);
    println!();
    println!("合计      : ¥{:.6}", r.total);
    if r.saved() > 0.0 {
        println!(
            "缓存省下  : ¥{:.6}（不命中会花 ¥{:.6}）",
            r.saved(),
            r.without_cache
        );
    }

    let n: usize = flag(args, "--calls")
        .map(str::parse)
        .transpose()?
        .unwrap_or(0);
    if n > 0 {
        use yunxi_bot_core::costlog::CallRecord;
        println!();
        println!("最近 {n} 次调用:");
        let recs: Vec<CallRecord> = l
            .events()
            .iter()
            .filter(|e| e.kind == EventKind::ModelCalled)
            .filter_map(|e| serde_json::from_value(e.data.clone()).ok())
            .collect();
        for rec in recs.iter().rev().take(n).rev() {
            let money = price_for(&rec.provider)
                .map(|p| rec.cost(&p).total())
                .unwrap_or(0.0);
            println!(
                "  {}/{} 思考{} {}tok 入/{}tok 出 ¥{:.6}",
                rec.provider,
                rec.model,
                if rec.thinking { "开" } else { "关" },
                rec.usage.prompt_tokens,
                rec.usage.completion_tokens,
                money
            );
        }
    }
    Ok(0)
}

fn cmd_log(args: &[String]) -> Result<i32, Box<dyn std::error::Error>> {
    let n: usize = flag(args, "-n").map(str::parse).transpose()?.unwrap_or(20);
    let l = Ledger::open(ledger_path())?;
    let events = l.events();
    if events.is_empty() {
        println!("（台账为空）");
        return Ok(0);
    }
    let start = events.len().saturating_sub(n);
    println!(
        "台账格式版本 {}｜共 {} 条事件，显示最后 {}",
        1,
        events.len(),
        events.len() - start
    );
    println!();
    for e in &events[start..] {
        let kind = serde_json::to_value(e.kind)?;
        let kind = kind.as_str().unwrap_or("?");
        let job = e.job.as_deref().unwrap_or("-");
        let span = e.span.map(|s| format!(" span#{s}")).unwrap_or_default();
        let extra = summarize_data(&e.data);
        println!("{:>4}  {:<22} {:<18}{}{}", e.seq, kind, job, span, extra);
    }
    Ok(0)
}

/// 把事件数据压成一行短摘要，便于终端阅读。
fn summarize_data(d: &serde_json::Value) -> String {
    let Some(obj) = d.as_object() else {
        return String::new();
    };
    let mut parts = Vec::new();
    for key in [
        "error",
        "exit_code",
        "duration_ms",
        "isolation",
        "reason",
        "by",
        "preset",
        "mode",
    ] {
        if let Some(v) = obj.get(key) {
            let s = match v {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            let s = s.replace('\n', " ");
            let s: String = s.chars().take(46).collect();
            parts.push(format!("{key}={s}"));
        }
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!("  {}", parts.join(" "))
    }
}

/// 决策层的演示入口。
///
/// `--demo` 不需要模型，直接打印六类降级方向——这是整个降级策略最容易搞错、
/// 也最该被一眼看清的地方。不带 `--demo` 时真实调用本地 sidecar。
fn cmd_decide(args: &[String]) -> Result<i32, Box<dyn std::error::Error>> {
    use yunxi_bot_core::decide::{
        DecisionClass, DecisionEngine, DegradationDirection, LayaDecider, Question,
    };

    if args.iter().any(|a| a == "--demo") {
        println!("决策类别与降级方向（ADR §7.2）:\n");
        println!("  {:<16} {:<14} 降级时的保守动作", "类别", "降级方向");
        for c in [
            DecisionClass::Interrupt,
            DecisionClass::Escalate,
            DecisionClass::Irreversible,
            DecisionClass::Classify,
            DecisionClass::Urgency,
            DecisionClass::Anomaly,
        ] {
            let dir = match c.direction() {
                DegradationDirection::FailClosed => "fail-closed",
                DegradationDirection::FailOpen => "fail-open",
                DegradationDirection::NotApplicable => "不参与降级",
            };
            println!("  {:<16} {:<14} {}", c.label(), dir, c.degraded_action());
        }
        println!(
            "\n注意第 1 行与第 2 行【方向相反】：打扰类不确定就不打扰，\n\
             安全类不确定就要叫人。统一的「模型挂了走规则兜底」会同时犯两个错。"
        );
        return Ok(0);
    }

    let endpoint = flag(args, "--endpoint").unwrap_or("http://127.0.0.1:17870/decide");

    // 内置问题集：直接对应本项目的存在理由——此刻该不该介入
    let questions = vec![
        Question::choice(
            "intervention",
            "此刻应当如何介入？",
            &[
                ("speak", "有值得主动说明的信息，且现在说不打扰"),
                ("quiet", "没有需要主动说明的，保持静默即可"),
                ("hold", "有信息但现在不适合说，留到合适的时候"),
            ],
        ),
        Question::noul("needs_support", "使用者此刻是否处于需要情绪支持的状态？"),
        Question::noul(
            "is_anomaly",
            "近期的运行结果中是否出现了显著偏离常态的情况？",
        ),
    ];

    let state = serde_json::json!({
        "local_time": "2026-10-05T19:30:00+08:00",
        "quiet_hours": false,
        "unread_events": 3,
        "last_interaction_hours_ago": 6,
        "recent_failures": 0,
    });

    println!("决策模型 : {endpoint}");
    println!("类别     : 打扰/通知（降级方向 fail-closed）");
    println!();

    let decider = LayaDecider::new(endpoint);
    let mut engine = DecisionEngine::new(decider, DecisionClass::Interrupt);

    let req = yunxi_bot_core::decide::DecisionRequest::new(state, questions);
    let outcome = engine.decide(&req);

    println!("台账数据（降级必须可见）:");
    println!("{}", serde_json::to_string_pretty(&outcome.ledger_data())?);
    println!();

    match &outcome {
        yunxi_bot_core::decide::DecisionOutcome::Decided(r) => {
            println!("模型作答（注意：这只是策略的输入，不是最终决定）:");
            println!("  模型 : {}", r.model);
            for (id, a) in &r.answers {
                println!("  {id:<16} {a:?}");
            }
            println!("\n最终该不该打扰，仍由约束层用阈值与硬规则决定。");
            Ok(0)
        }
        yunxi_bot_core::decide::DecisionOutcome::Degraded { reason, action, .. } => {
            println!("已降级: {reason}");
            println!("保守动作: {action}");
            println!("\n（sidecar 未启动时会走到这里，这是预期行为，不是故障。）");
            Ok(0)
        }
    }
}

/// 陪伴层演示：完整走一遍「记忆 → 模型判断 → 约束收紧」。
///
/// `--demo` 用桩模型（总是说"该开口"），把重点放在**约束如何把它压下来**——
/// 这是整个陪伴层最容易被实现反的地方（方向搞反就变成"模型说了算"）。
fn cmd_companion(args: &[String]) -> Result<i32, Box<dyn std::error::Error>> {
    use yunxi_bot_core::companion::{CompanionPolicy, companion_engine, decide_intervention};
    use yunxi_bot_core::decide::{LayaDecider, StubDecider};
    use yunxi_bot_core::ledger::{Event, EventKind};
    use yunxi_bot_core::memory::{Memory, MemoryKind, Situation};

    let demo = args.iter().any(|a| a == "--demo");

    // 用几条记忆构造 state（体现"陪伴的温度来自连续性"）
    let memory = Memory::from_events(&[
        Event {
            seq: 1,
            at: 1_000,
            kind: EventKind::MemoryRecorded,
            span: None,
            job: None,
            data: serde_json::json!({"id":"f1","kind":"fact","text":"在准备 AI 岗位面试"}),
        },
        Event {
            seq: 2,
            at: 2_000,
            kind: EventKind::MemoryRecorded,
            span: None,
            job: None,
            data: serde_json::json!({"id":"p1","kind":"preference","text":"深夜不喜欢被打扰"}),
        },
        Event {
            seq: 3,
            at: 3_000,
            kind: EventKind::MemoryRecorded,
            span: None,
            job: None,
            data: serde_json::json!({"id":"r1","kind":"relationship","text":"一起把简历改完了"}),
        },
    ]);

    println!("记忆 {} 条（事实/偏好/关系）", memory.len());
    for k in [
        MemoryKind::Fact,
        MemoryKind::Preference,
        MemoryKind::Relationship,
    ] {
        let items: Vec<&str> = memory
            .recall(k, 10_000_000, 5)
            .iter()
            .map(|e| e.text.as_str())
            .collect();
        if !items.is_empty() {
            println!("  {:<4} {}", k.label(), items.join("；"));
        }
    }
    println!();

    let policy = CompanionPolicy::default();
    println!(
        "陪伴策略（使用者设定的硬事实，不交给模型判断）: 安静时段 {}:00–{}:00，每日上限 {} 次，最小间隔 {} 分钟",
        policy.quiet_start_hour,
        policy.quiet_end_hour,
        policy.max_interventions_per_day,
        policy.min_minutes_between_interventions
    );
    println!();

    let endpoint = flag(args, "--endpoint").unwrap_or("http://127.0.0.1:17870/decide");

    // --demo：用"总是建议开口"的桩模型，专门检验约束是否真的能压住它
    let mut stub_engine = demo.then(|| {
        companion_engine(
            StubDecider::succeeding()
                .with_choice("intervention", "speak")
                .with_noul("needs_support", 0.8)
                .with_noul("is_anomaly", 0.1),
        )
    });
    let mut real_engine = (!demo).then(|| companion_engine(LayaDecider::new(endpoint)));

    let (h_time, h_model, h_action, h_note) = ("时刻", "模型建议", "最终动作", "说明");
    println!("  {h_time:<6} {h_model:<10} {h_action:<10} {h_note}");
    println!("  {}", "-".repeat(76));

    let base = Situation {
        quiet_hours: false,
        minutes_since_last_interaction: 600,
        unread_events: 3,
        recent_failures: 0,
        interventions_today: 0,
        relationship_stage: "熟悉".into(),
    };

    // 决策台账：每次判断都写成一对被边界包住的审计事件。
    // 没有它，就永远无法回答"当初为什么决定不打扰、事后看对不对"。
    let mut ledger = Ledger::open(ledger_path())?;
    let mut recorded = 0usize;

    for (hour, label) in [(3u8, "凌晨"), (9, "上午"), (14, "下午"), (23, "深夜")] {
        let decision = if let Some(engine) = stub_engine.as_mut() {
            decide_intervention(engine, &memory, &base, &policy, hour, 10_000_000)
        } else if let Some(engine) = real_engine.as_mut() {
            decide_intervention(engine, &memory, &base, &policy, hour, 10_000_000)
        } else {
            unreachable!()
        };

        // 记录决策：asked + decided 一对，落在同一个边界内。
        // `degraded` 如实来自判断结果。
        if yunxi_bot_core::decide::record_decision(
            &mut ledger,
            yunxi_bot_core::decide::DecisionClass::Interrupt,
            decision.degraded,
            decision.action.label(),
            &decision.reason,
            None,
            &["intervention".to_string(), "needs_support".to_string()],
        )
        .is_ok()
        {
            recorded += 1;
        }

        let suggested = if decision.model_suggested_speak {
            "开口"
        } else if decision.degraded {
            "（降级）"
        } else {
            "不开口"
        };
        let mark = if decision.action == yunxi_bot_core::companion::Intervention::Speak {
            ""
        } else {
            " ← 被约束压住"
        };
        println!(
            "  {:<6} {:<10} {:<10} {}{}",
            format!("{hour:02}:00 {label}"),
            suggested,
            decision.action.label(),
            decision.reason,
            mark
        );
    }
    println!("\n已写入决策台账：{recorded} 对审计事件（asked + decided，各自被边界包住）");
    println!("台账文件：{}", ledger.path().display());

    println!();
    if demo {
        println!("说明：桩模型每一刻都建议「开口」。约束层把安静时段与每日上限");
        println!("     压了回来——这就是「约束只能更保守」的直接演示。");
    } else if real_engine
        .as_ref()
        .map(|e| e.circuit_open())
        .unwrap_or(false)
    {
        println!("警告：决策模型连续失败已熔断。");
    }
    Ok(0)
}

fn cmd_policy() -> Result<i32, Box<dyn std::error::Error>> {
    println!("权限预设（两个正交旋钮：沙箱级别 × 审批策略）:\n");
    let (h_preset, h_sandbox, h_approval) = ("预设", "沙箱", "审批");
    println!("  {h_preset:<12} {h_sandbox:<20} {h_approval}");
    for p in policy::BUILTIN_PRESETS {
        println!(
            "  {:<12} {:<20} {:?}",
            p.name,
            format!("{:?}", p.sandbox),
            p.approval
        );
    }
    println!("\n审批结果词汇表（封闭）:");
    for o in ["allowed-once", "rejected", "cancelled", "unavailable"] {
        println!("  {o}");
    }
    println!("\n注意: `never` 策略表示「需要批准的动作自动拒绝」，不是自动放行。");
    Ok(0)
}
