use super::*;
fn location() -> Value {
    json!({"account_id":"a","mailbox_path":["INBOX"],"id":"7"})
}
fn thread() -> Value {
    json!({"items":[{"account_id":"a","mailbox_path":["INBOX"],"id":"7","message_id":"new@example.com","subject":"改期"},{"account_id":"a","mailbox_path":["Archive"],"id":"2","message_id":"old@example.com","subject":"原安排"}],"truncated":false,"complete_in_account":true})
}
fn reminder(account: &str, message: &str) -> Value {
    let saved = json!({"date":"2040-01-10","settings":{"windows":[{"start":"09:00","end":"18:00"}],"buffer_minutes":10},"generation":0,
        "task":{"id":"meeting","title":"旧标题","duration_minutes":60,"source_mail":{"account_id":account,"message_id":message}}});
    json!({"id":"reminder-id","revision":"r1","title":"系统中的新标题","managed":true,"completed":true,"notes":format!("云熙日程 v1\n{saved}")})
}
#[test]
fn followup_finds_original_task_after_archive_and_reads_actual_completion() {
    let mut calls = Vec::new();
    let value=context(&location(),&mut |action,_| {
        calls.push(action.to_string());
        Ok(match action {
            "mac_mail_thread"=>thread(),
            "mac_reminders_list"=>json!({"items":[reminder("a","old@example.com"),reminder("b","old@example.com"),reminder("a","unrelated@example.com")],"truncated":false}),
            _=>panic!("关联查询不得写入"),
        })
    }).unwrap();
    assert_eq!(value["linked_tasks"].as_array().unwrap().len(), 1);
    assert_eq!(value["linked_tasks"][0]["task"]["status"], "completed");
    assert_eq!(value["linked_tasks"][0]["task"]["title"], "系统中的新标题");
    assert_eq!(
        value["linked_tasks"][0]["task"]["source_mail"]["message_id"],
        "old@example.com"
    );
    assert_eq!(value["source_mail"]["message_id"], "new@example.com");
    assert_eq!(value["body_read_required"], true);
    assert_eq!(calls, vec!["mac_mail_thread", "mac_reminders_list"]);
}
#[test]
fn incomplete_or_mismatched_thread_never_claims_no_existing_schedule() {
    for (field, value) in [
        ("truncated", json!(true)),
        ("complete_in_account", json!(false)),
        ("items", json!([])),
    ] {
        let mut t = thread();
        t[field] = value;
        assert!(
            context(&location(), &mut |action, _| {
                assert_eq!(action, "mac_mail_thread");
                Ok(t.clone())
            })
            .is_err()
        );
    }
    for field in ["account_id", "message_id"] {
        let mut t = thread();
        t["items"][1][field] = json!("");
        assert!(context(&location(), &mut |_, _| Ok(t.clone())).is_err());
    }
    assert!(
        context(&location(), &mut |action, _| Ok(
            if action == "mac_mail_thread" {
                thread()
            } else {
                json!({"items":[],"truncated":true})
            }
        ))
        .is_err()
    );
}
#[test]
fn malformed_locations_do_not_call_native_or_accept_instructions() {
    for args in [
        json!({}),
        json!({"account_id":"a","mailbox_path":["INBOX"],"id":"7","execute":"delete"}),
        json!({"account_id":"a","mailbox_path":[".."],"id":"7"}),
    ] {
        assert!(context(&args, &mut |_, _| panic!("invalid input")).is_err());
    }
}
#[test]
fn corrupt_managed_notes_fail_closed_and_other_reminders_are_ignored() {
    let mut r = reminder("a", "old@example.com");
    r["notes"] = json!("云熙日程 v1\n{broken}");
    assert!(
        agenda::linked_mail_tasks(&[r], "a", &BTreeSet::from(["old@example.com".into()])).is_err()
    );
    assert!(
        agenda::linked_mail_tasks(
            &[json!({"managed":false,"notes":"personal"})],
            "a",
            &BTreeSet::new()
        )
        .unwrap()
        .is_empty()
    );
}
