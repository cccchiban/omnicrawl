use std::ffi::c_void;
use std::io;
use std::mem::size_of;
use std::thread;
use windows_sys::Win32::Foundation::{
    CloseHandle, DuplicateHandle, SetHandleInformation,
    DUPLICATE_SAME_ACCESS, HANDLE, HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE,
    WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::Security::{
    SECURITY_ATTRIBUTES, SECURITY_CAPABILITIES,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, ReadFile, FILE_ATTRIBUTE_NORMAL,
    FILE_GENERIC_READ, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows_sys::Win32::System::JobObjects::TerminateJobObject;
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::Threading::{
    DeleteProcThreadAttributeList, GetCurrentProcess, GetExitCodeProcess,
    InitializeProcThreadAttributeList, TerminateProcess, UpdateProcThreadAttribute,
    WaitForSingleObject, LPPROC_THREAD_ATTRIBUTE_LIST,
    PROC_THREAD_ATTRIBUTE_HANDLE_LIST, PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES,
};

use super::base::*;

/// 独占一个 Win32 句柄，离开作用域即关闭。
///
/// `new` 对 `0` 与 `INVALID_HANDLE_VALUE`(-1) 都返回 `None`：Win32 的失败返回值不止 0，
/// 而 `-1` 混进 `PROC_THREAD_ATTRIBUTE_HANDLE_LIST` 会让 `CreateProcessW` 以
/// `ERROR_INVALID_PARAMETER`(87) 失败——那种报错离真正的原因（某个 `CreateFileW` /
/// `CreatePipe` 失败）太远，必须在源头就拦下。
pub(super) struct OwnedHandle(HANDLE);

/// 独立于 [`SandboxChild`] 的终止句柄。
///
/// `monitor` 把子进程交给等待线程，而停止路径在别的线程上——两者都要能终止它。
/// 因此这里复制一份进程句柄：各自关闭，互不影响。
pub struct Terminator(OwnedHandle);

/// 按 MS CRT 的规则给一个参数加引号。
///
/// 反斜杠只在「后面紧跟引号」或「位于参数末尾」时才需要翻倍，这是 `CommandLineToArgvW`
/// 的还原规则；不按这条规则拼，`C:\path\` 之类结尾带反斜杠的参数会吞掉收尾引号。
pub fn append_quoted(command_line: &mut String, argument: &str) {
    if !argument.is_empty() && !argument.contains([' ', '\t', '\n', '\x0b', '"']) {
        command_line.push_str(argument);
        return;
    }
    command_line.push('"');
    let mut backslashes = 0usize;
    for ch in argument.chars() {
        match ch {
            '\\' => backslashes += 1,
            '"' => {
                command_line.push_str(&"\\".repeat(backslashes * 2 + 1));
                command_line.push('"');
                backslashes = 0;
            }
            _ => {
                command_line.push_str(&"\\".repeat(backslashes));
                command_line.push(ch);
                backslashes = 0;
            }
        }
    }
    command_line.push_str(&"\\".repeat(backslashes * 2));
    command_line.push('"');
}

pub fn build_command_line(args: &[String]) -> Vec<u16> {
    let mut line = String::new();
    for (index, argument) in args.iter().enumerate() {
        if index > 0 {
            line.push(' ');
        }
        append_quoted(&mut line, argument);
    }
    wide(&line)
}

impl OwnedHandle {
    pub(super) fn new(handle: HANDLE) -> Option<Self> {
        let invalid = handle == 0 || handle == INVALID_HANDLE_VALUE;
        (!invalid).then_some(Self(handle))
    }

    pub(super) fn get(&self) -> HANDLE {
        self.0
    }

    pub(super) fn into_inner(self) -> HANDLE {
        let handle = self.0;
        std::mem::forget(self);
        handle
    }
}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if self.0 != 0 && self.0 != INVALID_HANDLE_VALUE {
            // SAFETY: 句柄由本类型独占持有，只在这里关闭一次。
            unsafe { CloseHandle(self.0) };
        }
    }
}

/// 一个以 AppContainer 身份运行的子进程。
///
/// 只提供 `tools/command.rs` 轮询循环需要的那几项能力，刻意不模仿 `std::process::Child`
/// 的完整接口。
pub struct SandboxChild {
    pub(super) process: OwnedHandle,
    pub(super) job: Option<OwnedHandle>,
    pub(super) pid: u32,
    pub(super) exited: Option<u32>,
}

impl SandboxChild {
    pub fn id(&self) -> u32 {
        self.pid
    }

    /// 进程句柄（只读借用）：供 [`Terminator`] 复制一份独立可终止引用。
    pub(super) fn process_handle(&self) -> HANDLE {
        self.process.get()
    }

    /// 已退出时返回退出码。
    pub fn try_wait(&mut self) -> Option<u32> {
        if let Some(code) = self.exited {
            return Some(code);
        }
        // SAFETY: 句柄有效；只读进程状态。
        let waited = unsafe { WaitForSingleObject(self.process.get(), 0) };
        if waited == WAIT_TIMEOUT {
            return None;
        }
        if waited != WAIT_OBJECT_0 {
            return None;
        }
        let mut code: u32 = 0;
        // SAFETY: 同上，`code` 是本地有效输出。
        let ok = unsafe { GetExitCodeProcess(self.process.get(), &mut code) };
        let code = if ok != 0 { code } else { u32::MAX };
        // `STILL_ACTIVE` 是「信号量刚释放但退出码还没落定」的窗口，下一轮再读。
        if code == STILL_ACTIVE {
            return None;
        }
        self.exited = Some(code);
        Some(code)
    }

    /// 终止整棵进程树：Job 句柄存在时一次回收全部后代。
    pub fn kill(&mut self) {
        if let Some(job) = self.job.as_ref() {
            // SAFETY: 句柄有效；`1` 是给整棵树的退出码。
            unsafe { TerminateJobObject(job.get(), 1) };
            return;
        }
        // SAFETY: 句柄有效；进程已退出时调用失败，忽略即可。
        unsafe { TerminateProcess(self.process.get(), 1) };
    }
}

impl Drop for SandboxChild {
    fn drop(&mut self) {
        // `OwnedHandle` 的析构顺序是字段顺序：process 先于 job 关闭。
        // Job 的 kill-on-close 只依赖 job 自己的句柄，与进程句柄无关，因此顺序无碍。
    }
}

/// 一对匿名管道：(读端, 写端)。读端不继承，写端交给子进程。
pub(super) fn pipe() -> Result<(OwnedHandle, OwnedHandle), String> {
    let attributes = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: std::ptr::null_mut(),
        bInheritHandle: 1,
    };
    let mut read: HANDLE = 0;
    let mut write: HANDLE = 0;
    // SAFETY: 两个输出指针都指向本地变量；`attributes` 在调用期间有效。
    let ok = unsafe { CreatePipe(&mut read, &mut write, &attributes, 0) };
    if ok == 0 {
        return Err(io_error("创建管道", last_error()));
    }
    let read = OwnedHandle::new(read).ok_or_else(|| "创建管道失败：读端句柄无效".to_string())?;
    let write = OwnedHandle::new(write).ok_or_else(|| "创建管道失败：写端句柄无效".to_string())?;
    // 读端只留在父进程，不能被子进程继承——否则父进程读不到 EOF（子进程自己也持着写端）。
    // SAFETY: 句柄有效；只改继承位。
    unsafe { SetHandleInformation(read.get(), HANDLE_FLAG_INHERIT, 0) };
    Ok((read, write))
}

/// 子进程的 stdin：接 `NUL`，避免交互式命令挂在等待输入上。
pub(super) fn null_input() -> Result<OwnedHandle, String> {
    let attributes = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: std::ptr::null_mut(),
        bInheritHandle: 1,
    };
    let name = wide("NUL");
    // SAFETY: 名字与属性在调用期间有效。
    let handle = unsafe {
        CreateFileW(
            name.as_ptr(),
            FILE_GENERIC_READ,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            &attributes,
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            0,
        )
    };
    OwnedHandle::new(handle).ok_or_else(|| io_error("打开 NUL", last_error()))
}

/// 在一个独立线程里读完整个管道。
/// 在一个独立线程里读完整个管道，**保留原始字节**。
///
/// 不做 `String::from_utf8_lossy`：沙盒里的程序按控制台代码页（中文系统 936）写字节，
/// 提前按 UTF-8 有损转换会把本来能还原的 GBK 字节变成替换字符，再解就救不回来了。
pub(super) fn spawn_pipe_reader(handle: HANDLE) -> thread::JoinHandle<Vec<u8>> {
    let handle = OwnedHandle::new(handle);
    thread::spawn(move || {
        let Some(handle) = handle else {
            return Vec::new();
        };
        let mut collected: Vec<u8> = Vec::new();
        let mut buffer = vec![0u8; 16 * 1024];
        loop {
            let mut read: u32 = 0;
            // SAFETY: 缓冲区大小与传入长度一致；句柄由本闭包独占。
            let ok = unsafe {
                ReadFile(
                    handle.get(),
                    buffer.as_mut_ptr(),
                    buffer.len() as u32,
                    &mut read,
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 || read == 0 {
                break;
            }
            collected.extend_from_slice(&buffer[..read as usize]);
        }
        collected
    })
}

/// 一次成功派生：子进程本体与两个后台读取线程。
pub struct Spawned {
    pub child: SandboxChild,
    pub stdout: thread::JoinHandle<Vec<u8>>,
    pub stderr: thread::JoinHandle<Vec<u8>>,
}

/// 一次流式派生：子进程本体加上交给调用方逐行读的输出。
pub struct Streamed {
    pub child: SandboxChild,
    pub stdout: std::fs::File,
    pub stderr: std::fs::File,
    pub terminator: Terminator,
}

impl Terminator {
    pub(super) fn duplicate(process: HANDLE) -> io::Result<Self> {
        let mut duplicate: HANDLE = 0;
        // SAFETY: `process` 有效；`duplicate` 是本地输出；伪句柄 -1 表示当前进程。
        let ok = unsafe {
            DuplicateHandle(
                GetCurrentProcess(),
                process,
                GetCurrentProcess(),
                &mut duplicate,
                0,
                0,
                DUPLICATE_SAME_ACCESS,
            )
        };
        if ok == 0 {
            return Err(other(io_error("复制进程句柄", last_error())));
        }
        OwnedHandle::new(duplicate)
            .map(Self)
            .ok_or_else(|| other("复制进程句柄失败：句柄无效".to_string()))
    }

    /// 终止进程（不等待退出）。
    pub fn kill(&self) {
        // SAFETY: 句柄由本类型独占持有；进程已退出时调用失败，忽略即可。
        unsafe { TerminateProcess(self.0.get(), 1) };
    }
}

/// `InitializeProcThreadAttributeList` 的缓冲区，按 `usize` 分配以满足指针对齐要求。
pub(super) struct AttributeList {
    storage: Vec<usize>,
    initialized: bool,
}

impl AttributeList {
    /// 构造「容器身份 + 显式句柄清单」两件套。
    ///
    /// 三处指针都必须活到 `CreateProcessW` 返回：属性列表只存指针，不复制内容。因此
    /// `capability` 由调用方持有，句柄清单在下面复制进自身，Job 句柄本身由 `SandboxChild` 活着。
    pub(super) fn new(capability: &SECURITY_CAPABILITIES, handles: &[HANDLE]) -> io::Result<Self> {
        let count = 2;
        let mut size: usize = 0;
        // SAFETY: 传空指针只查询所需字节数，这是该 API 的既定用法。
        unsafe { InitializeProcThreadAttributeList(std::ptr::null_mut(), count, 0, &mut size) };
        if size == 0 {
            return Err(other(io_error("查询属性列表大小", last_error())));
        }
        let slots = size.div_ceil(size_of::<usize>());
        let mut list = Self {
            storage: vec![0usize; slots],
            initialized: false,
        };
        let pointer = list.pointer();
        // SAFETY: 缓冲区按指针大小对齐，且字节数不小于查询值。
        let ok = unsafe { InitializeProcThreadAttributeList(pointer, count, 0, &mut size) };
        if ok == 0 {
            let code = last_error();
            if std::env::var("DBG_PIN").is_ok() {
                eprintln!("[dbg] InitializeProcThreadAttributeList count={count} slots={slots} failed code={code}");
            }
            return Err(other(io_error("初始化属性列表", code)));
        }
        list.initialized = true;

        // SAFETY: `capability` 由调用方持有到 `CreateProcessW` 之后。
        unsafe {
            list.set(
                PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES,
                capability as *const SECURITY_CAPABILITIES as *const c_void,
                size_of::<SECURITY_CAPABILITIES>(),
            )?;
            // 显式句柄清单：三个管道的**写端**。句柄值由系统复制进属性列表，
            // 因此 `handles` 本身在调用后即失效也没关系。
            list.set(
                PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
                handles.as_ptr() as *const c_void,
                std::mem::size_of_val(handles),
            )?;
        }

        Ok(list)
    }

    pub(super) fn pointer(&self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
        self.storage.as_ptr() as LPPROC_THREAD_ATTRIBUTE_LIST
    }

    /// # Safety
    /// `value` 必须在首次 `CreateProcessW` 之前保持有效；`size` 必须与它指向的类型一致。
    unsafe fn set(&mut self, attribute: u32, value: *const c_void, size: usize) -> io::Result<()> {
        let ok = UpdateProcThreadAttribute(
            self.pointer(),
            0,
            attribute as usize,
            value,
            size,
            std::ptr::null_mut(),
            std::ptr::null(),
        );
        if ok == 0 {
            let code = last_error();
            if std::env::var("DBG_PIN").is_ok() {
                eprintln!("[dbg] UpdateProcThreadAttribute attr=0x{attribute:X} size={size} failed code={code}");
            }
            return Err(other(io_error(&format!("设置属性 0x{attribute:X}"), code)));
        }
        Ok(())
    }
}

impl Drop for AttributeList {
    fn drop(&mut self) {
        if self.initialized {
            // SAFETY: 只对初始化成功的列表调用一次。
            unsafe { DeleteProcThreadAttributeList(self.pointer()) };
        }
    }
}
