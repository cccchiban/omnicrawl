//! 跨平台进程树控制辅助（对应 `omnicrawl/workspace/process_control.py`）。
//!
//! Windows 用 Job Object 的 Kill-On-Job-Close，保证后台任务及其子进程在句柄关闭时被回收；
//! 非 Windows 用进程组（`killpg`）。`taskkill` 在受限桌面会话里可能没有足够权限，即使目标是
//! 当前进程启动的子进程；Job Object 直接使用当前进程持有的句柄，不依赖按 PID 重新查找。
//!
//! 内核侧不依赖 `Popen`：这套动作定义在 [`ManagedProcess`] 上，宿主进程与测试替身走同一份
//! 回收逻辑。与 Python 的差异（见 crate `README.md`）：
//!
//! - Rust 标准库只有 `Child::kill`（Windows 是 `TerminateProcess`、Unix 是 `SIGKILL`），没有
//!   `terminate()` 的 SIGTERM 语义，因此 `terminate()` / `kill()` 都映射到强杀；
//! - Python 非 Windows 分支的第二次 `wait(timeout=5)` 超时会抛 `TimeoutExpired`，Rust 侧没有
//!   异常面，超时后直接返回（此时 `killpg` 已发出，进程通常立即退出）。

use std::process::Child;
use std::time::{Duration, Instant};

/// `JobObjectExtendedLimitInformation` 的信息类别编号。
pub const JOB_OBJECT_EXTENDED_LIMIT_INFORMATION_CLASS: i32 = 9;
/// 关闭 Job 句柄时终止其中所有进程。
pub const JOB_OBJECT_LIMIT_KILL_ON_CLOSE: u32 = 0x0000_2000;
/// 等待进程退出时的轮询间隔。
pub const WAIT_POLL_SECONDS: f64 = 0.05;

/// 进程状态（对映 Python `poll() -> int | None`，并区分「被信号终止」）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessState {
    Running,
    /// 正常退出（含退出码 0）。
    Exited(i32),
    /// 被信号终止 / 无退出码（Python 的 `poll()` 是负数）。
    Signalled,
}

/// 进程树控制需要的最小进程面。
pub trait ManagedProcess: Send {
    /// 进程 ID（对映 `process.pid`）。
    fn process_id(&self) -> u32;
    /// 非阻塞取状态（对映 `process.poll()`）。
    fn state(&mut self) -> ProcessState;
    /// 请求终止（对映 `process.terminate()`）。
    fn terminate(&mut self);
    /// 强杀（对映 `process.kill()`）。
    fn kill(&mut self);
    /// Windows 原生进程句柄（可用于 Job Object）；其他平台或不可用返回 `None`。
    fn raw_process_handle(&self) -> Option<isize>;
}

impl ManagedProcess for Child {
    fn process_id(&self) -> u32 {
        self.id()
    }

    fn state(&mut self) -> ProcessState {
        match self.try_wait() {
            Ok(Some(status)) => match status.code() {
                Some(code) => ProcessState::Exited(code),
                None => ProcessState::Signalled,
            },
            _ => ProcessState::Running,
        }
    }

    fn terminate(&mut self) {
        let _ = self.kill();
    }

    fn kill(&mut self) {
        let _ = self.kill();
    }

    #[cfg(windows)]
    fn raw_process_handle(&self) -> Option<isize> {
        use std::os::windows::io::AsRawHandle;

        Some(self.as_raw_handle() as isize)
    }

    #[cfg(not(windows))]
    fn raw_process_handle(&self) -> Option<isize> {
        None
    }
}

/// 在时限内轮询等待进程退出；返回是否已退出。
pub fn wait_with_timeout(process: &mut dyn ManagedProcess, seconds: f64) -> bool {
    let deadline = Instant::now() + Duration::from_secs_f64(seconds.max(0.0));
    loop {
        if process.state() != ProcessState::Running {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_secs_f64(WAIT_POLL_SECONDS));
    }
}

/// 把后台任务树纳入 Job Object，关闭句柄时强制回收；失败返回 `None`。
pub fn assign_process_to_kill_on_close_job(process: &dyn ManagedProcess) -> Option<isize> {
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, SetInformationJobObject,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        };

        let handle = process.raw_process_handle()?;
        // 任何一步失败都返回 None（Python 同样把 AttributeError / OSError 收敛成 None）。
        unsafe {
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if job == 0 {
                return None;
            }
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_CLOSE;
            let configured = SetInformationJobObject(
                job,
                JOB_OBJECT_EXTENDED_LIMIT_INFORMATION_CLASS,
                std::ptr::addr_of!(info).cast(),
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            );
            let assigned = configured != 0 && AssignProcessToJobObject(job, handle) != 0;
            if assigned {
                return Some(job);
            }
            CloseHandle(job);
            None
        }
    }
    #[cfg(not(windows))]
    {
        let _ = process;
        None
    }
}

/// 关闭 Windows 句柄；非 Windows 或 `None` 时是空操作。
pub fn close_windows_handle(handle: Option<isize>) {
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::CloseHandle;

        let Some(handle) = handle else {
            return;
        };
        unsafe {
            CloseHandle(handle);
        }
    }
    #[cfg(not(windows))]
    {
        let _ = handle;
    }
}

/// 强制终止进程及其子进程树。
///
/// `wait=false` 专供当前回合 ESC 的关闭回调：Job Object / 进程组的终止请求发出后立即返回，
/// 不把 UI 取消路径变成另一个阻塞工具。正常 timeout / stop / close 路径保留 `wait=true`，
/// 以便调用方得到稳定的终态。
pub fn terminate_process_tree(
    process: &mut dyn ManagedProcess,
    job_handle: Option<isize>,
    wait: bool,
) {
    if process.state() != ProcessState::Running {
        if job_handle.is_some() {
            close_windows_handle(job_handle);
        }
        return;
    }

    #[cfg(windows)]
    {
        match job_handle {
            // Kill-on-close 会递归终止 Job 中的所有后代进程。
            Some(handle) => close_windows_handle(Some(handle)),
            None => {
                if !taskkill_tree(process.process_id(), wait) {
                    process.terminate();
                }
            }
        }
        if wait && !wait_with_timeout(process, 5.0) {
            process.kill();
            let _ = wait_with_timeout(process, 5.0);
        }
    }

    #[cfg(unix)]
    {
        let _ = job_handle;
        if !kill_process_group(process.process_id()) {
            process.kill();
        }
        if wait && process.state() == ProcessState::Running {
            let _ = wait_with_timeout(process, 5.0);
        }
    }

    #[cfg(not(any(windows, unix)))]
    {
        let _ = job_handle;
        process.kill();
        if wait {
            let _ = wait_with_timeout(process, 5.0);
        }
    }
}

#[cfg(windows)]
fn taskkill_tree(pid: u32, wait: bool) -> bool {
    use std::process::{Command, Stdio};

    let mut command = Command::new("taskkill");
    command
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let Ok(mut child) = command.spawn() else {
        return false;
    };
    // Python 侧是 `subprocess.run(..., timeout=5 if wait else 1)`：超时就当失败，
    // 让调用方退回 terminate()。
    let seconds = if wait { 5.0 } else { 1.0 };
    if !wait_with_timeout(&mut child, seconds) {
        let _ = child.kill();
        return false;
    }
    matches!(child.state(), ProcessState::Exited(0))
}

#[cfg(unix)]
fn kill_process_group(pid: u32) -> bool {
    unsafe {
        let group = libc::getpgid(pid as libc::pid_t);
        if group < 0 {
            return false;
        }
        libc::killpg(group, libc::SIGKILL) == 0
    }
}

/// 判断进程是否仍在运行（对映 Python `os.kill(pid, 0)` / Win32 探测）。
///
/// PID 可能被操作系统复用，存在极小概率误判；对「同一平台只跑一个实例」的判定来说，
/// 即使误判也只是「认为已有实例在跑」而跳过启动，不会破坏数据，安全侧可接受。
pub fn pid_is_running(pid: i64) -> bool {
    if pid <= 0 {
        return false;
    }
    #[cfg(windows)]
    {
        /// `STILL_ACTIVE`（0x103）：仅此退出码表示进程仍在运行。
        const STILL_ACTIVE: u32 = 0x103;

        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Threading::{
            GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };

        unsafe {
            let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid as u32);
            if handle == 0 {
                return false;
            }
            let mut exit_code: u32 = 0;
            let ok = GetExitCodeProcess(handle, &mut exit_code) != 0;
            CloseHandle(handle);
            ok && exit_code == STILL_ACTIVE
        }
    }
    #[cfg(unix)]
    {
        unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
    }
    #[cfg(not(any(windows, unix)))]
    {
        let _ = pid;
        false
    }
}
