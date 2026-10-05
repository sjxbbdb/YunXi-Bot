//! Windows Job Object 隔离。
//!
//! ## 它真实保证什么
//!
//! 1. **进程树强制回收**：`KILL_ON_JOB_CLOSE` 保证 Job 句柄一关，整个子进程树
//!    连同它派生的孙进程一起被杀。这解决了"杀了直接子进程，孙子还在占端口"的问题。
//! 2. **进程数上限**：`ACTIVE_PROCESS` 阻止 fork 炸弹把机器打满。
//! 3. **内存上限**：`PROCESS_MEMORY` 阻止单个任务吃光内存拖垮常驻进程。
//!
//! ## 它**不**保证什么
//!
//! **不做文件系统写入隔离。** 子进程仍以当前用户身份运行，能写用户能写的任何位置。
//! 写入隔离需要受限令牌 + 能力 SID（见 [`crate::exec::IsolationLevel`] 的
//! `WindowsRestrictedToken`），尚未实现。
//!
//! 这条边界必须如实标注：把"进程与资源隔离"说成"沙箱"是 ADR §八第 4 条明令禁止的。

#[cfg(windows)]
mod imp {
    use std::ffi::c_void;
    use std::process::Child;

    /// Job 句柄的薄封装，析构即关闭（从而杀掉整棵进程树）。
    #[derive(Debug)]
    pub struct JobObject {
        handle: *mut c_void,
    }

    // 句柄本身是内核对象，跨线程移动是安全的
    unsafe impl Send for JobObject {}
    unsafe impl Sync for JobObject {}

    #[repr(C)]
    #[derive(Default, Clone, Copy)]
    struct BasicLimitInformation {
        per_process_user_time_limit: i64,
        per_job_user_time_limit: i64,
        limit_flags: u32,
        minimum_working_set_size: usize,
        maximum_working_set_size: usize,
        active_process_limit: u32,
        affinity: usize,
        priority_class: u32,
        scheduling_class: u32,
    }

    #[repr(C)]
    #[derive(Default, Clone, Copy)]
    struct IoCounters {
        read_operation_count: u64,
        write_operation_count: u64,
        other_operation_count: u64,
        read_transfer_count: u64,
        write_transfer_count: u64,
        other_transfer_count: u64,
    }

    #[repr(C)]
    #[derive(Default, Clone, Copy)]
    struct ExtendedLimitInformation {
        basic: BasicLimitInformation,
        io: IoCounters,
        process_memory_limit: usize,
        job_memory_limit: usize,
        peak_process_memory_used: usize,
        peak_job_memory_used: usize,
    }

    const JOB_OBJECT_LIMIT_ACTIVE_PROCESS: u32 = 0x0000_0008;
    const JOB_OBJECT_LIMIT_PROCESS_MEMORY: u32 = 0x0000_0100;
    const JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE: u32 = 0x0000_2000;
    const JOB_OBJECT_EXTENDED_LIMIT_INFORMATION: u32 = 9;

    unsafe extern "system" {
        fn CreateJobObjectW(attrs: *mut c_void, name: *const u16) -> *mut c_void;
        fn SetInformationJobObject(
            job: *mut c_void,
            class: u32,
            info: *mut c_void,
            len: u32,
        ) -> i32;
        fn AssignProcessToJobObject(job: *mut c_void, process: *mut c_void) -> i32;
        fn CloseHandle(handle: *mut c_void) -> i32;
        fn GetLastError() -> u32;
    }

    #[derive(Debug)]
    pub enum JobError {
        Create(u32),
        Configure(u32),
        Assign(u32),
    }

    impl std::fmt::Display for JobError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                JobError::Create(c) => write!(f, "CreateJobObject 失败（Win32 错误 {c}）"),
                JobError::Configure(c) => {
                    write!(f, "SetInformationJobObject 失败（Win32 错误 {c}）")
                }
                JobError::Assign(c) => {
                    write!(f, "AssignProcessToJobObject 失败（Win32 错误 {c}）")
                }
            }
        }
    }

    impl std::error::Error for JobError {}

    impl JobObject {
        /// 建一个带资源上限的 Job。
        ///
        /// `memory_limit_bytes` / `max_processes` 为 `None` 表示该项不设限；
        /// 无论是否设限，**`KILL_ON_JOB_CLOSE` 永远开启**——这是本隔离的主要价值。
        pub fn create(
            memory_limit_bytes: Option<u64>,
            max_processes: Option<u32>,
        ) -> Result<Self, JobError> {
            // SAFETY: 传空属性与空名字，是 CreateJobObjectW 的合法调用方式
            let handle = unsafe { CreateJobObjectW(std::ptr::null_mut(), std::ptr::null()) };
            if handle.is_null() {
                return Err(JobError::Create(unsafe { GetLastError() }));
            }

            let mut info = ExtendedLimitInformation::default();
            info.basic.limit_flags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            if let Some(n) = max_processes {
                info.basic.limit_flags |= JOB_OBJECT_LIMIT_ACTIVE_PROCESS;
                info.basic.active_process_limit = n;
            }
            if let Some(bytes) = memory_limit_bytes {
                info.basic.limit_flags |= JOB_OBJECT_LIMIT_PROCESS_MEMORY;
                info.process_memory_limit = bytes as usize;
            }

            // SAFETY: info 是 #[repr(C)] 且按 Win32 结构布局声明；长度取实际大小
            let ok = unsafe {
                SetInformationJobObject(
                    handle,
                    JOB_OBJECT_EXTENDED_LIMIT_INFORMATION,
                    &mut info as *mut _ as *mut c_void,
                    std::mem::size_of::<ExtendedLimitInformation>() as u32,
                )
            };
            if ok == 0 {
                let code = unsafe { GetLastError() };
                unsafe { CloseHandle(handle) };
                return Err(JobError::Configure(code));
            }

            Ok(Self { handle })
        }

        /// 把子进程放进 Job。
        ///
        /// `std::process::Child` 在 Windows 上实现了 `AsRawHandle`，因此不需要
        /// 自己走 `CreateProcess` 就能拿到进程句柄。
        pub fn assign(&self, child: &Child) -> Result<(), JobError> {
            use std::os::windows::io::AsRawHandle;
            // RawHandle 本身就是 *mut c_void，无需转换
            let proc = child.as_raw_handle();
            // SAFETY: 两个句柄都来自有效对象，且在本调用期间存活
            let ok = unsafe { AssignProcessToJobObject(self.handle, proc) };
            if ok == 0 {
                return Err(JobError::Assign(unsafe { GetLastError() }));
            }
            Ok(())
        }
    }

    impl Drop for JobObject {
        fn drop(&mut self) {
            // 关闭句柄即触发 KILL_ON_JOB_CLOSE：整棵进程树随之结束
            unsafe { CloseHandle(self.handle) };
        }
    }
}

#[cfg(windows)]
pub use imp::{JobError, JobObject};

/// 非 Windows 平台的占位：Job Object 是 Windows 专有机制。
#[cfg(not(windows))]
#[derive(Debug)]
pub struct JobObject;

#[cfg(not(windows))]
#[derive(Debug)]
pub enum JobError {
    Unsupported,
}

#[cfg(not(windows))]
impl std::fmt::Display for JobError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Job Object 是 Windows 专有机制")
    }
}

#[cfg(not(windows))]
impl std::error::Error for JobError {}

#[cfg(not(windows))]
impl JobObject {
    pub fn create(_m: Option<u64>, _p: Option<u32>) -> Result<Self, JobError> {
        Err(JobError::Unsupported)
    }
    pub fn assign(&self, _child: &std::process::Child) -> Result<(), JobError> {
        Err(JobError::Unsupported)
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    #[test]
    fn creates_a_job_with_limits() {
        let job = JobObject::create(Some(512 * 1024 * 1024), Some(4));
        assert!(job.is_ok(), "建 Job 失败: {:?}", job.err());
    }

    #[test]
    fn kill_on_close_terminates_the_whole_tree() {
        let job = JobObject::create(None, None).expect("建 Job");

        // 起一个长命子进程，它自己再派生一个孙进程
        let mut child = Command::new("cmd")
            .args(["/C", "ping -n 60 127.0.0.1 > NUL"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("起子进程");

        job.assign(&child).expect("放入 Job");

        // 确认它活着
        assert!(
            child.try_wait().expect("try_wait").is_none(),
            "子进程应仍在运行"
        );

        // 关掉 Job：KILL_ON_JOB_CLOSE 应当把进程树一并杀死
        drop(job);

        let deadline = Instant::now() + Duration::from_secs(10);
        let mut exited = false;
        while Instant::now() < deadline {
            if child.try_wait().expect("try_wait").is_some() {
                exited = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        if !exited {
            let _ = child.kill();
        }
        assert!(
            exited,
            "关闭 Job 句柄后子进程必须在合理时间内结束（KILL_ON_JOB_CLOSE）"
        );
    }

    #[test]
    fn memory_limit_is_enforced() {
        // 严格限制：单进程 32 MiB。
        // 用 powershell 是因为它是 Windows 上最容易造出大分配的现成工具；
        // 32 MiB 的额度连它正常启动都撑不住，所以期望是"进程异常结束"。
        let job = JobObject::create(Some(32 * 1024 * 1024), None).expect("建 Job");
        let mut child = Command::new("powershell")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "$null = New-Object byte[] 268435456; 'done'",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("起子进程");
        job.assign(&child).expect("放入 Job");

        let deadline = Instant::now() + Duration::from_secs(60);
        let mut status = None;
        while Instant::now() < deadline {
            if let Some(s) = child.try_wait().expect("try_wait") {
                status = Some(s);
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        match status {
            Some(s) => {
                let _ = child.wait(); // 回收，避免留下僵尸句柄
                assert!(
                    !s.success(),
                    "超出内存上限的进程不应正常成功退出（实际: {s:?}）"
                );
            }
            None => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("超出内存上限的进程在 60 秒内既没被杀也没结束，内存限制未生效");
            }
        }
    }
}
