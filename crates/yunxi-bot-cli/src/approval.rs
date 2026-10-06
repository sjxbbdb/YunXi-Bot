//! 命令行侧的审批交互与工具装配。
//!
//! ## 审批提示是使用者和 Agent 之间最重要的一个界面
//!
//! 它会问几百次，所以每一行都必须有用：
//!
//! - **必须显示参数原文。** 不看参数就批准等于没批准——`run_command` 的
//!   `["git","push"]` 和 `["git","push","--force"]` 是两个完全不同的决定。
//! - **必须说明为什么要问。** "写入能力，无可归类粒度" 和 "命中拒绝规则"
//!   是两回事，前者可以放心批准，后者不该被绕过。
//! - **必须给出"总是允许"的粒度。** 使用者想表达的是"总是允许读这个目录"，
//!   不是"总是允许这个工具的一切"。所以要显示这条规则会记住什么。
//!
//! ## 默认答案是否
//!
//! 直接回车 = 拒绝。审批提示的默认值必须是"不执行"——**手滑敲回车不该
//! 变成一个不可逆动作**。这是整个工具层"失败方向朝不执行"的最后一个落点。

use std::io::{BufRead, Write};

use yunxi_bot_core::tool::{Approval, ApprovalRequest, Approver};

/// 从标准输入读审批答案。
pub struct StdinApprover {
    /// 不问就批。给自动化用，**默认关闭**。
    pub assume_yes: bool,
    /// 一次性把所有要问的都记录下来，不真问（干跑用）。
    pub dry_run: bool,
}

impl StdinApprover {
    pub fn new() -> Self {
        Self {
            assume_yes: false,
            dry_run: false,
        }
    }

    pub fn assume_yes(mut self) -> Self {
        self.assume_yes = true;
        self
    }

    pub fn dry_run(mut self) -> Self {
        self.dry_run = true;
        self
    }
}

impl Default for StdinApprover {
    fn default() -> Self {
        Self::new()
    }
}

impl Approver for StdinApprover {
    fn approve(&mut self, req: &ApprovalRequest) -> Approval {
        if self.assume_yes {
            eprintln!("· 自动批准（--yes）：{}", req.tool);
            return Approval::Once;
        }
        if self.dry_run {
            // 干跑：不真问，也不假装批准——**报成拒绝**，因为它确实没被执行
            eprintln!("· 干跑：{} 需要审批，未执行", req.tool);
            return Approval::Deny;
        }

        println!();
        println!("┌─ 需要你确认 ─────────────────────────────");
        println!("│ 工具      : {}（{}）", req.tool, req.capability.label());
        if let Some(s) = &req.specifier {
            println!("│ 动的是    : {s}");
        }
        // **先给后果，再给参数。**
        //
        // 顺序是有意的：人读审批框是从上往下的，读到能判断"批不批"
        // 就该停下了。参数 JSON 是给"想再核对一眼"用的，不是判决依据。
        //
        // 而这一段**可能没有**——读取类工具本来就没有"后果"。
        // 那时如实说"没提供预览"，而不是让人以为下面那串 JSON 就是后果。
        match &req.preview {
            Some(p) => {
                println!("│ 会变成    :");
                for line in p.lines() {
                    println!("│   {line}");
                }
            }
            None => {
                println!("│ 会变成    : （这个工具没提供预览，请直接看下面的参数）");
            }
        }
        println!("│ 参数      : {}", req.arguments);
        println!("│ 为什么问  : {}", req.reason);
        if req.capability.is_irreversible() {
            println!("│ ⚠ 不可逆动作：这次批准不会变成长期规则");
        }
        println!("└──────────────────────────────────────────");
        print!("批准吗？ [y]本次 / [a]总是允许 / [n]拒绝（直接回车=拒绝）: ");
        let _ = std::io::stdout().flush();

        let mut line = String::new();
        if std::io::stdin().lock().read_line(&mut line).is_err() {
            // 读不到输入（管道关了、非交互）→ 拒绝。
            // 没人应答必须等于不执行。
            eprintln!("（读不到输入，按拒绝处理）");
            return Approval::Deny;
        }
        match line.trim().to_ascii_lowercase().as_str() {
            "y" | "yes" => Approval::Once,
            "a" | "always" => Approval::Always,
            // 包括空行。空行 = 拒绝是有意的：回车是手最容易碰到的键。
            _ => Approval::Deny,
        }
    }
}

/// 解析 `--allow <工具[:粒度]>` / `--deny <工具[:粒度]>`。
///
/// 语法和 Claude Code 的 `ToolName(specifier)` 一个意思，只是写成命令行
/// 更顺手的 `工具:粒度`。
pub fn parse_rule(s: &str) -> (String, Option<String>) {
    match s.split_once(':') {
        Some((tool, spec)) if !spec.is_empty() => (tool.to_string(), Some(spec.to_string())),
        Some((tool, _)) => (tool.to_string(), None),
        None => (s.to_string(), None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use yunxi_bot_core::tool::Capability;

    fn req(tool: &str, cap: Capability) -> ApprovalRequest {
        ApprovalRequest {
            tool: tool.into(),
            capability: cap,
            specifier: Some("D:\\notes".into()),
            arguments: "{\"path\":\"a.md\"}".into(),
            reason: "写入能力".into(),
            preview: None,
        }
    }

    #[test]
    fn assume_yes_approves_once_not_always() {
        // 自动化场景下也不该写长期规则——那是使用者的决定，不是脚本的
        let mut a = StdinApprover::new().assume_yes();
        assert_eq!(a.approve(&req("w", Capability::Write)), Approval::Once);
    }

    #[test]
    fn dry_run_denies_because_nothing_is_executed() {
        // 干跑报成"批准"会让预览与真实运行不一致
        let mut a = StdinApprover::new().dry_run();
        assert_eq!(a.approve(&req("w", Capability::Write)), Approval::Deny);
    }

    #[test]
    fn rule_syntax_splits_on_the_first_colon_only() {
        // Windows 路径里有冒号，所以只能按**第一个**冒号切，剩下的全是粒度
        assert_eq!(
            parse_rule("read_file:D:\\notes"),
            ("read_file".to_string(), Some("D:\\notes".to_string()))
        );
        assert_eq!(parse_rule("read_file"), ("read_file".to_string(), None));
        assert_eq!(
            parse_rule("web_fetch:https://docs.rs"),
            ("web_fetch".to_string(), Some("https://docs.rs".to_string()))
        );
        // 冒号后为空 → 当作没有粒度
        assert_eq!(parse_rule("read_file:"), ("read_file".to_string(), None));
    }

    #[test]
    fn irreversible_is_flagged_to_the_user() {
        // 这条提示是使用者判断"要不要按 a"的唯一依据
        assert!(Capability::Outbound.is_irreversible());
        assert!(!Capability::Write.is_irreversible());
    }
}
