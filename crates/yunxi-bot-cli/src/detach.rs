//! 分离启动 sidecar：**先把不该被继承的句柄从句柄继承链上摘掉。**
//!
//! ## 这个模块为什么存在（一个真把人卡死的缺陷）
//!
//! 现象：调用方**用管道**拉起 `yunxi-bot chat`（Python 的
//! `subprocess.run(..., capture_output=True)`、任何 CI 作业、任何 GUI 外壳
//! 都是这个形状），`chat` 自己 rc=0 正常退出，而**调用方的 `communicate()`
//! 永远不返回**。实测：一轮 14 分 03 秒的端到端里约 12 分钟卡在这里，
//! 而它真正干的活只有 2 分钟左右；`taskkill` 掉那个 sidecar 之后 0.1 秒就返回。
//!
//! ## 机制（两层，都有出处，不是"大概是句柄泄漏"）
//!
//! 1. Windows 上 `CreateProcess` 的 `bInheritHandles=TRUE` 会让子进程继承
//!    **调用进程里每一个带继承位的句柄**，不只是 stdio 那三个。Rust std 在
//!    Windows 上把这个参数**写死成 `TRUE`**（`sys/process/windows.rs` 里
//!    `CreateProcessW(..., inherit_handles, ...)`，而它的默认值就是 `true`），
//!    稳定版 std **没有任何口子**能关掉：`CommandExt::inherit_handles` 确实
//!    存在，但在 1.97 上还是 nightly
//!    （`windows_process_extensions_inherit_handles`，#146407）；
//!    `spawn_with_attributes` + `ProcThreadAttributeList` 同样 nightly。
//! 2. 调用方给我的那两根管道写端**就是我自己的 stdout/stderr**——它就是通过
//!    标准句柄交到我手上的（Python 建这两端时必须让它可继承，否则我也拿不到）。
//!    所以 `Stdio::null()` 只改了子进程**自己**的 stdio，**挡不住句柄继承**：
//!    分离出去的 sidecar 手里多了一份写端，调用方就永远读不到 EOF。
//!
//! 现场证据（一次性诊断 `target/probe`，跑完即删）：被管道拉起来的进程，
//! 句柄表里带继承位的句柄**恰好只有那 3 个管道**；它再用
//! `std::process::Command` 分离启动一个孙进程，孙进程句柄表里同样是那 3 个
//! 管道，而中间进程退出之后调用方的管道 3 秒内**没有 EOF**；把
//! creation_flags 换成 `0` / `DETACHED_PROCESS` / `CREATE_NEW_PROCESS_GROUP` /
//! `DETACHED_PROCESS|CREATE_NO_WINDOW`，**一个都不影响**（继承与否只由
//! `bInheritHandles` 决定）；只把那 3 个标准句柄的继承位清掉之后，孙进程
//! 句柄表里 0 个管道，调用方**立刻**拿到 EOF。
//!
//! ## 所以这里做的事
//!
//! `spawn` 之前把三个标准句柄的 `HANDLE_FLAG_INHERIT` 清掉，`spawn` 之后
//! 还原。用 `kernel32` 直接 FFI（`GetStdHandle` / `GetHandleInformation` /
//! `SetHandleInformation`），**不引 winapi 依赖树**——和 `instance.rs`、
//! `win_job.rs`、`win_token.rs` 的做法一致。
//!
//! 换 creation_flags 没用（上面量过），换 std 的 API 没有（都要 nightly），
//! 而"起一个中间进程"也不解决问题：中间进程同样是 std 拉起来的，
//! 一样继承那两根管道，它的孩子照样拿着——**继承链得从我们这一环断开**。
//!
//! ## 为什么清掉这几个位不会把别的东西弄坏
//!
//! - 子进程**自己的** stdio 不受影响：`Stdio::null()` 是这次 spawn 现开的
//!   `\\.\NUL`，开的时候就带继承位（std 源码 `Stdio::Null` 那一支），
//!   和我们的标准句柄可不可继承没有关系。
//! - `Stdio::inherit()` / `Stdio::from(File)` 也不受影响：std 那两条路都是
//!   **先按 `inherit=true` 复制一份**（`Stdio::to_handle` 里的 `duplicate`），
//!   复制出来的新句柄带继承位，不看原件。
//! - 还原紧跟在 spawn 后面（**不是等子进程退出**），窗口只有一次系统调用宽。
//!
//! ## 这里**没有**覆盖到的（写出来，免得下一个人以为它包治百病）
//!
//! 只清标准句柄。调用方如果**另外**塞了可继承句柄进来（Python 的
//! `pass_fds=`、父进程故意留下的句柄），它们照样会被继承。要根治得走
//! `PROC_THREAD_ATTRIBUTE_HANDLE_LIST`，而那条路要求自己调 `CreateProcessW`
//! （命令行引号、stdio、属性表全得自己写），不是一个"小改动"。
//! 这里解决的是**实测到的那一类**：卡死调用方的就是标准句柄这两根管道，
//! 而一个 `chat` 进程里带继承位的句柄实测也只有它们。
//!
//! ## 并发
//!
//! "清位 → spawn → 还原"这一小段**不是原子的**。当前的调用点在 `cmd_chat`
//! 开头，那时 MCP、tokio、工具子进程都还没起来（进程是单线程的），所以这里
//! 没有加锁；而且就算撞上，代价也只是"别人那一刻复制出去的句柄不带继承位"，
//! 而 std 自己会给它加上（见上）。**要在多线程里用它，先想清楚这一段。**
//! （std 自己也有同形的竞态，它用一把私有 `Mutex` 护住"建可继承句柄 +
//! CreateProcess"，那把锁我们借不到。）

use std::process::{Child, Command};

/// 分离启动：`cmd` 的 `creation_flags` / stdio 由调用方配好，
/// 这里只负责**别让子进程继承调用方的管道**（理由见模块文档）。
///
/// 返回 `Child` 而不是丢掉：调用方拿它做 `try_wait()`，
/// 就能在"起来就退了"和"只是还没监听"之间说实话。
pub fn spawn_detached(cmd: &mut Command) -> std::io::Result<Child> {
    #[cfg(windows)]
    {
        let saved = windows::strip_std_inherit();
        let spawned = cmd.spawn();
        windows::restore_std_inherit(&saved);
        spawned
    }
    #[cfg(not(windows))]
    {
        // 非 Windows 没有这个问题：std 在 fork 之后会把子进程的 0/1/2
        // 换成它自己开的那些（`Stdio::null()` 走的是 dup2），
        // 调用方的管道不会留在孙进程手里。
        cmd.spawn()
    }
}

#[cfg(windows)]
mod windows {
    //! 三个 Win32 调用，直接声明——**为一个位操作引 winapi 依赖树不值得**
    //! （和 `instance.rs` 里 `OpenProcess` 那段的理由一样）。
    use std::ffi::c_void;

    // kernel32 是 std 默认就链接的库，所以这里不需要 `#[link]`。
    unsafe extern "system" {
        /// 取标准句柄。拿不到（GUI 进程没有 stdio）时返回空或 `INVALID_HANDLE_VALUE`。
        fn GetStdHandle(n_std_handle: u32) -> *mut c_void;
        /// 查句柄的 flags——我们要的是 `HANDLE_FLAG_INHERIT` 在不在。
        fn GetHandleInformation(h_object: *mut c_void, lpdw_flags: *mut u32) -> i32;
        /// 改句柄的 flags。**只动 `dw_mask` 里那几位**，别的位不动。
        fn SetHandleInformation(h_object: *mut c_void, dw_mask: u32, dw_flags: u32) -> i32;
    }

    /// 这一位就是"能被 `CreateProcess` 继承"。
    const HANDLE_FLAG_INHERIT: u32 = 0x0000_0001;
    /// `GetStdHandle` 的编号。**是 `DWORD` 负数的补码**，不是 -10 这种有符号值。
    const STD_INPUT_HANDLE: u32 = -10i32 as u32;
    const STD_OUTPUT_HANDLE: u32 = -11i32 as u32;
    const STD_ERROR_HANDLE: u32 = -12i32 as u32;

    /// 被临时改过的句柄，连同它**原来的 flags**。
    ///
    /// 存原值而不是"还原成 1"：原本就没有继承位的句柄，不该因为我们走过一趟
    /// 而多出来一位——那等于把一个问题换成了另一个问题。
    pub(super) struct Saved {
        std_id: u32,
        flags: u32,
    }

    /// 把三个标准句柄的继承位清掉，返回需要还原的那些。
    ///
    /// 不假设它们一定在、一定可继承：控制台被重定向过、句柄是别人给的，
    /// 两种都会让 `GetHandleInformation` 失败——失败就跳过，**不猜**。
    pub(super) fn strip_std_inherit() -> Vec<Saved> {
        let mut saved = Vec::new();
        for std_id in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
            // SAFETY: 下面全是"查标准句柄 + 查/改它的 flags"，参数都是普通整数；
            // 无效句柄由两个 API 各自用返回值告诉我们，不会解引用任何东西。
            unsafe {
                let h = GetStdHandle(std_id);
                if h.is_null() || h as isize == -1 {
                    continue;
                }
                let mut flags: u32 = 0;
                if GetHandleInformation(h, &mut flags) == 0 {
                    continue;
                }
                if flags & HANDLE_FLAG_INHERIT == 0 {
                    // 本来就不可继承——**没什么可摘的**（调用方没给我们管道，
                    // 或者它自己已经处理过了）。
                    continue;
                }
                if SetHandleInformation(h, HANDLE_FLAG_INHERIT, 0) != 0 {
                    saved.push(Saved { std_id, flags });
                }
            }
        }
        saved
    }

    /// 把 [`strip_std_inherit`] 摘掉的位还回去。
    ///
    /// **无论 spawn 成功还是失败都要还**（调用方就是这么调的）：
    /// 少还一位，这一次启动失败就会变成"后面所有 `Stdio::inherit()`
    /// 都拿不到 stdio"——一个只发生一次的故障，最难查。
    pub(super) fn restore_std_inherit(saved: &[Saved]) {
        for s in saved {
            // SAFETY: 同 `strip_std_inherit`；这里只把刚才记下来的原位写回去。
            unsafe {
                let h = GetStdHandle(s.std_id);
                let _ = SetHandleInformation(h, HANDLE_FLAG_INHERIT, s.flags & HANDLE_FLAG_INHERIT);
            }
        }
    }
}
