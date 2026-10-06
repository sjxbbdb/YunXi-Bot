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
    }

    if pid == 0 {
        return false;
    }
    // SAFETY: 两个调用都是标准的 Win32 句柄获取/释放，参数为普通整数。
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            // 拿不到句柄有两种可能：进程不存在，或权限不足。
            // 权限不足时说明进程确实在（只是不归我们管），所以再用 tasklist 兜一次。
            return process_listed_by_tasklist(pid);
        }
        CloseHandle(handle);
        true
    }
}

/// 兜底探测。只在 `OpenProcess` 失败时调用，避免常态下多起一个进程。
#[cfg(windows)]
fn process_listed_by_tasklist(pid: u32) -> bool {
    let out = std::process::Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"])
        .stdin(std::process::Stdio::null())
        .output();
    match out {
        Ok(o) => {
            let text = String::from_utf8_lossy(&o.stdout);
            // tasklist 找不到时输出 "信息: 没有运行的任务匹配指定标准。"
            text.contains(&pid.to_string()) && !text.contains("没有运行")
        }
        // 探测本身失败：保守认为活着，避免重复启动
        Err(_) => true,
    }
}

#[cfg(target_os = "linux")]
pub fn is_process_alive(pid: u32) -> bool {
    pid != 0 && Path::new(&format!("/proc/{pid}")).exists()
}

#[cfg(not(any(windows, target_os = "linux")))]
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
            at: crate::now_millis().unwrap_or(0),
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
