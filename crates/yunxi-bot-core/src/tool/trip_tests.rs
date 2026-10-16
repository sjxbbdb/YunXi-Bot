use super::*;
use crate::policy::SandboxMode;
use crate::tool::{GateDecision, Rule, ToolPolicy, gate};

fn request() -> Input {
    input(&json!({"days":[{"date":"2040-01-12"},{"date":"2040-01-10"},{"date":"2040-01-11"}]}))
        .unwrap()
}
fn day(date: &Value, revision: &str, applied: bool) -> Value {
    json!({"date":date,"revision":revision,"applied":applied,"entries":[],"unscheduled_count":0,"ready_to_apply":true,"blocking_issues":[]})
}

#[test]
fn invalid_days_and_nested_revisions_fail_before_any_native_call() {
    let tool = TripTool::new("mac_trip_plan");
    let mut ctx = ToolContext::new(std::env::temp_dir(), SandboxMode::ReadOnly);
    for args in [
        json!({"days":[]}),
        json!({"days":[{"date":"2040-01-10"},{"date":"2040-01-10"}]}),
        json!({"days":[{"date":"2040-01-10"},{"date":"2040-01-24"}]}),
        json!({"days":[{"date":"2040-02-30"}]}),
        json!({"days":[{"date":"2040-01-10","expected_revision":"old-day"}]}),
        json!({"days":[{"date":"2040-01-10","tasks":"invalid"}]}),
        json!({"days":[{"date":"2040-01-10","tasks":[{"id":"a","title":"unknown duration"}]}]}),
        json!({"days":[{"date":"2040-01-10","settings":{"windows":[],"buffer_minutes":10}}]}),
        json!({"days":[{"date":"2040-01-10","unexpected":true}]}),
    ] {
        assert!(matches!(
            tool.call(&args, &mut ctx),
            Err(ToolError::BadArgs { .. })
        ));
    }
}

#[test]
fn incomplete_later_day_does_not_write_earlier_days() {
    let mut i = request();
    i.expected_revision = Some("irrelevant".into());
    let mut calls = Vec::new();
    let result = apply(&i, &mut |name, args| {
        calls.push(name);
        if args["date"] == "2040-01-11" {
            return Err(failed("native list truncated"));
        }
        Ok(day(&args["date"], "v1", false))
    });
    assert!(result.is_err());
    assert_eq!(calls, vec!["mac_agenda_plan", "mac_agenda_plan"]);
}

#[test]
fn a_required_connection_blocked_on_the_last_day_prevents_any_day_being_saved() {
    let report = |date: &Value, applied| {
        let mut r = day(date, "v1", applied);
        if *date == "2040-01-12" {
            r["ready_to_apply"] = json!(false);
            r["blocking_issues"] = json!([{"task_id":"checkout","reason":"退房衔接冲突"}]);
        }
        r
    };
    let mut i = request();
    i.expected_revision = plan(&i, &mut |_, a| Ok(report(&a["date"], false))).unwrap()["revision"]
        .as_str()
        .map(str::to_owned);
    let mut writes = 0;
    assert!(
        apply(&i, &mut |name, a| {
            if name == "mac_agenda_apply" {
                writes += 1;
            }
            Ok(report(&a["date"], name == "mac_agenda_apply"))
        })
        .is_err()
    );
    assert_eq!(writes, 0);
}

#[test]
fn approval_covers_all_dates_and_rejects_any_stale_day_without_writes() {
    let mut i = request();
    assert!(apply(&i, &mut |_, _| panic!("缺少预排版本不应调用每日接口")).is_err());
    let p = plan(&i, &mut |_, a| Ok(day(&a["date"], "v1", false))).unwrap();
    i.expected_revision = Some(p["revision"].as_str().unwrap().into());
    let mut writes = 0;
    assert!(
        apply(&i, &mut |name, a| {
            if name == "mac_agenda_apply" {
                writes += 1;
            }
            Ok(day(
                &a["date"],
                if a["date"] == "2040-01-12" {
                    "v2"
                } else {
                    "v1"
                },
                false,
            ))
        })
        .is_err()
    );
    assert_eq!(writes, 0);
}

#[test]
fn sorted_days_keep_each_days_own_revision_and_verify_all_receipts() {
    let mut i = request();
    let p = plan(&i, &mut |_, a| {
        Ok(day(&a["date"], a["date"].as_str().unwrap(), false))
    })
    .unwrap();
    assert!(p["next_step"].as_str().unwrap().contains("mac_trip_apply"));
    i.expected_revision = Some(p["revision"].as_str().unwrap().into());
    let mut writes = Vec::new();
    let result = apply(&i, &mut |name, a| {
        if name == "mac_agenda_apply" {
            assert_eq!(a["expected_revision"], a["date"]);
            writes.push(a["date"].clone());
        }
        Ok(day(
            &a["date"],
            a["date"].as_str().unwrap(),
            name == "mac_agenda_apply",
        ))
    })
    .unwrap();
    assert_eq!(
        writes,
        vec![
            json!("2040-01-10"),
            json!("2040-01-11"),
            json!("2040-01-12")
        ]
    );
    assert_eq!(result["applied"], true);
    assert!(
        !result["next_step"]
            .as_str()
            .unwrap()
            .contains("mac_trip_apply")
    );
    assert_eq!(result["booking_actions_performed"], false);
}

#[test]
fn partial_failure_reports_completed_days_and_never_attempts_later_days() {
    let receipt = |date: &Value, applied| {
        let mut report = day(date, "v1", applied);
        report["entries"] = json!(vec![json!({"task":{"details":"x".repeat(1200)}}); 3]);
        report
    };
    let mut i = request();
    i.expected_revision = plan(&i, &mut |_, a| Ok(receipt(&a["date"], false))).unwrap()["revision"]
        .as_str()
        .map(str::to_owned);
    let mut written = Vec::new();
    let error = apply(&i, &mut |name, a| {
        if name == "mac_agenda_apply" {
            written.push(a["date"].clone());
            if a["date"] == "2040-01-11" {
                return Err(failed("mid-day system failure"));
            }
        }
        Ok(receipt(&a["date"], name == "mac_agenda_apply"))
    })
    .unwrap_err();
    let ToolError::Failed { detail } = error else {
        panic!("expected failure receipt")
    };
    assert!(
        detail
            .chars()
            .take(2000)
            .collect::<String>()
            .contains("degraded"),
        "台账截短前必须保留降级标记"
    );
    let v: Value = serde_json::from_str(detail.split_once('\n').unwrap().1).unwrap();
    assert_eq!(v["degraded"], true);
    assert_eq!(v["applied"], false);
    assert_eq!(v["failed_date"], "2040-01-11");
    assert_eq!(v["completed_days"].as_array().unwrap().len(), 1);
    assert_eq!(v["not_attempted_dates"], json!(["2040-01-12"]));
    assert_eq!(written.len(), 2);
}

#[test]
fn missing_or_false_completion_receipt_is_not_accepted() {
    let mut i = request();
    i.expected_revision =
        plan(&i, &mut |_, a| Ok(day(&a["date"], "v1", false))).unwrap()["revision"]
            .as_str()
            .map(str::to_owned);
    assert!(apply(&i, &mut |_, a| Ok(day(&a["date"], "v1", false))).is_err());
    assert!(
        day_report(
            &json!({"date":"2040-01-10","applied":true}),
            &json!("2040-01-10"),
            true
        )
        .is_err()
    );
    let mut blocked = day(&json!("2040-01-10"), "v1", true);
    blocked["ready_to_apply"] = json!(false);
    blocked["blocking_issues"] = json!([{"task_id":"train","reason":"衔接冲突"}]);
    assert!(day_report(&blocked, &json!("2040-01-10"), true).is_err());
}

#[test]
fn too_many_saved_tasks_stop_the_entire_preflight() {
    let i = request();
    let result = plan(&i, &mut |_, a| {
        let mut r = day(&a["date"], "v1", false);
        r["entries"] = json!(vec![json!({}); 40]);
        Ok(r)
    });
    assert!(matches!(result, Err(ToolError::BadArgs { .. })));
}

#[test]
fn trip_writes_use_existing_human_approval_boundary() {
    let ctx = ToolContext::new(std::env::temp_dir(), SandboxMode::ReadOnly);
    assert!(matches!(
        gate(
            &TripTool::new("mac_trip_apply"),
            &json!({}),
            &ToolPolicy::default(),
            &ctx,
            None
        ),
        GateDecision::Ask { .. }
    ));
    assert_eq!(
        TripTool::new("mac_trip_plan").capability(),
        Capability::ReadOnly
    );
    let policy = ToolPolicy {
        allow: vec![Rule::tool("mac_trip_apply")],
        deny: vec![Rule::tool("mac_trip_apply")],
        ..Default::default()
    };
    assert!(matches!(
        gate(
            &TripTool::new("mac_trip_apply"),
            &json!({}),
            &policy,
            &ctx,
            None
        ),
        GateDecision::Deny { .. }
    ));
}
