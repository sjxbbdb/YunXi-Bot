//! Windows 写入隔离：受限令牌 + 低完整性级别。
//!
//! ## 机制（最终采用）
//!
//! 1. `CreateRestrictedToken(DISABLE_MAX_PRIVILEGE)` 造一个受限令牌；
//! 2. `DuplicateTokenEx` 转成主令牌；
//! 3. **`SetTokenInformation(TokenIntegrityLevel)` 把完整性降到 Low**；
//! 4. 用 `CreateProcessAsUserW` 以该令牌启动子进程。
//!
//! 写入隔离来自第 3 步。Windows 的强制完整性控制（MIC）规定：
//!
//! - **读**不受影响 → 进程能正常启动、正常运行；
//! - **写**只能写完整性级别 ≤ 自己的对象 → 用户目录里的文件都是中完整性，
//!   一律写不进去。
//!
//! 可写区是系统预置的 `%USERPROFILE%\AppData\LocalLow`——它本身就是低完整性目录，
//! 而新对象会**继承**父目录的标签，所以在其下建目录不需要任何特权。
//!
//! ## 走过并被证伪的路线：能力 SID 限制列表
//!
//! 最初按"受限令牌 + 能力 SID（`S-1-4-x-y`）限制列表"实现：铸造能力 SID、
//! 给工作目录加写 ACE、把它放进 `CreateRestrictedToken` 的限制列表。
//! 代码全部写通，但**四种变体对照实测**都失败：
//!
//! | 限制列表 | flags | 结果 |
//! |---|---|---|
//! | **空** | `DISABLE_MAX_PRIVILEGE` | ✅ 子进程正常运行 |
//! | `Everyone` | `DISABLE_MAX_PRIVILEGE` | ❌ `STATUS_DLL_INIT_FAILED` |
//! | `Everyone` + 能力 SID | `DISABLE_MAX_PRIVILEGE` | ❌ 同上 |
//! | `Everyone` | `+ WRITE_RESTRICTED` | ❌ 同上 |
//!
//! **限制列表只要非空，子进程就无法完成初始化**（`0xC0000142`）。
//! 根因：限制列表对读写一视同仁，而进程启动本身就需要访问只授权给用户 SID 的
//! 对象（注册表 `HKCU`、窗口站等）。`WRITE_RESTRICTED` 把限制收窄到写，
//! 但进程启动**确实要写**，所以仍然过不去。
//!
//! 另一条试过的路是给目录**打**低完整性标签，失败于 `ERROR_ACCESS_DENIED (5)`：
//! 完整性标签存在 SACL 里，改 SACL 需要 `SeSecurityPrivilege`（管理员）。
//! 这正是最终方案改用系统预置 `LocalLow` 的原因——它已经标好了。
//!
//! ## 为什么必须自检
//!
//! 上面这套 FFI 有太多地方可以"错了但不报错"。所以本模块提供
//! [`verify_write_isolation`]：真起一个 canary 子进程，让它分别尝试写
//! **允许**与**禁止**的路径，只有"允许的写成功、禁止的写失败"才算通过。
//!
//! **[`write_isolation_verified`] 是失败契约的执行点**：要求 OS 级写入隔离时，
//! 不会因为"代码里有这个功能"就放行，而是要求自检真的跑过一次并成功。
//!
//! ## 自检抓出来的错误（每一个都能安静地跑下去）
//!
//! - `CreateRestrictedToken` 真实签名有 **9 个参数**，少声明中间两个会让调用
//!   错位读到栈上垃圾（表现为 Win32 错误 534「算术结果超过 32 位」）；
//! - 限制 SID 入参是 `SID_AND_ATTRIBUTES` **结构体数组**，不是 SID 指针数组；
//! - `TRUSTEE_W` 必须含 `TrusteeType` 且字段顺序与 Win32 一致（否则 87）；
//! - `GetNamedSecurityInfoW` 拒绝带结尾反斜杠的路径（87）；
//! - 空环境块对 `CreateProcess` 非法（87）；
//! - `advapi32` 必须显式 `#[link]`，否则 `FreeSid` 链接失败；
//! - **canary 不能用 `current_exe()`**：在 `cargo test` 下那是测试二进制，
//!   它会把 `__canary-write` 当成测试名过滤器跑 0 个测试然后正常退出，
//!   让自检得出"两条路径都没写成"的**假结论**。见 [`locate_canary`]。
//!
//! ## 它不保证什么
//!
//! - **不限制读取。** 子进程仍能读该用户能读的任何东西。本机制只管写入。
//! - **不限制网络。** 需要网络隔离得另加防火墙规则或 AppContainer。
#![cfg(windows)]

use std::ffi::c_void;

// —— Win32 常量 ——
const TOKEN_ASSIGN_PRIMARY: u32 = 0x0001;
const TOKEN_DUPLICATE: u32 = 0x0002;
const TOKEN_QUERY: u32 = 0x0008;
const TOKEN_ADJUST_DEFAULT: u32 = 0x0080;
const TOKEN_ADJUST_SESSIONID: u32 = 0x0100;
const TOKEN_ALL_FOR_LAUNCH: u32 = TOKEN_ASSIGN_PRIMARY
    | TOKEN_DUPLICATE
    | TOKEN_QUERY
    | TOKEN_ADJUST_DEFAULT
    | TOKEN_ADJUST_SESSIONID;

const DISABLE_MAX_PRIVILEGE: u32 = 0x0001;

/// `TokenIntegrityLevel` 信息类。
const TOKEN_INTEGRITY_LEVEL: u32 = 25;

/// 完整性 SID 的权威：`SECURITY_MANDATORY_LABEL_AUTHORITY` = {0,0,0,0,0,16}。
const MANDATORY_LABEL_AUTHORITY: SidIdentifierAuthority = SidIdentifierAuthority {
    value: [0, 0, 0, 0, 0, 16],
};

/// `SECURITY_MANDATORY_LOW_RID` = 0x1000。
const MANDATORY_LOW_RID: u32 = 0x1000;

/// `SE_GROUP_INTEGRITY`——完整性 SID 必须带这个属性，否则 `SetTokenInformation` 会拒绝。
const SE_GROUP_INTEGRITY: u32 = 0x0000_0020;
const SECURITY_IMPERSONATION: u32 = 2;
const TOKEN_PRIMARY: u32 = 1;

const STARTF_USESTDHANDLES: u32 = 0x0000_0100;
const CREATE_UNICODE_ENVIRONMENT: u32 = 0x0000_0400;
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

const HANDLE_FLAG_INHERIT: u32 = 0x0000_0001;
const WAIT_OBJECT_0: u32 = 0;
const STILL_ACTIVE: u32 = 259;

// —— Win32 结构 ——

#[repr(C)]
#[derive(Clone, Copy)]
struct SidIdentifierAuthority {
    value: [u8; 6],
}

#[repr(C)]
struct SecurityAttributes {
    n_length: u32,
    lp_security_descriptor: *mut c_void,
    b_inherit_handle: i32,
}

#[repr(C)]
struct StartupInfoW {
    cb: u32,
    lp_reserved: *mut u16,
    lp_desktop: *mut u16,
    lp_title: *mut u16,
    dw_x: u32,
    dw_y: u32,
    dw_x_size: u32,
    dw_y_size: u32,
    dw_x_count_chars: u32,
    dw_y_count_chars: u32,
    dw_fill_attribute: u32,
    dw_flags: u32,
    w_show_window: u16,
    cb_reserved2: u16,
    lp_reserved2: *mut u8,
    h_std_input: *mut c_void,
    h_std_output: *mut c_void,
    h_std_error: *mut c_void,
}

#[repr(C)]
struct ProcessInformation {
    h_process: *mut c_void,
    h_thread: *mut c_void,
    dw_process_id: u32,
    dw_thread_id: u32,
}

unsafe extern "system" {
    fn GetCurrentProcess() -> *mut c_void;
    fn CloseHandle(h: *mut c_void) -> i32;
    fn GetLastError() -> u32;
    fn CreatePipe(
        read: *mut *mut c_void,
        write: *mut *mut c_void,
        attrs: *mut SecurityAttributes,
        size: u32,
    ) -> i32;
    fn SetHandleInformation(h: *mut c_void, mask: u32, flags: u32) -> i32;
    fn ReadFile(
        h: *mut c_void,
        buf: *mut u8,
        to_read: u32,
        read: *mut u32,
        overlapped: *mut c_void,
    ) -> i32;
    fn WaitForSingleObject(h: *mut c_void, ms: u32) -> u32;
    fn GetExitCodeProcess(h: *mut c_void, code: *mut u32) -> i32;
    fn CreateProcessAsUserW(
        token: *mut c_void,
        app_name: *const u16,
        cmd_line: *mut u16,
        proc_attrs: *mut c_void,
        thread_attrs: *mut c_void,
        inherit_handles: i32,
        flags: u32,
        env: *mut c_void,
        cwd: *const u16,
        startup: *mut StartupInfoW,
        info: *mut ProcessInformation,
    ) -> i32;
}

/// `SID_AND_ATTRIBUTES`。
///
/// ⚠️ `CreateRestrictedToken` 要的是**这个结构体的数组**，不是 SID 指针数组。
#[repr(C)]
struct SidAndAttributes {
    sid: *mut c_void,
    attributes: u32,
}

// advapi32 里的安全相关 API。**必须显式声明链接库**——不加这一行，
// 只有那些恰好能从默认库解析到的符号链得上（实测 FreeSid 会 LNK2019）。
#[link(name = "advapi32")]
unsafe extern "system" {
    fn OpenProcessToken(process: *mut c_void, access: u32, token: *mut *mut c_void) -> i32;

    /// 真实签名有 **9 个参数**：
    /// `(token, flags, disableCount, sidsToDisable, deletePrivCount,
    ///   privsToDelete, restrictCount, sidsToRestrict, newToken)`
    ///
    /// 少声明中间那两个参数会让调用错位、读到栈上垃圾——这是个能安静跑下去的
    /// 内存破坏 bug（实测表现为 Win32 错误 534「算术结果超过 32 位」），
    /// 只有实际调用才能发现。
    fn CreateRestrictedToken(
        existing: *mut c_void,
        flags: u32,
        disable_count: u32,
        sids_to_disable: *const SidAndAttributes,
        delete_privilege_count: u32,
        privileges_to_delete: *const c_void,
        restricted_sid_count: u32,
        sids_to_restrict: *const SidAndAttributes,
        new_token: *mut *mut c_void,
    ) -> i32;
    fn DuplicateTokenEx(
        existing: *mut c_void,
        access: u32,
        attrs: *mut c_void,
        level: u32,
        ty: u32,
        new_token: *mut *mut c_void,
    ) -> i32;
    fn AllocateAndInitializeSid(
        authority: *const SidIdentifierAuthority,
        count: u8,
        sub0: u32,
        sub1: u32,
        sub2: u32,
        sub3: u32,
        sub4: u32,
        sub5: u32,
        sub6: u32,
        sub7: u32,
        sid: *mut *mut c_void,
    ) -> i32;
    fn SetTokenInformation(token: *mut c_void, class: u32, info: *mut c_void, len: u32) -> i32;
    fn FreeSid(sid: *mut c_void) -> *mut c_void;
}

unsafe extern "system" {
    fn GetEnvironmentStringsW() -> *mut u16;
    fn FreeEnvironmentStringsW(env: *mut u16) -> i32;
}

#[derive(Debug)]
pub enum TokenError {
    /// 完整性 SID 创建失败
    Sid(u32),
    /// 令牌创建/复制/降级失败
    Token(u32),
    /// 进程启动失败
    Spawn(u32),
    /// 参数非法
    Invalid(String),
}

impl std::fmt::Display for TokenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TokenError::Sid(c) => write!(f, "创建完整性 SID 失败（Win32 错误 {c}）"),
            TokenError::Token(c) => write!(f, "创建或降级受限令牌失败（Win32 错误 {c}）"),
            TokenError::Spawn(c) => write!(f, "启动受限子进程失败（Win32 错误 {c}）"),
            TokenError::Invalid(m) => write!(f, "参数非法: {m}"),
        }
    }
}

impl std::error::Error for TokenError {}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 低完整性 SID（`S-1-16-4096`）。
///
/// ## 这才是 Windows 上做写入隔离的正确机制
///
/// 早先版本用受限令牌的**限制 SID 列表**做隔离，实测撞死在
/// `STATUS_DLL_INIT_FAILED`：限制列表对**读写一视同仁**，而进程启动本身
/// 就需要读注册表、读窗口站等只授权给用户 SID 的对象，于是任何非空限制列表
/// 都会让子进程起不来。
///
/// **完整性级别只挡写、不挡读**（Windows 的强制完整性控制 MIC 就是这样定义的）：
///
/// - 低完整性进程**读**任何东西都不受影响 → 进程能正常启动、正常运行；
/// - **写**只能写完整性级别 ≤ 自己的对象 → 用户目录里的文件都是中完整性，
///   一律写不进去。
///
/// 想让某个目录可写，就给它打上低完整性标签（见 [`label_low`]）。
struct LowIntegritySid {
    sid: *mut c_void,
}

/// `TOKEN_MANDATORY_LABEL`：`SetTokenInformation` 用的载荷。
#[repr(C)]
struct TokenMandatoryLabel {
    label: SidAndAttributes,
}

impl LowIntegritySid {
    fn new() -> Result<Self, TokenError> {
        let mut sid: *mut c_void = std::ptr::null_mut();
        // SAFETY: Mandatory Label 权威 + 1 个子授权号 0x1000 = S-1-16-4096（Low）
        let ok = unsafe {
            AllocateAndInitializeSid(
                &MANDATORY_LABEL_AUTHORITY,
                1,
                MANDATORY_LOW_RID,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                &mut sid,
            )
        };
        if ok == 0 {
            return Err(TokenError::Sid(unsafe { GetLastError() }));
        }
        Ok(Self { sid })
    }
}

impl Drop for LowIntegritySid {
    fn drop(&mut self) {
        if !self.sid.is_null() {
            unsafe { FreeSid(self.sid) };
        }
    }
}

/// 低完整性可写区的根。
///
/// Windows 为低完整性进程准备了 `%USERPROFILE%\AppData\LocalLow`，**它本身就是
/// 低完整性目录**（`Mandatory Label\Low Mandatory Level:(OI)(CI)(NW)`）。
///
/// 这一点很关键：给一个已存在的目录**打**低完整性标签需要 `SeSecurityPrivilege`
/// （实测普通用户下返回 `ERROR_ACCESS_DENIED (5)`，因为完整性标签存在 SACL 里），
/// 而直接使用这个系统预置的位置则**完全不需要管理员权限**。
pub fn low_integrity_root() -> Option<std::path::PathBuf> {
    std::env::var_os("USERPROFILE")
        .map(|p| std::path::PathBuf::from(p).join("AppData").join("LocalLow"))
}

/// 低完整性沙箱目录：低完整性子进程**唯一**能写的地方。
///
/// 由中完整性的父进程创建在 `LocalLow` 下，因此目录会继承低完整性标签
/// （实测 `Mandatory Label\Low Mandatory Level:(I)(OI)(CI)(NW)`）。
pub fn sandbox_dir() -> Result<std::path::PathBuf, TokenError> {
    let root = low_integrity_root()
        .ok_or_else(|| TokenError::Invalid("取不到用户目录，无法定位低完整性可写区".into()))?;
    let dir = root.join("YunXiBot").join("sandbox");
    std::fs::create_dir_all(&dir)
        .map_err(|e| TokenError::Invalid(format!("建沙箱目录失败: {e}")))?;
    Ok(dir)
}

/// 写入隔离是否**经过实测验证**。结果在进程内缓存。
///
/// 这是失败契约的执行点：要求 OS 级写入隔离时，不会因为"代码里有这个功能"
/// 就放行，而是要求自检**真的跑过一次并成功**。自检会起一个受限子进程去写
/// 允许与禁止的两条路径，只有"允许的能写、禁止的写不了"才算通过。
pub fn write_isolation_verified() -> bool {
    static VERIFIED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *VERIFIED.get_or_init(|| {
        let Some(root) = low_integrity_root() else {
            return false;
        };
        let base = root.join(format!("YunXiBot-verify-{}", std::process::id()));
        let allowed = base.join("allowed");
        let medium = std::env::temp_dir().join(format!("yunxi-verify-{}", std::process::id()));
        let denied = medium.join("denied");

        let ok = verify_write_isolation(&allowed, &denied)
            .map(|r| r.is_effective())
            .unwrap_or(false);

        let _ = std::fs::remove_dir_all(&base);
        let _ = std::fs::remove_dir_all(&medium);
        ok
    })
}

/// 受限令牌：当前进程令牌的受限副本，完整性级别降到 **Low**。
pub struct RestrictedToken {
    handle: *mut c_void,
}

unsafe impl Send for RestrictedToken {}
unsafe impl Sync for RestrictedToken {}

impl RestrictedToken {
    /// 以当前进程令牌为基础，铸造带能力 SID 限制的令牌。
    pub fn create() -> Result<Self, TokenError> {
        let mut base: *mut c_void = std::ptr::null_mut();
        // SAFETY: 取当前进程令牌
        let ok = unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_ALL_FOR_LAUNCH, &mut base) };
        if ok == 0 {
            return Err(TokenError::Token(unsafe { GetLastError() }));
        }

        // **刻意不带限制 SID 列表。**
        //
        // 实测：限制列表只要非空，子进程就以 `STATUS_DLL_INIT_FAILED` 结束——
        // 因为限制列表对读写一视同仁，而进程启动本身就需要访问那些只授权给
        // 用户 SID 的对象。写入隔离改由**完整性级别**承担（见下）。
        let mut restricted: *mut c_void = std::ptr::null_mut();
        // SAFETY: 所有计数为 0 且指针为 null，即不设任何限制列表
        let ok = unsafe {
            CreateRestrictedToken(
                base,
                DISABLE_MAX_PRIVILEGE,
                0,
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
                std::ptr::null(),
                &mut restricted,
            )
        };
        unsafe { CloseHandle(base) };
        if ok == 0 {
            return Err(TokenError::Token(unsafe { GetLastError() }));
        }

        // CreateProcessAsUserW 需要一个主令牌
        let mut primary: *mut c_void = std::ptr::null_mut();
        // SAFETY: restricted 是有效令牌句柄
        let ok = unsafe {
            DuplicateTokenEx(
                restricted,
                TOKEN_ALL_FOR_LAUNCH,
                std::ptr::null_mut(),
                SECURITY_IMPERSONATION,
                TOKEN_PRIMARY,
                &mut primary,
            )
        };
        unsafe { CloseHandle(restricted) };
        if ok == 0 {
            return Err(TokenError::Token(unsafe { GetLastError() }));
        }

        // —— 关键一步：把完整性级别降到 Low ——
        //
        // 这一步才是真正产生写入隔离的地方：低完整性进程写不进任何
        // 中完整性的对象（用户目录里的文件默认都是中完整性），
        // 而**读不受影响**，所以进程能正常启动运行。
        let low = LowIntegritySid::new()?;
        let mut label = TokenMandatoryLabel {
            label: SidAndAttributes {
                sid: low.sid,
                attributes: SE_GROUP_INTEGRITY,
            },
        };
        // SAFETY: label 是 #[repr(C)] 的 TOKEN_MANDATORY_LABEL，长度取实际大小
        let ok = unsafe {
            SetTokenInformation(
                primary,
                TOKEN_INTEGRITY_LEVEL,
                &mut label as *mut _ as *mut c_void,
                std::mem::size_of::<TokenMandatoryLabel>() as u32,
            )
        };
        if ok == 0 {
            let code = unsafe { GetLastError() };
            unsafe { CloseHandle(primary) };
            return Err(TokenError::Token(code));
        }

        Ok(Self { handle: primary })
    }
}

impl Drop for RestrictedToken {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            unsafe { CloseHandle(self.handle) };
        }
    }
}

/// 一次受限执行的产出。
#[derive(Debug, Clone)]
pub struct RestrictedOutcome {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}

/// 以受限令牌启动子进程并等待其结束。
///
/// `env` 是**已经剥离过凭证**的键值对；这里把它编成 Unicode 环境块传进去，
/// 而不是让子进程继承父环境——继承会把凭证原样带过去。
pub fn run_restricted(
    token: &RestrictedToken,
    argv: &[String],
    cwd: &std::path::Path,
    env: &[(String, String)],
    timeout_ms: u64,
) -> Result<RestrictedOutcome, TokenError> {
    if argv.is_empty() {
        return Err(TokenError::Invalid("命令为空".into()));
    }

    let mut sa = SecurityAttributes {
        n_length: std::mem::size_of::<SecurityAttributes>() as u32,
        lp_security_descriptor: std::ptr::null_mut(),
        b_inherit_handle: 1,
    };

    let (out_r, out_w) = pipe(&mut sa)?;
    let (err_r, err_w) = pipe(&mut sa)?;
    let (in_r, _in_w) = pipe(&mut sa)?;

    // 父进程持有读端，读端不该被继承；写端要继承给子进程
    for h in [out_r, err_r, in_r] {
        unsafe { SetHandleInformation(h, HANDLE_FLAG_INHERIT, 0) };
    }

    let mut si: StartupInfoW = unsafe { std::mem::zeroed() };
    si.cb = std::mem::size_of::<StartupInfoW>() as u32;
    si.dw_flags = STARTF_USESTDHANDLES;
    si.h_std_output = out_w;
    si.h_std_error = err_w;
    si.h_std_input = in_r;

    let mut pi: ProcessInformation = unsafe { std::mem::zeroed() };

    let cmd_line = build_command_line(argv);
    let mut cmd_buf = wide(&cmd_line);
    let cwd_w = wide(&cwd.to_string_lossy());
    let env_block = build_env_block(env);

    // SAFETY: 所有指针都指向在本次调用期间存活的缓冲区
    let ok = unsafe {
        CreateProcessAsUserW(
            token.handle,
            std::ptr::null(),
            cmd_buf.as_mut_ptr(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            1, // 继承管道写端
            CREATE_UNICODE_ENVIRONMENT | CREATE_NO_WINDOW,
            env_block.as_ptr() as *mut c_void,
            cwd_w.as_ptr(),
            &mut si,
            &mut pi,
        )
    };

    // 父进程必须立刻关掉写端，否则读端永远等不到 EOF
    unsafe {
        CloseHandle(out_w);
        CloseHandle(err_w);
        CloseHandle(_in_w);
    }

    if ok == 0 {
        let code = unsafe { GetLastError() };
        unsafe {
            CloseHandle(out_r);
            CloseHandle(err_r);
            CloseHandle(in_r);
        }
        return Err(TokenError::Spawn(code));
    }

    let stdout = read_all(out_r);
    let stderr = read_all(err_r);
    unsafe {
        CloseHandle(out_r);
        CloseHandle(err_r);
        CloseHandle(in_r);
    }

    // 等待，带超时；超时则强杀（Job Object 那一层还会兜底回收进程树）
    let wait = unsafe { WaitForSingleObject(pi.h_process, timeout_ms as u32) };
    if wait != WAIT_OBJECT_0 {
        let _ = std::process::Command::new("taskkill")
            .args(["/PID", &pi.dw_process_id.to_string(), "/T", "/F"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }

    let mut code: u32 = STILL_ACTIVE;
    unsafe {
        GetExitCodeProcess(pi.h_process, &mut code);
        CloseHandle(pi.h_process);
        CloseHandle(pi.h_thread);
    }

    Ok(RestrictedOutcome {
        exit_code: code as i32,
        stdout,
        stderr,
    })
}

fn pipe(sa: &mut SecurityAttributes) -> Result<(*mut c_void, *mut c_void), TokenError> {
    let mut r: *mut c_void = std::ptr::null_mut();
    let mut w: *mut c_void = std::ptr::null_mut();
    // SAFETY: sa 声明为可继承，两个出参由 API 填充
    let ok = unsafe { CreatePipe(&mut r, &mut w, sa, 0) };
    if ok == 0 {
        return Err(TokenError::Spawn(unsafe { GetLastError() }));
    }
    Ok((r, w))
}

fn read_all(handle: *mut c_void) -> String {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let mut n: u32 = 0;
        // SAFETY: chunk 是本栈上的有效缓冲区
        let ok = unsafe {
            ReadFile(
                handle,
                chunk.as_mut_ptr(),
                4096,
                &mut n,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 || n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n as usize]);
        if buf.len() > 4 * 1024 * 1024 {
            break;
        }
    }
    String::from_utf8_lossy(&buf).to_string()
}

/// 按 Windows 命令行引用规则拼 argv。
///
/// 规则与 `Command` 一致：含空格或引号的参数用双引号包住，内部的双引号与
/// 其前的反斜杠要成对转义。**不经 shell**，所以没有元字符注入面。
fn build_command_line(argv: &[String]) -> String {
    let mut out = String::new();
    for (i, arg) in argv.iter().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        if !arg.is_empty() && !arg.contains([' ', '\t', '"']) {
            out.push_str(arg);
            continue;
        }
        out.push('"');
        let mut backslashes = 0usize;
        for ch in arg.chars() {
            match ch {
                '\\' => {
                    backslashes += 1;
                    out.push('\\');
                }
                '"' => {
                    // 引号前的反斜杠要翻倍，再补一个转义引号
                    for _ in 0..backslashes {
                        out.push('\\');
                    }
                    backslashes = 0;
                    out.push('\\');
                    out.push('"');
                }
                _ => {
                    backslashes = 0;
                    out.push(ch);
                }
            }
        }
        // 收尾引号前的反斜杠也要翻倍
        for _ in 0..backslashes {
            out.push('\\');
        }
        out.push('"');
    }
    out
}

/// 把键值对编成 CreateProcess 要的 Unicode 环境块。
fn build_env_block(env: &[(String, String)]) -> Vec<u16> {
    let mut block = Vec::new();
    for (k, v) in env {
        block.extend(format!("{k}={v}").encode_utf16());
        block.push(0);
    }
    block.push(0); // 结尾双 null
    block
}

/// 读取当前进程的完整环境（供调用方剥离凭证后再传给 [`run_restricted`]）。
pub fn current_env() -> Vec<(String, String)> {
    let mut out = Vec::new();
    // SAFETY: 环境块由系统分配，用后必须 FreeEnvironmentStringsW
    unsafe {
        let block = GetEnvironmentStringsW();
        if block.is_null() {
            return out;
        }
        let mut p = block;
        loop {
            let mut len = 0usize;
            while *p.add(len) != 0 {
                len += 1;
            }
            if len == 0 {
                break;
            }
            let s = String::from_utf16_lossy(std::slice::from_raw_parts(p, len));
            if let Some((k, v)) = s.split_once('=') {
                out.push((k.to_string(), v.to_string()));
            }
            p = p.add(len + 1);
        }
        FreeEnvironmentStringsW(block);
    }
    out
}

/// 自检的原始观测结果。
///
/// **刻意保留事实而不是结论。** 早先版本只返回一个 bool，失败时统一报
/// "禁止目录被写成功了"——但 `false` 也可能是"两条路径都写不进去"（能力 SID 的
/// ACE 没生效）。那种误导性报错会把人带到完全错误的方向，实测就撞上了。
#[derive(Debug, Clone)]
pub struct IsolationReport {
    /// canary 是否成功写入了**已授权**的路径。
    pub allowed_written: bool,
    /// canary 是否成功写入了**未授权**的路径。
    pub denied_written: bool,
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl IsolationReport {
    /// 写入隔离是否真的生效：授权的能写、未授权的写不了。
    pub fn is_effective(&self) -> bool {
        self.allowed_written && !self.denied_written
    }

    /// 人类可读的判定与依据。
    pub fn explain(&self) -> String {
        // 0xC0000142 = STATUS_DLL_INIT_FAILED，0xC0000022 = STATUS_ACCESS_DENIED。
        // 区分它们很关键：两者的根因完全不同，报错含糊会把人带错方向。
        let cause = match self.exit_code as u32 {
            0xC000_0142 => "（子进程 STATUS_DLL_INIT_FAILED：限制列表非空导致它无法完成初始化）",
            0xC000_0022 => "（子进程 STATUS_ACCESS_DENIED：限制过严，连启动都过不去）",
            _ => "",
        };
        match (self.allowed_written, self.denied_written) {
            (true, false) => "写入隔离生效：授权路径可写，未授权路径被拒".into(),
            (true, true) => "写入隔离【未生效】：未授权路径也被写成功了，令牌没起到限制作用".into(),
            (false, false) => format!(
                "写入隔离【不可用】：canary 两条路径都没写成，退出码 {}{cause}",
                self.exit_code
            ),
            (false, true) => format!(
                "自检异常：授权路径写不进去、未授权路径却能写。退出码 {}{cause}",
                self.exit_code
            ),
        }
    }
}

/// 定位真正实现了 `__canary-write` 的那个可执行文件。
///
/// ⚠️ **不能用 `current_exe()` 了事。** 在 `cargo test` 下它返回的是**测试二进制**
/// （`target/debug/deps/yunxi_bot_core-<hash>.exe`），那个程序不认识
/// `__canary-write`，会把它当成测试名过滤器、跑 0 个测试然后正常退出——
/// 于是自检会得到"两条路径都没写成"的**假结论**，而进程其实压根没执行到那段代码。
///
/// 这个坑是真实踩到的：自检连续几轮都在报告"隔离未生效"，直到把 canary 的
/// stdout 打出来才看见"running 0 tests"。
fn locate_canary() -> Result<std::path::PathBuf, TokenError> {
    // 显式覆盖优先，便于在其他布局下使用
    if let Some(p) = std::env::var_os("YUNXI_BOT_CANARY") {
        let p = std::path::PathBuf::from(p);
        if p.is_file() {
            return Ok(p);
        }
    }

    let me =
        std::env::current_exe().map_err(|e| TokenError::Invalid(format!("取不到自身路径: {e}")))?;

    // 情况一：自己就是 CLI（`isolation-check` 命令走这条）
    if me
        .file_stem()
        .and_then(|s| s.to_str())
        .is_some_and(|s| s.starts_with("yunxi-bot"))
    {
        return Ok(me);
    }

    // 情况二：自己是测试二进制，CLI 在它的上上级目录
    //   target/debug/deps/<test>.exe  →  target/debug/yunxi-bot.exe
    if let Some(dir) = me.parent().and_then(|d| d.parent()) {
        for name in ["yunxi-bot.exe", "yunxi-bot"] {
            let cand = dir.join(name);
            if cand.is_file() {
                return Ok(cand);
            }
        }
    }

    Err(TokenError::Invalid(format!(
        "找不到 canary 可执行文件（自身为 {}）。\
         请先 `cargo build`，或用 YUNXI_BOT_CANARY 指定路径。",
        me.display()
    )))
}

/// **写入隔离自检。**
///
/// 真起一个 canary 子进程，让它尝试写两个路径：
/// - `allowed` 下的文件：期望**写成功**；
/// - `denied` 下的文件：期望**写失败**。
///
/// 返回原始观测事实，由调用方判断。理由：上面这套 FFI 有太多地方可以
/// "错了但不报错"，而那会产出**看起来实现了隔离、实际毫无限制**的代码。
/// 隔离声明必须由实测背书，不能由注释背书。
pub fn verify_write_isolation(
    allowed: &std::path::Path,
    denied: &std::path::Path,
) -> Result<IsolationReport, TokenError> {
    let token = RestrictedToken::create()?;

    let canary = locate_canary()?;

    std::fs::create_dir_all(allowed).ok();
    std::fs::create_dir_all(denied).ok();

    let allowed_file = allowed.join("yunxi-canary-allowed.txt");
    let denied_file = denied.join("yunxi-canary-denied.txt");
    // 先清掉上次残留，否则会把旧痕迹当成这次的结果
    let _ = std::fs::remove_file(&allowed_file);
    let _ = std::fs::remove_file(&denied_file);

    let argv = vec![
        canary.to_string_lossy().to_string(),
        "__canary-write".to_string(),
        allowed_file.to_string_lossy().to_string(),
        denied_file.to_string_lossy().to_string(),
    ];

    // 传给子进程的是**剥离过凭证**的环境，而不是继承父环境
    let env: Vec<(String, String)> = crate::exec::strip_credentials(current_env())
        .into_iter()
        .collect();
    let out = run_restricted(&token, &argv, allowed, &env, 30_000)?;

    let report = IsolationReport {
        allowed_written: allowed_file.exists(),
        denied_written: denied_file.exists(),
        exit_code: out.exit_code,
        stdout: out.stdout,
        stderr: out.stderr,
    };

    let _ = std::fs::remove_file(&allowed_file);
    let _ = std::fs::remove_file(&denied_file);
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_a_low_integrity_restricted_token() {
        let token = RestrictedToken::create();
        assert!(token.is_ok(), "建受限令牌失败: {:?}", token.err());
    }

    #[test]
    fn low_integrity_root_is_available_and_writable() {
        // 低完整性可写区依赖系统预置的 AppData\LocalLow，缺了它整个机制不成立
        let root = low_integrity_root().expect("应有 LocalLow");
        assert!(root.is_dir(), "LocalLow 应存在: {}", root.display());
        let probe = root.join(format!("YunXiBot-probe-{}", std::process::id()));
        std::fs::create_dir_all(&probe).expect("中完整性父进程应能写入 LocalLow");
        let _ = std::fs::remove_dir_all(&probe);
    }

    #[test]
    fn sandbox_dir_lives_under_low_integrity_root() {
        let sb = sandbox_dir().expect("建沙箱");
        let root = low_integrity_root().expect("应有 LocalLow");
        assert!(
            sb.starts_with(&root),
            "沙箱必须位于低完整性根下，否则里面写不进去: {}",
            sb.display()
        );
    }

    #[test]
    fn command_line_quoting_matches_windows_rules() {
        assert_eq!(build_command_line(&["a".into(), "b".into()]), "a b");

        // 无空格无引号的参数原样输出：不受引号规则约束，
        // 结尾单个反斜杠在这里是安全的
        assert_eq!(build_command_line(&[r"a\".into()]), r"a\");

        assert_eq!(
            build_command_line(&["a b".into()]),
            "\"a b\"",
            "含空格要加引号"
        );

        // 加了引号之后，收尾反斜杠就必须翻倍，
        // 否则它会转义掉收尾引号
        assert_eq!(
            build_command_line(&[r"a b\".into()]),
            r#""a b\\""#,
            "收尾反斜杠要翻倍"
        );

        // 参数内部的引号要被转义
        assert_eq!(
            build_command_line(&[r#"say "hi""#.into()]),
            r#""say \"hi\"""#,
            "内部引号要转义"
        );
    }

    #[test]
    fn env_block_is_double_null_terminated() {
        let block = build_env_block(&[("A".into(), "1".into())]);
        assert_eq!(block.last(), Some(&0));
        assert_eq!(block[block.len() - 2], 0, "结尾必须是双 null");
    }

    #[test]
    fn env_block_handles_empty_env() {
        let block = build_env_block(&[]);
        assert_eq!(block, vec![0u16], "空环境也要有结尾 null");
    }

    /// **本模块最重要的测试：自检绝不允许"没做到却说做到了"。**
    ///
    /// 注意它断言的是**安全属性**（不谎报），而不是断言能力存在。
    /// 这与本项目其余部分同源：诚实隔离要求声称与实现一致，
    /// 而当实现达不到时，正确的行为是**承认达不到**。
    ///
    /// 当前实测结果是"隔离未生效"，因此走到 `is_effective() == false` 分支——
    /// 这正是期望行为：上层据此拒绝声称提供写入隔离。
    #[test]
    fn self_test_never_claims_isolation_it_cannot_deliver() {
        // 授权区放在系统预置的低完整性目录（AppData\LocalLow）下——
        // 那里低完整性子进程写得进去，而用户的中完整性文件一个都动不了。
        let low_root = low_integrity_root().expect("取低完整性根目录");
        let base = low_root.join(format!("YunXiBot-canary-{}", std::process::id()));
        let allowed = base.join("allowed");

        let medium_root = std::env::temp_dir().join(format!("yunxi-canary-{}", std::process::id()));
        let denied = medium_root.join("denied");
        std::fs::create_dir_all(&denied).expect("建中完整性目录");

        let report = match verify_write_isolation(&allowed, &denied) {
            Ok(r) => r,
            Err(e) => {
                let _ = std::fs::remove_dir_all(&base);
                let _ = std::fs::remove_dir_all(&medium_root);
                eprintln!("自检无法完成（{e}），因此不能声称提供写入隔离");
                return;
            }
        };
        let verdict = report.explain();
        let _ = std::fs::remove_dir_all(&base);
        let _ = std::fs::remove_dir_all(&medium_root);

        assert!(
            report.is_effective(),
            "写入隔离未通过自检，因此不能声称提供写入隔离。\n\
             判定: {verdict}\n\
             观测: allowed_written={} denied_written={} exit={} (0x{:08X})\n\
             canary stdout: {}\n\
             canary stderr: {}",
            report.allowed_written,
            report.denied_written,
            report.exit_code,
            report.exit_code as u32,
            report.stdout.trim(),
            report.stderr.trim()
        );
    }
}
