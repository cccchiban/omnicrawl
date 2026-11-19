use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use windows_sys::Win32::Foundation::LocalFree;
use windows_sys::Win32::Security::Authorization::{
    GetNamedSecurityInfoW, SetEntriesInAclW, SetNamedSecurityInfoW, EXPLICIT_ACCESS_W,
    GRANT_ACCESS, NO_MULTIPLE_TRUSTEE, REVOKE_ACCESS, SE_FILE_OBJECT, TRUSTEE_IS_SID,
    TRUSTEE_IS_UNKNOWN, TRUSTEE_W,
};
use windows_sys::Win32::Security::{
    GetAce, ACE_HEADER, DACL_SECURITY_INFORMATION,
    SUB_CONTAINERS_AND_OBJECTS_INHERIT,
};

use super::base::*;
use super::environment::normalize_path;

/// DACL 编辑的进程级串行化。
///
/// 「读 DACL → 加 ACE → 写回」不是原子操作：两条命令并发首次准备沙盒时，各自读到同一份
/// 旧 DACL，后者写回的内容会把前者刚加的 ACE 抹掉。症状极其误导——工作区突然「拒绝访问」，
/// 或容器内的 PowerShell 把相对路径解析到 `C:\`，看起来像路径写错。
/// 代价可以忽略：准备只在首次使用时发生一次。
static DACL_EDIT: Mutex<()> = Mutex::new(());

/// 给 `path` 加一条针对容器 SID 的 ACE。
///
/// `inherit` 为真时对目录做可继承授予（子项自动继承），文件用 `false`。
///
/// **可继承授予是昂贵的**：`SetNamedSecurityInfoW` 会把 ACE 立刻传播到整棵子树。
/// 实测 `D:\ProgramData\Anaconda3`（19.5 万文件 / 2.3 万目录）单次约 50 秒，
/// `C:\Program Files\nodejs` 约 39 秒。所以调用方必须先查 [`has_access`]，
/// 只在确实缺权时写——否则每次启动都要重付一次这个代价。
pub(super) fn grant(path: &Path, sid: *mut c_void, access: u32, inherit: bool) -> Result<(), String> {
    edit_ace(path, sid, access, inherit, GRANT_ACCESS)
}

/// 容器身份对 `path` 是否已具备 `access` 所需的权限（不写任何东西）。
///
/// **不能用 `GetEffectiveRightsFromAclW`**：它对 AppContainer 的 package SID 会给出
/// 「有权限」的错误答案（实测 `C:\Program Files\nodejs` 返回可读，容器内实际是「拒绝访问」），
/// 因为该函数只按 ACE 计算位掩码，不考虑受限令牌的访问检查路径。
/// 这里直接遍历 DACL，找针对本 SID 的显式允许 ACE。
pub(super) fn has_access(path: &Path, sid: *mut c_void, access: u32) -> bool {
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
pub(super) fn revoke(path: &Path, sid: *mut c_void) -> Result<(), String> {
    edit_ace(path, sid, 0, false, REVOKE_ACCESS)
}

fn edit_ace(
    path: &Path,
    sid: *mut c_void,
    access: u32,
    inherit: bool,
    mode: i32,
) -> Result<(), String> {
    let _serial = DACL_EDIT.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
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
pub(super) fn grant_ancestors(target: &Path, sid: *mut c_void, granted: &mut Vec<PathBuf>) {
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
pub(super) fn system_drive_roots() -> Vec<PathBuf> {
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
pub(super) fn can_traverse(path: &Path, sid: *mut c_void) -> bool {
    // 只需「能穿过」：`FILE_TRAVERSE`(0x20) 与 `FILE_READ_ATTRIBUTES`(0x80)。
    // 用与 `has_access` 同一套显式 ACE 检查：`GetEffectiveRightsFromAclW` 对 AppContainer
    // 的 package SID 会给出错误答案（实测把不可读的目录判成可读）。
    has_access(path, sid, 0x0000_0020 | 0x0000_0080)
}
