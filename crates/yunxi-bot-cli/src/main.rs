//! YunXi Bot 命令行入口。
//!
//! 当前实现覆盖「看得见台账」这一步：创建任务、列出任务、批准、查看台账概况。
//! 调度与执行由后续里程碑补上（见 `docs/adr/0001-架构与边界.md` §九）。

use std::path::PathBuf;

use yunxi_bot_core::job::{JobId, JobSpec, Trigger};
use yunxi_bot_core::ledger::{EventKind, Ledger};
use yunxi_bot_core::{now_millis, policy};

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

  yunxi-bot list              列出全部任务
  yunxi-bot approve <id>      批准待批准任务
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

    match cmd {
        "help" | "-h" | "--help" => {
            print!("{USAGE}");
            Ok(0)
        }
        "status" => cmd_status(),
        "list" => cmd_list(),
        "add" => cmd_add(&args[1..]),
        "approve" => cmd_approve(&args[1..]),
        "policy" => cmd_policy(),
        other => {
            eprintln!("未知命令: {other}\n");
            print!("{USAGE}");
            Ok(2)
        }
    }
}

fn cmd_status() -> Result<i32, Box<dyn std::error::Error>> {
    let l = Ledger::open(ledger_path())?;
    let jobs = l.rebuild();
    println!("数据目录 : {}", default_home().display());
    println!("台账文件 : {}", l.path().display());
    println!("事件总数 : {}", l.len());
    println!("任务总数 : {}", jobs.len());
    if !jobs.is_empty() {
        println!("\n状态分布:");
        let mut counts: std::collections::BTreeMap<String, usize> = Default::default();
        for j in jobs.values() {
            *counts.entry(format!("{:?}", j.state)).or_default() += 1;
        }
        for (k, v) in counts {
            println!("  {k:<18} {v}");
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
        let trig = match &j.spec.trigger {
            Trigger::Manual => "手动".to_string(),
            Trigger::Every { seconds } => format!("每 {seconds}s"),
            Trigger::Cron { expr } => format!("cron({expr})"),
            Trigger::Watch { path } => format!("watch({path})"),
        };
        let flag = if j.spec.irreversible {
            " ⚠不可逆"
        } else {
            ""
        };
        println!(
            "{:<18} {:<16} {:<20} {}{}  尝试 {}/{}  批准 {}",
            id,
            format!("{:?}", j.state),
            j.spec.name,
            trig,
            flag,
            j.attempts,
            j.spec.max_attempts,
            j.approvals
        );
        if let Some(e) = &j.last_error {
            println!("                    上次错误: {e}");
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
        // 解析失败直接拒绝，不入库一个永远不触发的任务
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
    let jobs = l.rebuild();
    let jid = JobId::new(id.clone());
    if !jobs.contains_key(&jid) {
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
