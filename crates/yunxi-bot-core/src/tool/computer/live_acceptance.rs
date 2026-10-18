//! Explicitly opted-in hardware tests; the fixture has no accounts or external requests.
use super::*;
use crate::policy::SandboxMode;
use crate::think::prompt::PromptLayout;
use crate::think::{
    OpenAiThinker, ThinkError, ThinkRequest, ThinkResponse, Thinker, ThinkerConfig, Thinking,
};
use crate::tool::{Approval, ApprovalRequest, Approver, ToolPolicy, ToolRegistry, ToolRunner};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::AtomicBool;

const PAGE: &str = r#"<!doctype html><meta charset="utf-8"><title>云熙电脑操控验收</title>
<style>body{font:20px system-ui;max-width:900px;margin:30px}button,input{font:inherit;margin:8px;padding:8px}.pad{height:80px;border:1px solid;padding:10px}</style>
<h1>本机临时测试，不产生真实订单</h1>
<label>姓名<input aria-label="姓名" id="name"></label><label>目的地<input aria-label="目的地" id="city"></label>
<button onclick="result.textContent='计划预览：'+document.getElementById('name').value+' / '+city.value">生成预览</button><p id="result">尚未生成预览</p>
<button aria-label="指针测试" onmouseenter="hovered.textContent='悬停成功'" ondblclick="doubled.textContent='双击成功'" oncontextmenu="event.preventDefault();righted.textContent='右击成功'">指针测试</button>
<span id="hovered">尚未悬停</span> <span id="doubled">尚未双击</span> <span id="righted">尚未右击</span>
<div class="pad" aria-label="拖动区域" role="button" onpointerdown="this.setPointerCapture(event.pointerId);this.dataset.start=event.clientY" onpointerup="if(Math.abs(event.clientY-Number(this.dataset.start))>50)dragged.textContent='拖动成功'">按住拖动</div><button aria-label="拖动目标">拖动目标</button><p id="dragged">尚未拖动</p>
<label>测试附件<input aria-label="测试附件" type="file" onchange="uploaded.textContent='附件已选：'+this.files[0].name"></label><p id="uploaded">尚未选择附件</p>
<a href="/download" download="yunxi-ui-test.txt">下载测试文件</a>
<div style="height:1000px"></div><p>滚动到底部成功</p>"#;

struct Fixture {
    url: String,
    root: PathBuf,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl Fixture {
    fn new() -> Self {
        Self::with_page(PAGE)
    }
    fn with_page(page: &'static str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let done = stop.clone();
        let root = std::env::temp_dir().join(format!(
            "yunxi-ui-{}-{}",
            std::process::id(),
            crate::now_millis().unwrap()
        ));
        fs::create_dir(&root).unwrap();
        fs::write(root.join("附件.txt"), "云熙合成附件，无个人资料").unwrap();
        let thread = std::thread::spawn(move || {
            for stream in listener.incoming() {
                if done.load(Ordering::Relaxed) {
                    break;
                }
                let mut stream = stream.unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut request = [0; 4096];
                let Ok(n) = stream.read(&mut request) else {
                    continue;
                };
                let download = String::from_utf8_lossy(&request[..n]).starts_with("GET /download ");
                let (kind, body) = if download {
                    (
                        "text/plain; charset=utf-8\r\nContent-Disposition: attachment; filename=yunxi-ui-test.txt",
                        "yunxi download fixture\n",
                    )
                } else {
                    ("text/html; charset=utf-8", page)
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes());
            }
        });
        Self {
            url: format!("http://{address}/"),
            root,
            stop,
            thread: Some(thread),
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = TcpStream::connect(self.url.trim_start_matches("http://").trim_end_matches('/'));
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn observe(tools: &[ComputerTool], ctx: &mut ToolContext, op: &str, args: Value) -> ToolOutput {
    tools[1]
        .call(&json!({"operation":op,"arguments":args}), ctx)
        .unwrap()
}
fn binding(tools: &[ComputerTool]) -> Value {
    tools[1]
        .controller
        .lock()
        .unwrap()
        .frame
        .as_ref()
        .unwrap()
        .arguments
        .clone()
}
fn act(tools: &[ComputerTool], ctx: &mut ToolContext, op: &str, args: Value) -> Result<ToolOutput> {
    let id = tools[1]
        .controller
        .lock()
        .unwrap()
        .frame
        .as_ref()
        .unwrap()
        .id
        .clone();
    tools[2].call(&json!({"operation":op,"arguments":args,"observation_id":id,"purpose":"仅在本轮临时测试界面核验此动作"}), ctx)
}
fn page(tools: &[ComputerTool], ctx: &mut ToolContext, target: &Value, tab: &Value) -> ToolOutput {
    observe(
        tools,
        ctx,
        "get_browser_state",
        json!({"target_id":target,"tab_id":tab,"include_screenshot":true}),
    )
}
fn named_ref(tools: &[ComputerTool], name: &str) -> Value {
    let state = binding(tools);
    let matches = state["refs"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|r| r["name"].as_str() == Some(name))
        .collect::<Vec<_>>();
    assert_eq!(
        matches.len(),
        1,
        "ambiguous/missing fixture control: {name}"
    );
    matches[0]["ref"].clone()
}

struct NativeDocument {
    path: PathBuf,
    target: Value,
    controller: Arc<Mutex<Controller>>,
}
impl Drop for NativeDocument {
    fn drop(&mut self) {
        // Close only this synthetic document, before its temporary directory is removed.
        let observe = ComputerTool {
            name: "computer_observe",
            controller: self.controller.clone(),
        };
        let mut ctx = ToolContext::new(
            self.path.parent().unwrap_or(&self.path),
            SandboxMode::WorkspaceWrite,
        );
        if observe
            .call(
                &json!({"operation":"get_window_state","arguments":self.target}),
                &mut ctx,
            )
            .is_err()
        {
            return;
        }
        let id = self
            .controller
            .lock()
            .ok()
            .and_then(|c| c.frame.as_ref().map(|f| f.id.clone()));
        if let Some(id) = id {
            let act = ComputerTool {
                name: "computer_act",
                controller: self.controller.clone(),
            };
            let result = act.call(&json!({"operation":"hotkey","arguments":{"pid":self.target["pid"],"window_id":self.target["window_id"],"keys":["cmd","w"],"delivery_mode":"foreground"},"observation_id":id,"purpose":"关闭本轮已结束的合成测试文档"}),&mut ctx);
            if result.is_err() {
                eprintln!("synthetic document cleanup could not close its window");
            }
        }
    }
}

fn native_document(
    fixture: &Fixture,
    tools: &[ComputerTool],
    ctx: &mut ToolContext,
) -> NativeDocument {
    let title = format!(
        "{}-native.txt",
        fixture.root.file_name().unwrap().to_string_lossy()
    );
    let path = fixture.root.join(&title);
    fs::write(&path, "temporary fixture only").unwrap();
    let opened = crate::exec::run_command(
        &[
            "/usr/bin/open".into(),
            "-a".into(),
            "TextEdit".into(),
            path.to_string_lossy().into_owned(),
        ],
        &crate::exec::ExecOptions::default(),
    )
    .unwrap();
    assert_eq!(opened.exit_code, 0);
    for _ in 0..30 {
        observe(tools, ctx, "list_windows", json!({}));
        if let Some(window) = binding(tools)["windows"]
            .as_array()
            .unwrap()
            .iter()
            .find(|w| w["title"].as_str().is_some_and(|s| s.starts_with(&title)))
        {
            return NativeDocument {
                path,
                target: json!({"pid":window["pid"],"window_id":window["window_id"]}),
                controller: tools[1].controller.clone(),
            };
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("temporary TextEdit window not found");
}

fn native_bound(arguments: &Value, target: &Value) -> bool {
    let pid = arguments
        .get("pid")
        .or_else(|| arguments["target"].get("pid"));
    let window = arguments
        .get("window_id")
        .or_else(|| arguments["target"].get("window_id"));
    let window_allowed = window.is_some_and(|w| {
        (target["owned_test_process"] == true && w.as_u64().is_some())
            || target.get("window_id") == Some(w)
            || target["window_ids"]
                .as_array()
                .is_some_and(|ids| ids.contains(w))
    });
    pid.is_some()
        && pid == target.get("pid")
        && window_allowed
        && ["pid", "window_id"].iter().all(|key| {
            let flat = arguments.get(*key);
            let nested = arguments["target"].get(*key);
            flat.is_none() || nested.is_none() || flat == nested
        })
        && arguments
            .get("target")
            .is_none_or(|t| t["kind"] == "window")
}

#[test]
fn fixture_accepts_both_exact_native_target_forms_but_never_conflicts_or_desktop() {
    let target = json!({"pid":1,"window_id":2});
    assert!(native_bound(&target, &target));
    assert!(native_bound(
        &json!({"pid":1,"window_id":99}),
        &json!({"pid":1,"owned_test_process":true})
    ));
    assert!(!native_bound(
        &json!({"pid":2,"window_id":99}),
        &json!({"pid":1,"owned_test_process":true})
    ));
    assert!(native_bound(
        &json!({"target":{"kind":"window","pid":1,"window_id":2}}),
        &target
    ));
    for arguments in [
        json!({}),
        json!({"pid":1,"window_id":2,"target":{"kind":"window","pid":9,"window_id":2}}),
        json!({"target":{"kind":"desktop","pid":1,"window_id":2}}),
    ] {
        assert!(!native_bound(&arguments, &target));
    }
}

// Only this ignored fixture test has a bounded approver; production still asks for each action.
struct FixtureApprover {
    target: Value,
    tab: Value,
    native: Option<Value>,
    approved: usize,
}
impl Approver for FixtureApprover {
    fn approve(&mut self, req: &ApprovalRequest) -> Approval {
        let Ok(v) = serde_json::from_str::<Value>(&req.arguments) else {
            return Approval::Deny;
        };
        let native = self.native.as_ref().is_some_and(|target| {
            native_bound(&v["arguments"], target)
                && match v["operation"].as_str() {
                    Some("bring_to_front" | "click") => true,
                    Some("press_key") => matches!(
                        v["arguments"]["key"].as_str(),
                        Some("escape" | "esc" | "Escape")
                    ),
                    Some("type_text") => v["arguments"]["text"]
                        .as_str()
                        .is_some_and(|s| s.len() <= 500),
                    Some("hotkey") => {
                        [json!(["cmd", "a"]), json!(["cmd", "s"])].contains(&v["arguments"]["keys"])
                    }
                    _ => false,
                }
        });
        if req.tool == "computer_act"
            && req.require_human
            && req.private
            && (native
                || (self.target.is_string()
                    && self.tab.is_string()
                    && v["arguments"]["target_id"] == self.target
                    && v["arguments"]["tab_id"] == self.tab
                    && ["browser_type", "browser_click", "browser_dialog"]
                        .contains(&v["operation"].as_str().unwrap_or(""))))
        {
            self.approved += 1;
            Approval::Once
        } else {
            eprintln!(
                "fixture approval guard rejected operation={}, native_bound={}",
                v["operation"]
                    .as_str()
                    .filter(|op| catalog().get(*op).is_some())
                    .unwrap_or("unknown"),
                native
            );
            Approval::Deny
        }
    }
}
struct FixtureTool {
    inner: ComputerTool,
    target: Value,
    tab: Value,
    native: Option<Value>,
}
impl Tool for FixtureTool {
    fn name(&self) -> &str {
        self.inner.name()
    }
    fn description(&self) -> &str {
        self.inner.description()
    }
    fn parameters(&self) -> Value {
        self.inner.parameters()
    }
    fn capability(&self) -> Capability {
        self.inner.capability()
    }
    fn fresh_approval(&self) -> bool {
        self.inner.fresh_approval()
    }
    fn preflight_rejection(&self, args: &Value, ctx: &ToolContext) -> Option<String> {
        self.inner.preflight_rejection(args, ctx)
    }
    fn private_io(&self) -> bool {
        self.inner.private_io()
    }
    fn interaction_group(&self) -> Option<&'static str> {
        self.inner.interaction_group()
    }
    fn requires_user_choice(&self) -> bool {
        self.inner.requires_user_choice()
    }
    fn reset_observation(&self) -> Result<()> {
        self.inner.reset_observation()
    }
    fn audit_arguments(&self, args: &Value) -> Value {
        self.inner.audit_arguments(args)
    }
    fn preview(&self, args: &Value, ctx: &ToolContext) -> Option<String> {
        self.inner.preview(args, ctx)
    }
    fn call(&self, args: &Value, ctx: &mut ToolContext) -> Result<ToolOutput> {
        eprintln!(
            "model tool: {} / {} keys={:?}",
            self.name(),
            args["operation"],
            args["arguments"]
                .as_object()
                .map(|a| a.keys().collect::<Vec<_>>())
        );
        if self.name() != "computer_schema" {
            let arguments = if self.name() == "computer_verify" {
                self.inner
                    .controller
                    .lock()
                    .unwrap()
                    .frame
                    .as_ref()
                    .map(|f| f.arguments.clone())
                    .unwrap_or(Value::Null)
            } else {
                args["arguments"].clone()
            };
            let browser = self.target.is_string()
                && self.tab.is_string()
                && arguments["target_id"] == self.target
                && arguments["tab_id"] == self.tab
                && (self.name() != "computer_observe"
                    || matches!(
                        args["operation"].as_str(),
                        Some("get_browser_state" | "browser_dialog")
                    ));
            let native = self
                .native
                .as_ref()
                .is_some_and(|target| native_bound(&arguments, target))
                && (self.name() != "computer_observe" || args["operation"] == "get_window_state");
            let native_list = self.name() == "computer_observe"
                && args["operation"] == "list_windows"
                && self
                    .native
                    .as_ref()
                    .is_some_and(|target| arguments["pid"] == target["pid"]);
            let browser_bind = self.name() == "computer_observe"
                && args["operation"] == "get_browser_state"
                && self.target.is_string()
                && self.tab.is_string()
                && self
                    .native
                    .as_ref()
                    .is_some_and(|target| native_bound(&arguments, target));
            if !browser && !native && !native_list && !browser_bind {
                return Err(bad("验收只允许本轮合成页面及临时文档"));
            }
        }
        let choice_before = if args["operation"] == "click" {
            let c = self.inner.controller.lock().unwrap();
            Some((
                c.choice_pending,
                c.frame
                    .as_ref()
                    .is_some_and(|frame| native::selects_button(frame, &input(args).unwrap())),
            ))
        } else {
            None
        };
        let result = self.inner.call(args, ctx);
        if let Some((pending, selects)) = choice_before {
            let receipt: Value = result
                .as_ref()
                .ok()
                .and_then(|output| output.text.rsplit_once("结构化回执："))
                .and_then(|(_, text)| serde_json::from_str(text).ok())
                .unwrap_or(Value::Null);
            eprintln!(
                "synthetic click choice: pending={pending:?} selects={selects} action={} route={} effect={} after={:?}",
                args["arguments"]["action"],
                receipt["route"],
                receipt["effect"],
                self.inner.controller.lock().unwrap().choice_pending
            );
        }
        if args["operation"] == "hotkey" {
            eprintln!(
                "fixture keyboard: select_all={} save={}",
                args["arguments"]["keys"] == json!(["cmd", "a"]),
                args["arguments"]["keys"] == json!(["cmd", "s"])
            );
        }
        if args["operation"] == "get_window_state"
            && let Ok(output) = &result
        {
            let data: Value = output
                .text
                .rsplit_once("结构化回执：")
                .and_then(|(_, text)| serde_json::from_str(text).ok())
                .unwrap_or(Value::Null);
            let dialogs: Vec<_> = data["native_dialogs"].as_array().into_iter().flatten().map(|dialog| json!({"title":dialog["title"],"buttons":dialog["buttons"],"field_count":dialog["field_count"],"complete":dialog["complete"]})).collect();
            eprintln!(
                "synthetic native context: {}",
                json!({"requested_window":args["arguments"]["window_id"],"window":data["window_id"],"pid":data["pid"],"degraded":data["degraded"]==true,"truncated":data["truncated"],"native_dialogs":dialogs,"elements":data["elements"].as_array().map(Vec::len),"pending":self.inner.controller.lock().unwrap().choice_pending})
            );
            eprintln!(
                "fixture native read: original={} preview={}",
                output.text.contains("temporary fixture only"),
                output.text.contains("计划预览：")
            );
        }
        if let Err(error) = &result {
            eprintln!("fixture call failed: {error}");
        }
        if self.inner.name == "computer_verify"
            && let Ok(output) = &result
            && let Ok(data) = serde_json::from_str::<Value>(&output.text)
        {
            eprintln!(
                "synthetic verification: matched={} checks={}",
                data["matched"],
                data["checks"].as_array().map_or(0, Vec::len)
            );
        }
        result
    }
}

struct FixtureCallLog;
impl crate::tool::runner::ToolCallSink for FixtureCallLog {
    fn record(&self, call: &crate::tool::runner::ToolCallRecord) {
        eprintln!(
            "fixture result: {} {:?} error={:?}",
            call.tool,
            call.decision,
            call.output.as_ref().and_then(|v| v.as_ref().err())
        );
    }
}

// The CLI's MeteredThinker waits for local rate limits; this core-only test must pace too.
struct PacedThinker(OpenAiThinker, std::sync::atomic::AtomicUsize);
impl Thinker for PacedThinker {
    fn think(&self, req: &ThinkRequest) -> std::result::Result<ThinkResponse, ThinkError> {
        if req.messages.iter().any(|m| !m.images.is_empty()) {
            self.1.fetch_add(1, Ordering::Relaxed);
        }
        if let Some(wait) = self.0.throttle_wait() {
            std::thread::sleep(wait);
        }
        let response = self.0.think(req);
        if let Ok(response) = &response {
            let reused = response.tool_calls.iter().any(|call| {
                req.messages
                    .iter()
                    .any(|m| m.tool_calls.iter().any(|old| old["id"] == call["id"]))
            });
            eprintln!("model tool ID reused across turns: {reused}");
        }
        response
    }
    fn think_stream(
        &self,
        req: &ThinkRequest,
        delta: &mut dyn FnMut(&str),
    ) -> std::result::Result<ThinkResponse, ThinkError> {
        if req.messages.iter().any(|m| !m.images.is_empty()) {
            self.1.fetch_add(1, Ordering::Relaxed);
        }
        if let Some(wait) = self.0.throttle_wait() {
            std::thread::sleep(wait);
        }
        let response = self.0.think_stream(req, delta);
        if let Ok(response) = &response {
            let reused = response.tool_calls.iter().any(|call| {
                req.messages
                    .iter()
                    .any(|m| m.tool_calls.iter().any(|old| old["id"] == call["id"]))
            });
            eprintln!("model tool ID reused across turns: {reused}");
        }
        response
    }
    fn model(&self) -> &str {
        self.0.model()
    }
}

fn prepare_browser(
    fixture: &Fixture,
    ts: &[ComputerTool],
    ctx: &mut ToolContext,
    ready: &str,
) -> (u64, Value, Value, Value) {
    prepare_browser_profile(fixture, ts, ctx, ready, json!({"mode":"isolated_new"}))
}

fn prepare_browser_profile(
    fixture: &Fixture,
    ts: &[ComputerTool],
    ctx: &mut ToolContext,
    ready: &str,
    profile: Value,
) -> (u64, Value, Value, Value) {
    observe(ts, ctx, "list_apps", json!({}));
    let prepared = act(
        ts,
        ctx,
        "browser_prepare",
        json!({"allow_launch":true,"profile":profile}),
    )
    .unwrap();
    let meta: Value =
        serde_json::from_str(prepared.text.split_once("结构化回执：").unwrap().1).unwrap();
    let pid = meta["prepared_pid"].as_u64().unwrap();
    let mut window = Value::Null;
    for _ in 0..20 {
        observe(ts, ctx, "list_windows", json!({}));
        window = binding(ts)["windows"]
            .as_array()
            .unwrap()
            .iter()
            .find(|w| w["pid"].as_u64() == Some(pid) && w["title"] == "about:blank")
            .map(|w| w["window_id"].clone())
            .unwrap_or(Value::Null);
        if !window.is_null() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(!window.is_null());
    observe(
        ts,
        ctx,
        "get_browser_state",
        json!({"pid":pid,"window_id":window}),
    );
    let state = binding(ts);
    let target = state["target_id"].clone();
    let tab = state["tabs"][0]["tab_id"].clone();
    assert!(target.is_string() && tab.is_string());
    page(ts, ctx, &target, &tab);
    act(
        ts,
        ctx,
        "browser_navigate",
        json!({"target_id":target,"tab_id":tab,"url":fixture.url}),
    )
    .unwrap();
    for _ in 0..20 {
        let out = page(ts, ctx, &target, &tab);
        if out.text.contains(ready) {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let out = page(ts, ctx, &target, &tab);
    assert_eq!(out.images.len(), 1);
    assert!(out.text.contains(ready));
    (pid, window, target, tab)
}

#[test]
#[ignore = "Runs only an isolated local browser fixture to verify bounded condition waits; no recording"]
fn live_conditional_observation_wait() {
    let fixture = Fixture::with_page(
        r#"<!doctype html><meta charset="utf-8"><title>云熙异步条件测试</title><p>条件测试就绪</p><button onclick="out.textContent='处理中';setTimeout(()=>out.textContent='条件已完成',1500)">开始异步</button><p id="out">尚未开始</p>"#,
    );
    let ts = tools();
    let mut ctx = ToolContext::new(&fixture.root, SandboxMode::WorkspaceWrite);
    let (_, _, target, tab) = prepare_browser(&fixture, &ts, &mut ctx, "条件测试就绪");
    let reference = named_ref(&ts, "开始异步");
    act(
        &ts,
        &mut ctx,
        "browser_click",
        json!({"target_id":target,"tab_id":tab,"ref":reference}),
    )
    .unwrap();
    let output = ts[1].call(&json!({"operation":"get_browser_state","arguments":{"target_id":target,"tab_id":tab},"wait_for":{"timeout_ms":5000,"checks":[{"kind":"text_present","text":"条件已完成"}]}}),&mut ctx).unwrap();
    let receipt: Value =
        serde_json::from_str(output.text.rsplit_once("只读等待回执：").unwrap().1).unwrap();
    assert_eq!(receipt["wait_status"], "matched");
    assert!(receipt["observations"].as_u64().unwrap() <= 8);
    assert!(output.text.contains("条件已完成"));
    eprintln!("real conditional observation: {receipt}");
    page(&ts, &mut ctx, &target, &tab);
    act(
        &ts,
        &mut ctx,
        "browser_navigate",
        json!({"target_id":target,"tab_id":tab,"url":"about:blank"}),
    )
    .unwrap();
}

#[test]
#[ignore = "Opens an isolated Mac browser, runs fixture actions and calls DeepSeek; YUNXI_TEST_HOME required"]
fn live_bot_browser_actions_and_deepseek_task() {
    let home = PathBuf::from(std::env::var_os("YUNXI_TEST_HOME").unwrap());
    let fixture = Fixture::new();
    let ts = tools();
    let mut ctx = ToolContext::new(&fixture.root, SandboxMode::WorkspaceWrite);
    let (pid, window, target, tab) = prepare_browser(&fixture, &ts, &mut ctx, "尚未生成预览");
    let input_ref = named_ref(&ts, "姓名");
    let refused = act(
        &ts,
        &mut ctx,
        "browser_click",
        json!({"target_id":target,"tab_id":tab,"ref":input_ref,"delivery_mode":"foreground"}),
    )
    .unwrap_err();
    assert!(refused.to_string().contains("degraded=true"));
    assert!(
        page(&ts, &mut ctx, &target, &tab)
            .text
            .contains("尚未生成预览")
    );
    eprintln!("Driver effect=refused: returned failure, not successful execution");
    let old_id = ts[1]
        .controller
        .lock()
        .unwrap()
        .frame
        .as_ref()
        .unwrap()
        .id
        .clone();
    let reference = named_ref(&ts, "生成预览");
    page(&ts, &mut ctx, &target, &tab);
    let rejected = json!({"operation":"browser_click","arguments":{"target_id":target,"tab_id":tab,"ref":reference},"observation_id":old_id,"purpose":"验证旧观察必须被拒绝"});
    assert!(
        ts[2]
            .call(&rejected, &mut ctx)
            .unwrap_err()
            .to_string()
            .contains("stale_observation")
    );
    assert!(
        page(&ts, &mut ctx, &target, &tab)
            .text
            .contains("尚未生成预览")
    );
    let mut wrong_target = rejected;
    wrong_target["observation_id"] =
        json!(ts[1].controller.lock().unwrap().frame.as_ref().unwrap().id);
    wrong_target["arguments"]["tab_id"] = json!("different-tab");
    assert!(
        ts[2]
            .call(&wrong_target, &mut ctx)
            .unwrap_err()
            .to_string()
            .contains("目标")
    );
    eprintln!("negative checks: stale observation and wrong tab rejected before dispatch");
    eprintln!(
        "fixture bound: native pid={pid}, window={window}; screenshot and semantic refs passed"
    );

    let thinker = PacedThinker(
        OpenAiThinker::from_home(&home, ThinkerConfig::deepseek()).unwrap(),
        std::sync::atomic::AtomicUsize::new(0),
    );
    let proof_dir = std::env::var_os("YUNXI_PROOF_DIR").map(PathBuf::from);
    // Existing browser-only recording acceptance stays browser-only; ordinary development has no recording.
    let document = proof_dir
        .is_none()
        .then(|| native_document(&fixture, &ts, &mut ctx));
    let native = document.as_ref().map(|doc| doc.target.clone());
    let mut registry = ToolRegistry::new();
    for inner in ts.iter().filter(|t| t.name().starts_with("computer_")) {
        registry
            .register(Arc::new(FixtureTool {
                inner: ComputerTool {
                    name: inner.name,
                    controller: inner.controller.clone(),
                },
                target: target.clone(),
                tab: tab.clone(),
                native: native.clone(),
            }))
            .unwrap();
    }
    let synthetic_name = if proof_dir.is_some() {
        format!("验收员{}", crate::now_millis().unwrap() % 10000)
    } else {
        format!("云熙验收员{}", crate::now_millis().unwrap() % 10000)
    };
    let destination = if proof_dir.is_some() {
        "成都"
    } else {
        "杭州"
    };
    let expected = format!("计划预览：{synthetic_name} / {destination}");
    let mut layout = PromptLayout::new(format!(
        "{}\n验收范围仅指定的合成网页和临时TextEdit文档，禁止导航、发送、购买、脚本、其他窗口或剪贴板。browser_type支持replace=true；点击和原生键盘输入可显式delivery_mode=foreground。",
        workflow::GUIDE
    ));
    let mut goal = format!(
        "请在测试页target_id={target},tab_id={tab}中，将姓名填写为‘{synthetic_name}’，目的地填写为‘{destination}’，点击‘生成预览’。重新读取并用computer_verify核对真实预览文字。每次get_browser_state请include_screenshot=true。"
    );
    if let Some(doc) = &document {
        let target = &doc.target;
        goal.push_str(&format!("随后把网页实际预览结果通过电脑操控原样写入已打开的临时TextEdit文档pid={}、window_id={}，替换原文并保存。自行根据当前界面完成替换与保存，不使用文件或命令工具。最后重新观察文档，用computer_verify核对文字再总结。",target["pid"],target["window_id"]));
    }
    layout.ask(goal);
    let mut approver = FixtureApprover {
        target: target.clone(),
        tab: tab.clone(),
        native: native.clone(),
        approved: 0,
    };
    let start = Instant::now();
    let mut recording = proof_dir.as_ref().map(|root| {
        use std::os::unix::fs::DirBuilderExt;
        fs::DirBuilder::new().mode(0o700).create(root).unwrap();
        let helper =
            std::env::var_os("YUNXI_TEST_RECORDER").expect("explicit tested recorder required");
        let mut child = std::process::Command::new(helper)
            .arg(window.as_u64().unwrap().to_string())
            .arg(root.join("bot-autonomous-form.mp4"))
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let mut output = BufReader::new(child.stdout.take().unwrap());
        let mut ready = String::new();
        output.read_line(&mut ready).unwrap();
        assert_eq!(
            ready.trim(),
            "READY",
            "recording must start before model actions"
        );
        (child, output)
    });
    let outcome = ToolRunner::new(
        &registry,
        ToolPolicy::default(),
        &mut approver,
        ToolContext::new(&fixture.root, SandboxMode::WorkspaceWrite),
    )
    .with_max_rounds(32)
    .with_sink(Arc::new(FixtureCallLog))
    .with_thinking(Some(if document.is_some() {
        Thinking::Enabled
    } else {
        Thinking::Disabled
    }))
    .run(
        &thinker,
        &mut layout,
        if document.is_some() { 8192 } else { 2048 },
    )
    .unwrap();
    eprintln!(
        "DeepSeek task: {} calls, {} approvals, {:.1}s, answer={}",
        outcome.calls.len(),
        approver.approved,
        start.elapsed().as_secs_f64(),
        outcome.text
    );
    assert!(approver.approved >= 3);
    assert!(page(&ts, &mut ctx, &target, &tab).text.contains(&expected));
    assert!(
        outcome
            .calls
            .iter()
            .any(|call| call.tool == "computer_verify" && matches!(&call.output, Some(Ok(_))))
    );
    if let Some(doc) = &document {
        let path = &doc.path;
        for _ in 0..20 {
            if fs::read_to_string(path).unwrap().trim() == expected {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        assert_eq!(
            fs::read_to_string(path).unwrap().trim(),
            expected,
            "UI save must independently match actual web result on disk"
        );
        eprintln!("cross-app result independently verified from saved UTF-8 file; no recording");
    }
    // The proof clip ends before the scripted Driver API checks below.
    if let Some((mut child, mut output)) = recording.take() {
        child.stdin.take().unwrap().write_all(b"stop\n").unwrap();
        let mut end = String::new();
        output.read_to_string(&mut end).unwrap();
        assert!(child.wait().unwrap().success() && end.contains("FINISHED duration="));
        let root = proof_dir.as_ref().unwrap();
        fs::write(root.join("proof.json"), serde_json::to_vec_pretty(&json!({
            "scope":"production ToolRunner + DeepSeek + ComputerTool; synthetic local form only",
            "goal":format!("将姓名填写为{synthetic_name}，目的地填写为{destination}，生成预览并回读核验"),
            "expected_visible_result":expected,
            "result_verified_from_live_page":true,"operator_ui_actions_during_clip":0,
            "model_requests_with_images":thinker.1.load(Ordering::Relaxed),"computer_actions_approved_within_fixture":approver.approved,
            "elapsed_seconds":start.elapsed().as_secs_f64(),"answer":outcome.text,
            "tool_calls":outcome.calls,"video":"bot-autonomous-form.mp4",
            "excluded":"fixture setup before clip; scripted API checks after clip; production front-end routing and real booking not proven by this clip"
        })).unwrap()).unwrap();
    }
    let saved = serde_json::to_string(&layout).unwrap();
    assert!(!saved.contains("data:image") && !saved.contains("screenshot_png_b64"));
    assert!(
        outcome
            .calls
            .iter()
            .filter(|c| c.tool == "computer_act")
            .all(|c| c.arguments["private_arguments"] == true)
    );

    for (gesture, expected) in [
        ("hover", "悬停成功"),
        ("double_click", "双击成功"),
        ("right_click", "右击成功"),
    ] {
        page(&ts, &mut ctx, &target, &tab);
        let reference = named_ref(&ts, "指针测试");
        act(&ts, &mut ctx, "browser_pointer", json!({"target_id":target,"tab_id":tab,"ref":reference,"action":gesture,"delivery_mode":"foreground"})).unwrap();
        assert!(page(&ts, &mut ctx, &target, &tab).text.contains(expected));
        eprintln!("browser pointer {gesture}: verified {expected}");
    }
    let origin = named_ref(&ts, "拖动区域");
    let destination = named_ref(&ts, "拖动目标");
    act(&ts, &mut ctx, "browser_pointer", json!({"target_id":target,"tab_id":tab,"ref":origin,"destination_ref":destination,"action":"drag","delivery_mode":"foreground"})).unwrap();
    assert!(page(&ts, &mut ctx, &target, &tab).text.contains("拖动成功"));
    eprintln!("browser drag: pointer displacement verified");
    let reference = named_ref(&ts, "测试附件");
    act(&ts, &mut ctx, "browser_set_input_files", json!({"target_id":target,"tab_id":tab,"ref":reference,"files":[fixture.root.join("附件.txt")]})).unwrap();
    assert!(
        page(&ts, &mut ctx, &target, &tab)
            .text
            .contains("附件已选：附件.txt")
    );
    eprintln!("file input: verified synthetic attachment filename");
    let reference = named_ref(&ts, "下载测试文件");
    let download = act(&ts, &mut ctx, "browser_download", json!({"target_id":target,"tab_id":tab,"ref":reference,"destination_root":fixture.root.canonicalize().unwrap()})).unwrap();
    eprintln!("download receipt: {}", download.text);
    assert!(
        fs::read_dir(&fixture.root)
            .unwrap()
            .filter_map(|e| e.ok())
            .any(|e| fs::read(e.path()).ok().as_deref() == Some(b"yunxi download fixture\n"))
    );
    page(&ts, &mut ctx, &target, &tab);
    act(&ts, &mut ctx, "browser_pointer", json!({"target_id":target,"tab_id":tab,"action":"scroll","x":800,"y":500,"delta_y":1100,"delivery_mode":"foreground"})).unwrap();
    assert!(
        page(&ts, &mut ctx, &target, &tab)
            .text
            .contains("滚动到底部成功")
    );
    eprintln!("browser scroll: bottom marker observed; fixture cleanup follows");
}

#[test]
#[ignore = "Opens and edits a new temporary TextEdit file through Bot tools; requires Mac permissions"]
fn live_bot_native_text_edit_save_and_close() {
    let fixture = Fixture::new();
    let ts = tools();
    let mut ctx = ToolContext::new(&fixture.root, SandboxMode::WorkspaceWrite);
    for operation in [
        "check_permissions",
        "get_screen_size",
        "get_cursor_position",
    ] {
        assert!(!observe(&ts, &mut ctx, operation, json!({})).text.is_empty());
    }
    assert!(
        !observe(
            &ts,
            &mut ctx,
            "clipboard_read",
            json!({"include_text":false})
        )
        .text
        .is_empty()
    );
    eprintln!(
        "permissions, screen size, cursor and clipboard format metadata: read successfully; clipboard unchanged"
    );
    let document = native_document(&fixture, &ts, &mut ctx);
    let path = &document.path;
    let target = document.target.clone();
    let pid = target["pid"].clone();
    let wid = target["window_id"].clone();
    let snapshot = observe(&ts, &mut ctx, "get_window_state", target.clone());
    assert_eq!(snapshot.images.len(), 1);
    assert!(snapshot.text.contains("temporary fixture only"));
    act(&ts, &mut ctx, "bring_to_front", target.clone()).unwrap();
    observe(&ts, &mut ctx, "get_window_state", target.clone());
    act(
        &ts,
        &mut ctx,
        "set_window_frame",
        json!({"pid":pid,"window_id":wid,"x":100,"y":120,"width":800,"height":600}),
    )
    .unwrap();
    observe(&ts, &mut ctx, "list_windows", json!({}));
    let windows = binding(&ts);
    let moved = windows["windows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|w| w["pid"] == pid && w["window_id"] == wid)
        .unwrap();
    // AppKit reported 1–2 point rounding here; use observed bounds, not the requested frame, for later input.
    for (key, expected) in [
        ("x", 100.0),
        ("y", 120.0),
        ("width", 800.0),
        ("height", 600.0),
    ] {
        assert!(
            (moved["bounds"][key].as_f64().unwrap() - expected).abs() <= 3.0,
            "window frame {key} outside measured tolerance"
        );
    }
    observe(&ts, &mut ctx, "get_window_state", target.clone());
    act(
        &ts,
        &mut ctx,
        "hotkey",
        json!({"pid":pid,"window_id":wid,"keys":["cmd","a"],"delivery_mode":"foreground"}),
    )
    .unwrap();
    observe(&ts, &mut ctx, "get_window_state", target.clone());
    let text = "云熙电脑操控验收\n中文、English、数字 123。\n仅临时文件。";
    act(
        &ts,
        &mut ctx,
        "type_text",
        json!({"pid":pid,"window_id":wid,"text":text,"delivery_mode":"foreground"}),
    )
    .unwrap();
    assert!(
        observe(&ts, &mut ctx, "get_window_state", target.clone())
            .text
            .contains("云熙电脑操控验收")
    );
    act(
        &ts,
        &mut ctx,
        "hotkey",
        json!({"pid":pid,"window_id":wid,"keys":["cmd","s"],"delivery_mode":"foreground"}),
    )
    .unwrap();
    for _ in 0..20 {
        if fs::read_to_string(path).unwrap() == text {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(
        fs::read_to_string(path).unwrap(),
        text,
        "UI save must match bytes on disk"
    );
    drop(document);
    observe(&ts, &mut ctx, "list_windows", json!({}));
    assert!(
        !binding(&ts)["windows"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w["pid"] == pid && w["window_id"] == wid)
    );
    eprintln!(
        "native TextEdit: focus, window frame, select-all, mixed-language typing, save byte verification and close passed"
    );
}

#[path = "popup_acceptance.rs"]
mod popup_acceptance;

#[test]
#[ignore = "Real embedded CDP learning and parameterized replay on localhost only; no recording"]
fn live_learn_and_replay_browser_workflow() {
    let fixture = Fixture::with_page(
        r#"<!doctype html><meta charset="utf-8"><title>云熙流程复用验收</title><h1>流程测试就绪</h1><label>目的地<input aria-label="目的地" id="city"></label><button onclick="out.textContent='计划：'+city.value">生成预览</button><p id="out">未生成</p>"#,
    );
    fs::create_dir(fixture.root.join("data")).unwrap();
    let ts = tools_at(&fixture.root.join("data"));
    let mut ctx = ToolContext::new(&fixture.root, SandboxMode::WorkspaceWrite);
    let (_, _, target, tab) = prepare_browser(&fixture, &ts, &mut ctx, "流程测试就绪");
    let wf = ts.iter().find(|t| t.name == "browser_workflow").unwrap();
    wf.call(
        &json!({"operation":"begin","name":"fixture-reuse"}),
        &mut ctx,
    )
    .unwrap();
    act(
        &ts,
        &mut ctx,
        "browser_navigate",
        json!({"target_id":target,"tab_id":tab,"url":fixture.url}),
    )
    .unwrap();
    page(&ts, &mut ctx, &target, &tab);
    let reference = named_ref(&ts, "目的地");
    act(
        &ts,
        &mut ctx,
        "browser_type",
        json!({"target_id":target,"tab_id":tab,"ref":reference,"text":"上海","replace":true}),
    )
    .unwrap();
    page(&ts, &mut ctx, &target, &tab);
    let reference = named_ref(&ts, "生成预览");
    act(
        &ts,
        &mut ctx,
        "browser_click",
        json!({"target_id":target,"tab_id":tab,"ref":reference}),
    )
    .unwrap();
    page(&ts, &mut ctx, &target, &tab);
    ts.iter()
        .find(|t| t.name == "browser_workflow_save")
        .unwrap()
        .call(&json!({"success_text":"计划：上海"}), &mut ctx)
        .unwrap();
    let stored = wf
        .call(
            &json!({"operation":"inspect","name":"fixture-reuse"}),
            &mut ctx,
        )
        .unwrap();
    assert!(
        !stored.text.contains("上海")
            && !stored.text.contains("observation_id")
            && !stored.text.contains("127.0.0.1:0")
    );
    let args = json!({"name":"fixture-reuse","target_id":target,"tab_id":tab,"parameters":{"input_1":fixture.url,"input_2":"北京","expected_result":"计划：北京"}});
    struct ReplayThinker {
        args: Value,
        count: std::sync::atomic::AtomicUsize,
    }
    impl Thinker for ReplayThinker {
        fn think(&self, _: &ThinkRequest) -> std::result::Result<ThinkResponse, ThinkError> {
            let first = self.count.fetch_add(1, Ordering::Relaxed) == 0;
            Ok(ThinkResponse {
                content: if first {
                    String::new()
                } else {
                    "fixture done".into()
                },
                model: "fixture-no-network".into(),
                usage: Default::default(),
                finish_reason: Some(if first { "tool_calls" } else { "stop" }.into()),
                reasoning: None,
                thinking: Thinking::Disabled,
                tool_calls: if first {
                    vec![
                        json!({"id":"flow","type":"function","function":{"name":"browser_workflow_run","arguments":self.args.to_string()}}),
                    ]
                } else {
                    vec![]
                },
            })
        }
        fn model(&self) -> &str {
            "fixture-no-network"
        }
    }
    struct ScopedApprover {
        target: Value,
        tab: Value,
        deny: bool,
        actions: usize,
    }
    impl Approver for ScopedApprover {
        fn approve(&mut self, req: &ApprovalRequest) -> Approval {
            let a: Value = serde_json::from_str(&req.arguments).unwrap();
            if req.tool == "browser_workflow_run" && a["name"] == "fixture-reuse" {
                return Approval::Once;
            }
            if req.tool == "computer_act"
                && a["arguments"]["target_id"] == self.target
                && a["arguments"]["tab_id"] == self.tab
            {
                self.actions += 1;
                return if self.deny {
                    Approval::Deny
                } else {
                    Approval::Once
                };
            }
            Approval::Deny
        }
    }
    #[derive(Default)]
    struct Sink(Mutex<Vec<crate::tool::ToolCallRecord>>);
    impl crate::tool::ToolCallSink for Sink {
        fn record(&self, r: &crate::tool::ToolCallRecord) {
            self.0.lock().unwrap().push(r.clone());
        }
    }
    let mut registry = ToolRegistry::new();
    for t in ts {
        registry.register(Arc::new(t)).unwrap();
    }
    for denied in [false, true] {
        let sink = Arc::new(Sink::default());
        let mut approver = ScopedApprover {
            target: target.clone(),
            tab: tab.clone(),
            deny: denied,
            actions: 0,
        };
        let thinker = ReplayThinker {
            args: args.clone(),
            count: std::sync::atomic::AtomicUsize::new(0),
        };
        let mut layout = PromptLayout::new("synthetic workflow fixture");
        let outcome = ToolRunner::new(&registry, ToolPolicy::default(), &mut approver, ctx.clone())
            .with_sink(sink.clone())
            .run(&thinker, &mut layout, 100)
            .unwrap();
        let result = outcome
            .calls
            .iter()
            .find(|r| r.tool == "browser_workflow_run")
            .unwrap();
        if denied {
            assert!(!result.executed());
            assert_eq!(approver.actions, 1);
        } else {
            assert!(result.executed(), "{:?}", result.output);
            assert_eq!(approver.actions, 3);
        }
        let audit = serde_json::to_string(&*sink.0.lock().unwrap()).unwrap();
        assert!(!audit.contains("北京") && !audit.contains("input_1") && !audit.contains("计划："));
        assert_eq!(thinker.count.load(Ordering::Relaxed), 2);
    }
    eprintln!(
        "REAL CDP PASS: learned 3 steps; replay changed 上海 to 北京; 3 separate action approvals; denied replay stopped at first action; private inputs absent from audit; zero model calls inside replay"
    );
}

#[test]
#[ignore = "DeepSeek autonomously learns then replays only a local synthetic workflow; no recording"]
fn live_model_learns_browser_workflow() {
    let fixture = Fixture::with_page(
        r#"<!doctype html><meta charset="utf-8"><title>云熙自动学习验收</title><h1>流程学习就绪</h1><input aria-label="目的地" id="city"><button onclick="out.textContent='计划：'+city.value">生成预览</button><p id="out">未生成</p>"#,
    );
    fs::create_dir(fixture.root.join("data")).unwrap();
    let ts = tools_at(&fixture.root.join("data"));
    let mut ctx = ToolContext::new(&fixture.root, SandboxMode::WorkspaceWrite);
    let (_, _, target, tab) = prepare_browser(&fixture, &ts, &mut ctx, "流程学习就绪");
    struct Guard {
        inner: ComputerTool,
        target: Value,
        tab: Value,
        url: String,
    }
    impl Tool for Guard {
        fn name(&self) -> &str {
            self.inner.name()
        }
        fn description(&self) -> &str {
            self.inner.description()
        }
        fn parameters(&self) -> Value {
            self.inner.parameters()
        }
        fn capability(&self) -> Capability {
            self.inner.capability()
        }
        fn private_io(&self) -> bool {
            self.inner.private_io()
        }
        fn fresh_approval(&self) -> bool {
            self.inner.fresh_approval()
        }
        fn interaction_group(&self) -> Option<&'static str> {
            self.inner.interaction_group()
        }
        fn reset_observation(&self) -> Result<()> {
            self.inner.reset_observation()
        }
        fn preview(&self, a: &Value, c: &ToolContext) -> Option<String> {
            self.inner.preview(a, c)
        }
        fn preflight_rejection(&self, a: &Value, c: &ToolContext) -> Option<String> {
            let scoped = match self.name() {
                "computer_schema" => true,
                "computer_verify" => true,
                "computer_observe" => {
                    a["operation"] == "get_browser_state"
                        && a["arguments"]["target_id"] == self.target
                        && a["arguments"]["tab_id"] == self.tab
                }
                "computer_act" => {
                    a["arguments"]["target_id"] == self.target
                        && a["arguments"]["tab_id"] == self.tab
                        && match a["operation"].as_str() {
                            Some("browser_navigate") => a["arguments"]["url"] == self.url,
                            Some("browser_click" | "browser_type") => true,
                            _ => false,
                        }
                }
                "browser_workflow" => {
                    a["operation"] == "list"
                        || a["operation"] == "cancel"
                        || a["name"] == "model-fixture"
                }
                "browser_workflow_save" => a["success_text"] == "计划：上海",
                "browser_workflow_run" => {
                    a["name"] == "model-fixture"
                        && a["target_id"] == self.target
                        && a["tab_id"] == self.tab
                        && a["parameters"]["input_1"] == self.url
                        && a["parameters"]["input_2"] == "北京"
                }
                _ => false,
            };
            if !scoped {
                Some("outside_synthetic_learning_scope".into())
            } else {
                self.inner.preflight_rejection(a, c)
            }
        }
        fn call(&self, a: &Value, c: &mut ToolContext) -> Result<ToolOutput> {
            if let Some(e) = self.preflight_rejection(a, c) {
                return Err(bad(&e));
            }
            self.inner.call(a, c)
        }
    }
    struct Bounded;
    impl Approver for Bounded {
        fn approve(&mut self, r: &ApprovalRequest) -> Approval {
            if [
                "computer_act",
                "browser_workflow_save",
                "browser_workflow_run",
            ]
            .contains(&r.tool.as_str())
            {
                Approval::Once
            } else {
                Approval::Deny
            }
        }
    }
    let mut registry = ToolRegistry::new();
    for inner in ts {
        if !inner.name.starts_with("travel_") {
            registry
                .register(Arc::new(Guard {
                    inner,
                    target: target.clone(),
                    tab: tab.clone(),
                    url: fixture.url.clone(),
                }))
                .unwrap();
        }
    }
    let home = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../data");
    let thinker = PacedThinker(
        OpenAiThinker::from_home(&home, ThinkerConfig::deepseek()).unwrap(),
        std::sync::atomic::AtomicUsize::new(0),
    );
    let mut layout = PromptLayout::new(format!(
        "{}\n本轮仅可操作给定本机合成标签，禁止其他页面/原生窗口。",
        workflow::GUIDE
    ));
    layout.ask(format!("在当前target_id={target}、tab_id={tab}完成网页流程学习：候选命名model-fixture。导航URL={}，填写目的地上海并生成预览，读取实际‘计划：上海’后保存。随后inspect流程，用同一个URL、目的地北京、expected_result=计划：北京调用browser_workflow_run，验证参数化复用。由你调用工具完成学习和回放，不要用代码工具。",fixture.url));
    let start = Instant::now();
    let mut approver = Bounded;
    let outcome = ToolRunner::new(&registry, ToolPolicy::default(), &mut approver, ctx)
        .with_max_rounds(32)
        .with_thinking(Some(Thinking::Enabled))
        .with_sink(Arc::new(FixtureCallLog))
        .run(&thinker, &mut layout, 8192)
        .unwrap();
    assert!(
        outcome
            .calls
            .iter()
            .any(|r| r.tool == "browser_workflow_save" && r.executed()),
        "model did not save workflow"
    );
    assert!(
        outcome
            .calls
            .iter()
            .any(|r| r.tool == "browser_workflow_run" && r.executed()),
        "model did not replay workflow"
    );
    eprintln!(
        "MODEL LEARNING PASS: {} model rounds, {} audited calls, {:.1}s; learned 上海 and replayed 北京 via real CDP",
        outcome.model_calls,
        outcome.calls.len(),
        start.elapsed().as_secs_f64()
    );
}

#[test]
#[ignore = "Named owned Chromium profile survives session restart; synthetic localStorage only"]
fn live_owned_browser_profile_persists() {
    let fixture = Fixture::with_page(
        r#"<!doctype html><meta charset="utf-8"><title>云熙持久浏览器验收</title><h1>持久测试就绪</h1><p id="state"></p><script>const n=Number(localStorage.getItem('yunxi_fixture')||0)+1;localStorage.setItem('yunxi_fixture',String(n));state.textContent='访问次数：'+n;</script>"#,
    );
    let profile = format!(
        "yunxi-fixture-{}-{}",
        std::process::id(),
        crate::now_millis().unwrap()
    );
    for expected in [1, 2] {
        let ts = tools();
        let mut ctx = ToolContext::new(&fixture.root, SandboxMode::WorkspaceWrite);
        let (pid, window, target, tab) = prepare_browser_profile(
            &fixture,
            &ts,
            &mut ctx,
            "持久测试就绪",
            json!({"mode":"isolated_named","name":profile}),
        );
        let out = page(&ts, &mut ctx, &target, &tab);
        assert!(
            out.text.contains(&format!("访问次数：{expected}")),
            "persistent localStorage was not retained; expected={expected}, synthetic page={}",
            out.text
        );
        // Graceful quit flushes Chromium's pending storage writes; Driver session teardown is abrupt.
        observe(
            &ts,
            &mut ctx,
            "get_window_state",
            json!({"pid":pid,"window_id":window}),
        );
        act(
            &ts,
            &mut ctx,
            "hotkey",
            json!({"pid":pid,"window_id":window,"keys":["cmd","q"],"delivery_mode":"foreground"}),
        )
        .unwrap();
        for _ in 0..40 {
            let alive = std::process::Command::new("/bin/kill")
                .args(["-0", &pid.to_string()])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .unwrap()
                .success();
            if !alive {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        // Test-only cleanup of this test's own session, never a public automation operation.
        let mut c = ts[0].controller.lock().unwrap();
        let session = c.session.clone();
        let ended = driver_call(
            c.hub.as_mut().unwrap(),
            "end_session",
            json!({"session":session}),
        )
        .unwrap();
        parse_response(ended, false).unwrap();
    }
    let path = PathBuf::from(std::env::var_os("HOME").unwrap())
        .join("Library/Application Support/CuaDriver/BrowserProfiles")
        .join(&profile);
    if path.is_dir() && !path.is_symlink() {
        fs::remove_dir_all(path).unwrap();
    }
    eprintln!(
        "OWNED PROFILE PASS: two independent Driver sessions; same named profile retained synthetic localStorage; both owned sessions ended and test-only profile removed"
    );
}
