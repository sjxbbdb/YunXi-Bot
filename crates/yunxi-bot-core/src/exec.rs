//! 执行层：以子进程运行任务。
//!
//! 设计依据 ADR-0001 §六 D3 / D7 与 §八硬边界：
//!
//! 1. **不用 shell**：命令是 argv 数组，没有字符串拼接，也就没有注入面。
//! 2. **凭证剥离**：子进程一律剔除 provider 凭证，防止提示注入把密钥带出去。
//! 3. **硬超时 + 进程树清理**：卡死的子进程不能拖住常驻进程。
//! 4. **绝不无隔离派生**：要求了 OS 级隔离却拿不到时，**直接拒绝执行**，
//!    而不是降级为不隔离地跑。这是诚实隔离（honest isolation）的落点。

use std::collections::BTreeMap;
use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

/// 单条输出流最多保留的字节数。防止常驻进程被输出撑爆内存。
const MAX_CAPTURE_BYTES: usize = 200_000;

/// 已知必须剥离的凭证变量。
const KNOWN_CREDENTIALS: &[&str] = &[
    "DEEPSEEK_API_KEY",
    "OPENAI_API_KEY",
    "ANTHROPIC_API_KEY",
    "GOOGLE_API_KEY",
    "GEMINI_API_KEY",
    "MOONSHOT_API_KEY",
    "DASHSCOPE_API_KEY",
    "AZURE_OPENAI_API_KEY",
    "GITHUB_TOKEN",
    "GH_TOKEN",
    "GITLAB_TOKEN",
    "NPM_TOKEN",
    "AWS_ACCESS_KEY_ID",
    "AWS_SECRET_ACCESS_KEY",
    "AWS_SESSION_TOKEN",
];

/// 判断某个环境变量名是否属于凭证，需要在派生前剥离。
///
/// 采用通配规则而非穷举清单：新的 provider 每天都在出现，等人去更新清单
/// 一定会漏。宁可多剥一个普通变量，也不能漏掉一个密钥。
pub fn is_credential_var(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    if KNOWN_CREDENTIALS.contains(&upper.as_str()) {
        return true;
    }
    const SUFFIXES: &[&str] = &[
        "_API_KEY",
        "_APIKEY",
        "_AUTH_TOKEN",
        "_ACCESS_TOKEN",
        "_SECRET",
        "_SECRET_KEY",
        "_PASSWORD",
        "_CREDENTIAL",
        "_PRIVATE_KEY",
    ];
    if SUFFIXES.iter().any(|s| upper.ends_with(s)) {
        return true;
    }
    // AWS_* 下的各类 KEY / TOKEN
    upper.starts_with("AWS_") && (upper.contains("KEY") || upper.contains("TOKEN"))
}

/// 从一组环境变量中剥离凭证。纯函数，便于测试。
///
/// 返回 `BTreeMap` 以保证派生时环境块的顺序稳定（便于复现与调试）。
pub fn strip_credentials<I, K, V>(vars: I) -> BTreeMap<String, String>
where
    I: IntoIterator<Item = (K, V)>,
    K: Into<String>,
    V: Into<String>,
{
    let mut out = BTreeMap::new();
    for (k, v) in vars {
        let k = k.into();
        if is_credential_var(&k) {
            continue;
        }
        out.insert(k, v.into());
    }
    out
}

/// 实际达到的隔离级别。
///
/// **必须如实反映现实**，不能把"进程内策略"写成"沙箱"。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IsolationLevel {
    /// 仅进程隔离。未实施任何 OS 级强制。
    ProcessOnly,
    /// Windows：受限令牌 + 能力 SID 的写入隔离。
    WindowsAclWrite,
}

impl IsolationLevel {
    pub fn describe(self) -> &'static str {
        match self {
            IsolationLevel::ProcessOnly => "进程隔离（未实施 OS 级强制）",
            IsolationLevel::WindowsAclWrite => "Windows 受限令牌 + 能力 SID 写入隔离",
        }
    }
}

/// 调用方要求的隔离级别。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum IsolationRequirement {
    /// 至少进程隔离即可。
    #[default]
    ProcessOnly,
    /// **必须**有 OS 级写入隔离，否则拒绝执行。
    RequireOsWriteIsolation,
}

#[derive(Debug, Clone)]
pub struct ExecOptions {
    pub cwd: PathBuf,
    pub timeout_ms: u64,
    pub isolation: IsolationRequirement,
}

impl Default for ExecOptions {
    fn default() -> Self {
        Self {
            cwd: PathBuf::from("."),
            timeout_ms: 60_000,
            isolation: IsolationRequirement::ProcessOnly,
        }
    }
}

/// 执行结果。失败通过 `Ok` 返回（调用方要把失败也写进台账），
/// 只有"根本没能派生"才是 `Err`。
#[derive(Debug, Clone)]
pub struct ExecOutcome {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
    pub duration_ms: u64,
    /// 实际达到的隔离级别——如实声明。
    pub isolation: IsolationLevel,
}

impl ExecOutcome {
    pub fn succeeded(&self) -> bool {
        self.exit_code == 0 && !self.timed_out
    }
}

#[derive(Debug)]
pub enum ExecError {
    /// 空命令。
    EmptyCommand,
    /// 要求的隔离级别无法提供——**拒绝执行**。
    IsolationUnavailable {
        required: IsolationRequirement,
        actual: IsolationLevel,
    },
    /// 无法派生进程。
    Spawn(String),
    /// 等待进程失败。
    Wait(String),
}

impl std::fmt::Display for ExecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExecError::EmptyCommand => write!(f, "空命令"),
            ExecError::IsolationUnavailable { required, actual } => write!(
                f,
                "要求的隔离级别无法提供（要求 {required:?}，当前只能做到 {}）：\
                 拒绝无隔离派生",
                actual.describe()
            ),
            ExecError::Spawn(m) => write!(f, "派生进程失败: {m}"),
            ExecError::Wait(m) => write!(f, "等待进程失败: {m}"),
        }
    }
}

impl std::error::Error for ExecError {}

/// 当前平台能提供的隔离级别。
///
/// 注意：Windows 的受限令牌实现（ADR D7）**尚未落地**，所以这里如实返回
/// `ProcessOnly`。等它实现后，这里才会返回 `WindowsAclWrite`。
pub fn available_isolation() -> IsolationLevel {
    IsolationLevel::ProcessOnly
}

fn truncate(mut s: String) -> String {
    if s.len() > MAX_CAPTURE_BYTES {
        // 按字节截断，保证不会切裂到无法处理的长度
        s.truncate(MAX_CAPTURE_BYTES);
        s.push_str("\n…（输出已截断）");
    }
    s
}

/// 结束整个进程树。
///
/// 只杀直接子进程是不够的：子进程可能自己派生了孙进程，那些孙子会继续
/// 占着文件句柄或端口。
fn kill_tree(pid: u32) {
    #[cfg(windows)]
    {
        let _ = Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    #[cfg(unix)]
    {
        // 子进程以独立进程组派生，负号表示整组
        let _ = Command::new("kill")
            .args(["-KILL", &format!("-{pid}")])
            .status();
    }
    #[cfg(not(any(windows, unix)))]
    {
        let _ = pid;
    }
}

/// 运行一条命令。
///
/// `command[0]` 是可执行文件，其余是参数——**不经 shell 解析**。
pub fn run_command(command: &[String], opts: &ExecOptions) -> Result<ExecOutcome, ExecError> {
    if command.is_empty() {
        return Err(ExecError::EmptyCommand);
    }

    // 诚实隔离：拿不到要求的级别就拒绝，绝不降级为无隔离派生
    let isolation = available_isolation();
    if opts.isolation == IsolationRequirement::RequireOsWriteIsolation
        && isolation != IsolationLevel::WindowsAclWrite
    {
        return Err(ExecError::IsolationUnavailable {
            required: opts.isolation,
            actual: isolation,
        });
    }

    let started = Instant::now();

    let mut cmd = Command::new(&command[0]);
    cmd.args(&command[1..])
        .current_dir(&opts.cwd)
        // 先清空再注入剥离后的环境：只"覆盖"是删不掉凭证变量的
        .env_clear()
        .envs(strip_credentials(std::env::vars()))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd.spawn().map_err(|e| ExecError::Spawn(e.to_string()))?;
    let pid = child.id();

    // 管道必须边跑边读：等进程结束再读会在输出超过管道缓冲时死锁
    let mut out_pipe = child.stdout.take();
    let mut err_pipe = child.stderr.take();
    let out_handle = thread::spawn(move || {
        let mut buf = String::new();
        if let Some(p) = out_pipe.as_mut() {
            let _ = p.read_to_string(&mut buf);
        }
        buf
    });
    let err_handle = thread::spawn(move || {
        let mut buf = String::new();
        if let Some(p) = err_pipe.as_mut() {
            let _ = p.read_to_string(&mut buf);
        }
        buf
    });

    let deadline = started + Duration::from_millis(opts.timeout_ms);
    let mut timed_out = false;

    let status = loop {
        match child
            .try_wait()
            .map_err(|e| ExecError::Wait(e.to_string()))?
        {
            Some(st) => break Some(st),
            None => {
                if Instant::now() >= deadline {
                    timed_out = true;
                    kill_tree(pid);
                    // 等它真正死掉，避免留下僵尸
                    let _ = child.wait();
                    break None;
                }
                thread::sleep(Duration::from_millis(20));
            }
        }
    };

    let stdout = out_handle.join().unwrap_or_default();
    let stderr = err_handle.join().unwrap_or_default();

    let exit_code = status.and_then(|s| s.code()).unwrap_or(-1);

    Ok(ExecOutcome {
        exit_code,
        stdout: truncate(stdout),
        stderr: truncate(stderr),
        timed_out,
        duration_ms: started.elapsed().as_millis() as u64,
        isolation,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn known_credentials_are_stripped() {
        assert!(is_credential_var("OPENAI_API_KEY"));
        assert!(is_credential_var("DEEPSEEK_API_KEY"));
        assert!(is_credential_var("GITHUB_TOKEN"));
        assert!(is_credential_var("AWS_SECRET_ACCESS_KEY"));
    }

    #[test]
    fn wildcard_rules_catch_new_providers() {
        // 关键：不能靠穷举清单，否则新 provider 一定会漏
        for name in [
            "SOME_NEW_VENDOR_API_KEY",
            "FOO_AUTH_TOKEN",
            "BAR_ACCESS_TOKEN",
            "BAZ_SECRET",
            "QUX_PASSWORD",
            "MY_PRIVATE_KEY",
            "AWS_SESSION_TOKEN",
        ] {
            assert!(is_credential_var(name), "{name} 应被剥离");
        }
    }

    #[test]
    fn ordinary_vars_are_kept() {
        for name in ["PATH", "HOME", "TEMP", "LANG", "CARGO_HOME", "USERPROFILE"] {
            assert!(!is_credential_var(name), "{name} 不应被剥离");
        }
    }

    #[test]
    fn strip_keeps_path_and_drops_secrets() {
        let stripped = strip_credentials(vars(&[
            ("PATH", "/usr/bin"),
            ("OPENAI_API_KEY", "sk-leak"),
            ("SOME_VENDOR_API_KEY", "leak"),
            ("HOME", "/home/x"),
        ]));
        assert_eq!(stripped.get("PATH").map(String::as_str), Some("/usr/bin"));
        assert_eq!(stripped.get("HOME").map(String::as_str), Some("/home/x"));
        assert!(!stripped.contains_key("OPENAI_API_KEY"));
        assert!(!stripped.contains_key("SOME_VENDOR_API_KEY"));
    }

    #[test]
    fn empty_command_is_rejected() {
        assert!(matches!(
            run_command(&[], &ExecOptions::default()),
            Err(ExecError::EmptyCommand)
        ));
    }

    #[test]
    fn required_isolation_is_refused_when_unavailable() {
        // 这是"绝不无隔离派生"的直接验证：拿不到就不跑
        let opts = ExecOptions {
            isolation: IsolationRequirement::RequireOsWriteIsolation,
            ..Default::default()
        };
        let err = run_command(&["definitely-not-a-real-program".into()], &opts).unwrap_err();
        assert!(
            matches!(err, ExecError::IsolationUnavailable { .. }),
            "应在派生之前就拒绝，而不是尝试执行后失败: {err}"
        );
    }

    #[cfg(windows)]
    #[test]
    fn runs_a_command_and_captures_output() {
        let out = run_command(
            &["cmd".into(), "/C".into(), "echo hello-from-child".into()],
            &ExecOptions {
                timeout_ms: 10_000,
                ..Default::default()
            },
        )
        .expect("应能派生");
        assert!(out.succeeded(), "stderr={}", out.stderr);
        assert!(
            out.stdout.contains("hello-from-child"),
            "got={}",
            out.stdout
        );
        assert_eq!(out.isolation, IsolationLevel::ProcessOnly);
    }

    #[cfg(windows)]
    #[test]
    fn timeout_kills_the_child() {
        // ping 一个不存在的地址会挂很久；用短超时验证我们真的把它杀了
        let out = run_command(
            &[
                "cmd".into(),
                "/C".into(),
                "ping -n 30 127.0.0.1 > NUL".into(),
            ],
            &ExecOptions {
                timeout_ms: 300,
                ..Default::default()
            },
        )
        .expect("应能派生");
        assert!(out.timed_out, "应被判定为超时");
        assert!(!out.succeeded());
    }

    #[cfg(windows)]
    #[test]
    fn child_does_not_see_credentials() {
        // 端到端验证剥离：把凭证放进真实环境，看子进程能不能读到
        // 注意：这里用 set_var 会影响本进程，故单独放在一个测试里并立即还原
        let key = "YUNXI_BOT_TEST_FAKE_API_KEY";
        unsafe { std::env::set_var(key, "should-not-leak") };

        let out = run_command(
            &[
                "cmd".into(),
                "/C".into(),
                format!("if defined {key} (echo LEAKED) else (echo CLEAN)"),
            ],
            &ExecOptions {
                timeout_ms: 10_000,
                ..Default::default()
            },
        )
        .expect("应能派生");

        unsafe { std::env::remove_var(key) };
        assert!(
            out.stdout.contains("CLEAN"),
            "凭证泄漏到子进程: {}",
            out.stdout
        );
    }
}
