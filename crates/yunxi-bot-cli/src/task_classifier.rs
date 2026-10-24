//! 输入分类：聊天还是任务。
//!
//! 只负责“这句话该走哪个代理”，不负责选择具体模型。分类由本地 Laya
//! 决策模型完成；分类失败时**默认聊天**（本地、较保守），不偷偷上云。
//! 显式 `/task`、`/work`、`/chat` 优先于模型判断。

use std::path::PathBuf;
use std::sync::Arc;

use yunxi_bot_core::Ledger;
use yunxi_bot_core::decide::Decider;
use yunxi_bot_core::think::ModelRouter;

use yunxi_bot_core::task::TaskError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputKind {
    Chat,
    Task,
}

#[derive(Debug, Clone)]
pub struct Classification {
    pub kind: InputKind,
    pub reason: String,
    pub model: Option<String>,
}

pub struct TaskClassifier {
    decider: Arc<dyn Decider>,
    ledger_path: PathBuf,
}

impl TaskClassifier {
    pub fn new(decider: Arc<dyn Decider>, ledger_path: PathBuf) -> Self {
        Self {
            decider,
            ledger_path,
        }
    }

    /// 返回分类并写决策台账。`/task` 或 `/work` 前缀会被调用方剥掉。
    pub fn classify(&self, text: &str) -> Result<Classification, TaskError> {
        let trimmed = text.trim_start();
        if command_matches(trimmed, "/task") || command_matches(trimmed, "/work") {
            self.record(true, "显式工作指令", None)?;
            return Ok(Classification {
                kind: InputKind::Task,
                reason: "显式工作指令".into(),
                model: None,
            });
        }
        if command_matches(trimmed, "/chat") {
            self.record(false, "显式聊天指令", None)?;
            return Ok(Classification {
                kind: InputKind::Chat,
                reason: "显式聊天指令".into(),
                model: None,
            });
        }

        let router = ModelRouter::default();
        let verdict = router.classify_input(&*self.decider, text);
        let (kind, reason, model) = match verdict {
            Some((true, model)) => (
                InputKind::Task,
                "决策模型判为任务，交给云端工作代理".to_string(),
                Some(model),
            ),
            Some((false, model)) => (
                InputKind::Chat,
                "决策模型建议聊天，走本机陪伴代理".to_string(),
                Some(model),
            ),
            None => (
                InputKind::Chat,
                "决策模型弃权，默认本机聊天".to_string(),
                None,
            ),
        };
        self.record(kind == InputKind::Task, &reason, model.as_deref())?;
        Ok(Classification {
            kind,
            reason,
            model,
        })
    }

    fn record(&self, task: bool, reason: &str, model: Option<&str>) -> Result<(), TaskError> {
        let mut ledger = Ledger::open(&self.ledger_path).map_err(|e| TaskError::NeedsHuman {
            reason: format!("输入分类台账不可用: {e}"),
        })?;
        let action = if task { "task" } else { "chat" };
        yunxi_bot_core::decide::record_decision(
            &mut ledger,
            yunxi_bot_core::decide::DecisionClass::Classify,
            task,
            action,
            reason,
            model,
            &["input_kind".to_string()],
        )
        .map_err(|e| TaskError::NeedsHuman {
            reason: format!("输入分类无法留痕: {e}"),
        })?;
        Ok(())
    }
}

fn command_matches(text: &str, command: &str) -> bool {
    text == command
        || text
            .strip_prefix(command)
            .is_some_and(|rest| rest.starts_with(char::is_whitespace))
}

/// 去掉显式指令前缀，只把正文交给模型。
pub fn strip_command(text: &str) -> String {
    let trimmed = text.trim_start();
    for command in ["/task", "/work", "/chat"] {
        if command_matches(trimmed, command) {
            return trimmed[command.len()..].trim_start().to_string();
        }
    }
    text.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use yunxi_bot_core::decide::StubDecider;

    fn classifier(choice: Option<&str>) -> (TaskClassifier, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "yunxi-classifier-{}-{}",
            std::process::id(),
            choice.unwrap_or("none")
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("ledger.jsonl");
        let mut stub = StubDecider::succeeding();
        if let Some(choice) = choice {
            stub = stub.with_choice("input_kind", choice);
        }
        (TaskClassifier::new(Arc::new(stub), path), dir)
    }

    #[test]
    fn explicit_task_beats_model() {
        let (c, dir) = classifier(Some("chat"));
        let out = c.classify("/task 把这份邮件总结一下").unwrap();
        assert_eq!(out.kind, InputKind::Task);
        assert_eq!(
            strip_command("/task 把这份邮件总结一下"),
            "把这份邮件总结一下"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn model_task_routes_to_cloud() {
        let (c, dir) = classifier(Some("task"));
        let out = c.classify("帮我把这个文件改成 UTF-8").unwrap();
        assert_eq!(out.kind, InputKind::Task);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn unknown_defaults_to_local_chat() {
        let (c, dir) = classifier(None);
        let out = c.classify("在吗").unwrap();
        assert_eq!(out.kind, InputKind::Chat);
        let _ = std::fs::remove_dir_all(dir);
    }
}
