//! Parent-window Sheet discovery follows Peekaboo's structural targeting; no coordinate or input fallback.
use super::{Frame, Input, Result, bad};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

pub(super) fn has_dialog(metadata: &Value) -> bool {
    metadata["elements"]
        .as_array()
        .is_some_and(|nodes| nodes.iter().any(is_dialog))
}

pub(super) fn parent_candidates(inventory: &Value, current: &Value) -> Vec<Value> {
    let Some(pid) = current["pid"].as_u64().filter(|pid| *pid > 0) else {
        return Vec::new();
    };
    inventory["windows"].as_array().into_iter().flatten()
        .filter(|window| window["pid"].as_u64() == Some(pid) && window["window_id"] != current["window_id"])
        .filter_map(|window| {
            let window_id = window["window_id"].as_u64().filter(|id| *id>0)?;
            Some(json!({"relationship":"candidate_only_requires_AXSheet_evidence","window_title":window["title"],"observe":{"operation":"get_window_state","arguments":{"pid":pid,"window_id":window_id,"tree_format":"both","max_elements":2000,"max_depth":5,"timeout_ms":5000}}}))
        }).take(8).collect()
}

fn is_dialog(node: &Value) -> bool {
    matches!(
        node["role"].as_str(),
        Some("AXSheet" | "AXDialog" | "AXAlert")
    ) || (node["role"] == "AXWindow"
        && matches!(
            node["subrole"].as_str(),
            Some("AXDialog" | "AXSystemDialog")
        ))
}

fn descendants(index: u64, root: u64, nodes: &BTreeMap<u64, &Value>) -> bool {
    let mut current = Some(index);
    for _ in 0..=nodes.len() {
        match current {
            Some(i) if i == root => return true,
            Some(i) => current = nodes.get(&i).and_then(|n| n["parent_index"].as_u64()),
            None => return false,
        }
    }
    false
}

fn index(metadata: &Value) -> Result<BTreeMap<u64, &Value>> {
    let mut indexed = BTreeMap::new();
    for node in metadata["elements"].as_array().into_iter().flatten() {
        let key = node["element_index"]
            .as_u64()
            .ok_or_else(|| bad("native_tree_unavailable：结构节点缺少索引，不能猜弹窗所属关系"))?;
        if indexed.len() >= 4096 || indexed.insert(key, node).is_some() {
            return Err(bad(
                "native_tree_unavailable：结构过大或节点索引冲突，未建立弹窗关系",
            ));
        }
    }
    for node in indexed.values() {
        let mut parent = node["parent_index"].as_u64();
        for depth in 0..=indexed.len() {
            let Some(id) = parent else {
                break;
            };
            if depth == indexed.len() {
                return Err(bad(
                    "native_tree_unavailable：父子关系存在循环，未建立弹窗关系",
                ));
            }
            parent =
                indexed.get(&id).ok_or_else(|| {
                    bad("native_tree_unavailable：缺少真实祖先节点，不能关联弹窗")
                })?["parent_index"]
                    .as_u64();
        }
    }
    Ok(indexed)
}

fn leaf_dialogs<'a>(nodes: &'a BTreeMap<u64, &'a Value>) -> Vec<(u64, &'a Value)> {
    let mut outer_dialogs = BTreeSet::new();
    for node in nodes.values().filter(|node| is_dialog(node)) {
        let mut parent = node["parent_index"].as_u64();
        for _ in 0..nodes.len() {
            let Some(id) = parent else {
                break;
            };
            let Some(ancestor) = nodes.get(&id) else {
                break;
            };
            if is_dialog(ancestor) {
                outer_dialogs.insert(id);
            }
            parent = ancestor["parent_index"].as_u64();
        }
    }
    nodes
        .iter()
        .filter(|(_, n)| is_dialog(n))
        .filter(|(id, _)| !outer_dialogs.contains(id))
        .map(|(id, n)| (*id, *n))
        .collect()
}

pub(super) fn enrich(metadata: &mut Value) -> Result<()> {
    if !has_dialog(metadata) {
        return Ok(());
    }
    let nodes = index(metadata)?;
    let roots = leaf_dialogs(&nodes);
    let mut dialogs = Vec::new();
    for (root, node) in roots {
        let mut texts = Vec::new();
        let mut buttons = Vec::new();
        let mut fields = Vec::new();
        for (id, child) in &nodes {
            if !descendants(*id, root, &nodes) {
                continue;
            }
            let label = child["label"].as_str().unwrap_or("");
            let token = child["element_token"].as_str().unwrap_or("");
            match child["role"].as_str() {
                Some("AXButton") if !label.is_empty() && !token.is_empty() => {
                    buttons.push(json!({"element_token":token,"label":label.chars().take(200).collect::<String>(),"enabled":child["enabled"] != false}));
                }
                Some("AXStaticText") => {
                    let value = child["value"]
                        .as_str()
                        .filter(|s| !s.trim().is_empty())
                        .unwrap_or(label);
                    if !value.is_empty() {
                        texts.push(value.chars().take(600).collect::<String>());
                    }
                }
                Some("AXTextField" | "AXTextArea" | "AXComboBox" | "AXSearchField")
                    if !token.is_empty() =>
                {
                    fields.push(json!({"element_token":token,"role":child["role"],"label":label.chars().take(200).collect::<String>(),"value":child["value"].as_str().unwrap_or("").chars().take(600).collect::<String>()}));
                }
                _ => {}
            }
        }
        let field_count = fields.len();
        fields.truncate(16);
        dialogs.push(json!({"root_token":node["element_token"],"role":node["role"],"title":node["label"],"text":texts.join("\n").chars().take(2000).collect::<String>(),"buttons":buttons,"fields":fields,"field_count":field_count,"fields_truncated":field_count>16,"complete":metadata["elements_complete"]==true}));
    }
    let needs_choice = dialogs.len() > 1
        || dialogs.iter().any(|dialog| {
            dialog["buttons"]
                .as_array()
                .is_some_and(|buttons| buttons.iter().filter(|b| b["enabled"] == true).count() > 1)
                || dialog["fields"]
                    .as_array()
                    .is_some_and(|fields| !fields.is_empty())
        });
    metadata["native_dialogs"] = json!(dialogs);
    metadata["native_requires_choice"] = json!(needs_choice);
    Ok(())
}

pub(super) fn check_action(frame: &Frame, input: &Input) -> Result<()> {
    if input.operation != "click" || !has_dialog(&frame.arguments) {
        return Ok(());
    }
    let nodes = index(&frame.arguments)?;
    let roots = leaf_dialogs(&nodes);
    if roots.len() != 1 {
        return Err(bad(
            "native_dialog_ambiguous：当前有多个或无法唯一关联的弹窗，保持原状并先追问",
        ));
    }
    let Some(token) = input.arguments["element_token"].as_str() else {
        return Err(bad(
            "native_dialog_semantic_required：当前父窗口存在活动弹窗，不能用父窗口截图坐标猜点；先读取弹窗的真实AX控件，不自动关闭",
        ));
    };
    let matches: Vec<_> = nodes
        .iter()
        .filter(|(_, n)| n["element_token"] == token)
        .collect();
    if matches.len() != 1 || !descendants(*matches[0].0, roots[0].0, &nodes) {
        return Err(bad(
            "native_dialog_blocks_background：目标不在当前活动弹窗子树内，未点击背景或外层控件",
        ));
    }
    Ok(())
}

pub(super) fn selects_button(frame: &Frame, input: &Input) -> bool {
    input.operation == "click"
        && has_dialog(&frame.arguments)
        && input.arguments["action"]
            .as_str()
            .is_none_or(|a| a == "press")
        && input.arguments["button"]
            .as_str()
            .is_none_or(|b| b == "left")
        && input.arguments.get("x").is_none()
        && input.arguments.get("y").is_none()
        && check_action(frame, input).is_ok()
        && frame.arguments["elements"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|node| {
                node["role"] == "AXButton"
                    && node["element_token"] == input.arguments["element_token"]
            })
}

pub(super) fn choice_dispatched(receipt: &Value) -> bool {
    receipt["route"] == "accessibility"
        && matches!(
            receipt["effect"].as_str(),
            Some("unverifiable" | "confirmed")
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn metadata() -> Value {
        json!({"pid":1,"window_id":2,"elements_complete":true,"elements":[
            {"element_index":0,"role":"AXWindow","element_token":"s:0"},
            {"element_index":1,"parent_index":0,"role":"AXButton","label":"背景按钮","element_token":"s:1"},
            {"element_index":2,"parent_index":0,"role":"AXSheet","label":"合成选择","element_token":"s:2"},
            {"element_index":3,"parent_index":2,"role":"AXStaticText","value":"合成说明","element_token":"s:3"},
            {"element_index":4,"parent_index":2,"role":"AXButton","label":"选项甲","element_token":"s:4","enabled":true},
            {"element_index":5,"parent_index":2,"role":"AXButton","label":"选项乙","element_token":"s:5","enabled":true}
        ]})
    }

    #[test]
    fn unknown_or_suspected_noop_receipts_do_not_consume_the_user_choice() {
        for effect in ["unverifiable", "confirmed"] {
            assert!(choice_dispatched(
                &json!({"route":"accessibility","effect":effect})
            ));
        }
        for receipt in [
            json!({}),
            json!({"route":"accessibility","effect":"suspected_noop"}),
            json!({"route":"accessibility","effect":"unknown"}),
            json!({"route":"pointer","effect":"unverifiable"}),
            json!({"path":"ax","effect":"unverifiable"}),
        ] {
            assert!(!choice_dispatched(&receipt));
        }
    }

    #[test]
    fn only_an_actual_dialog_button_press_can_consume_a_choice() {
        let frame = Frame {
            id: "current".into(),
            at: Instant::now(),
            text: String::new(),
            hotel_dates: None,
            arguments: metadata(),
        };
        let mut input = Input {
            operation: "click".into(),
            arguments: json!({"element_token":"s:4"}),
            observation_id: Some("current".into()),
            purpose: Some("已批准的具体选择".into()),
        };
        assert!(selects_button(&frame, &input));
        for arguments in [
            json!({"element_token":"s:1"}),
            json!({"element_token":"s:4","action":"show_menu"}),
            json!({"element_token":"s:4","button":"right"}),
            json!({"x":10,"y":20}),
        ] {
            input.arguments = arguments;
            assert!(!selects_button(&frame, &input));
        }
    }

    #[test]
    fn dialog_field_summary_is_bounded_without_losing_actual_tree_or_claiming_completeness() {
        let mut metadata = metadata();
        let nodes = metadata["elements"].as_array_mut().unwrap();
        for i in 6..36 {
            nodes.push(json!({"element_index":i,"parent_index":2,"role":"AXTextField","label":format!("field {i}"),"element_token":format!("s:{i}")}));
        }
        enrich(&mut metadata).unwrap();
        assert_eq!(metadata["elements"].as_array().unwrap().len(), 36);
        let dialog = &metadata["native_dialogs"][0];
        assert_eq!(dialog["fields"].as_array().unwrap().len(), 16);
        assert_eq!(dialog["field_count"], 30);
        assert_eq!(dialog["fields_truncated"], true);
        assert_eq!(metadata["native_requires_choice"], true);
    }

    #[test]
    fn parent_suggestions_are_readonly_same_process_and_never_claim_a_proven_relationship() {
        let inventory = json!({"windows":[{"pid":1,"window_id":2},{"pid":1,"window_id":3,"title":"parent"},{"pid":9,"window_id":4},{"pid":1,"window_id":0}]});
        let hints = parent_candidates(&inventory, &json!({"pid":1,"window_id":2}));
        assert_eq!(hints.len(), 1);
        assert_eq!(hints[0]["observe"]["arguments"]["window_id"], 3);
        assert_eq!(hints[0]["observe"]["operation"], "get_window_state");
        assert!(
            hints[0]["relationship"]
                .as_str()
                .unwrap()
                .starts_with("candidate_only")
        );
    }

    #[test]
    fn malformed_or_cyclic_native_ancestors_are_not_guessed_into_a_dialog() {
        for parent in [json!(999), json!(2)] {
            let mut metadata = metadata();
            metadata["elements"][2]["parent_index"] = parent;
            assert!(enrich(&mut metadata).is_err());
        }
    }

    #[test]
    fn dialog_context_uses_only_the_actual_sheet_and_keeps_pending_choices() {
        let mut metadata = metadata();
        enrich(&mut metadata).unwrap();
        assert_eq!(metadata["native_requires_choice"], true);
        let dialogs = metadata["native_dialogs"].as_array().unwrap();
        assert_eq!(dialogs.len(), 1);
        assert_eq!(dialogs[0]["root_token"], "s:2");
        assert!(!dialogs[0].to_string().contains("背景按钮"));
        assert!(dialogs[0].to_string().contains("合成说明"));
    }

    #[test]
    fn active_sheet_rejects_background_tokens_and_parent_pixels_but_accepts_its_own_token() {
        let frame = Frame {
            id: "now".into(),
            at: Instant::now(),
            text: String::new(),
            hotel_dates: None,
            arguments: metadata(),
        };
        let mut input = Input {
            operation: "click".into(),
            arguments: json!({"element_token":"s:1"}),
            observation_id: Some("now".into()),
            purpose: Some("test".into()),
        };
        assert!(check_action(&frame, &input).is_err());
        input.arguments = json!({"x":30,"y":40});
        assert!(check_action(&frame, &input).is_err());
        input.arguments = json!({"element_token":"s:4"});
        assert!(check_action(&frame, &input).is_ok());
    }

    #[test]
    fn nested_or_ambiguous_dialogs_never_authorize_the_outer_or_wrong_sheet() {
        let mut metadata = metadata();
        metadata["elements"].as_array_mut().unwrap().extend([
            json!({"element_index":6,"parent_index":2,"role":"AXSheet","element_token":"s:6"}),
            json!({"element_index":7,"parent_index":6,"role":"AXButton","label":"内层选择","element_token":"s:7"})
        ]);
        enrich(&mut metadata).unwrap();
        assert_eq!(metadata["native_dialogs"][0]["root_token"], "s:6");
        let mut frame = Frame {
            id: "now".into(),
            at: Instant::now(),
            text: String::new(),
            hotel_dates: None,
            arguments: metadata,
        };
        let mut input = Input {
            operation: "click".into(),
            arguments: json!({"element_token":"s:4"}),
            observation_id: None,
            purpose: None,
        };
        assert!(check_action(&frame, &input).is_err());
        input.arguments = json!({"element_token":"s:7"});
        assert!(check_action(&frame, &input).is_ok());
        frame.arguments["elements"].as_array_mut().unwrap().push(
            json!({"element_index":8,"parent_index":0,"role":"AXSheet","element_token":"s:8"}),
        );
        assert!(check_action(&frame, &input).is_err());
        frame.arguments["elements"]
            .as_array_mut()
            .unwrap()
            .push(json!({"element_index":8,"role":"AXButton"}));
        assert!(enrich(&mut frame.arguments).is_err());
    }
}
