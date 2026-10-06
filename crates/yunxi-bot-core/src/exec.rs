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
    /// 子进程输出里有多少字节不是合法 UTF-8、被替换掉了。
    ///
    /// **这个字段存在的理由是一类静默失效。** 原先这里写的是
    /// `let _ = p.read_to_string(&mut buf)`——`read_to_string` 要求输入是
    /// 合法 UTF-8，Windows 上一条 GBK 输出的命令就会让它返回 `Err`，
    /// 而 `let _ =` 把错误吞掉。结果模型看到一段**空输出**，
    /// 分不清"命令没输出"和"解码失败了"——这两种情况的下一步完全不同。
    ///
    /// 现在改成按字节读 + 有损解码，并**把替换掉的字节数如实报出来**：
    /// 调用方看到 `invalid_bytes > 0` 就知道"这段文字里有一部分没解出来"。
    pub invalid_utf8_bytes: usize,
}

impl ExecOutcome {
    pub fn succeeded(&self) -> bool {
        self.exit_code == 0 && !self.timed_out
    }

    /// 输出是不是**完整可信**的。
    ///
    /// 有损解码时为假。调用方在把输出交给模型之前应该看一眼这个——
    /// 一段被替换过的输出看起来和正常输出一模一样，但内容缺了一块。
    pub fn output_is_faithful(&self) -> bool {
        self.invalid_utf8_bytes == 0
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

        let out = crate::win_token::run_restricted(
            &token,
            command,
            &sandbox,
            &env,
            opts.timeout_ms,
            opts.memory_limit_bytes,
            opts.max_processes,
        )
        .map_err(|e| ExecError::Spawn(format!("受限执行失败: {e}")))?;

        return Ok(ExecOutcome {
            exit_code: out.exit_code,
            stdout: truncate(out.stdout),
            stderr: truncate(out.stderr),
            // 如实上报，不再硬编码 false
            timed_out: out.timed_out,
            duration_ms: started.elapsed().as_millis() as u64,
            isolation: IsolationLevel::WindowsRestrictedToken,
            // 受限令牌那条路走的是另一套管道读取，它自己也是按字节读的；
            // 这里如实带上它报的计数，不假装是 0
            invalid_utf8_bytes: out.invalid_utf8_bytes,
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
    let out_handle = thread::spawn(move || read_pipe(out_pipe.as_mut()));
    let err_handle = thread::spawn(move || read_pipe(err_pipe.as_mut()));

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

    let (stdout, out_bad) = out_handle.join().unwrap_or_default();
    let (stderr, err_bad) = err_handle.join().unwrap_or_default();

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
        invalid_utf8_bytes: out_bad + err_bad,
    })
}

/// 读完一根管道，返回（文本，被替换掉的字节数）。
///
/// ## 为什么不是 `read_to_string`
///
/// 因为 `read_to_string` **要求整段都是合法 UTF-8**。Windows 上一条
/// GBK 输出的命令（`chcp 936` 的老工具、不少 .NET CLI）会让它返回 `Err`——
/// 而原先这里是 `let _ = p.read_to_string(&mut buf)`，
/// **错误被吞掉，`buf` 只保留出错前读到的部分（可能完全是空的）**。
///
/// 后果很隐蔽：模型看到一段空输出，分不清"命令没输出"和"解码失败了"。
/// 前者该换个命令，后者该换个解编码方式——**两种情况的下一步完全不同，
/// 而它看到的是同一个东西。**
///
/// 现在按字节读、有损解码，并把替换掉的字节数报给调用方。
/// 信息不会丢：解不出来的部分变成 U+FFFD（肉眼可见的"这里有问题"），
/// 而 [`ExecOutcome::invalid_utf8_bytes`] 让这件事**可被程序检查**。
fn read_pipe(pipe: Option<&mut impl std::io::Read>) -> (String, usize) {
    let Some(p) = pipe else {
        return (String::new(), 0);
    };
    let mut raw = Vec::new();
    // 读错误仍然吞掉（管道另一头被杀等），但**解码问题不再吞**
    let _ = p.read_to_end(&mut raw);
    decode_output_lossy(&raw)
}

/// 有损解码并统计被替换掉的字节数。
///
/// **这是内核里唯一一份输出解码实现。** 正常路径（`exec`）和受限令牌路径
/// （`win_token`）都用它——两处各写一份的话，迟早有一处忘了报有损，
/// 而那正是这个函数要消灭的那类静默失效。
///
/// 逐段推进 `from_utf8` 的错误位置，而不是按 `> 0x7F` 粗算——
/// 后者会把**合法的中文**也算成坏的（中文每个字节都 > 0x7F）。
pub(crate) fn decode_output_lossy(buf: &[u8]) -> (String, usize) {
    if std::str::from_utf8(buf).is_ok() {
        return (String::from_utf8_lossy(buf).into_owned(), 0);
    }
    let mut bad = 0usize;
    let mut rest = buf;
    while !rest.is_empty() {
        match std::str::from_utf8(rest) {
            Ok(_) => break,
            Err(e) => {
                let valid = e.valid_up_to();
                let skip = e.error_len().unwrap_or(rest.len() - valid);
                bad += skip;
                let next = valid + skip;
                if next >= rest.len() {
                    break;
                }
                rest = &rest[next..];
            }
        }
    }
    (String::from_utf8_lossy(buf).into_owned(), bad)
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

    // ---- 输出解码：静默失效的回归测试 ----

    #[test]
    fn valid_utf8_output_is_not_flagged() {
        // 正常情况不该被误报——这条是防"修一个 bug 引入另一个"的
        let (s, bad) = decode_probe("hello 世界".as_bytes());
        assert_eq!(s, "hello 世界");
        assert_eq!(bad, 0, "合法 UTF-8 不该报有损");
    }

    #[test]
    fn gbk_output_is_decoded_lossily_and_flagged() {
        // **这是那个静默失效的回归测试。**
        //
        // 0xB2 0xE2 0xCA 0xD4 是 GBK 的"测试"，不是合法 UTF-8。
        // 原先 `let _ = p.read_to_string(&mut buf)` 会让这段变成**空输出**，
        // 而错误被吞掉——模型分不清"命令没输出"和"解码失败了"。
        //
        // 现在：内容以 U+FFFD 的形式可见（肉眼能看出这里有问题），
        // 且**字节数可被程序检查**。
        let gbk = [0xB2u8, 0xE2, 0xCA, 0xD4];
        let (s, bad) = decode_probe(&gbk);
        assert!(!s.is_empty(), "解不出来也不能变成空输出——那就回到了老 bug");
        assert!(s.contains('\u{FFFD}'), "坏字节要变成可见的替换字符");
        assert_eq!(bad, 4, "四个坏字节都要被数出来");
    }

    #[test]
    fn loss_count_ignores_valid_multibyte_characters() {
        // **不能按 `> 0x7F` 粗算**：中文每个字节都 > 0x7F，
        // 那样数出来的"坏字节"会把一段完全正常的中文全算进去。
        let mixed = "中文".as_bytes().to_vec();
        let mut with_bad = mixed.clone();
        with_bad.extend_from_slice(&[0xFFu8, 0xFE]);
        let (s, bad) = decode_probe(&with_bad);
        assert!(s.starts_with("中文"));
        assert_eq!(bad, 2, "只该数真正非法的两个字节，不该把中文算进去");
    }

    #[test]
    fn a_clean_command_reports_faithful_output() {
        // `output_is_faithful` 是给调用方判断"这段输出能不能直接交给模型"用的
        let out = run_command(&echo_cmd(), &fast_opts());
        let out = out.expect("echo 该能跑");
        assert!(out.output_is_faithful(), "普通命令的输出该是完整可信的");
        assert_eq!(out.invalid_utf8_bytes, 0);
    }

    #[cfg(windows)]
    #[test]
    fn a_gbk_emitting_command_keeps_its_output_and_flags_it() {
        // **真机验证。** 用 PowerShell 直接吐 GBK 字节，
        // 走完整条执行链路，确认输出不再被静默丢掉。
        let cmd = vec![
            "powershell".to_string(),
            "-NoProfile".to_string(),
            "-Command".to_string(),
            "[Console]::OpenStandardOutput().Write([byte[]](0xB2,0xE2,0xCA,0xD4),0,4)".to_string(),
        ];
        let out = run_command(&cmd, &fast_opts()).expect("该能跑");
        assert_eq!(out.exit_code, 0);
        assert!(
            !out.stdout.is_empty(),
            "**这就是老 bug：GBK 输出会变成空字符串**"
        );
        assert!(out.invalid_utf8_bytes > 0, "该如实报出有损解码");
        assert!(!out.output_is_faithful());
    }

    #[cfg(windows)]
    fn echo_cmd() -> Vec<String> {
        vec!["cmd".to_string(), "/c".to_string(), "echo hi".to_string()]
    }

    #[cfg(not(windows))]
    fn echo_cmd() -> Vec<String> {
        vec!["echo".to_string(), "hi".to_string()]
    }

    fn fast_opts() -> ExecOptions {
        ExecOptions {
            timeout_ms: 15_000,
            ..Default::default()
        }
    }

    /// 直接测**真的那个**解码函数，不派生进程、不复制逻辑。
    fn decode_probe(bytes: &[u8]) -> (String, usize) {
        decode_output_lossy(bytes)
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
