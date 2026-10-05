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

  yunxi-bot install-autostart    注册当前用户登录时自启（Windows 计划任务）
  yunxi-bot uninstall-autostart  取消自启
  yunxi-bot autostart-status     查看自启注册状态

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

  yunxi-bot help              显示本帮助
";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
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
        "install-autostart" => cmd_autostart(AutostartAction::Install),
        "uninstall-autostart" => cmd_autostart(AutostartAction::Uninstall),
        "autostart-status" => cmd_autostart(AutostartAction::Status),
        "tick" => cmd_tick(),
        "daemon" => cmd_daemon(rest),
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
        if max_ticks > 0 && n >= max_ticks {
            break;
        }
        thread::sleep(Duration::from_millis(interval));
    }

    println!("\n守护已停止（共 {n} 轮）");
    Ok(0)
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

                // 0 = 隐藏窗口，False = 不等待
                let script = format!(
                    "' YunXi Bot 常驻守护 —— 由 `yunxi-bot install-autostart` 生成\r\n\
                     ' 删除本文件即可取消自启。\r\n\
                     CreateObject(\"WScript.Shell\").Run \"\"\"{exe}\"\" daemon\", 0, False\r\n",
                    exe = exe.display()
                );
                std::fs::write(&file, script)?;

                println!("已注册自启：当前用户登录时后台启动");
                println!("  启动器 : {}", file.display());
                println!("  目标   : {} daemon", exe.display());
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
