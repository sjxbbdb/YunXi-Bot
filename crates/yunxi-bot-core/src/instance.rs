//! 单实例锁。
//!
//! 常驻进程最怕的是"起了两个"，两个守护同时调度同一批任务会导致重复执行。
//!
//! Unix 上用**内核文件锁（flock）**判定归属：锁随进程存活，进程正常退出、
//! 崩溃、被杀死或整机关机时都由内核自动释放，不存在"陈旧锁"。锁文件里的 pid
//! 只用于诊断和报错文案，**不再作为"是否已有实例"的依据**——PID 会被系统复用，
//! 把复用后的无关进程当成持锁者会让守护进程永久起不来（2026-10-21、10-23 两次
//! 实机事故：锁里的 pid 分别被 ScreenTime 和 iCloud 进程复用，开机问候与冒泡
//! 因此整体消失）。Windows 上 `OpenProcess` 探测仍是主路径。
//!
//! 只依赖 std 与少量平台 FFI，不引入 winapi/libc 依赖树。

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// 锁文件内容。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LockInfo {
    pub pid: u32,
    /// 写入时刻（Unix 毫秒），仅用于诊断。
    pub at: u64,
    /// 版本号，便于将来识别过期锁。
    #[serde(default)]
    pub schema: u32,
}

#[derive(Debug)]
pub enum LockError {
    Io(String),
    /// 已有存活实例在运行。
    AlreadyRunning {
        pid: u32,
    },
}

impl std::fmt::Display for LockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LockError::Io(m) => write!(f, "锁文件操作失败: {m}"),
            LockError::AlreadyRunning { pid } => write!(f, "已有实例在运行（pid {pid}）"),
        }
    }
}

impl std::error::Error for LockError {}

/// 判断进程是否存活。
///
/// - Windows：`OpenProcess` 探测句柄能否拿到。
/// - Linux：`/proc/<pid>` 是否存在。
/// - macOS：kill(pid, 0) 探测，不发送信号；只有明确不存在才允许接管。
/// - 其他平台：保守返回 `true`（宁可拒绝启动，也不要冒重复运行的风险）。
#[cfg(windows)]
pub fn is_process_alive(pid: u32) -> bool {
    // 直接 FFI，避免为这一个调用引入 winapi 依赖树
    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;

    unsafe extern "system" {
        fn OpenProcess(
            dw_desired_access: u32,
            b_inherit_handle: i32,
            dw_process_id: u32,
        ) -> *mut core::ffi::c_void;
        fn CloseHandle(h_object: *mut core::ffi::c_void) -> i32;
        fn GetLastError() -> u32;
    }

    if pid == 0 {
        return false;
    }
    // SAFETY: 两个调用都是标准的 Win32 句柄获取/释放，参数为普通整数。
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            // **拿不到句柄有两种可能，而 `GetLastError()` 直接说清了是哪一种。**
            //
            // 这里原来是用 `tasklist` 兜底的——**那是个严重的问题**：
            // 真机上 `tasklist /FI "PID eq 4"`（PID 4 是内核进程，
            // `OpenProcess` 打不开）**会挂五分钟以上**。
            // 而这段代码在 `InstanceLock::acquire` 里，
            // 也就是**启动路径上**：一个"活没活"的探测把启动卡住几分钟。
            //
            // 那个卡顿还伪装成了测试失败——`refuses_when_lock_held_by_a_live_process`
            // 报的是"没拒绝"，实际是"探测还没回来"。**查了半天才看出是它。**
            //
            // 而这两件事根本不需要外部命令：
            // - **拒绝访问** → 进程存在，只是不归我们管 → **活着**
            // - **参数无效** → 这个 PID 不存在 → **死了**
            const ERROR_ACCESS_DENIED: u32 = 5;
            const ERROR_INVALID_PARAMETER: u32 = 87;
            let code = GetLastError();
            return match code {
                ERROR_ACCESS_DENIED => true,
                ERROR_INVALID_PARAMETER => false,
                // **说不清的时候按"活着"算。**
                // 接管一个活进程持有的锁 = 两个实例同时写同一个 home，
                // 那比"多拒绝启动一次"坏得多。
                _ => true,
            };
        }
        CloseHandle(handle);
        true
    }
}

#[cfg(target_os = "linux")]
pub fn is_process_alive(pid: u32) -> bool {
    pid != 0 && Path::new(&format!("/proc/{pid}")).exists()
}

#[cfg(target_os = "macos")]
pub fn is_process_alive(pid: u32) -> bool {
    // kill 的 0 和负 PID 表示进程组，不能把锁文件里的无效 PID 转成组探测。
    let Ok(pid) = i32::try_from(pid) else {
        return false;
    };
    if pid == 0 {
        return false;
    }
    unsafe extern "C" {
        fn kill(pid: i32, signal: i32) -> i32;
    }
    // SAFETY: 正 PID、信号 0 只检查存活/权限，不修改目标进程。
    if unsafe { kill(pid, 0) } == 0 {
        return true;
    }
    const ESRCH: i32 = 3; // macOS SDK sys/errno.h: No such process
    // EPERM 或其他未知错误都不能证明进程已死。
    std::io::Error::last_os_error().raw_os_error() != Some(ESRCH)
}

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
pub fn is_process_alive(pid: u32) -> bool {
    let _ = pid;
    true
}

/// 单实例锁。持有期间保证同一数据目录只有一个实例。
///
/// Unix 上锁由 flock 持有，`Drop` 时自动释放；锁文件保留用于诊断。
#[derive(Debug)]
pub struct InstanceLock {
    path: PathBuf,
    pid: u32,
    /// Unix 上持有 flock 的描述符；它的生命周期就是锁的生命周期。
    #[cfg(unix)]
    _file: std::fs::File,
}

impl InstanceLock {
    /// 获取锁。
    ///
    /// 返回 `Err(AlreadyRunning)` 表示已有存活实例——调用方**不应该**继续启动。
    pub fn acquire(path: impl AsRef<Path>) -> Result<Self, LockError> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent).map_err(|e| LockError::Io(e.to_string()))?;
        }

        #[cfg(unix)]
        {
            Self::acquire_unix(path)
        }
        #[cfg(not(unix))]
        {
            Self::acquire_by_pid(path)
        }
    }

    /// Unix：内核文件锁。锁文件保留在磁盘上，内容只作诊断。
    #[cfg(unix)]
    fn acquire_unix(path: PathBuf) -> Result<Self, LockError> {
        use std::os::{fd::AsRawFd, unix::fs::OpenOptionsExt};
        const LOCK_EX: i32 = 0x02;
        const LOCK_NB: i32 = 0x04;
        unsafe extern "C" {
            fn flock(fd: i32, operation: i32) -> i32;
        }
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(&path)
            .map_err(|e| LockError::Io(e.to_string()))?;
        // SAFETY: 有效描述符；LOCK_EX | LOCK_NB 只尝试加锁，不写文件。
        if unsafe { flock(file.as_raw_fd(), LOCK_EX | LOCK_NB) } != 0 {
            let error = std::io::Error::last_os_error();
            // EWOULDBLOCK / EAGAIN：锁已被另一个存活实例持有（macOS 35、Linux 11）。
            if matches!(error.raw_os_error(), Some(35) | Some(11)) {
                return Err(LockError::AlreadyRunning {
                    pid: read_holder_pid(&path).unwrap_or(0),
                });
            }
            // 文件系统不支持 flock：退回旧的 pid 探测，保守处理。
            let pid = claim_by_pid(&path, 1)?;
            return Ok(Self {
                path,
                pid,
                _file: file,
            });
        }
        let pid = write_lock_info(&path, 2)?;
        Ok(Self {
            path,
            pid,
            _file: file,
        })
    }

    /// 非 Unix：沿用 pid 探测（Windows 的存活探测无法被 flock 替代）。
    #[cfg(not(unix))]
    fn acquire_by_pid(path: PathBuf) -> Result<Self, LockError> {
        let pid = claim_by_pid(&path, 1)?;
        Ok(Self { path, pid })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn pid(&self) -> u32 {
        self.pid
    }
}

/// 读取锁文件里记录的 pid（仅用于诊断与错误文案）。
fn read_holder_pid(path: &Path) -> Option<u32> {
    fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str::<LockInfo>(&text).ok())
        .map(|info| info.pid)
}

/// 写入锁信息并返回当前 pid（调用方必须已经实际持有锁）。
fn write_lock_info(path: &Path, schema: u32) -> Result<u32, LockError> {
    let pid = std::process::id();
    let info = LockInfo {
        pid,
        at: crate::now_millis().map_err(|e| LockError::Io(e.to_string()))?,
        schema,
    };
    let body = serde_json::to_string(&info).map_err(|e| LockError::Io(e.to_string()))?;
    fs::write(path, body).map_err(|e| LockError::Io(e.to_string()))?;
    Ok(pid)
}

/// 旧路径（非 Unix 或文件系统不支持 flock）：先做 pid 探测，再写锁文件。
fn claim_by_pid(path: &Path, schema: u32) -> Result<u32, LockError> {
    if let Ok(text) = fs::read_to_string(path)
        && let Ok(info) = serde_json::from_str::<LockInfo>(&text)
        && is_process_alive(info.pid)
        && info.pid != std::process::id()
    {
        return Err(LockError::AlreadyRunning { pid: info.pid });
    }
    write_lock_info(path, schema)
}

/// Windows 等没有 flock 的平台仍在析构时清理锁文件。
#[cfg(not(unix))]
impl Drop for InstanceLock {
    fn drop(&mut self) {
        // 只删自己的锁：万一被别的实例接管过，不要误删它的
        if let Some(pid) = read_holder_pid(&self.path)
            && pid != self.pid
        {
            return;
        }
        let _ = fs::remove_file(&self.path);
    }
}

/// 显式解锁，避免并发创建子进程时继承的描述符延长锁的生命周期。
pub(crate) struct ExclusiveFileLock(std::fs::File);

impl Drop for ExclusiveFileLock {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            unsafe extern "C" {
                fn flock(fd: i32, operation: i32) -> i32;
            }
            loop {
                // SAFETY: File 仍存活；LOCK_UN 只释放本守卫取得的锁，不删除锁文件。
                if unsafe { flock(self.0.as_raw_fd(), 0x08) } == 0 {
                    break;
                }
                let error = std::io::Error::last_os_error();
                if error.kind() != std::io::ErrorKind::Interrupted {
                    eprintln!("操作锁释放失败：{error}");
                    break;
                }
            }
        }
    }
}

/// 内核文件锁保护跨进程读改写；守卫释放时解锁，锁文件不能删除。
#[cfg(unix)]
pub(crate) fn exclusive_file_lock(path: &Path) -> Result<ExclusiveFileLock, String> {
    use std::os::{fd::AsRawFd, unix::fs::OpenOptionsExt};
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)
        .map_err(|_| "无法打开操作锁")?;
    unsafe extern "C" {
        fn flock(fd: i32, operation: i32) -> i32;
    }
    // SAFETY: 有效的 File 描述符；LOCK_EX | LOCK_NB，不删除文件使其他进程锁失效。
    if unsafe { flock(file.as_raw_fd(), 0x02 | 0x04) } != 0 {
        return Err("另一个进程正在处理，请稍后重试".into());
    }
    Ok(ExclusiveFileLock(file))
}
#[cfg(not(unix))]
pub(crate) fn exclusive_file_lock(_: &Path) -> Result<ExclusiveFileLock, String> {
    Err("邮件理解当前只支持本机Mac".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn closing_owner_releases_lock_despite_an_inherited_descriptor() {
        let path = tmp("inherited-file-lock");
        let owner = exclusive_file_lock(&path).unwrap();
        let inherited = owner.0.try_clone().unwrap();
        assert!(exclusive_file_lock(&path).is_err());
        drop(owner);
        let next = exclusive_file_lock(&path)
            .expect("进程创建期间继承的描述符不应延长原持有者的锁生命周期");
        drop(inherited);
        assert!(exclusive_file_lock(&path).is_err());
        drop(next);
        assert!(exclusive_file_lock(&path).is_ok());
    }

    fn tmp(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("yunxi-bot-lock-{}-{}", std::process::id(), name));
        let _ = fs::remove_file(&p);
        p
    }

    #[test]
    fn acquires_when_no_lock_exists() {
        let p = tmp("fresh");
        let lock = InstanceLock::acquire(&p).expect("首次应能拿到锁");
        assert_eq!(lock.pid(), std::process::id());
        assert!(p.exists());
    }

    /// 一个几乎必然存在的系统进程 pid，用于测试"锁被活进程占着"。
    #[cfg(not(unix))]
    fn system_pid() -> u32 {
        if cfg!(windows) { 4 } else { 1 }
    }

    #[cfg(unix)]
    #[test]
    fn refuses_when_another_instance_holds_the_lock() {
        let p = tmp("held");
        let first = InstanceLock::acquire(&p).expect("首次应能拿到锁");
        match InstanceLock::acquire(&p) {
            Err(LockError::AlreadyRunning { pid }) => assert_eq!(pid, std::process::id()),
            other => panic!("锁被存活实例占着时必须拒绝启动，实际: {other:?}"),
        }
        drop(first);
        assert!(
            InstanceLock::acquire(&p).is_ok(),
            "持有者退出后锁必须自动释放"
        );
    }

    /// 回归 2026-10-21 / 10-23 实机事故：锁里记的 pid 被系统进程复用。
    /// 旧实现只看"pid 是否活着"，于是把无关进程当成持锁者，守护进程永久起不来。
    #[cfg(unix)]
    #[test]
    fn takes_over_a_lock_whose_pid_was_reused_by_an_unrelated_process() {
        let p = tmp("reused-pid");
        let reused = LockInfo {
            pid: 1, // 一定存在的系统进程，但并没有持有 flock
            at: 0,
            schema: 1,
        };
        fs::write(&p, serde_json::to_string(&reused).unwrap()).unwrap();
        assert!(
            InstanceLock::acquire(&p).is_ok(),
            "pid 被无关进程复用时必须接管，不能拒绝启动"
        );
    }

    #[cfg(not(unix))]
    #[test]
    fn refuses_when_lock_held_by_a_live_process() {
        let p = tmp("held");
        let held = LockInfo {
            pid: system_pid(),
            at: 0,
            schema: 1,
        };
        fs::write(&p, serde_json::to_string(&held).unwrap()).unwrap();

        match InstanceLock::acquire(&p) {
            Err(LockError::AlreadyRunning { pid }) => assert_eq!(pid, system_pid()),
            other => panic!("锁被活进程占着时必须拒绝启动，实际: {other:?}"),
        }
        let _ = fs::remove_file(&p);
    }

    #[test]
    fn takes_over_a_stale_lock() {
        // 崩溃留下的锁必须能被接管，否则崩过一次就再也起不来
        let p = tmp("stale");
        let dead = LockInfo {
            pid: 0xFFFF_FFF0,
            at: 0,
            schema: 1,
        };
        fs::write(&p, serde_json::to_string(&dead).unwrap()).unwrap();
        assert!(InstanceLock::acquire(&p).is_ok());
        let _ = fs::remove_file(&p);
    }

    #[test]
    fn corrupted_lock_is_taken_over() {
        let p = tmp("corrupt");
        fs::write(&p, "这不是 JSON").unwrap();
        assert!(
            InstanceLock::acquire(&p).is_ok(),
            "损坏的锁文件不该让人永远起不来"
        );
        let _ = fs::remove_file(&p);
    }

    #[test]
    fn drop_releases_the_lock() {
        let p = tmp("release");
        {
            let _lock = InstanceLock::acquire(&p).unwrap();
            assert!(p.exists());
        }
        #[cfg(not(unix))]
        assert!(!p.exists(), "非 Unix 平台析构后锁文件应被清理");
        assert!(InstanceLock::acquire(&p).is_ok(), "析构后必须能重新获得锁");
    }

    #[test]
    fn current_process_is_detected_alive() {
        assert!(is_process_alive(std::process::id()));
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn invalid_pids_are_not_treated_as_process_groups() {
        assert!(!is_process_alive(0));
        assert!(!is_process_alive(u32::MAX));
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn a_reaped_child_no_longer_holds_the_instance_lock() {
        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", "exit 0"])
            .spawn()
            .unwrap();
        let pid = child.id();
        child.wait().unwrap();
        assert!(!is_process_alive(pid));
        let p = tmp("reaped");
        fs::write(
            &p,
            serde_json::to_string(&LockInfo {
                pid,
                at: 0,
                schema: 1,
            })
            .unwrap(),
        )
        .unwrap();
        assert!(InstanceLock::acquire(&p).is_ok());
    }
}
