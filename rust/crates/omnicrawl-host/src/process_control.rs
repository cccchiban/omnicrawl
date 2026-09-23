//! 跨平台进程树控制辅助（对映 Python `omnicrawl/workspace/process_control.py`）。
//!
//! 两条互补的回收路径：
//!
//! - **Windows**：把后台命令树纳入 Job Object 的 Kill-On-Job-Close，句柄无论
//!   正常关闭还是宿主崩溃时由操作系统关闭，Job 内的全部后代都被递归终止。
//!   `taskkill /T /F` 只作为拿不到 Job 时的退路——它在受限桌面会话里可能没有
//!   足够权限，而且要求按 PID 重新查找进程。
//! - **Unix**：让子进程自成一个进程组（`process_group(0)`，等价 `setsid` 的
//!   会话隔离效果），回收时对整组发 `SIGKILL`（`kill(-pgid, SIGKILL)`），
//!   因此 `bash -c '… & …'` 再拉起的后代也一起被回收，不止直接子进程。
//!
//! Unix 侧直接声明 `kill` / `getpgid` 的 C 符号，不引入 `libc` 依赖：这两个
//! 函数由所有 Unix C 运行时无条件提供，与 Windows 侧声明 `windows-sys` 的用法
//! 保持同一风格。

use std::process::Child;

#[cfg(windows)]
mod platform {
    use std::ffi::c_void;
    use std::mem::size_of;
    use std::process::{Child, Command};

    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE,
    };

    /// Windows 没有 Unix 的 session / process group 语义；`CREATE_NEW_PROCESS_GROUP`
    /// 只用于让 CTRL 事件不广播到宿主，递归回收由 Job Object 负责。
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    /// kill-on-close 的 Job 对象。
    ///
    /// 句柄无论以哪种方式关闭（正常返回、`Drop`、宿主崩溃时由操作系统关闭），
    /// Job 内的全部后代进程都会被递归终止。
    pub struct KillOnCloseJob {
        handle: HANDLE,
    }

    impl KillOnCloseJob {
        /// 创建 Job 并把子进程纳入；环境不支持时返回 `None`，调用方退回按 PID 回收。
        ///
        /// 失败是正常情形之一（例如当前进程已在不允许嵌套的 Job 里），不是错误。
        pub fn assign(child: &Child) -> Option<Self> {
            // SAFETY: 句柄只在本类型内传递与关闭；结构体按 Win32 定义零值初始化。
            unsafe {
                let handle = CreateJobObjectW(std::ptr::null(), std::ptr::null());
                if handle == 0 {
                    return None;
                }

                let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
                info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                let configured = SetInformationJobObject(
                    handle,
                    JobObjectExtendedLimitInformation,
                    (&info as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast::<c_void>(),
                    size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                );
                if configured == 0 {
                    CloseHandle(handle);
                    return None;
                }

                // `Child` 只暴露 PID，而 Job 要进程句柄；纳入成功后立刻还掉这个句柄，
                // 进程本身已由 Job 持有引用。
                let process = OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, 0, child.id());
                if process == 0 {
                    CloseHandle(handle);
                    return None;
                }
                let assigned = AssignProcessToJobObject(handle, process);
                CloseHandle(process);
                if assigned == 0 {
                    CloseHandle(handle);
                    return None;
                }

                Some(Self { handle })
            }
        }

        /// 关闭句柄：递归终止 Job 内的全部进程。等价于 `drop`，用于显式表达意图。
        pub fn close(self) {}
    }

    impl Drop for KillOnCloseJob {
        fn drop(&mut self) {
            // SAFETY: 句柄由本类型独占持有，且只在这里关闭一次。
            unsafe {
                CloseHandle(self.handle);
            }
        }
    }

    /// 让子进程独立成一个进程组（Windows 下只影响控制台事件传播）。
    pub fn configure_process_group(command: &mut Command) {
        use std::os::windows::process::CommandExt;
        command.creation_flags(CREATE_NEW_PROCESS_GROUP);
    }

    /// 按 PID 回收整棵进程树：优先 `taskkill /T /F`（它按父链递归）。
    pub fn kill_process_tree_by_pid(pid: u32) {
        use std::os::windows::process::CommandExt;
        let _ = Command::new("taskkill")
            .args(["/T", "/F", "/PID", &pid.to_string()])
            .creation_flags(CREATE_NO_WINDOW)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }

    /// 只发出回收请求、不等它结束（`Esc` 取消路径用，不阻塞界面）。
    pub fn request_process_tree_kill(pid: u32) {
        use std::os::windows::process::CommandExt;
        // 不调用 `status()`：`taskkill` 的等待会把界面取消变成另一个阻塞操作。
        // 句柄不回收也没关系——它只是 taskkill 自己的进程对象，只需终态不要结果。
        let _ = Command::new("taskkill")
            .args(["/T", "/F", "/PID", &pid.to_string()])
            .creation_flags(CREATE_NO_WINDOW)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
    }

    /// 进程所属的进程组；Windows 用 Job Object 代替进程组语义，永远没有组可查。
    pub fn process_group_of(_pid: u32) -> Option<u32> {
        None
    }
}

#[cfg(unix)]
mod platform {
    use std::os::unix::process::CommandExt;
    use std::process::Command;

    /// 与 `signal.h` 同一组取值；只用来发终止信号，不解释信号语义。
    const SIGKILL: i32 = 9;

    extern "C" {
        /// POSIX `kill(2)`：`pid` 为负时表示「向进程组 `-pid` 发信号」。
        fn kill(pid: i32, sig: i32) -> i32;
        /// POSIX `getpgid(2)`：查进程所属的进程组。
        fn getpgid(pid: i32) -> i32;
    }

    /// Unix 没有 Job Object：纳入始终失败，调用方退回进程组回收。
    pub struct KillOnCloseJob;

    impl KillOnCloseJob {
        pub fn assign(_child: &std::process::Child) -> Option<Self> {
            None
        }

        pub fn close(self) {}
    }

    /// 让子进程独立成一个进程组：`pgid == pid`，于是 `kill(-pid)` 只作用于它和它的后代。
    ///
    /// 用 `process_group(0)` 而不是 `setsid`：这里只要「自成一组」，不需要脱离控制
    /// 终端（需要脱离的是连接器子进程，它另走 `start_new_session`）。
    pub fn configure_process_group(command: &mut Command) {
        command.process_group(0);
    }

    /// 按 PID 回收整棵进程树：对子进程所在的进程组发 `SIGKILL`。
    ///
    /// 子进程创建时已用 [`configure_process_group`] 自成一组，`pgid == pid`，
    /// 因此整组收回覆盖 `bash -c '… & …'` 拉起的全部后代；拿不到组（旧代码路径、
    /// 或进程已被回收）时退回只杀该 PID。
    pub fn kill_process_tree_by_pid(pid: u32) {
        let pid = pid as i32;
        // SAFETY: 两个调用都只传本进程有权操作的 pid；返回值只用来判断是否退路。
        unsafe {
            let group = getpgid(pid);
            let target = if group > 0 { -group } else { -pid };
            if kill(target, SIGKILL) != 0 {
                kill(pid, SIGKILL);
            }
        }
    }

    /// 只发出回收请求；`killpg` 本身不阻塞，这里与同步版本实现相同，
    /// 保留独立入口是为了让调用点的意图（不等终态）显式可读。
    pub fn request_process_tree_kill(pid: u32) {
        kill_process_tree_by_pid(pid);
    }

    /// 进程所属的进程组（回收测试与诊断用）。
    pub fn process_group_of(pid: u32) -> Option<u32> {
        // SAFETY: `getpgid` 对无效 pid 返回 -1，不触碰其他内存。
        let group = unsafe { getpgid(pid as i32) };
        (group > 0).then_some(group as u32)
    }
}

pub use platform::KillOnCloseJob;
pub use platform::{
    configure_process_group, kill_process_tree_by_pid, process_group_of, request_process_tree_kill,
};

/// 回收一整棵进程树：纳入了 Job 时关句柄让它递归终止，否则按 PID（Unix 下按进程组）回收。
///
/// `wait` 的语义与 Python `terminate_process_tree` 一致：正常 timeout / stop / close
/// 路径用 `true` 拿到稳定终态；`Esc` 取消路径用 `false`，发完终止请求立刻返回，
/// 不让界面取消变成另一个阻塞操作。
pub fn terminate_process_tree(child: &mut Child, job: Option<KillOnCloseJob>, wait: bool) {
    match (job, wait) {
        // Job 句柄的关闭本身就是递归终止请求，且立刻返回。
        (Some(job), _) => job.close(),
        (None, true) => kill_process_tree_by_pid(child.id()),
        (None, false) => request_process_tree_kill(child.id()),
    }
    let _ = child.kill();
    if wait {
        let _ = child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    /// 等子进程退出；超时即失败。
    fn wait_until_exit(child: &mut Child, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if matches!(child.try_wait(), Ok(Some(_))) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        false
    }

    /// 关闭 Job 句柄必须真正终止其中已纳入的进程——这是「宿主崩溃也不留孤儿」的依据。
    #[test]
    #[cfg(windows)]
    fn closing_the_job_terminates_assigned_processes() {
        let mut child = Command::new("cmd")
            .args(["/C", "ping -n 60 127.0.0.1"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("启动测试子进程");

        let Some(job) = KillOnCloseJob::assign(&child) else {
            // 当前环境不允许把子进程纳入 Job（例如已在不可嵌套的 Job 中）。
            let _ = child.kill();
            let _ = child.wait();
            eprintln!("跳过：当前环境不支持分配子进程到 Job Object");
            return;
        };

        job.close();

        assert!(
            wait_until_exit(&mut child, Duration::from_secs(15)),
            "关闭 Job 句柄后子进程仍在运行，kill-on-close 没有生效"
        );
    }

    /// 分配失败不能是错误：拿不到 Job 时调用方必须能退回按 PID 回收。
    #[test]
    #[cfg(windows)]
    fn assign_reports_failure_without_panicking() {
        let mut child = Command::new("cmd")
            .args(["/C", "exit 0"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("启动测试子进程");
        let job = KillOnCloseJob::assign(&child);
        if let Some(job) = job {
            job.close();
        }
        let _ = child.wait();
    }

    /// Windows 退路：没有 Job 时 `taskkill /T /F` 仍要能收掉子进程。
    #[test]
    #[cfg(windows)]
    fn pid_reclaim_terminates_the_child() {
        let mut child = Command::new("cmd")
            .args(["/C", "ping -n 60 127.0.0.1"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("启动测试子进程");
        kill_process_tree_by_pid(child.id());
        assert!(
            wait_until_exit(&mut child, Duration::from_secs(15)),
            "按 PID 回收后子进程仍在运行"
        );
    }

    /// `configure_process_group` 必须让子进程自成一个进程组：这是 Unix 侧整组回收的前提。
    #[test]
    #[cfg(unix)]
    fn configured_child_leads_its_own_process_group() {
        let mut command = Command::new("sh");
        command
            .args(["-c", "sleep 30"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        configure_process_group(&mut command);
        let mut child = command.spawn().expect("启动测试子进程");
        assert_eq!(
            process_group_of(child.id()),
            Some(child.id()),
            "子进程应当是自身进程组的组长（pgid == pid）"
        );
        kill_process_tree_by_pid(child.id());
        assert!(wait_until_exit(&mut child, Duration::from_secs(10)));
    }

    /// Unix 整组回收必须连带收掉后代：只杀直接子进程会留下 `sleep` 孤儿。
    #[test]
    #[cfg(unix)]
    fn reclaim_covers_grandchildren() {
        let mut command = Command::new("sh");
        // 父进程拉起一个后台孙子进程后自己也退出；孙子进程会被 `sh` 释放到同一进程组。
        command
            .args(["-c", "sleep 30 & echo $!; sleep 30"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        configure_process_group(&mut command);
        let mut child = command.spawn().expect("启动测试子进程");
        let mut stdout = child.stdout.take().expect("管道 stdout");
        let mut text = String::new();
        {
            use std::io::Read;
            let _ = stdout.read_to_string(&mut text);
        }
        let grandchild: u32 = text
            .lines()
            .next()
            .and_then(|line| line.trim().parse().ok())
            .expect("孙子进程 PID");

        kill_process_tree_by_pid(child.id());
        assert!(wait_until_exit(&mut child, Duration::from_secs(10)));

        // 孙子进程也必须在同一组回收里终止（`kill -0` 探测存活）。
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let alive = Command::new("kill")
                .args(["-0", &grandchild.to_string()])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .map(|status| status.success())
                .unwrap_or(false);
            if !alive {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "孙子进程 {grandchild} 未被进程组回收"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// 预热一次进程创建：首次 `spawn` 在冷启动（DLL 加载 / 杀软扫描）下要一秒以上，
    /// 把它从被测的计时里排掉，否则断言量到的是环境噪声而不是「等不等终态」。
    #[cfg(windows)]
    fn warm_up_process_creation() {
        if let Ok(mut child) = Command::new("cmd")
            .args(["/C", "exit 0"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            let _ = child.wait();
        }
    }

    /// `wait=false` 路径不能阻塞调用方：发出终止请求就立刻返回，不等终态。
    #[test]
    #[cfg(windows)]
    fn terminate_without_wait_returns_immediately() {
        warm_up_process_creation();
        let mut child = Command::new("cmd")
            .args(["/C", "ping -n 60 127.0.0.1"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("启动测试子进程");
        let started = Instant::now();
        terminate_process_tree(&mut child, None, false);
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "非等待路径不应阻塞：{:?}",
            started.elapsed()
        );
        kill_process_tree_by_pid(child.id());
        let _ = child.wait();
    }
}
