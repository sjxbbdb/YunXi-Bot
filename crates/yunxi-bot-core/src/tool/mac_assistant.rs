//! 第二批 Mac 能力共用原生传输与审批，工具参数只维护一份目录。

use std::path::Path;

use serde::Deserialize;
use serde_json::{Value, json};

use super::{Capability, Tool, ToolContext, ToolError, ToolOutput, macos};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AssistantTool {
    name: String,
    capability: Capability,
    description: String,
    parameters: Value,
}

fn bad(detail: impl Into<String>) -> ToolError {
    ToolError::BadArgs {
        detail: detail.into(),
    }
}

pub fn tools() -> Result<Vec<AssistantTool>, ToolError> {
    serde_json::from_str(include_str!(
        "../../../../native/macos/assistant-tools.json"
    ))
    .map_err(|_| bad("Mac 助理工具目录无法解析"))
}

// 仅支持此目录实际使用的类型；未知类型拒绝，不能把缺失校验当作放行。
fn validate(value: &Value, schema: &Value, field: &str) -> Result<(), ToolError> {
    if let Some(choices) = schema["enum"].as_array()
        && !choices.contains(value)
    {
        return Err(bad(format!("{field} 不在允许的选项内")));
    }
    let valid = match schema["type"].as_str() {
        Some("object") => {
            let object = value
                .as_object()
                .ok_or_else(|| bad(format!("{field} 必须是对象")))?;
            let properties = schema["properties"]
                .as_object()
                .ok_or_else(|| bad("工具目录缺少字段定义"))?;
            for required in schema["required"].as_array().into_iter().flatten() {
                if !required
                    .as_str()
                    .is_some_and(|key| object.contains_key(key))
                {
                    return Err(bad(format!("{field} 缺少必填字段 {required}")));
                }
            }
            for (key, item) in object {
                let property = properties
                    .get(key)
                    .ok_or_else(|| bad(format!("{field} 不接受字段 {key}")))?;
                validate(item, property, &format!("{field}.{key}"))?;
            }
            true
        }
        Some("array") => {
            let array = value
                .as_array()
                .ok_or_else(|| bad(format!("{field} 必须是数组")))?;
            for item in array {
                validate(item, &schema["items"], field)?;
            }
            array.len() as u64 <= schema["maxItems"].as_u64().unwrap_or(0)
        }
        Some("string") => value.as_str().is_some_and(|s| {
            !s.contains('\0')
                && s.len() as u64 <= schema["maxLength"].as_u64().unwrap_or(200)
                && (schema["minLength"].as_u64().unwrap_or(0) == 0 || !s.trim().is_empty())
        }),
        Some("boolean") => value.is_boolean(),
        Some("integer" | "number") => value.as_f64().is_some_and(|n| {
            n.is_finite()
                && (schema["type"] != "integer" || value.is_i64() || value.is_u64())
                && n >= schema["minimum"].as_f64().unwrap_or(f64::INFINITY)
                && n <= schema["maximum"].as_f64().unwrap_or(f64::NEG_INFINITY)
        }),
        _ => false,
    };
    if valid {
        Ok(())
    } else {
        Err(bad(format!("{field} 类型或取值范围不合法")))
    }
}

impl AssistantTool {
    fn resolved(&self, args: &Value, ctx: &ToolContext) -> Result<Value, ToolError> {
        validate(args, &self.parameters, "参数")?;
        let mut args = args.clone();
        if let Some(path) = args["path"].as_str() {
            args["path"] = json!(ctx.resolve(Path::new(path)).to_string_lossy());
        }
        if let Some(attachments) = args.get_mut("attachments").and_then(Value::as_array_mut) {
            for item in attachments {
                let path = item.as_str().ok_or_else(|| bad("附件路径必须是文本"))?;
                *item = json!(ctx.resolve(Path::new(path)).to_string_lossy());
            }
        }
        Ok(args)
    }
}

impl Tool for AssistantTool {
    fn name(&self) -> &str {
        &self.name
    }
    fn description(&self) -> &str {
        &self.description
    }
    fn parameters(&self) -> Value {
        self.parameters.clone()
    }
    fn capability(&self) -> Capability {
        self.capability
    }
    fn specifier(&self, args: &Value, ctx: &ToolContext) -> Option<String> {
        args["path"]
            .as_str()
            .map(|s| ctx.resolve(Path::new(s)).to_string_lossy().into_owned())
    }
    fn preview(&self, args: &Value, ctx: &ToolContext) -> Option<String> {
        if self.capability != Capability::Outbound {
            return None;
        }
        let args = match self.resolved(args, ctx) {
            Ok(args) => args,
            Err(error) => return Some(format!("参数无效，不能执行：{error}")),
        };
        let preview = if self.name == "mac_mail_send" {
            match macos::request(
                &macos::bundled_app(),
                "mac_mail_draft_get",
                &json!({"draft_id":args["draft_id"]}),
            ) {
                Ok(draft) if draft["revision"] == args["expected_revision"] => {
                    format!("将向以下真实地址发送邮件（此动作不可撤回）：\n{draft}")
                }
                _ => "草稿无法核实或已变化，本次发送将被拒绝；请先重新读取。".into(),
            }
        } else {
            format!("Mac 操作 {}：\n{args}", self.name)
        };
        Some(
            preview
                .chars()
                .filter(|c| !c.is_control() || *c == '\n' || *c == '\t')
                .collect(),
        )
    }
    fn call(&self, args: &Value, ctx: &mut ToolContext) -> Result<ToolOutput, ToolError> {
        let args = self.resolved(args, ctx)?;
        let result = macos::request(&macos::bundled_app(), &self.name, &args)?;
        if self.name == "mac_document_read"
            && let Some(path) = args["path"].as_str()
        {
            ctx.note_read(Path::new(path));
        }
        Ok(if self.capability == Capability::Outbound {
            ToolOutput::changed(result.to_string())
        } else {
            ToolOutput::read(result.to_string())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        policy::SandboxMode,
        tool::{GateDecision, ToolPolicy, gate},
    };

    #[test]
    fn malformed_native_arguments_fail_before_any_system_access() {
        let tools = tools().unwrap();
        let mut ctx = ToolContext::new(std::env::temp_dir(), SandboxMode::ReadOnly);
        for tool in &tools {
            assert!(
                tool.call(&json!({"execute":"arbitrary code"}), &mut ctx)
                    .is_err()
            );
        }
        let weather = tools.iter().find(|t| t.name() == "mac_weather").unwrap();
        for latitude in [json!(true), json!(91), json!("31"), Value::Null] {
            assert!(
                weather
                    .call(
                        &json!({"latitude":latitude,"longitude":121,"date":"2026-10-16"}),
                        &mut ctx
                    )
                    .is_err()
            );
        }
        let draft = tools.iter().find(|t| t.name() == "mac_mail_draft").unwrap();
        assert!(draft.call(&json!({"mode":"send","sender":"a@example.com","to":[],"subject":"x","body":"x","request_id":"test-1234"}), &mut ctx).is_err());
    }

    #[test]
    fn mail_changes_always_need_human_approval_without_an_explicit_allow() {
        let ctx = ToolContext::new(std::env::temp_dir(), SandboxMode::ReadOnly);
        for tool in tools()
            .unwrap()
            .into_iter()
            .filter(|t| t.capability == Capability::Outbound)
        {
            assert!(matches!(
                gate(&tool, &json!({}), &ToolPolicy::default(), &ctx, None),
                GateDecision::Ask { .. }
            ));
        }
    }

    #[test]
    fn document_scope_and_resolved_path_are_the_same() {
        let ctx = ToolContext::new(std::env::temp_dir(), SandboxMode::ReadOnly);
        let tool = tools()
            .unwrap()
            .into_iter()
            .find(|t| t.name() == "mac_document_read")
            .unwrap();
        let args = json!({"path":"attachment.pdf"});
        assert_eq!(
            Some(
                tool.resolved(&args, &ctx).unwrap()["path"]
                    .as_str()
                    .unwrap()
                    .to_owned()
            ),
            tool.specifier(&args, &ctx)
        );
    }

    #[test]
    fn resolving_arguments_does_not_insert_an_absent_attachments_field() {
        let ctx = ToolContext::new(std::env::temp_dir(), SandboxMode::ReadOnly);
        let tool = tools()
            .unwrap()
            .into_iter()
            .find(|t| t.name() == "mac_contacts_search")
            .unwrap();
        let args = json!({"query":"张"});
        assert_eq!(tool.resolved(&args, &ctx).unwrap(), args);
    }

    #[test]
    fn replacement_attachments_resolve_paths_without_weakening_approval() {
        let ctx = ToolContext::new(std::env::temp_dir(), SandboxMode::ReadOnly);
        let tools = tools().unwrap();
        let update = tools
            .iter()
            .find(|t| t.name() == "mac_mail_draft_update")
            .unwrap();
        let args =
            json!({"draft_id":"draft-1234","expected_revision":"v1","attachments":["new.txt"]});
        let resolved = update.resolved(&args, &ctx).unwrap();
        assert_eq!(
            resolved["attachments"][0],
            ctx.resolve(Path::new("new.txt")).to_string_lossy().as_ref()
        );
        assert!(matches!(
            gate(update, &args, &ToolPolicy::default(), &ctx, None),
            GateDecision::Ask { .. }
        ));
        assert!(
            update
                .resolved(
                    &json!({"draft_id":"draft-1234","expected_revision":"v1","attachments":[]}),
                    &ctx
                )
                .is_ok()
        );
        assert!(update.resolved(&json!({"draft_id":"draft-1234","expected_revision":"v1","attachments":vec!["file"; 11]}), &ctx).is_err());
        let outbox = tools
            .iter()
            .find(|t| t.name() == "mac_mail_outbox")
            .unwrap();
        assert_eq!(outbox.capability(), Capability::ReadOnly);
        assert!(
            outbox
                .resolved(&json!({"status":"delivered"}), &ctx)
                .is_err()
        );
    }
}
