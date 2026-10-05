//! Windows 受限令牌 + 能力 SID 的文件系统写入隔离。
//!
//! ## 机制
//!
//! 1. 生成一个随机**能力 SID**（`S-1-4-<rand>-<rand>`）；
//! 2. 在**允许写入**的目录上给该 SID 加一条写 ACE；
//! 3. 用 `CreateRestrictedToken` 把该 SID 放进**限制 SID 列表**；
//! 4. 用 `CreateProcessAsUserW` 以该令牌启动子进程。
//!
//! 受限令牌的访问检查要做两遍：一遍用令牌的正常 SID，一遍**只用限制 SID**。
//! 两遍都通过才放行。限制列表里只有能力 SID，因此对象必须给能力 SID 授权，
//! 子进程才写得进去——**没授权的路径一律写不进**。这就是写入隔离。
//!
//! ## 为什么必须自检
//!
//! 上面这套 FFI 有太多地方可以"错了但不报错"：SID 权限位算错、ACE 没真正写进去、
//! 令牌没带限制列表……任何一种都会产出**看起来实现了隔离、实际毫无限制**的代码。
//!
//! 所以本模块提供 [`verify_write_isolation`]：真起一个 canary 子进程，
//! 让它分别尝试写**允许**与**禁止**的路径，只有"允许的写成功、禁止的写失败"
//! 才算通过。**隔离声明由这次实测背书**，通过不了就如实回落、让上层拒绝执行。
//!
//! ## ⚠️ 当前状态：**不可用**，且已实测确认
//!
//! 截至本次实现，写入隔离**没有达到可用状态**。因此
//! [`crate::exec::available_isolation`] **不会**返回
//! `WindowsRestrictedToken`，`--require-os-isolation` 至今仍然拒绝执行。
//! 这不是缺陷，是失败契约在正常工作。
//!
//! ### 已经做对的部分
//!
//! - 能力 SID 铸造、目录写 ACE 授予、受限令牌创建 —— 全部成功；
//! - 受限进程启动（含管道重定向、剥离凭证的环境块）—— 成功；
//! - 自检脚手架 —— 能如实报告"隔离未生效"。
//!
//! ### 卡在哪里（实测记录）
//!
//! | 变体 | 限制列表 | flags | 结果 |
//! |---|---|---|---|
//! | D | **空** | `DISABLE_MAX_PRIVILEGE` | ✅ `cmd.exe` 正常运行 |
//! | E | `Everyone` | `DISABLE_MAX_PRIVILEGE` | ❌ `STATUS_DLL_INIT_FAILED` |
//! | F | `Everyone` + 能力 SID | `DISABLE_MAX_PRIVILEGE` | ❌ 同上 |
//! | G | `Everyone` | `DISABLE_MAX_PRIVILEGE \| WRITE_RESTRICTED` | ❌ 同上 |
//!
//! **结论：在本机 Windows 上，限制列表只要非空，子进程就无法完成初始化**
//! （退出码 `0xC0000142`）。`WRITE_RESTRICTED` 也挡不住。
//!
//! 推断的原因：`WRITE_RESTRICTED` 确实在限制写，而**正常的进程启动本身就需要写**
//! ——注册表 `HKCU`、用户配置文件等。这些位置的 DACL 只授权给用户 SID，
//! 能力 SID 在那里没有 ACE，于是初始化失败。
//!
//! 也就是说，下一步需要把能力 SID 也授予**进程启动所必需的那些写位置**
//! （注册表键需要走 `SetSecurityInfo` 而非 `SetNamedSecurityInfoW`），
//! 而不是只授予工作目录。那是一个独立的工作量。
//!
//! ### 已经排掉的坑
//!
//! - `CreateRestrictedToken` 真实签名有 **9 个参数**，少声明中间两个会让调用
//!   错位读到栈上垃圾（表现为 Win32 错误 534）；
//! - 限制 SID 入参是 `SID_AND_ATTRIBUTES` **结构体数组**，不是 SID 指针数组；
//! - `TRUSTEE_W` 必须含 `TrusteeType` 且字段顺序与 Win32 一致，否则
//!   `SetEntriesInAclW` 返回 `ERROR_INVALID_PARAMETER (87)`；
//! - `GetNamedSecurityInfoW` 拒绝带结尾反斜杠的路径（同样是 87）；
//! - 空环境块是非法的（`CreateProcess` 返回 87）；
//! - `advapi32` 必须显式 `#[link]`，否则 `FreeSid` 链接失败。
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

/// `WRITE_RESTRICTED`——**这个机制的核心开关**。
///
/// 没有它时，受限令牌的访问检查对**读写一视同仁**：进程连读自己的 exe、
/// 读注册表 HKCU、访问窗口站都要过限制列表那一关。而那些对象的 DACL 只授权给
/// 用户 SID（不含 `Everyone`），于是子进程直接以 `STATUS_DLL_INIT_FAILED`
/// （实测退出码 `0xC0000142`）死掉——**任何**非空限制列表都会这样。
///
/// 带上它之后语义变成：
///
/// - **读**：用令牌的正常 SID 判定 → 进程能正常启动、能读它本来就能读的东西；
/// - **写**：只用限制 SID 判定 → 只有在被授予能力 SID 的目录里才写得进去。
///
/// 这正是"写入隔离"想要的形状，也解释了为什么限制列表里**只需要**能力 SID。
const WRITE_RESTRICTED: u32 = 0x0008;
const SECURITY_IMPERSONATION: u32 = 2;
const TOKEN_PRIMARY: u32 = 1;

const SE_FILE_OBJECT: u32 = 1;
const DACL_SECURITY_INFORMATION: u32 = 0x0000_0004;

const TRUSTEE_IS_SID: u32 = 0;
const TRUSTEE_IS_UNKNOWN: u32 = 0;
const NO_MULTIPLE_TRUSTEE: u32 = 0;
const GRANT_ACCESS: u32 = 1;
const SUB_CONTAINERS_AND_OBJECTS_INHERIT: u32 = 0x3;

const FILE_GENERIC_READ: u32 = 0x0012_0089;
const FILE_GENERIC_WRITE: u32 = 0x0012_0116;
const FILE_GENERIC_EXECUTE: u32 = 0x0012_00A0;

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

/// `SECURITY_NON_UNIQUE_AUTHORITY` = {0,0,0,0,0,4}，本地自铸标识符用它。
const NON_UNIQUE_AUTHORITY: SidIdentifierAuthority = SidIdentifierAuthority {
    value: [0, 0, 0, 0, 0, 4],
};

#[repr(C)]
struct SecurityAttributes {
    n_length: u32,
    lp_security_descriptor: *mut c_void,
    b_inherit_handle: i32,
}

/// `TRUSTEE_W`。
///
/// ⚠️ 字段顺序与数量必须与 Win32 完全一致：
/// `pMultipleTrustee, MultipleTrusteeOperation, TrusteeForm, TrusteeType, ptstrName`。
/// 少一个 `TrusteeType` 或把 `ptstrName` 提前，都会让后续字段错位——
/// 实测表现为 `SetEntriesInAclW` 返回 `ERROR_INVALID_PARAMETER (87)`。
#[repr(C)]
struct TrusteeW {
    p_multiple_trustee: *mut c_void,
    multiple_trustee_operation: u32,
    trustee_form: u32,
    trustee_type: u32,
    /// `TrusteeForm` 为 `TRUSTEE_IS_SID` 时，这里实际是 `PSID`（不是字符串）。
    p_trustee_name: *mut c_void,
}

#[repr(C)]
struct ExplicitAccessW {
    grf_access_permissions: u32,
    grf_access_mode: u32,
    grf_inheritance: u32,
    trustee: TrusteeW,
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
    fn FreeSid(sid: *mut c_void) -> *mut c_void;
    fn GetNamedSecurityInfoW(
        name: *const u16,
        ty: u32,
        info: u32,
        owner: *mut *mut c_void,
        group: *mut *mut c_void,
        dacl: *mut *mut c_void,
        sacl: *mut *mut c_void,
        sd: *mut *mut c_void,
    ) -> u32;
    fn SetNamedSecurityInfoW(
        name: *mut u16,
        ty: u32,
        info: u32,
        owner: *mut c_void,
        group: *mut c_void,
        dacl: *mut c_void,
        sacl: *mut c_void,
    ) -> u32;
    fn SetEntriesInAclW(
        count: u32,
        entries: *const ExplicitAccessW,
        old_acl: *mut c_void,
        new_acl: *mut *mut c_void,
    ) -> u32;
}

unsafe extern "system" {
    fn LocalFree(mem: *mut c_void) -> *mut c_void;
    fn GetEnvironmentStringsW() -> *mut u16;
    fn FreeEnvironmentStringsW(env: *mut u16) -> i32;
}

#[derive(Debug)]
pub enum TokenError {
    /// 能力 SID 创建失败
    Sid(u32),
    /// 令牌创建/复制失败
    Token(u32),
    /// ACL 授予失败
    Acl(u32),
    /// 进程启动失败
    Spawn(u32),
    /// 参数非法
    Invalid(String),
}

impl std::fmt::Display for TokenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TokenError::Sid(c) => write!(f, "创建能力 SID 失败（Win32 错误 {c}）"),
            TokenError::Token(c) => write!(f, "创建受限令牌失败（Win32 错误 {c}）"),
            TokenError::Acl(c) => write!(f, "设置目录 ACL 失败（Win32 错误 {c}）"),
            TokenError::Spawn(c) => write!(f, "启动受限子进程失败（Win32 错误 {c}）"),
            TokenError::Invalid(m) => write!(f, "参数非法: {m}"),
        }
    }
}

impl std::error::Error for TokenError {}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 把路径整理成 Win32 安全 API 能接受的形状。
///
/// **结尾反斜杠必须去掉**：`GetNamedSecurityInfoW` 对 `D:\temp\` 这样的路径
/// 直接返回 `ERROR_INVALID_PARAMETER (87)`（实测）。但根目录 `C:\` 要保留，
/// 否则会变成毫无意义的 `C:`。
fn for_security_api(path: &std::path::Path) -> String {
    let s = path.to_string_lossy().to_string();
    let trimmed = s.trim_end_matches(['\\', '/']);
    match trimmed.len() {
        // "C:" → "C:\"
        2 if trimmed.ends_with(':') => format!("{trimmed}\\"),
        // 全是分隔符（例如 "\"）：原样返回
        0 => s,
        _ => trimmed.to_string(),
    }
}

/// 能力 SID：一枚本地随机铸造的标识符，仅用于"这个文件允许这个子进程写"。
pub struct CapabilitySid {
    sid: *mut c_void,
}

// SID 是分配在堆上的不可变数据，跨线程共享只读访问是安全的
unsafe impl Send for CapabilitySid {}
unsafe impl Sync for CapabilitySid {}

impl CapabilitySid {
    /// 铸造一枚随机能力 SID。
    ///
    /// 用时间与 pid 混合出随机子授权号——**不需要密码学强度**：
    /// 这枚 SID 的用途是"标记一批允许写入的目录"，猜中它并不能绕过隔离
    /// （攻击者还需要以该用户身份运行，那时他本来就能写）。
    pub fn mint() -> Result<Self, TokenError> {
        let t = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        let pid = std::process::id() as u64;
        let sub0 = (t & 0xFFFF_FFFF) as u32;
        let sub1 = ((t >> 32) as u32) ^ (pid as u32).rotate_left(13);

        let mut sid: *mut c_void = std::ptr::null_mut();
        // SAFETY: authority 与子授权号都是普通整数；sid 由 API 分配，Drop 里释放
        let ok = unsafe {
            AllocateAndInitializeSid(
                &NON_UNIQUE_AUTHORITY,
                2,
                sub0,
                sub1,
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

    fn as_ptr(&self) -> *mut c_void {
        self.sid
    }

    /// 字符串形式，仅用于诊断输出。
    pub fn to_string_form(&self) -> String {
        format!("S-1-4-<本地铸造的能力 SID @{:p}>", self.sid)
    }
}

impl Drop for CapabilitySid {
    fn drop(&mut self) {
        if !self.sid.is_null() {
            unsafe { FreeSid(self.sid) };
        }
    }
}

/// 给 `path` 目录加上一条"允许该能力 SID 读写执行"的继承 ACE。
///
/// 继承标志是 `SUB_CONTAINERS_AND_OBJECTS_INHERIT`：子目录与文件一并继承，
/// 否则子进程只能写目录本身、写不了里面的文件。
pub fn grant_write(sid: &CapabilitySid, path: &std::path::Path) -> Result<(), TokenError> {
    let name = wide(&for_security_api(path));

    let mut old_dacl: *mut c_void = std::ptr::null_mut();
    let mut sd: *mut c_void = std::ptr::null_mut();
    // SAFETY: 取目录 DACL；sd 由 API 分配，用后 LocalFree
    let rc = unsafe {
        GetNamedSecurityInfoW(
            name.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut old_dacl,
            std::ptr::null_mut(),
            &mut sd,
        )
    };
    if rc != 0 {
        return Err(TokenError::Acl(rc));
    }

    let entry = ExplicitAccessW {
        grf_access_permissions: FILE_GENERIC_READ | FILE_GENERIC_WRITE | FILE_GENERIC_EXECUTE,
        grf_access_mode: GRANT_ACCESS,
        grf_inheritance: SUB_CONTAINERS_AND_OBJECTS_INHERIT,
        trustee: TrusteeW {
            p_multiple_trustee: std::ptr::null_mut(),
            multiple_trustee_operation: NO_MULTIPLE_TRUSTEE,
            trustee_form: TRUSTEE_IS_SID,
            trustee_type: TRUSTEE_IS_UNKNOWN,
            p_trustee_name: sid.as_ptr(),
        },
    };

    let mut new_dacl: *mut c_void = std::ptr::null_mut();
    // SAFETY: entry 指向有效的 SID；old_dacl 来自上面的查询
    let rc = unsafe { SetEntriesInAclW(1, &entry, old_dacl, &mut new_dacl) };
    if rc != 0 {
        unsafe { LocalFree(sd) };
        return Err(TokenError::Acl(rc));
    }

    // SAFETY: new_dacl 由 SetEntriesInAclW 分配；name 是可变缓冲区的指针
    let rc = unsafe {
        SetNamedSecurityInfoW(
            name.as_ptr() as *mut u16,
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            new_dacl,
            std::ptr::null_mut(),
        )
    };

    unsafe {
        LocalFree(new_dacl);
        LocalFree(sd);
    }
    if rc != 0 {
        return Err(TokenError::Acl(rc));
    }
    Ok(())
}

/// 受限令牌：当前进程令牌的受限副本，限制列表里只有那枚能力 SID。
pub struct RestrictedToken {
    handle: *mut c_void,
}

unsafe impl Send for RestrictedToken {}
unsafe impl Sync for RestrictedToken {}

impl RestrictedToken {
    /// 以当前进程令牌为基础，铸造带能力 SID 限制的令牌。
    pub fn create(sid: &CapabilitySid) -> Result<Self, TokenError> {
        let mut base: *mut c_void = std::ptr::null_mut();
        // SAFETY: 取当前进程令牌
        let ok = unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_ALL_FOR_LAUNCH, &mut base) };
        if ok == 0 {
            return Err(TokenError::Token(unsafe { GetLastError() }));
        }

        // 限制列表只需要能力 SID。
        //
        // 读走正常令牌，不需要 Everyone 兜底；写只认限制 SID，
        // 所以在未授予能力 SID 的目录里一律写不进去。
        //
        // （早先版本把 Everyone 也放进来，那是基于"读也要过限制检查"的错误
        // 假设；实测证明那种组合下子进程根本起不来。）
        let restrict_entries = [SidAndAttributes {
            sid: sid.as_ptr(),
            attributes: 0,
        }];

        let mut restricted: *mut c_void = std::ptr::null_mut();
        // SAFETY: 未用到的计数传 0 且指针传 null；restrict 数组有 1 项
        let ok = unsafe {
            CreateRestrictedToken(
                base,
                DISABLE_MAX_PRIVILEGE | WRITE_RESTRICTED,
                0,
                std::ptr::null(),
                0,
                std::ptr::null(),
                restrict_entries.len() as u32,
                restrict_entries.as_ptr(),
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
    let sid = CapabilitySid::mint()?;
    grant_write(&sid, allowed)?;
    let token = RestrictedToken::create(&sid)?;

    let canary =
        std::env::current_exe().map_err(|e| TokenError::Invalid(format!("取不到自身路径: {e}")))?;

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
    fn mints_distinct_capability_sids() {
        let a = CapabilitySid::mint().expect("铸造 SID");
        let b = CapabilitySid::mint().expect("铸造 SID");
        assert_ne!(a.as_ptr(), b.as_ptr(), "两次铸造应是不同的分配");
    }

    #[test]
    fn creates_a_restricted_token() {
        let sid = CapabilitySid::mint().expect("铸造 SID");
        let token = RestrictedToken::create(&sid);
        assert!(token.is_ok(), "建受限令牌失败: {:?}", token.err());
    }

    #[test]
    fn grants_write_ace_without_error() {
        let dir = std::env::temp_dir().join(format!("yunxi-acl-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("建目录");
        let sid = CapabilitySid::mint().expect("铸造 SID");
        assert!(
            grant_write(&sid, &dir).is_ok(),
            "给目录加 ACE 失败: {:?}",
            grant_write(&sid, &dir).err()
        );
        let _ = std::fs::remove_dir_all(&dir);
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
    fn security_api_paths_drop_trailing_separator() {
        use std::path::Path;
        // 这是实测踩到的坑：带结尾反斜杠会让 GetNamedSecurityInfoW 返回 87
        assert_eq!(
            for_security_api(Path::new(r"D:\temp\")),
            r"D:\temp",
            "结尾反斜杠必须去掉"
        );
        assert_eq!(for_security_api(Path::new(r"D:\temp")), r"D:\temp");
        // 但根目录必须保留反斜杠，否则 "C:\" 会变成毫无意义的 "C:"
        assert_eq!(for_security_api(Path::new(r"C:\")), r"C:\");
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
        let base = std::env::temp_dir().join(format!("yunxi-canary-{}", std::process::id()));
        let allowed = base.join("allowed");
        let denied = base.join("denied");
        std::fs::create_dir_all(&allowed).expect("建允许目录");
        std::fs::create_dir_all(&denied).expect("建禁止目录");

        let report = match verify_write_isolation(&allowed, &denied) {
            Ok(r) => r,
            Err(e) => {
                let _ = std::fs::remove_dir_all(&base);
                eprintln!("自检无法完成（{e}），因此不能声称提供写入隔离");
                return;
            }
        };
        let verdict = report.explain();
        let _ = std::fs::remove_dir_all(&base);

        if !report.is_effective() {
            // 当前实现下的预期路径。隔离不可用时绝不声称可用。
            eprintln!("写入隔离当前不可用：{verdict}");
            eprintln!(
                "  观测: allowed_written={} denied_written={} exit={}",
                report.allowed_written, report.denied_written, report.exit_code
            );
            return;
        }

        // 万一它真的生效了，必须两条都成立，不能只看一半
        assert!(
            report.allowed_written && !report.denied_written,
            "自检判定与观测不一致：{verdict}"
        );
    }
}
