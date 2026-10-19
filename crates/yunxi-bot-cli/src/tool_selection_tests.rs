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
            messages.push(Message::system(yunxi_bot_core::prompts::tool_catalog(
                &specs,
            )));
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
                    .with_temperature(0.0)
                    .with_thinking(yunxi_bot_core::think::Thinking::Enabled)
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

#[test]
#[ignore = "Explicit real location/weather + DeepSeek probe; saves only coordinate-free forecast for independent comparison"]
fn live_weather_reply_retains_independent_forecast_evidence() {
    use std::sync::Arc;
    use yunxi_bot_core::tool::{Rule, ToolPolicy};
    let home = PathBuf::from(std::env::var("YUNXI_TEST_HOME").expect("isolated test home"));
    assert!(
        home.join("acceptance-only").exists(),
        "must use isolated data"
    );
    let out = PathBuf::from(std::env::var("YUNXI_WEATHER_REPORT").expect("new report path"));
    drop(
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&out)
            .unwrap(),
    );
    let identity: Vec<_> = ["persona.md", "profile.md"]
        .iter()
        .map(|n| fs::read(home.join(n)).unwrap())
        .collect();
    let registry = crate::tooling::default_registry(&home).unwrap();
    let allowed = ["now", "mac_location", "mac_weather"];
    let policy = ToolPolicy {
        allow: allowed.iter().map(|n| Rule::tool(*n)).collect(),
        deny: registry
            .specs()
            .iter()
            .map(|s| s["function"]["name"].as_str().unwrap())
            .filter(|n| !allowed.contains(n))
            .map(Rule::tool)
            .collect(),
        ..ToolPolicy::default()
    };
    let mut handler = super::ChatHandler::new(
        home.clone(),
        "云熙",
        super::DEFAULT_PERSONA,
        super::default_rules(),
    )
    .with_tools(registry)
    .with_policy(policy)
    .with_sink(Arc::new(
        crate::tool_sink::LedgerToolSink::open(home.join("ledger.jsonl")).unwrap(),
    ))
    .with_decider(Arc::new(yunxi_bot_core::decide::LayaDecider::new(
        "http://127.0.0.1:17870/decide",
    )));
    let mut rows = Vec::new();
    for input in [
        "请查询我这里明天下午三点的预报，只回答温度、降雨概率以及是不是一定会下雨。",
        "再解释一下：这个百分比能证明三点开始下、四点就停吗？不要查其他资料。",
    ] {
        let route = handler
            .route_for(input, yunxi_bot_core::think::ReasoningEffort::Auto)
            .unwrap();
        let mut records = Vec::new();
        let reply = handler.chat_turn(&route, input, &mut records).unwrap();
        let forecasts: Vec<Value> = handler
            .chat_layout()
            .unwrap()
            .build()
            .iter()
            .filter(|m| m.role == yunxi_bot_core::think::Role::Tool)
            .filter_map(|m| {
                m.live_content
                    .as_ref()
                    .and_then(|s| serde_json::from_str::<Value>(s).ok())
            })
            .filter(|v| v.get("forecast").is_some())
            .collect();
        assert!(!forecasts.is_empty());
        for data in &forecasts {
            assert!(data["forecast"].get("latitude").is_none());
            assert!(data["forecast"].get("longitude").is_none());
            assert_eq!(data["source_url"], "https://open-meteo.com/");
        }
        rows.push(json!({"input":input,"reply":reply,"forecasts":forecasts}));
        fs::write(&out, serde_json::to_vec_pretty(&json!({"scope":"real ChatHandler with native weather; semantic evaluation is independent/manual", "turns":rows})).unwrap()).unwrap();
    }
    for (index, name) in ["persona.md", "profile.md"].iter().enumerate() {
        assert_eq!(fs::read(home.join(name)).unwrap(), identity[index]);
    }
}
