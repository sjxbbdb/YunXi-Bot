//! Opt-in live acceptance through ChatHandler; screen recording requires an explicit trusted flag.
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::PathBuf;
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use yunxi_bot_core::tool::{
    Approval, ApprovalRequest, Approver, Capability, Tool, ToolContext, ToolError, ToolOutput,
    ToolRegistry,
};

#[derive(Clone, Copy, PartialEq, Debug)]
enum Page {
    Query,
    Checkout,
    Unknown,
}
fn may_continue(summary: &str, turn: u32, calls: usize) -> bool {
    summary.contains("工具循环跑了 32 轮") && turn < 3 && calls < 200
}

fn page(url: &str) -> Page {
    let Some(rest) = url.strip_prefix("https://") else {
        return Page::Unknown;
    };
    let (host, tail) = rest.split_once('/').unwrap_or((rest, ""));
    let path = tail.split(['?', '#']).next().unwrap_or("");
    // shortcut: this scope covers the approved Beijing–Shanghai test; calibrate routes for other tests.
    match (host, path) {
        ("kyfw.12306.cn", "otn/leftTicket/init")
        | ("www.12306.cn", "index/" | "index/index.html")
        | ("trains.ctrip.com", "")
        | ("trains.ctrip.com", "trainbooking/beijing-shanghai")
        | ("hotels.ctrip.com", "" | "hotels/list" | "hotels/list/")
        | ("www.ctrip.com", "") => Page::Query,
        ("hotels.ctrip.com", p) if p.starts_with("hotels/detail/") || p.starts_with("hotel/") => {
            Page::Query
        }
        ("kyfw.12306.cn", "otn/confirmPassenger/initDc")
        | ("hotels.ctrip.com", "hotels/booknew") => Page::Checkout,
        _ => Page::Unknown,
    }
}

fn query_control(node: &Value, url: &str) -> bool {
    let name = node["name"].as_str().unwrap_or("").trim();
    if [
        "提交",
        "支付",
        "购买",
        "删除",
        "取消订单",
        "同意",
        "保险",
        "会员",
        "收藏",
        "登录",
        "注册",
    ]
    .iter()
    .any(|s| name.contains(s))
    {
        return false;
    }
    // 12306 gives each reservation link a full train/time label, not just "预订".
    if url.split(['?', '#']).next() == Some("https://kyfw.12306.cn/otn/leftTicket/init")
        && node["role"] == "link"
        && name.len() <= 500
        && name.ends_with("的车票")
        && name.contains("出发")
        && name.contains("历时")
        && name
            .strip_prefix("预订")
            .and_then(|s| s.split_once("次车"))
            .is_some_and(|(code, _)| {
                let digits = if code.as_bytes().first().is_some_and(u8::is_ascii_uppercase) {
                    &code[1..]
                } else {
                    code
                };
                (1..=5).contains(&digits.len()) && digits.bytes().all(|b| b.is_ascii_digit())
            })
    {
        return true;
    }
    matches!(
        node["role"].as_str(),
        Some("cell" | "gridcell" | "combobox" | "textbox" | "option")
    ) || [
        "查询",
        "搜索",
        "预订",
        "领券订",
        "选择房型",
        "查看详情",
        "确定",
        "完成",
        "下一月",
        "上一月",
        "下一页",
        "北京南",
        "上海虹桥",
        "上海",
    ]
    .contains(&name)
        || (node["role"] == "link"
            && node["href"]
                .as_str()
                .is_some_and(|url| page(url) == Page::Query))
}

struct Recording {
    child: Child,
    output: BufReader<ChildStdout>,
    path: PathBuf,
}
impl Recording {
    fn stop(mut self) {
        if let Some(mut input) = self.child.stdin.take() {
            let _ = input.write_all(b"stop\n");
        }
        let mut output = String::new();
        let read = self.output.read_to_string(&mut output);
        let status = self.child.wait();
        let valid = read.is_ok()
            && status.as_ref().is_ok_and(|s| s.success())
            && output.contains("FINISHED duration=");
        eprintln!(
            "RECORDING_STOPPED {} validated={valid} status={status:?} {}",
            self.path.display(),
            output.trim()
        );
    }
}

struct Scope {
    root: PathBuf,
    started: Instant,
    pid: Option<u64>,
    existing: Option<(u64, u64)>,
    recording: bool,
    bound_windows: BTreeSet<u64>,
    windows: BTreeMap<u64, Recording>,
    targets: BTreeMap<String, u64>,
    frame: Value,
    calls: usize,
    paused: bool,
    human_once: Option<Value>,
    passenger: Option<String>,
}
impl Scope {
    fn within_budget(&self) -> bool {
        !self.paused && self.started.elapsed() <= Duration::from_secs(3600) && self.calls < 200
    }
    fn check_recordings(&mut self) {
        let ended = self
            .windows
            .iter_mut()
            .filter_map(|(w, r)| (!matches!(r.child.try_wait(), Ok(None))).then_some(*w))
            .collect::<Vec<_>>();
        for window in ended {
            let current = self.frame["target_id"]
                .as_str()
                .and_then(|t| self.targets.get(t))
                .copied()
                == Some(window);
            if let Some(recording) = self.windows.remove(&window) {
                recording.stop();
            }
            self.targets.retain(|_, w| *w != window);
            self.bound_windows.remove(&window);
            if current || self.windows.is_empty() {
                self.paused = true;
                eprintln!("RECORDING_INTERRUPTED: computer actions paused");
            }
        }
    }
    fn permitted(&self, req: &Value) -> bool {
        if !self.within_budget() {
            return false;
        }
        let a = &req["arguments"];
        match req["operation"].as_str().unwrap_or("") {
            "browser_prepare" => {
                let mut args = a.clone();
                if matches!(
                    args["delivery_mode"].as_str(),
                    Some("foreground" | "background")
                ) && let Some(fields) = args.as_object_mut()
                {
                    fields.remove("delivery_mode");
                }
                if let Some((pid, window)) = self.existing {
                    return args
                        == json!({"pid":pid,"window_id":window,"strategy":{"kind":"existing_profile"}});
                }
                self.pid.is_none()
                    && args
                        == json!({"allow_launch":true,"profile":{"mode":"isolated_named","name":"yunxi-recorded-travel"}})
            }
            "bring_to_front" | "set_window_frame" => {
                a["pid"].as_u64() == self.pid
                    && a["window_id"].as_u64().is_some_and(|w| {
                        self.bound_windows.contains(&w)
                            && (!self.recording || self.windows.contains_key(&w))
                    })
            }
            "press_key" | "hotkey" | "invoke_menu" => {
                let Some(pid) = self.pid else { return false };
                if a.get("pid")
                    .or_else(|| a["target"].get("pid"))
                    .and_then(Value::as_u64)
                    != Some(pid)
                {
                    return false;
                }
                let Some(window) = a
                    .get("window_id")
                    .or_else(|| a["target"].get("window_id"))
                    .and_then(Value::as_u64)
                    .filter(|w| {
                        self.bound_windows.contains(w)
                            && (!self.recording || self.windows.contains_key(w))
                    })
                else {
                    return false;
                };
                if req["operation"] == "invoke_menu" {
                    return a
                        == &json!({"pid":pid,"window_id":window,"path":["文件","新建标签页"]});
                }
                let mut args = a.clone();
                let Some(fields) = args.as_object_mut() else {
                    return false;
                };
                if let Some(target) = fields.get("target") {
                    if target != &json!({"kind":"window","pid":pid,"window_id":window}) {
                        return false;
                    }
                    fields.remove("target");
                    fields.entry("pid").or_insert(json!(pid));
                    fields.entry("window_id").or_insert(json!(window));
                }
                if fields.get("scope") == Some(&json!("window")) {
                    fields.remove("scope");
                }
                if matches!(
                    fields.get("delivery_mode").and_then(Value::as_str),
                    Some("foreground" | "background")
                ) {
                    fields.remove("delivery_mode");
                }
                if req["operation"] == "press_key" {
                    args == json!({"pid":pid,"window_id":window,"key":"t","modifiers":["cmd"]})
                } else {
                    args == json!({"pid":pid,"window_id":window,"keys":["cmd","t"]})
                }
            }
            "browser_navigate" => {
                self.bound(a) && a["url"].as_str().is_some_and(|u| page(u) == Page::Query)
            }
            "browser_pointer" if a["action"] == "scroll" => {
                self.bound(a) && self.current_page() != Page::Unknown
            }
            "browser_type" => {
                self.bound(a)
                    && self.current_page() != Page::Unknown
                    && self.node(a).is_some_and(|n| {
                        matches!(n["role"].as_str(), Some("textbox" | "combobox"))
                            && !["密码", "验证", "支付", "银行卡"]
                                .iter()
                                .any(|s| n["name"].as_str().unwrap_or("").contains(s))
                    })
                    && a["text"].as_str().is_some_and(|s| {
                        s.chars().count() <= 200 && !s.chars().any(char::is_control)
                    })
            }
            "browser_click" => {
                self.bound(a)
                    && a.get("x").is_none()
                    && a.get("y").is_none()
                    && self.node(a).is_some_and(|n| {
                        (self.current_page() == Page::Query && query_control(n, self.current_url()))
                            || (self.current_page() == Page::Checkout && self.passenger_control(n))
                    })
            }
            "browser_pointer" if a["action"] == "double_click" => {
                self.bound(a)
                    && self.current_page() == Page::Query
                    && self
                        .node(a)
                        .is_some_and(|n| matches!(n["role"].as_str(), Some("textbox" | "combobox")))
            }
            _ => false,
        }
    }
    fn bound(&self, a: &Value) -> bool {
        a["target_id"].as_str().is_some_and(|t| {
            self.targets.get(t).is_some_and(|w| {
                self.bound_windows.contains(w) && (!self.recording || self.windows.contains_key(w))
            })
        }) && a["target_id"] == self.frame["target_id"]
            && (a["tab_id"] == self.frame["tab_id"]
                || self.frame["tabs"].as_array().is_some_and(|tabs| {
                    a["tab_id"].is_string() && tabs.iter().any(|t| t["tab_id"] == a["tab_id"])
                }))
    }
    fn current_page(&self) -> Page {
        page(self.current_url())
    }
    fn current_url(&self) -> &str {
        self.frame["url"]
            .as_str()
            .or_else(|| self.frame["page"]["url"].as_str())
            .unwrap_or("")
    }
    fn named_passenger(&self, node: &Value) -> bool {
        self.passenger.as_deref().is_some_and(|person| {
            node["name"] == person
                || node["name"] == format!("{person}（学生）")
                || node["name"] == format!("{person}(学生)")
        })
    }
    fn passenger_control(&self, node: &Value) -> bool {
        if !matches!(
            node["role"].as_str(),
            Some("checkbox" | "labeltext" | "statictext")
        ) {
            return false;
        }
        if node["role"] == "checkbox" && self.named_passenger(node) {
            return true;
        }
        if self.current_url().split(['?', '#']).next()
            != Some("https://kyfw.12306.cn/otn/confirmPassenger/initDc")
            || self.frame["snapshot"]["scope"] != "subtree"
            || self.frame["snapshot"]["complete"] != true
        {
            return false;
        }
        // Only one complete passenger list item may associate an otherwise unnamed checkbox.
        let nodes = ["refs", "content_refs"]
            .iter()
            .flat_map(|key| self.frame[key].as_array().into_iter().flatten())
            .collect::<Vec<_>>();
        nodes.len() == 4
            && ["checkbox", "labeltext", "statictext", "listitem"]
                .iter()
                .all(|role| nodes.iter().filter(|n| n["role"] == *role).count() == 1)
            && nodes
                .iter()
                .any(|n| n["role"] == "statictext" && self.named_passenger(n))
    }
    fn node(&self, a: &Value) -> Option<&Value> {
        let reference = a["ref"].as_str()?;
        self.frame["refs"]
            .as_array()?
            .iter()
            .find(|n| n["ref"] == reference)
    }
    fn record(&mut self, window: u64) -> std::io::Result<()> {
        if !self.recording {
            self.bound_windows.insert(window);
            return Ok(());
        }
        if self.windows.contains_key(&window) {
            return Ok(());
        }
        let path = self.root.join(format!(
            "window-{window}-{}.mp4",
            yunxi_bot_core::now_millis().map_err(std::io::Error::other)?
        ));
        let helper = std::env::var_os("YUNXI_TEST_RECORDER")
            .ok_or_else(|| std::io::Error::other("explicit tested recorder required"))?;
        let mut child = Command::new(helper)
            .arg(window.to_string())
            .arg(&path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()?;
        let mut output = BufReader::new(
            child
                .stdout
                .take()
                .ok_or_else(|| std::io::Error::other("recorder output missing"))?,
        );
        let mut line = String::new();
        output.read_line(&mut line)?;
        if line.trim() != "READY" {
            let _ = child.wait();
            return Err(std::io::Error::other(
                "Window recording did not become ready",
            ));
        }
        eprintln!("RECORDING_STARTED {}", path.display());
        self.windows.insert(
            window,
            Recording {
                child,
                path,
                output,
            },
        );
        self.bound_windows.insert(window);
        Ok(())
    }
    fn stop_recordings(&mut self) {
        for recording in std::mem::take(&mut self.windows).into_values() {
            recording.stop();
        }
    }
}
impl Drop for Scope {
    fn drop(&mut self) {
        self.stop_recordings();
    }
}

struct ScopedApprover(Arc<Mutex<Scope>>);
impl Approver for ScopedApprover {
    fn approve(&mut self, req: &ApprovalRequest) -> Approval {
        self.0.lock().unwrap().check_recordings();
        let permitted = req.tool == "computer_act"
            && serde_json::from_str::<Value>(&req.arguments)
                .ok()
                .is_some_and(|v| self.0.lock().unwrap().permitted(&v));
        if permitted {
            return Approval::Once;
        }
        let value = serde_json::from_str::<Value>(&req.arguments).unwrap_or(Value::Null);
        let checkout = {
            let s = self.0.lock().unwrap();
            s.within_budget()
                && s.current_page() == Page::Checkout
                && s.bound(&value["arguments"])
                && value["operation"] == "browser_click"
                && value["arguments"]["ref"].is_string()
        };
        if checkout {
            eprintln!(
                "HUMAN_CHECKOUT_APPROVAL_REQUIRED: only the human may authorize this exact action"
            );
            let result = crate::approval::StdinApprover::new().approve(req);
            if result == Approval::Once {
                self.0.lock().unwrap().human_once = Some(value);
            }
            result
        } else {
            let s = self.0.lock().unwrap();
            eprintln!(
                "SCOPE_HOLD: budget={} calls={} bound={} target_match={} tab_match={} page={:?}; no automatic approval",
                s.within_budget(),
                s.calls,
                s.bound(&value["arguments"]),
                value["arguments"]["target_id"] == s.frame["target_id"],
                value["arguments"]["tab_id"] == s.frame["tab_id"],
                s.current_page()
            );
            let node = s.node(&value["arguments"]);
            eprintln!(
                "SCOPE_SHAPE: browser_click={} browser_pointer={} browser_navigate={} ref_known={} query_control={} coordinates={} pointer_double_click={}",
                value["operation"] == "browser_click",
                value["operation"] == "browser_pointer",
                value["operation"] == "browser_navigate",
                node.is_some(),
                node.is_some_and(|n| query_control(n, s.current_url())),
                value["arguments"].get("x").is_some() || value["arguments"].get("y").is_some(),
                value["arguments"]["action"] == "double_click"
            );
            if value["operation"] == "browser_prepare" {
                eprintln!(
                    "PREPARE_SHAPE keys={:?} launch={:?} mode={:?} expected_name={} delivery={:?}",
                    value["arguments"]
                        .as_object()
                        .map(|a| a.keys().collect::<Vec<_>>()),
                    value["arguments"]["allow_launch"].as_bool(),
                    value["arguments"]["profile"]["mode"].as_str().filter(|s| [
                        "isolated_named",
                        "isolated_new"
                    ]
                    .contains(s)),
                    value["arguments"]["profile"]["name"] == "yunxi-recorded-travel",
                    value["arguments"]["delivery_mode"].as_str().filter(|s| [
                        "foreground",
                        "background"
                    ]
                    .contains(s))
                );
            }
            Approval::Deny
        }
    }
}

struct RecordedTool {
    inner: yunxi_bot_core::tool::computer::ComputerTool,
    scope: Arc<Mutex<Scope>>,
}
impl RecordedTool {
    fn scope_rejection(&self, args: &Value) -> Option<String> {
        if self.name() != "computer_act"
            || !matches!(
                args["operation"].as_str(),
                Some("browser_click" | "browser_type" | "browser_navigate" | "browser_pointer")
            )
        {
            return None;
        }
        let scope = self.scope.lock().unwrap();
        if !scope.bound(&args["arguments"]) {
            return Some("test_scope_binding_mismatch：本轮查询已获授权，但动作的target_id/tab_id未匹配最新实际绑定，不是用户拒绝。重新get_browser_state核对返回的精确target_id及实际tab_id，再读该标签后使用最新ref；不得猜标识、扩大窗口或请求重复授权。".into());
        }
        if scope.current_page() == Page::Checkout
            && args["operation"] == "browser_click"
            && scope.node(&args["arguments"]).is_some_and(|node| {
                (matches!(node["role"].as_str(), Some("checkbox" | "labeltext"))
                    || (node["role"] == "statictext" && scope.named_passenger(node)))
                    && !scope.passenger_control(node)
            })
        {
            return Some("test_scope_passenger_context_required：尚无法将此复选框关联到用户已明确的本人，不是用户拒绝。姓名与复选框分离时，用实际乘车人listitem的scope_ref读取完整子树；必须只有一人、一复选框及姓名标签，再用新ref。不能按列表顺序、坐标或动作目的推断人员。".into());
        }
        (scope.current_page() == Page::Query && !scope.permitted(args)).then(||
                "test_scope_action_unmatched：本轮查询已获授权，但该动作/ref/路径未匹配有限测试守卫，不是用户拒绝。不支持坐标或猜测路径，先重新观察当前实际控件与actions；无可用的已接入路径时明确报告适配限制，不要求用户重复授权。".into())
    }
}
impl Tool for RecordedTool {
    fn name(&self) -> &str {
        self.inner.name()
    }
    fn description(&self) -> &str {
        if self.name() == "computer_act" {
            if self.scope.lock().unwrap().existing.is_some() {
                return "本轮用户已授权连接其日常Edge的指定pid/window_id；browser_prepare仅给该精确pid/window_id及strategy={kind:existing_profile}，不加profile或allow_launch。已有窗口不复制Cookie、不重启，不猜其他窗口。成功绑定后先用target_id/tab_id读真实页面。已绑定窗口可先get_window_state，再press_key(key=t,modifiers=[cmd])新建标签，快捷键不可达时可在同一窗口invoke_menu path=[文件,新建标签页]，仅这个精确菜单项预批准，其余快捷键或菜单不预批准。沿用已审核的12306/携程查询与填写范围，未知动作不自动批准；下单须实际报价核对后逐次人工批准，密码/验证码/付款交用户。开发默认不录屏，真实窗口绑定仍必须校验。";
            }
            "本次开发验收的有限授权电脑动作，仍需最新observation_id及每次重新观察。browser_prepare仅allow_launch=true及profile={mode:isolated_named,name:yunxi-recorded-travel}，可加foreground。仅操作本轮pid/target/tab。允许12306车票查询和携程酒店查询/房型页上的textbox/combobox/option/gridcell、明确的查询/搜索/预订/领券订/查看详情/选择房型/日期翻页，以及北京南/上海虹桥/上海候选；browser_type不可带回车，scroll可用browser_pointer。已绑定窗口可先get_window_state，再press_key(key=t,modifiers=[cmd])新建标签，快捷键不可达时可在同一窗口invoke_menu path=[文件,新建标签页]，仅这个精确菜单项预批准，其余快捷键或菜单不预批准。仅结算页用户已明确的本人checkbox可预批准，提交订单仍人工批准。未知/坐标点击不放行；返回人工拒绝可能是测试守卫阻断，不代表用户否认整项任务。不要重复被拒调用或幻觉新的operation，改用已提供合法路径或报告阻断。密码、验证码、支付由用户处理。"
        } else {
            self.inner.description()
        }
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
        self.scope_rejection(args)
            .or_else(|| self.inner.preflight_rejection(args, ctx))
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
    fn reset_observation(&self) -> Result<(), ToolError> {
        self.inner.reset_observation()
    }
    fn audit_arguments(&self, args: &Value) -> Value {
        self.inner.audit_arguments(args)
    }
    fn preview(&self, args: &Value, ctx: &ToolContext) -> Option<String> {
        self.inner.preview(args, ctx)
    }
    fn call(&self, req: &Value, ctx: &mut ToolContext) -> Result<ToolOutput, ToolError> {
        let op = req["operation"].as_str().unwrap_or("");
        let a = &req["arguments"];
        {
            let mut scope = self.scope.lock().unwrap();
            if !scope.within_budget() {
                return Err(ToolError::Denied {
                    reason: "本轮验收已暂停或达到时间/调用上限，未执行".into(),
                });
            }
            if self.name() == "computer_act" {
                scope.check_recordings();
                let human = scope.human_once.take();
                if !scope.permitted(req) && human.as_ref() != Some(req) {
                    return Err(ToolError::Denied {
                        reason: "超出本轮查询与填写授权，未执行".into(),
                    });
                }
            }
            if self.name() == "computer_observe" {
                let owned = a["pid"].as_u64().is_some_and(|pid| Some(pid) == scope.pid);
                let target = a["target_id"]
                    .as_str()
                    .is_some_and(|t| scope.targets.contains_key(t));
                let allowed = match op {
                    "check_permissions" => true,
                    "list_windows" => {
                        owned
                            || ((scope.pid.is_none() || scope.existing.is_some())
                                && a["app"] == "Microsoft Edge")
                    }
                    "get_window_state" | "get_accessibility_tree" => {
                        owned
                            && a["window_id"].is_u64()
                            && scope
                                .existing
                                .is_none_or(|(_, w)| a["window_id"].as_u64() == Some(w))
                    }
                    "get_browser_state" => {
                        (owned
                            && scope
                                .existing
                                .is_none_or(|(_, w)| a["window_id"].as_u64() == Some(w)))
                            || target
                    }
                    _ => false,
                };
                if !allowed {
                    return Err(ToolError::Denied {
                        reason: "仅允许观察本轮明确授权的浏览器窗口".into(),
                    });
                }
            }
            scope.calls += 1;
        }
        let out = self.inner.call(req, ctx);
        eprintln!(
            "BOT_TOOL {} {op} {}",
            self.name(),
            if out.is_ok() { "returned" } else { "failed" }
        );
        if let Ok(output) = &out {
            let meta = output
                .text
                .rsplit_once("结构化回执：")
                .and_then(|(_, s)| serde_json::from_str::<Value>(s).ok());
            if let Some(mut meta) = meta {
                let mut scope = self.scope.lock().unwrap();
                if op == "browser_prepare" {
                    scope.pid = meta["prepared_pid"].as_u64().or(scope.pid);
                    eprintln!(
                        "BROWSER_LIFECYCLE idle_expires_in_seconds={:?}",
                        meta["lifecycle"]["idle_expires_in_seconds"].as_u64()
                    );
                }
                if op == "get_browser_state" {
                    if let (Some(target), Some(window)) =
                        (meta["target_id"].as_str(), a["window_id"].as_u64())
                    {
                        if !scope.paused {
                            scope.record(window).map_err(|_| ToolError::Failed {
                                detail: "录屏未启动，停止操控".into(),
                            })?;
                        }
                        scope.targets.insert(target.into(), window);
                    }
                    for key in ["target_id", "tab_id"] {
                        if meta.get(key).is_none() {
                            meta[key] = a[key].clone();
                        }
                    }
                    scope.frame = meta;
                    eprintln!("BROWSER_STATE page={:?}", scope.current_page());
                }
            }
        }
        out
    }
}

#[test]
fn ctrip_rail_queries_do_not_open_unknown_or_transaction_routes() {
    for url in [
        "https://trains.ctrip.com/",
        "https://trains.ctrip.com/trainbooking/beijing-shanghai?date=2026-10-20",
    ] {
        assert_eq!(page(url), Page::Query, "{url}");
    }
    for url in [
        "https://trains.ctrip.com/booking",
        "https://trains.ctrip.com/order/detail",
        "https://trains.ctrip.com/trainbooking/submit",
        "https://trains.ctrip.com/trainbooking/submit-order",
        "https://trains.ctrip.com/trainbooking/shanghai-beijing",
        "https://trains.ctrip.com/trainbooking/beijing-shanghai/submit",
        "https://trains.ctrip.com.attacker.example/",
        "https://user@trains.ctrip.com/",
    ] {
        assert_eq!(page(url), Page::Unknown, "{url}");
    }
}

#[test]
fn rail_query_accepts_the_real_accessible_reservation_name_only_on_the_query_page() {
    let node = json!({"role":"link","name":"预订G8888次车06:30从甲站出发，11:24到乙站,当日到达历时04:54的车票"});
    let url = "https://kyfw.12306.cn/otn/leftTicket/init?date=2026-10-20";
    let mut scope = Scope {
        root: PathBuf::new(),
        started: Instant::now(),
        pid: Some(123),
        existing: None,
        recording: false,
        bound_windows: BTreeSet::from([1]),
        windows: BTreeMap::new(),
        targets: BTreeMap::from([("t".into(), 1)]),
        frame: json!({"target_id":"t","tab_id":"tab","url":url,"refs":[]}),
        calls: 0,
        paused: false,
        human_once: None,
        passenger: None,
    };
    scope.frame["refs"] = json!([{ "ref":"r", "role":node["role"], "name":node["name"] }]);
    let click =
        json!({"operation":"browser_click","arguments":{"target_id":"t","tab_id":"tab","ref":"r"}});
    assert!(scope.permitted(&click));
    for wrong in [
        "https://trains.ctrip.com/",
        "https://kyfw.12306.cn/otn/confirmPassenger/initDc",
        "https://kyfw.12306.cn.attacker.example/otn/leftTicket/init",
    ] {
        scope.frame["url"] = json!(wrong);
        assert!(!scope.permitted(&click));
    }
    scope.frame["url"] = json!(url);
    scope.frame["refs"][0]["name"] = json!("预订G8888次车并支付的车票");
    assert!(!scope.permitted(&click));
}

#[test]
fn mismatched_browser_binding_reports_a_test_guard_error_before_human_approval() {
    let scope = Arc::new(Mutex::new(Scope {
        root: PathBuf::new(),
        started: Instant::now(),
        pid: Some(123),
        existing: Some((123, 1)),
        recording: false,
        bound_windows: BTreeSet::from([1]),
        windows: BTreeMap::new(),
        targets: BTreeMap::from([("t".into(), 1)]),
        frame: json!({"target_id":"t","tab_id":"tab","url":"https://trains.ctrip.com/","refs":[{"ref":"r","role":"button","name":"搜索"}]}),
        calls: 0,
        paused: false,
        human_once: None,
        passenger: None,
    }));
    let inner = yunxi_bot_core::tool::computer::tools()
        .into_iter()
        .find(|t| t.name() == "computer_act")
        .unwrap();
    let tool = RecordedTool { inner, scope };
    let mut req = json!({"operation":"browser_click","arguments":{"target_id":"wrong","tab_id":"tab","ref":"r"}});
    let reason = tool.scope_rejection(&req).unwrap();
    assert!(reason.starts_with("test_scope_binding_mismatch"));
    assert!(reason.contains("不是用户拒绝"));
    assert!(!reason.contains("wrong"));
    req["arguments"]["target_id"] = json!("t");
    assert!(tool.scope_rejection(&req).is_none());
    req["arguments"]["ref"] = json!("unseen");
    assert!(
        tool.scope_rejection(&req)
            .unwrap()
            .starts_with("test_scope_action_unmatched")
    );
    req["arguments"]["ref"] = json!("r");
    req["arguments"]["tab_id"] = json!("other");
    assert!(tool.scope_rejection(&req).is_some());
    req["arguments"]["tab_id"] = json!("tab");
    {
        let mut scope = tool.scope.lock().unwrap();
        scope.frame["url"] = json!("https://kyfw.12306.cn/otn/confirmPassenger/initDc");
        scope.frame["refs"][0] = json!({"ref":"r","role":"checkbox","name":"按空格键进行操作"});
    }
    assert!(
        tool.scope_rejection(&req)
            .unwrap()
            .starts_with("test_scope_passenger_context_required")
    );
    {
        let mut scope = tool.scope.lock().unwrap();
        scope.passenger = Some("合成乘客甲".into());
        scope.frame["refs"][0] = json!({"ref":"r","role":"statictext","name":"合成乘客甲(学生)"});
    }
    assert!(
        tool.scope_rejection(&req)
            .unwrap()
            .starts_with("test_scope_passenger_context_required")
    );
}

#[test]
fn recorded_scope_refuses_orders_unknown_pages_unbound_targets_and_hidden_submit_keys() {
    assert!(may_continue("工具循环跑了 32 轮仍未结束", 1, 100));
    assert!(!may_continue("delivery_unknown", 1, 100));
    assert!(!may_continue("工具循环跑了 32 轮", 3, 100));
    assert!(!may_continue("工具循环跑了 32 轮", 1, 200));
    let mut scope = Scope {
        root: PathBuf::new(),
        started: Instant::now(),
        pid: Some(123),
        existing: None,
        recording: true,
        bound_windows: BTreeSet::from([1]),
        windows: BTreeMap::new(),
        targets: BTreeMap::from([("t".into(), 1)]),
        frame: json!({"target_id":"t","tab_id":"tab","url":"https://kyfw.12306.cn/otn/leftTicket/init","refs":[{"ref":"r","role":"button","name":"查询"},{"ref":"p","role":"button","name":"提交订单"},{"ref":"i","role":"textbox","name":"出发地"}]}),
        calls: 0,
        paused: false,
        human_once: None,
        passenger: None,
    };
    let click =
        json!({"operation":"browser_click","arguments":{"target_id":"t","tab_id":"tab","ref":"r"}});
    let mut live = Command::new("/bin/cat")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let output = BufReader::new(live.stdout.take().unwrap());
    scope.windows.insert(
        1,
        Recording {
            child: live,
            output,
            path: PathBuf::new(),
        },
    );
    assert!(scope.permitted(&click));
    scope.frame["refs"][0] = json!({"ref":"r","role":"cell","name":"G7次列车，二等座票价661元，余票有","actions":["click","pointer"]});
    assert!(scope.permitted(&click));
    scope.frame["refs"][0]["name"] = json!("支付订单");
    assert!(!scope.permitted(&click));
    scope.frame["refs"][0] = json!({"ref":"r","role":"button","name":"查询"});
    scope.calls = 200;
    assert!(!scope.permitted(&click));
    scope.calls = 0;
    let started = scope.started;
    scope.started = Instant::now() - Duration::from_secs(3601);
    assert!(!scope.permitted(&click));
    scope.started = started;
    scope.frame["refs"][0]["name"] = json!("北京南");
    scope.frame["refs"][0]["role"] = json!("link");
    assert!(scope.permitted(&click));
    scope.frame["refs"][0] = json!({"ref":"r","role":"button","name":"查询"});
    let url = scope.frame.as_object_mut().unwrap().remove("url").unwrap();
    scope.frame["page"] = json!({"url":url});
    assert!(scope.permitted(&click));
    scope.frame["url"] = scope.frame["page"]["url"].clone();
    let mut wrong = click.clone();
    wrong["arguments"]["tab_id"] = json!("other");
    assert!(!scope.permitted(&wrong));
    wrong = click.clone();
    wrong["arguments"]["ref"] = json!("p");
    assert!(!scope.permitted(&wrong));
    wrong = click.clone();
    wrong["arguments"]["x"] = json!(10);
    assert!(!scope.permitted(&wrong));
    assert!(!scope.permitted(&json!({"operation":"browser_type","arguments":{"target_id":"t","tab_id":"tab","ref":"i","text":"上海\n"}})));
    scope.frame["url"] = json!("https://kyfw.12306.cn/otn/confirmPassenger/initDc");
    assert!(!scope.permitted(&click));
    scope.passenger = Some("合成乘客甲".into());
    scope.frame["refs"] = json!([{"ref":"r","role":"checkbox","name":"合成乘客甲"}]);
    assert!(scope.permitted(&click));
    scope.frame["refs"][0]["name"] = json!("合成乘客乙");
    assert!(!scope.permitted(&click));
    scope.frame["refs"] = json!([
        {"ref":"r","role":"checkbox","name":"按空格键进行操作"},
        {"ref":"label","role":"labeltext"},
        {"ref":"person","role":"statictext","name":"合成乘客甲（学生）"}
    ]);
    scope.frame["content_refs"] = json!([{"ref":"row","role":"listitem"}]);
    scope.frame["snapshot"] = json!({"scope":"subtree","complete":true});
    assert!(
        scope.permitted(&click),
        "one actual passenger row binds the unnamed checkbox"
    );
    let mut label_click = click.clone();
    label_click["arguments"]["ref"] = json!("person");
    assert!(
        scope.permitted(&label_click),
        "the actual name label selects the same passenger"
    );
    scope.frame["snapshot"]["scope"] = json!("viewport");
    assert!(!scope.permitted(&click));
    scope.frame["snapshot"]["scope"] = json!("subtree");
    scope.frame["snapshot"]["complete"] = json!(false);
    assert!(!scope.permitted(&click));
    scope.frame["snapshot"]["complete"] = json!(true);
    scope.frame["refs"][2]["name"] = json!("合成乘客乙（学生）");
    assert!(!scope.permitted(&click));
    scope.frame["refs"][2]["name"] = json!("合成乘客甲（学生）");
    scope.frame["content_refs"]
        .as_array_mut()
        .unwrap()
        .push(json!({"ref":"other","role":"checkbox"}));
    assert!(!scope.permitted(&click));
    scope.frame["content_refs"] = json!([{"ref":"row","role":"listitem"}]);
    scope.frame["url"] = json!("https://hotels.ctrip.com/hotels/booknew");
    assert!(!scope.permitted(&click));
    scope.frame["url"] = json!("https://kyfw.12306.cn.attacker.example/otn/leftTicket/init");
    assert!(!scope.permitted(&click));
    assert_eq!(
        page("https://user@kyfw.12306.cn/otn/leftTicket/init"),
        Page::Unknown
    );
    assert_eq!(
        page("https://hotels.ctrip.com/hotels/booknew?x=1"),
        Page::Checkout
    );
    scope.frame = json!({"target_id":"t","tabs":[{"tab_id":"tab","url":"about:blank"}]});
    assert!(scope.permitted(&json!({"operation":"browser_navigate","arguments":{"target_id":"t","tab_id":"tab","url":"https://kyfw.12306.cn/otn/leftTicket/init"}})));
    assert!(!scope.permitted(&click));
    scope.pid = None;
    assert!(scope.permitted(&json!({"operation":"browser_prepare","arguments":{"allow_launch":true,"profile":{"mode":"isolated_named","name":"yunxi-recorded-travel"},"delivery_mode":"foreground"}})));
    assert!(!scope.permitted(&json!({"operation":"browser_prepare","arguments":{"allow_launch":true,"profile":{"mode":"isolated_named","name":"other"}}})));
    scope.existing = Some((123, 1));
    let attach = json!({"operation":"browser_prepare","arguments":{"pid":123,"window_id":1,"strategy":{"kind":"existing_profile"}}});
    assert!(scope.permitted(&attach));
    let mut other = attach.clone();
    other["arguments"]["window_id"] = json!(2);
    assert!(!scope.permitted(&other));
    scope.existing = None;
    if let Some(mut r) = scope.windows.remove(&1) {
        r.child.stdin.take();
        r.child.wait().unwrap();
    }
    scope.recording = false;
    scope.pid = Some(123);
    scope.frame = json!({"target_id":"t","tab_id":"tab","url":"https://kyfw.12306.cn/otn/leftTicket/init","refs":[{"ref":"r","role":"button","name":"查询"}]});
    scope.record(1).unwrap();
    assert!(
        scope.windows.is_empty(),
        "ordinary development must not spawn a recorder"
    );
    assert!(scope.permitted(&click));
    let new_tab = json!({"operation":"press_key","arguments":{"pid":123,"window_id":1,"key":"t","modifiers":["cmd"]}});
    assert!(scope.permitted(&new_tab));
    let mut explicit_target = new_tab.clone();
    explicit_target["arguments"]["target"] = json!({"kind":"window","pid":123,"window_id":1});
    assert!(scope.permitted(&explicit_target));
    explicit_target["arguments"]["target"]["window_id"] = json!(2);
    assert!(!scope.permitted(&explicit_target));
    let target_only = json!({"operation":"press_key","arguments":{"target":{"kind":"window","pid":123,"window_id":1},"key":"t","modifiers":["cmd"],"scope":"window"}});
    assert!(scope.permitted(&target_only));
    let mut conflicting = target_only.clone();
    conflicting["arguments"]["pid"] = json!(456);
    assert!(!scope.permitted(&conflicting));
    conflicting = target_only.clone();
    conflicting["arguments"]["target"]["unexpected"] = json!(true);
    assert!(!scope.permitted(&conflicting));
    let chord = json!({"operation":"hotkey","arguments":{"pid":123,"window_id":1,"keys":["cmd","t"],"delivery_mode":"foreground"}});
    assert!(scope.permitted(&chord));
    let mut menu = json!({"operation":"invoke_menu","arguments":{"pid":123,"window_id":1,"path":["文件","新建标签页"]}});
    assert!(scope.permitted(&menu));
    menu["arguments"]["path"] = json!(["文件", "关闭窗口"]);
    assert!(!scope.permitted(&menu));
    for (key, value) in [
        ("pid", json!(456)),
        ("window_id", json!(2)),
        ("key", json!("w")),
        ("modifiers", json!(["cmd", "shift"])),
        ("x", json!(10)),
        ("scope", json!("desktop")),
        ("target", json!({"kind":"desktop"})),
    ] {
        let mut wrong = new_tab.clone();
        wrong["arguments"][key] = value;
        assert!(
            !scope.permitted(&wrong),
            "unapproved new-tab variant: {key}"
        );
    }
    scope.bound_windows.clear();
    assert!(!scope.permitted(&new_tab));
    scope.bound_windows.insert(1);
    scope.recording = true;
    assert!(
        !scope.permitted(&click),
        "explicit recording still requires an active capture"
    );
    let mut child = Command::new("/usr/bin/true")
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let output = BufReader::new(child.stdout.take().unwrap());
    child.wait().unwrap();
    scope.windows.insert(
        1,
        Recording {
            child,
            output,
            path: PathBuf::new(),
        },
    );
    scope.check_recordings();
    assert!(
        scope.paused,
        "stopped recording must pause computer actions"
    );
    assert!(!scope.permitted(&json!({"operation":"browser_prepare","arguments":{"allow_launch":true,"profile":{"mode":"isolated_named","name":"yunxi-recorded-travel"}}})));
    scope.windows.clear();
}

#[test]
#[ignore = "Real private account UI + DeepSeek; explicit private test directory required; recording only with YUNXI_RECORD_SCREEN=1"]
fn live_bot_travel() -> Result<(), Box<dyn std::error::Error>> {
    let root = PathBuf::from(
        std::env::var_os("YUNXI_RECORDED_TRAVEL_DIR")
            .ok_or("explicit output directory required")?,
    );
    fs::DirBuilder::new().mode(0o700).create(&root)?;
    let home = crate::default_home();
    let recording = match std::env::var_os("YUNXI_RECORD_SCREEN") {
        None => false,
        Some(v) if v == "0" => false,
        Some(v) if v == "1" => true,
        _ => return Err("YUNXI_RECORD_SCREEN only accepts 0 or 1".into()),
    };
    let existing = match (
        std::env::var("YUNXI_TEST_EDGE_PID").ok(),
        std::env::var("YUNXI_TEST_EDGE_WINDOW").ok(),
    ) {
        (None, None) => None,
        (Some(pid), Some(window)) => Some((pid.parse::<u64>()?, window.parse::<u64>()?)),
        _ => return Err("both exact Edge pid and window are required".into()),
    };
    let scope = Arc::new(Mutex::new(Scope {
        root: root.clone(),
        started: Instant::now(),
        pid: existing.map(|(pid, _)| pid),
        existing,
        recording,
        bound_windows: BTreeSet::new(),
        windows: BTreeMap::new(),
        targets: BTreeMap::new(),
        frame: Value::Null,
        calls: 0,
        paused: false,
        human_once: None,
        passenger: std::env::var("YUNXI_TEST_PASSENGER").ok(),
    }));
    let mut registry = ToolRegistry::new();
    for inner in yunxi_bot_core::tool::computer::tools() {
        registry.register(Arc::new(RecordedTool {
            inner,
            scope: scope.clone(),
        }))?;
    }
    registry.register(Arc::new(yunxi_bot_core::tool::system::NowTool))?;
    registry.register(Arc::new(yunxi_bot_core::tool::travel::TravelPrepare))?;
    let mut handler = crate::chat_handler::ChatHandler::new(
        home,
        "云熙",
        crate::chat_handler::DEFAULT_PERSONA,
        crate::chat_handler::default_rules(),
    )
    .with_tools(registry)
    .with_approver(Arc::new(Mutex::new(ScopedApprover(scope.clone()))))
    .with_sink(crate::tool_sink_or_warn())
    .with_decider(Arc::new(yunxi_bot_core::decide::LayaDecider::new(
        "http://127.0.0.1:17870/decide",
    )));
    handler.set_on_delta(Arc::new(|s| {
        print!("{s}");
        let _ = std::io::stdout().flush();
    }));
    let mut input = String::from(
        "请自主开发验证真实火车票＋酒店链路：2026年10月20日北京南→上海虹桥，1名成人、二等座；上海住10月20至21日1晚1间房。用户授权自主决定合适班次、酒店和合理预算，使用平台保存的本人资料，有多人不能猜。目标到付款界面，不付款；创建真实订单前先展示方案、总价和退改，等待本次人工确认。本轮没有文件、命令、邮件或脚本旁路；外部只发起、观察与转达用户，不代操作网站。每动作后重新观察；先用真实页面查库存、选人、排除附加服务，再travel_checkout匹配当前结算原文，将返回computer_act原样交人工审批。提交后重新观察订单详情/付款页，用travel_order_read核对订单号与状态；状态不明先查订单，不重提。密码、验证码和付款交用户。未知/坐标动作不自动放行；不要重复被拒调用或声称所有拒绝都来自真人。",
    );
    if let Some((pid, window)) = existing {
        input.push_str(&format!("\n本轮已获用户授权使用其日常Edge，精确pid={pid}、window_id={window}，保留现有登录。先computer_schema查所需参数，再computer_observe get_window_state给该pid/window_id建立原生观察；computer_act browser_prepare只给同一pid/window_id及strategy={{kind:existing_profile}}，不加profile/allow_launch；本轮可用delivery_mode=foreground完成浏览器原生授权。不要启动隔离浏览器。接着get_browser_state用该pid/window_id绑定，再用返回target_id及tabs中的实际tab_id读semantic_v2截图；窗口绑定回执不可直接动作。只在该窗口中新建测试标签或访问12306/携程，不覆盖其他无关标签，遇无法确定的标签操作停止询问。"));
    } else {
        input.push_str("\n本轮是明确的独立测试环境：先computer_schema查参数、computer_observe list_windows(app=Microsoft Edge)建立观察；computer_act browser_prepare给allow_launch=true和profile={mode:isolated_named,name:yunxi-recorded-travel}。只观察并操作返回pid的精确窗口。get_browser_state首次pid/window_id只绑定，必须再用返回target_id及选定tab_id读取semantic_v2截图后动作。新窗口重新绑定，不复制用户profile。");
    }
    input.push_str(if recording {
        "\n用户明确要求本次录屏验收，录制中断须停止动作。"
    } else {
        "\n本次是普通开发验证，不录屏，不要求用户关闭录屏或寻找录像控件。"
    });
    let mut turn = 0;
    if let Ok(message) = std::env::var("YUNXI_TEST_USER_MESSAGE") {
        input.push_str("\n【用户已补充的本轮信息】\n");
        input.push_str(&message);
    }
    let stdin = std::io::stdin();
    loop {
        turn += 1;
        let route = handler.route_for(&input, yunxi_bot_core::think::ReasoningEffort::Auto)?;
        let mut records = Vec::new();
        let result = handler.chat_turn(&route, &input, &mut records);
        let summary = match result {
            Ok(s) => s,
            Err(e) => format!("TURN_FAILED: {e}"),
        };
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(root.join(format!("bot-turn-{turn:02}.txt")))?;
        file.write_all(summary.as_bytes())?;
        if may_continue(&summary, turn, scope.lock().unwrap().calls) {
            println!(
                "AUTOMATIC_CONTINUATION: same user goal, no operator action or extra authority"
            );
            input = "继续同一项用户任务，保持原有条件与审批边界，结合本进程已有结果继续，不从头重复。此前提交或派发结果不明先查实际订单，不重复提交。遇登录、身份不明或下单确认仍需停下。".into();
            continue;
        }
        let controls = if recording {
            "/record-pause /record-resume /stop"
        } else {
            "/stop"
        };
        println!(
            "\nBOT_WAITING_FOR_USER turn={turn}\n{summary}\n{controls}; other input is relayed as the user's message"
        );
        let mut next = String::new();
        if stdin.lock().read_line(&mut next)? == 0 || next.trim() == "/stop" {
            break;
        }
        match next.trim() {
            "/record-pause" => {
                let mut s = scope.lock().unwrap();
                s.stop_recordings();
                s.paused = true;
                println!("RECORDING_PAUSED");
            }
            "/record-resume" => {
                let mut s = scope.lock().unwrap();
                s.stop_recordings();
                s.paused = false;
                let windows = s.targets.values().copied().collect::<Vec<_>>();
                for w in windows {
                    s.record(w)?;
                }
                println!("RECORDING_RESUMED");
            }
            _ => {
                input = next;
                continue;
            }
        }
        // Recording commands never become model instructions or run another model turn.
        loop {
            next.clear();
            if stdin.lock().read_line(&mut next)? == 0 || next.trim() == "/stop" {
                scope.lock().unwrap().stop_recordings();
                return Ok(());
            }
            if next.trim() == "/record-resume" {
                let mut s = scope.lock().unwrap();
                s.stop_recordings();
                s.paused = false;
                let windows = s.targets.values().copied().collect::<Vec<_>>();
                for w in windows {
                    s.record(w)?;
                }
                println!("RECORDING_RESUMED");
            } else {
                input = next;
                break;
            }
        }
    }
    scope.lock().unwrap().stop_recordings();
    Ok(())
}
