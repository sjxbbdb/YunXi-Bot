//! Fixed Cua contract, transport and private response decoding.
use super::{Input, Result, ToolOutput, failed};
use crate::think::ImageInput;
use serde_json::{Value, json};
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

const VERSION: &str = "cua-driver 0.36.0";
const HOST_ID: &str = "local.yunxi.bot.computer";

fn bundled_app() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/macos/云熙电脑组件.app")
}

fn embedded_spec(driver: &Path, socket: &Path) -> Result<crate::mcp::ServerSpec> {
    Ok(crate::mcp::ServerSpec {
        name: "computer".into(),
        command: driver
            .to_str()
            .ok_or_else(|| failed("电脑组件路径不是UTF-8"))?
            .into(),
        args: vec![
            "mcp".into(),
            "--embedded".into(),
            "--socket".into(),
            socket
                .to_str()
                .ok_or_else(|| failed("电脑组件socket路径不是UTF-8"))?
                .into(),
            "--host-bundle-id".into(),
            HOST_ID.into(),
        ],
        env: [
            ("CUA_DRIVER_RS_TELEMETRY_ENABLED".into(), "false".into()),
            ("CUA_TELEMETRY_ENABLED".into(), "false".into()),
        ]
        .into(),
        require_approval: true,
    })
}

fn host_identity_matches(value: &Value) -> bool {
    value["structuredContent"]["checks"]
        .as_array()
        .is_some_and(|checks| {
            checks.iter().any(|check| {
                check["name"] == "bundle_identity"
                    && check["status"] == "pass"
                    && check["data"]["bundle_identifier"] == HOST_ID
                    && check["data"]["identity_source"] == "parent_application"
                    && check["data"]["parent_process_id"]
                        .as_u64()
                        .is_some_and(|pid| pid > 0)
            })
        })
}

pub(super) fn catalog() -> &'static Value {
    static CATALOG: OnceLock<Value> = OnceLock::new();
    CATALOG.get_or_init(|| {
        serde_json::from_str(include_str!("driver-0.36.0.json")).expect("bundled, tested JSON")
    })
}
pub(super) fn connect() -> Result<crate::mcp::McpHub> {
    let app = bundled_app();
    let driver = app.join("Contents/MacOS/cua-driver");
    let out = crate::exec::run_command(
        &[driver.to_string_lossy().into(), "--version".into()],
        &crate::exec::ExecOptions {
            timeout_ms: 3000,
            ..Default::default()
        },
    )
    .map_err(|_| {
        failed("computer_unavailable：缺少内置电脑组件，请运行scripts/build-macos-computer.py")
    })?;
    if !out.succeeded() || out.stdout.trim() != VERSION {
        return Err(failed("Driver版本不匹配，须重新审核操作契约"));
    }
    let home = std::env::var_os("HOME").ok_or_else(|| failed("HOME缺失，无法定位电脑组件"))?;
    let state = PathBuf::from(home).join("Library/Application Support/YunXiBot/computer");
    let socket = state.join("driver.sock");
    let launched = crate::exec::run_command(
        &[
            "/usr/bin/open".into(),
            "-g".into(),
            app.to_string_lossy().into(),
        ],
        &crate::exec::ExecOptions {
            timeout_ms: 5000,
            ..Default::default()
        },
    )
    .map_err(|_| failed("computer_unavailable：内置宿主启动失败"))?;
    if !launched.succeeded() {
        return Err(failed("computer_unavailable：内置宿主启动失败"));
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if let (Ok(dir), Ok(sock)) = (
            std::fs::symlink_metadata(&state),
            std::fs::symlink_metadata(&socket),
        ) {
            if !dir.is_dir()
                || dir.mode() & 0o077 != 0
                || !sock.file_type().is_socket()
                || sock.uid() != dir.uid()
            {
                return Err(failed("computer_unavailable：内置组件端点不安全"));
            }
            if std::os::unix::net::UnixStream::connect(&socket).is_ok() {
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    if Instant::now() >= deadline {
        return Err(failed(
            "computer_unavailable：内置组件未就绪；检查组件安装与权限，不回退外部Driver",
        ));
    }
    let cfg = crate::mcp::McpConfig {
        servers: vec![embedded_spec(&driver, &socket)?],
    };
    let (mut hub, problems) = crate::mcp::McpHub::connect(&cfg)
        .map_err(|_| failed("computer_unavailable：Driver握手失败"))?;
    if !problems.is_empty() {
        return Err(failed(
            "computer_unavailable：Driver握手失败，检查系统权限和服务状态",
        ));
    }
    let identity = driver_call(
        &mut hub,
        "health_report",
        json!({"include":["bundle_identity"]}),
    )?;
    if !host_identity_matches(&identity) {
        return Err(failed(
            "computer_unavailable：Driver实际宿主身份不匹配，停止连接",
        ));
    }
    Ok(hub)
}

pub(super) fn driver_call(hub: &mut crate::mcp::McpHub, op: &str, args: Value) -> Result<Value> {
    let out = hub.call_content("computer", op, args).map_err(|_| {
        failed(
            "degraded=true delivery_unknown：Driver调用失败或超时，先观察并查询结果，禁止盲目重试",
        )
    })?;
    serde_json::to_value(out).map_err(|_| failed("Driver响应解码失败"))
}

pub(super) fn dialog_present(value: &Value) -> Result<bool> {
    parse_response(value.clone(), false)?;
    value["structuredContent"]["present"].as_bool().ok_or_else(||failed("degraded=true dialog_watch_unavailable：无法核实弹窗状态，当前观察未绑定，不执行页面动作"))
}

pub(super) fn parse_response(value: Value, changed: bool) -> Result<ToolOutput> {
    if value["isError"] == true
        || value["is_error"] == true
        || value["structuredContent"]["refusal"].is_object()
        || matches!(
            value["structuredContent"]["status"].as_str(),
            Some("refused" | "failed" | "error")
        )
        || matches!(
            value["structuredContent"]["effect"].as_str(),
            Some("refused" | "failed" | "error")
        )
    {
        let code = value["structuredContent"]["refusal"]["code"]
            .as_str()
            .or_else(|| value["structuredContent"]["error"]["code"].as_str())
            .or_else(|| value["structuredContent"]["code"].as_str());
        // Only reviewed fixed codes escape; upstream messages can contain private page content.
        let recovery = match code {
            Some("browser_wrong_target_refused") => {
                "browser_wrong_target_refused：无法精确验证浏览器窗口或设置控件，停止并核对目标"
            }
            Some("browser_consent_required") => {
                "browser_consent_required：当前会话尚未建立配置接入授权；先经已授权的browser_prepare连接，不能自行提权"
            }
            Some("browser_requires_setup") => {
                "browser_requires_setup：先精确原生观察，再经审批browser_prepare"
            }
            Some("background_unavailable" | "browser_background_unavailable") => {
                "background_unavailable：该设置可能前置窗口；需用户允许后foreground，不盲试"
            }
            Some("browser_endpoint_owner_mismatch") => {
                "browser_endpoint_owner_mismatch：端点身份不符，停止连接"
            }
            Some("browser_route_unavailable") => {
                "browser_route_unavailable：当前连接路径不可用，停止并核对浏览器支持"
            }
            Some("browser_input_trust_unavailable") => {
                "browser_input_trust_unavailable：当前输入路径不能保持所需交付方式，停止核对"
            }
            Some("browser_action_unavailable") => {
                "browser_action_unavailable：控件不支持该动作，重新观察实际actions"
            }
            Some("browser_ref_stale" | "browser_binding_stale") => {
                "browser_target_stale：页面或引用已变化，重新读取精确标签；旧ref和弹窗缓存不能当当前状态"
            }
            Some(
                "browser_binding_ambiguous" | "browser_tab_required" | "browser_tab_not_found",
            ) => {
                "browser_target_unresolved：标签缺失或目标不唯一，先读取真实标签目录，不猜目标或复用旧索引"
            }
            Some("browser_input_incomplete") => {
                "browser_input_incomplete：输入可能只完成一部分，先回读实际字段及焦点，不重新输入整段"
            }
            Some("browser_reconnect_exhausted") => {
                "browser_reconnect_exhausted：连接恢复失败，当前弹窗状态未知，不能以缓存无记录判断没有弹窗；停止动作并交还用户"
            }
            Some("browser_consent_revoked" | "browser_origin_outside_scope") => {
                "browser_access_refused：接入授权已撤销或目标超出范围，停止，不自动扩权或切换路径"
            }
            Some("screenshot_context_missing") => {
                "screenshot_context_missing：截图未建立可执行坐标上下文；能查看不代表能点击。重新观察仍无上下文时交还用户，不反复猜坐标"
            }
            _ => "Driver报告失败，不能确认结果",
        };
        return Err(failed(&format!(
            "degraded=true：{recovery}；动作可能部分送达，先重新观察或查实际订单，不重复提交"
        )));
    }
    let mut images = Vec::new();
    let mut text = Vec::new();
    if let Some(content) = value["content"].as_array() {
        for block in content {
            match block["type"].as_str() {
                Some("text") => text.push(block["text"].as_str().unwrap_or("").to_string()),
                Some("image") => images.push(
                    ImageInput::from_base64(
                        block["data"]
                            .as_str()
                            .ok_or_else(|| failed("Driver图片缺少编码"))?,
                    )
                    .map_err(|_| failed("Driver图片校验失败，不能声称看过截图"))?,
                ),
                _ => return Err(failed("Driver返回未支持内容块，不能静默忽略")),
            }
        }
    } else {
        return Err(failed("Driver缺少MCP内容块，不能确认结果"));
    }
    if let Some(mut metadata) = value.get("structuredContent").cloned() {
        if metadata["native_dialogs"]
            .as_array()
            .is_some_and(|dialogs| !dialogs.is_empty())
        {
            text.clear();
            text.push("【活动原生弹窗】以下内容来自该精确父窗口的实际AX弹窗子树；buttons/fields给出当前element_token。先理解正文与选项，不点背景控件，不用父窗口截图猜子弹窗坐标。当前图片仍是该父窗口的截图。需要选择却没有明确用户意图时使用ask_user；重要信息保留。".into());
            if metadata["native_full_output"] != true
                && let Some(object) = metadata.as_object_mut()
            {
                object.remove("elements");
                object.remove("tree_markdown");
                object.insert("view_scope".into(), json!("active_native_dialogs_only"));
            }
        } else if metadata["degraded_reason"]
            .as_str()
            .is_some_and(|s| s.starts_with("ax_window_unresolved"))
        {
            text.push("【需要观察真实父窗口】这是独立CG弹窗图片，AX及点击上下文未建立；不是换坐标或强制foreground能修复。用list_windows(pid=当前pid)查同一应用的父窗口，再以它的真实pid/window_id读取get_window_state；从实际AXSheet子树取得按钮与element_token。不得将本图坐标用于父窗口，不伪造capture_id，不能确认父子关系就ask_user并保留弹窗。".into());
        }
        if let Some(object) = metadata.as_object_mut() {
            object.remove("screenshot_png_b64");
            if let Some(snapshot) = object.get("snapshot").filter(|v| v.is_object()) {
                text.push(format!("浏览器定位与分页：{}", json!({"target_id":object.get("target_id"),"tab_id":object.get("tab_id"),"snapshot":snapshot})));
                if snapshot["complete"] == false {
                    text.push("【观察不完整】这是页面片段。所需行或同名按钮的上下文不全时，使用snapshot.continuation继续读取，或用实际行祖先scope_ref读取整行。表格缺少行引用时，可先query=row，从content_refs取实际row引用，再仅用scope_ref读取整行；确认同行信息与按钮后才动作。query会过滤兄弟节点，不能按同名按钮序号猜；continuation或scope_ref不要与query混用。也可滚动后读取带截图的完整片段，动作后重新观察。".into());
                }
            }
            if let Some(outline) = object
                .get("outline")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            {
                if !text.iter().any(|part| part.contains(outline)) {
                    text.push(outline.to_owned());
                }
                object.remove("outline");
            }
            if object
                .get("tree_markdown")
                .and_then(Value::as_str)
                .is_some_and(|tree| text.iter().any(|part| part.contains(tree)))
            {
                object.remove("tree_markdown");
            }
        }
        text.push(format!("结构化回执：{metadata}"));
    }
    if images.len() > 1 {
        return Err(failed("Driver返回多张截图，需缩小到一个窗口"));
    }
    let mut output = if changed {
        ToolOutput::changed(text.join("\n"))
    } else {
        ToolOutput::read(text.join("\n"))
    };
    output.images = images;
    Ok(output)
}

pub(super) fn driver_arguments(i: &Input, session: &str) -> Value {
    let mut args = i.arguments.clone();
    // Existing-profile browser calls require a stable public session in Driver 0.36.
    args["session"] = json!(session);
    if i.operation == "get_window_state"
        && args["include_accessibility_tree"] != false
        && (args.get("tree_format").is_none() || args["tree_format"] == "markdown")
    {
        args["tree_format"] = json!("both");
    }
    if ["get_window_state", "get_desktop_state"].contains(&i.operation.as_str())
        && args.get("max_image_dimension").is_none()
    {
        args["max_image_dimension"] = json!(1600);
    }
    if i.operation == "get_browser_state"
        && args.get("target_id").is_some()
        && args.get("snapshot_format").is_none()
    {
        args["snapshot_format"] = json!("semantic_v2");
    }
    if i.operation == "get_browser_state"
        && args.get("target_id").is_some()
        && args.get("include_screenshot").is_none()
    {
        args["include_screenshot"] = json!(true);
    }
    args
}

#[cfg(test)]
mod embedded_tests {
    use super::*;

    #[test]
    fn embedded_transport_cannot_fall_back_to_the_standalone_driver() {
        let spec = embedded_spec(
            Path::new("/bundle/cua-driver"),
            Path::new("/private/driver.sock"),
        )
        .unwrap();
        assert_eq!(spec.command, "/bundle/cua-driver");
        assert_eq!(
            spec.args,
            [
                "mcp",
                "--embedded",
                "--socket",
                "/private/driver.sock",
                "--host-bundle-id",
                HOST_ID
            ]
        );
        assert!(spec.require_approval);
        assert!(
            !spec
                .args
                .iter()
                .any(|a| a.contains("bypass") || a == "--grant")
        );
    }

    #[test]
    fn claimed_host_label_is_not_a_verified_parent_identity() {
        let mut value = json!({"structuredContent":{"checks":[{"name":"bundle_identity","status":"pass",
            "data":{"bundle_identifier":HOST_ID,"identity_source":"parent_application","parent_process_id":123}}]}});
        assert!(host_identity_matches(&value));
        value["structuredContent"]["checks"][0]["data"]["identity_source"] = json!("environment");
        assert!(!host_identity_matches(&value));
        assert!(!host_identity_matches(&json!({})));
    }

    #[test]
    #[ignore = "Requires the installed YunXi embedded host; read-only identity and permission probe"]
    fn live_embedded_host_identity_and_permissions() {
        let mut hub = connect().unwrap();
        let value = driver_call(&mut hub, "check_permissions", json!({"prompt":false})).unwrap();
        assert_eq!(value["structuredContent"]["source"]["attribution"], "host");
        assert_eq!(value["structuredContent"]["accessibility"], true);
        assert_eq!(value["structuredContent"]["screen_recording"], true);
    }
}
