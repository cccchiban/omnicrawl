use std::io;
use windows_sys::Win32::Foundation::GetLastError;

pub(super) const ACCESS_ALLOWED_ACE_TYPE_LOCAL: u8 = 0;
pub(super) const ACCESS_ALLOWED_OBJECT_ACE_TYPE_LOCAL: u8 = 5;

use windows_sys::Win32::Storage::FileSystem::{
    DELETE, FILE_DELETE_CHILD, FILE_GENERIC_EXECUTE,
    FILE_GENERIC_READ, FILE_GENERIC_WRITE,
};




/// `STILL_ACTIVE`：仅此退出码表示进程仍在运行。
pub(super) const STILL_ACTIVE: u32 = 0x103;

// 目录遍历只需要这三项：能穿过、能读属性、能同步。**不含** `FILE_LIST_DIRECTORY`，
// 因此加了这条 ACE 的父目录仍然无法被沙盒列举内容。
pub(super) const TRAVERSE_ONLY: u32 = 0x0000_0020 | 0x0000_0080 | 0x0010_0000;

/// 读写执行**加删除**：工作区与沙盒临时目录用这一组。
///
/// 必须显式带 `DELETE`(0x0001_0000) 与 `FILE_DELETE_CHILD`(0x0000_0040)：
/// `FILE_GENERIC_WRITE` 只覆盖「写数据、追加」，**不含删除**，于是目录能建、文件能写，
/// 却删不掉——`Remove-Item`、`rmdir /s`、`Directory.Delete` 一律报「访问被拒绝」
/// （实测：新建目录的容器 ACE 只有 `Write, ReadAndExecute, Synchronize`）。
/// 删不掉自己刚创建的构建产物是最典型的日常动作，缺这一项等于把工作区变成只增不减。
pub(super) const FULL_CONTROL_FOR_SANDBOX: u32 =
    FILE_GENERIC_READ | FILE_GENERIC_WRITE | FILE_GENERIC_EXECUTE | DELETE | FILE_DELETE_CHILD;

/// 只读执行：解释器目录等「必须能跑但不能改」的位置。
pub(super) const READ_EXECUTE: u32 = FILE_GENERIC_READ | FILE_GENERIC_EXECUTE;

/// 给解释器目录授权前允许的最大条目数。
///
/// `SetNamedSecurityInfoW` 的耗时随子树规模增长（实测 `C:\Program Files\nodejs` 约 0.3 秒 /
/// 0.29 万项，`D:\ProgramData\Anaconda3` 约 18 秒 / 22 万项）。1 万项大致对应「几百毫秒」，
/// 超过它就跳过并如实报告，不让启动悄悄卡住。
pub(super) const GRANT_ENTRY_BUDGET: u64 = 10_000;

pub(super) fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

pub(super) fn last_error() -> u32 {
    // SAFETY: 无参数、无副作用。
    unsafe { GetLastError() }
}

pub(super) fn io_error(action: &str, code: u32) -> String {
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
pub(super) fn describe_create_error(code: u32) -> io::Error {
    if code == 87 {
        return io::Error::other(
            "CreateProcessW 失败（错误码 87，参数错误）：扩展启动信息不完整——\
             `cb` 必须是 STARTUPINFOEXW 的大小、句柄清单里的句柄必须有效且可继承"
                .to_string(),
        );
    }
    io::Error::from_raw_os_error(code as i32)
}

pub(super) fn other(message: String) -> io::Error {
    io::Error::other(message)
}
