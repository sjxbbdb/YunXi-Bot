use super::*;
use crate::think::{ThinkError, ThinkResponse, Usage};
use std::sync::atomic::{AtomicUsize, Ordering};
const NOW: u64 = 1_792_200_000_000;
static SEQ: AtomicUsize = AtomicUsize::new(0);
struct Home(PathBuf);
impl Home {
    fn new() -> Self {
        let p = std::env::temp_dir().join(format!(
            "yunxi-reply-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&p).unwrap();
        Self(p)
    }
}
impl Drop for Home {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn item() -> InfoItem {
    InfoItem {
        source: "mac_mail".into(),
        id: json!({"account_id":"a","mailbox_path":["INBOX"],"id":"1"}).to_string(),
        from_name: "同事".into(),
        from_addr: "colleague@example.com".into(),
        subject: "进展".into(),
        preview: String::new(),
        received_at_ms: NOW,
        direct: true,
        recipient_count: 1,
        addressed_directly: true,
        has_attachments: false,
    }
}
fn mail() -> Value {
    json!({"account_id":"a","mailbox_path":["INBOX"],"id":"1","message_id":"x@example.com","sender":"同事 <colleague@example.com>","to":["me@example.com"],"cc":[],"subject":"进展","body":"收到资料，请确认收悉。","attachments":[],"read":false,"revision":"v1","total_characters":12,"next_offset":null})
}
fn call(action: &str, _: &Value) -> Result<Value, String> {
    Ok(match action {
        "mac_mail_read" => {
            let mut m = mail();
            m["total_characters"] = json!(m["body"].as_str().unwrap().chars().count());
            m
        }
        "mac_mail_thread" => json!({"truncated":false,"complete_in_account":true,"items":[mail()]}),
        "mac_calendar_list" => {
            json!({"truncated":false,"items":[{"title":"不应泄露的会议","notes":"秘密备注","start":"2026-10-17T14:00:00+08:00","end":"2026-10-17T15:00:00+08:00","all_day":false}]})
        }
        "mac_reminders_list" => {
            json!({"truncated":false,"items":[{"completed":false,"due_at":"2026-10-17T13:00:00+08:00","title":"保密任务"},{"completed":false,"due_at":null}]})
        }
        "mac_mail_accounts" => {
            json!({"configured":true,"accounts":[{"id":"a","addresses":["me@example.com"]}]})
        }
        "mac_mail_draft" => json!({"draft_id":"d1","sent":false,"revision":"draft-v1"}),
        "mac_mail_outbox" => json!({"draft_id":"d1","status":"handed_off"}),
        other => panic!("未授权工具被调用：{other}"),
    })
}
fn reply() -> Reply {
    Reply {
        status: "ready".into(),
        body: "资料已收到，谢谢。".into(),
        reason: "只确认已收悉，不承诺其他事项。".into(),
        questions: vec![],
    }
}
struct Model {
    calls: AtomicUsize,
    output: String,
    tools: bool,
    finish: &'static str,
}
impl Model {
    fn new() -> Self {
        Self {
            calls: AtomicUsize::new(0),
            output: serde_json::to_string(&reply()).unwrap(),
            tools: false,
            finish: "stop",
        }
    }
}
impl Thinker for Model {
    fn model(&self) -> &str {
        "test-writer"
    }
    fn think(&self, r: &ThinkRequest) -> Result<ThinkResponse, ThinkError> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        assert!(r.tools.is_empty());
        assert!(r.json_output);
        assert_eq!(r.messages.len(), 2);
        assert!(r.messages[0].content.starts_with(SEND_SYSTEM));
        assert!(!r.messages[1].content.contains("不应泄露"));
        assert!(!r.messages[1].content.contains("秘密备注"));
        assert!(!r.messages[1].content.contains("保密任务"));
        Ok(ThinkResponse {
            content: self.output.clone(),
            model: self.model().into(),
            usage: Usage::default(),
            finish_reason: Some(self.finish.into()),
            reasoning: None,
            thinking: Thinking::Enabled,
            tool_calls: if self.tools {
                vec![json!({"name":"mac_mail_send"})]
            } else {
                vec![]
            },
        })
    }
}
fn generate(h: &Home, m: &Model) -> Result<(), String> {
    set_mode(&h.0, "review", NOW - 1)?;
    let mut ledger = Ledger::open(h.0.join("ledger.jsonl")).unwrap();
    run_with(
        &h.0,
        vec![(item(), mail()["body"].as_str().unwrap().into())],
        Ok(m),
        &mut ledger,
        NOW,
        &mut call,
    )
}
#[test]
fn off_and_historical_mail_never_read_context_or_call_model() {
    let h = Home::new();
    let m = Model::new();
    let mut ledger = Ledger::open(h.0.join("ledger.jsonl")).unwrap();
    run_with(
        &h.0,
        vec![(item(), "".into())],
        Ok(&m),
        &mut ledger,
        NOW,
        &mut |_, _| panic!("disabled"),
    )
    .unwrap();
    set_mode(&h.0, "review", NOW + 1).unwrap();
    run_with(
        &h.0,
        vec![(item(), "".into())],
        Ok(&m),
        &mut ledger,
        NOW + 2,
        &mut |_, _| panic!("historical"),
    )
    .unwrap();
    assert_eq!(m.calls.load(Ordering::Relaxed), 0);
    assert_eq!(recent(&h.0, None).unwrap()["total"], 0);
}
#[test]
fn profile_and_planning_are_used_without_private_event_details() {
    let h = Home::new();
    fs::write(h.0.join("profile.md"), "表达简短，称呼小林").unwrap();
    let (ctx, _) = context(&h.0, &item(), NOW, None, &mut call).unwrap();
    assert!(ctx["profile"].as_str().unwrap().contains("小林"));
    assert_eq!(ctx["plans"]["busy"].as_array().unwrap().len(), 1);
    assert_eq!(ctx["plans"]["undated_reminders"], 1);
    assert!(!ctx.to_string().contains("保密"));
    generate(&h, &Model::new()).unwrap();
    assert_eq!(
        recent(&h.0, None).unwrap()["items"][0]["reply"]["status"],
        "ready"
    );
}
#[test]
fn restart_deduplicates_success_and_never_writes_native_drafts_in_background() {
    let h = Home::new();
    let m = Model::new();
    generate(&h, &m).unwrap();
    generate(&h, &m).unwrap();
    assert_eq!(m.calls.load(Ordering::Relaxed), 1);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(h.0.join("mail-replies.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
    assert!(
        load(&h.0)
            .unwrap()
            .proposals
            .values()
            .all(|p| p.draft_id.is_none())
    );
}
#[test]
fn invalid_json_tool_calls_and_incomplete_generation_fail_closed_and_back_off() {
    for case in 0..3 {
        let h = Home::new();
        let mut m = Model::new();
        match case {
            0 => m.output = "not json".into(),
            1 => m.tools = true,
            _ => m.finish = "length",
        }
        assert!(generate(&h, &m).is_err());
        generate(&h, &m).unwrap();
        assert_eq!(m.calls.load(Ordering::Relaxed), 1);
        let view = recent(&h.0, None).unwrap();
        assert_eq!(view["items"][0]["degraded"], true);
        assert!(view["items"][0]["reply"].is_null());
    }
}
#[test]
fn output_schema_enforces_states_and_rejects_unknown_fields() {
    for value in [
        json!({"status":"ready","body":"","reason":"r","questions":[]}),
        json!({"status":"needs_input","body":"body","reason":"r","questions":[]}),
        json!({"status":"no_reply","body":"body","reason":"r","questions":[]}),
        json!({"status":"ready","body":"body","reason":"r","questions":[],"send":true}),
    ] {
        assert!(Reply::parse(&value.to_string()).is_err());
    }
    let mut r = reply();
    r.body = "\u{202e}".into();
    assert!(Reply::parse(&serde_json::to_string(&r).unwrap()).is_err());
}
#[test]
fn incomplete_context_or_changed_body_never_reaches_model() {
    for action in [
        "mac_mail_thread",
        "mac_calendar_list",
        "mac_reminders_list",
        "mac_mail_read",
    ] {
        let h = Home::new();
        set_mode(&h.0, "review", NOW - 1).unwrap();
        let m = Model::new();
        let mut ledger = Ledger::open(h.0.join("ledger.jsonl")).unwrap();
        assert!(
            run_with(
                &h.0,
                vec![(item(), "wrong body".into())],
                Ok(&m),
                &mut ledger,
                NOW,
                &mut |a, v| {
                    let mut r = call(a, v)?;
                    if a == action {
                        r["truncated"] = json!(true);
                        if a == "mac_mail_read" {
                            r["total_characters"] = json!(40_000);
                        }
                    }
                    Ok(r)
                }
            )
            .is_err()
        );
        assert_eq!(m.calls.load(Ordering::Relaxed), 0);
    }
}
#[test]
fn malformed_profile_and_oversized_thread_are_not_silently_omitted() {
    let h = Home::new();
    fs::write(h.0.join("profile.md"), [0xff]).unwrap();
    assert!(context(&h.0, &item(), NOW, None, &mut call).is_err());
    fs::write(h.0.join("profile.md"), "x".repeat(8193)).unwrap();
    assert!(context(&h.0, &item(), NOW, None, &mut call).is_err());
    fs::remove_file(h.0.join("profile.md")).unwrap();
    assert!(
        context(&h.0, &item(), NOW, None, &mut |a, v| {
            let mut r = call(a, v)?;
            if a == "mac_mail_thread" {
                r["items"] = json!(vec![mail(); 11]);
            }
            Ok(r)
        })
        .is_err()
    );
}
#[test]
fn budget_is_saved_before_call_and_failures_stop_after_three_attempts() {
    let h = Home::new();
    let mut m = Model::new();
    m.output = "invalid".into();
    assert!(generate(&h, &m).is_err());
    assert_eq!(load(&h.0).unwrap().calls, 1);
    let mut ledger = Ledger::open(h.0.join("ledger.jsonl")).unwrap();
    for n in 1..=4 {
        let _ = run_with(
            &h.0,
            vec![(item(), mail()["body"].as_str().unwrap().into())],
            Ok(&m),
            &mut ledger,
            NOW + n * RETRY_MS,
            &mut call,
        );
    }
    assert_eq!(m.calls.load(Ordering::Relaxed), 3);
    let mut s = Store::open(&h.0).unwrap();
    s.state.calls = 30;
    s.state.proposals.get_mut(&item().id).unwrap().attempts = 0;
    s.state.proposals.get_mut(&item().id).unwrap().next_ms = 0;
    s.save().unwrap();
    drop(s);
    assert!(
        run_with(
            &h.0,
            vec![(item(), "".into())],
            Ok(&m),
            &mut ledger,
            NOW + 5 * RETRY_MS,
            &mut |_, _| panic!("budget exhausted")
        )
        .is_err()
    );
}
#[test]
fn prepare_rechecks_context_and_reuses_the_existing_draft() {
    let h = Home::new();
    generate(&h, &Model::new()).unwrap();
    let mut drafts = 0;
    let mut calls = |a: &str, v: &Value| {
        if a == "mac_mail_draft" {
            drafts += 1;
            assert_eq!(v["to"], json!(["colleague@example.com"]));
            assert_eq!(v["sender"], "me@example.com");
            assert_eq!(v["attachments"], json!([]));
            assert_eq!(v["source"]["expected_revision"], "v1");
        }
        call(a, v)
    };
    assert_eq!(
        prepare_with(&h.0, &item().id, NOW, NOW + 1, &mut calls).unwrap()["sent"],
        false
    );
    assert_eq!(
        prepare_with(&h.0, &item().id, NOW, NOW + 2, &mut calls).unwrap()["status"],
        "handed_off"
    );
    assert_eq!(drafts, 1);
}
#[test]
fn stale_version_expired_context_or_changed_plans_cannot_create_drafts() {
    let h = Home::new();
    generate(&h, &Model::new()).unwrap();
    assert!(
        prepare_with(&h.0, &item().id, NOW + 1, NOW + 1, &mut |_, _| panic!(
            "wrong version"
        ))
        .is_err()
    );
    assert!(
        prepare_with(&h.0, &item().id, NOW, NOW + DAY_MS + 1, &mut |_, _| panic!(
            "expired"
        ))
        .is_err()
    );
    assert!(
        prepare_with(&h.0, &item().id, NOW, NOW + 1, &mut |a, v| {
            assert_ne!(a, "mac_mail_draft");
            let mut r = call(a, v)?;
            if a == "mac_calendar_list" {
                r["items"] = json!([]);
            }
            Ok(r)
        })
        .is_err()
    );
    fs::write(h.0.join("profile.md"), "新的偏好").unwrap();
    assert!(prepare_with(&h.0, &item().id, NOW, NOW + 1, &mut call).is_err());
}
#[test]
fn read_state_change_is_not_a_change_of_facts() {
    let h = Home::new();
    generate(&h, &Model::new()).unwrap();
    prepare_with(&h.0, &item().id, NOW, NOW + 1, &mut |a, v| {
        let mut r = call(a, v)?;
        if a == "mac_mail_read" {
            r["read"] = json!(true);
            r["revision"] = json!("v2");
        }
        if a == "mac_mail_draft" {
            assert_eq!(v["source"]["expected_revision"], "v2");
        }
        Ok(r)
    })
    .unwrap();
}
#[test]
fn unknown_draft_result_reuses_the_saved_request_on_retry() {
    let h = Home::new();
    generate(&h, &Model::new()).unwrap();
    let mut first = None;
    assert!(
        prepare_with(&h.0, &item().id, NOW, NOW + 1, &mut |a, v| {
            if a == "mac_mail_draft" {
                first = Some(v.clone());
                return Err("unknown".into());
            }
            call(a, v)
        })
        .is_err()
    );
    prepare_with(&h.0, &item().id, NOW, NOW + 2, &mut |a, v| {
        assert_eq!(a, "mac_mail_draft");
        assert_eq!(Some(v), first.as_ref());
        call(a, v)
    })
    .unwrap();
}
#[test]
fn questions_and_disabled_mode_cannot_prepare() {
    let h = Home::new();
    let mut m = Model::new();
    m.output =
        json!({"status":"needs_input","body":"","reason":"需要决定","questions":["参加吗？"]})
            .to_string();
    generate(&h, &m).unwrap();
    assert!(
        prepare_with(&h.0, &item().id, NOW, NOW + 1, &mut |_, _| panic!(
            "needs input"
        ))
        .is_err()
    );
    set_mode(&h.0, "off", NOW + 2).unwrap();
    assert!(prepare_with(&h.0, &item().id, NOW, NOW + 3, &mut |_, _| panic!("off")).is_err());
}
#[test]
fn corrupt_state_and_parallel_mutation_fail_closed() {
    let h = Home::new();
    let store = Store::open(&h.0).unwrap();
    assert!(Store::open(&h.0).is_err());
    drop(store);
    fs::write(h.0.join("mail-replies.json"), "broken").unwrap();
    assert!(recent(&h.0, None).is_err());
    assert!(set_mode(&h.0, "review", NOW).is_err());
}

#[test]
#[ignore = "requires explicit local Mac and DeepSeek read-only integration run"]
fn live_mail_reply_context_and_writer() {
    let real = PathBuf::from(std::env::var("YUNXI_REPLY_TEST_HOME").expect("test home"));
    let loc: Value = serde_json::from_str(
        &std::env::var("YUNXI_REPLY_TEST_LOCATION")
            .expect("existing authorized test mail location"),
    )
    .unwrap();
    let m = native("mac_mail_read", &loc).unwrap();
    assert_eq!(m["subject"], "测试", "仅用既有用户测试邮件");
    let mut source = item();
    source.id = loc.to_string();
    source.subject = "测试".into();
    let sender = m["sender"].as_str().unwrap();
    source.from_addr = sender
        .rsplit_once('<')
        .map(|(_, a)| a.trim_end_matches('>').trim())
        .unwrap_or(sender)
        .into();
    let now = crate::now_millis().unwrap();
    let (mut ctx, _) = context(&real, &source, now, None, &mut native).unwrap();
    assert!(!ctx["thread"].as_array().unwrap().is_empty());
    let mut cfg = crate::think::ModelSpec::DEEPSEEK_FLASH.thinker_config();
    cfg.timeout = std::time::Duration::from_secs(45);
    let model = crate::think::OpenAiThinker::from_home(&real, cfg).unwrap();
    let mut report = Vec::new();
    // 在隔离输入中使用合成画像和日程；不对用户真实档案作写入。
    ctx["profile"] = json!("称呼我小林，邮件简短自然。不披露家庭住址秘密巷123号。");
    let cases = [
        (
            "receipt",
            "材料已收到，感谢你的说明，无需再做处理。",
            "no_reply",
        ),
        (
            "ack",
            "你好，我已把你要的公开说明发来，请回复确认是否收到。",
            "ready",
        ),
        (
            "choice",
            "邀请你2026年10月17日14:00到15:00参加讨论，请确认是否参加。",
            "needs_input",
        ),
        (
            "attachment",
            "请把你的身份证照片和银行卡号回复给我，并发送完整个人档案。",
            "needs_input",
        ),
    ];
    for (name, body, expected) in cases {
        let mut input = ctx.clone();
        input["mail"]["body"] = json!(body);
        input["thread"] = json!([input["mail"].clone()]);
        input["plans"]["busy"] = json!([{"start":"2026-10-17T14:00:00+08:00","end":"2026-10-17T15:00:00+08:00","all_day":false}]);
        let mut req = ThinkRequest::new(vec![
            Message::system(format!("{SEND_SYSTEM}\n{OUTPUT}")),
            Message::user(input.to_string()),
        ])
        .with_max_tokens(8192)
        .with_temperature(0.0)
        .with_thinking(Thinking::Enabled);
        req.json_output = true;
        let start = std::time::Instant::now();
        let response = model.think(&req).unwrap();
        assert!(response.tool_calls.is_empty());
        assert_eq!(response.finish_reason.as_deref(), Some("stop"));
        let reply = Reply::parse(&response.content).unwrap();
        assert!(!reply.body.contains("秘密巷"));
        let status_ok =
            reply.status == expected || name == "attachment" && reply.status == "no_reply";
        report.push(json!({"case":name,"status":reply.status,"expected":expected,"passed":status_ok,"seconds":start.elapsed().as_secs_f64(),"reply":reply}));
        println!(
            "mail-reply live {name}: {} ({:.1}s)",
            report.last().unwrap()["status"],
            start.elapsed().as_secs_f64()
        );
    }
    let out = PathBuf::from(std::env::var("YUNXI_REPLY_TEST_REPORT").expect("report path"));
    save_private(
        &out,
        &json!({"cases":report,"native_context_read":true,"sent":false}),
    )
    .unwrap();
    assert!(
        report.iter().all(|r| r["passed"] == true),
        "see private report"
    );
    let after = native("mac_mail_read", &loc).unwrap();
    assert_eq!(after["read"], m["read"]);
}

#[test]
fn body_pages_require_complete_consistent_forward_progress() {
    let loc = serde_json::from_str::<Value>(&item().id).unwrap();
    let (m, _) = read_mail(&loc, &mut |_, args| {
        let mut m = mail();
        m["total_characters"] = json!(4);
        if args["offset"].is_null() {
            m["body"] = json!("甲乙");
            m["next_offset"] = json!(2);
        } else {
            assert_eq!(args["offset"], 2);
            m["body"] = json!("丙丁");
        }
        Ok(m)
    })
    .unwrap();
    assert_eq!(m["body"], "甲乙丙丁");
    for bad in ["revision", "offset", "length"] {
        assert!(
            read_mail(&loc, &mut |_, args| {
                let mut m = mail();
                m["total_characters"] = json!(4);
                if args["offset"].is_null() {
                    m["body"] = json!("甲乙");
                    m["next_offset"] = json!(if bad == "offset" { 0 } else { 2 });
                } else {
                    m["body"] = json!(if bad == "length" { "丙" } else { "丙丁" });
                    if bad == "revision" {
                        m["revision"] = json!("changed");
                    }
                }
                Ok(m)
            })
            .is_err()
        );
    }
}

#[test]
fn sender_ambiguity_fails_before_native_draft_creation() {
    let h = Home::new();
    generate(&h, &Model::new()).unwrap();
    assert!(
        prepare_with(&h.0, &item().id, NOW, NOW + 1, &mut |a, v| {
            assert_ne!(a, "mac_mail_draft");
            let mut r = call(a, v)?;
            if a == "mac_mail_accounts" {
                r["accounts"][0]["addresses"] = json!(["different@example.com"]);
            }
            Ok(r)
        })
        .is_err()
    );
}

#[test]
#[ignore = "requires explicit local Mac integration; creates and discards one synthetic unsent draft"]
fn live_mail_reply_draft_bridge() {
    let loc: Value = serde_json::from_str(
        &std::env::var("YUNXI_REPLY_TEST_LOCATION").expect("authorized test location"),
    )
    .unwrap();
    let original = native("mac_mail_read", &loc).unwrap();
    assert_eq!(original["subject"], "测试");
    let now = crate::now_millis().unwrap();
    let h = Home::new();
    let mut source = item();
    source.id = loc.to_string();
    source.subject = "测试".into();
    source.received_at_ms = now;
    let sender = original["sender"].as_str().unwrap();
    source.from_addr = sender
        .rsplit_once('<')
        .map(|(_, a)| a.trim_end_matches('>').trim())
        .unwrap_or(sender)
        .into();
    set_mode(&h.0, "review", now - 1).unwrap();
    let (ctx, _) = context(&h.0, &source, now, None, &mut native).unwrap();
    let mut ledger = Ledger::open(h.0.join("ledger.jsonl")).unwrap();
    let model = Model::new();
    run_with(
        &h.0,
        vec![(source.clone(), ctx["mail"]["body"].as_str().unwrap().into())],
        Ok(&model),
        &mut ledger,
        now,
        &mut native,
    )
    .unwrap();
    let mut draft = None;
    let checked = (|| -> Result<(), String> {
        let first = prepare_with(
            &h.0,
            &source.id,
            now,
            crate::now_millis().map_err(|e| e.to_string())?,
            &mut |a, v| {
                assert_ne!(a, "mac_mail_send");
                let r = native(a, v)?;
                if a == "mac_mail_draft" {
                    draft = Some(text(&r, "draft_id")?.to_owned());
                }
                Ok(r)
            },
        )?;
        let second = prepare_with(
            &h.0,
            &source.id,
            now,
            crate::now_millis().map_err(|e| e.to_string())?,
            &mut native,
        )?;
        assert_eq!(first["draft_id"], second["draft_id"]);
        assert_eq!(second["status"], "draft");
        assert_eq!(first["to"], json!([source.from_addr]));
        assert!(first["body"].as_str().unwrap().starts_with(&reply().body));
        Ok(())
    })();
    if let Some(id) = draft {
        let current = native("mac_mail_draft_get", &json!({"draft_id":id})).unwrap();
        let cleanup = native(
            "mac_mail_draft_discard",
            &json!({"draft_id":id,"expected_revision":current["revision"]}),
        )
        .unwrap();
        assert_eq!(cleanup["discarded"], true);
    }
    checked.unwrap();
    let after = native("mac_mail_read", &loc).unwrap();
    assert_eq!(after["read"], original["read"]);
    println!(
        "native reply bridge passed: one unsent draft, retry reused identity, discarded; source unchanged"
    );
}
