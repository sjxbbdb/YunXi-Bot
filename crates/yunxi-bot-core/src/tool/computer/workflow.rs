//! Generic observations and postconditions; business receipts stay in their own tools.
use super::{Frame, Result, ToolOutput, bad, page_url};
use crate::tool::ToolObservation;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::hash::{Hash, Hasher};
use std::time::{Duration, Instant};

pub(super) const GUIDE: &str = include_str!("../../prompts/computer_use.md");

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct WaitFor {
    timeout_ms: u64,
    checks: Vec<Check>,
}

pub(super) fn wait_parameters() -> Value {
    let mut checks = parameters()["properties"]["checks"].clone();
    if let Some(choices) = checks["items"]["oneOf"].as_array_mut() {
        choices.truncate(2);
    }
    json!({"type":"object","description":"仅同一精确网页/窗口的只读条件等待，最多8次观察；timeout_ms是轮询预算，单次Driver请求仍受其自身超时约束。弹窗或读取错误立即停止，不重试动作。", "properties":{"timeout_ms":{"type":"integer","minimum":1,"maximum":10000},"checks":checks},"required":["timeout_ms","checks"],"additionalProperties":false})
}

pub(super) fn wait_request(args: &Value) -> Result<(WaitFor, Value)> {
    if args.to_string().len() > 64_000 {
        return Err(bad("观察请求过大"));
    }
    let wait: WaitFor = serde_json::from_value(args["wait_for"].clone())
        .map_err(|_| bad("wait_for仅接受timeout_ms与正向文字/URL checks"))?;
    if !(1..=10000).contains(&wait.timeout_ms)
        || wait.checks.is_empty()
        || wait.checks.len() > 8
        || wait.checks.iter().any(|check| match check {
            Check::TextPresent { text } => !valid_text(text),
            Check::UrlIs { url } => !valid_text(url),
            Check::ElementState { .. } => true,
        })
    {
        return Err(bad(
            "wait_for须为1–10000ms及1–8条文字/URL条件，不接受旧控件引用",
        ));
    }
    let mut poll = args.clone();
    poll.as_object_mut()
        .ok_or_else(|| bad("观察参数必须为object"))?
        .remove("wait_for");
    let input = super::input(&poll)?;
    super::validate(&input, &super::catalog()[&input.operation], true)?;
    let exact = match input.operation.as_str() {
        "get_browser_state" => ["target_id", "tab_id"]
            .iter()
            .all(|key| input.arguments[*key].as_str().is_some_and(valid_text)),
        "get_window_state" => ["pid", "window_id"]
            .iter()
            .all(|key| input.arguments[*key].as_u64().is_some_and(|n| n > 0)),
        _ => false,
    };
    if !exact || input.arguments.get("continuation").is_some() {
        return Err(bad(
            "wait_for仅用于固定target_id/tab_id或pid/window_id的当前快照，不用于列表、分页或动作",
        ));
    }
    Ok((wait, poll))
}

impl WaitFor {
    pub(super) fn verification(&self, id: &str) -> Value {
        json!({"observation_id":id,"checks":self.checks})
    }

    pub(super) fn run(
        &self,
        mut observe: impl FnMut(Duration) -> Result<(ToolOutput, bool, bool)>,
    ) -> Result<ToolOutput> {
        let start = Instant::now();
        let budget = Duration::from_millis(self.timeout_ms);
        for attempt in 1..=8 {
            let (mut output, matched, blocked) = observe(budget.saturating_sub(start.elapsed()))?;
            if !blocked && !matched && attempt < 8 && start.elapsed() < budget {
                std::thread::sleep(
                    Duration::from_millis(200).min(budget.saturating_sub(start.elapsed())),
                );
            }
            let expired = start.elapsed() >= budget || attempt == 8;
            if blocked || matched || expired {
                let status = if blocked {
                    "interrupted_by_dialog"
                } else if matched {
                    "matched"
                } else {
                    "condition_unmet"
                };
                output.text.push_str(&format!("\n只读等待回执：{}", json!({"wait_status":status,"matched":matched && !blocked,"degraded":!matched || blocked,"observations":attempt,"task_completion":"not_assessed","note":"仅当前观察；条件未满足不证明先前动作失败，不重放写动作。"})));
                return Ok(output);
            }
        }
        Err(bad("等待未得到观察结果"))
    }
}

fn digest(value: &Value) -> u64 {
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    value.to_string().hash(&mut hash);
    hash.finish()
}

fn nodes(frame: &Frame) -> impl Iterator<Item = &Value> {
    ["refs", "content_refs", "elements"]
        .into_iter()
        .flat_map(|key| frame.arguments[key].as_array().into_iter().flatten())
}

pub(super) fn observation(frame: &Frame) -> ToolObservation {
    let data = &frame.arguments;
    let scope = digest(&json!({
        "pid":data["pid"], "window_id":data["window_id"],
        "target_id":data["target_id"], "tab_id":data["tab_id"],
        "query":data["query"],"scope_ref":data["scope_ref"],
        "continuation":data["continuation"],"scope":data["snapshot"]["scope"]
    }));
    // Ephemeral ref/capture IDs must not make an unchanged page look like progress.
    let semantic: Vec<Value> = nodes(frame)
        .map(|node| {
            let mut out = serde_json::Map::new();
            for key in [
                "role", "name", "label", "value", "checked", "selected", "enabled", "disabled",
                "expanded",
            ] {
                if let Some(value) = node.get(key) {
                    out.insert(key.into(), value.clone());
                }
            }
            Value::Object(out)
        })
        .collect();
    let tree = data["tree_markdown"].as_str().unwrap_or("");
    let state = (!semantic.is_empty() || !tree.is_empty()).then(|| {
        digest(&json!({
            "url":page_url(frame),"title":data["page"]["title"],"nodes":semantic,"tree":tree,
            "complete":data["snapshot"]["complete"]
        }))
    });
    ToolObservation { scope, state }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Verify {
    observation_id: String,
    checks: Vec<Check>,
}

#[derive(Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Check {
    TextPresent {
        text: String,
    },
    UrlIs {
        url: String,
    },
    ElementState {
        r#ref: String,
        field: String,
        value: Value,
    },
}

pub(super) fn parameters() -> Value {
    json!({"type":"object","properties":{
        "observation_id":{"type":"string"},
        "checks":{"type":"array","minItems":1,"maxItems":8,"items":{"oneOf":[
            {"type":"object","properties":{"kind":{"const":"text_present"},"text":{"type":"string","minLength":1,"maxLength":1000}},"required":["kind","text"],"additionalProperties":false},
            {"type":"object","properties":{"kind":{"const":"url_is"},"url":{"type":"string","minLength":1,"maxLength":1000}},"required":["kind","url"],"additionalProperties":false},
            {"type":"object","properties":{"kind":{"const":"element_state"},"ref":{"type":"string","description":"浏览器ref或原生element_token；原生字段需tree_format=both/elements返回实际elements"},"field":{"enum":["name","label","value","checked","selected","enabled","disabled","expanded"]},"value":{"type":["string","boolean"]}},"required":["kind","ref","field","value"],"additionalProperties":false}
        ]}}
    },"required":["observation_id","checks"],"additionalProperties":false})
}

fn valid_text(value: &str) -> bool {
    !value.trim().is_empty() && value.chars().count() <= 1000 && !value.contains('\0')
}

pub(super) fn verify(args: &Value, frame: Option<&Frame>, epoch: &str) -> Result<Value> {
    if args.to_string().len() > 40_000 {
        return Err(bad("核验参数过大"));
    }
    let request: Verify = serde_json::from_value(args.clone())
        .map_err(|_| bad("核验参数无效；只接受当前观察与1–8项明确条件"))?;
    if request.checks.is_empty() || request.checks.len() > 8 {
        return Err(bad("核验条件须为1–8项"));
    }
    let frame = frame.ok_or_else(|| bad("先重新观察，再核验结果"))?;
    if request.observation_id != frame.id
        || epoch != frame.id
        || frame.at.elapsed() > Duration::from_secs(120)
    {
        return Err(bad(
            "stale_observation：核验只接受同一最新实际观察，先重新读取",
        ));
    }
    let mut outcomes = Vec::new();
    for (index, check) in request.checks.iter().enumerate() {
        let (matched, available) = match check {
            Check::TextPresent { text } => {
                if !valid_text(text) {
                    return Err(bad("核验文字为空、过长或含NUL"));
                }
                let matched = nodes(frame).any(|node| {
                    ["name", "label", "value"]
                        .iter()
                        .any(|key| node[*key].as_str().is_some_and(|s| s.contains(text)))
                }) || frame.arguments["tree_markdown"]
                    .as_str()
                    .is_some_and(|s| s.contains(text));
                (
                    matched,
                    nodes(frame).next().is_some() || frame.arguments["tree_markdown"].is_string(),
                )
            }
            Check::UrlIs { url } => {
                if !valid_text(url) {
                    return Err(bad("核验URL为空或过长"));
                }
                let actual = page_url(frame);
                (actual == url, !actual.is_empty())
            }
            Check::ElementState {
                r#ref,
                field,
                value,
            } => {
                if !valid_text(r#ref)
                    || ![
                        "name", "label", "value", "checked", "selected", "enabled", "disabled",
                        "expanded",
                    ]
                    .contains(&field.as_str())
                    || !(value.is_boolean() || value.as_str().is_some_and(valid_text))
                {
                    return Err(bad("控件核验只接受实际ref及已声明的文字/布尔字段"));
                }
                let matches: Vec<_> = nodes(frame)
                    .filter(|node| {
                        node["ref"]
                            .as_str()
                            .or_else(|| node["element_token"].as_str())
                            == Some(r#ref)
                    })
                    .collect();
                let unique = matches
                    .first()
                    .copied()
                    .filter(|first| matches.iter().all(|node| *node == *first));
                let actual = unique.and_then(|node| node.get(field));
                (actual == Some(value), actual.is_some())
            }
        };
        let mut outcome = json!({"index":index,"matched":matched,"available":available});
        if matched {
            outcome["evidence"] =
                serde_json::to_value(check).map_err(|_| bad("无法整理核验结果"))?;
        }
        outcomes.push(outcome);
    }
    let matched = outcomes.iter().all(|v| v["matched"] == true);
    let mut source = serde_json::Map::new();
    for key in ["pid", "window_id", "target_id", "tab_id"] {
        if let Some(value) = frame.arguments.get(key) {
            source.insert(key.into(), value.clone());
        }
    }
    Ok(json!({"matched":matched,"checks":outcomes,
        "observation_id":frame.id,"source":source,"evidence_scope":"current_observation_only","task_completion":"not_assessed",
        "note":"matched=true表示这些检查已通过，可继续剩余步骤；task_completion=not_assessed表示本工具不判断整个任务，不是任务失败或要求继续核验。evidence仅为此来源和observation_id实际匹配的历史摘录，可作跨应用取材，不是之后界面的当前状态。片段未找到不等于全页不存在；匹配不证明发送、保存或交易成功。"}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn conditional_wait_is_bounded_read_only_and_never_reuses_element_tokens() {
        let base = json!({"operation":"get_browser_state","arguments":{"target_id":"t","tab_id":"p"},"wait_for":{"timeout_ms":1000,"checks":[{"kind":"text_present","text":"done"}]}});
        let (wait, poll) = wait_request(&base).unwrap();
        assert!(poll.get("wait_for").is_none());
        let mut calls = 0;
        let out = wait
            .run(|_| {
                calls += 1;
                Ok((
                    ToolOutput::read(format!("snapshot-{calls}")),
                    calls == 2,
                    false,
                ))
            })
            .unwrap();
        assert_eq!(calls, 2);
        assert!(out.text.contains("\"wait_status\":\"matched\""));
        assert!(!out.text.contains("snapshot-1"));
        for invalid in [
            json!({"timeout_ms":10001,"checks":[{"kind":"text_present","text":"done"}]}),
            json!({"timeout_ms":100,"checks":[]}),
            json!({"timeout_ms":100,"checks":[{"kind":"element_state","ref":"old","field":"enabled","value":true}]}),
        ] {
            let mut args = base.clone();
            args["wait_for"] = invalid;
            assert!(wait_request(&args).is_err());
        }
        for op in ["browser_click", "list_windows", "browser_dialog"] {
            let mut args = base.clone();
            args["operation"] = json!(op);
            assert!(wait_request(&args).is_err());
        }
        let mut missing = base.clone();
        missing["arguments"]
            .as_object_mut()
            .unwrap()
            .remove("tab_id");
        assert!(wait_request(&missing).is_err());
    }

    #[test]
    fn wait_stops_on_dialog_error_or_budget_without_replaying_any_action() {
        let wait = WaitFor {
            timeout_ms: 1,
            checks: vec![Check::TextPresent {
                text: "done".into(),
            }],
        };
        let mut calls = 0;
        let out = wait
            .run(|_| {
                calls += 1;
                Ok((ToolOutput::read("dialog"), true, true))
            })
            .unwrap();
        assert_eq!(calls, 1);
        assert!(out.text.contains("interrupted_by_dialog"));
        assert!(out.text.contains("\"matched\":false"));
        let mut calls = 0;
        assert!(
            wait.run(|_| {
                calls += 1;
                Err(bad("driver unavailable"))
            })
            .is_err()
        );
        assert_eq!(calls, 1);
        let out = wait
            .run(|_| {
                std::thread::sleep(Duration::from_millis(2));
                Ok((ToolOutput::read("pending"), false, false))
            })
            .unwrap();
        assert!(out.text.contains("condition_unmet"));
        assert!(out.text.contains("\"degraded\":true"));
    }

    fn frame() -> Frame {
        Frame {
            id: "current".into(),
            at: Instant::now(),
            hotel_dates: None,
            text: "模型目的、查询条件和指导语不能作为页面证据".into(),
            arguments: json!({"target_id":"t","tab_id":"tab","query":"not-on-page","page":{"url":"https://example.com/result"},
                "snapshot":{"scope":"page","complete":false},"refs":[{"ref":"r1","role":"checkbox","name":"保存选项","checked":true}],
                "content_refs":[{"ref":"r2","role":"statictext","name":"预览：合成内容"}]}),
        }
    }

    #[test]
    fn verification_keeps_source_identity_without_claiming_task_is_incomplete() {
        let frame = frame();
        let out = verify(&json!({"observation_id":"current","checks":[{"kind":"text_present","text":"预览：合成内容"}]}),Some(&frame),"current").unwrap();
        assert_eq!(out["matched"], true);
        assert!(out.get("task_completed").is_none());
        assert_eq!(out["task_completion"], "not_assessed");
        assert_eq!(out["source"]["target_id"], "t");
        assert_eq!(out["source"]["tab_id"], "tab");
        assert!(out["source"].get("query").is_none());
    }

    #[test]
    fn a_model_retention_flag_cannot_turn_verification_into_a_handoff_bypass() {
        let frame = frame();
        let args = json!({"observation_id":"current","retain_dialog":true,"checks":[{"kind":"text_present","text":"预览：合成内容"}]});
        assert!(verify(&args, Some(&frame), "current").is_err());
    }

    #[test]
    fn conditions_use_real_nodes_not_query_purpose_or_generated_guidance() {
        let frame = frame();
        for text in ["not-on-page", "模型目的", "current"] {
            let out = verify(
                &json!({"observation_id":"current","checks":[{"kind":"text_present","text":text}]}),
                Some(&frame),
                "current",
            )
            .unwrap();
            assert_eq!(out["matched"], false);
        }
        let out=verify(&json!({"observation_id":"current","checks":[{"kind":"text_present","text":"预览：合成内容"},{"kind":"element_state","ref":"r1","field":"checked","value":true},{"kind":"url_is","url":"https://example.com/result"}]}),Some(&frame),"current").unwrap();
        assert_eq!(out["matched"], true);
        assert_eq!(out["task_completion"], "not_assessed");
        assert_eq!(out["checks"][0]["evidence"]["text"], "预览：合成内容");
    }

    #[test]
    fn stale_unknown_partial_and_missing_evidence_fail_closed() {
        let mut frame = frame();
        let args = json!({"observation_id":"current","checks":[{"kind":"element_state","ref":"r1","field":"selected","value":true}]});
        let out = verify(&args, Some(&frame), "current").unwrap();
        assert_eq!(out["matched"], false);
        assert_eq!(out["checks"][0]["available"], false);
        assert!(verify(&args, None, "current").is_err());
        assert!(verify(&args, Some(&frame), "other-process").is_err());
        frame.at = Instant::now() - Duration::from_secs(121);
        assert!(verify(&args, Some(&frame), "current").is_err());
    }

    #[test]
    fn native_elements_and_flat_browser_urls_are_real_evidence_without_field_aliases() {
        let mut frame = frame();
        frame.arguments = json!({"url":"https://example.com/flat","elements":[{"element_token":"s00000001:1","role":"AXTextArea","label":"临时文档","value":"合成内容","enabled":true}]});
        for check in [
            json!({"kind":"text_present","text":"合成内容"}),
            json!({"kind":"url_is","url":"https://example.com/flat"}),
            json!({"kind":"element_state","ref":"s00000001:1","field":"enabled","value":true}),
        ] {
            let out = verify(
                &json!({"observation_id":"current","checks":[check]}),
                Some(&frame),
                "current",
            )
            .unwrap();
            assert_eq!(out["matched"], true);
        }
    }

    #[test]
    fn malformed_limits_unknown_fields_and_conflicting_refs_are_not_evidence() {
        let mut frame = frame();
        for args in [
            json!({"observation_id":"current","checks":[]}),
            json!({"observation_id":"current","checks":vec![json!({"kind":"text_present","text":"x"});9]}),
            json!({"observation_id":"current","checks":[{"kind":"text_present","text":" "}]}),
            json!({"observation_id":"current","checks":[{"kind":"text_absent","text":"x"}]}),
            json!({"observation_id":"current","checks":[{"kind":"element_state","ref":"r1","field":"script","value":"x"}]}),
            json!({"observation_id":"current","checks":[{"kind":"text_present","text":"x","trusted":true}]}),
            json!({"observation_id":"current","checks":[{"kind":"text_present","text":"x"}],"approved":true}),
        ] {
            assert!(verify(&args, Some(&frame), "current").is_err());
        }
        let args = json!({"observation_id":"current","checks":[{"kind":"element_state","ref":"r1","field":"checked","value":true}]});
        frame.arguments["content_refs"] = json!([{"ref":"r1","checked":false}]);
        let out = verify(&args, Some(&frame), "current").unwrap();
        assert_eq!(out["checks"][0]["available"], false);
        assert_eq!(out["matched"], false);
        frame.arguments = json!({"tree_markdown":"- [1] textfield: 本机真实内容"});
        let out = verify(&json!({"observation_id":"current","checks":[{"kind":"text_present","text":"本机真实内容"}]}),Some(&frame),"current").unwrap();
        assert_eq!(out["matched"], true);
    }

    #[test]
    fn changed_ref_ids_are_not_progress_but_changed_values_and_scopes_are() {
        let mut frame = frame();
        let first = observation(&frame);
        frame.arguments["refs"][0]["ref"] = json!("new-r1");
        assert_eq!(first, observation(&frame));
        frame.arguments["refs"][0]["checked"] = json!(false);
        assert_ne!(first.state, observation(&frame).state);
        frame.arguments["tab_id"] = json!("another");
        assert_ne!(first.scope, observation(&frame).scope);
    }
}
