use super::*;

fn request() -> Input {
    input(&json!({"date":"2040-01-10"})).unwrap()
}
fn at(clock: &str) -> i64 {
    local_time(NaiveDate::from_ymd_opt(2040, 1, 10).unwrap(), clock).unwrap()
}
fn task(id: &str, minutes: u32) -> Task {
    Task {
        id: id.into(),
        title: format!("验收-{id}"),
        details: String::new(),
        source_mail: None,
        duration_minutes: minutes,
        priority: 3,
        earliest: None,
        deadline: None,
        fixed_start: None,
        travel_for: None,
        after_task: None,
        must_schedule: false,
        status: Status::Pending,
    }
}
fn empty() -> Snapshot {
    Snapshot {
        events: vec![],
        reminders: vec![],
    }
}
fn appointment(id: &str, start: &str, end: &str) -> Value {
    json!({"id":id,"revision":"v1","title":"固定约会","start":stamp(at(start)).unwrap(),"end":stamp(at(end)).unwrap(),"managed":false,"notes":"","all_day":false,"has_attendees":false})
}
fn entry<'a>(p: &'a Proposal, id: &str) -> &'a Value {
    p.report["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["task"]["id"] == id)
        .unwrap()
}
fn start(p: &Proposal, id: &str) -> i64 {
    parse_time(entry(p, id)["start"].as_str().unwrap()).unwrap()
}
fn authorize(i: &mut Input, store: &mut Store, now: i64) -> Proposal {
    let p = proposal(i, &snapshot(i, &mut |a, v| store.call(a, v)).unwrap(), now).unwrap();
    i.expected_revision = p.report["revision"].as_str().map(str::to_owned);
    p
}
#[derive(Default)]
struct Store {
    events: Vec<Value>,
    reminders: Vec<Value>,
    receipts: BTreeMap<String, (Value, Value)>,
    writes: usize,
    fail_at: Option<usize>,
    sequence: usize,
}
impl Store {
    fn call(&mut self, action: &str, args: &Value) -> Result<Value> {
        if action == "mac_calendar_list" {
            return Ok(json!({"items":self.events,"truncated":false}));
        }
        if action == "mac_reminders_list" {
            let matches = self
                .reminders
                .iter()
                .filter(|r| {
                    args["notes_prefix"]
                        .as_str()
                        .is_none_or(|p| r["notes"].as_str().unwrap_or("").starts_with(p))
                })
                .collect::<Vec<_>>();
            return Ok(json!({"items":matches,"truncated":false}));
        }
        self.writes += 1;
        if self.fail_at == Some(self.writes) {
            return Err(failed("模拟系统写入中断"));
        }
        let event = action.starts_with("mac_calendar_");
        let items = if event {
            &mut self.events
        } else {
            &mut self.reminders
        };
        self.sequence += 1;
        if action.ends_with("_create") {
            let key = args["request_id"].as_str().unwrap().to_string();
            if let Some((original, receipt)) = self.receipts.get(&key) {
                if original != args {
                    return Err(failed("request_conflict"));
                }
                return items
                    .iter()
                    .find(|i| i["id"] == receipt["item"]["id"])
                    .map(|i| json!({"item":i,"replayed":true}))
                    .ok_or_else(|| failed("not_found"));
            }
            let mut item = args.clone();
            item.as_object_mut().unwrap().remove("request_id");
            item["id"] = json!(format!("record-{}", self.sequence));
            item["revision"] = json!(self.sequence.to_string());
            item["managed"] = json!(true);
            item["content_truncated"] = json!(false);
            if event {
                item["has_attendees"] = json!(false);
                item["all_day"] = json!(false);
            } else {
                item["completed"] = json!(false);
            }
            items.push(item.clone());
            let receipt = json!({"item":item,"created":true});
            self.receipts.insert(key, (args.clone(), receipt.clone()));
            Ok(receipt)
        } else {
            let index = items
                .iter()
                .position(|i| i["id"] == args["id"])
                .ok_or_else(|| failed("not_found"))?;
            if items[index]["revision"] != args["expected_revision"] {
                return Err(failed("conflict"));
            }
            if action.ends_with("_delete") {
                let item = items.remove(index);
                return Ok(json!({"deleted_id":item["id"],"verified_absent":true}));
            }
            for (k, v) in args.as_object().unwrap() {
                if !["expected_revision", "id"].contains(&k.as_str()) {
                    items[index][k] = v.clone();
                }
            }
            items[index]["revision"] = json!(self.sequence.to_string());
            Ok(json!({"item":items[index]}))
        }
    }
}
fn run(i: &mut Input, store: &mut Store) -> Value {
    authorize(i, store, at("08:00"));
    apply(i, &mut |a, v| store.call(a, v), at("08:00")).unwrap()
}

#[test]
fn booking_details_survive_readback_and_updates_without_recreating_records() {
    let mut i = request();
    let mut t = task("hotel-checkin", 30);
    t.details = "合成酒店；已由用户确认预订；来源：合成邮件；订单TEST-123".into();
    i.tasks = vec![t];
    let mut store = Store::default();
    let first = run(&mut i, &mut store);
    let id = first["entries"][0]["reminder_id"].clone();
    let event_id = first["entries"][0]["event_id"].clone();
    let read = proposal(
        &request(),
        &snapshot(&request(), &mut |a, v| store.call(a, v)).unwrap(),
        at("08:00"),
    )
    .unwrap();
    assert_eq!(
        read.report["entries"][0]["task"]["details"],
        i.tasks[0].details
    );
    i.tasks[0].details = "合成酒店；地址已更正；订单TEST-123".into();
    let updated = run(&mut i, &mut store);
    assert_eq!(updated["entries"][0]["reminder_id"], id);
    assert_eq!(updated["entries"][0]["event_id"], event_id);
    assert_eq!(updated["entries"][0]["task"]["details"], i.tasks[0].details);
    i.tasks[0].status = Status::Completed;
    let completed = run(&mut i, &mut store);
    assert_eq!(
        completed["entries"][0]["task"]["details"],
        i.tasks[0].details
    );
}

#[test]
fn oversized_or_nul_details_are_rejected_and_old_tasks_need_no_details() {
    let mut args =
        json!({"date":"2040-01-10","tasks":[{"id":"a","title":"stay","duration_minutes":30}]});
    assert!(input(&args).unwrap().tasks[0].details.is_empty());
    for value in ["x".repeat(1201), "nul\0value".into()] {
        args["tasks"][0]["details"] = json!(value);
        assert!(input(&args).is_err());
    }
}

#[test]
fn mail_origin_survives_schedule_updates_and_is_saved_in_both_system_records() {
    let mut i = request();
    let mut t = task("mail-meeting", 60);
    t.source_mail = Some(MailLink {
        account_id: "a".into(),
        message_id: "original@example.com".into(),
    });
    i.tasks = vec![t.clone()];
    let mut store = Store::default();
    run(&mut i, &mut store);
    let event_id = store.events[0]["id"].clone();
    let reminder_id = store.reminders[0]["id"].clone();
    let saved = metadata::<SavedTask>(&store.reminders[0]).unwrap().unwrap();
    let block = metadata::<Block>(&store.events[0]).unwrap().unwrap();
    assert_eq!(saved.task.source_mail, t.source_mail);
    assert_eq!(block.source_mail, t.source_mail);
    i.tasks[0].fixed_start = Some(stamp(at("14:00")).unwrap());
    i.tasks[0].source_mail = None;
    run(&mut i, &mut store);
    assert_eq!(store.events[0]["id"], event_id);
    assert_eq!(store.reminders[0]["id"], reminder_id);
    assert_eq!(
        metadata::<SavedTask>(&store.reminders[0])
            .unwrap()
            .unwrap()
            .task
            .source_mail,
        t.source_mail
    );
    i.tasks[0].source_mail = Some(MailLink {
        account_id: "a".into(),
        message_id: "different@example.com".into(),
    });
    assert!(
        proposal(
            &i,
            &snapshot(&i, &mut |a, v| store.call(a, v)).unwrap(),
            at("08:00")
        )
        .is_err()
    );
}

#[test]
fn mail_origin_is_optional_for_old_records_and_rejects_malformed_identity() {
    let mut args =
        json!({"date":"2040-01-10","tasks":[{"id":"a","title":"meeting","duration_minutes":30}]});
    assert!(input(&args).unwrap().tasks[0].source_mail.is_none());
    for source in [
        json!({"account_id":"a","message_id":""}),
        json!({"account_id":"a","message_id":"id\nspoof"}),
        json!({"account_id":"a","message_id":"id","command":"execute"}),
    ] {
        args["tasks"][0]["source_mail"] = source;
        assert!(input(&args).is_err());
    }
}

#[test]
fn respects_appointments_lunch_deadlines_priority_and_buffer() {
    let mut i = request();
    let mut urgent = task("urgent", 30);
    urgent.deadline = Some(stamp(at("10:00")).unwrap());
    let mut high = task("high", 60);
    high.priority = 5;
    i.tasks = vec![task("low", 60), high, urgent, task("long", 120)];
    let s = Snapshot {
        events: vec![appointment("meeting", "10:00", "11:00")],
        reminders: vec![],
    };
    let p = proposal(&i, &s, at("08:00")).unwrap();
    assert_eq!(start(&p, "urgent"), at("09:00"));
    assert_eq!(start(&p, "high"), at("13:00"));
    assert_eq!(start(&p, "long"), at("14:10"));
    assert_eq!(start(&p, "low"), at("16:20"));
}
#[test]
fn ongoing_and_manual_moves_stay_put_and_append_preserves_existing_ids() {
    let mut i = request();
    i.tasks = vec![task("first", 60), task("second", 60)];
    let mut store = Store::default();
    run(&mut i, &mut store);
    let original = store.events[0].clone();
    store.events[1]["start"] = json!(stamp(at("15:00")).unwrap());
    store.events[1]["end"] = json!(stamp(at("16:00")).unwrap());
    i.tasks = vec![task("new", 30)];
    let p = proposal(
        &i,
        &snapshot(&i, &mut |a, v| store.call(a, v)).unwrap(),
        at("09:30"),
    )
    .unwrap();
    assert_eq!(p.report["entries"].as_array().unwrap().len(), 3);
    assert_eq!(entry(&p, "first")["event_id"], original["id"]);
    assert_eq!(start(&p, "first"), at("09:00"));
    assert!(entry(&p, "first")["preserved"].as_bool().unwrap());
    assert_eq!(start(&p, "second"), at("15:00"));
    assert!(start(&p, "new") >= at("10:10"));
}
#[test]
fn explicit_fixed_time_and_duration_corrections_update_the_same_records() {
    let mut i = request();
    let mut meeting = task("meeting", 60);
    meeting.fixed_start = Some(stamp(at("15:00")).unwrap());
    i.tasks = vec![meeting];
    let mut store = Store::default();
    run(&mut i, &mut store);
    let event_id = store.events[0]["id"].clone();
    let reminder_id = store.reminders[0]["id"].clone();
    i.tasks[0].fixed_start = Some(stamp(at("16:00")).unwrap());
    i.tasks[0].duration_minutes = 90;
    let out = run(&mut i, &mut store);
    assert_eq!(out["applied"], true);
    assert_eq!(store.events.len(), 1);
    assert_eq!(store.events[0]["id"], event_id);
    assert_eq!(store.reminders[0]["id"], reminder_id);
    assert_eq!(store.events[0]["start"], stamp(at("16:00")).unwrap());
    assert_eq!(store.events[0]["end"], stamp(at("17:30")).unwrap());
    let writes = store.writes;
    run(&mut i, &mut store);
    assert_eq!(store.writes, writes, "重试不能再次修改或新建");
}

#[test]
fn correcting_a_started_or_manually_moved_meeting_never_saves_inconsistent_metadata() {
    for manual in [false, true] {
        let mut i = request();
        let mut meeting = task("meeting", 60);
        meeting.fixed_start = Some(stamp(at("15:00")).unwrap());
        i.tasks = vec![meeting];
        let mut store = Store::default();
        run(&mut i, &mut store);
        if manual {
            store.events[0]["start"] = json!(stamp(at("14:00")).unwrap());
            store.events[0]["end"] = json!(stamp(at("15:00")).unwrap());
        }
        i.tasks[0].fixed_start = Some(stamp(at("16:00")).unwrap());
        let snap = snapshot(&i, &mut |a, v| store.call(a, v)).unwrap();
        assert!(proposal(&i, &snap, at(if manual { "08:00" } else { "15:30" })).is_err());
    }
}

#[test]
fn interrupted_fixed_time_correction_recovers_from_saved_metadata_without_duplicates() {
    let mut i = request();
    let mut meeting = task("meeting", 60);
    meeting.fixed_start = Some(stamp(at("15:00")).unwrap());
    i.tasks = vec![meeting];
    let mut store = Store::default();
    run(&mut i, &mut store);
    let id = store.events[0]["id"].clone();
    i.tasks[0].fixed_start = Some(stamp(at("16:00")).unwrap());
    store.fail_at = Some(store.writes + 2);
    authorize(&mut i, &mut store, at("08:00"));
    assert!(apply(&i, &mut |a, v| store.call(a, v), at("08:00")).is_err());
    store.fail_at = None;
    let mut recovered = request();
    run(&mut recovered, &mut store);
    assert_eq!(store.events.len(), 1);
    assert_eq!(store.reminders.len(), 1);
    assert_eq!(store.events[0]["id"], id);
    assert_eq!(store.events[0]["start"], stamp(at("16:00")).unwrap());
}

#[test]
fn a_conflicting_correction_keeps_the_original_meeting_and_reminder() {
    let mut i = request();
    let mut meeting = task("meeting", 60);
    meeting.fixed_start = Some(stamp(at("15:00")).unwrap());
    i.tasks = vec![meeting];
    let mut store = Store::default();
    run(&mut i, &mut store);
    store.events.push(appointment("other", "16:00", "17:00"));
    let events = store.events.clone();
    let reminders = store.reminders.clone();
    i.tasks[0].fixed_start = Some(stamp(at("16:00")).unwrap());
    let snap = snapshot(&i, &mut |a, v| store.call(a, v)).unwrap();
    let error = proposal(&i, &snap, at("08:00")).err().unwrap().to_string();
    assert!(
        error.contains("固定约会"),
        "必须给模型真实冲突证据：{error}"
    );
    assert!(error.contains(&stamp(at("16:00")).unwrap()));
    assert_eq!(store.events, events);
    assert_eq!(store.reminders, reminders);
}

#[test]
fn completion_cancel_and_reopen_do_not_get_stuck_on_deleted_create_receipts() {
    let mut i = request();
    i.tasks = vec![task("one", 60)];
    let mut store = Store::default();
    run(&mut i, &mut store);
    let rid = store.reminders[0]["id"].clone();
    i.tasks[0].status = Status::Completed;
    run(&mut i, &mut store);
    assert!(store.events.is_empty());
    assert_eq!(store.reminders[0]["completed"], true);
    i.tasks[0].status = Status::Pending;
    run(&mut i, &mut store);
    assert_eq!(store.reminders[0]["id"], rid);
    assert_eq!(store.events.len(), 1);
    i.tasks[0].status = Status::Cancelled;
    run(&mut i, &mut store);
    assert!(store.events.is_empty());
}
#[test]
fn unscheduled_tasks_remain_in_reminders_without_overbooking() {
    let mut i = request();
    i.tasks = vec![task("large", 720), task("small", 30)];
    let mut store = Store::default();
    let out = run(&mut i, &mut store);
    assert_eq!(out["unscheduled_count"], 1);
    assert_eq!(store.reminders.len(), 2);
    assert_eq!(store.events.len(), 1);
}
#[test]
fn added_urgent_item_replans_remaining_day_with_same_record_ids() {
    let mut i = request();
    i.tasks = vec![task("original", 120)];
    let mut store = Store::default();
    run(&mut i, &mut store);
    let id = store.events[0]["id"].clone();
    let mut urgent = task("urgent", 60);
    urgent.deadline = Some(stamp(at("10:00")).unwrap());
    i.tasks = vec![urgent];
    let out = run(&mut i, &mut store);
    assert_eq!(store.events.len(), 2);
    assert_eq!(out["entries"][0]["event_id"], id);
    assert_eq!(
        store.events.iter().find(|e| e["id"] == id).unwrap()["start"],
        stamp(at("13:00")).unwrap()
    );
}
#[test]
fn settings_survive_new_session_and_empty_task_list_never_clears_day() {
    let mut i = request();
    i.settings = Some(Settings {
        windows: vec![Window {
            start: "16:00".into(),
            end: "20:00".into(),
        }],
        buffer_minutes: 5,
    });
    i.tasks = vec![task("one", 30)];
    let mut store = Store::default();
    run(&mut i, &mut store);
    let next = request();
    let p = proposal(
        &next,
        &snapshot(&next, &mut |a, v| store.call(a, v)).unwrap(),
        at("08:00"),
    )
    .unwrap();
    assert_eq!(p.report["settings"]["buffer_minutes"], 5);
    assert_eq!(p.report["entries"].as_array().unwrap().len(), 1);
    assert!(p.operations.is_empty());
}
#[test]
fn retry_after_partial_write_recovers_without_duplicate_records() {
    let mut i = request();
    i.tasks = vec![task("one", 30)];
    let mut store = Store {
        fail_at: Some(2),
        ..Store::default()
    };
    authorize(&mut i, &mut store, at("08:00"));
    let e = apply(&i, &mut |a, v| store.call(a, v), at("08:00"))
        .unwrap_err()
        .to_string();
    assert!(e.contains("degraded"));
    assert_eq!(store.reminders.len(), 1);
    store.fail_at = None;
    run(&mut i, &mut store);
    assert_eq!(store.reminders.len(), 1);
    assert_eq!(store.events.len(), 1);
    let count = store.writes;
    run(&mut i, &mut store);
    assert_eq!(store.writes, count);
}
#[test]
fn snapshot_change_and_missing_approval_version_prevent_any_write() {
    let mut i = request();
    i.tasks = vec![task("one", 60)];
    let mut store = Store::default();
    assert!(apply(&i, &mut |a, v| store.call(a, v), at("08:00")).is_err());
    assert_eq!(store.writes, 0);
    authorize(&mut i, &mut store, at("08:00"));
    store.events.push(appointment("added", "09:00", "10:00"));
    assert!(apply(&i, &mut |a, v| store.call(a, v), at("08:00")).is_err());
    assert_eq!(store.writes, 0);
}
#[test]
fn malformed_or_truncated_system_data_never_becomes_empty_day() {
    let i = request();
    for data in [
        json!({"items":[],"truncated":true}),
        json!({"items":[]}),
        json!({"items":[{}],"truncated":false}),
    ] {
        assert!(snapshot(&i, &mut |_, _| Ok(data.clone())).is_err());
    }
    let mut s = empty();
    s.reminders
        .push(json!({"managed":true,"notes":"云熙日程 v1\n{invalid"}));
    assert!(proposal(&i, &s, at("08:00")).is_err());
}
#[test]
fn fixed_time_conflicts_and_all_day_appointments_are_explicit() {
    let mut i = request();
    let mut t = task("fixed", 60);
    t.fixed_start = Some(stamp(at("10:00")).unwrap());
    i.tasks = vec![t];
    let s = Snapshot {
        events: vec![appointment("meeting", "09:30", "11:30")],
        reminders: vec![],
    };
    let p = proposal(&i, &s, at("08:00")).unwrap();
    assert_eq!(p.report["unscheduled_count"], 1);
    assert_eq!(p.operations.len(), 1);
    let mut s = empty();
    let mut e = appointment("holiday", "00:00", "23:59");
    e["all_day"] = json!(true);
    s.events.push(e);
    assert_eq!(
        proposal(&i, &s, at("08:00")).unwrap().report["unscheduled_count"],
        1
    );
}
#[test]
fn validation_rejects_typos_dates_duplicates_invalid_durations_and_windows() {
    for args in [
        json!({"date":"bad"}),
        json!({"date":"2040-01-10","extra":true}),
        json!({"date":"2040-01-10","tasks":[{"id":"x","title":"a","duration_minutes":0}]}),
        json!({"date":"2040-01-10","tasks":[{"id":"../x","title":"a","duration_minutes":30}]}),
    ] {
        assert!(input(&args).is_err());
    }
    let mut i = request();
    i.tasks = vec![task("same", 10), task("same", 10)];
    assert!(day_bounds(&i).is_err());
    i.tasks.clear();
    i.settings = Some(Settings {
        windows: vec![
            Window {
                start: "09:00".into(),
                end: "12:00".into(),
            },
            Window {
                start: "11:00".into(),
                end: "13:00".into(),
            },
        ],
        buffer_minutes: 10,
    });
    assert!(proposal(&i, &empty(), at("08:00")).is_err());
}
#[test]
fn native_failure_and_false_success_cannot_report_applied() {
    let mut i = request();
    i.tasks = vec![task("one", 30)];
    let mut store = Store::default();
    authorize(&mut i, &mut store, at("08:00"));
    let e = apply(
        &i,
        &mut |a, v| {
            if a.ends_with("_create") {
                Ok(json!({"item":{"managed":true}}))
            } else {
                store.call(a, v)
            }
        },
        at("08:00"),
    )
    .unwrap_err()
    .to_string();
    assert!(e.contains("degraded"));
}
#[test]
fn elapsed_time_requires_replanning_instead_of_scheduling_into_past() {
    let mut i = request();
    i.tasks = vec![task("one", 30)];
    let mut store = Store::default();
    authorize(&mut i, &mut store, at("09:00"));
    assert!(apply(&i, &mut |a, v| store.call(a, v), at("09:06")).is_err());
    assert_eq!(store.writes, 0);
}
#[test]
fn planner_is_read_only_and_apply_always_requires_explicit_approval() {
    let plan = AgendaTool::new("mac_agenda_plan");
    let apply = AgendaTool::new("mac_agenda_apply");
    assert_eq!(plan.capability(), Capability::ReadOnly);
    assert_eq!(apply.capability(), Capability::Outbound);
}

#[test]
fn lock_is_exclusive_and_released_without_deleting_inode() {
    let path = std::env::temp_dir().join(format!("yunxi-agenda-lock-test-{}", std::process::id()));
    let first = lock_at(&path).unwrap();
    assert!(lock_at(&path).is_err());
    drop(first);
    drop(lock_at(&path).unwrap());
    std::fs::remove_file(path).unwrap();
}
#[test]
fn explicit_approval_cannot_be_replaced_by_model_or_override_deny() {
    use super::super::{GateDecision, Rule, ToolPolicy, gate};
    let ctx = ToolContext::new(std::env::temp_dir(), crate::policy::SandboxMode::ReadOnly);
    let tool = AgendaTool::new("mac_agenda_apply");
    assert!(matches!(
        gate(&tool, &json!({}), &ToolPolicy::default(), &ctx, None),
        GateDecision::Ask { .. }
    ));
    let policy = ToolPolicy {
        allow: vec![Rule::tool("mac_agenda_apply")],
        deny: vec![Rule::tool("mac_agenda_apply")],
        ..Default::default()
    };
    assert!(matches!(
        gate(&tool, &json!({}), &policy, &ctx, None),
        GateDecision::Deny { .. }
    ));
}
#[test]
fn new_conflict_after_writes_is_reported_and_not_silently_accepted() {
    let mut i = request();
    let mut t = task("fixed", 60);
    t.fixed_start = Some(stamp(at("09:00")).unwrap());
    i.tasks = vec![t];
    let mut store = Store::default();
    authorize(&mut i, &mut store, at("08:00"));
    let e = apply(
        &i,
        &mut |a, v| {
            if a == "mac_calendar_list" && store.writes > 0 {
                store.events.push(appointment("race", "09:00", "10:00"));
            }
            store.call(a, v)
        },
        at("08:00"),
    )
    .unwrap_err()
    .to_string();
    assert!(e.contains("degraded"));
    assert!(e.contains("conflicting_preserved_tasks"));
}
#[test]
fn externally_completed_reminder_preserves_past_block_without_repeating_work() {
    let mut i = request();
    i.tasks = vec![task("one", 60)];
    let mut store = Store::default();
    run(&mut i, &mut store);
    store.reminders[0]["completed"] = json!(true);
    i.tasks.clear();
    let p = proposal(
        &i,
        &snapshot(&i, &mut |a, v| store.call(a, v)).unwrap(),
        at("11:00"),
    )
    .unwrap();
    assert_eq!(entry(&p, "one")["task"]["status"], "completed");
    assert_eq!(start(&p, "one"), at("09:00"));
    assert!(
        p.operations
            .iter()
            .all(|o| !o.action.starts_with("mac_calendar_"))
    );
}

struct LiveCleanup {
    date: String,
    prefix: String,
    extra_events: Vec<String>,
    done: bool,
}
impl LiveCleanup {
    fn cleanup(&mut self) -> Result<()> {
        let i = input(&json!({"date":self.date})).unwrap();
        let snap = snapshot(&i, &mut native)?;
        for e in snap.events {
            let ours = metadata::<Block>(&e)?
                .is_some_and(|b| b.date == self.date && b.task_id.starts_with(&self.prefix))
                || self.extra_events.iter().any(|id| e["id"] == *id);
            if ours {
                native(
                    "mac_calendar_delete",
                    &json!({"id":e["id"],"expected_revision":e["revision"]}),
                )?;
            }
        }
        for r in snap.reminders {
            if metadata::<SavedTask>(&r)?
                .is_some_and(|s| s.date == self.date && s.task.id.starts_with(&self.prefix))
            {
                native(
                    "mac_reminder_delete",
                    &json!({"id":r["id"],"expected_revision":r["revision"]}),
                )?;
            }
        }
        self.done = true;
        Ok(())
    }
}
impl Drop for LiveCleanup {
    fn drop(&mut self) {
        if !self.done
            && let Err(e) = self.cleanup()
        {
            eprintln!("合成日程清理失败，prefix={}：{e}", self.prefix);
        }
    }
}
fn live_tool(name: &'static str, args: &Value) -> Value {
    let mut ctx = ToolContext::new(std::env::temp_dir(), crate::policy::SandboxMode::ReadOnly);
    let out = AgendaTool::new(name).call(args, &mut ctx).unwrap();
    serde_json::from_str(&out.text).unwrap()
}
fn live_apply(args: &mut Value) -> Value {
    args.as_object_mut().unwrap().remove("expected_revision");
    let p = live_tool("mac_agenda_plan", args);
    args["expected_revision"] = p["revision"].clone();
    let out = live_tool("mac_agenda_apply", args);
    assert_eq!(out["applied"], true);
    out
}
#[test]
#[ignore = "真实日历/待办验收：创建未来合成安排、追加、完成、恢复，最后清理，仅显式运行"]
fn live_day_plan_replan_complete_reopen_and_recover_new_session() {
    let prefix = format!("test{}", crate::now_millis().unwrap());
    let date = "2031-04-17";
    let mut cleanup = LiveCleanup {
        date: date.into(),
        prefix: prefix.clone(),
        extra_events: vec![],
        done: false,
    };
    let day = NaiveDate::parse_from_str(date, "%Y-%m-%d").unwrap();
    let fixed=native("mac_calendar_create",&json!({"title":"云熙日程验收：固定约会（自动清理）","start":stamp(local_time(day,"11:00").unwrap()).unwrap(),"end":stamp(local_time(day,"12:00").unwrap()).unwrap(),"request_id":format!("{prefix}-fixed")})).unwrap();
    cleanup
        .extra_events
        .push(fixed["item"]["id"].as_str().unwrap().into());
    let a = format!("{prefix}a");
    let b = format!("{prefix}b");
    let mut args = json!({"date":date,"tasks":[{"id":a,"title":"云熙日程验收：报告（自动清理）","duration_minutes":90}]});
    let first = live_apply(&mut args);
    let original = first["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["task"]["id"] == a)
        .unwrap()["event_id"]
        .clone();
    assert!(original.is_string());
    args = json!({"date":date,"tasks":[{"id":b,"title":"云熙日程验收：紧急事项（自动清理）","duration_minutes":30,"deadline":stamp(local_time(day,"10:00").unwrap()).unwrap()}]});
    let second = live_apply(&mut args);
    assert_eq!(second["unscheduled_count"], 0);
    let report = second["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["task"]["id"] == a)
        .unwrap();
    assert_eq!(report["event_id"], original);
    assert_eq!(
        parse_time(report["start"].as_str().unwrap()).unwrap(),
        local_time(day, "13:00").unwrap()
    );
    let mut done = report["task"].clone();
    done["status"] = json!("completed");
    args = json!({"date":date,"tasks":[done]});
    let third = live_apply(&mut args);
    assert_eq!(
        third["entries"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["task"]["id"] == a)
            .unwrap()["event_id"],
        Value::Null
    );
    let recovered = live_tool("mac_agenda_plan", &json!({"date":date}));
    assert!(recovered["changes"].as_array().unwrap().is_empty());
    assert_eq!(
        recovered["entries"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["task"]["id"].as_str().unwrap().starts_with(&prefix))
            .count(),
        2
    );
    args["tasks"][0]["status"] = json!("pending");
    let reopened = live_apply(&mut args);
    assert!(
        reopened["entries"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["task"]["id"] == a)
            .unwrap()["event_id"]
            .is_string()
    );
    let c = format!("{prefix}c");
    args = json!({"date":date,"tasks":[{"id":c,"title":"云熙日程验收：改时会议（自动清理）","duration_minutes":60,"fixed_start":stamp(local_time(day,"15:00").unwrap()).unwrap()}]});
    let before_correction = live_apply(&mut args);
    let original_meeting = before_correction["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["task"]["id"] == c)
        .unwrap();
    args["tasks"][0]["fixed_start"] = json!(stamp(local_time(day, "16:00").unwrap()).unwrap());
    args["tasks"][0]["duration_minutes"] = json!(90);
    let corrected = live_apply(&mut args);
    let meeting = corrected["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["task"]["id"] == c)
        .unwrap();
    assert_eq!(meeting["event_id"], original_meeting["event_id"]);
    assert_eq!(meeting["reminder_id"], original_meeting["reminder_id"]);
    assert_eq!(
        meeting["start"],
        stamp(local_time(day, "16:00").unwrap()).unwrap()
    );
    assert_eq!(
        meeting["end"],
        stamp(local_time(day, "17:30").unwrap()).unwrap()
    );
    let fixed_after = snapshot(&input(&json!({"date":date})).unwrap(), &mut native).unwrap();
    assert_eq!(
        fixed_after
            .events
            .iter()
            .find(|e| e["id"] == fixed["item"]["id"])
            .unwrap()["revision"],
        fixed["item"]["revision"]
    );
    cleanup.cleanup().unwrap();
    let cleaned = snapshot(&input(&json!({"date":date})).unwrap(), &mut native).unwrap();
    assert!(
        !cleaned
            .events
            .iter()
            .chain(cleaned.reminders.iter())
            .any(|i| i["notes"].as_str().unwrap_or("").contains(&prefix))
    );
}

#[test]
fn varied_day_plans_never_overlap_or_escape_windows_and_deadlines() {
    for seed in 0..60_u32 {
        let mut i = request();
        i.tasks = (0..8)
            .map(|n| {
                let mut t = task(&format!("t{n}"), 10 + (seed * 37 + n * 53) % 180);
                t.priority = (1 + (seed + n) % 5) as u8;
                if n % 3 == 0 {
                    t.deadline = Some(stamp(at("15:00")).unwrap());
                }
                t
            })
            .collect();
        let mut snap = empty();
        snap.events.push(appointment("fixed", "10:30", "11:15"));
        let p = proposal(&i, &snap, at("09:17")).unwrap();
        let mut occupied = vec![Slot::from_item(&snap.events[0]).unwrap()];
        for e in p.report["entries"].as_array().unwrap() {
            if e["start"].is_null() {
                continue;
            }
            let slot = Slot {
                start: parse_time(e["start"].as_str().unwrap()).unwrap(),
                end: parse_time(e["end"].as_str().unwrap()).unwrap(),
            };
            assert!(slot.start >= at("09:17"));
            assert!(
                (slot.start >= at("09:00") && slot.end <= at("12:00"))
                    || (slot.start >= at("13:00") && slot.end <= at("18:00"))
            );
            assert!(!occupied.iter().any(|b| slot.overlaps(b, 600)));
            if let Some(deadline) = e["task"]["deadline"].as_str() {
                assert!(slot.end <= parse_time(deadline).unwrap());
            }
            occupied.push(slot);
        }
    }
}

fn travel_request() -> Input {
    input(&json!({"date":"2040-01-10","tasks":[
        {"id":"meeting","title":"会议","duration_minutes":60,"fixed_start":stamp(at("15:00")).unwrap()},
        {"id":"travel","title":"出发准备及通勤","duration_minutes":40,"travel_for":"meeting"}
    ]})).unwrap()
}

#[test]
fn trip_connections_reorder_existing_flexible_work_and_keep_external_appointments() {
    let mut store = Store::default();
    store.events.push(appointment("external", "09:00", "10:00"));
    let mut original = request();
    original.tasks = vec![task("work", 90)];
    run(&mut original, &mut store);
    let work_id = store
        .events
        .iter()
        .find(|e| e["title"] == "验收-work")
        .unwrap()["id"]
        .clone();
    let mut journey = input(&json!({"date":"2040-01-10","tasks":[
        {"id":"train","title":"列车","duration_minutes":60,"fixed_start":stamp(at("11:00")).unwrap(),"must_schedule":true},
        {"id":"prep","title":"准备出发","duration_minutes":30,"travel_for":"train","must_schedule":true},
        {"id":"hotel","title":"入住","duration_minutes":30,"earliest":stamp(at("13:00")).unwrap(),"after_task":"train","must_schedule":true}
    ]})).unwrap();
    let p = authorize(&mut journey, &mut store, at("08:00"));
    assert_eq!(p.report["ready_to_apply"], true);
    assert_eq!(start(&p, "prep"), at("10:20"));
    assert_eq!(start(&p, "hotel"), at("13:00"));
    assert_eq!(start(&p, "work"), at("13:40"));
    assert_eq!(
        entry(&p, "work")["previous_start"],
        stamp(at("10:10")).unwrap()
    );
    run(&mut journey, &mut store);
    assert_eq!(
        store
            .events
            .iter()
            .find(|e| e["title"] == "验收-work")
            .unwrap()["id"],
        work_id
    );
    assert_eq!(
        store.events.iter().find(|e| e["id"] == "external").unwrap(),
        &appointment("external", "09:00", "10:00")
    );
}

#[test]
fn required_unscheduled_item_prevents_all_writes_but_optional_backlog_still_saves() {
    let mut store = Store::default();
    store.events.push(appointment("external", "09:00", "18:00"));
    let mut i = input(&json!({"date":"2040-01-10","tasks":[
        {"id":"train","title":"必须赶上列车","duration_minutes":60,"fixed_start":stamp(at("11:00")).unwrap(),"must_schedule":true}
    ]})).unwrap();
    let p = authorize(&mut i, &mut store, at("08:00"));
    assert_eq!(p.report["ready_to_apply"], false);
    assert!(p.report["next_step"].as_str().unwrap().contains("不能保存"));
    assert_eq!(p.report["blocking_issues"][0]["task_id"], "train");
    assert!(apply(&i, &mut |a, v| store.call(a, v), at("08:00")).is_err());
    assert_eq!(store.writes, 0);
    let mut optional = request();
    optional.tasks = vec![task("work", 60)];
    assert_eq!(run(&mut optional, &mut store)["unscheduled_count"], 1);
}

#[test]
fn plan_explains_approval_handoff_but_verified_apply_does_not_request_another_write() {
    let mut i = request();
    i.tasks = vec![task("work", 30)];
    let mut store = Store::default();
    let p = authorize(&mut i, &mut store, at("08:00"));
    assert_eq!(p.report["applied"], false);
    assert!(
        p.report["next_step"]
            .as_str()
            .unwrap()
            .contains("mac_agenda_apply")
    );
    assert_eq!(store.writes, 0);
    let saved = run(&mut i, &mut store);
    assert_eq!(saved["applied"], true);
    assert!(
        !saved["next_step"]
            .as_str()
            .unwrap()
            .contains("mac_agenda_apply")
    );
}

#[test]
fn dependency_order_survives_new_session_and_tracks_upstream_duration() {
    let mut i=input(&json!({"date":"2040-01-10","tasks":[
        {"id":"z-source","title":"到达","duration_minutes":30,"earliest":stamp(at("13:00")).unwrap()},
        {"id":"a-next","title":"到达后取行李","duration_minutes":20,"after_task":"z-source","must_schedule":true}
    ]})).unwrap();
    let mut store = Store::default();
    run(&mut i, &mut store);
    i.tasks.truncate(1);
    i.tasks[0].duration_minutes = 60;
    let p = authorize(&mut i, &mut store, at("08:00"));
    assert_eq!(start(&p, "a-next"), at("14:10"));
    let saved = run(&mut i, &mut store);
    assert_eq!(saved["ready_to_apply"], true);
    let fresh = proposal(
        &request(),
        &snapshot(&request(), &mut |a, v| store.call(a, v)).unwrap(),
        at("08:00"),
    )
    .unwrap();
    assert_eq!(entry(&fresh, "a-next")["task"]["after_task"], "z-source");
    i.tasks[0].status = Status::Completed;
    let p = authorize(&mut i, &mut store, at("08:00"));
    assert_eq!(start(&p, "a-next"), at("09:00"));
    assert_eq!(run(&mut i, &mut store)["ready_to_apply"], true);
}

#[test]
fn invalid_dependency_chains_and_cancelled_origin_are_never_guessed() {
    for tasks in [
        json!([{"id":"a","title":"a","duration_minutes":30,"after_task":"missing"}]),
        json!([{"id":"a","title":"a","duration_minutes":30,"after_task":"a"}]),
        json!([{"id":"a","title":"a","duration_minutes":30,"after_task":"b"},{"id":"b","title":"b","duration_minutes":30,"after_task":"a"}]),
    ] {
        let i = input(&json!({"date":"2040-01-10","tasks":tasks})).unwrap();
        assert!(proposal(&i, &empty(), at("08:00")).is_err());
    }
    let mut i = input(&json!({"date":"2040-01-10","tasks":[
        {"id":"a","title":"原起点","duration_minutes":30},
        {"id":"b","title":"后续行程","duration_minutes":30,"after_task":"a"}
    ]}))
    .unwrap();
    let mut store = Store::default();
    run(&mut i, &mut store);
    let before = store.writes;
    i.tasks.truncate(1);
    i.tasks[0].status = Status::Cancelled;
    assert!(
        proposal(
            &i,
            &snapshot(&i, &mut |a, v| store.call(a, v)).unwrap(),
            at("08:00")
        )
        .is_err()
    );
    assert_eq!(store.writes, before);
}

#[test]
fn repeating_derived_departure_without_fixed_start_does_not_fake_a_reschedule() {
    let mut i = input(&json!({"date":"2040-01-10","tasks":[
        {"id":"train","title":"列车","duration_minutes":30,"fixed_start":stamp(at("13:00")).unwrap(),"must_schedule":true},
        {"id":"hotel","title":"入住","duration_minutes":30,"fixed_start":stamp(at("15:00")).unwrap(),"must_schedule":true},
        {"id":"transfer","title":"去酒店","duration_minutes":40,"travel_for":"hotel","after_task":"train","must_schedule":true}
    ]})).unwrap();
    let mut store = Store::default();
    run(&mut i, &mut store);
    // 固定开始由系统推导；重复原参数时，即使通勤已开始也不能误认作改期。
    let p = proposal(
        &i,
        &snapshot(&i, &mut |a, v| store.call(a, v)).unwrap(),
        at("14:20"),
    )
    .unwrap();
    assert!(p.operations.is_empty());
    i.tasks[0].fixed_start = Some(stamp(at("16:00")).unwrap());
    let before = store.writes;
    let p = authorize(&mut i, &mut store, at("08:00"));
    assert_eq!(p.report["ready_to_apply"], false);
    assert!(entry(&p, "transfer")["preserved"].as_bool().unwrap());
    assert!(apply(&i, &mut |a, v| store.call(a, v), at("08:00")).is_err());
    assert_eq!(store.writes, before);
}

#[test]
fn delayed_arrival_blocks_fixed_connection_and_external_conflicts_are_not_ignored() {
    let mut i=input(&json!({"date":"2040-01-10","tasks":[
        {"id":"arrival","title":"到达","duration_minutes":30,"fixed_start":stamp(at("13:00")).unwrap(),"must_schedule":true},
        {"id":"connection","title":"后续活动","duration_minutes":30,"fixed_start":stamp(at("14:00")).unwrap(),"after_task":"arrival","must_schedule":true}
    ]})).unwrap();
    let mut store = Store::default();
    run(&mut i, &mut store);
    let before = store.writes;
    i.tasks.truncate(1);
    i.tasks[0].fixed_start = Some(stamp(at("15:00")).unwrap());
    let p = authorize(&mut i, &mut store, at("08:00"));
    assert_eq!(p.report["ready_to_apply"], false);
    assert!(apply(&i, &mut |a, v| store.call(a, v), at("08:00")).is_err());
    assert_eq!(store.writes, before);
    store
        .events
        .push(appointment("new-conflict", "14:00", "15:00"));
    let p = proposal(
        &request(),
        &snapshot(&request(), &mut |a, v| store.call(a, v)).unwrap(),
        at("08:00"),
    )
    .unwrap();
    assert_eq!(p.report["ready_to_apply"], false);
}

#[test]
fn travel_follows_meeting_corrections_and_cancellation_across_sessions() {
    let mut i = travel_request();
    let mut store = Store::default();
    let p = authorize(&mut i, &mut store, at("08:00"));
    assert_eq!(start(&p, "travel"), at("14:10"));
    run(&mut i, &mut store);
    let travel_id = store
        .events
        .iter()
        .find(|e| e["title"] == "出发准备及通勤")
        .unwrap()["id"]
        .clone();
    i.tasks.truncate(1);
    i.tasks[0].fixed_start = Some(stamp(at("16:00")).unwrap());
    run(&mut i, &mut store);
    let event = store.events.iter().find(|e| e["id"] == travel_id).unwrap();
    assert_eq!(event["start"], stamp(at("15:10")).unwrap());
    i.tasks[0].status = Status::Cancelled;
    run(&mut i, &mut store);
    assert!(store.events.is_empty());
    let p = proposal(
        &request(),
        &snapshot(&i, &mut |a, v| store.call(a, v)).unwrap(),
        at("08:00"),
    )
    .unwrap();
    assert_eq!(entry(&p, "travel")["task"]["status"], "cancelled");
}

#[test]
fn invalid_or_unplaceable_travel_never_leaves_an_orphan_departure() {
    let mut i = travel_request();
    let mut snap = empty();
    snap.events.push(appointment("conflict", "14:00", "14:30"));
    assert!(proposal(&i, &snap, at("08:00")).is_err());
    i.tasks.remove(0);
    assert!(proposal(&i, &empty(), at("08:00")).is_err());
}

#[test]
fn started_travel_cannot_be_moved_by_changing_its_meeting() {
    let mut i = travel_request();
    let mut store = Store::default();
    run(&mut i, &mut store);
    i.tasks.truncate(1);
    i.tasks[0].fixed_start = Some(stamp(at("16:00")).unwrap());
    let snap = snapshot(&i, &mut |a, v| store.call(a, v)).unwrap();
    assert!(proposal(&i, &snap, at("14:20")).is_err());
}

#[test]
#[ignore = "真实日历/待办验收：合成会议及通勤绑定，改期和取消后清理"]
fn live_travel_moves_with_meeting_and_cleans_up() {
    let prefix = format!("travel{}", crate::now_millis().unwrap());
    let date = "2031-04-19";
    let mut cleanup = LiveCleanup {
        date: date.into(),
        prefix: prefix.clone(),
        extra_events: vec![],
        done: false,
    };
    let meeting = format!("{prefix}m");
    let travel = format!("{prefix}t");
    let mut args = json!({"date":date,"tasks":[
        {"id":meeting,"title":format!("{prefix}会议"),"duration_minutes":60,"fixed_start":"2031-04-19T15:00:00+08:00"},
        {"id":travel,"title":format!("{prefix}准备通勤"),"duration_minutes":40,"travel_for":meeting}
    ]});
    let original = live_apply(&mut args);
    let old = original["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["task"]["id"] == travel)
        .unwrap();
    let old_id = old["event_id"].clone();
    args["tasks"].as_array_mut().unwrap().truncate(1);
    args["tasks"][0]["fixed_start"] = json!("2031-04-19T16:00:00+08:00");
    let updated = live_apply(&mut args);
    let new = updated["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["task"]["id"] == travel)
        .unwrap();
    assert_eq!(new["event_id"], old_id);
    assert_eq!(
        parse_time(new["start"].as_str().unwrap()).unwrap(),
        parse_time("2031-04-19T15:10:00+08:00").unwrap()
    );
    args["tasks"][0]["status"] = json!("cancelled");
    let cancelled = live_apply(&mut args);
    assert!(
        cancelled["entries"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["task"]["id"].as_str().unwrap().starts_with(&prefix))
            .all(|e| e["task"]["status"] == "cancelled")
    );
    cleanup.cleanup().unwrap();
}
