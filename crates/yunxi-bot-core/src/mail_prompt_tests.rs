//! 用合成复杂邮件验证实际收发提示词；不连接Mac、不发送、不通知。
use super::*;
const CASES: &str = include_str!("../../../models/eval/mail-prompt-workflow-20261016.json");
fn cases() -> Vec<Value> {
    serde_json::from_str(CASES).unwrap()
}
fn check(case: &Value, result: &Value) -> Vec<String> {
    let expected = &case["expect"];
    let mut failures = Vec::new();
    if let Some(status) = expected["status"].as_array()
        && !status.contains(&result["status"])
    {
        failures.push("status".into());
    }
    if let Some(value) = expected.get("needs_action")
        && result["needs_action"] != *value
    {
        failures.push("needs_action".into());
    }
    if let Some(value) = expected.get("risks")
        && result["risks"] != *value
    {
        failures.push("risks".into());
    }
    if let Some(value) = expected.get("risk_contains")
        && !result["risks"]
            .as_array()
            .is_some_and(|rs| rs.contains(value))
    {
        failures.push("risk_contains".into());
    }
    let field = |key: &str| -> String {
        if key == "combined" {
            result.to_string()
        } else if key == "questions" {
            result[key].to_string()
        } else {
            result[key].as_str().unwrap_or("").to_owned()
        }
    };
    if let Some(fields) = expected["contains"].as_object() {
        for (k, groups) in fields {
            for group in groups.as_array().unwrap() {
                if !group
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|s| field(k).contains(s.as_str().unwrap()))
                {
                    failures.push(format!("{k}: missing {group}"));
                }
            }
        }
    }
    if let Some(fields) = expected["excludes"].as_object() {
        for (k, words) in fields {
            for word in words.as_array().unwrap() {
                if field(k)
                    .to_lowercase()
                    .contains(&word.as_str().unwrap().to_lowercase())
                {
                    failures.push(format!("{k}: unexpected {word}"));
                }
            }
        }
    }
    if expected["english_body"] == true
        && (field("body").is_empty()
            || field("body")
                .chars()
                .any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c)))
    {
        failures.push("english_body".into());
    }
    failures
}
#[test]
fn workflow_cases_have_unique_ids_and_meaningful_failure_checks() {
    let cases = cases();
    let mut seen = std::collections::BTreeSet::new();
    assert_eq!(cases.len(), 15);
    for c in &cases {
        assert!(seen.insert(c["id"].as_str().unwrap()));
        assert!(["send", "receive"].contains(&c["mode"].as_str().unwrap()));
        assert!(!c["body"].as_str().unwrap().is_empty());
        assert!(!check(c, &json!({})).is_empty());
    }
    let c = &cases[0];
    assert!(check(c, &json!({"status":"needs_input","body":"来信已收到，谢谢。","questions":["你是否已收到公开说明？"]})).is_empty());
    assert!(
        !check(
            c,
            &json!({"status":"ready","body":"已收到，附件是不是漏发？"})
        )
        .is_empty()
    );
}
#[test]
#[ignore = "explicit DeepSeek evaluation on synthetic mail only, no native tools"]
fn live_mail_prompt_workflows() {
    let home = PathBuf::from(std::env::var("YUNXI_MAIL_PROMPT_HOME").expect("credential home"));
    let report = PathBuf::from(std::env::var("YUNXI_MAIL_PROMPT_REPORT").expect("report path"));
    assert!(!report.exists(), "preserve previous evidence");
    let mut cfg = crate::think::ModelSpec::DEEPSEEK_FLASH.thinker_config();
    cfg.timeout = std::time::Duration::from_secs(45);
    let model = crate::think::OpenAiThinker::from_home(&home, cfg).unwrap();
    let mut rows = Vec::new();
    let mut selected = cases();
    if let Ok(ids) = std::env::var("YUNXI_MAIL_PROMPT_ONLY") {
        let ids: Vec<_> = ids.split(',').collect();
        selected.retain(|c| ids.contains(&c["id"].as_str().unwrap()));
        assert_eq!(selected.len(), ids.len());
    }
    let start_time = chrono::DateTime::parse_from_rfc3339("2026-10-16T19:30:00+08:00")
        .unwrap()
        .timestamp_millis();
    for case in selected {
        let mail = json!({"subject":case["subject"],"sender":"同事 <person@example.com>","from_name":"同事","from_addr":"person@example.com","to":["owner@example.com"],"cc":[],"body":case["body"],"received_at":"2026-10-16T11:30:00Z","received_at_ms":start_time,"attachments":[]});
        let input = if case["mode"] == "send" {
            json!({"mail":mail,"thread":[mail],"profile":case["profile"].as_str().unwrap_or(""),"profile_missing":case["profile"].is_null(),"plans":{"period":{"start":"2026-10-16T00:00:00+08:00","end":"2026-10-23T00:00:00+08:00"},"busy":case.get("busy").cloned().unwrap_or(json!([])),"reminder_deadlines":[],"undated_reminders":0,"details_withheld":true},"captured_at_ms":start_time})
        } else {
            json!({"mail":mail,"body":case["body"],"received_time_is_unverified":true})
        };
        let system = if case["mode"] == "send" {
            format!("{SEND_SYSTEM}\n{OUTPUT}")
        } else {
            format!(
                "{}\n本次分析时间（Unix毫秒）：{start_time}；邮件中的相对日期以received_at_ms为参照。",
                include_str!("prompts/mail_review.md")
            )
        };
        let mut req = ThinkRequest::new(vec![
            Message::system(system),
            Message::user(input.to_string()),
        ])
        .with_max_tokens(8192)
        .with_temperature(0.0)
        .with_thinking(Thinking::Enabled);
        req.json_output = true;
        // 测试串行等待已有客户端的最小间隔；不关闭限流，也不重试已发出的请求。
        if let Some(wait) = model.throttle_wait() {
            std::thread::sleep(wait + std::time::Duration::from_millis(1));
        }
        let started = std::time::Instant::now();
        let evaluated = (|| -> Result<Value, String> {
            let response = model.think(&req).map_err(|e| match e {
                crate::think::ThinkError::LocalThrottle { .. } => "local throttle",
                crate::think::ThinkError::RateLimited { .. } => "provider rate limit",
                crate::think::ThinkError::Auth(_) => "authentication error",
                crate::think::ThinkError::Payment(_) => "quota error",
                crate::think::ThinkError::Network(_) => "network error",
                _ => "model response error",
            })?;
            if !response.tool_calls.is_empty() || response.finish_reason.as_deref() != Some("stop")
            {
                return Err("incomplete output or tool request".into());
            }
            let parsed = if case["mode"] == "send" {
                serde_json::to_value(Reply::parse(&response.content)?).unwrap()
            } else {
                let a = crate::mail_review::Analysis::parse(
                    &response.content,
                    case["body"].as_str().unwrap(),
                )?;
                if (a.category == "suspicious" || !a.risks.is_empty()) && a.evidence.is_empty() {
                    return Err("risk without evidence".into());
                }
                serde_json::to_value(a).unwrap()
            };
            Ok(
                json!({"result":parsed,"failures":check(&case,&parsed),"model":response.model,"usage":response.usage}),
            )
        })();
        let mut row = match evaluated {
            Ok(row) => row,
            Err(e) => json!({"error":e,"failures":[e]}),
        };
        row["id"] = case["id"].clone();
        row["fixture"] = case.clone();
        row["seconds"] = json!(started.elapsed().as_secs_f64());
        println!(
            "{} failures={} ({:.1}s)",
            case["id"],
            row["failures"],
            started.elapsed().as_secs_f64()
        );
        rows.push(row);
        save_private(&report,&json!({"synthetic_only":true,"native_calls":0,"email_sent":false,"system_send":SEND_SYSTEM,"system_receive":include_str!("prompts/mail_review.md"),"cases":rows})).unwrap();
    }
    assert!(
        rows.iter()
            .all(|r| r["failures"].as_array().unwrap().is_empty()),
        "see report; do not weaken expectations"
    );
}
