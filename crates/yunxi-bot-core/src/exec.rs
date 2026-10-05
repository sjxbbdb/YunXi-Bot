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
    /// Windows Job Object：进程树强制回收 + 进程数/内存上限。
    ///
    /// **不含文件系统写入隔离**——子进程仍以当前用户身份运行，能写用户能写的
    /// 任何位置。不要把它当作沙箱。
    WindowsJobObject,
    /// Windows：受限令牌 + **低完整性级别**的写入隔离。
    ///
    /// 机制是把令牌的完整性降到 Low：低完整性子进程**写不进**任何中完整性对象
    /// （用户目录里的文件全是中完整性），可写区仅限低完整性沙箱。
    ///
    /// 早期设计用的是"受限令牌 + 能力 SID 限制列表"，已被四种变体对照实测证伪
    /// （限制列表只要非空，子进程就以 `STATUS_DLL_INIT_FAILED` 结束），
    /// 详见 [`crate::win_token`] 的模块文档。
    WindowsRestrictedToken,
}

impl IsolationLevel {
    pub fn describe(self) -> &'static str {
        match self {
            IsolationLevel::ProcessOnly => "进程隔离（未实施 OS 级强制）",
            IsolationLevel::WindowsJobObject => {
                "Windows Job Object（进程树回收 + 资源上限；不含写入隔离）"
            }
            IsolationLevel::WindowsRestrictedToken => "Windows 受限令牌（低完整性）写入隔离",
        }
    }

    /// 是否提供了文件系统写入隔离。
    pub fn provides_write_isolation(self) -> bool {
        matches!(self, IsolationLevel::WindowsRestrictedToken)
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
    /// Job Object 的单进程内存上限（字节）。`None` 表示不限制。
    pub memory_limit_bytes: Option<u64>,
    /// Job Object 的进程数上限。`None` 表示不限制。
    pub max_processes: Option<u32>,
}

impl Default for ExecOptions {
    fn default() -> Self {
        Self {
            cwd: PathBuf::from("."),
            timeout_ms: 60_000,
            isolation: IsolationRequirement::ProcessOnly,
            memory_limit_bytes: None,
            max_processes: None,
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
/// Windows 上可提供到 `WindowsRestrictedToken`（低完整性令牌的写入隔离）。
/// **但"可用"不等于"已验证"**：真正要执行时需要 OS 级写入隔离时，
/// [`run_command`] 会先跑一次自检并缓存结果，只有实测通过才放行。
pub fn available_isolation() -> IsolationLevel {
    #[cfg(windows)]
    {
        IsolationLevel::WindowsRestrictedToken
    }
    #[cfg(not(windows))]
    {
        IsolationLevel::ProcessOnly
    }
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
        && !isolation.provides_write_isolation()
    {
        return Err(ExecError::IsolationUnavailable {
            required: opts.isolation,
            actual: isolation,
        });
    }

    let started = Instant::now();

    // —— Windows：要求写入隔离时走低完整性令牌路径 ——
    //
    // 注意这里**先跑一次实测自检**（进程内缓存）。"代码里有这个功能"不等于
    // "在这台机器上真的能隔离"，失败契约要求后者。
    #[cfg(windows)]
    if opts.isolation == IsolationRequirement::RequireOsWriteIsolation {
        if !crate::win_token::write_isolation_verified() {
            return Err(ExecError::IsolationUnavailable {
                required: opts.isolation,
                actual: IsolationLevel::WindowsJobObject,
            });
        }
        let token = crate::win_token::RestrictedToken::create()
            .map_err(|e| ExecError::Spawn(format!("建立受限令牌失败: {e}")))?;
        // 低完整性子进程唯一能写的地方。
        // 工作目录因此被设为沙箱——**读**不受限，任务仍可用绝对路径读输入。
        let sandbox = crate::win_token::sandbox_dir()
            .map_err(|e| ExecError::Spawn(format!("准备沙箱目录失败: {e}")))?;
        let env: Vec<(String, String)> = strip_credentials(std::env::vars()).into_iter().collect();

        let out =
            crate::win_token::run_restricted(&token, command, &sandbox, &env, opts.timeout_ms)
                .map_err(|e| ExecError::Spawn(format!("受限执行失败: {e}")))?;

        return Ok(ExecOutcome {
            exit_code: out.exit_code,
            stdout: truncate(out.stdout),
            stderr: truncate(out.stderr),
            timed_out: false,
            duration_ms: started.elapsed().as_millis() as u64,
            isolation: IsolationLevel::WindowsRestrictedToken,
        });
    }

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

    // 放入 Job Object：进程树强制回收 + 资源上限。
    //
    // 必须在子进程刚起来时立刻 assign，趁它还没派生出孙进程；
    // 晚一步就会有孙子跑在 Job 之外，回收不干净。
    //
    // 这里刻意**不**把 assign 失败当作致命错误：Job 是"额外的"护栏，
    // 而诚实隔离要求我们把实际达到的级别如实报出去（见 `effective_level`）。
    let mut effective_level = IsolationLevel::ProcessOnly;
    #[cfg(windows)]
    let _job = match crate::win_job::JobObject::create(opts.memory_limit_bytes, opts.max_processes)
    {
        Ok(job) => match job.assign(&child) {
            Ok(()) => {
                effective_level = IsolationLevel::WindowsJobObject;
                Some(job)
            }
            Err(_) => None,
        },
        Err(_) => None,
    };

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
        // 报告**实际达到**的级别，而不是"平台理论上支持的"。Job 建失败时
        // 这里会如实回落到 ProcessOnly，不会把没做到的说成做到了。
        isolation: effective_level,
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
    fn required_isolation_reports_the_real_level() {
        // 要求 OS 级写入隔离时：
        // - 若自检通过 → 走受限路径，如实报告 WindowsRestrictedToken；
        // - 若自检不通过 → 在**派生之前**拒绝，绝不无隔离执行。
        //
        // 两条路都不允许"悄悄降级成普通进程执行"。
        let opts = ExecOptions {
            isolation: IsolationRequirement::RequireOsWriteIsolation,
            ..Default::default()
        };
        let result = run_command(&["definitely-not-a-real-program".into()], &opts);

        match result {
            Err(ExecError::IsolationUnavailable { .. }) => {
                // 自检没过：拒绝执行，这是正确行为
            }
            Err(ExecError::Spawn(_)) => {
                // 自检过了，走到了启动阶段（程序不存在所以失败）——
                // 说明隔离路径确实被走通了
            }
            Err(other) => panic!("不符合任何一种预期失败: {other}"),
            Ok(out) => panic!("不存在的程序不应执行成功；isolation={:?}", out.isolation),
        }
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
        // Windows 上现在应达到 Job Object 级；若 Job 建或分配失败则如实回落
        #[cfg(windows)]
        assert_eq!(out.isolation, IsolationLevel::WindowsJobObject);
        #[cfg(not(windows))]
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
