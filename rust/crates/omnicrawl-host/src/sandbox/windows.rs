//! Windows 沙盒执行体（P1）：AppContainer 身份 + Job Object 限额 + 裸 `CreateProcessW`。
//!
//! 设计见 `rust/assets/docs/sandbox_windows_design.md` §4.2.1、§6、§8。这里只做执行，
//! 「该不该拦」的判定仍由 [`super::Sandbox`] 负责——两者分工与 P0 一致。
//!
//! 三条约束决定了本模块的形状：
//!
//! 1. **不能用 `CreateProcessAsUserW`**：它要求 `SE_INCREASE_QUOTA_NAME` /
//!    `SE_ASSIGNPRIMARYTOKEN_NAME`，普通交互式进程会以 `ERROR_PRIVILEGE_NOT_HELD` 失败。
//!    正确做法是用 `PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES` 修饰一次普通的
//!    `CreateProcessW`（零特权）。
//! 2. **`std::process::Child` 无法从 `PROCESS_INFORMATION` 构造**（字段私有），所以本模块
//!    自持 `hProcess`/`hThread`/管道句柄，提供 [`SandboxChild`]；`tools/command.rs` 的轮询
//!    循环对两种子进程共用。
//! 3. **AppContainer 是受限令牌**：访问检查要求常规 SID 链与受限 SID 链（只含 package SID
//!    与能力 SID）**同时**放行。因此不仅要给工作区授权，还要给通往它的每一级父目录加
//!    遍历权——否则工作区的 ACE 根本走不到。这是真机上最容易踩空的一处。

use std::ffi::c_void;
use std::io;
use std::mem::size_of;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;

use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, LocalFree, SetHandleInformation, ERROR_ALREADY_EXISTS, HANDLE,
    HANDLE_FLAG_INHERIT, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::Security::Authorization::{
    GetEffectiveRightsFromAclW, GetNamedSecurityInfoW, SetEntriesInAclW, SetNamedSecurityInfoW,
    EXPLICIT_ACCESS_W, GRANT_ACCESS, NO_MULTIPLE_TRUSTEE, REVOKE_ACCESS, SE_FILE_OBJECT,
    TRUSTEE_IS_SID, TRUSTEE_IS_UNKNOWN, TRUSTEE_W,
};
use windows_sys::Win32::Security::Isolation::{
    CreateAppContainerProfile, DeriveAppContainerSidFromAppContainerName,
};
use windows_sys::Win32::Security::{
    FreeSid, DACL_SECURITY_INFORMATION, SECURITY_ATTRIBUTES, SECURITY_CAPABILITIES,
    SUB_CONTAINERS_AND_OBJECTS_INHERIT,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, ReadFile, FILE_ATTRIBUTE_NORMAL, FILE_GENERIC_EXECUTE, FILE_GENERIC_READ,
    FILE_GENERIC_WRITE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectCpuRateControlInformation,
    JobObjectExtendedLimitInformation, SetInformationJobObject, TerminateJobObject,
    JOBOBJECT_CPU_RATE_CONTROL_INFORMATION, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_CPU_RATE_CONTROL_ENABLE, JOB_OBJECT_CPU_RATE_CONTROL_HARD_CAP,
    JOB_OBJECT_LIMIT_ACTIVE_PROCESS, JOB_OBJECT_LIMIT_JOB_MEMORY,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::Threading::{
    CreateProcessW, DeleteProcThreadAttributeList, GetExitCodeProcess,
    InitializeProcThreadAttributeList, TerminateProcess, UpdateProcThreadAttribute,
    WaitForSingleObject, CREATE_NO_WINDOW, CREATE_UNICODE_ENVIRONMENT,
    EXTENDED_STARTUPINFO_PRESENT, LPPROC_THREAD_ATTRIBUTE_LIST, PROCESS_INFORMATION,
    PROC_THREAD_ATTRIBUTE_HANDLE_LIST, PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES,
    STARTF_USESTDHANDLES, STARTUPINFOEXW, STARTUPINFOW,
};

use omnicrawl_config::features::sandbox::SandboxLimits;

use super::Sandbox;

/// `STILL_ACTIVE`：仅此退出码表示进程仍在运行。
const STILL_ACTIVE: u32 = 0x103;

// 目录遍历只需要这三项：能穿过、能读属性、能同步。**不含** `FILE_LIST_DIRECTORY`，
// 因此加了这条 ACE 的父目录仍然无法被沙盒列举内容。
const TRAVERSE_ONLY: u32 = 0x0000_0020 | 0x0000_0080 | 0x0010_0000;

/// 读写执行：工作区与沙盒临时目录用这一组。
const FULL_CONTROL_FOR_SANDBOX: u32 = FILE_GENERIC_READ | FILE_GENERIC_WRITE | FILE_GENERIC_EXECUTE;

/// 只读执行：解释器目录等「必须能跑但不能改」的位置。
const READ_EXECUTE: u32 = FILE_GENERIC_READ | FILE_GENERIC_EXECUTE;

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

fn last_error() -> u32 {
    // SAFETY: 无参数、无副作用。
    unsafe { GetLastError() }
}

fn io_error(action: &str, code: u32) -> String {
    format!(
        "{action}失败（错误码 {code}）：{}",
        std::io::Error::from_raw_os_error(code as i32)
    )
}

// ── 容器身份 ─────────────────────────────────────────────────

/// 一个 AppContainer profile 及其 package SID。
///
/// 名字里带工作区哈希：不同工作区用不同容器，因此给某个工作区加的 ACE 不会顺带把
/// 另一个工作区暴露给本条沙盒。
pub struct Container {
    name: String,
    sid: *mut c_void,
}

impl Container {
    /// 取或建容器。已存在时只派生 SID，不改任何既有设置。
    pub fn ensure(name: &str) -> Result<Self, String> {
        let name_wide = wide(name);
        let display = wide("OmniCrawl 沙盒");
        let description = wide("OmniCrawl 以 AppContainer 身份执行的命令");

        // 不给能力：P1 的网络边界就是「完全不能出网」（回环代理是 P3）。
        let mut sid: *mut c_void = std::ptr::null_mut();
        // SAFETY: 三个字符串指针在调用期间有效；capabilities 为 0 个时可以为空。
        let created = unsafe {
            CreateAppContainerProfile(
                name_wide.as_ptr(),
                display.as_ptr(),
                description.as_ptr(),
                std::ptr::null(),
                0,
                &mut sid,
            )
        };
        if created >= 0 && !sid.is_null() {
            return Ok(Self {
                name: name.to_string(),
                sid,
            });
        }

        // 已存在是常态（第二次启动起），改为派生同一个 SID。
        let already_exists = created == (0x8007_0000u32 | ERROR_ALREADY_EXISTS) as i32;
        if !already_exists && !sid.is_null() {
            // 其他失败也可能已经填了 SID：以 SID 为准，泄漏前先释放。
            // SAFETY: SID 由 userenv 分配，FreeSid 是配对的释放方式。
            unsafe { FreeSid(sid) };
        }
        let mut derived: *mut c_void = std::ptr::null_mut();
        // SAFETY: 同上，名字指针在调用期间有效。
        let result =
            unsafe { DeriveAppContainerSidFromAppContainerName(name_wide.as_ptr(), &mut derived) };
        if result < 0 || derived.is_null() {
            return Err(format!(
                "无法建立 AppContainer 容器 `{name}`（HRESULT 0x{created:08X}/0x{result:08X}）"
            ));
        }
        Ok(Self {
            name: name.to_string(),
            sid: derived,
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    fn sid(&self) -> *mut c_void {
        self.sid
    }
}

impl Drop for Container {
    fn drop(&mut self) {
        if !self.sid.is_null() {
            // SAFETY: SID 由 `CreateAppContainerProfile` / `Derive…` 分配，配对的释放是 FreeSid。
            unsafe { FreeSid(self.sid) };
        }
    }
}

// SAFETY: `Container` 只持有一个 SID 指针，SID 是进程范围内的只读内存，跨线程共享安全。
unsafe impl Send for Container {}
unsafe impl Sync for Container {}

// ── ACL 授予 ─────────────────────────────────────────────────

/// 给 `path` 加一条针对容器 SID 的 ACE。
///
/// `inherit` 为真时对目录做可继承授予（子项自动继承），文件用 `false`。
pub fn grant(path: &Path, sid: *mut c_void, access: u32, inherit: bool) -> Result<(), String> {
    edit_ace(path, sid, access, inherit, GRANT_ACCESS)
}

/// 撤掉针对容器 SID 的 ACE（测试与将来退出清理用）。
pub fn revoke(path: &Path, sid: *mut c_void) -> Result<(), String> {
    edit_ace(path, sid, 0, false, REVOKE_ACCESS)
}

fn edit_ace(
    path: &Path,
    sid: *mut c_void,
    access: u32,
    inherit: bool,
    mode: i32,
) -> Result<(), String> {
    let wide_path = wide(&path.to_string_lossy());
    let mut old_dacl: *mut windows_sys::Win32::Security::ACL = std::ptr::null_mut();
    let mut descriptor: *mut c_void = std::ptr::null_mut();

    // SAFETY: 输出指针都指向本地变量；`descriptor` 由系统分配，最后用 LocalFree 归还。
    let status = unsafe {
        GetNamedSecurityInfoW(
            wide_path.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut old_dacl,
            std::ptr::null_mut(),
            &mut descriptor,
        )
    };
    if status != 0 {
        return Err(io_error(&format!("读取 {} 的 ACL", path.display()), status));
    }

    let entry = EXPLICIT_ACCESS_W {
        grfAccessPermissions: access,
        grfAccessMode: mode,
        grfInheritance: if inherit {
            SUB_CONTAINERS_AND_OBJECTS_INHERIT
        } else {
            0
        },
        Trustee: TRUSTEE_W {
            pMultipleTrustee: std::ptr::null_mut(),
            MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
            TrusteeForm: TRUSTEE_IS_SID,
            TrusteeType: TRUSTEE_IS_UNKNOWN,
            // 这个字段在 TRUSTEE_IS_SID 下承载的就是 SID 指针本身（不是字符串）。
            ptstrName: sid as *mut u16,
        },
    };

    let mut new_dacl: *mut windows_sys::Win32::Security::ACL = std::ptr::null_mut();
    // SAFETY: 单条 entry，old_dacl 可空；new_dacl 由系统分配。
    let status = unsafe { SetEntriesInAclW(1, &entry, old_dacl, &mut new_dacl) };
    if status != 0 {
        // SAFETY: descriptor 由 GetNamedSecurityInfoW 分配，尚未被别的路径释放。
        unsafe { LocalFree(descriptor) };
        return Err(io_error(&format!("构造 {} 的 ACL", path.display()), status));
    }

    // SAFETY: new_dacl 有效；其余输出参数为空表示"不改"。
    let status = unsafe {
        SetNamedSecurityInfoW(
            wide_path.as_ptr() as *mut u16,
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            new_dacl,
            std::ptr::null_mut(),
        )
    };
    // SAFETY: 两块内存都由系统分配、都由本函数负责归还，且各自归还一次。
    unsafe {
        LocalFree(new_dacl as *mut c_void);
        LocalFree(descriptor);
    }
    if status != 0 {
        return Err(io_error(&format!("写入 {} 的 ACL", path.display()), status));
    }
    Ok(())
}

/// 给通往 `target` 的每一级父目录补「只遍历」权。
///
/// AppContainer 是受限令牌：受限 SID 链里只有 package SID，用户自己的 ACE 一律不参与
/// 访问检查。所以即便工作区自己授了权，父目录没有 package SID 的遍历权时，进程也到不了
/// 工作区——表现为「路径不存在」而不是「拒绝访问」，极易误判。
///
/// **先查再写**：只有当前权限不够遍历时才改 DACL。这一条是可用性要求，不是优化——
/// 给卷根（`D:\`）写可继承 ACE 会触发全卷继承传播，实测一次 108 秒。多数系统的卷根
/// 已经给了 `Authenticated Users`/`Everyone` 遍历权，本来就够用。
fn grant_ancestors(target: &Path, sid: *mut c_void, granted: &mut Vec<PathBuf>) {
    let mut current = target.parent();
    while let Some(directory) = current {
        if directory.as_os_str().is_empty() {
            break;
        }
        if !granted.iter().any(|entry| entry == directory) {
            if !can_traverse(directory, sid) {
                // 失败不致命：卷根等位置可能不允许改 DACL，而它多半已经可遍历。
                let _ = grant(directory, sid, TRAVERSE_ONLY, false);
            }
            granted.push(directory.to_path_buf());
        }
        current = directory.parent();
    }
}

/// 容器身份对 `path` 是否已有遍历权（不写任何东西）。
fn can_traverse(path: &Path, sid: *mut c_void) -> bool {
    let wide_path = wide(&path.to_string_lossy());
    let mut dacl: *mut windows_sys::Win32::Security::ACL = std::ptr::null_mut();
    let mut descriptor: *mut c_void = std::ptr::null_mut();
    // SAFETY: 输出指针都指向本地变量；`descriptor` 由系统分配，末尾用 LocalFree 归还。
    let status = unsafe {
        GetNamedSecurityInfoW(
            wide_path.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut dacl,
            std::ptr::null_mut(),
            &mut descriptor,
        )
    };
    if status != 0 {
        return false;
    }
    let trustee = TRUSTEE_W {
        pMultipleTrustee: std::ptr::null_mut(),
        MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
        TrusteeForm: TRUSTEE_IS_SID,
        TrusteeType: TRUSTEE_IS_UNKNOWN,
        ptstrName: sid as *mut u16,
    };
    let mut rights: u32 = 0;
    // SAFETY: dacl 与 trustee 在调用期间有效；只读查询。
    let check = unsafe { GetEffectiveRightsFromAclW(dacl, &trustee, &mut rights) };
    // SAFETY: descriptor 由 GetNamedSecurityInfoW 分配，这里归还一次。
    unsafe { LocalFree(descriptor) };
    if check != 0 {
        return false;
    }
    // 只需「能穿过」：`FILE_TRAVERSE`(0x20) 与 `FILE_READ_ATTRIBUTES`(0x80)。
    // 注意 `SYNCHRONIZE`(0x100000) 也常一并出现，但它不是遍历的必要条件。
    rights & 0x0000_0020 != 0 && rights & 0x0000_0080 != 0
}

// ── 环境块 ───────────────────────────────────────────────────

/// 重建一份最小环境：**不继承宿主环境**，避免把 `OMNICRAWL_*` 之类的凭据带进沙盒。
///
/// `TEMP`/`TMP`/`USERPROFILE`/`HOME` 全部改指向沙盒专用目录：否则子进程往真实 `%TEMP%`
/// 写临时文件会 ACL 被拒，而报错位置离真正原因很远。
pub fn debug_environment(temp_dir: &Path, workspace: &Path) -> Vec<u16> {
    build_environment(temp_dir, workspace)
}

fn build_environment(temp_dir: &Path, workspace: &Path) -> Vec<u16> {
    let temp = temp_dir.to_string_lossy().to_string();
    let home = temp_dir.join("home").to_string_lossy().to_string();
    let workspace = workspace.to_string_lossy().to_string();

    let mut vars: Vec<(String, String)> = Vec::new();
    let host = |name: &str| {
        std::env::var(name)
            .ok()
            .filter(|value| !value.trim().is_empty())
    };
    for name in [
        "PATH",
        "PATHEXT",
        "SYSTEMROOT",
        "SYSTEMDRIVE",
        "WINDIR",
        "COMSPEC",
        "PROGRAMFILES",
        "PROGRAMFILES(X86)",
        "PROGRAMW6432",
        "PROGRAMDATA",
        "NUMBER_OF_PROCESSORS",
        "PROCESSOR_ARCHITECTURE",
        "OS",
        // 这一项**不能少**：AppContainer profile 落在 `%LOCALAPPDATA%\Packages`，
        // `CreateProcessW` 从调用方环境解析它；缺了就报 `ERROR_ENVVAR_NOT_FOUND`(203)，
        // 一个完全不指向真实原因的错误码（设计文档 §16 记录了这次排查）。
        "LOCALAPPDATA",
        "APPDATA",
        // PowerShell 靠它找 cmdlet 与模块。**不能原样继承**：宿主的值里常含用户目录下的
        // 路径（沙盒读不到），而 pwsh 在模块目录都解析不了时会连内置 cmdlet 都加载失败。
        // 处理见下方 `sandboxed_module_path`。
        "USERNAME",
        "USERDOMAIN",
        // 控制台代码页与区域：缺了之后中文输出变成乱码，`whoami` 之类的系统程序
        // 还会在 DLL 初始化阶段以 0xC0000142 退出。
        "windir",
        "COMPUTERNAME",
        "PROCESSOR_IDENTIFIER",
        "PROCESSOR_LEVEL",
        "PROCESSOR_REVISION",
        "TZ",
    ] {
        if let Some(value) = host(name) {
            vars.push((name.to_string(), value));
        }
    }

    vars.push(("TEMP".to_string(), temp.clone()));
    vars.push(("TMP".to_string(), temp));
    // `USERPROFILE` 指向沙盒内的家目录：真实用户目录读不到，留着真实路径只会让程序
    // 拿到一个打不开的位置后行为诡异（`whoami` 曾因此以 0xC0000142 崩溃）。
    vars.push(("USERPROFILE".to_string(), home.clone()));
    vars.push(("HOME".to_string(), home));
    vars.push(("HOMEDRIVE".to_string(), "C:".to_string()));
    vars.push(("HOMEPATH".to_string(), "\\".to_string()));
    // PowerShell 的 `~\` 与部分工具从这两个变量取当前目录。
    vars.push(("PWD".to_string(), workspace.clone()));
    vars.push(("OMNICRAWL_SANDBOX".to_string(), "1".to_string()));
    // 沙盒进程的输出不走控制台，而是管道；代码页仍是 OEM（中文系统上是 936），
    // 按 UTF-8 解会是乱码。`PYTHONUTF8` 让 Python 自己吐 UTF-8，其余程序由
    // `tools/command.rs` 在读取时按 OEM 代码页解码。
    vars.push(("PYTHONUTF8".to_string(), "1".to_string()));
    vars.push(("LANG".to_string(), "C.UTF-8".to_string()));
    vars.push(("LC_ALL".to_string(), "C.UTF-8".to_string()));
    vars.push(("PYTHONIOENCODING".to_string(), "utf-8".to_string()));
    vars.push(("PSModulePath".to_string(), sandboxed_module_path()));

    // 缓存目录必须在可写范围内，否则 npm/pip 每次都要重下（设计 §14.3）。
    vars.push((
        "npm_config_cache".to_string(),
        format!("{workspace}\\.omnicrawl\\.agent_tmp\\npm"),
    ));
    vars.push((
        "PIP_CACHE_DIR".to_string(),
        format!("{workspace}\\.omnicrawl\\.agent_tmp\\pip"),
    ));

    let mut block: Vec<u16> = Vec::new();
    for (key, value) in vars {
        block.extend(wide(&format!("{key}={value}")));
    }
    block.push(0);
    block
}

/// 沙盒内可用的 `PSModulePath`：**剔除用户目录下的路径**。
///
/// pwsh 启动时会逐个解析模块目录；只要存在一个打不开的项，它就把整份模块搜索路径
/// 判为不可用，连 `Write-Output` 这类内置 cmdlet 都加载不出来（真机实测现象）。
/// 系统目录下的模块对容器可读（`ALL APPLICATION PACKAGES: RX`），保留它们即可。
fn sandboxed_module_path() -> String {
    let host = std::env::var("PSModulePath").unwrap_or_default();
    let kept: Vec<String> = std::env::split_paths(&host)
        .map(|path| path.to_string_lossy().to_string())
        .filter(|path| !path.trim().is_empty() && is_system_path(path))
        .collect();
    if kept.is_empty() {
        // 宿主没有可用值时给出系统默认，避免空串让 pwsh 彻底找不到模块。
        let root = std::env::var("SystemRoot").unwrap_or_else(|_| String::from("C:/Windows"));
        return format!("{root}/system32/WindowsPowerShell/v1.0/Modules");
    }
    kept.join(";")
}

/// 路径是否在系统范围内（含 `C:\Program Files` 与 `SystemRoot`）。
fn is_system_path(path: &str) -> bool {
    let lower = path.to_lowercase();
    let system_root = std::env::var("SystemRoot")
        .unwrap_or_else(|_| String::from("C:/Windows"))
        .to_lowercase();
    if lower.starts_with(&system_root) {
        return true;
    }
    ["program files", "programdata"]
        .iter()
        .any(|prefix| lower.starts_with(prefix))
}

// ── 命令行拼装 ───────────────────────────────────────────────

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

fn build_command_line(args: &[String]) -> Vec<u16> {
    let mut line = String::new();
    for (index, argument) in args.iter().enumerate() {
        if index > 0 {
            line.push(' ');
        }
        append_quoted(&mut line, argument);
    }
    wide(&line)
}

// ── 句柄 ─────────────────────────────────────────────────────

/// 独占一个 Win32 句柄，离开作用域即关闭。
struct OwnedHandle(HANDLE);

impl OwnedHandle {
    fn new(handle: HANDLE) -> Option<Self> {
        (handle != 0).then_some(Self(handle))
    }

    fn get(&self) -> HANDLE {
        self.0
    }

    fn into_inner(self) -> HANDLE {
        let handle = self.0;
        std::mem::forget(self);
        handle
    }
}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if self.0 != 0 {
            // SAFETY: 句柄由本类型独占持有，只在这里关闭一次。
            unsafe { CloseHandle(self.0) };
        }
    }
}

// ── 子进程 ───────────────────────────────────────────────────

/// 一个以 AppContainer 身份运行的子进程。
///
/// 只提供 `tools/command.rs` 轮询循环需要的那几项能力，刻意不模仿 `std::process::Child`
/// 的完整接口。
pub struct SandboxChild {
    process: OwnedHandle,
    job: Option<OwnedHandle>,
    pid: u32,
    exited: Option<u32>,
}

impl SandboxChild {
    pub fn id(&self) -> u32 {
        self.pid
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

// ── 管道 ─────────────────────────────────────────────────────

/// 一对匿名管道：(读端, 写端)。读端不继承，写端交给子进程。
fn pipe() -> Result<(OwnedHandle, OwnedHandle), String> {
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
fn null_input() -> Result<OwnedHandle, String> {
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
fn spawn_pipe_reader(handle: HANDLE) -> thread::JoinHandle<Vec<u8>> {
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

// ── 隔离环境 ─────────────────────────────────────────────────

/// 一条沙盒命令所需的全部上下文：容器身份、专用目录与限额。
pub struct Isolation {
    container: Container,
    temp: PathBuf,
    workspace: PathBuf,
    limits: SandboxLimits,
}

impl Isolation {
    /// 准备容器与目录，并给工作区、沙盒临时目录与配置里额外声明的路径授权。
    ///
    /// 任何一步失败都返回错误——调用方据此**拒绝执行**，不放行到宿主身份（失败即关闭）。
    pub fn prepare(sandbox: &Sandbox) -> Result<Arc<Self>, String> {
        let workspace = sandbox.root().to_path_buf();
        let temp = sandbox.sandbox_temp();
        std::fs::create_dir_all(temp.join("home"))
            .map_err(|error| format!("无法创建沙盒临时目录 {}：{error}", temp.display()))?;

        let name = container_name(&workspace);
        let container = Container::ensure(&name)?;
        let sid = container.sid();

        let mut granted: Vec<PathBuf> = Vec::new();
        for path in [&workspace, &temp] {
            grant_ancestors(path, sid, &mut granted);
            grant_ancestors(&temp.join("home"), sid, &mut granted);
            grant(path, sid, FULL_CONTROL_FOR_SANDBOX, true)
                .map_err(|error| format!("无法给 {} 授予沙盒访问权：{error}", path.display()))?;
        }

        let config = sandbox.config();
        for entry in config.writable.iter().chain(config.readable.iter()) {
            let resolved = if Path::new(entry).is_absolute() {
                PathBuf::from(entry)
            } else {
                workspace.join(entry)
            };
            if !resolved.exists() {
                continue;
            }
            let access = if config.writable.contains(entry) {
                FULL_CONTROL_FOR_SANDBOX
            } else {
                READ_EXECUTE
            };
            grant_ancestors(&resolved, sid, &mut granted);
            grant(&resolved, sid, access, resolved.is_dir()).map_err(|error| {
                format!("无法给 {} 授予沙盒访问权：{error}", resolved.display())
            })?;
        }

        Ok(Arc::new(Self {
            container,
            temp,
            workspace,
            limits: config.limits,
        }))
    }

    pub fn container_name(&self) -> &str {
        self.container.name()
    }

    /// 以容器身份拉起 `args`（`args[0]` 是可执行文件路径）。
    ///
    /// 返回的 [`SandboxChild`] 已经归属 Job：后代无法逃离，宿主崩溃时由操作系统递归回收。
    pub fn spawn(&self, args: &[String]) -> io::Result<Spawned> {
        if args.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "缺少可执行文件",
            ));
        }
        let executable = wide(&args[0]);
        let mut command_line = build_command_line(args);
        let directory = wide(&self.workspace.to_string_lossy());
        let mut environment = build_environment(&self.temp, &self.workspace);

        let (stdout_read, stdout_write) = pipe().map_err(other)?;
        let (stderr_read, stderr_write) = pipe().map_err(other)?;
        let stdin = null_input().map_err(other)?;
        {
            let dbg = std::env::var("DBG_PIN").is_ok();
            if dbg {
                eprintln!(
                    "[dbg] handles in={} out={} err={} stdout_r={}",
                    stdin.get(),
                    stdout_write.get(),
                    stderr_write.get(),
                    stdout_read.get()
                );
            }
        }

        let job = self.create_job().map_err(other)?;
        // 能力结构体必须活到 `CreateProcessW` 返回：属性列表只存指针，不复制内容。
        let capability = SECURITY_CAPABILITIES {
            AppContainerSid: self.container.sid(),
            Capabilities: std::ptr::null_mut(),
            CapabilityCount: 0,
            Reserved: 0,
        };
        let attributes = AttributeList::new(
            &capability,
            &[stdin.get(), stdout_write.get(), stderr_write.get()],
        )?;

        let mut info: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
        // 必须是 `sizeof(STARTUPINFOEXW)`：传 104（`STARTUPINFOW` 的大小）时系统会认为
        // 扩展区不在可读范围内，直接以 `ERROR_INVALID_PARAMETER` 失败。
        info.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
        info.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        info.StartupInfo.hStdInput = stdin.get();
        info.StartupInfo.hStdOutput = stdout_write.get();
        info.StartupInfo.hStdError = stderr_write.get();
        info.lpAttributeList = attributes.pointer();

        let mut process_info: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: 所有指针在 `CreateProcessW` 期间有效（`capability` 与 `attributes` 都在本栈帧）。
        // 必须把整个 `STARTUPINFOEXW` 传进去——属性列表在扩展区的 `lpAttributeList`，
        // 只传首字段 `StartupInfo` 会让扩展信息整份失效（进程会以宿主身份起来）。
        let created = unsafe {
            CreateProcessW(
                executable.as_ptr(),
                command_line.as_mut_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                1,
                EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT | CREATE_NO_WINDOW,
                environment.as_mut_ptr() as *const c_void,
                directory.as_ptr(),
                &info as *const STARTUPINFOEXW as *const STARTUPINFOW,
                &mut process_info,
            )
        };
        // 子进程已持有自己的副本，父进程的三个句柄可以立刻关闭。
        // SAFETY: 三个句柄都由本函数独占，各关闭一次。
        unsafe {
            CloseHandle(stdin.into_inner());
            CloseHandle(stdout_write.into_inner());
            CloseHandle(stderr_write.into_inner());
        }
        if created == 0 {
            return Err(io::Error::from_raw_os_error(last_error() as i32));
        }

        // 线程句柄用不上；进程句柄与 Job 由 `SandboxChild` 持有。
        // SAFETY: 两个句柄都刚由 CreateProcessW 输出，各关闭一次。
        unsafe { CloseHandle(process_info.hThread) };
        let process = OwnedHandle::new(process_info.hProcess)
            .ok_or_else(|| io::Error::other("CreateProcessW 未返回进程句柄"))?;

        // 实测 `PROC_THREAD_ATTRIBUTE_JOB_LIST` 会让 `CreateProcessW` 以
        // `ERROR_INVALID_HANDLE` 失败，因此统一改为创建后纳入。风险窗口是创建到 `Assign`
        // 之间的几微秒：此时进程已在容器里（读不到用户目录），只是尚未受 Job 限额约束。
        if let Some(job) = job.as_ref() {
            // SAFETY: 两个句柄均有效。
            let assigned = unsafe { AssignProcessToJobObject(job.get(), process.get()) };
            if assigned == 0 {
                let code = last_error();
                // 起得来但管不住的后代比不隔离更危险：直接拒绝这条命令。
                // SAFETY: 进程已创建，终止后句柄仍归本函数关闭。
                unsafe { TerminateProcess(process.get(), 1) };
                return Err(other(io_error("把沙盒进程纳入 Job", code)));
            }
        }

        Ok(Spawned {
            child: SandboxChild {
                process,
                job,
                pid: process_info.dwProcessId,
                exited: None,
            },
            stdout: spawn_pipe_reader(stdout_read.into_inner()),
            stderr: spawn_pipe_reader(stderr_read.into_inner()),
        })
    }

    fn create_job(&self) -> Result<Option<OwnedHandle>, String> {
        // SAFETY: 名字与属性都为空，得到匿名 Job。
        let raw = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        let Some(job) = OwnedHandle::new(raw) else {
            return Err(io_error("创建 Job 对象", last_error()));
        };

        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        if self.limits.max_processes > 0 {
            info.BasicLimitInformation.LimitFlags |= JOB_OBJECT_LIMIT_ACTIVE_PROCESS;
            info.BasicLimitInformation.ActiveProcessLimit = self.limits.max_processes;
        }
        if self.limits.max_memory_mb > 0 {
            info.BasicLimitInformation.LimitFlags |= JOB_OBJECT_LIMIT_JOB_MEMORY;
            info.JobMemoryLimit = (self.limits.max_memory_mb as usize) * 1024 * 1024;
        }
        // SAFETY: 结构体按 Win32 定义零值初始化后逐字段赋值；大小按类型给出。
        let ok = unsafe {
            SetInformationJobObject(
                job.get(),
                JobObjectExtendedLimitInformation,
                (&info as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast::<c_void>(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if ok == 0 {
            return Err(io_error("设置 Job 限额", last_error()));
        }

        if self.limits.cpu_rate_percent > 0 {
            let mut rate: JOBOBJECT_CPU_RATE_CONTROL_INFORMATION = unsafe { std::mem::zeroed() };
            rate.ControlFlags =
                JOB_OBJECT_CPU_RATE_CONTROL_ENABLE | JOB_OBJECT_CPU_RATE_CONTROL_HARD_CAP;
            // 单位是 1/100 百分比：50% → 5000。
            rate.Anonymous.CpuRate = self.limits.cpu_rate_percent.min(100) * 100;
            // SAFETY: 同上。CPU 速率不被支持时（部分容器/虚拟化环境）不致命，忽略即可。
            unsafe {
                SetInformationJobObject(
                    job.get(),
                    JobObjectCpuRateControlInformation,
                    (&rate as *const JOBOBJECT_CPU_RATE_CONTROL_INFORMATION).cast::<c_void>(),
                    size_of::<JOBOBJECT_CPU_RATE_CONTROL_INFORMATION>() as u32,
                )
            };
        }
        Ok(Some(job))
    }
}

/// 一次成功派生：子进程本体与两个后台读取线程。
pub struct Spawned {
    pub child: SandboxChild,
    pub stdout: thread::JoinHandle<Vec<u8>>,
    pub stderr: thread::JoinHandle<Vec<u8>>,
}

fn other(message: String) -> io::Error {
    io::Error::other(message)
}

// ── 属性列表 ─────────────────────────────────────────────────

/// `InitializeProcThreadAttributeList` 的缓冲区，按 `usize` 分配以满足指针对齐要求。
struct AttributeList {
    storage: Vec<usize>,
    initialized: bool,
}

impl AttributeList {
    /// 构造「容器身份 + 显式句柄清单」两件套。
    ///
    /// 三处指针都必须活到 `CreateProcessW` 返回：属性列表只存指针，不复制内容。因此
    /// `capability` 由调用方持有，句柄清单在下面复制进自身，Job 句柄本身由 `SandboxChild` 活着。
    fn new(capability: &SECURITY_CAPABILITIES, handles: &[HANDLE]) -> io::Result<Self> {
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

    fn pointer(&self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
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

/// 容器名：按「用户 + 工作区」派生，保证同一工作区复用同一身份、不同工作区互不可见。
fn container_name(workspace: &Path) -> String {
    fn hash(text: &str) -> u64 {
        // FNV-1a：只用来取稳定短标识，不做安全用途。
        let mut value: u64 = 0xcbf2_9ce4_8422_2325;
        for byte in text.as_bytes() {
            value ^= *byte as u64;
            value = value.wrapping_mul(0x0000_0100_0000_01B3);
        }
        value
    }
    let user = std::env::var("USERNAME").unwrap_or_else(|_| "user".to_string());
    let user = user.to_lowercase();
    format!(
        "omnicrawl-sandbox-{}-{:016x}",
        hash(&user) & 0xffff_ffff,
        hash(&workspace.to_string_lossy().to_lowercase())
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_arguments_are_not_quoted() {
        let mut line = String::new();
        append_quoted(&mut line, r"C:\Windows\System32\cmd.exe");
        assert_eq!(line, r"C:\Windows\System32\cmd.exe");
    }

    #[test]
    fn arguments_with_spaces_are_quoted() {
        let mut line = String::new();
        append_quoted(&mut line, r"D:\my project\a.txt");
        assert_eq!(line, r#""D:\my project\a.txt""#);
    }

    #[test]
    fn trailing_backslash_does_not_escape_the_closing_quote() {
        let mut line = String::new();
        append_quoted(&mut line, r"D:\my project\");
        // 结尾反斜杠必须翻倍，否则收尾引号被转义、参数被吞进下一段。
        assert_eq!(line, r#""D:\my project\\""#);
    }

    #[test]
    fn embedded_quotes_are_escaped() {
        let mut line = String::new();
        append_quoted(&mut line, r#"a"b"#);
        assert_eq!(line, r#""a\"b""#);
    }

    #[test]
    fn command_line_joins_arguments_with_spaces() {
        let line = build_command_line(&[
            "powershell.exe".to_string(),
            "-Command".to_string(),
            "echo hi".to_string(),
        ]);
        let text: String = line
            .iter()
            .take_while(|unit| **unit != 0)
            .map(|unit| char::from_u32(*unit as u32).unwrap_or('?'))
            .collect();
        assert_eq!(text, r#"powershell.exe -Command "echo hi""#);
    }

    #[test]
    fn environment_is_rebuilt_without_host_secrets() {
        let block = build_environment(
            Path::new(r"D:\work\.omnicrawl\.agent_tmp\sandbox"),
            Path::new(r"D:\work"),
        );
        let text = String::from_utf16_lossy(&block);
        // 临时目录必须指进沙盒内，否则子进程往真实 %TEMP% 写会被 ACL 拒。
        assert!(
            text.contains(r"D:\work\.omnicrawl\.agent_tmp\sandbox"),
            "{text}"
        );
        assert!(text.contains("OMNICRAWL_SANDBOX=1"), "{text}");
        // 不继承宿主环境：任何 OMNICRAWL_* 凭据都不该出现。
        assert!(!text.contains("OMNICRAWL_TOKEN"), "{text}");
        assert!(!text.contains("OMNICRAWL_API_KEY"), "{text}");
        // 环境块以双 NUL 结束。
        assert_eq!(block.last(), Some(&0));
        assert_eq!(block.get(block.len().saturating_sub(2)), Some(&0));
    }

    #[test]
    fn container_names_differ_per_workspace() {
        let first = container_name(Path::new(r"D:\work\a"));
        let second = container_name(Path::new(r"D:\work\b"));
        assert_ne!(first, second);
        assert!(first.starts_with("omnicrawl-sandbox-"), "{first}");
        // 大小写不同是同一个工作区，应复用同一容器。
        assert_eq!(first, container_name(Path::new(r"D:\WORK\A")));
    }
}
