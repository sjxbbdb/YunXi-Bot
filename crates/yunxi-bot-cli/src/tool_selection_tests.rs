//! Full production catalog selection probes. Proposed calls are never dispatched.
use serde_json::{Value, json};
use std::{fs, os::unix::fs::OpenOptionsExt, path::PathBuf};
use yunxi_bot_core::think::{Message, OpenAiThinker, ThinkRequest, Thinker, ThinkerConfig};

fn cases() -> Vec<Value> {
    serde_json::from_str(include_str!(
        "../../../sidecar/fixtures/tool-selection.json"
    ))
    .unwrap()
}

fn check(case: &Value, calls: &[Value], content: &str) -> bool {
    let allowed = case["allowed"].as_array().unwrap();
    let names: Vec<_> = calls.iter().map(|c| &c["function"]["name"]).collect();
    let required = case["required"].as_array().unwrap();
    names.iter().all(|n| allowed.contains(n))
        && required.iter().all(|n| names.contains(&n))
        && (allowed.is_empty() || !names.is_empty() || case["clarify"] == true)
        && (case["clarify"] != true
            || (case["preparatory"] == true && !names.is_empty())
            || names.iter().any(|n| n.as_str() == Some("ask_user"))
            || content.contains(['?', '？']))
        && calls.iter().all(|c| {
            let Ok(args) =
                serde_json::from_str::<Value>(c["function"]["arguments"].as_str().unwrap_or(""))
            else {
                return false;
            };
            args.is_object()
                && case["args"][c["function"]["name"].as_str().unwrap_or("")]
                    .as_object()
                    .is_none_or(|expected| expected.iter().all(|(k, v)| args.get(k) == Some(v)))
        })
}

#[test]
fn tool_selection_fixture_and_scorer_reject_missing_wrong_and_changed_calls() {
    let registry = crate::tooling::default_registry(&std::env::temp_dir()).unwrap();
    for case in cases() {
        for name in case["allowed"].as_array().unwrap() {
            assert!(registry.get(name.as_str().unwrap()).is_some(), "{case}");
        }
    }
    let c = json!({"allowed":["read_file"],"required":["read_file"],"args":{"read_file":{"path":"a.txt"}}});
    assert!(!check(&c, &[], "done"));
    for (name, path, valid) in [
        ("read_file", "a.txt", true),
        ("write_file", "a.txt", false),
        ("read_file", "b.txt", false),
    ] {
        assert_eq!(
            check(
                &c,
                &[json!({"function":{"name":name,"arguments":json!({"path":path}).to_string()}})],
                ""
            ),
            valid
        );
    }
}

#[test]
#[ignore = "Explicit DeepSeek selection benchmark, all 61 schemas, synthetic context, no tool execution"]
fn live_full_catalog_model_selection() {
    let home = PathBuf::from(std::env::var("YUNXI_TEST_HOME").expect("credential home"));
    let out = PathBuf::from(std::env::var("YUNXI_SELECTION_REPORT").expect("new report path"));
    let file = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(&out)
        .unwrap();
    drop(file);
    let registry = crate::tooling::default_registry(&home).unwrap();
    let specs = registry.specs();
    assert_eq!(specs.len(), 61);
    let mut cfg = ThinkerConfig::deepseek();
    cfg.timeout = std::time::Duration::from_secs(60);
    let model = OpenAiThinker::from_home(&home, cfg).unwrap();
    let mut rows = Vec::new();
    let rounds: usize = std::env::var("YUNXI_SELECTION_ROUNDS")
        .unwrap_or("3".into())
        .parse()
        .unwrap();
    assert!((1..=5).contains(&rounds));
    let selected = std::env::var("YUNXI_SELECTION_ONLY").ok();
    for round in 1..=rounds {
        for case in cases() {
            if selected
                .as_ref()
                .is_some_and(|ids| !ids.split(',').any(|id| Some(id) == case["id"].as_str()))
            {
                continue;
            }
            let mut messages = vec![Message::system(super::Intent::Chat.system())];
            if let Some(history) = case["history"].as_array() {
                for m in history {
                    messages.push(if m["role"] == "user" {
                        Message::user(m["content"].as_str().unwrap())
                    } else {
                        Message::assistant(m["content"].as_str().unwrap())
                    });
                }
            }
            messages.push(Message::user(case["prompt"].as_str().unwrap()));
            if let Some(wait) = model.throttle_wait() {
                std::thread::sleep(wait);
            }
            let start = std::time::Instant::now();
            let result = model.think(
                &ThinkRequest::new(messages)
                    .with_tools(specs.clone())
                    .with_max_tokens(4096),
            );
            let row = match result {
                Ok(r) => {
                    json!({"round":round,"id":case["id"],"passed":check(&case,&r.tool_calls,&r.content),"calls":r.tool_calls,"content":r.content,"seconds":start.elapsed().as_secs_f64(),"model":r.model,"thinking":format!("{:?}",r.thinking)})
                }
                Err(e) => {
                    json!({"round":round,"id":case["id"],"passed":false,"transport_error":e.to_string(),"seconds":start.elapsed().as_secs_f64()})
                }
            };
            eprintln!(
                "selection round={round} case={} passed={}",
                case["id"], row["passed"]
            );
            rows.push(row);
            fs::write(&out,serde_json::to_vec_pretty(&json!({"scope":"proposed first batch only; not execution or real personal context", "tool_count":specs.len(),"cases":rows})).unwrap()).unwrap();
        }
    }
    assert!(!rows.is_empty());
    assert!(
        rows.iter().all(|r| r["passed"] == true),
        "selection failures saved in {}",
        out.display()
    );
}
