//! 当前用户的 launchd LaunchAgent。配置只保存路径与启动参数，不复制凭证环境变量。
use std::path::{Path, PathBuf};

use crate::AutostartAction;
use yunxi_bot_core::exec::{ExecOptions, ExecOutcome, run_command};

const LABEL: &str = "com.yunxi.bot.agent";
type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

fn xml(text: &str) -> Result<String> {
    if text
        .chars()
        .any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t'))
    {
        return Err("LaunchAgent 配置包含 XML 不允许的控制字符".into());
    }
    Ok(text
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;"))
}

fn path_xml(path: &Path) -> Result<String> {
    xml(path.to_str().ok_or("LaunchAgent 路径不是有效 UTF-8")?)
}

fn plist(
    label: &str,
    exe: &Path,
    home: &Path,
    cwd: &Path,
    path: &str,
    args: &[String],
) -> Result<String> {
    if !exe.is_absolute() || !home.is_absolute() || !cwd.is_absolute() {
        return Err("LaunchAgent 必须使用绝对路径，不能依赖登录时的工作目录".into());
    }
    let mut argv = format!(
        "<string>{}</string>\n<string>daemon</string>\n",
        path_xml(exe)?
    );
    for arg in args {
        argv.push_str(&format!("<string>{}</string>\n", xml(arg)?));
    }
    Ok(format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>Label</key><string>{label}</string>
<key>ProgramArguments</key><array>{argv}</array>
<key>WorkingDirectory</key><string>{cwd}</string>
<key>EnvironmentVariables</key><dict>
<key>YUNXI_BOT_HOME</key><string>{home}</string>
<key>PATH</key><string>{path}</string>
</dict>
<key>RunAtLoad</key><true/>
<key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict>
<key>ThrottleInterval</key><integer>30</integer>
<key>LimitLoadToSessionType</key><string>Aqua</string>
<key>StandardOutPath</key><string>{stdout}</string>
<key>StandardErrorPath</key><string>{stderr}</string>
<key>Umask</key><integer>63</integer>
</dict></plist>
"#,
        label = xml(label)?,
        cwd = path_xml(cwd)?,
        home = path_xml(home)?,
        path = xml(path)?,
        stdout = path_xml(&home.join("logs/daemon.stdout.log"))?,
        stderr = path_xml(&home.join("logs/daemon.stderr.log"))?
    ))
}

fn launchctl(args: &[&str]) -> Result<ExecOutcome> {
    let command = std::iter::once("/bin/launchctl")
        .chain(args.iter().copied())
        .map(String::from)
        .collect::<Vec<_>>();
    Ok(run_command(
        &command,
        &ExecOptions {
            timeout_ms: 10_000,
            ..Default::default()
        },
    )?)
}

fn require_success(out: ExecOutcome, action: &str) -> Result<()> {
    if out.succeeded() {
        Ok(())
    } else {
        Err(format!(
            "{action}失败（退出码 {}，超时 {}）：{}",
            out.exit_code,
            out.timed_out,
            out.stderr.trim()
        )
        .into())
    }
}

/// 先完整写入候选配置，再替换；加载失败时恢复旧配置和原来的服务。
fn install_plist(
    file: &Path,
    body: &str,
    domain: &str,
    service: &str,
    call: &mut impl FnMut(&[&str]) -> Result<ExecOutcome>,
) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let previous = match std::fs::read(file) {
        Ok(body) => Some(body),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e.into()),
    };
    let staged = file.with_extension(format!("plist.{}.pending", std::process::id()));
    let mut writer = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&staged)?;
    let result = (|| -> Result<()> {
        writer.write_all(body.as_bytes())?;
        writer.sync_all()?;
        let file_str = file.to_str().ok_or("LaunchAgent 路径不是有效 UTF-8")?;
        let loaded = call(&["print", service])?.succeeded();
        if loaded {
            require_success(call(&["bootout", service])?, "卸载旧 LaunchAgent")?;
        }
        let update = (|| -> Result<()> {
            std::fs::rename(&staged, file)?;
            require_success(call(&["bootstrap", domain, file_str])?, "加载 LaunchAgent")
        })();
        if let Err(error) = update {
            if let Some(old) = &previous {
                std::fs::write(file, old)?;
                if loaded {
                    require_success(
                        call(&["bootstrap", domain, file_str])?,
                        "恢复旧 LaunchAgent",
                    )?;
                }
            } else if file.exists() {
                std::fs::remove_file(file)?;
            }
            return Err(error);
        }
        Ok(())
    })();
    let _ = std::fs::remove_file(staged);
    result
}

pub(super) fn run(action: AutostartAction, args: &[String]) -> Result<i32> {
    let user_home = std::env::var_os("HOME").ok_or("HOME 缺失，无法定位用户 LaunchAgents")?;
    let dir = PathBuf::from(user_home).join("Library/LaunchAgents");
    let file = dir.join(format!("{LABEL}.plist"));
    unsafe extern "C" {
        fn getuid() -> u32;
    }
    // SAFETY: getuid 无参数、无副作用，只读取调用者 UID。
    let domain = format!("gui/{}", unsafe { getuid() });
    let service = format!("{domain}/{LABEL}");
    match action {
        AutostartAction::Install => {
            let preview = args.first().is_some_and(|a| a == "--dry-run");
            let args = if preview { &args[1..] } else { args };
            let daemon_args = if args.is_empty() {
                args
            } else {
                args.strip_prefix(&["--".to_string()])
                    .ok_or("用法：install-autostart [--dry-run] [-- <daemon 参数>]")?
            };
            let cwd = std::env::current_dir()?;
            let home = std::path::absolute(crate::default_home())?;
            let exe = std::env::current_exe()?;
            let path = std::env::var("PATH").unwrap_or_else(|_| {
                "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin".into()
            });
            let body = plist(LABEL, &exe, &home, &cwd, &path, daemon_args)?;
            if preview {
                print!("{body}");
                return Ok(0);
            }
            std::fs::create_dir_all(&dir)?;
            std::fs::create_dir_all(home.join("logs"))?;
            install_plist(&file, &body, &domain, &service, &mut launchctl)?;
            println!("已注册并加载 macOS 登录自启：{}", file.display());
            println!("数据目录：{}", home.display());
            println!("日志目录：{}", home.join("logs").display());
            println!("launchd 已接管进程；用 autostart-status 查看运行状态。");
            Ok(0)
        }
        AutostartAction::Uninstall => {
            if launchctl(&["print", &service])?.succeeded() {
                require_success(launchctl(&["bootout", &service])?, "停止 LaunchAgent")?;
            }
            match std::fs::remove_file(&file) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
            println!("已取消 macOS 登录自启；保留运行数据。");
            Ok(0)
        }
        AutostartAction::Status => {
            println!(
                "配置：{}（{}）",
                file.display(),
                if file.is_file() {
                    "已注册"
                } else {
                    "未注册"
                }
            );
            let out = launchctl(&["print", &service])?;
            if out.succeeded() {
                for line in out.stdout.lines().filter(|line| {
                    line.trim().starts_with("state =")
                        || line.trim().starts_with("pid =")
                        || line.trim().starts_with("last exit code =")
                }) {
                    println!("{}", line.trim());
                }
                Ok(0)
            } else {
                println!("当前登录会话未查到此服务：{}", out.stderr.trim());
                Ok(1)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_launchctl_restores_the_previous_registration() {
        use yunxi_bot_core::exec::IsolationLevel;
        let root =
            std::env::temp_dir().join(format!("yunxi-launchd-rollback-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let file = root.join("agent.plist");
        for old in [None, Some("old plist")] {
            if let Some(old) = old {
                std::fs::write(&file, old).unwrap();
            }
            let mut bootstraps = 0;
            let mut call = |args: &[&str]| -> Result<ExecOutcome> {
                if args[0] == "bootstrap" {
                    bootstraps += 1;
                    if bootstraps == 1 {
                        return Err("launchctl could not start".into());
                    }
                }
                Ok(ExecOutcome {
                    exit_code: if args[0] == "print" && old.is_none() {
                        1
                    } else {
                        0
                    },
                    stdout: String::new(),
                    stderr: String::new(),
                    timed_out: false,
                    duration_ms: 0,
                    isolation: IsolationLevel::ProcessOnly,
                    invalid_utf8_bytes: 0,
                })
            };
            assert!(
                install_plist(&file, "new plist", "gui/501", "gui/501/test", &mut call).is_err()
            );
            assert_eq!(std::fs::read_to_string(&file).ok().as_deref(), old);
            assert_eq!(bootstraps, if old.is_some() { 2 } else { 1 });
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn launchd_plist_keeps_paths_and_arguments_intact_without_a_shell() {
        let root = std::env::temp_dir().join(format!("yunxi-launchd-plist-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let file = root.join("agent.plist");
        let exe = root.join("中文 & \"quoted\"/yunxi-bot");
        let home = root.join("data & state");
        let args = vec!["--agent".into(), "--interval".into(), "5000".into()];
        std::fs::write(
            &file,
            plist("com.yunxi.test", &exe, &home, &root, "/bin:/usr/bin", &args).unwrap(),
        )
        .unwrap();
        let output = std::process::Command::new("/usr/bin/plutil")
            .args(["-convert", "json", "-o", "-", "--"])
            .arg(&file)
            .output()
            .unwrap();
        std::fs::remove_dir_all(&root).unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let v: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(
            v["ProgramArguments"],
            serde_json::json!([exe, "daemon", "--agent", "--interval", "5000"])
        );
        assert_eq!(
            v["EnvironmentVariables"]["YUNXI_BOT_HOME"],
            home.to_str().unwrap()
        );
        assert_eq!(v["EnvironmentVariables"].as_object().unwrap().len(), 2);
        assert_eq!(v["KeepAlive"]["SuccessfulExit"], false);
    }

    #[test]
    fn invalid_paths_and_control_characters_cannot_be_registered() {
        assert!(
            plist(
                "x",
                Path::new("relative"),
                Path::new("/tmp"),
                Path::new("/tmp"),
                "/bin",
                &[]
            )
            .is_err()
        );
        assert!(xml("bad\u{0001}path").is_err());
    }
}
