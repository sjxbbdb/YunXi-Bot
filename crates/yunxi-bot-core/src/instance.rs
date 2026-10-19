//! 单实例锁。
//!
//! 常驻进程最怕的是"起了两个"，两个守护同时调度同一批任务会导致重复执行。
//! 锁文件里记 pid，启动时**判断该 pid 是否真的还活着**——只看文件存在会把
//! 上次崩溃留下的陈旧锁当成"已有实例在跑"，从此再也起不来。
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
            LockError::AlreadyRunning { pid } => {
                write!(
                    f,
                    "已有实例在运行（pid {pid}）。若确认它已死，删除锁文件后重试。"
                )
            }
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

/// 单实例锁。析构时释放。
#[derive(Debug)]
pub struct InstanceLock {
    path: PathBuf,
    pid: u32,
}

impl InstanceLock {
    /// 获取锁。
    ///
    /// 返回 `Err(AlreadyRunning)` 表示已有存活实例——调用方**不应该**继续启动。
    /// 陈旧锁（pid 已死或文件损坏）会被自动接管。
    pub fn acquire(path: impl AsRef<Path>) -> Result<Self, LockError> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent).map_err(|e| LockError::Io(e.to_string()))?;
        }

        if let Ok(text) = fs::read_to_string(&path)
            && let Ok(info) = serde_json::from_str::<LockInfo>(&text)
            && is_process_alive(info.pid)
            && info.pid != std::process::id()
        {
            return Err(LockError::AlreadyRunning { pid: info.pid });
        }
        // 解析失败或进程已死：视为陈旧锁，直接接管

        let pid = std::process::id();
        let info = LockInfo {
            pid,
            at: crate::now_millis().map_err(|e| LockError::Io(e.to_string()))?,
            schema: 1,
        };
        let body = serde_json::to_string(&info).map_err(|e| LockError::Io(e.to_string()))?;
        fs::write(&path, body).map_err(|e| LockError::Io(e.to_string()))?;

        Ok(Self { path, pid })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn pid(&self) -> u32 {
        self.pid
    }
}

impl Drop for InstanceLock {
    fn drop(&mut self) {
        // 只删自己的锁：万一被别的实例接管过，不要误删它的
        if let Ok(text) = fs::read_to_string(&self.path)
            && let Ok(info) = serde_json::from_str::<LockInfo>(&text)
        {
            if info.pid == self.pid {
                let _ = fs::remove_file(&self.path);
            }
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
    fn system_pid() -> u32 {
        if cfg!(windows) { 4 } else { 1 }
    }

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
        assert!(!p.exists(), "析构后锁文件应被清理");
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
