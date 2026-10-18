//! Learned semantic steps, not executable page code. Every replay action returns to ToolRunner's gate.
use super::*;
use serde::Serialize;
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Locator {
    role: String,
    name: String,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum Step {
    Navigate {
        origin: String,
        parameter: String,
    },
    Click {
        origin: String,
        locator: Locator,
    },
    Type {
        origin: String,
        locator: Locator,
        parameter: String,
    },
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Recipe {
    version: u8,
    name: String,
    steps: Vec<Step>,
    final_origin: String,
}
pub(super) struct Recording {
    name: String,
    target: Value,
    tab: Value,
    steps: Vec<Step>,
    valid: bool,
}
fn safe_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}
fn text(s: &str) -> bool {
    !s.trim().is_empty() && s.len() <= 2000 && !s.contains('\0')
}
fn origin(url: &str) -> Result<String> {
    // Parse with the already-installed HTTP stack; never persist paths, queries, fragments or credentials.
    let parsed = ureq::get(url)
        .request_url()
        .map_err(|_| bad("workflow_url_invalid"))?;
    let parsed = parsed.as_url();
    if !matches!(parsed.scheme(), "http" | "https")
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || url.contains('\\')
    {
        return Err(bad("workflow_url_invalid"));
    }
    Ok(parsed.origin().ascii_serialization())
}
fn nodes(data: &Value) -> impl Iterator<Item = &Value> {
    ["refs", "content_refs"]
        .into_iter()
        .flat_map(|k| data[k].as_array().into_iter().flatten())
}
fn page_origin(data: &Value) -> Result<String> {
    origin(
        data["page"]["url"]
            .as_str()
            .or_else(|| data["url"].as_str())
            .unwrap_or(""),
    )
}
fn complete(data: &Value) -> Result<()> {
    if ["budget", "page_occluded", "unprovable_frame"]
        .iter()
        .any(|k| {
            data["snapshot"]["omitted"][*k]
                .as_u64()
                .is_some_and(|n| n > 0)
        })
        || nodes(data).any(|n| matches!(n["role"].as_str(), Some("dialog" | "alertdialog")))
        || data["_yunxi_dialog_observation"] == true
        || data["present"] == true
        || data["snapshot"]["complete"] != true
        || data.get("query").is_some()
        || data.get("scope_ref").is_some()
        || data.get("continuation").is_some()
    {
        return Err(bad(
            "degraded=true workflow_observation_incomplete_or_dialog：保持原状，重新观察或交还模型/用户",
        ));
    }
    Ok(())
}
impl Locator {
    fn valid(&self) -> bool {
        text(&self.role)
            && text(&self.name)
            && self.name.len() <= 200
            && ![
                "password",
                "验证码",
                "密码",
                "信用卡",
                "card number",
                "one-time",
                "token",
                "secret",
            ]
            .iter()
            .any(|s| self.name.to_lowercase().contains(s))
    }
    fn resolve<'a>(&self, data: &'a Value) -> Result<&'a Value> {
        complete(data)?;
        if !self.valid() {
            return Err(bad("workflow_sensitive_or_invalid_locator"));
        }
        let found: Vec<_> = nodes(data)
            .filter(|n| n["role"] == self.role && n["name"] == self.name)
            .collect();
        if found.len() != 1 {
            return Err(bad(if found.is_empty() {
                "workflow_locator_missing"
            } else {
                "workflow_locator_ambiguous"
            }));
        }
        let n = found[0];
        if n["visibility"]
            .as_str()
            .is_some_and(|s| !matches!(s, "in_viewport" | "near_viewport"))
            || n["states"]["disabled"] == true
            || n["states"]["protected"] == true
            || n["states"]["readonly"] == true
            || n["disabled"] == true
            || n["enabled"] == false
            || n["visible"] == false
            || n["type"] == "password"
            || n["input_type"] == "password"
        {
            return Err(bad("workflow_control_unavailable"));
        }
        if !n["ref"].as_str().is_some_and(text) {
            return Err(bad("workflow_ref_missing"));
        }
        Ok(n)
    }
}
impl Recording {
    pub(super) fn stage(&mut self, i: &Input, frame: Option<&Frame>) -> Option<Step> {
        let result = self.capture(i, frame);
        if result.is_err() {
            self.valid = false;
        }
        result.ok()
    }
    fn capture(&self, i: &Input, frame: Option<&Frame>) -> Result<Step> {
        if !self.valid || self.steps.len() >= 32 {
            return Err(bad("workflow_recording_invalid"));
        }
        let frame = frame.ok_or_else(|| bad("workflow_frame_missing"))?;
        if i.arguments["target_id"] != self.target || i.arguments["tab_id"] != self.tab {
            return Err(bad("workflow_target_changed"));
        }
        let parameter = format!("input_{}", self.steps.len() + 1);
        if i.operation == "browser_navigate" {
            return Ok(Step::Navigate {
                origin: origin(i.arguments["url"].as_str().unwrap_or(""))?,
                parameter,
            });
        }
        if self.steps.is_empty() {
            return Err(bad("workflow_must_start_with_navigation"));
        }
        complete(&frame.arguments)?;
        let node = nodes(&frame.arguments)
            .find(|n| n["ref"] == i.arguments["ref"])
            .ok_or_else(|| bad("workflow_requires_semantic_ref"))?;
        let locator = Locator {
            role: node["role"].as_str().unwrap_or("").into(),
            name: node["name"].as_str().unwrap_or("").into(),
        };
        locator.resolve(&frame.arguments)?;
        let origin = page_origin(&frame.arguments)?;
        match i.operation.as_str() {
            "browser_click"
                if i.arguments.get("x").is_none()
                    && i.arguments.get("y").is_none()
                    && i.arguments
                        .get("input_route")
                        .is_none_or(|v| v == "trusted") =>
            {
                Ok(Step::Click { origin, locator })
            }
            "browser_type"
                if i.arguments["replace"] == true
                    && i.arguments.get("mode").is_none_or(|v| v == "insert_text") =>
            {
                Ok(Step::Type {
                    origin,
                    locator,
                    parameter,
                })
            }
            _ => Err(bad("workflow_operation_not_reusable")),
        }
    }
    pub(super) fn commit(&mut self, step: Option<Step>) {
        if let Some(step) = step {
            self.steps.push(step);
            self.valid = true;
        }
    }
    pub(super) fn invalidate(&mut self) {
        self.valid = false;
    }
}
fn root(ctx: &ToolContext) -> PathBuf {
    std::env::var_os("YUNXI_BOT_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| ctx.cwd.join("data"))
        .join("browser-workflows")
}
fn directory(root: &std::path::Path, create: bool) -> Result<()> {
    if create && !root.exists() {
        fs::DirBuilder::new()
            .mode(0o700)
            .create(root)
            .map_err(|_| failed("workflow_store_unavailable：父数据目录须已存在"))?;
    }
    let m = fs::symlink_metadata(root).map_err(|_| failed("workflow_store_unavailable"))?;
    let uid = fs::metadata(std::env::var_os("HOME").ok_or_else(|| failed("HOME不可用"))?)
        .map_err(|_| failed("HOME不可用"))?
        .uid();
    if !m.is_dir()
        || m.file_type().is_symlink()
        || m.uid() != uid
        || m.permissions().mode() & 0o077 != 0
    {
        return Err(failed("workflow_store_not_private"));
    }
    Ok(())
}
fn read(root: &std::path::Path, name: &str) -> Result<Recipe> {
    if !safe_name(name) {
        return Err(bad("workflow_name_invalid"));
    }
    directory(root, false)?;
    let path = root.join(format!("{name}.json"));
    let m = fs::symlink_metadata(&path).map_err(|_| failed("workflow_not_found"))?;
    if !m.is_file()
        || m.file_type().is_symlink()
        || m.permissions().mode() & 0o077 != 0
        || m.len() > 64_000
    {
        return Err(failed("workflow_file_invalid"));
    }
    // O_NOFOLLOW is the macOS flag; validate the opened object as well as its path.
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(0x100)
        .open(&path)
        .map_err(|_| failed("workflow_read_failed"))?;
    let opened = file
        .metadata()
        .map_err(|_| failed("workflow_read_failed"))?;
    if opened.ino() != m.ino()
        || opened.nlink() != 1
        || opened.uid()
            != fs::metadata(root)
                .map_err(|_| failed("workflow_store_unavailable"))?
                .uid()
    {
        return Err(failed("workflow_file_changed_or_shared"));
    }
    let mut raw = String::new();
    file.take(64_001)
        .read_to_string(&mut raw)
        .map_err(|_| failed("workflow_read_failed"))?;
    let recipe: Recipe = serde_json::from_str(&raw).map_err(|_| bad("workflow_schema_invalid"))?;
    if recipe.name != name
        || recipe.version != 1
        || recipe.steps.is_empty()
        || recipe.steps.len() > 32
        || !matches!(recipe.steps[0], Step::Navigate { .. })
        || origin(&recipe.final_origin)? != recipe.final_origin
    {
        return Err(bad("workflow_schema_invalid"));
    }
    for (i, step) in recipe.steps.iter().enumerate() {
        let (site, locator, param) = match step {
            Step::Navigate { origin, parameter } => (origin, None, Some(parameter)),
            Step::Click { origin, locator } => (origin, Some(locator), None),
            Step::Type {
                origin,
                locator,
                parameter,
            } => (origin, Some(locator), Some(parameter)),
        };
        if origin(site)? != *site
            || locator.is_some_and(|l| !l.valid())
            || param.is_some_and(|p| *p != format!("input_{}", i + 1))
        {
            return Err(bad("workflow_schema_invalid"));
        }
    }
    Ok(recipe)
}
fn store(root: &std::path::Path, recipe: &Recipe) -> Result<()> {
    directory(root, true)?;
    let temporary = root.join(format!(
        ".candidate-{}-{}",
        std::process::id(),
        NEXT_ID.fetch_add(1, Ordering::Relaxed)
    ));
    let data = serde_json::to_vec_pretty(recipe).map_err(|_| failed("workflow_encode_failed"))?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)
        .map_err(|_| failed("workflow_store_failed"))?;
    let result = file
        .write_all(&data)
        .and_then(|()| file.sync_all())
        .and_then(|()| fs::hard_link(&temporary, root.join(format!("{}.json", recipe.name))));
    fs::remove_file(&temporary).map_err(|_| failed("workflow_temporary_cleanup_failed"))?;
    result.map_err(|_| failed("workflow_name_exists_or_store_failed：不会覆盖旧流程"))
}

pub(super) fn parameters(name: &str) -> Value {
    match name {
        "browser_workflow_run" => {
            json!({"type":"object","properties":{"name":{"type":"string"},"target_id":{"type":"string"},"tab_id":{"type":"string"},"parameters":{"type":"object","description":"inspect返回的input_N与expected_result；所有值为本次字符串，禁止密码验证码"}},"required":["name","target_id","tab_id","parameters"],"additionalProperties":false})
        }
        "browser_workflow_save" => {
            json!({"type":"object","properties":{"success_text":{"type":"string","description":"当前页面实际可见的完成条件，本值只用于本次核验，不保存"}},"required":["success_text"],"additionalProperties":false})
        }
        _ => {
            json!({"type":"object","properties":{"operation":{"enum":["list","inspect","begin","cancel"]},"name":{"type":"string","description":"1–64 ASCII字母数字_-，不放个人资料"}},"required":["operation"],"additionalProperties":false})
        }
    }
}
pub(super) fn call(tool: &ComputerTool, args: &Value, ctx: &mut ToolContext) -> Result<ToolOutput> {
    if args.to_string().len() > 64_000 {
        return Err(bad("workflow_request_too_large"));
    }
    if tool.name == "browser_workflow_run" {
        return Err(bad("workflow_runner_required：必须经过ToolRunner逐步审批"));
    }
    let root = tool
        .controller
        .lock()
        .map_err(|_| failed("操控状态不可用"))?
        .workflow_home
        .as_ref()
        .map(|p| p.join("browser-workflows"))
        .unwrap_or_else(|| root(ctx));
    if tool.name == "browser_workflow_save" {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Save {
            success_text: String,
        }
        let request: Save =
            serde_json::from_value(args.clone()).map_err(|_| bad("workflow_save_invalid"))?;
        if ctx.sandbox == crate::policy::SandboxMode::ReadOnly {
            return Err(bad("workflow_read_only"));
        }
        let _lock = DesktopLock::acquire()?;
        let mut c = tool
            .controller
            .lock()
            .map_err(|_| failed("操控状态不可用"))?;
        let rec = c
            .recording
            .as_ref()
            .ok_or_else(|| bad("workflow_recording_missing"))?;
        if !rec.valid || rec.steps.is_empty() {
            return Err(bad(
                "workflow_recording_invalid：失败、切换目标或不可复用动作后需重新学习",
            ));
        }
        let frame = c
            .frame
            .as_ref()
            .ok_or_else(|| bad("workflow_frame_missing"))?;
        complete(&frame.arguments)?;
        if frame.arguments["target_id"] != rec.target || frame.arguments["tab_id"] != rec.tab {
            return Err(bad("workflow_target_changed"));
        }
        let epoch = fs::read_to_string(
            PathBuf::from(std::env::var_os("HOME").ok_or_else(|| failed("HOME不可用"))?)
                .join(".cua-driver/yunxi-epoch"),
        )
        .map_err(|_| bad("workflow_epoch_missing"))?;
        let verified = workflow::verify(
            &json!({"observation_id":frame.id,"checks":[{"kind":"text_present","text":request.success_text}]}),
            Some(frame),
            &epoch,
        )?;
        if verified["matched"] != true {
            return Err(bad("workflow_success_condition_unmatched"));
        }
        let recipe = Recipe {
            version: 1,
            name: rec.name.clone(),
            steps: rec.steps.clone(),
            final_origin: page_origin(&frame.arguments)?,
        };
        store(&root, &recipe)?;
        c.recording = None;
        return Ok(ToolOutput::changed(json!({"saved":recipe.name,"status":"candidate","steps":recipe.steps.len(),"note":"需用不同参数回放核验；没有存输入值、截图、Cookie或旧ref"}).to_string()));
    }
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Request {
        operation: String,
        name: Option<String>,
    }
    let request: Request =
        serde_json::from_value(args.clone()).map_err(|_| bad("workflow_request_invalid"))?;
    match request.operation.as_str() {
        "begin" => {
            let name = request
                .name
                .filter(|s| safe_name(s))
                .ok_or_else(|| bad("workflow_name_invalid"))?;
            let mut c = tool
                .controller
                .lock()
                .map_err(|_| failed("操控状态不可用"))?;
            let frame = c
                .frame
                .as_ref()
                .ok_or_else(|| bad("workflow_frame_missing：先观察目标标签"))?;
            if !frame.arguments["target_id"].is_string()
                || !frame.arguments["tab_id"].is_string()
                || c.choice_pending.is_some()
            {
                return Err(bad("workflow_exact_tab_required"));
            }
            c.recording = Some(Recording {
                name,
                target: frame.arguments["target_id"].clone(),
                tab: frame.arguments["tab_id"].clone(),
                steps: vec![],
                valid: true,
            });
            Ok(ToolOutput::read(
                "开始在内存学习；下一动作必须browser_navigate。只支持导航、唯一role/name点击和replace=true输入；其他动作/错误使候选失效。完成后重新观察，browser_workflow_save核验并保存。",
            ))
        }
        "cancel" => {
            tool.controller
                .lock()
                .map_err(|_| failed("操控状态不可用"))?
                .recording = None;
            Ok(ToolOutput::read("已丢弃内存候选"))
        }
        "inspect" => Ok(ToolOutput::read(
            serde_json::to_string(&read(&root, request.name.as_deref().unwrap_or(""))?)
                .map_err(|_| failed("workflow_encode_failed"))?,
        )),
        "list" => {
            if !root.exists() {
                return Ok(ToolOutput::read("[]"));
            }
            directory(&root, false)?;
            let mut list = vec![];
            for entry in fs::read_dir(&root).map_err(|_| failed("workflow_list_failed"))? {
                let entry = entry.map_err(|_| failed("workflow_list_failed"))?;
                if entry.path().extension().is_some_and(|x| x == "json") {
                    let n = entry
                        .path()
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or("")
                        .to_owned();
                    let r = read(&root, &n)?;
                    list.push(json!({"name":r.name,"status":"candidate_recheck_each_run","steps":r.steps.len()}));
                }
            }
            list.sort_by_key(|v| v["name"].as_str().unwrap_or("").to_owned());
            Ok(ToolOutput::read(json!(list).to_string()))
        }
        _ => Err(bad("workflow_operation_invalid")),
    }
}

fn snapshot(output: &ToolOutput) -> Result<(String, Value)> {
    let id = output
        .text
        .lines()
        .next()
        .and_then(|s| s.strip_prefix("observation_id="))
        .ok_or_else(|| bad("workflow_observation_missing"))?;
    let raw = output
        .text
        .rsplit_once("结构化回执：")
        .map(|(_, s)| s.lines().next().unwrap_or(""))
        .ok_or_else(|| bad("workflow_snapshot_missing"))?;
    let data: Value = serde_json::from_str(raw).map_err(|_| bad("workflow_snapshot_invalid"))?;
    complete(&data)?;
    Ok((id.into(), data))
}
fn action(
    step: &Step,
    id: &str,
    data: &Value,
    params: &BTreeMap<String, String>,
    target: &str,
    tab: &str,
) -> Result<Value> {
    let mut arguments = json!({"target_id":target,"tab_id":tab});
    if data["target_id"] != target || data["tab_id"] != tab {
        return Err(bad("workflow_target_changed"));
    }
    let op = match step {
        Step::Navigate {
            origin: site,
            parameter,
        } => {
            let url = params
                .get(parameter)
                .ok_or_else(|| bad("workflow_parameter_missing"))?;
            if origin(url)? != *site {
                return Err(bad("workflow_origin_changed"));
            }
            arguments["url"] = json!(url);
            "browser_navigate"
        }
        Step::Click {
            origin: site,
            locator,
        }
        | Step::Type {
            origin: site,
            locator,
            ..
        } => {
            if page_origin(data)? != *site {
                return Err(bad("workflow_origin_changed"));
            }
            arguments["ref"] = locator.resolve(data)?["ref"].clone();
            if let Step::Type { parameter, .. } = step {
                arguments["text"] = json!(
                    params
                        .get(parameter)
                        .ok_or_else(|| bad("workflow_parameter_missing"))?
                );
                arguments["replace"] = json!(true);
                "browser_type"
            } else {
                "browser_click"
            }
        }
    };
    Ok(
        json!({"operation":op,"arguments":arguments,"observation_id":id,"purpose":"回放已学习的网页流程；本次真实控件与参数仍须逐步批准"}),
    )
}

pub(crate) fn replay(
    args: &Value,
    ctx: &ToolContext,
    mut dispatch: impl FnMut(&str, Value) -> Result<ToolOutput>,
) -> Result<ToolOutput> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Run {
        name: String,
        target_id: String,
        tab_id: String,
        parameters: BTreeMap<String, String>,
    }
    if args.to_string().len() > 64_000 || ctx.sandbox == crate::policy::SandboxMode::ReadOnly {
        return Err(bad("workflow_request_invalid_or_read_only"));
    }
    let req: Run = serde_json::from_value(args.clone()).map_err(|_| bad("workflow_run_invalid"))?;
    if !text(&req.target_id) || !text(&req.tab_id) || req.parameters.values().any(|s| !text(s)) {
        return Err(bad("workflow_parameters_invalid"));
    }
    let loaded = dispatch(
        "browser_workflow",
        json!({"operation":"inspect","name":req.name}),
    )?;
    let recipe: Recipe =
        serde_json::from_str(&loaded.text).map_err(|_| bad("workflow_schema_invalid"))?;
    let mut expected = vec!["expected_result".to_string()];
    for step in &recipe.steps {
        if let Step::Navigate { parameter, .. } | Step::Type { parameter, .. } = step {
            expected.push(parameter.clone());
        }
    }
    expected.sort();
    if expected != req.parameters.keys().cloned().collect::<Vec<_>>() {
        return Err(bad(
            "workflow_parameters_mismatch：inspect流程后提供全部input_N与expected_result",
        ));
    }
    // Validate every URL before the first side effect, not only when its step is reached.
    for step in &recipe.steps {
        if let Step::Navigate {
            origin: site,
            parameter,
        } = step
            && origin(&req.parameters[parameter])? != *site
        {
            return Err(bad("workflow_origin_changed"));
        }
    }
    let observe = json!({"operation":"get_browser_state","arguments":{"target_id":req.target_id,"tab_id":req.tab_id,"include_screenshot":false,"snapshot_format":"semantic_v2"}});
    for (index, step) in recipe.steps.iter().enumerate() {
        let result = (|| {
            let start = Instant::now();
            let mut selected = None;
            for attempt in 0..8 {
                let observed = dispatch("computer_observe", observe.clone())?;
                let (id, data) = snapshot(&observed)?;
                match action(
                    step,
                    &id,
                    &data,
                    &req.parameters,
                    &req.target_id,
                    &req.tab_id,
                ) {
                    Ok(a) => {
                        selected = Some(a);
                        break;
                    }
                    Err(e)
                        if e.to_string().contains("workflow_locator_missing")
                            && attempt < 7
                            && start.elapsed() < Duration::from_secs(5) =>
                    {
                        std::thread::sleep(Duration::from_millis(200))
                    }
                    Err(e) => return Err(e),
                }
            }
            dispatch(
                "computer_act",
                selected.ok_or_else(|| bad("workflow_condition_timeout"))?,
            )?;
            Ok(())
        })();
        if let Err(e) = result {
            return Err(failed(&format!(
                "degraded=true workflow_stopped step={}：{e}；后续未执行，不自动重试已派发动作；重新观察后由模型/用户处理",
                index + 1
            )));
        }
    }
    let final_result = dispatch(
        "computer_observe",
        json!({"wait_for":{"timeout_ms":5000,"checks":[{"kind":"text_present","text":req.parameters["expected_result"]}]},"operation":"get_browser_state","arguments":observe["arguments"]}),
    )?;
    let (id, data) = snapshot(&final_result)?;
    if page_origin(&data)? != recipe.final_origin {
        return Err(bad("degraded=true workflow_final_origin_changed"));
    }
    let verification = dispatch(
        "computer_verify",
        json!({"observation_id":id,"checks":[{"kind":"text_present","text":req.parameters["expected_result"]}]}),
    )?;
    let result: Value = serde_json::from_str(&verification.text)
        .map_err(|_| failed("workflow_verification_invalid"))?;
    if result["matched"] != true {
        return Err(failed(
            "degraded=true workflow_postcondition_unmatched：动作可能已生效，不重跑",
        ));
    }
    // Validation is a local receipt, not authority for later actions. Keep the candidate immutable.
    Ok(ToolOutput::read(json!({"workflow":recipe.name,"replay_verified":true,"steps":recipe.steps.len(),"model_calls_during_replay":0,"task_completion":"not_assessed","note":"仅核验本次expected_result；业务结果仍需独立回读。流程不会获得永久动作授权"}).to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn data() -> Value {
        json!({"target_id":"t","tab_id":"tab","page":{"url":"https://example.org/form"},"snapshot":{"complete":true},"refs":[{"ref":"fresh","role":"textbox","name":"目的地","value":""}]})
    }
    fn locator() -> Locator {
        Locator {
            role: "textbox".into(),
            name: "目的地".into(),
        }
    }
    #[test]
    fn duplicate_partial_disabled_and_sensitive_controls_never_resolve() {
        let mut d = data();
        assert_eq!(locator().resolve(&d).unwrap()["ref"], "fresh");
        // Cua counts unknown-layout nodes even when they are included in a complete snapshot.
        d["snapshot"]["omitted"]["unknown"] = json!(1);
        assert!(locator().resolve(&d).is_ok());
        let duplicate = d["refs"][0].clone();
        d["refs"].as_array_mut().unwrap().push(duplicate);
        assert!(
            locator()
                .resolve(&d)
                .unwrap_err()
                .to_string()
                .contains("ambiguous")
        );
        for field in [
            "complete",
            "disabled",
            "password",
            "dialog",
            "html_dialog",
            "protected",
        ] {
            let mut d = data();
            match field {
                "complete" => d["snapshot"]["complete"] = json!(false),
                "disabled" => d["refs"][0]["disabled"] = json!(true),
                "password" => d["refs"][0]["type"] = json!("password"),
                "protected" => d["refs"][0]["states"]["protected"] = json!(true),
                "offscreen" => d["refs"][0]["visibility"] = json!("offscreen"),
                "html_dialog" => d["content_refs"] = json!([{"role":"dialog","name":"需要确认"}]),
                _ => d["_yunxi_dialog_observation"] = json!(true),
            }
            assert!(locator().resolve(&d).is_err(), "{field}");
        }
    }
    #[test]
    fn learned_steps_store_neither_input_values_nor_old_references() {
        let frame = Frame {
            id: "old".into(),
            arguments: data(),
            at: Instant::now(),
            text: String::new(),
            hotel_dates: None,
        };
        let mut r = Recording {
            name: "test".into(),
            target: json!("t"),
            tab: json!("tab"),
            steps: vec![Step::Navigate {
                origin: "https://example.org".into(),
                parameter: "input_1".into(),
            }],
            valid: true,
        };
        let i=input(&json!({"operation":"browser_type","arguments":{"target_id":"t","tab_id":"tab","ref":"fresh","text":"PRIVATE_INPUT_123","replace":true},"observation_id":"old","purpose":"test"})).unwrap();
        let step = r.stage(&i, Some(&frame)).unwrap();
        let raw = serde_json::to_string(&step).unwrap();
        assert!(!raw.contains("PRIVATE_INPUT") && !raw.contains("fresh") && !raw.contains("old"));
        let mut params = BTreeMap::new();
        params.insert("input_2".into(), "上海".into());
        let a = action(&step, "new", &data(), &params, "t", "tab").unwrap();
        assert_eq!(a["arguments"]["text"], "上海");
        assert_eq!(a["observation_id"], "new");
        let mut unsupported = i;
        unsupported.operation = "browser_hotkey".into();
        assert!(r.stage(&unsupported, Some(&frame)).is_none());
        assert!(!r.valid);
        assert!(r.stage(&i_for_type(), Some(&frame)).is_none());
    }
    fn i_for_type() -> Input {
        input(&json!({"operation":"browser_type","arguments":{"target_id":"t","tab_id":"tab","ref":"fresh","text":"new","replace":true},"observation_id":"old","purpose":"test"})).unwrap()
    }
    #[test]
    fn parameters_cannot_redirect_a_workflow_to_another_origin() {
        assert!(origin("https://user:pass@example.org/").is_err());
        assert!(origin("javascript:alert(1)").is_err());
        assert!(origin("https://example.org\\@evil.org/").is_err());
        assert_eq!(
            origin("https://EXAMPLE.org:443/a?private=yes#x").unwrap(),
            "https://example.org"
        );
        let step = Step::Navigate {
            origin: "https://example.org".into(),
            parameter: "input_1".into(),
        };
        let params = BTreeMap::from([("input_1".into(), "https://evil.org/".into())]);
        assert!(action(&step, "id", &data(), &params, "t", "tab").is_err());
    }
    #[test]
    fn store_is_private_new_only_and_rejects_symlinks_and_path_traversal() {
        let root = std::env::temp_dir().join(format!(
            "yunxi-recipe-{}-{}",
            std::process::id(),
            NEXT_ID.fetch_add(1, Ordering::Relaxed)
        ));
        let r = Recipe {
            version: 1,
            name: "test".into(),
            steps: vec![Step::Navigate {
                origin: "https://example.org".into(),
                parameter: "input_1".into(),
            }],
            final_origin: "https://example.org".into(),
        };
        store(&root, &r).unwrap();
        assert!(read(&root, "test").is_ok());
        assert!(store(&root, &r).is_err());
        assert!(read(&root, "../test").is_err());
        std::os::unix::fs::symlink(root.join("test.json"), root.join("link.json")).unwrap();
        assert!(read(&root, "link").is_err());
        assert_eq!(
            fs::metadata(root.join("test.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        fs::remove_dir_all(root).unwrap();
    }
}
