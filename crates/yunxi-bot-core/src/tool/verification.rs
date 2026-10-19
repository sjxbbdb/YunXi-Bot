//! 检查动作与同一对象的后置证据，不把工具成功提升为任意业务目标正确。
use super::{ToolCallRecord, ToolContext};

fn arguments(call: &ToolCallRecord) -> &Value {
    call.live_arguments.as_ref().unwrap_or(&call.arguments)
}
use serde_json::Value;
use std::path::Path;

// 当前轮完成检查只需目标与布尔回执，不能为它保留屏幕/网页正文或图片。
pub(super) fn computer_receipt(text: &str) -> Option<super::ToolOutput> {
    let value: Value = serde_json::from_str(text).ok()?;
    let source: serde_json::Map<String, Value> = ["pid", "window_id", "target_id", "tab_id"]
        .into_iter()
        .filter_map(|k| value["source"].get(k).map(|v| (k.to_string(), v.clone())))
        .collect();
    let checks: Vec<_> = value["checks"]
        .as_array()?
        .iter()
        .map(|c| serde_json::json!({"matched":c["matched"]}))
        .collect();
    Some(super::ToolOutput::read(
        serde_json::json!({"matched":value["matched"],"source":source,"checks":checks}).to_string(),
    ))
}

fn output(call: &ToolCallRecord) -> Option<&str> {
    if !matches!(call.decision, super::GateDecision::Allow { .. }) {
        return None;
    }
    call.live_output
        .as_ref()
        .map(|o| o.text.as_str())
        .or_else(|| call.output.as_ref()?.as_ref().ok().map(String::as_str))
}

pub fn completion(calls: &[ToolCallRecord], cwd: &Path) -> Result<(), String> {
    let ctx = ToolContext::new(cwd, crate::policy::SandboxMode::ReadOnly);
    let path = |args: &Value| {
        args["path"]
            .as_str()
            .and_then(|p| super::normalize_existing(&ctx.resolve(Path::new(p))))
    };
    for (index, call) in calls.iter().enumerate() {
        let later = &calls[index + 1..];
        let Some(text) = output(call) else {
            if matches!(call.decision, super::GateDecision::Allow { .. })
                && matches!(
                    call.tool.as_str(),
                    "write_file"
                        | "edit_file"
                        | "mac_agenda_apply"
                        | "mac_mail_send"
                        | "computer_act"
                )
            {
                // 文件后续成功写入仍需经过下方回读；其他未知副作用不能靠重放消除。
                let replaced = matches!(call.tool.as_str(), "write_file" | "edit_file")
                    && path(&call.arguments).is_some_and(|target| {
                        later.iter().any(|c| {
                            matches!(c.tool.as_str(), "write_file" | "edit_file")
                                && output(c).is_some()
                                && path(&c.arguments).as_ref() == Some(&target)
                        })
                    });
                if !replaced {
                    return Err("变更动作失败或回执缺失，不能沿用更早的成功证据；先检查当前状态，不能自动重放外部动作".into());
                }
            }
            continue;
        };
        match call.tool.as_str() {
            "write_file" | "edit_file" => {
                let target = path(&call.arguments).ok_or("写入对象无法解析，不能确认结果")?;
                // 只检查最后一次写入，较早的读取不能证明之后的状态。
                if later.iter().any(|c| {
                    matches!(c.tool.as_str(), "write_file" | "edit_file")
                        && output(c).is_some()
                        && path(&c.arguments).as_ref() == Some(&target)
                }) {
                    continue;
                }
                if !later.iter().any(|c| {
                    c.tool == "read_file"
                        && output(c).is_some()
                        && path(&c.arguments).as_ref() == Some(&target)
                }) {
                    return Err("文件已经写入，但缺少同一路径在最后写入之后的成功回读；不能确认任务完成，也不要重复写入".into());
                }
            }
            "mac_agenda_apply" => {
                let value: Value =
                    serde_json::from_str(text).map_err(|_| "日程结果不是有效回执")?;
                if value["applied"] != true
                    || !call.arguments["date"]
                        .as_str()
                        .is_some_and(|s| !s.is_empty())
                    || value["date"] != call.arguments["date"]
                    || value["ready_to_apply"] != true
                    || value["degraded"] == true
                    || !value["receipts"]
                        .as_array()
                        .is_some_and(|r| r.iter().all(|v| v["verified"] == true))
                {
                    return Err(
                        "日程写入未取得同一日期的完整核验回执，可能部分完成；需重新读取状态".into(),
                    );
                }
            }
            "mac_mail_send" => {
                let value: Value =
                    serde_json::from_str(text).map_err(|_| "邮件发送结果不是有效回执")?;
                if !call.arguments["draft_id"]
                    .as_str()
                    .is_some_and(|s| !s.is_empty())
                    || !call.arguments["expected_revision"]
                        .as_str()
                        .is_some_and(|s| !s.is_empty())
                    || value["draft_id"] != call.arguments["draft_id"]
                    || value["status"] != "handed_off"
                {
                    return Err("邮件提交结果无法对应已批准草稿；禁止自动重发".into());
                }
                let draft = calls[..index]
                    .iter()
                    .rev()
                    .find(|c| {
                        c.tool == "mac_mail_draft_get"
                            && c.arguments["draft_id"] == call.arguments["draft_id"]
                    })
                    .and_then(|c| output(c).and_then(|s| serde_json::from_str::<Value>(s).ok()));
                if !draft.is_some_and(|d| {
                    d["draft_id"] == call.arguments["draft_id"]
                        && d["revision"] == call.arguments["expected_revision"]
                        && d["to"].as_array().is_some_and(|to| !to.is_empty())
                }) {
                    return Err("Mail已接受提交，但本步骤缺少对应版本的收件人与草稿回读证据；不能宣称完整验收".into());
                }
                // handed_off 从不证明收件人收到，保留原生出口对附件/投递的限制。
                if value["degraded"] == true {
                    return Err(
                        "Mail已接受提交，但附件或投递仍有未核实项；禁止声称已送达或重发".into(),
                    );
                }
            }
            "computer_act" => {
                let target = arguments(call).get("arguments").unwrap_or(arguments(call));
                if later.iter().any(|c| {
                    c.tool == "computer_act" && output(c).is_some() && {
                        let next = arguments(c).get("arguments").unwrap_or(arguments(c));
                        ["pid", "window_id", "target_id", "tab_id"]
                            .iter()
                            .any(|k| target.get(*k).is_some())
                            && ["pid", "window_id", "target_id", "tab_id"]
                                .iter()
                                .all(|k| target.get(*k) == next.get(*k))
                    }
                }) {
                    continue;
                }
                let verified = later
                    .iter()
                    .filter(|c| c.tool == "computer_verify")
                    .filter_map(|c| output(c).and_then(|s| serde_json::from_str::<Value>(s).ok()))
                    .any(|v| {
                        let keys: Vec<_> = ["pid", "window_id", "target_id", "tab_id"]
                            .into_iter()
                            .filter(|k| target.get(*k).is_some())
                            .collect();
                        !keys.is_empty()
                            && v["matched"] == true
                            && keys.iter().all(|k| v["source"][*k] == target[*k])
                            && v["checks"].as_array().is_some_and(|checks| {
                                !checks.is_empty() && checks.iter().all(|c| c["matched"] == true)
                            })
                    });
                if !verified {
                    return Err("最后一次电脑动作缺少同一窗口/标签页的后置核验；动作可能已生效，先观察而非重放".into());
                }
            }
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::{Capability, GateDecision};
    use serde_json::json;
    fn call(tool: &str, args: Value, output: &str) -> ToolCallRecord {
        ToolCallRecord {
            live_arguments: None,
            tool: tool.into(),
            arguments: args,
            capability: Capability::ReadOnly,
            decision: GateDecision::Allow {
                reason: "test".into(),
            },
            output: Some(Ok(output.into())),
            started_at_ms: 1,
            duration_ms: 1,
            live_output: None,
        }
    }
    #[test]
    fn only_readback_after_last_write_to_the_same_file_counts() {
        let cwd = std::env::temp_dir();
        let write = call("write_file", json!({"path":"新核对.txt"}), "ok");
        let read = call("read_file", json!({"path":"新核对.txt"}), "12");
        assert!(completion(std::slice::from_ref(&write), &cwd).is_err());
        assert!(completion(&[read.clone(), write.clone()], &cwd).is_err());
        let wrong = call("read_file", json!({"path":"核对.txt"}), "12");
        assert!(completion(&[write.clone(), wrong], &cwd).is_err());
        assert!(completion(&[write.clone(), read.clone(), write.clone()], &cwd).is_err());
        assert!(completion(&[write, read], &cwd).is_ok());
    }
    #[test]
    fn agenda_and_send_receipts_must_identify_the_approved_object_and_state() {
        let cwd = std::env::temp_dir();
        let agenda = call(
            "mac_agenda_apply",
            json!({"date":"2026-10-20"}),
            r#"{"applied":false,"date":"2026-10-20"}"#,
        );
        assert!(completion(&[agenda], &cwd).is_err());
        let sent = call(
            "mac_mail_send",
            json!({"draft_id":"a","expected_revision":"r"}),
            r#"{"draft_id":"b","status":"handed_off"}"#,
        );
        assert!(completion(&[sent], &cwd).is_err());
    }
    #[test]
    fn browser_click_is_not_proven_by_an_old_or_unrelated_observation() {
        let cwd = std::env::temp_dir();
        let act = call(
            "computer_act",
            json!({"target_id":"browser","tab_id":"a"}),
            "{}",
        );
        assert!(completion(std::slice::from_ref(&act), &cwd).is_err());
        let verify = call(
            "computer_verify",
            json!({"target_id":"browser","tab_id":"b"}),
            r#"{"matched":true}"#,
        );
        assert!(completion(&[act, verify], &cwd).is_err());
    }

    #[test]
    fn private_browser_targets_are_checked_in_memory_without_being_archived() {
        let cwd = std::env::temp_dir();
        let mut act = call("computer_act", serde_json::json!({}), "{}");
        act.live_arguments =
            Some(serde_json::json!({"arguments":{"target_id":"private-target","tab_id":"a"}}));
        let verify = call(
            "computer_verify",
            serde_json::json!({}),
            r#"{"matched":true,"source":{"target_id":"private-target","tab_id":"a"},"checks":[{"matched":true}]}"#,
        );
        assert!(completion(&[act.clone(), verify], &cwd).is_ok());
        assert!(
            !serde_json::to_string(&act)
                .unwrap()
                .contains("private-target")
        );
        assert!(!format!("{act:?}").contains("private-target"));
    }

    #[test]
    fn same_date_agenda_receipt_and_revision_bound_mail_submission_are_accepted() {
        let cwd = std::env::temp_dir();
        let agenda = call(
            "mac_agenda_apply",
            serde_json::json!({"date":"2026-10-20"}),
            r#"{"applied":true,"ready_to_apply":true,"date":"2026-10-20","receipts":[{"verified":true}]}"#,
        );
        assert!(completion(&[agenda], &cwd).is_ok());
        let draft = call(
            "mac_mail_draft_get",
            serde_json::json!({"draft_id":"a"}),
            r#"{"draft_id":"a","revision":"r","to":["test@example.com"]}"#,
        );
        let sent = call(
            "mac_mail_send",
            serde_json::json!({"draft_id":"a","expected_revision":"r"}),
            r#"{"draft_id":"a","status":"handed_off","delivered":false,"degraded":false}"#,
        );
        assert!(completion(&[draft.clone(), sent.clone()], &cwd).is_ok());
        let mut stale = draft;
        stale.output = Some(Ok(r#"{"revision":"old","to":["test@example.com"]}"#.into()));
        assert!(completion(&[stale, sent], &cwd).is_err());
    }

    #[test]
    fn a_failed_last_mutation_cannot_reuse_an_earlier_successful_readback() {
        let cwd = std::env::temp_dir();
        let write = call("write_file", json!({"path":"a.txt"}), "ok");
        let read = call("read_file", json!({"path":"a.txt"}), "old contents");
        let mut failed = write.clone();
        failed.output = Some(Err(crate::tool::ToolError::Failed {
            detail: "write may have partially completed".into(),
        }));
        assert!(completion(&[write, read, failed], &cwd).is_err());
    }

    #[test]
    fn missing_object_identity_is_not_a_matching_receipt() {
        let cwd = std::env::temp_dir();
        let agenda = call(
            "mac_agenda_apply",
            json!({}),
            r#"{"applied":true,"ready_to_apply":true,"receipts":[]}"#,
        );
        assert!(completion(&[agenda], &cwd).is_err());
        let draft = call(
            "mac_mail_draft_get",
            json!({}),
            r#"{"to":["test@example.com"]}"#,
        );
        let send = call("mac_mail_send", json!({}), r#"{"status":"handed_off"}"#);
        assert!(completion(&[draft, send], &cwd).is_err());
    }
}
