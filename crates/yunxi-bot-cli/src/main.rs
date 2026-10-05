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

  yunxi-bot run <id>          立即执行一次（忽略触发条件）
  yunxi-bot list              列出全部任务
  yunxi-bot approve <id>      批准待批准任务
  yunxi-bot log [-n N]        显示最近 N 条台账事件（默认 20）
  yunxi-bot status            数据目录与台账概况
  yunxi-bot policy            列出权限预设

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
    println!("YunXi Bot 守护启动");
    println!("  台账   : {}", l.path().display());
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
    loop {
        n += 1;
        let r = tick(&mut l, &opts)?;
        if !r.is_quiet() {
            print!("[{n:>3}] ");
            print_report(&r);
        }
        if max_ticks > 0 && n >= max_ticks {
            break;
        }
        thread::sleep(Duration::from_millis(interval));
    }

    println!("\n守护已停止（共 {n} 轮）");
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
