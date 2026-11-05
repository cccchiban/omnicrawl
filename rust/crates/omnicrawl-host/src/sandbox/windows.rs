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
use std::os::windows::io::FromRawHandle;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;

use windows_sys::Win32::Foundation::PSID;
use windows_sys::Win32::Foundation::{
    CloseHandle, DuplicateHandle, GetLastError, LocalFree, SetHandleInformation,
    DUPLICATE_SAME_ACCESS, ERROR_ALREADY_EXISTS, HANDLE, HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE,
    WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::Security::Authorization::{
    GetNamedSecurityInfoW, SetEntriesInAclW, SetNamedSecurityInfoW, EXPLICIT_ACCESS_W,
    GRANT_ACCESS, NO_MULTIPLE_TRUSTEE, REVOKE_ACCESS, SE_FILE_OBJECT, TRUSTEE_IS_SID,
    TRUSTEE_IS_UNKNOWN, TRUSTEE_W,
};
use windows_sys::Win32::Security::Isolation::{
    CreateAppContainerProfile, DeleteAppContainerProfile, DeriveAppContainerSidFromAppContainerName,
};
use windows_sys::Win32::Security::{
    DeriveCapabilitySidsFromName, FreeSid, GetAce, ACE_HEADER, DACL_SECURITY_INFORMATION,
    SECURITY_ATTRIBUTES, SECURITY_CAPABILITIES, SID_AND_ATTRIBUTES,
    SUB_CONTAINERS_AND_OBJECTS_INHERIT,
};

// ACE 类型常量直接用数值：`Win32_System_SystemServices` 特性就为这两个常量引入太不划算。
// `ACCESS_ALLOWED_ACE_TYPE = 0`、`ACCESS_ALLOWED_OBJECT_ACE_TYPE = 5`（WinNT.h）。
const ACCESS_ALLOWED_ACE_TYPE_LOCAL: u8 = 0;
const ACCESS_ALLOWED_OBJECT_ACE_TYPE_LOCAL: u8 = 5;

use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, ReadFile, DELETE, FILE_ATTRIBUTE_NORMAL, FILE_DELETE_CHILD, FILE_GENERIC_EXECUTE,
    FILE_GENERIC_READ, FILE_GENERIC_WRITE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
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
    CreateProcessW, DeleteProcThreadAttributeList, GetCurrentProcess, GetExitCodeProcess,
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

/// 读写执行**加删除**：工作区与沙盒临时目录用这一组。
///
/// 必须显式带 `DELETE`(0x0001_0000) 与 `FILE_DELETE_CHILD`(0x0000_0040)：
/// `FILE_GENERIC_WRITE` 只覆盖「写数据、追加」，**不含删除**，于是目录能建、文件能写，
/// 却删不掉——`Remove-Item`、`rmdir /s`、`Directory.Delete` 一律报「访问被拒绝」
/// （实测：新建目录的容器 ACE 只有 `Write, ReadAndExecute, Synchronize`）。
/// 删不掉自己刚创建的构建产物是最典型的日常动作，缺这一项等于把工作区变成只增不减。
const FULL_CONTROL_FOR_SANDBOX: u32 =
    FILE_GENERIC_READ | FILE_GENERIC_WRITE | FILE_GENERIC_EXECUTE | DELETE | FILE_DELETE_CHILD;

/// 只读执行：解释器目录等「必须能跑但不能改」的位置。
const READ_EXECUTE: u32 = FILE_GENERIC_READ | FILE_GENERIC_EXECUTE;

/// 给解释器目录授权前允许的最大条目数。
///
/// `SetNamedSecurityInfoW` 的耗时随子树规模增长（实测 `C:\Program Files\nodejs` 约 0.3 秒 /
/// 0.29 万项，`D:\ProgramData\Anaconda3` 约 18 秒 / 22 万项）。1 万项大致对应「几百毫秒」，
/// 超过它就跳过并如实报告，不让启动悄悄卡住。
const GRANT_ENTRY_BUDGET: u64 = 10_000;

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

/// `CreateProcessW` 失败时的可读描述。
///
/// 87（`ERROR_INVALID_PARAMETER`）是这里最难自查的一类：它几乎总是**别的**参数登记出了
/// 问题，而不是命令本身。实测三种来源：`cb` 传了 `sizeof(STARTUPINFOW)` 而非
/// `STARTUPINFOEXW`、句柄清单里混进 `INVALID_HANDLE_VALUE`、清单中的句柄没有继承位。
/// 因此对 87 单独给出排查方向，省得再去猜。
fn describe_create_error(code: u32) -> io::Error {
    if code == 87 {
        return io::Error::other(
            "CreateProcessW 失败（错误码 87，参数错误）：扩展启动信息不完整——\
             `cb` 必须是 STARTUPINFOEXW 的大小、句柄清单里的句柄必须有效且可继承"
                .to_string(),
        );
    }
    io::Error::from_raw_os_error(code as i32)
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

        // 建容器时不给能力：容器在内核层出不了网，出网只能经宿主回环代理（P3）。
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
///
/// **可继承授予是昂贵的**：`SetNamedSecurityInfoW` 会把 ACE 立刻传播到整棵子树。
/// 实测 `D:\ProgramData\Anaconda3`（19.5 万文件 / 2.3 万目录）单次约 50 秒，
/// `C:\Program Files\nodejs` 约 39 秒。所以调用方必须先查 [`has_access`]，
/// 只在确实缺权时写——否则每次启动都要重付一次这个代价。
pub fn grant(path: &Path, sid: *mut c_void, access: u32, inherit: bool) -> Result<(), String> {
    edit_ace(path, sid, access, inherit, GRANT_ACCESS)
}

/// 容器身份对 `path` 是否已具备 `access` 所需的权限（不写任何东西）。
///
/// **不能用 `GetEffectiveRightsFromAclW`**：它对 AppContainer 的 package SID 会给出
/// 「有权限」的错误答案（实测 `C:\Program Files\nodejs` 返回可读，容器内实际是「拒绝访问」），
/// 因为该函数只按 ACE 计算位掩码，不考虑受限令牌的访问检查路径。
/// 这里直接遍历 DACL，找针对本 SID 的显式允许 ACE。
pub fn has_access(path: &Path, sid: *mut c_void, access: u32) -> bool {
    let Some((dacl, descriptor)) = read_dacl(path) else {
        return false;
    };
    let mut rights: u32 = 0;
    // SAFETY: dacl 由 GetNamedSecurityInfoW 返回，指向合法 ACL 结构；只读读取头两个字段。
    let count = unsafe { (*dacl).AceCount as u32 };
    for index in 0..count {
        let mut ace: *mut c_void = std::ptr::null_mut();
        // SAFETY: dacl 有效；ace 由系统借出，只在本次循环内使用。
        if unsafe { GetAce(dacl, index, &mut ace) } == 0 || ace.is_null() {
            continue;
        }
        // SAFETY: ace 指向一个 ACE_HEADER 起始的合法结构；先读 4 字节头判断类型。
        let header = unsafe { std::ptr::read_unaligned(ace as *const ACE_HEADER) };
        let is_allow = header.AceType == ACCESS_ALLOWED_ACE_TYPE_LOCAL
            || header.AceType == ACCESS_ALLOWED_OBJECT_ACE_TYPE_LOCAL;
        if !is_allow {
            continue;
        }
        // SAFETY: 类型已知是「允许」型 ACE，前 8 字节布局为
        // (ACE_HEADER 4B | Mask 4B | SidStart 4B…)，够读到 Mask 与 SID 起始。
        let allowed = unsafe { std::ptr::read_unaligned((ace as *const u8).add(4) as *const u32) };
        let ace_sid = unsafe { (ace as *const u8).add(8) as *mut c_void };
        // SAFETY: ace_sid 指向 ACE 内嵌的 SID；与容器 SID 比较是纯读取。
        if unsafe { equal_sid_in_ace(ace_sid, sid) } {
            rights |= allowed;
        }
    }
    // SAFETY: descriptor 由 GetNamedSecurityInfoW 分配，这里归还一次。
    unsafe { LocalFree(descriptor) };
    rights & access == access
}

/// 读一个对象的 DACL，返回（DACL 指针，需要由调用方 `LocalFree` 的描述符）。
fn read_dacl(path: &Path) -> Option<(*mut windows_sys::Win32::Security::ACL, *mut c_void)> {
    let wide_path = wide(&path.to_string_lossy());
    let mut dacl: *mut windows_sys::Win32::Security::ACL = std::ptr::null_mut();
    let mut descriptor: *mut c_void = std::ptr::null_mut();
    // SAFETY: 输出指针都指向本地变量；`descriptor` 由系统分配，调用方负责归还。
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
    if status != 0 || dacl.is_null() {
        if !descriptor.is_null() {
            // SAFETY: 上面刚分配，这里归还一次。
            unsafe { LocalFree(descriptor) };
        }
        return None;
    }
    Some((dacl, descriptor))
}

/// 比较 ACE 内嵌的 SID 与目标 SID 是否相同（纯读取）。
///
/// `EqualSid` 需要 `Win32_Security_Authentication_Identity` 特性；这里按 SID 结构自己比，
/// 避免为一个比较多引一层特性。
fn equal_sid_in_ace(ace_sid: *mut c_void, sid: *mut c_void) -> bool {
    if ace_sid.is_null() || sid.is_null() {
        return false;
    }
    // SAFETY: 两边都是合法 SID 指针（ace_sid 来自 DACL 内嵌，sid 来自容器）。
    unsafe {
        let left = ace_sid as *const u8;
        let right = sid as *const u8;
        let left_len = *left.add(1) as usize;
        let right_len = *right.add(1) as usize;
        if left_len != right_len || left_len == 0 {
            return false;
        }
        // Revision(1) + SubAuthorityCount(1) + IdentifierAuthority(6) + SubAuthority*n
        let total = 8 + left_len * 4;
        std::slice::from_raw_parts(left, total) == std::slice::from_raw_parts(right, total)
    }
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

/// 需要容器可遍历的系统盘根。
///
/// 目前只有系统盘（`%SystemDrive%`，即 `%SystemRoot%` 所在盘）：PowerShell 7 启动时要建立
/// 该盘的同名驱动器去加载自己的内置模块，缺这条 ACE 就整个 cmdlet 体系失效
/// （见 `Isolation::prepare` 里的论证）。其它盘不参与解释器启动，不必开口子。
fn system_drive_roots() -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();
    for key in ["SystemDrive", "SystemRoot"] {
        let Some(value) = std::env::var(key).ok().filter(|value| !value.trim().is_empty()) else {
            continue;
        };
        let path = PathBuf::from(value);
        // `SystemRoot` 是 `C:\Windows`，取到盘符根才是驱动器根。
        let root = if key == "SystemDrive" {
            path
        } else {
            match path.components().next() {
                Some(component) => PathBuf::from(component.as_os_str()).join("\\"),
                None => continue,
            }
        };
        if root.is_dir()
            && !roots
                .iter()
                .any(|entry| normalize_path(&entry.to_string_lossy()) == normalize_path(&root.to_string_lossy()))
        {
            roots.push(root);
        }
    }
    roots
}

/// 容器身份对 `path` 是否已有遍历权（不写任何东西）。
fn can_traverse(path: &Path, sid: *mut c_void) -> bool {
    // 只需「能穿过」：`FILE_TRAVERSE`(0x20) 与 `FILE_READ_ATTRIBUTES`(0x80)。
    // 用与 `has_access` 同一套显式 ACE 检查：`GetEffectiveRightsFromAclW` 对 AppContainer
    // 的 package SID 会给出错误答案（实测把不可读的目录判成可读）。
    has_access(path, sid, 0x0000_0020 | 0x0000_0080)
}

// ── 环境块 ───────────────────────────────────────────────────

/// 重建一份最小环境：**不继承宿主环境**，避免把 `OMNICRAWL_*` 之类的凭据带进沙盒。
///
/// `TEMP`/`TMP`/`USERPROFILE`/`HOME` 全部改指向沙盒专用目录：否则子进程往真实 `%TEMP%`
/// 写临时文件会 ACL 被拒，而报错位置离真正原因很远。
pub fn debug_environment(temp_dir: &Path, workspace: &Path) -> Vec<u16> {
    build_environment_with(temp_dir, workspace, &[], None)
}

fn build_environment(
    temp_dir: &Path,
    workspace: &Path,
    runtimes: &[super::runtime::RuntimeRoot],
) -> Vec<u16> {
    build_environment_with(temp_dir, workspace, runtimes, None)
}

/// 同 [`build_environment`]，但可注入出网代理。
///
/// `proxy` 是 `http://127.0.0.1:<port>`：注入 `HTTP_PROXY`/`HTTPS_PROXY` 后，
/// `curl`/`pip`/`npm`/`git http(s)`/`cargo`/`go mod` 都会走宿主代理，
/// 由它按 `[sandbox.net]` 判定。**不给值时也会清空代理变量**——不能让容器
/// 继承宿主的代理设置，否则出网绕过我们的判定。
fn build_environment_with(
    temp_dir: &Path,
    workspace: &Path,
    runtimes: &[super::runtime::RuntimeRoot],
    proxy: Option<&str>,
) -> Vec<u16> {
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
        // `windir` 已在上面（`WINDIR`）列过一次：Windows 变量名**不区分大小写**，
        // 重复项会让 PowerShell 的 `Env:` 驱动器报「已添加了具有相同键的项」，
        // 于是 `$env:FOO` 与 `Get-ChildItem Env:` 全部失效。
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
    // `PSModulePath` 必须补上 pwsh 自己的模块目录：PowerShell 7 的内置 cmdlet 是模块
    // （`Microsoft.PowerShell.*`），只在自己安装目录的 `Modules` 下找。宿主的值里那一项
    // 可能指向用户目录（容器读不到），于是 pwsh 起来了却连 `Write-Output` 都不认——
    // 症状是「脚本里每个命令都不存在」，与权限错误毫无相似之处（真机实测）。
    vars.push((
        "PSModulePath".to_string(),
        sandboxed_module_path(&powershell_module_roots()),
    ));
    // `PATH` 必须按**容器内实际可读**重建，不能继承宿主那份。
    // 宿主 PATH 里混着大量用户目录（`.cargo/bin`、`WindowsApps`、各类 SDK），容器读不到；
    // 更要紧的是 Windows 的「当前目录 + 每个 PATH 项」查找语义：只要排在前面的项不可访问，
    // `cmd` 起 `npm.cmd` 这类包装脚本就会在解析阶段失败，而报错说的是"不是内部或外部命令"，
    // 完全不指向真实原因（实测踩过）。
    vars.push(("PATH".to_string(), sandboxed_path(&workspace, runtimes)));

    // 缓存目录必须在可写范围内，否则 npm/pip 每次都要重下（设计 §14.3）。
    vars.push((
        "npm_config_cache".to_string(),
        format!("{workspace}\\.omnicrawl\\.agent_tmp\\npm"),
    ));
    vars.push((
        "PIP_CACHE_DIR".to_string(),
        format!("{workspace}\\.omnicrawl\\.agent_tmp\\pip"),
    ));
    // npm 的全局前缀默认是 `%APPDATA%\npm`，容器读不到。`npm.ps1` 会拿
    // `npm-prefix.js` 的输出做一次 `Test-Path`，对不可读路径直接抛
    // `Access is denied`——在 `$ErrorActionPreference = 'Stop'` 下这是一次**致命**错误，
    // 于是 `npm --version` 报的是 `Test-Path : Access is denied`，而不是版本号（实测）。
    // 指到工作区内既让这次探测有确定结果，也让 `npm i -g` 有可写落点。
    vars.push((
        "npm_config_prefix".to_string(),
        format!("{workspace}\\.omnicrawl\\.agent_tmp\\npm-global"),
    ));

    // 网络：容器没有 `internetClient`（内核层出不了网），唯一通路是宿主回环代理。
    // 注入这两个变量，让尊重代理的客户端（curl/pip/npm/git/cargo）走代理；
    // `NO_PROXY` 留给回环自身，避免访问本地 dev server 时被绕一圈。
    //
    // **不注入就是完全不注入**：宿主的代理变量一律不继承——否则容器可能直接
    // 打到用户配置的代理上去，我们的判定就被跳过了。
    if let Some(proxy) = proxy {
        vars.push(("HTTP_PROXY".to_string(), proxy.to_string()));
        vars.push(("HTTPS_PROXY".to_string(), proxy.to_string()));
        // 小写形式：部分工具（curl、git、pip）只认小写。
        vars.push(("http_proxy".to_string(), proxy.to_string()));
        vars.push(("https_proxy".to_string(), proxy.to_string()));
        vars.push((
            "NO_PROXY".to_string(),
            "127.0.0.1,localhost,::1".to_string(),
        ));
        vars.push((
            "no_proxy".to_string(),
            "127.0.0.1,localhost,::1".to_string(),
        ));
    }

    let mut block: Vec<u16> = Vec::new();
    for (key, value) in dedupe_vars(vars) {
        block.extend(wide(&format!("{key}={value}")));
    }
    block.push(0);
    block
}

/// 按**大小写不敏感**去重环境变量，保留首次出现的那条。
///
/// Windows 的环境块里 `WINDIR` 与 `windir` 是两个条目，但所有读取方（PowerShell 的
/// `Env:` 驱动器、.NET 的 `Environment`、大多数 CRT）都按大小写不敏感建字典，
/// 遇到同名两条就抛「已添加了具有相同键的项」——一条环境变量重复能让整个
/// `$env:FOO` 机制失效，报错还完全不指向重复项。
fn dedupe_vars(vars: Vec<(String, String)>) -> Vec<(String, String)> {
    let mut seen: Vec<String> = Vec::with_capacity(vars.len());
    let mut kept: Vec<(String, String)> = Vec::with_capacity(vars.len());
    for (key, value) in vars {
        let folded = key.to_ascii_uppercase();
        if seen.contains(&folded) {
            continue;
        }
        seen.push(folded);
        kept.push((key, value));
    }
    kept
}

/// 沙盒内可用的 `PATH`：只保留容器**确实能读**的目录。
///
/// 三类来源，顺序即查找优先级：
///
/// 1. 工作区自己的 `node_modules/.bin` 等（相对项按工作区解析）；
/// 2. 已授只读执行权的解释器目录（[`super::runtime`] 发现的那批）；
/// 3. 宿主 `PATH` 里的系统目录（`C:\Windows`、`Program Files`，容器默认可读）。
///
/// 不在这三类里的一律剔除：宿主 PATH 里的 `.cargo/bin`、`WindowsApps`、各类 SDK 都对容器
/// 不可访问，留着只会让程序查找在解析阶段失败（而且报错极具误导性）。
fn sandboxed_path(workspace: &str, runtimes: &[super::runtime::RuntimeRoot]) -> String {
    let mut entries: Vec<String> = Vec::new();
    let mut push = |value: &str| {
        let value = value.trim().trim_end_matches('\\');
        if value.is_empty() {
            return;
        }
        if entries
            .iter()
            .any(|entry| entry.eq_ignore_ascii_case(value))
        {
            return;
        }
        entries.push(value.to_string());
    };

    push(workspace);
    push(&format!("{workspace}\\.omnicrawl\\.agent_tmp\\npm"));
    // `npm i -g` 把可执行文件放进 prefix 目录（`build_environment` 指向这里），
    // 放进 PATH 才能被后续命令找到。
    push(&format!("{workspace}\\.omnicrawl\\.agent_tmp\\npm-global"));
    for runtime in runtimes {
        push(&runtime.path.to_string_lossy());
    }
    match std::env::var("PATH") {
        Ok(host) => {
            for entry in host.split(';') {
                if is_system_path(entry) {
                    push(entry);
                }
            }
        }
        Err(_) => {}
    }
    entries.join(";")
}

/// 沙盒内可用的 `PSModulePath`：**剔除用户目录下的路径**，并补上各候选解释器自带的模块目录。
///
/// pwsh 启动时会逐个解析模块目录；只要存在一个打不开的项，它就把整份模块搜索路径
/// 判为不可用，连 `Write-Output` 这类内置 cmdlet 都加载不出来（真机实测现象）。
/// 系统目录下的模块对容器可读（`ALL APPLICATION PACKAGES: RX`），保留它们即可。
///
/// 补解释器自己的 `Modules` 是**必要条件但非充分条件**：PowerShell 7 的内置 cmdlet 是
/// 模块（`Microsoft.PowerShell.*`），只在自己安装目录下找；实测容器内 `$PSHOME` 与
/// `PSModulePath` 都正确时它仍加载不出任何 cmdlet，最终由
/// [`Isolation::powershell`] 的实测探测决定是否回退到 5.1。
fn sandboxed_module_path(powershell_roots: &[PathBuf]) -> String {
    let host = std::env::var("PSModulePath").unwrap_or_default();
    let mut kept: Vec<String> = std::env::split_paths(&host)
        .map(|path| path.to_string_lossy().to_string())
        .filter(|path| !path.trim().is_empty() && is_system_path(path))
        .collect();
    for modules in powershell_roots {
        let text = modules.to_string_lossy().to_string();
        if !kept.iter().any(|entry| entry.eq_ignore_ascii_case(&text)) {
            kept.insert(0, text);
        }
    }
    if kept.is_empty() {
        // 宿主没有可用值时给出系统默认，避免空串让 pwsh 彻底找不到模块。
        let root = std::env::var("SystemRoot").unwrap_or_else(|_| String::from("C:/Windows"));
        return format!("{root}/system32/WindowsPowerShell/v1.0/Modules");
    }
    kept.join(";")
}

/// 各候选 PowerShell 自带的模块目录（存在才收）。
///
/// 环境块在**探测之前**就要组装好（探测本身也要用它），所以这里不区分「最终选中哪个」：
/// 把所有候选的模块目录都放进去，选中谁都能加载自己的 cmdlet。用不到的项多几个目录，
/// 代价是 pwsh 启动时多扫几处不存在的路径——比选错解释器安全得多。
fn powershell_module_roots() -> Vec<PathBuf> {
    let env = |name: &str| std::env::var(name).ok();
    super::runtime::powershell_candidates(&env)
        .iter()
        .filter_map(|executable| executable.parent())
        .map(|directory| directory.join("Modules"))
        .filter(|modules| modules.is_dir())
        .collect()
}

/// 路径是否在系统范围内（`SystemRoot`、`Program Files` 系、`ProgramData`）。
///
/// 按**路径组件**比较，不用字符串前缀：带盘符的路径以 `C:` 开头，`starts_with("program files")`
/// 永远不成立——这个写法曾经让「系统目录」判定事实上只覆盖 `SystemRoot`。
fn is_system_path(path: &str) -> bool {
    let candidate = normalize_path(path);
    if candidate.is_empty() {
        return false;
    }
    for name in [
        "SystemRoot",
        "ProgramFiles",
        "ProgramFiles(x86)",
        "ProgramW6432",
        "ProgramData",
        "windir",
    ] {
        let Ok(value) = std::env::var(name) else {
            continue;
        };
        let base = normalize_path(&value);
        if base.is_empty() {
            continue;
        }
        if candidate == base || candidate.starts_with(&format!("{base}\\")) {
            return true;
        }
    }
    false
}

/// 比较用路径：小写、正斜杠统一成反斜杠、去掉结尾分隔符。
fn normalize_path(path: &str) -> String {
    path.trim()
        .replace('/', "\\")
        .trim_end_matches('\\')
        .to_lowercase()
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
///
/// `new` 对 `0` 与 `INVALID_HANDLE_VALUE`(-1) 都返回 `None`：Win32 的失败返回值不止 0，
/// 而 `-1` 混进 `PROC_THREAD_ATTRIBUTE_HANDLE_LIST` 会让 `CreateProcessW` 以
/// `ERROR_INVALID_PARAMETER`(87) 失败——那种报错离真正的原因（某个 `CreateFileW` /
/// `CreatePipe` 失败）太远，必须在源头就拦下。
struct OwnedHandle(HANDLE);

impl OwnedHandle {
    fn new(handle: HANDLE) -> Option<Self> {
        let invalid = handle == 0 || handle == INVALID_HANDLE_VALUE;
        (!invalid).then_some(Self(handle))
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
        if self.0 != 0 && self.0 != INVALID_HANDLE_VALUE {
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

    /// 进程句柄（只读借用）：供 [`Terminator`] 复制一份独立可终止引用。
    fn process_handle(&self) -> HANDLE {
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

/// 准备解释器目录的可读性，返回（可用的、不可用的）。
///
/// **只做一次有效权限查询就结束**，不写 ACL、不扫描目录：
///
/// - 写 ACL 的代价随子树规模增长（`SetNamedSecurityInfoW` 改一次 DACL 就会重算整棵子树的
///   继承：`C:\Program Files\nodejs` 0.26 秒 / 0.29 万项，`D:\ProgramData\Anaconda3`
///   18.4 秒 / 22 万项），即便写入内容与原 DACL 完全相同也照付。
/// - 想"先看看目录有多大再决定"同样不行：全树遍历 22 万项本身就是几十秒。
///
/// 所以自动流程只**报告事实**：容器能读到的进 `PATH`，读不到的如实列进 `skipped`，
/// 让用户用 `[sandbox.fs] readable` 显式授权——那时代价由用户明确选择的目录承担，
/// 且 ACE 留在磁盘上，只需付一次。
///
/// `Program Files` 下的解释器默认带 `ALL APPLICATION PACKAGES: RX`，因此这是最常见的情形：
/// 零成本直接可用。
fn prepare_runtimes(
    sid: *mut c_void,
) -> (Vec<super::runtime::RuntimeRoot>, Vec<(PathBuf, String)>) {
    let mut available: Vec<super::runtime::RuntimeRoot> = Vec::new();
    let mut skipped: Vec<(PathBuf, String)> = Vec::new();
    for runtime in super::runtime::discover(&|name| std::env::var(name).ok()) {
        // 判「可用」必须看**目录里的可执行文件**，不能只看目录本身：
        // 目录上有 ACE、文件上没有时，容器能列目录却读不到 `node.exe`，
        // `cmd` 只会报「不是内部或外部命令」，把根因完全掩盖（实测踩过）。
        if probe_runtime(runtime.reason, &runtime.path, sid) {
            available.push(runtime);
        } else {
            skipped.push((
                runtime.path.clone(),
                "沙盒内读不到其中的可执行文件；如需使用，请把该目录加入 `[sandbox.fs] readable`"
                    .to_string(),
            ));
        }
    }
    (available, skipped)
}

/// 该解释器目录里的可执行文件是否真的能被容器读到。
///
/// 只看「至少有一个已知入口可读」——沙盒里要跑的就是这些入口。
fn probe_runtime(reason: &str, directory: &Path, sid: *mut c_void) -> bool {
    let candidates: &[&str] = match reason {
        "node" => &["node.exe"],
        "npm" => &["npm.cmd"],
        "python" => &["python.exe", "python3.exe"],
        _ => &[],
    };
    candidates.iter().any(|name| {
        let path = directory.join(name);
        path.is_file() && has_access(&path, sid, FILE_GENERIC_READ)
    })
}

/// 一条沙盒命令所需的全部上下文：容器身份、专用目录与限额。
pub struct Isolation {
    container: Container,
    temp: PathBuf,
    workspace: PathBuf,
    limits: SandboxLimits,
    runtimes: Vec<super::runtime::RuntimeRoot>,
    skipped: Vec<(PathBuf, String)>,
    /// 允许的网络能力名（空表示完全断网）。
    capabilities: Vec<String>,
    /// 已解析的能力 SID 与其底层数组：必须活到 `CreateProcessW` 返回。
    capability_owned: Vec<CapabilitySids>,
    /// 出网代理地址（`http://127.0.0.1:<port>`）：`None` 表示不注入，容器无出网通路。
    proxy_url: std::sync::Mutex<Option<String>>,
    /// 容器内实测可用的 PowerShell（`None` 表示尚未探测或一个都不通）。探测要起进程，
    /// 一次约 1–2 秒，因此结果缓存。
    powershell: std::sync::Mutex<Option<Option<PathBuf>>>,
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
        // `npm_config_prefix` 指到这里（见 `build_environment`）：先建出来，
        // 否则 `npm i -g` 要自己造目录树，而 `npm.ps1` 的前缀探测会先撞上不存在。
        let npm_global = workspace
            .join(".omnicrawl")
            .join(".agent_tmp")
            .join("npm-global");
        std::fs::create_dir_all(&npm_global)
            .map_err(|error| format!("无法创建 {}：{error}", npm_global.display()))?;

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
        // 系统盘根必须可遍历，否则 PowerShell 7 **一个 cmdlet 都加载不出来**。
        //
        // 根因是 pwsh 启动时要建立 `C:`（系统盘）驱动器：它用
        // `Directory.Exists("C:\\")` 探路，而 AppContainer 对没有 ACE 的卷根返回 False，
        // 于是以「The specified drive root "C:\" either does not exist」失败，`C:` 驱动器
        // 没建起来；接着它自己的内置模块（`Microsoft.PowerShell.*`）全部导入失败
        // （`Cannot find drive. A drive with the name 'C' does not exist.`），最终连
        // `Write-Output` 都报「无法识别」。Windows PowerShell 5.1 不需要这一步，所以同一
        // 容器里 5.1 一切正常——这正是「只有 pwsh 坏掉」的真正原因。
        //
        // 工作区在 D: 时 `grant_ancestors` 只覆盖 D: 那一支，系统盘根不在祖先链上，
        // 于是这条 ACE 必须单独补。只授遍历（`TRAVERSE_ONLY`，不可继承），代价极小：
        // 卷根本来就有大量同类 ACE，且这一条不改写继承。
        for root in system_drive_roots() {
            if !granted.iter().any(|entry| entry == &root) && !can_traverse(&root, sid) {
                // 失败不致命：容器内 PowerShell 会退回到 5.1，命令仍能跑。
                let _ = grant(&root, sid, TRAVERSE_ONLY, false);
            }
            granted.push(root);
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

        // 解释器根目录：`node`/`npm`/`python` 常装在用户目录下，容器默认读不到。
        //
        // 成本已实测：改一次 DACL 会让系统重算整棵子树的继承，`D:\ProgramData\Anaconda3`
        // （22 万项）单次 18.4 秒，`C:\Program Files\nodejs`（0.29 万项）0.26 秒。
        // 所以这里按规模分档：小的直接授权（代价可忽略），大的跳过并在 `/sandbox` 里
        // 如实报告——宁可让用户知道「这个解释器沙盒内不可用」，也不让启动默默卡 20 秒。
        // 已经授过的（容器 SID 有权限）零成本，直接进 PATH。
        let (runtimes, skipped) = prepare_runtimes(sid);

        Ok(Arc::new(Self {
            container,
            temp,
            workspace,
            limits: config.limits,
            runtimes,
            skipped,
            capabilities: Vec::new(),
            capability_owned: Vec::new(),
            proxy_url: std::sync::Mutex::new(None),
            powershell: std::sync::Mutex::new(None),
        }))
    }

    /// 设定容器可用的网络能力（空列表 = 完全断网）。
    ///
    /// 只有 `internetClient`/`internetClientServer` 两个名字有意义：AppContainer 的
    /// 出网权限就是能力 SID，没能力连回环都会被拒（实测超时）。因此「给网络」与
    /// 「给回环」是两件事：能出网靠能力，能连本机代理靠回环豁免。
    pub fn set_capabilities(&mut self, names: &[String]) -> Result<(), String> {
        let mut owned = Vec::with_capacity(names.len());
        for name in names {
            owned.push(capability_sid(name)?);
        }
        self.capabilities = names.to_vec();
        self.capability_owned = owned;
        Ok(())
    }

    /// 当前生效的网络能力名。
    pub fn capabilities(&self) -> &[String] {
        &self.capabilities
    }

    /// 设定出网代理地址（注入 `HTTP_PROXY` 等）。
    ///
    /// 传 `None` 表示**不注入任何代理变量**：容器既没有网络能力、也没有代理通路，
    /// 那就是彻底断网。这正是 `offline` 策略与「豁免失败」时的降级形态。
    pub fn set_proxy(&self, proxy_url: Option<String>) {
        let mut guard = self
            .proxy_url
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *guard = proxy_url;
    }

    /// 当前注入的代理地址。
    pub fn proxy_url(&self) -> Option<String> {
        self.proxy_url
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// 容器内可用的 PowerShell：候选按「7.* 最新 → 5.1」排序，逐个在容器里实测。
    ///
    /// 为什么必须实测而不是按版本挑：PowerShell 7 在 AppContainer 里能否加载 cmdlet，
    /// 取决于它的模块目录是否真的可读——能启动、能报版本号，但 `Write-Output` 都不认，
    /// 症状是「脚本里每个命令都不存在」。这类失败只能在容器里跑一次才看得见。
    ///
    /// 探测用 `Write-Output` 并核对输出：退出码 0 但报错文本说明 cmdlet 没加载，同样算不通。
    pub fn powershell(&self) -> Option<PathBuf> {
        let mut cache = self
            .powershell
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(selected) = cache.as_ref() {
            return selected.clone();
        }
        let env = |name: &str| std::env::var(name).ok();
        let selected = super::runtime::powershell_candidates(&env)
            .into_iter()
            .find(|candidate| self.powershell_works(candidate));
        *cache = Some(selected.clone());
        selected
    }

    /// 在容器里跑一次最小脚本，判断该解释器是否真能用。
    ///
    /// 判据只看「输出里有没有探针字符串」：PowerShell 7 在 AppContainer 里启动正常、
    /// `$PSHOME` 与 `$env:PSModulePath` 都正确，却**一个 cmdlet 都加载不出来**，
    /// 连 `Write-Output` 都报「无法识别」——此时它照样可能以退出码 0 结束，因此退出码
    /// 不能作为判据（真机实测）。
    fn powershell_works(&self, executable: &Path) -> bool {
        let args = vec![
            executable.to_string_lossy().to_string(),
            "-NoLogo".to_string(),
            "-NoProfile".to_string(),
            "-NonInteractive".to_string(),
            "-Command".to_string(),
            "Write-Output '__omnicrawl_probe__'".to_string(),
        ];
        let Ok(spawned) = self.spawn(&args) else {
            return false;
        };
        let mut spawned = spawned;
        // 探测也要有上限：真坏掉时它可能一直挂着，不能把首次执行拖死。
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            if spawned.child.try_wait().is_some() {
                break;
            }
            if std::time::Instant::now() >= deadline {
                spawned.child.kill();
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let stdout = String::from_utf8_lossy(&spawned.stdout.join().unwrap_or_default()).to_string();
        stdout.contains("__omnicrawl_probe__")
    }

    /// 能力 SID 指针表，供 `SECURITY_CAPABILITIES.Capabilities` 使用。
    fn capability_attributes(&self) -> Vec<SID_AND_ATTRIBUTES> {
        self.capability_owned
            .iter()
            .flat_map(|owned| owned.sids.iter().copied())
            .map(|sid| SID_AND_ATTRIBUTES {
                Sid: sid as PSID,
                // SE_GROUP_ENABLED = 0x4：能力必须处于启用状态才生效。
                Attributes: 0x0000_0004,
            })
            .collect()
    }

    /// 已可在容器内读取的解释器目录（会进沙盒 `PATH`）。
    pub fn runtimes(&self) -> &[super::runtime::RuntimeRoot] {
        &self.runtimes
    }

    /// 因目录过大而**未授权**的解释器目录，附原因（`/sandbox` 与日志如实展示）。
    pub fn skipped_runtimes(&self) -> &[(PathBuf, String)] {
        &self.skipped
    }

    pub fn container_name(&self) -> &str {
        self.container.name()
    }

    /// 销毁本工作区的容器 profile（退出清理）。
    ///
    /// 安全性来自 SID 的确定性：容器名由「用户 + 工作区」派生，同名永远得到同一个
    /// SID（实测建 profile 前后、删除后重建都是同一个 `S-1-15-2-…`）。因此磁盘上那些
    /// 给该 SID 的 ACE 不会变成悬空引用——下次启动重建 profile 后它们继续有效，
    /// 也就不必重付一次可继承授权的代价（大目录单次可达数十秒）。
    ///
    /// 删不掉不算错误：profile 残留只会让 `%LOCALAPPDATA%\\Packages` 多一项，
    /// 而容器名稳定，后续启动会复用同一个 profile。因此这里只返回是否真的删除了。
    pub fn destroy_container_named(name: &str) -> bool {
        let name_wide = wide(name);
        // SAFETY: 名字指针在调用期间有效；只按名字删除，不持有其他资源。
        let result = unsafe { DeleteAppContainerProfile(name_wide.as_ptr()) };
        result >= 0
    }

    /// 本容器的 SID（测试与诊断用）。
    pub fn sid_for_test(&self) -> *mut c_void {
        self.container.sid()
    }

    /// 本容器 SID 的字符串形式（`S-1-15-2-...`），回环豁免按它登记。
    pub fn sid_string(&self) -> String {
        format_sid(self.container.sid())
    }

    /// 给本容器加回环豁免，返回的守卫在析构时撤销。
    ///
    /// 为什么必须做：AppContainer 不给网络能力时**连宿主回环都被拒**（实测超时）。
    /// 沙盒的出网通道是宿主侧代理，它就在回环上，因此没有豁免代理形同虚设。
    ///
    /// 为什么用 `-p=<SID>` 而不是 `-n=<名字>`：`-n` 要的是**包名**
    /// （如 `microsoft.net.native.framework.2.1_8wekyb3d8bbwe`），AppContainer 名不是包名，
    /// 传它一律返回 87（参数错误）——这是实测踩出来的。
    ///
    /// 加不上就是加不上：调用方据此把网络强制为 `offline`，绝不放宽成「有
    /// `internetClient` 但代理不通」那种更危险的中间态。
    #[cfg(windows)]
    pub fn exempt_loopback(&self) -> Result<LoopbackExemption, String> {
        let sid = self.sid_string();
        // 幂等：先撤一次再设，避免上次崩溃残留的重复条目累积。
        let _ = run_checknetisolation("-d", &sid);
        run_checknetisolation("-a", &sid)
            .map_err(|error| format!("回环豁免不可用，网络已禁（{error}）"))?;
        Ok(LoopbackExemption { sid })
    }

    /// 撤掉本容器 SID 在 `path` 上的 ACE（清理由本沙盒写入的权限）。
    ///
    /// 只供测试与诊断使用：正常运行时 ACE 会保留，下次启动复用（重新授予很贵）。
    pub fn revoke(&self, path: &Path) -> Result<(), String> {
        revoke(path, self.container.sid())
    }

    /// 以容器身份拉起 `args`（`args[0]` 是可执行文件路径）。
    ///
    /// 返回的 [`SandboxChild`] 已经归属 Job：后代无法逃离，宿主崩溃时由操作系统递归回收。
    /// 输出一次性读全——`monitor` 需要增量读，用 [`Self::spawn_streamed`]。
    pub fn spawn(&self, args: &[String]) -> io::Result<Spawned> {
        let (child, stdout_read, stderr_read) = self.spawn_raw(args)?;
        Ok(Spawned {
            child,
            stdout: spawn_pipe_reader(stdout_read.into_inner()),
            stderr: spawn_pipe_reader(stderr_read.into_inner()),
        })
    }

    /// 以容器身份拉起 `args`，输出管道交给调用方逐行读取（`monitor` 的后台任务）。
    ///
    /// 与 [`Self::spawn`] 的差别只在输出怎么消费：这里把两个读端交出去，不占线程。
    /// 另附一个 [`Terminator`]：子进程交给等待线程后，停止路径仍要能终止它。
    pub fn spawn_streamed(&self, args: &[String]) -> io::Result<Streamed> {
        let (child, stdout_read, stderr_read) = self.spawn_raw(args)?;
        let terminator = Terminator::duplicate(child.process_handle())?;
        // SAFETY: 两个读端由本函数独占，转换后由 `File` 负责关闭，不会重复关闭。
        let stdout = unsafe { std::fs::File::from_raw_handle(stdout_read.into_inner() as _) };
        // SAFETY: 同上。
        let stderr = unsafe { std::fs::File::from_raw_handle(stderr_read.into_inner() as _) };
        Ok(Streamed {
            child,
            stdout,
            stderr,
            terminator,
        })
    }

    /// 真正的一次 `CreateProcessW`：返回子进程与两个**原始**读端句柄。
    fn spawn_raw(&self, args: &[String]) -> io::Result<(SandboxChild, OwnedHandle, OwnedHandle)> {
        if args.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "缺少可执行文件",
            ));
        }
        let executable = wide(&args[0]);
        let mut command_line = build_command_line(args);
        let directory = wide(&self.workspace.to_string_lossy());
        let proxy = self.proxy_url();
        let mut environment = build_environment_with(
            &self.temp,
            &self.workspace,
            &self.runtimes,
            proxy.as_deref(),
        );

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
        // 无能力（`capabilities` 为空）时 `CapabilityCount = 0`，容器完全断网。
        let mut capability_attrs = self.capability_attributes();
        let capability = SECURITY_CAPABILITIES {
            AppContainerSid: self.container.sid(),
            Capabilities: if capability_attrs.is_empty() {
                std::ptr::null_mut()
            } else {
                capability_attrs.as_mut_ptr()
            },
            CapabilityCount: capability_attrs.len() as u32,
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
        // `GetLastError` 会被后续任何一次 Win32 调用覆盖，所以必须先取错误码再关句柄。
        let create_error = if created == 0 { last_error() } else { 0 };
        // SAFETY: 三个句柄都由本函数独占，各关闭一次。
        unsafe {
            CloseHandle(stdin.into_inner());
            CloseHandle(stdout_write.into_inner());
            CloseHandle(stderr_write.into_inner());
        }
        if created == 0 {
            return Err(describe_create_error(create_error));
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

        Ok((
            SandboxChild {
                process,
                job,
                pid: process_info.dwProcessId,
                exited: None,
            },
            stdout_read,
            stderr_read,
        ))
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

/// 一次流式派生：子进程本体加上交给调用方逐行读的输出。
pub struct Streamed {
    pub child: SandboxChild,
    pub stdout: std::fs::File,
    pub stderr: std::fs::File,
    pub terminator: Terminator,
}

/// 独立于 [`SandboxChild`] 的终止句柄。
///
/// `monitor` 把子进程交给等待线程，而停止路径在别的线程上——两者都要能终止它。
/// 因此这里复制一份进程句柄：各自关闭，互不影响。
pub struct Terminator(OwnedHandle);

impl Terminator {
    fn duplicate(process: HANDLE) -> io::Result<Self> {
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

/// 回环豁免的存活期：析构时撤销。
///
/// 不做"撤销失败要重试"的花活：撤销失败只会在豁免表里留一条过期记录，
/// 而那个 SID 只属于本工作区，下次启动会先 `-d` 再 `-a`（幂等）覆盖掉。
pub struct LoopbackExemption {
    sid: String,
}

impl Drop for LoopbackExemption {
    fn drop(&mut self) {
        let _ = run_checknetisolation("-d", &self.sid);
    }
}

/// 调 `CheckNetIsolation.exe LoopbackExempt <action> -p=<sid>`。
fn run_checknetisolation(action: &str, sid: &str) -> Result<(), String> {
    let output = std::process::Command::new("CheckNetIsolation.exe")
        .args(["LoopbackExempt", action, &format!("-p={sid}")])
        .output()
        .map_err(|error| format!("无法调用 CheckNetIsolation：{error}"))?;
    if output.status.success() {
        return Ok(());
    }
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    Err(format!(
        "CheckNetIsolation {action} 退出码 {:?}：{text}",
        output.status.code()
    ))
}

/// 把 SID 指针格式化成 `S-1-15-2-...`。
fn format_sid(sid: *mut c_void) -> String {
    if sid.is_null() {
        return String::new();
    }
    // SAFETY: SID 布局固定：Revision(1) SubAuthorityCount(1) IdentifierAuthority(6)
    // 后跟 `SubAuthorityCount` 个 u32。
    unsafe {
        let bytes = sid as *const u8;
        let revision = *bytes;
        let count = *bytes.add(1) as usize;
        let mut authority: u64 = 0;
        for index in 0..6 {
            authority = (authority << 8) | *bytes.add(2 + index) as u64;
        }
        let mut parts = vec![format!("S-{revision}-{authority}")];
        for index in 0..count {
            let offset = 8 + index * 4;
            let mut value: u32 = 0;
            for byte in 0..4 {
                value |= (*bytes.add(offset + byte) as u32) << (byte * 8);
            }
            parts.push(value.to_string());
        }
        parts.join("-")
    }
}

/// 解析一个 AppContainer 网络能力名（`internetClient` 等）到 SID。
///
/// `DeriveCapabilitySidsFromName` 返回**两个**堆上的 SID 数组：能力组 SID 和直接能力 SID，
/// 都由调用方 `LocalFree`。`SECURITY_CAPABILITIES.Capabilities` 要的是后者，且必须活到
/// `CreateProcessW` 返回。
///
/// **两个数组的所有权都交还给调用方**：SID 指针指向数组内部，提前释放数组会留下
/// 悬垂指针（`CreateProcessW` 会读它）。释放统一在 [`CapabilitySids`] 的 `Drop` 里做。
fn capability_sid(name: &str) -> Result<CapabilitySids, String> {
    let wide_name = wide(name);
    let mut group_sids: *mut PSID = std::ptr::null_mut();
    let mut group_count: u32 = 0;
    let mut capability_sids: *mut PSID = std::ptr::null_mut();
    let mut capability_count: u32 = 0;
    // SAFETY: 四个出参都是本栈帧的局部变量，指针在调用期间有效。
    let ok = unsafe {
        DeriveCapabilitySidsFromName(
            wide_name.as_ptr(),
            &mut group_sids,
            &mut group_count,
            &mut capability_sids,
            &mut capability_count,
        )
    };
    if ok == 0 {
        return Err(io_error("解析网络能力", last_error()));
    }
    if capability_count == 0 || capability_sids.is_null() {
        // SAFETY: 两个数组都由上面的 API 分配，成对释放。
        unsafe {
            if !group_sids.is_null() {
                LocalFree(group_sids as *mut c_void);
            }
            if !capability_sids.is_null() {
                LocalFree(capability_sids as *mut c_void);
            }
        }
        return Err(format!("能力 {name:?} 没有对应的 SID"));
    }
    // SAFETY: `capability_sids` 指向 `capability_count` 个有效 PSID。
    let first = unsafe { *capability_sids };
    Ok(CapabilitySids {
        group: group_sids,
        capability: capability_sids,
        sids: vec![first as *mut c_void],
    })
}

/// 一组能力 SID 及其底层数组，负责在离开作用域时释放。
struct CapabilitySids {
    group: *mut PSID,
    capability: *mut PSID,
    sids: Vec<*mut c_void>,
}

// SID 数组由本结构体独占（不共享、不跨线程转移所有权），`Isolation` 会被放进 `Arc`
// 供多个工具调用共享。裸指针本身不实现 `Send`/`Sync`，但这两个 impl 的前提成立：
// 读到的是 `DeriveCapabilitySidsFromName` 分配的只读 SID 数据，且释放只发生在
// `Drop`（此时已无其他引用）。因此按 Rust 的规则显式声明。
unsafe impl Send for CapabilitySids {}
unsafe impl Sync for CapabilitySids {}

impl Drop for CapabilitySids {
    fn drop(&mut self) {
        // SAFETY: 两个数组都由 `DeriveCapabilitySidsFromName` 分配，这里成对归还；
        // 每个字段只会被释放一次（本结构体不实现 Clone）。
        unsafe {
            if !self.group.is_null() {
                LocalFree(self.group as *mut c_void);
                self.group = std::ptr::null_mut();
            }
            if !self.capability.is_null() {
                LocalFree(self.capability as *mut c_void);
                self.capability = std::ptr::null_mut();
            }
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

    /// 容器内必须能遍历系统盘根，`powershell_works` 的探针才有意义。
    ///
    /// 这条守的是一处真实缺陷：工作区放在 `D:` 时，`grant_ancestors` 只覆盖 `D:` 那一支，
    /// 系统盘 `C:\` 不在祖先链上、拿不到容器的 ACE，于是 PowerShell 7 建立 `C:` 驱动器失败
    /// （`Directory.Exists("C:\\")` 返回 False），它自己的内置模块全部导入不了，最终连
    /// `Write-Output` 都报「无法识别」——而 Windows PowerShell 5.1 完全正常，因为 5.1 不需要
    /// 那一步。症状是「pwsh 起来了但一个 cmdlet 都不认」，与权限错误毫无相似之处。
    #[cfg(windows)]
    #[test]
    fn appcontainer_can_traverse_the_system_drive_root() {
        let root = std::env::current_dir()
            .unwrap_or_else(|_| std::env::temp_dir())
            .join(".omnicrawl")
            .join(".agent_tmp")
            .join("sandbox-system-drive-test");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("建工作区");
        let config = omnicrawl_config::features::sandbox::SandboxConfig {
            enabled: true,
            mode: omnicrawl_config::features::sandbox::SandboxMode::AppContainer,
            ..Default::default()
        };
        let sandbox = Sandbox::new(
            config,
            root.clone(),
            root.join(".omnicrawl").join(".agent_tmp"),
        );
        let Some(isolation) = sandbox.isolation().ok() else {
            return;
        };
        // 用 5.1 探测：它一定能起，可用来单独判断「遍历权」这件事。
        let shell = r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe";
        for drive_root in system_drive_roots() {
            let script = format!(
                "[System.IO.Directory]::Exists('{}')",
                drive_root.to_string_lossy().replace('\\', "\\\\")
            );
            let args = vec![
                shell.to_string(),
                "-NoLogo".to_string(),
                "-NoProfile".to_string(),
                "-NonInteractive".to_string(),
                "-Command".to_string(),
                script,
            ];
            let mut spawned = match isolation.spawn(&args) {
                Ok(spawned) => spawned,
                Err(_) => continue,
            };
            let mut tries = 0;
            while spawned.child.try_wait().is_none() && tries < 2000 {
                std::thread::sleep(std::time::Duration::from_millis(20));
                tries += 1;
            }
            let stdout =
                String::from_utf8_lossy(&spawned.stdout.join().unwrap_or_default()).to_string();
            assert!(
                stdout.contains("True"),
                "容器内必须能遍历系统盘根 {}：{stdout}",
                drive_root.display()
            );
        }
        let _ = std::fs::remove_dir_all(&root);
    }

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
            &[],
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

    /// 环境块里不能有大小写不敏感的重复键。
    ///
    /// `WINDIR` 与 `windir` 同时出现时，PowerShell 的 `Env:` 驱动器会抛
    /// 「已添加了具有相同键的项」，于是 `$env:FOO` 与 `Get-ChildItem Env:` 全部失效——
    /// 一条重复项毁掉整套环境变量读取，而报错完全不指向重复项。
    #[test]
    fn environment_has_no_case_insensitive_duplicates() {
        let block = build_environment(
            Path::new(r"D:\work\.omnicrawl\.agent_tmp\sandbox"),
            Path::new(r"D:\work"),
            &[],
        );
        let text = String::from_utf16_lossy(&block);
        let mut seen: Vec<String> = Vec::new();
        for entry in text.split('\0').filter(|entry| !entry.is_empty()) {
            let key = entry.split('=').next().unwrap_or_default();
            let folded = key.to_ascii_uppercase();
            assert!(
                !seen.contains(&folded),
                "环境变量 {key} 重复（大小写不敏感）：{seen:?}"
            );
            seen.push(folded);
        }
    }

    #[test]
    fn dedupe_vars_keeps_the_first_occurrence() {
        let kept = dedupe_vars(vec![
            ("WINDIR".to_string(), "C:\\Windows".to_string()),
            ("windir".to_string(), "D:\\Other".to_string()),
            ("PATH".to_string(), "C:\\".to_string()),
        ]);
        assert_eq!(
            kept,
            vec![
                ("WINDIR".to_string(), "C:\\Windows".to_string()),
                ("PATH".to_string(), "C:\\".to_string()),
            ]
        );
    }

    /// `npm_config_prefix` 必须指进工作区。
    ///
    /// `npm.ps1` 会拿默认前缀（`%APPDATA%\npm`，容器读不到）做一次 `Test-Path`；
    /// 在 `$ErrorActionPreference = 'Stop'` 下那次探测是**致命**错误，
    /// `npm --version` 于是报 `Test-Path : Access is denied` 而不是版本号（实测）。
    #[test]
    fn npm_prefix_points_inside_the_workspace() {
        let block = build_environment(
            Path::new(r"D:\work\.omnicrawl\.agent_tmp\sandbox"),
            Path::new(r"D:\work"),
            &[],
        );
        let text = String::from_utf16_lossy(&block);
        assert!(
            text.contains(r"npm_config_prefix=D:\work\.omnicrawl\.agent_tmp\npm-global"),
            "{text}"
        );
        // 也必须在 PATH 里，否则 `npm i -g` 装出来的命令找不到。
        assert!(
            text.contains(r"npm-global"),
            "prefix 目录应当在 PATH 中：{text}"
        );
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
