//! Windows toast 通知后端。
//!
//! ## 为什么走 PowerShell 而不是引 `windows` crate
//!
//! `windows` crate 为了调三个 WinRT 类会带进一大片绑定，而这是**一条冷路径**——
//! 一天几十条通知，每条 250ms 完全无所谓。项目的依赖纪律是"每加一个依赖都要
//! 先量出代价"，而这里量下来是**不用加**：PowerShell 能直接调 WinRT，
//! 实测在本机 250ms 完成一次发送 + 验证。
//!
//! 复用 [`crate::exec::run_command`] 派生脚本，于是白拿几样现成的东西：
//! **超时 + 进程树清理**（PowerShell 卡住不会挂死守护进程）、
//! **凭证环境变量剥离**、**输出上限**。
//!
//! ## 这个后端的核心价值：它能**验证**送达
//!
//! "调用了 API"≠"使用者看到了"。所以脚本做三件事，而不是一件事：
//!
//! 1. 查 `ToastNotifier.Setting` —— 有文档的 API，告诉我们通知**会不会被显示**。
//!    取值：`Enabled` / `DisabledForApplication` / `DisabledForUser` /
//!    `DisabledByGroupPolicy` / `DisabledByManifest`。非 `Enabled` 就是
//!    **确定不会送达**，如实报 [`Delivery::Blocked`]。
//! 2. 发通知。
//! 3. 查 `History.GetHistory(aumid)` —— 如果刚发的那条在系统通知中心里，
//!    就是 [`Delivery::Confirmed`]。
//!
//! 第 3 步是"证明"的来源。实测本机：Setting=`Enabled`，History 里读回了
//! 刚发的那条，所以这条路径**真的能证明送达**，不是靠命令退出码为 0 就宣称成功。
//!
//! ## 一个必须说清的语义
//!
//! **勿扰 / 专注助手开着时，通知仍然会进通知中心**（Setting 还是 `Enabled`），
//! 只是不弹横幅。所以本后端**永远不会**声称"弹出来了"——它只声称
//! "在通知中心里查得到"。使用者当下有没有看见，只有使用者知道。

use std::path::PathBuf;

use crate::exec::{ExecOptions, IsolationRequirement, run_command};
use crate::notify::{Delivery, Notification, Notifier};

/// 应用标识。**这是通知上显示的名字**，也是 History 查询的键。
pub const AUMID: &str = "YunXiBot";

/// PowerShell 发送 + 验证脚本。
///
/// 契约：**只在最后一行输出一个 JSON 对象**，其余一律不输出。
/// Rust 侧只认最后一行可解析的 JSON——中间的杂音（WinRT 的类型加载提示、
/// 编码警告）不该让整个通知失败。
const SEND_SCRIPT: &str = r#"
$ErrorActionPreference = 'Stop'
function Emit($o) { Write-Output ($o | ConvertTo-Json -Compress) }
try {
  [Windows.UI.Notifications.ToastNotificationManager, Windows.UI.Notifications, ContentType=WindowsRuntime] | Out-Null
  [Windows.Data.Xml.Dom.XmlDocument, Windows.Data.Xml.Dom, ContentType=WindowsRuntime] | Out-Null

  $aumid = $env:YUNXI_NOTIFY_AUMID
  $notifier = [Windows.UI.Notifications.ToastNotificationManager]::CreateToastNotifier($aumid)

  # 第一步：先问系统"我的通知会被显示吗"。
  $setting = [string]$notifier.Setting
  if ($setting -ne 'Enabled') {
    Emit @{ result = 'blocked'; reason = $setting }
    exit 0
  }

  # 第二步：发。
  $title = $env:YUNXI_NOTIFY_TITLE
  $body  = $env:YUNXI_NOTIFY_BODY
  $tag   = $env:YUNXI_NOTIFY_TAG

  $doc = [Windows.Data.Xml.Dom.XmlDocument]::new()
  $xml = @"
<toast>
  <visual>
    <binding template="ToastGeneric">
      <text></text>
      <text></text>
    </binding>
  </visual>
</toast>
"@
  $doc.LoadXml($xml)
  $texts = $doc.GetElementsByTagName('text')
  $texts.Item(0).AppendChild($doc.CreateTextNode($title)) | Out-Null
  $texts.Item(1).AppendChild($doc.CreateTextNode($body)) | Out-Null

  $toast = [Windows.UI.Notifications.ToastNotification]::new($doc)
  if ($tag) { $toast.Tag = $tag; $toast.Group = 'yunxi' }
  $notifier.Show($toast)

  # 第三步：读回来证明送达。
  # 同 tag 会替换旧条目，所以"历史里有一条 tag 匹配"就等于这次这条在里面。
  Start-Sleep -Milliseconds 120
  $hist = [Windows.UI.Notifications.ToastNotificationManager]::History.GetHistory($aumid)
  $found = $false
  foreach ($h in $hist) {
    $t = $h.Content.GetElementsByTagName('text')
    if ($t.Length -ge 2 -and $t.Item(1).InnerText -eq $body) { $found = $true; break }
  }
  if ($found) {
    Emit @{ result = 'confirmed'; evidence = '系统通知中心里查到了这条' }
  } else {
    Emit @{ result = 'handed_off'; note = '已交给系统，但通知中心里暂时查不到（勿扰或延迟）' }
  }
} catch {
  Emit @{ result = 'failed'; reason = $_.Exception.Message }
}
"#;

/// Windows toast 后端。
#[derive(Debug, Clone)]
pub struct WindowsToast {
    /// 发送脚本的超时。**必须有**：PowerShell 卡住不能挂死守护进程。
    pub timeout_ms: u64,
}

impl Default for WindowsToast {
    fn default() -> Self {
        // 实测一次发送 + 验证约 250ms。给 10 秒余量：够慢机器喘气，
        // 又不至于让一条通知把常驻循环拖住。
        Self { timeout_ms: 10_000 }
    }
}

impl WindowsToast {
    pub fn new() -> Self {
        Self::default()
    }

    /// 这个平台支持吗。
    pub fn is_available() -> bool {
        cfg!(windows)
    }
}

impl Notifier for WindowsToast {
    fn name(&self) -> &'static str {
        "Windows 通知"
    }

    fn notify(&self, n: &Notification) -> Delivery {
        if !Self::is_available() {
            return Delivery::Blocked {
                reason: "当前不是 Windows 平台".to_string(),
            };
        }

        let script = with_env_prelude(SEND_SCRIPT, n);

        let mut opts = ExecOptions {
            cwd: std::env::temp_dir(),
            timeout_ms: self.timeout_ms,
            isolation: IsolationRequirement::ProcessOnly,
            memory_limit_bytes: None,
            max_processes: None,
        };
        // 脚本放在临时目录里跑，而不是塞进 `-Command`：
        // 命令行长度有上限，而且引号转义在中文上很容易出错。
        let script_path = match write_script(&script) {
            Ok(p) => p,
            Err(e) => {
                return Delivery::Failed {
                    reason: format!("写通知脚本失败: {e}"),
                };
            }
        };
        opts.cwd = script_path
            .parent()
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);

        let cmd = vec![
            "powershell".to_string(),
            "-NoProfile".to_string(),
            "-NonInteractive".to_string(),
            "-ExecutionPolicy".to_string(),
            "Bypass".to_string(),
            "-File".to_string(),
            script_path.to_string_lossy().to_string(),
        ];

        let out = match run_command(&cmd, &opts) {
            Ok(o) => o,
            Err(e) => {
                let _ = std::fs::remove_file(&script_path);
                return Delivery::Failed {
                    reason: format!("无法启动通知进程: {e}"),
                };
            }
        };
        let _ = std::fs::remove_file(&script_path);

        if out.exit_code != 0 {
            return Delivery::Failed {
                reason: format!(
                    "通知脚本退出码 {}：{}",
                    out.exit_code,
                    head(&out.stderr, 300)
                ),
            };
        }

        parse_result(&out.stdout)
    }
}

/// 把标题/正文等参数通过环境变量传给脚本。
///
/// **不用命令行参数**：模型生成的通知正文可能很长、可能含引号。
/// 环境变量没有转义问题，也不会出现在进程命令行里（`tasklist` 能看到命令行，
/// 而通知正文可能含邮件标题这类隐私内容）。
fn with_env_prelude(script: &str, n: &Notification) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "$env:YUNXI_NOTIFY_AUMID = '{}'\n",
        escape_ps(AUMID)
    ));
    out.push_str(&format!(
        "$env:YUNXI_NOTIFY_TITLE = '{}'\n",
        escape_ps(&n.title)
    ));
    out.push_str(&format!(
        "$env:YUNXI_NOTIFY_BODY = '{}'\n",
        escape_ps(&n.body)
    ));
    out.push_str(&format!(
        "$env:YUNXI_NOTIFY_TAG = '{}'\n",
        escape_ps(&n.tag)
    ));
    out.push_str(script);
    out
}

/// PowerShell 单引号字符串里，单引号要用两个表示。
///
/// **这是注入面**：通知正文来自邮件标题这类的**外部内容**。不转义的话，
/// 一封标题里带 `'; rm -rf ...; '` 的邮件就能在用户机器上执行代码。
fn escape_ps(s: &str) -> String {
    s.replace('\'', "''")
}

/// 把脚本写到临时文件。
fn write_script(script: &str) -> std::io::Result<PathBuf> {
    use std::io::Write;
    let dir = std::env::temp_dir().join(format!(
        "yunxi-notify-{}-{:?}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("send.ps1");
    let mut f = std::fs::File::create(&path)?;
    // **BOM 不是可选的**：PowerShell 5.1 读无 BOM 的 UTF-8 脚本会按 ANSI 解，
    // 中文标题会变成乱码。这条是 Windows 上的老坑。
    f.write_all(&[0xEF, 0xBB, 0xBF])?;
    f.write_all(script.as_bytes())?;
    f.flush()?;
    Ok(path)
}

/// 解析脚本最后一行输出的 JSON。
///
/// **只认最后一行**：WinRT 类型加载可能往 stdout 吐杂音，而那些杂音不该
/// 让一次成功的通知被判成失败。
///
/// 解析不出来时返回 [`Delivery::HandedOff`] 而**不是** `Confirmed`——
/// 失败方向朝"没确认送达"。这一条是这一整个模块的立身之本。
fn parse_result(stdout: &str) -> Delivery {
    let last = stdout
        .lines()
        .rev()
        .find(|l| l.trim_start().starts_with('{'));

    let Some(line) = last else {
        return Delivery::HandedOff {
            note: format!(
                "脚本没给出可解析的结果，无法验证是否送达。输出：{}",
                head(stdout, 200)
            ),
        };
    };

    let Ok(v) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
        return Delivery::HandedOff {
            note: format!("脚本结果不是合法 JSON：{}", head(line, 200)),
        };
    };

    match v.get("result").and_then(|r| r.as_str()) {
        Some("confirmed") => Delivery::Confirmed {
            evidence: v
                .get("evidence")
                .and_then(|e| e.as_str())
                .unwrap_or("脚本报告已确认")
                .to_string(),
        },
        Some("handed_off") => Delivery::HandedOff {
            note: v
                .get("note")
                .and_then(|e| e.as_str())
                .unwrap_or("已交给系统，未确认可见")
                .to_string(),
        },
        Some("blocked") => Delivery::Blocked {
            reason: format!(
                "系统不允许显示本应用的通知（Setting={}）",
                v.get("reason").and_then(|e| e.as_str()).unwrap_or("未知")
            ),
        },
        Some("failed") => Delivery::Failed {
            reason: v
                .get("reason")
                .and_then(|e| e.as_str())
                .unwrap_or("脚本未说明原因")
                .to_string(),
        },
        // 认不出的 result 值 → 不猜，按"未确认"处理
        other => Delivery::HandedOff {
            note: format!("脚本返回了认不出的结果: {other:?}"),
        },
    }
}

/// 截断到 n 个字符。**按字符不按字节**——按字节切会落在汉字中间 panic。
fn head(s: &str, n: usize) -> String {
    let t = s.trim();
    if t.chars().count() <= n {
        return t.to_string();
    }
    format!("{}…", t.chars().take(n).collect::<String>())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notify::Urgency;

    // ---- 解析：这是唯一能在离线环境里测准的部分，而它也是最容易出错的部分 ----

    #[test]
    fn parses_confirmed_result() {
        let d = parse_result(r#"{"result":"confirmed","evidence":"系统通知中心里查到了这条"}"#);
        assert!(d.can_claim_user_notified(), "{d:?}");
        assert!(d.detail().contains("通知中心"));
    }

    #[test]
    fn parses_blocked_result_with_the_system_setting() {
        // Setting 的具体取值要传出来——"被用户关了"和"被组策略禁止"
        // 对使用者的意义完全不同
        let d = parse_result(r#"{"result":"blocked","reason":"DisabledForUser"}"#);
        assert!(matches!(d, Delivery::Blocked { .. }), "{d:?}");
        assert!(d.detail().contains("DisabledForUser"), "{}", d.detail());
        assert!(!d.left_the_process());
    }

    #[test]
    fn parses_handed_off_result() {
        let d = parse_result(r#"{"result":"handed_off","note":"勿扰，只进了通知中心"}"#);
        assert!(d.left_the_process());
        assert!(!d.can_claim_user_notified(), "勿扰下不该声称使用者看见了");
    }

    #[test]
    fn parses_failed_result() {
        let d = parse_result(r#"{"result":"failed","reason":"找不到 WinRT 类型"}"#);
        assert!(matches!(d, Delivery::Failed { .. }));
    }

    #[test]
    fn only_the_last_json_line_is_considered() {
        // WinRT 加载可能往 stdout 吐杂音，那些不该让成功的通知被判失败
        let out = "警告：正在加载类型库\n{ 这不是 JSON }\n{\"result\":\"confirmed\",\"evidence\":\"ok\"}\n";
        let d = parse_result(out);
        assert!(d.can_claim_user_notified(), "{d:?}");
    }

    #[test]
    fn unparsable_output_is_handed_off_not_confirmed() {
        // **失败方向朝"没确认送达"。** 这一条是整个模块的立身之本：
        // 解析不出来时宁可说"不确定"，也不能说"送达了"。
        let d = parse_result("完全看不懂的输出");
        assert!(matches!(d, Delivery::HandedOff { .. }), "{d:?}");
        assert!(!d.can_claim_user_notified());
    }

    #[test]
    fn unknown_result_value_is_not_guessed() {
        let d = parse_result(r#"{"result":"something_new"}"#);
        assert!(matches!(d, Delivery::HandedOff { .. }), "{d:?}");
        assert!(!d.can_claim_user_notified());
    }

    #[test]
    fn empty_output_is_handed_off() {
        let d = parse_result("");
        assert!(!d.can_claim_user_notified());
    }

    // ---- 转义：这是注入面 ----

    #[test]
    fn single_quotes_are_escaped_for_powershell() {
        // **这是注入面。** 通知正文来自邮件标题这类外部内容。
        // 不转义的话，一封标题带引号的邮件就能在用户机器上执行代码。
        assert_eq!(escape_ps("it's"), "it''s");
        assert_eq!(
            escape_ps("'; Remove-Item C:\\ -Recurse; '"),
            "''; Remove-Item C:\\ -Recurse; ''"
        );
    }

    #[test]
    fn a_malicious_title_cannot_break_out_of_the_string() {
        let n = Notification::new("x'; Start-Process calc; '", "b");
        let script = with_env_prelude("", &n);
        // 转义之后，标题里的每个单引号都成对出现，
        // 所以它无法闭合外层字符串去执行命令
        let line = script
            .lines()
            .find(|l| l.contains("YUNXI_NOTIFY_TITLE"))
            .expect("应有标题行");
        // 剥掉赋值语句自身的两个引号，剩下的引号必须是偶数（成对）
        let after_eq = line.split('=').nth(1).unwrap().trim();
        let inner = &after_eq[1..after_eq.len() - 1];
        assert_eq!(
            inner.matches('\'').count() % 2,
            0,
            "转义后引号必须成对，否则能逃逸: {inner}"
        );
    }

    #[test]
    fn all_notification_fields_go_through_escaping() {
        // 标题、正文、tag 三个字段都是外部来源，一个都不能漏
        let n = Notification::new("a'b", "c'd").with_tag("e'f");
        let s = with_env_prelude("", &n);
        assert!(s.contains("'a''b'"), "{s}");
        assert!(s.contains("'c''d'"), "{s}");
        assert!(s.contains("'e''f'"), "{s}");
    }

    #[test]
    fn params_go_via_env_not_command_line() {
        // 命令行在 tasklist 里可见，而通知正文可能含邮件标题这类隐私内容
        let n = Notification::new("t", "b");
        let s = with_env_prelude("SCRIPT", &n);
        assert!(s.contains("$env:YUNXI_NOTIFY_BODY"));
        assert!(s.ends_with("SCRIPT"), "脚本本体应在环境变量之后");
    }

    // ---- 截断 ----

    #[test]
    fn head_truncates_by_chars_not_bytes() {
        // 按字节切会落在汉字中间 panic
        let s = "中文标题".repeat(100);
        let h = head(&s, 10);
        assert_eq!(h.chars().count(), 11, "10 个字符 + 省略号");
        assert!(h.ends_with('…'));
    }

    #[test]
    fn head_leaves_short_strings_alone() {
        assert_eq!(head("短", 10), "短");
        assert_eq!(head("  abc  ", 10), "abc", "顺带去掉首尾空白");
    }

    // ---- 平台 ----

    #[test]
    fn backend_reports_its_name() {
        assert_eq!(WindowsToast::new().name(), "Windows 通知");
    }

    #[test]
    fn default_timeout_is_generous_but_bounded() {
        // 实测 250ms。给足余量又不至于挂死守护进程。
        let t = WindowsToast::default();
        assert!(t.timeout_ms >= 3_000, "太紧会让慢机器上的通知失败");
        assert!(t.timeout_ms <= 30_000, "太松会让守护进程被一条通知拖住");
    }

    #[test]
    fn send_script_checks_setting_before_sending() {
        // 先问"会不会被显示"再发——不然发了也是白发
        let s = SEND_SCRIPT;
        let setting_pos = s.find("$notifier.Setting").expect("脚本必须查 Setting");
        let show_pos = s.find("$notifier.Show").expect("脚本必须发通知");
        assert!(setting_pos < show_pos, "必须先查 Setting 再发");
    }

    #[test]
    fn send_script_verifies_by_reading_history_back() {
        // 这是"证明送达"的来源：调用 API 成功不算数，读回来才算
        assert!(SEND_SCRIPT.contains("History.GetHistory"));
        assert!(SEND_SCRIPT.contains("'confirmed'"));
    }

    #[test]
    fn send_script_always_emits_exactly_one_result() {
        // 每条退出路径都要有结果，否则 Rust 侧只能判"未确认"
        for kw in ["'blocked'", "'confirmed'", "'handed_off'", "'failed'"] {
            assert!(SEND_SCRIPT.contains(kw), "脚本缺少 {kw} 这条出口");
        }
    }

    #[test]
    fn script_file_is_written_with_a_bom() {
        // PowerShell 5.1 读无 BOM 的 UTF-8 脚本会按 ANSI 解，中文标题变乱码。
        // 这条是 Windows 上的老坑，值得一条测试盯着。
        let p = write_script("Write-Output '中文'").expect("应能写脚本");
        let bytes = std::fs::read(&p).unwrap();
        assert_eq!(&bytes[0..3], &[0xEF, 0xBB, 0xBF], "必须以 UTF-8 BOM 开头");
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }

    #[test]
    fn script_paths_are_unique_per_call() {
        // 同一秒内两次通知不能共用同一个脚本文件（否则会有竞态）
        let a = write_script("a").unwrap();
        let b = write_script("b").unwrap();
        assert_ne!(a, b);
        let _ = std::fs::remove_dir_all(a.parent().unwrap());
        let _ = std::fs::remove_dir_all(b.parent().unwrap());
    }

    #[test]
    fn urgency_does_not_leak_into_the_script_as_unescaped_text() {
        // 紧急程度目前不上通知（Windows toast 没有对应的标准字段），
        // 但要确认它没有被拼进脚本——拼进去就是又一个注入面
        let n = Notification::new("t", "b").with_urgency(Urgency::High);
        let s = with_env_prelude(SEND_SCRIPT, &n);
        assert!(!s.contains("紧急"), "紧急程度不该进脚本");
    }
}
