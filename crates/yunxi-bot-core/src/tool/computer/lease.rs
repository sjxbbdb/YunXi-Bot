//! Fresh observations and exact targets are checked before approval and again before dispatch.
use super::{ADAPTER_FIELDS, Frame, Input, Result, bad, failed};
use serde_json::Value;
use std::time::Duration;

pub(super) fn validate(i: &Input, schema: &Value, observe: bool) -> Result<()> {
    let readonly = if i.operation == "browser_dialog" {
        i.arguments["action"] == "inspect"
    } else {
        schema["observe"] == true
    };
    if schema.is_null() || readonly != observe {
        return Err(bad("不在已审核的操作目录或观察/动作入口错误"));
    }
    let args = i
        .arguments
        .as_object()
        .ok_or_else(|| bad("arguments必须是object"))?;
    for key in ADAPTER_FIELDS {
        if args.contains_key(*key) {
            return Err(bad("禁止覆盖会话、截图库或输出路径"));
        }
    }
    if args.contains_key("target")
        && ["pid", "window_id", "scope"]
            .iter()
            .any(|key| args.contains_key(*key))
    {
        return Err(bad(
            "target_shape_conflict：target与平铺pid/window_id/scope不能同时提供；只保留一种精确窗口形式，本次未派发",
        ));
    }
    if i.operation == "get_window_state" && args.contains_key("since") {
        return Err(bad(
            "current_snapshot_required：旧完整画面已退役，不能仅用增量差异；去掉since，读取完整或query限定的当前快照",
        ));
    }
    for required in schema["input_schema"]["required"]
        .as_array()
        .into_iter()
        .flatten()
    {
        if let Some(key) = required.as_str()
            && key != "session"
            && !args.contains_key(key)
        {
            return Err(bad("缺少Driver必填参数；先查看computer_schema"));
        }
    }
    // The driver validates types; unknown names are refused here even on permissive upstream schemas.
    for key in args.keys() {
        if schema["input_schema"]["properties"].get(key).is_none() {
            return Err(bad("未知Driver参数"));
        }
    }
    if i.operation == "browser_prepare"
        && i.arguments["strategy"]["kind"]
            .as_str()
            .is_some_and(|s| s == "existing_profile")
        && (["pid", "window_id"]
            .iter()
            .any(|key| i.arguments[*key].as_u64().is_none_or(|n| n == 0))
            || i.arguments["allow_launch"] == true
            || i.arguments.get("profile").is_some())
    {
        return Err(bad(
            "连接已授权的日常Edge须给精确pid/window_id；不接受同时启动或复制配置",
        ));
    }
    if i.arguments
        .get("max_image_dimension")
        .is_some_and(|v| v.as_u64().is_none_or(|n| n == 0 || n > 1600))
    {
        return Err(bad(
            "截图长边限1–1600像素；较小文字使用zoom观察，不能静默生成超大图",
        ));
    }
    if !observe && i.purpose.as_deref().is_none_or(|s| s.trim().is_empty()) {
        return Err(bad("动作必须说明具体目的，不能依据屏幕内容取得授权"));
    }
    Ok(())
}

pub(super) fn check_existing_edge(i: &Input) -> Result<()> {
    if i.operation != "browser_prepare" || i.arguments["strategy"]["kind"] != "existing_profile" {
        return Ok(());
    }
    let pid = i.arguments["pid"]
        .as_u64()
        .ok_or_else(|| bad("Edge进程缺失"))?;
    let process = crate::exec::run_command(
        &[
            "/bin/ps".into(),
            "-ww".into(),
            "-p".into(),
            pid.to_string(),
            "-o".into(),
            "comm=".into(),
        ],
        &crate::exec::ExecOptions {
            timeout_ms: 3000,
            ..Default::default()
        },
    )
    .map_err(|_| failed("无法核实日常Edge进程，未连接"))?;
    if !process.succeeded()
        || process.stdout.trim() != "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge"
    {
        return Err(bad(
            "本轮仅授权本机日常Microsoft Edge；进程身份不符，未连接",
        ));
    }
    Ok(())
}

pub(super) fn captures_frame(input: &Input) -> bool {
    [
        "list_apps",
        "list_windows",
        "get_window_state",
        "get_accessibility_tree",
        "get_desktop_state",
        "get_browser_state",
        "zoom",
    ]
    .contains(&input.operation.as_str())
        || (input.operation == "browser_dialog" && input.arguments["action"] == "inspect")
}

pub(super) fn check_frame(frame: Option<&Frame>, i: &Input, epoch: &str) -> Result<()> {
    let frame = frame.ok_or_else(|| bad("先观察目标窗口，再操控"))?;
    if i.observation_id.as_deref() != Some(frame.id.as_str())
        || epoch != frame.id
        || frame.at.elapsed() > Duration::from_secs(120)
    {
        return Err(bad(
            "stale_observation：观察已过期或其他会话已接管；未执行，重新观察",
        ));
    }
    super::native::check_action(frame, i)?;
    if i.operation == "click"
        && i.arguments["from_zoom"] != true
        && (i.arguments.get("x").is_some() || i.arguments.get("y").is_some())
    {
        if let Some(capture) = i.arguments.get("capture_id")
            && (!capture.is_string() || frame.arguments.get("capture_id") != Some(capture))
        {
            return Err(bad(
                "coordinate_capture_mismatch：坐标须来自当前观察的同一capture_id，未派发",
            ));
        }
        if let (Some(width), Some(height)) = (
            frame.arguments["screenshot_width"].as_f64(),
            frame.arguments["screenshot_height"].as_f64(),
        ) {
            let inside = i.arguments["x"]
                .as_f64()
                .zip(i.arguments["y"].as_f64())
                .is_some_and(|(x, y)| {
                    x.is_finite()
                        && y.is_finite()
                        && x >= 0.0
                        && y >= 0.0
                        && x < width
                        && y < height
                });
            if !inside {
                return Err(bad(
                    "coordinate_outside_capture：点位不在当前实际截图内，核对图片尺寸及坐标原点；未派发",
                ));
            }
        }
    }
    let dialog_frame = frame.arguments["_yunxi_dialog_observation"] == true;
    if dialog_frame && i.operation != "browser_dialog" {
        return Err(bad(
            "dialog_observation_only：这份观察只含弹窗，不是页面快照；处理弹窗后重新观察页面",
        ));
    }
    if i.operation == "browser_dialog"
        && i.arguments["action"] != "inspect"
        && (!dialog_frame
            || frame.arguments["present"] != true
            || i.arguments["dialog_id"].as_str().is_none()
            || i.arguments["dialog_id"] != frame.arguments["dialog_id"])
    {
        return Err(bad(
            "stale_dialog：先只读inspect当前弹窗，再使用其最新observation_id和dialog_id；不能凭旧页面或猜测确认",
        ));
    }
    if i.operation == "click"
        && (i.arguments.get("x").is_some() || i.arguments.get("y").is_some())
        && frame.arguments["screenshot_width"].is_number()
        && frame.arguments["capture_id"].as_str().is_none()
        && frame.arguments["snapshot_id"].as_str().is_none()
    {
        return Err(bad(
            "coordinate_context_unavailable：当前窗口截图仅供观察，没有可执行的截图上下文；本次未派发。重新观察仍不可用就ask_user，不重复猜坐标，也不自动关闭或取消弹窗",
        ));
    }
    let global = [
        "launch_app",
        "clipboard_write",
        "browser_prepare",
        "bring_to_front",
        "kill_app",
    ]
    .contains(&i.operation.as_str());
    if !global {
        let browser = i.operation.starts_with("browser_");
        if browser
            && frame.arguments["target_id"].is_string()
            && frame.arguments.get("tab_id").is_none()
            && frame.arguments["tabs"].is_array()
        {
            return Err(bad(
                "browser_binding_only：首次pid/window_id只绑定窗口，尚未读取标签。下一步必须computer_observe get_browser_state，用回执target_id及tabs中选定tab_id，不再传pid/window_id；取得该标签的新observation_id后再动作",
            ));
        }
        let desktop = i.arguments["target"]["kind"] == "desktop";
        let keys = if browser {
            &["target_id", "tab_id"][..]
        } else {
            &["pid", "window_id"][..]
        };
        if !desktop
            && keys.iter().any(|key| {
                i.arguments
                    .get(*key)
                    .or_else(|| i.arguments["target"].get(*key))
                    .is_none()
            })
        {
            return Err(bad(
                "动作须绑定精确窗口pid/window_id或浏览器target_id/tab_id，禁止默认前台目标",
            ));
        }
    }
    let app_action = ["bring_to_front", "kill_app"].contains(&i.operation.as_str());
    let known_app = app_action
        && i.arguments["pid"].as_u64().is_some_and(|pid| {
            frame.arguments["apps"]
                .as_array()
                .is_some_and(|apps| apps.iter().any(|a| a["pid"].as_u64() == Some(pid)))
        });
    if app_action && !known_app && i.arguments["pid"] != frame.arguments["pid"] {
        return Err(bad("应用未在最新观察中，未执行"));
    }
    for key in ["pid", "window_id", "target_id", "tab_id"] {
        if key == "pid" && known_app {
            continue;
        }
        let wanted = i
            .arguments
            .get(key)
            .or_else(|| i.arguments["target"].get(key));
        if let Some(wanted) = wanted
            && frame.arguments.get(key) != Some(wanted)
        {
            return Err(bad("动作目标与观察窗口/浏览器标签不一致"));
        }
    }
    if i.arguments.get("target").is_some()
        && i.arguments["target"]["kind"] == "desktop"
        && frame.arguments["desktop"] != true
    {
        return Err(bad("桌面坐标必须来自桌面截图"));
    }
    Ok(())
}
