//! 每次系统开机的一次问候；原始位置只在取材线程中存在。
use chrono::{DateTime, Local, Timelike};
use serde_json::{Value, json};
use std::{path::PathBuf, sync::mpsc};
use yunxi_bot_core::{
    EventKind, Ledger,
    continuity::{DEFAULT_PERSONA, Identity, default_rules},
    costlog::CallRecord,
    memory::Memory,
    notify::{Notification, Notifier},
    think::{Message, ModelSpec, OpenAiThinker, ThinkRequest, Thinker, Thinking},
    tool::ToolPolicy,
};

#[derive(Default)]
struct Brief {
    body: String,
    reason: Option<String>,
    calls: Vec<Value>,
    model_call: Option<CallRecord>,
}
#[derive(Clone, Debug)]
struct Boot {
    id: String,
    started_at: u64,
}
impl Boot {
    fn read(now: u64) -> Result<Self, String> {
        #[cfg(target_os = "macos")]
        {
            let output = std::process::Command::new("/usr/sbin/sysctl")
                .args(["-n", "kern.bootsessionuuid", "kern.boottime"])
                .output()
                .map_err(|_| "无法读取系统开机批次".to_string())?;
            if !output.status.success() {
                return Err("系统未提供开机批次".into());
            }
            Self::parse(&String::from_utf8_lossy(&output.stdout), now)
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = now;
            Err("当前平台尚未实现系统开机批次读取".into())
        }
    }
    #[cfg(any(target_os = "macos", test))]
    fn parse(raw: &str, now: u64) -> Result<Self, String> {
        let parsed = (|| {
            let mut lines = raw.lines();
            let id = lines.next()?.trim().to_ascii_uppercase();
            if !valid_boot_id(&id) {
                return None;
            }
            let (seconds, tail) = lines
                .next()?
                .strip_prefix("{ sec = ")?
                .split_once(", usec = ")?;
            let seconds = seconds.parse::<u64>().ok()?;
            let micros = tail.split_once(" }")?.0.parse::<u64>().ok()?;
            let started_at = seconds.checked_mul(1000)?.checked_add(micros / 1000)?;
            if seconds == 0 || micros >= 1_000_000 || started_at > now || lines.next().is_some() {
                return None;
            }
            Some(Self { id, started_at })
        })();
        parsed.ok_or_else(|| "系统开机批次或开机时间无效，未生成问候".into())
    }
    fn contains(&self, event: &yunxi_bot_core::Event) -> bool {
        // Legacy records have no UUID; conservatively count attempts since this boot.
        match event.data.get("boot_id") {
            Some(id) => id == &self.id,
            None => event.at >= self.started_at,
        }
    }
}
fn valid_boot_id(id: &str) -> bool {
    id.len() == 36
        && id.bytes().enumerate().all(|(i, c)| {
            if [8, 13, 18, 23].contains(&i) {
                c == b'-'
            } else {
                c.is_ascii_hexdigit()
            }
        })
        && id.bytes().any(|c| c.is_ascii_hexdigit() && c != b'0')
}
pub struct StartupCare {
    home: PathBuf,
    policy: ToolPolicy,
    started: bool,
    running: Option<(Boot, u64, mpsc::Receiver<Brief>)>,
}
impl StartupCare {
    pub fn new(home: PathBuf, policy: ToolPolicy) -> Self {
        Self {
            home,
            policy,
            started: false,
            running: None,
        }
    }
    pub fn poll(&mut self, ledger: &mut Ledger, notifier: &dyn Notifier) -> Result<(), String> {
        let now = yunxi_bot_core::now_millis().map_err(|e| e.to_string())?;
        if let Some((boot, started, rx)) = &self.running {
            let result = match rx.try_recv() {
                Ok(brief) => Some(brief),
                Err(mpsc::TryRecvError::Empty) => return Ok(()),
                Err(mpsc::TryRecvError::Disconnected) => Some(Brief {
                    reason: Some("开机取材线程中断，未通知".into()),
                    ..Brief::default()
                }),
            };
            let boot = boot.clone();
            let started = *started;
            self.running = None;
            if let Some(brief) = result {
                finish(ledger, notifier, &boot, started, now, brief)?;
            }
        }
        if self.started {
            return Ok(());
        }
        self.started = true;
        self.start(ledger, notifier, now, Boot::read(now))
    }
    fn start(
        &mut self,
        ledger: &mut Ledger,
        notifier: &dyn Notifier,
        now: u64,
        boot: Result<Boot, String>,
    ) -> Result<(), String> {
        let boot = match boot {
            Ok(boot) => boot,
            Err(reason) => {
                ledger.append(EventKind::InfoTriaged, None, json!({"source":"startup_care","phase":"unavailable","degraded":true,"reason":reason})).map_err(|e|e.to_string())?;
                return Ok(());
            }
        };
        let time = local_time(now)?;
        let day = time.format("%Y-%m-%d").to_string();
        if !claim(ledger, &day, &boot)? {
            return Ok(());
        }
        // The boot claim reserves this attempt before starting model work.
        if crate::companion_runtime::generation_budget_used(ledger.events(), now) > 48 {
            return finish(
                ledger,
                notifier,
                &boot,
                now,
                now,
                Brief {
                    reason: Some("陪伴与记忆整理24小时调用预算已用完，开机问候不生成".into()),
                    ..Brief::default()
                },
            );
        }
        let home = self.home.clone();
        let policy = self.policy.clone();
        let (tx, rx) = mpsc::channel();
        self.running = Some((boot, now, rx));
        std::thread::spawn(move || {
            let _ = tx.send(prepare(home, policy, now));
        });
        Ok(())
    }
    pub fn is_running(&self) -> bool {
        self.running.is_some()
    }
}
fn local_time(now: u64) -> Result<DateTime<Local>, String> {
    i64::try_from(now)
        .ok()
        .and_then(DateTime::from_timestamp_millis)
        .map(|d| d.with_timezone(&Local))
        .ok_or("开机问候无法确认本地日期时间".into())
}
fn claim(ledger: &mut Ledger, day: &str, boot: &Boot) -> Result<bool, String> {
    ledger.reload().map_err(|e| e.to_string())?;
    if ledger.events().iter().any(|e| {
        e.data["source"] == "startup_care"
            && e.data
                .get("boot_id")
                .is_some_and(|id| !id.as_str().is_some_and(valid_boot_id))
    }) {
        return Err("开机问候历史包含无效批次，拒绝重新通知".into());
    }
    if ledger.events().iter().any(|e| {
        e.kind == EventKind::InfoTriaged
            && e.data["source"] == "startup_care"
            && boot.contains(e)
            && e.data["phase"] == "started"
    }) {
        if !ledger.events().iter().any(|e| {
            boot.contains(e)
                && ((e.kind == EventKind::NoticeSent && e.data["kind"] == "startup_care")
                    || (e.data["source"] == "startup_care"
                        && (e.data["phase"] == "interrupted"
                            || (e.data["phase"] == "prepared" && e.data["body"] == ""))))
        }) {
            ledger.append(EventKind::InfoTriaged,None,json!({"source":"startup_care","day":day,"boot_id":boot.id,"phase":"interrupted","degraded":true,"reason":"此前启动未取得通知回执，可能中断；本次开机不自动重发以免重复"})).map_err(|e|e.to_string())?;
        }
        return Ok(false);
    }
    // Claim before any external effect. After a crash we prefer one missed greeting to duplicate alerts.
    ledger
        .append(
            EventKind::InfoTriaged,
            None,
            json!({"source":"startup_care","day":day,"boot_id":boot.id,"phase":"started","degraded":false}),
        )
        .map_err(|e| e.to_string())?;
    Ok(true)
}
fn prepare(home: PathBuf, policy: ToolPolicy, now: u64) -> Brief {
    let mut cfg = ModelSpec::DEEPSEEK_FLASH.thinker_config();
    cfg.timeout = std::time::Duration::from_secs(45);
    let thinker = OpenAiThinker::from_home(&home, cfg);
    let mut calls = Vec::new();
    let mut brief = prepare_with(
        &home,
        now,
        thinker
            .as_ref()
            .map(|t| t as &dyn Thinker)
            .map_err(|_| "DeepSeek不可用，未生成开机问候"),
        &mut |name, args| read(&home, &policy, name, args, &mut calls),
    );
    brief.calls = calls;
    brief
}
fn prepare_with(
    home: &std::path::Path,
    now: u64,
    thinker: Result<&dyn Thinker, &str>,
    read: &mut impl FnMut(&str, &Value) -> Result<Value, String>,
) -> Brief {
    let mut brief = Brief::default();
    let result = (|| -> Result<String, String> {
        let thinker = thinker.map_err(str::to_string)?;
        let identity = Identity::load(
            home,
            "云熙",
            DEFAULT_PERSONA,
            &default_rules(),
            &Memory::default(),
        )?
        .render();
        let clock = read("now", &json!({}))?;
        let time = clock_time(&clock, now)?;
        let weather = (|| {
            let position = read("mac_location", &json!({}))?;
            if position["source"] != "macOS CoreLocation"
                || position["approximate"] != true
                || position["coordinate_decimals"] != 1
            {
                return Err("系统定位回执无法验证，未查询天气".to_string());
            }
            let forecast = read(
                "mac_weather",
                &json!({"latitude":position["latitude"],"longitude":position["longitude"],"date":time.format("%Y-%m-%d").to_string()}),
            )?;
            weather_facts(&forecast, time.timestamp_millis() as u64)
        })();
        brief.reason = weather.as_ref().err().cloned();
        let weather = match weather {
            Ok(facts) => facts,
            Err(_) => {
                json!({"available":false,"reason":"未取得完整、有效的当地天气，不可推测天气或穿衣建议"})
            }
        };
        let request = greeting_request(
            &identity,
            json!({"time":{"tool":"now","result":clock},"weather":weather}),
        );
        let peak = yunxi_bot_core::costlog::peak_now();
        let response = thinker
            .think(&request)
            .map_err(|_| "开机问候模型调用失败，未使用固定文案替代".to_string())?;
        brief.model_call = Some(
            CallRecord::new("deepseek", thinker.model(), peak)
                .with_usage(response.usage)
                .with_thinking(false),
        );
        let text = response.content.trim();
        if !response.tool_calls.is_empty()
            || response.finish_reason.as_deref() != Some("stop")
            || text.is_empty()
            || text.chars().count() > 160
            || matches!(text, "-" | "—")
            || text.chars().any(|c| c.is_control() && c != '\n')
            || yunxi_bot_core::continuity::may_contain_secret(text)
        {
            return Err("开机问候输出不完整或不符合通知约束，未发送".into());
        }
        Ok(text.into())
    })();
    match result {
        Ok(body) => brief.body = body,
        Err(e) => {
            brief.reason = Some(match brief.reason {
                Some(reason) => format!("{reason}；{e}"),
                None => e,
            });
        }
    }
    brief
}
fn clock_time(value: &Value, started: u64) -> Result<DateTime<Local>, String> {
    let seconds = value
        .as_str()
        .and_then(|s| {
            s.lines()
                .find_map(|line| line.strip_prefix("Unix 时间戳："))
        })
        .and_then(|s| s.parse::<u64>().ok())
        .and_then(|s| s.checked_mul(1000));
    let now = seconds
        .filter(|ms| ms.abs_diff(started) <= 120_000)
        .ok_or("时间工具回执无效或过期，未生成问候")?;
    let time = local_time(now)?;
    if time.date_naive() != local_time(started)?.date_naive() {
        return Err("时间取材跨日，未生成问候".into());
    }
    Ok(time)
}
fn greeting_request(identity: &str, facts: Value) -> ThinkRequest {
    ThinkRequest::new(vec![
        Message::system(format!(
            "{identity}\n\n{}",
            yunxi_bot_core::prompts::expression(yunxi_bot_core::prompts::STARTUP)
        )),
        Message::user(facts.to_string()),
    ])
    .with_max_tokens(320)
    .with_temperature(0.7)
    .with_thinking(Thinking::Disabled)
}

fn read(
    home: &std::path::Path,
    policy: &ToolPolicy,
    name: &str,
    args: &Value,
    calls: &mut Vec<Value>,
) -> Result<Value, String> {
    use yunxi_bot_core::tool::{Rule, Tool, ToolContext, gate};
    let tool: Box<dyn Tool> = if name == "now" {
        Box::new(yunxi_bot_core::tool::system::NowTool)
    } else {
        #[cfg(target_os = "macos")]
        {
            if !matches!(name, "mac_location" | "mac_weather") {
                return Err("开机取材工具未获准".into());
            }
            Box::new(
                yunxi_bot_core::tool::mac_assistant::tools()
                    .map_err(|_| "本机工具目录不可用")?
                    .into_iter()
                    .find(|t| t.name() == name)
                    .ok_or("天气取材工具不存在")?,
            )
        }
        #[cfg(not(target_os = "macos"))]
        {
            return Err("自动定位天气暂只支持macOS".into());
        }
    };
    let mut policy = policy.clone();
    // The explicit --startup-care opt-in authorizes only this fixed weather query; deny still wins.
    if name == "mac_weather" {
        policy.allow.push(Rule::tool("mac_weather"));
    }
    let mut ctx = ToolContext::new(home, yunxi_bot_core::policy::SandboxMode::ReadOnly);
    let allowed = gate(tool.as_ref(), args, &policy, &ctx, None).is_allow();
    let result = if allowed {
        tool.call(args, &mut ctx)
            .map_err(|_| format!("{name} 不可用（权限、时间、定位或网络未就绪）"))
            .and_then(|output| {
                if name == "now" {
                    Ok(json!(output.text))
                } else {
                    serde_json::from_str(&output.text)
                        .map_err(|_| "天气取材回执不是JSON".to_string())
                }
            })
    } else {
        Err(format!("{name} 被工具策略拒绝，未执行"))
    };
    calls.push(json!({"source":"startup_care","tool":name,"decision":if allowed {"allow"} else {"deny"},"executed":allowed,"outcome":if result.is_ok(){"ok"}else{"error"},"private_arguments":true,"private_output":true,"degraded":result.is_err()}));
    result
}
fn weather_facts(value: &Value, now: u64) -> Result<Value, String> {
    let invalid = || "天气回执的日期、单位、时区或数值不完整，未生成天气建议".to_string();
    let time = local_time(now)?;
    let day = time.format("%Y-%m-%d").to_string();
    let fetched = value["fetched_at"]
        .as_str()
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|t| t.timestamp_millis());
    if value["source"] != "Open-Meteo"
        || !fetched.is_some_and(|t| t >= 0 && (now as i128 - t as i128).abs() <= 120_000)
    {
        return Err(invalid());
    }
    let forecast = &value["forecast"];
    if forecast["utc_offset_seconds"].as_i64() != Some(i64::from(time.offset().local_minus_utc())) {
        return Err(invalid());
    }
    let units = &forecast["hourly_units"];
    if units["temperature_2m"] != "°C"
        || units["precipitation_probability"] != "%"
        || units["wind_speed_10m"] != "km/h"
    {
        return Err(invalid());
    }
    let hours = &forecast["hourly"];
    let times = hours["time"]
        .as_array()
        .filter(|a| a.len() == 24)
        .ok_or_else(invalid)?;
    let mut rows = Vec::new();
    let mut last = None;
    for (i, t) in times.iter().enumerate() {
        let s = t.as_str().ok_or_else(invalid)?;
        let parsed =
            chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M").map_err(|_| invalid())?;
        if parsed.format("%Y-%m-%d").to_string() != day
            || parsed.minute() != 0
            || parsed.hour() as usize != i
            || last.is_some_and(|p| parsed < p)
        {
            return Err(invalid());
        }
        last = Some(parsed);
        let number = |key: &str, min: f64, max: f64| -> Result<f64, String> {
            let list = hours[key]
                .as_array()
                .filter(|a| a.len() == times.len())
                .ok_or_else(invalid)?;
            list[i]
                .as_f64()
                .filter(|n| n.is_finite() && *n >= min && *n <= max)
                .ok_or_else(invalid)
        };
        let temp = number("temperature_2m", -90., 65.)?;
        let rain = number("precipitation_probability", 0., 100.)?;
        let wind = number("wind_speed_10m", 0., 400.)?;
        if parsed.hour() >= time.hour() {
            rows.push((parsed.hour(), temp, rain, wind));
        }
    }
    if rows.is_empty()
        || times.first().and_then(Value::as_str) != Some(format!("{day}T00:00").as_str())
        || times.last().and_then(Value::as_str) != Some(format!("{day}T23:00").as_str())
    {
        return Err(invalid());
    }
    let low = rows.iter().map(|r| r.1).fold(f64::INFINITY, f64::min);
    let high = rows.iter().map(|r| r.1).fold(f64::NEG_INFINITY, f64::max);
    let rain = rows.iter().map(|r| r.2).fold(0., f64::max);
    let wind = rows.iter().map(|r| r.3).fold(0., f64::max);
    let mut cooling = Value::Null;
    if let Some(peak) = rows.iter().max_by(|a, b| a.1.total_cmp(&b.1)) {
        let later_low = rows
            .iter()
            .filter(|r| r.0 > peak.0)
            .map(|r| r.1)
            .fold(f64::INFINITY, f64::min);
        if peak.1 - later_low >= 5. {
            cooling = json!({"after_hour":peak.0,"drop_celsius":peak.1 - later_low});
        }
    }
    Ok(
        json!({"available":true,"tool":"mac_weather","source":"Open-Meteo","date":day,
        "period":"today_remaining_hours_forecast","from_hour":time.hour(),
        "temperature_celsius":{"low":low,"high":high},"rain_probability_percent_max":rain,
        "wind_kmh_max":wind,"later_cooling":cooling,"yesterday_comparison":null}),
    )
}
fn finish(
    ledger: &mut Ledger,
    notifier: &dyn Notifier,
    boot: &Boot,
    started: u64,
    now: u64,
    brief: Brief,
) -> Result<(), String> {
    if let Some(call) = &brief.model_call {
        yunxi_bot_core::costlog::record_call(ledger, call).map_err(|e| e.to_string())?;
    }
    for call in brief.calls {
        ledger
            .append(EventKind::ToolCalled, None, call)
            .map_err(|e| e.to_string())?;
    }
    let day = local_time(started)?.format("%Y-%m-%d").to_string();
    let valid = local_time(now)?.format("%Y-%m-%d").to_string() == day
        && now >= started
        && now - started <= 300_000
        && !brief.body.is_empty();
    ledger.append(EventKind::InfoTriaged, None, json!({"source":"startup_care","phase":"prepared","day":day,"boot_id":boot.id,"body":brief.body,"degraded":brief.reason.is_some() || !valid,"reason":brief.reason.clone().unwrap_or_else(|| if valid { String::new() } else { "启动取材已过期或中断，不发送".into() })})).map_err(|e| e.to_string())?;
    if !valid {
        return Ok(());
    }
    let notice = Notification::new("云熙 · 陪伴｜开机问候", &brief.body)
        .with_tag(format!("startup-care-{}", boot.id));
    let delivery = notifier.notify(&notice);
    ledger.append(EventKind::NoticeSent, None, json!({"source":"companion","kind":"startup_care","day":day,"boot_id":boot.id,"body":brief.body,"delivery":delivery,"tag":notice.tag,"degraded":brief.reason.is_some() || !delivery.can_claim_user_notified(),"reason":brief.reason})).map_err(|e| e.to_string())?;
    println!("[开机问候] {}（不表示已读）", delivery.label());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use yunxi_bot_core::notify::Delivery;
    fn boot() -> Boot {
        Boot {
            id: "22222222-2222-4222-8222-222222222222".into(),
            started_at: now(8),
        }
    }
    fn now(hour: u32) -> u64 {
        use chrono::TimeZone;
        Local
            .with_ymd_and_hms(2026, 10, 20, hour, 0, 0)
            .single()
            .unwrap()
            .timestamp_millis() as u64
    }
    fn forecast() -> Value {
        let t = local_time(now(8)).unwrap();
        json!({"source":"Open-Meteo","fetched_at":t.to_rfc3339(),"forecast":{"utc_offset_seconds":t.offset().local_minus_utc(),"hourly_units":{"temperature_2m":"°C","precipitation_probability":"%","wind_speed_10m":"km/h"},"hourly":{"time":(0..24).map(|h|format!("2026-10-20T{h:02}:00")).collect::<Vec<_>>(),"temperature_2m":(0..24).map(|h|if h<16{20}else{8}).collect::<Vec<_>>(),"precipitation_probability":vec![60;24],"wind_speed_10m":vec![35;24]}}})
    }
    struct Spy(Mutex<usize>, Delivery);
    impl Notifier for Spy {
        fn name(&self) -> &'static str {
            "spy"
        }
        fn notify(&self, _: &Notification) -> Delivery {
            *self.0.lock().unwrap() += 1;
            self.1.clone()
        }
    }
    fn ledger(name: &str) -> Ledger {
        let p = std::env::temp_dir().join(format!("yunxi-startup-{}-{name}", std::process::id()));
        let _ = std::fs::remove_file(&p);
        Ledger::open(p).unwrap()
    }
    fn test_home(name: &str) -> PathBuf {
        let p =
            std::env::temp_dir().join(format!("yunxi-startup-home-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&p).unwrap();
        p
    }
    struct Model {
        response: Result<yunxi_bot_core::think::ThinkResponse, yunxi_bot_core::think::ThinkError>,
        requests: Mutex<Vec<ThinkRequest>>,
    }
    impl Model {
        fn new(body: &str) -> Self {
            Self {
                response: yunxi_bot_core::think::client::StubThinker::replying(body)
                    .think(&ThinkRequest::new(vec![])),
                requests: Mutex::new(vec![]),
            }
        }
    }
    impl Thinker for Model {
        fn model(&self) -> &str {
            "synthetic"
        }
        fn think(
            &self,
            req: &ThinkRequest,
        ) -> Result<yunxi_bot_core::think::ThinkResponse, yunxi_bot_core::think::ThinkError>
        {
            self.requests.lock().unwrap().push(req.clone());
            self.response.clone()
        }
    }
    fn facts(name: &str, at: u64) -> Result<Value, String> {
        match name {
            "now" => Ok(json!(format!(
                "日期时间：{}\nUnix 时间戳：{}",
                local_time(at).unwrap().format("%Y-%m-%d %H:%M:%S"),
                at / 1000
            ))),
            "mac_location" => Ok(
                json!({"source":"macOS CoreLocation","approximate":true,"coordinate_decimals":1,"latitude":30.0,"longitude":120.0}),
            ),
            "mac_weather" => {
                let mut v = forecast();
                v["fetched_at"] = json!(local_time(at).unwrap().to_rfc3339());
                v["latitude"] = json!(30.0);
                v["source_url"] = json!("private-source-must-not-reach-model");
                Ok(v)
            }
            _ => Err("unexpected tool".into()),
        }
    }
    #[test]
    fn missing_weather_is_explicit_but_bad_time_never_reaches_the_model() {
        let home = test_home("missing-facts");
        let model = Model::new("天气暂时没查到，先慢慢开始今天吧。");
        let b = prepare_with(&home, now(8), Ok(&model), &mut |name, _| {
            if name == "mac_weather" {
                Err("weather unavailable".into())
            } else {
                facts(name, now(8))
            }
        });
        assert!(b.reason.is_some() && !b.body.is_empty());
        let req = &model.requests.lock().unwrap()[0];
        let evidence: Value = serde_json::from_str(&req.messages[1].content).unwrap();
        assert_eq!(evidence["weather"]["available"], false);
        for clock in [
            Err("denied".into()),
            Ok(json!("unknown")),
            facts("now", now(7)),
        ] {
            let unused = Model::new("should not be used");
            let b = prepare_with(&home, now(8), Ok(&unused), &mut |name, _| {
                assert_eq!(name, "now");
                clock.clone()
            });
            assert!(b.body.is_empty() && b.reason.is_some());
            assert!(unused.requests.lock().unwrap().is_empty());
        }
        std::fs::remove_dir_all(home).unwrap();
    }
    #[test]
    fn generation_failures_tool_calls_and_truncated_output_never_notify() {
        let home = test_home("invalid-generation");
        let mut l = ledger("invalid-generation");
        let spy = Spy(
            Mutex::new(0),
            Delivery::Blocked {
                reason: "test".into(),
            },
        );
        for case in ["error", "tools", "length", "empty", "too_long", "secret"] {
            let mut model = Model::new("合成问候");
            match case {
                "error" => {
                    model.response = Err(yunxi_bot_core::think::ThinkError::Network(
                        "synthetic".into(),
                    ))
                }
                "tools" => {
                    model.response.as_mut().unwrap().tool_calls = vec![json!({"id":"unexpected"})]
                }
                "length" => model.response.as_mut().unwrap().finish_reason = Some("length".into()),
                "empty" => model.response.as_mut().unwrap().content.clear(),
                "too_long" => model.response.as_mut().unwrap().content = "长".repeat(161),
                "secret" => model.response.as_mut().unwrap().content = "sk-synthetic-only".into(),
                _ => unreachable!(),
            }
            let b = prepare_with(&home, now(8), Ok(&model), &mut |name, _| {
                facts(name, now(8))
            });
            assert!(b.body.is_empty() && b.reason.is_some(), "{case}");
            finish(&mut l, &spy, &boot(), now(8), now(8), b).unwrap();
        }
        assert_eq!(*spy.0.lock().unwrap(), 0);
        assert_eq!(
            l.events()
                .iter()
                .filter(|e| e.kind == EventKind::ModelCalled)
                .count(),
            5
        );
        std::fs::remove_dir_all(home).unwrap();
    }
    #[test]
    fn time_tool_deny_is_audited_and_startup_attempt_reserves_shared_budget() {
        let mut calls = Vec::new();
        let mut policy = ToolPolicy::default();
        policy.deny.push(yunxi_bot_core::tool::Rule::tool("now"));
        assert!(
            read(
                std::path::Path::new("."),
                &policy,
                "now",
                &json!({}),
                &mut calls
            )
            .is_err()
        );
        assert_eq!(calls[0]["executed"], false);
        assert_eq!(calls[0]["degraded"], true);
        let mut l = ledger("budget");
        assert!(claim(&mut l, "2026-10-20", &boot()).unwrap());
        let timestamp = l.events()[0].at;
        assert_eq!(
            crate::companion_runtime::generation_budget_used(l.events(), timestamp),
            1
        );
        assert!(!claim(&mut l, "2026-10-20", &boot()).unwrap());
        assert_eq!(
            crate::companion_runtime::generation_budget_used(l.events(), timestamp),
            1
        );
        assert_eq!(
            crate::companion_runtime::generation_budget_used(l.events(), timestamp + 86_400_000),
            0
        );
    }
    #[test]
    fn exhausted_budget_records_failure_without_spawning_or_notifying_on_restart() {
        let mut l = ledger("exhausted-budget");
        for _ in 0..48 {
            l.append(
                EventKind::InfoTriaged,
                None,
                json!({"source":"companion_memory"}),
            )
            .unwrap();
        }
        let spy = Spy(
            Mutex::new(0),
            Delivery::Blocked {
                reason: "test".into(),
            },
        );
        let mut worker = StartupCare::new(PathBuf::new(), ToolPolicy::default());
        worker
            .start(
                &mut l,
                &spy,
                yunxi_bot_core::now_millis().unwrap(),
                Ok(boot()),
            )
            .unwrap();
        assert!(!worker.is_running());
        assert_eq!(*spy.0.lock().unwrap(), 0);
        assert!(
            l.events().last().unwrap().data["reason"]
                .as_str()
                .unwrap()
                .contains("预算")
        );
        let events = l.events().len();
        let mut restarted = StartupCare::new(PathBuf::new(), ToolPolicy::default());
        restarted
            .start(
                &mut l,
                &spy,
                yunxi_bot_core::now_millis().unwrap(),
                Ok(boot()),
            )
            .unwrap();
        assert_eq!(l.events().len(), events);
        assert!(!restarted.is_running());
    }
    #[test]
    fn weather_advice_requires_complete_fresh_local_forecast() {
        let good = forecast();
        let s = weather_facts(&good, now(8)).unwrap();
        assert_eq!(s["temperature_celsius"], json!({"low":8.0,"high":20.0}));
        assert_eq!(s["later_cooling"]["drop_celsius"], 12.0);
        assert_eq!(s["rain_probability_percent_max"], 60.0);
        assert_eq!(s["wind_kmh_max"], 35.0);
        for (pointer, bad) in [
            ("/forecast/hourly/temperature_2m/0", Value::Null),
            ("/forecast/hourly/precipitation_probability/0", json!(101)),
            ("/forecast/hourly/time/12", json!("2026-10-19T12:00")),
            ("/forecast/hourly_units/temperature_2m", json!("°F")),
            ("/forecast/utc_offset_seconds", json!(123)),
            ("/fetched_at", json!("2026-10-18T00:00:00Z")),
        ] {
            let mut v = good.clone();
            *v.pointer_mut(pointer).unwrap() = bad;
            assert!(weather_facts(&v, now(8)).is_err(), "{pointer}");
        }
        let mut v = good;
        v["forecast"]["hourly"]["temperature_2m"] = json!(vec![8; 24]);
        assert!(weather_facts(&v, now(8)).unwrap()["later_cooling"].is_null());
    }
    #[test]
    fn unavailable_generation_never_falls_back_to_a_canned_greeting() {
        let b = prepare(PathBuf::new(), ToolPolicy::deny_all(), now(0));
        assert!(
            b.body.is_empty(),
            "未取得工具事实与模型表达时不发送固定问候"
        );
        assert!(b.reason.is_some());
    }
    #[test]
    fn day_and_night_generation_receive_full_identity_and_real_tool_facts() {
        let home = test_home("identity");
        let persona = format!("{}人格末尾标记", "合成人格。".repeat(1500));
        std::fs::write(home.join("persona.md"), format!("# 云熙\n{persona}")).unwrap();
        std::fs::write(home.join("profile.md"), "合成画像标记").unwrap();
        for h in [8, 23] {
            let model = Model::new("风有点大，出门时带件外套吧。");
            let mut tools = Vec::new();
            let b = prepare_with(&home, now(h), Ok(&model), &mut |name, _| {
                tools.push(name.to_string());
                facts(name, now(h))
            });
            assert_eq!(tools, ["now", "mac_location", "mac_weather"]);
            assert_eq!(b.body, "风有点大，出门时带件外套吧。");
            assert!(b.reason.is_none() && b.model_call.is_some());
            let reqs = model.requests.lock().unwrap();
            assert_eq!(reqs.len(), 1);
            let req = &reqs[0];
            assert!(req.tools.is_empty());
            assert!(req.messages[0].content.contains(&persona));
            assert!(req.messages[0].content.contains("合成画像标记"));
            let evidence: Value = serde_json::from_str(&req.messages[1].content).unwrap();
            assert_eq!(evidence["time"]["result"], facts("now", now(h)).unwrap());
            assert_eq!(evidence["weather"]["available"], true);
            assert!(!req.messages[1].content.contains("latitude"));
            assert!(!req.messages[1].content.contains("source_url"));
        }
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    #[ignore = "真实DeepSeek合成人格对照，仅用替身天气，不发通知、不读私人画像"]
    fn live_startup_personality_with_synthetic_tool_facts() {
        let credentials = PathBuf::from(std::env::var("YUNXI_TEST_HOME").unwrap());
        let model = yunxi_bot_core::think::OpenAiThinker::from_home(
            &credentials,
            yunxi_bot_core::think::ModelSpec::DEEPSEEK_FLASH.thinker_config(),
        )
        .unwrap();
        let mut rows = Vec::new();
        for (label, persona) in [
            ("direct", "表达直接具体，偏短句，不使用比喻或昵称。"),
            (
                "expansive",
                "说话舒展，有自然的生活比喻，关心放在具体细节里，不用固定昵称。",
            ),
        ] {
            let home = test_home(&format!("live-style-{label}"));
            std::fs::write(home.join("persona.md"), format!("# 云熙\n{persona}")).unwrap();
            std::fs::write(home.join("profile.md"), "合成测试用户；未提供出门计划。").unwrap();
            for h in [8, 23] {
                if let Some(wait) = model.throttle_wait() {
                    std::thread::sleep(wait);
                }
                let brief = prepare_with(&home, now(h), Ok(&model), &mut |name, _| {
                    facts(name, now(h))
                });
                rows.push(json!({"style":label,"hour":h,"body":brief.body,"reason":brief.reason,"passed":!brief.body.is_empty() && brief.body!="-" && brief.body.chars().count()<=120 && brief.model_call.is_some()}));
            }
            std::fs::remove_dir_all(home).unwrap();
        }
        let report = PathBuf::from(std::env::var("YUNXI_STYLE_REPORT").unwrap());
        assert!(!report.exists());
        std::fs::write(report, serde_json::to_vec_pretty(&rows).unwrap()).unwrap();
        assert!(
            rows.iter().all(|r| r["passed"] == true),
            "检查合成问候报告；风格差异仍需人工审核"
        );
    }
    #[test]
    fn boot_identity_rejects_invalid_uuid_time_and_extra_output() {
        let raw = "22222222-2222-4222-8222-222222222222\n{ sec = 100, usec = 123456 } date\n";
        let parsed = Boot::parse(raw, 100_123).unwrap();
        assert_eq!(parsed.started_at, 100_123);
        for bad in [
            raw.replace("22222222-2222-4222-8222-222222222222", "not-a-boot"),
            raw.replace(
                "22222222-2222-4222-8222-222222222222",
                "00000000-0000-0000-0000-000000000000",
            ),
            raw.replace("sec = 100,", "sec = 0,"),
            raw.replace("sec = 100,", "sec = -1,"),
            raw.replace("sec = 100,", "sec = 18446744073709551615,"),
            raw.replace("123456", "1000000"),
            raw.replace("123456", "-1"),
            format!("{raw}unexpected\n"),
        ] {
            assert!(Boot::parse(&bad, 100_123).is_err(), "{bad}");
        }
        assert!(Boot::parse(raw, 100_122).is_err());
    }
    #[test]
    fn boot_read_failure_is_audited_without_generation_or_notification() {
        let mut l = ledger("boot-unavailable");
        let spy = Spy(
            Mutex::new(0),
            Delivery::Blocked {
                reason: "test".into(),
            },
        );
        let mut worker = StartupCare::new(PathBuf::new(), ToolPolicy::default());
        worker
            .start(&mut l, &spy, now(8), Err("无法确认开机批次".into()))
            .unwrap();
        assert!(!worker.is_running());
        assert_eq!(*spy.0.lock().unwrap(), 0);
        assert_eq!(l.events().len(), 1);
        assert_eq!(l.events()[0].data["phase"], "unavailable");
        assert_eq!(l.events()[0].data["degraded"], true);
    }
    #[test]
    fn legacy_attempts_only_block_the_current_boot_and_preserve_history() {
        for (name, at, allowed) in [
            ("legacy-old", now(0), true),
            ("legacy-current", now(8), false),
        ] {
            let mut l = ledger(name);
            let mut event = l
                .append(
                    EventKind::InfoTriaged,
                    None,
                    json!({"source":"startup_care","day":"2026-10-20","phase":"started"}),
                )
                .unwrap();
            event.at = at;
            let raw = std::fs::read_to_string(l.path()).unwrap();
            let (header, _) = raw.split_once('\n').unwrap();
            let history = format!("{header}\n{}\n", serde_json::to_string(&event).unwrap());
            std::fs::write(l.path(), &history).unwrap();
            assert_eq!(claim(&mut l, "2026-10-20", &boot()).unwrap(), allowed);
            assert!(
                std::fs::read_to_string(l.path())
                    .unwrap()
                    .starts_with(&history)
            );
            assert!(!claim(&mut Ledger::open(l.path()).unwrap(), "2026-10-21", &boot()).unwrap());
        }
        let mut l = ledger("invalid-history-boot");
        l.append(
            EventKind::InfoTriaged,
            None,
            json!({"source":"startup_care","phase":"started","boot_id":null}),
        )
        .unwrap();
        assert!(claim(&mut l, "2026-10-20", &boot()).is_err());
    }
    #[test]
    fn previous_boot_same_day_does_not_consume_new_boot_greeting() {
        let mut l = ledger("previous-boot");
        l.append(EventKind::InfoTriaged, None, json!({"source":"startup_care","day":"2026-10-20","phase":"started","boot_id":"11111111-1111-4111-8111-111111111111"})).unwrap();
        assert!(claim(&mut l, "2026-10-20", &boot()).unwrap());
    }
    #[test]
    fn same_boot_next_day_does_not_create_another_greeting() {
        let mut l = ledger("same-boot-next-day");
        assert!(claim(&mut l, "2026-10-20", &boot()).unwrap());
        assert!(!claim(&mut l, "2026-10-21", &boot()).unwrap());
    }
    #[test]
    fn restart_claim_survives_crash_and_new_boot_is_independent() {
        let mut l = ledger("dedupe");
        assert!(claim(&mut l, "2026-10-20", &boot()).unwrap());
        let mut reloaded = Ledger::open(l.path()).unwrap();
        assert!(!claim(&mut reloaded, "2026-10-20", &boot()).unwrap());
        let count = reloaded.events().len();
        assert!(!claim(&mut reloaded, "2026-10-21", &boot()).unwrap());
        assert_eq!(reloaded.events().len(), count);
        assert!(
            claim(
                &mut reloaded,
                "2026-10-21",
                &Boot {
                    id: "33333333-3333-4333-8333-333333333333".into(),
                    ..boot()
                }
            )
            .unwrap()
        );
    }
    #[test]
    fn stale_results_do_not_notify_and_blocked_delivery_is_not_success() {
        let mut l = ledger("finish");
        let spy = Spy(
            Mutex::new(0),
            Delivery::Blocked {
                reason: "off".into(),
            },
        );
        let brief = || Brief {
            body: "早上好".into(),
            ..Brief::default()
        };
        finish(&mut l, &spy, &boot(), now(8), now(9), brief()).unwrap();
        assert_eq!(*spy.0.lock().unwrap(), 0);
        finish(&mut l, &spy, &boot(), now(8), now(8), brief()).unwrap();
        assert_eq!(*spy.0.lock().unwrap(), 1);
        let e = l.events().last().unwrap();
        assert_eq!(e.data["delivery"]["kind"], "blocked");
        assert_eq!(e.data["degraded"], true);
        assert_eq!(e.data["boot_id"], boot().id);
        assert_eq!(e.data["tag"], format!("startup-care-{}", boot().id));
        finish(&mut l, &spy, &boot(), now(23), now(0) + 86_400_000, brief()).unwrap();
        assert_eq!(*spy.0.lock().unwrap(), 1);
        assert!(
            !std::fs::read_to_string(l.path())
                .unwrap()
                .contains("latitude")
        );
    }
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "真实人格、时间工具、系统定位、天气和DeepSeek，发送一条测试通知；需YUNXI_STARTUP_REPORT_DIR"]
    fn live_location_weather_and_native_notice() {
        let home = PathBuf::from(std::env::var("YUNXI_STARTUP_REPORT_DIR").unwrap());
        std::fs::create_dir_all(&home).unwrap();
        assert!(home.join("persona.md").is_file());
        let now = yunxi_bot_core::now_millis().unwrap();
        let time = local_time(now).unwrap();
        let mut brief = prepare(home.clone(), ToolPolicy::default(), now);
        assert!(brief.reason.is_none(), "{:?}", brief.reason);
        assert!(!brief.body.is_empty() && brief.model_call.is_some());
        assert_eq!(
            brief
                .calls
                .iter()
                .map(|v| v["tool"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["now", "mac_location", "mac_weather"]
        );
        assert!(brief.calls.iter().all(|v| v["outcome"] == "ok"));
        let usage = brief.model_call.as_ref().unwrap().usage;
        let body = brief.body.clone();
        brief.body = format!("【功能测试】{body}");
        let mut l = Ledger::open(home.join("ledger.jsonl")).unwrap();
        let day = time.format("%Y-%m-%d").to_string();
        let boot = Boot::read(now).unwrap();
        assert!(claim(&mut l, &day, &boot).unwrap());
        finish(
            &mut l,
            &yunxi_bot_core::notify_macos::MacNotification,
            &boot,
            now,
            yunxi_bot_core::now_millis().unwrap(),
            brief,
        )
        .unwrap();
        let confirmed = l.events().last().unwrap().data["delivery"]["kind"] == "confirmed";
        let persisted = std::fs::read_to_string(l.path()).unwrap();
        let private = !persisted.contains("latitude")
            && !persisted.contains("longitude")
            && !persisted.contains("source_url");
        std::fs::write(home.join("report.json"), serde_json::to_vec_pretty(&json!({"time_tool":true,"location_permission":true,"weather_validated":true,"real_deepseek":true,"usage":usage,"notification_confirmed":confirmed,"private_location":private,"body":body,"user_read":false})).unwrap()).unwrap();
        assert!(confirmed && private);
        assert!(!claim(&mut Ledger::open(l.path()).unwrap(), &day, &boot).unwrap());
    }
}
