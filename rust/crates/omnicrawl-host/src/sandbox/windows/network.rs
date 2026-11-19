use std::ffi::c_void;
use std::path::Path;
use windows_sys::Win32::Foundation::PSID;
use windows_sys::Win32::Foundation::LocalFree;
use windows_sys::Win32::Security::DeriveCapabilitySidsFromName;

use super::base::*;

/// 回环豁免的存活期：析构时撤销。
///
/// 不做"撤销失败要重试"的花活：撤销失败只会在豁免表里留一条过期记录，
/// 而那个 SID 只属于本工作区，下次启动会先 `-d` 再 `-a`（幂等）覆盖掉。
pub struct LoopbackExemption {
    pub(super) sid: String,
}

impl Drop for LoopbackExemption {
    fn drop(&mut self) {
        let _ = run_checknetisolation("-d", &self.sid);
    }
}

/// 调 `CheckNetIsolation.exe LoopbackExempt <action> -p=<sid>`。
pub(super) fn run_checknetisolation(action: &str, sid: &str) -> Result<(), String> {
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
pub(super) fn format_sid(sid: *mut c_void) -> String {
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
pub(super) fn capability_sid(name: &str) -> Result<CapabilitySids, String> {
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
pub(super) struct CapabilitySids {
    pub(super) group: *mut PSID,
    pub(super) capability: *mut PSID,
    pub(super) sids: Vec<*mut c_void>,
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
pub(super) fn container_name(workspace: &Path) -> String {
    pub(super) fn hash(text: &str) -> u64 {
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
