//! 工具装配：把工具注册成一套，把命令行参数变成审批策略。
//!
//! ## 为什么单独一个模块
//!
//! "接了哪些工具"和"默认允不允许"是两件经常要一起看的事，散在 `cmd_do`
//! 和 `cmd_resume` 里就会各写一份、迟早漂移。**两份不一致的表现是
//! "同一个任务在 `do` 和 `resume` 里行为不同"**，那种 bug 极难查。

use std::path::Path;
use std::sync::{Arc, Mutex};

use yunxi_bot_core::tool::files::{
    EditFileTool, ListDirTool, ReadFileTool, SearchFilesTool, WriteFileTool,
};
use yunxi_bot_core::tool::system::{AskFn, AskUserTool, NowTool, RunCommandTool};
use yunxi_bot_core::tool::web::{
    BochaKey, BochaSearch, DuckDuckGoLite, SearchProvider, WebFetchTool, WebSearchTool,
};
use yunxi_bot_core::tool::{Rule, ToolPolicy, ToolRegistry};

use crate::approval::{StdinApprover, parse_rule};

/// 从标准输入读一句回答。给 `ask_user` 用。
///
/// **它和审批是两条不同的路**：审批问的是"准不准你动我的东西"，
/// `ask_user` 问的是"这事我需要你告诉我一个信息"。混在一起会让
/// 使用者分不清自己是在授权还是在提供信息——而这两件事的后果差很远。
fn stdin_ask(question: &str, options: &[String]) -> Option<String> {
    println!();
    println!("┌─ 助理需要问你 ───────────────────────────");
    println!("│ {question}");
    if !options.is_empty() {
        println!("│ 可选项：");
        for (i, o) in options.iter().enumerate() {
            println!("│   {}. {o}", i + 1);
        }
    }
    println!("└──────────────────────────────────────────");
    print!("你的回答（直接回车=不作答）: ");
    {
        use std::io::Write;
        let _ = std::io::stdout().flush();
    }

    let mut line = String::new();
    let n = {
        use std::io::BufRead;
        std::io::stdin().lock().read_line(&mut line).ok()?
    };
    // 读不到（管道关了）或空行 → 当作"答不上来"。
    // 返回空字符串会让模型以为使用者给了一个空格当答案。
    if n == 0 || line.trim().is_empty() {
        return None;
    }

    let answer = line.trim().to_string();
    // 给了选项就接受序号，省得使用者手打一遍
    if !options.is_empty() {
        if let Ok(i) = answer.parse::<usize>() {
            if i >= 1 && i <= options.len() {
                return Some(options[i - 1].clone());
            }
        }
    }
    Some(answer)
}

/// 挑搜索后端。
///
/// **有 key 就用博查，没有才回落 DuckDuckGo Lite。** 顺序不能反：
/// DDG 那条路是抓 HTML 的，实测当天就已经反爬 202 / 走代理 400；
/// 而博查是有 key、有服务条款的正式接口。**稳定性不是靠解析得巧，
/// 是靠有一条正式约定。**
///
/// 但**回落必须存在**：一个能用的兜底比一个用不了的优选更有价值，
/// 没配 key 的人不该连搜索都没有。
///
/// 密钥位置与 Agnes / DeepSeek 同一套约定：`<home>/secrets/bocha.key`，
/// 在仓库之外，权限只给当前用户。
fn search_provider(home: &Path) -> Box<dyn SearchProvider> {
    let key_path = home.join("secrets").join("bocha.key");
    match BochaKey::load(&key_path) {
        Ok(k) => {
            println!("搜索      : 博查（带 key，直连不需要代理）");
            Box::new(BochaSearch::new(k))
        }
        Err(e) => {
            println!("搜索      : DuckDuckGo Lite（抓 HTML，**会失效**）");
            println!("           没读到博查密钥：{e}");
            println!("           想稳就去 https://open.bochaai.com/ 申请，存到");
            println!("           {}", key_path.display());
            Box::new(DuckDuckGoLite::new())
        }
    }
}

/// 默认工具集。
///
/// 十个工具分三类，都能在 `docs/工具层调研与设计.md` 里找到"为什么是它"：
///
/// | 类别 | 工具 | 能力 |
/// |---|---|---|
/// | 信息 | `now` `web_fetch` `web_search` | 只读 / 网络 |
/// | 文件 | `read_file` `list_dir` `search_files` `write_file` `edit_file` | 只读 / 写入 |
/// | 动作 | `run_command` `ask_user` | 执行 / 只读 |
pub fn default_registry(home: &Path) -> Result<ToolRegistry, String> {
    let mut r = ToolRegistry::new();
    let mut add = |t: Arc<dyn yunxi_bot_core::tool::Tool>| -> Result<(), String> {
        r.register(t).map_err(|e| e.to_string())
    };

    // —— 信息 ——
    add(Arc::new(NowTool))?;
    add(Arc::new(WebFetchTool::new()))?;
    add(Arc::new(WebSearchTool::new(search_provider(home))))?;

    // —— 文件 ——
    add(Arc::new(ReadFileTool))?;
    add(Arc::new(ListDirTool))?;
    add(Arc::new(SearchFilesTool))?;
    add(Arc::new(WriteFileTool))?;
    add(Arc::new(EditFileTool))?;

    // —— 动作 ——
    add(Arc::new(RunCommandTool))?;
    let ask: AskFn = Box::new(stdin_ask);
    add(Arc::new(AskUserTool::new(ask)))?;

    Ok(r)
}

/// 从命令行参数拼审批策略。
///
/// 没给任何 `--allow` 时策略是**默认的**：只读在工作区内免问，
/// 其余一律走"问人"。给使用者减负的是"只读免问"这一条，不是放宽写操作。
pub fn policy_from_args(args: &[String]) -> ToolPolicy {
    let mut p = ToolPolicy::default();
    for a in flags(args, "--allow") {
        let (tool, spec) = parse_rule(&a);
        p.allow.push(match spec {
            Some(s) => Rule::scoped(tool, s),
            None => Rule::tool(tool),
        });
    }
    for a in flags(args, "--deny") {
        let (tool, spec) = parse_rule(&a);
        p.deny.push(match spec {
            Some(s) => Rule::scoped(tool, s),
            None => Rule::tool(tool),
        });
    }
    p
}

/// 取某个 flag 的全部出现（不是只有第一个）。
///
/// `--flag value --flag value` 这种重复形式，`flag()` 只看第一个，
/// 而白名单天然是可重复的。
fn flags(args: &[String], name: &str) -> Vec<String> {
    args.iter()
        .enumerate()
        .filter(|(i, a)| {
            a.as_str() == name
                // 别把 --allow 后面的值当成另一个 flag 的名字
                && !(*i > 0 && args[i - 1].as_str() == name)
        })
        .filter_map(|(i, _)| args.get(i + 1).cloned())
        .collect()
}

/// 造审批者。
pub fn approver_from_args(args: &[String]) -> Arc<Mutex<dyn yunxi_bot_core::tool::Approver>> {
    let mut a = StdinApprover::new();
    if args.iter().any(|x| x == "--yes") {
        a = a.assume_yes();
    }
    if args.iter().any(|x| x == "--dry-run") {
        a = a.dry_run();
    }
    Arc::new(Mutex::new(a))
}

#[cfg(test)]
mod tests {
    use super::*;
    use yunxi_bot_core::tool::Capability;

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn all_ten_tools_register_without_name_clashes() {
        // 重名会被注册表拒绝，所以这条测试同时守着"名字没撞"
        let r = default_registry(&std::env::temp_dir()).expect("默认工具集应能注册");
        assert_eq!(r.len(), 10, "工具数变了要同步更新这个断言：{:?}", r.names());
        for n in [
            "now",
            "web_fetch",
            "web_search",
            "read_file",
            "list_dir",
            "search_files",
            "write_file",
            "edit_file",
            "run_command",
            "ask_user",
        ] {
            assert!(r.get(n).is_some(), "缺少工具 {n}");
        }
    }

    #[test]
    fn registry_order_is_stable() {
        // 工具清单进请求体，顺序不稳定会让前缀缓存作废
        let a = default_registry(&std::env::temp_dir()).unwrap().specs();
        let b = default_registry(&std::env::temp_dir()).unwrap().specs();
        assert_eq!(
            serde_json::to_string(&a).unwrap(),
            serde_json::to_string(&b).unwrap()
        );
    }

    #[test]
    fn only_read_and_network_tools_are_not_state_changing() {
        // 这条把"哪些工具会改状态"变成可检查的，而不是靠人记得
        let r = default_registry(&std::env::temp_dir()).unwrap();
        let mut outbound = 0;
        for (name, cap) in r.capabilities() {
            match name {
                "read_file" | "list_dir" | "search_files" | "now" | "ask_user" => {
                    assert_eq!(cap, Capability::ReadOnly, "{name} 应只读");
                }
                "write_file" | "edit_file" => assert_eq!(cap, Capability::Write, "{name}"),
                "run_command" => assert_eq!(cap, Capability::Execute, "{name}"),
                "web_fetch" | "web_search" => assert_eq!(cap, Capability::Network, "{name}"),
                other => panic!("多了一个工具没归类: {other}"),
            }
            if cap == Capability::Outbound {
                outbound += 1;
            }
        }
        // 目前没有对外发送类工具。将来加了（发邮件），要在这里显式承认它
        assert_eq!(outbound, 0, "新增不可逆工具要显式过一遍这条断言");
    }

    #[test]
    fn no_allow_flags_means_default_policy() {
        let p = policy_from_args(&args(&["do", "任务"]));
        assert!(p.allow.is_empty());
        assert!(p.deny.is_empty());
        assert!(
            p.inert_inside_cwd_is_free,
            "默认要给的是只读免问，不是放宽写操作"
        );
    }

    #[test]
    fn repeated_allow_flags_are_all_collected() {
        // 只看第一个的话，写了三条规则只有一条生效——而且是静默的
        let p = policy_from_args(&args(&[
            "--allow",
            "read_file:D:\\notes",
            "--allow",
            "run_command:git",
            "do",
            "任务",
        ]));
        assert_eq!(p.allow.len(), 2, "{:?}", p.allow);
        assert_eq!(p.allow[0].tool, "read_file");
        assert_eq!(p.allow[0].specifier.as_deref(), Some("D:\\notes"));
        assert_eq!(p.allow[1].tool, "run_command");
    }

    #[test]
    fn deny_flags_are_collected_too() {
        let p = policy_from_args(&args(&["--deny", "run_command:rm"]));
        assert_eq!(p.deny.len(), 1);
        assert_eq!(p.deny[0].tool, "run_command");
        assert_eq!(p.deny[0].specifier.as_deref(), Some("rm"));
    }

    #[test]
    fn a_flag_value_is_not_mistaken_for_a_flag_name() {
        // --allow 后面跟的值恰好叫 "--deny" 时不能把它当成另一个 flag
        let p = policy_from_args(&args(&["--allow", "--deny"]));
        assert_eq!(p.allow.len(), 1, "{:?}", p.allow);
        assert_eq!(p.allow[0].tool, "--deny");
        assert!(p.deny.is_empty());
    }

    #[test]
    fn approver_defaults_to_asking() {
        let a = approver_from_args(&args(&["do", "任务"]));
        let req = yunxi_bot_core::tool::ApprovalRequest {
            tool: "w".into(),
            capability: Capability::Write,
            specifier: None,
            arguments: "{}".into(),
            reason: "测试".into(),
        };
        // 非 --yes 且非 --dry-run 时它会去读 stdin；测试里 stdin 是空的，
        // 所以应当落到"读不到输入 → 拒绝"
        let ans = a.lock().unwrap().approve(&req);
        assert_eq!(ans, yunxi_bot_core::tool::Approval::Deny);
    }

    #[test]
    fn dry_run_approver_never_executes() {
        let a = approver_from_args(&args(&["--dry-run"]));
        let req = yunxi_bot_core::tool::ApprovalRequest {
            tool: "w".into(),
            capability: Capability::Write,
            specifier: None,
            arguments: "{}".into(),
            reason: "测试".into(),
        };
        assert_eq!(
            a.lock().unwrap().approve(&req),
            yunxi_bot_core::tool::Approval::Deny
        );
    }
}
