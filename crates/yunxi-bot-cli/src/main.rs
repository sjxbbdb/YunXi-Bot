//! YunXi Bot 命令行入口。
//!
//! 当前里程碑：**可运行的调度原型**——任务能按触发条件自动执行、结果落台账、
//! 需批准的任务被拦下、崩溃残留被回收、状态跨进程存活。

use std::path::PathBuf;
use std::thread;
use std::time::Duration;

use yunxi_bot_core::job::{JobId, JobSpec, JobState, Trigger};
use yunxi_bot_core::ledger::{EventKind, Ledger};
use yunxi_bot_core::{TickOptions, now_millis, policy, tick};

fn default_home() -> PathBuf {
    if let Ok(v) = std::env::var("YUNXI_BOT_HOME") {
        return PathBuf::from(v);
    }
    if cfg!(windows) {
        if let Ok(la) = std::env::var("LOCALAPPDATA") {
            return PathBuf::from(la).join("YunXiBot");
        }
    }
    let mut p = PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()));
    p.push(".yunxi-bot");
    p
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
  yunxi-bot approve <id>      批准待批准任务
  yunxi-bot log [-n N]        显示最近 N 条台账事件（默认 20）
  yunxi-bot status            数据目录与台账概况
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

fn cmd_daemon(args: &[String]) -> Result<i32, Box<dyn std::error::Error>> {
    let interval: u64 = flag(args, "--interval")
        .map(str::parse)
        .transpose()?
        .unwrap_or(5000);
    let max_ticks: u64 = flag(args, "--max-ticks")
        .map(str::parse)
        .transpose()?
        .unwrap_or(0);
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

    let mut engine = if agent_on {
        use yunxi_bot_core::agent::agent_engine;
        use yunxi_bot_core::decide::LayaDecider;
        let endpoint = flag(args, "--endpoint").unwrap_or("http://127.0.0.1:17870/decide");
        println!("  Agent  : 开启（每 {judge_every} 轮判断一次）");
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

    let mut n = 0u64;
    let mut consecutive_errors = 0u32;
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
            if judge_every > 0 && n % judge_every == 0 {
                run_agent_round(&mut l, engine, thinker.as_ref(), &agent_home, n);
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
