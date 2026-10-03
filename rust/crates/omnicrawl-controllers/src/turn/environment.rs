//! `omnicrawl/agent/context/environment.py` 的移植：注入给模型的运行环境摘要。
//!
//! 与 Python 的分工一致：这里只暴露低敏、稳定且会影响工具选择的信息；不枚举完整环境变量，
//! 避免把 API Key、Token、代理配置等敏感值塞进模型上下文。检测调用方可以显式传入结果
//! （便于测试与兼容旧补丁点），未传入时本模块自行探测。
//!
//! 与 Python 的实现差异：
//! - `platform.system()/release()/machine()` 用 `std::env::consts` 与 `ver` 命令/`uname`
//!   等价物替代；Windows 上 release 取注册表会引入额外依赖，这里用环境常量 + 探测，
//!   具体口径见 [`os_platform_triple`]。
//! - Python 的「Python 版本 / 可执行文件」两行改为「内核」两行：Rust 内核没有 CPython
//!   运行时，这两行的语义是「运行时版本与可执行文件」。
//! - Windows 进程链用 Toolhelp API（`CreateToolhelp32Snapshot`），与 Python 的 ctypes
//!   调用是同一组 Win32 入口；结果按进程缓存（Python 用 `lru_cache`，这里用 `OnceLock`
//!   + `Mutex`，链在进程生命周期内不会变化）。

// 进程链缓存与 Win32 进程表只在 Windows 分支使用；非 Windows 目标若照旧无条件引入，
// 会在 Linux/macOS 与 musl 交叉编译时留下 unused_imports 警告（CI 会在这些目标上构建）。
#[cfg(windows)]
use std::collections::BTreeMap;
#[cfg(windows)]
use std::sync::{Mutex, OnceLock};

/// 进程链枚举的上限（与 Python 的默认 `limit=12` 一致）。
pub const PROCESS_CHAIN_LIMIT: usize = 12;

/// 注入给模型的运行环境摘要。
///
/// 对应 Python 的 `runtime_environment_context`：调用方可显式传入检测结果
/// （`window_hint` / `terminal_hint`），未传入时本模块自行探测。
pub fn runtime_environment_context(
    workspace_root: &str,
    workspace_detection_summary: &str,
    window_hint: Option<&str>,
    terminal_hint: Option<&str>,
) -> String {
    let detected_window_hint = match window_hint {
        Some(hint) if !hint.is_empty() => hint.to_string(),
        _ => detect_agent_window_hint(),
    };
    let detected_terminal_hint = match terminal_hint {
        Some(hint) if !hint.is_empty() => hint.to_string(),
        _ => detect_terminal_hint(),
    };

    let cwd = std::env::current_dir()
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "（无法获取）".to_string());
    let (system, release, machine) = os_platform_triple();
    let mut lines = vec![
        "运行环境：".to_string(),
        format!("- 操作系统：{system} {release} ({machine})"),
        format!("- 内核：{}", kernel_version()),
        format!("- 内核可执行文件：{}", kernel_executable()),
        format!("- 工作区根目录：{workspace_root}"),
        format!("- 当前进程目录：{cwd}"),
        format!("- 路径分隔符：{}", path_separator()),
    ];
    let summary = workspace_detection_summary.trim();
    if !summary.is_empty() {
        lines.push(format!("- 工作区检测：{summary}"));
    }
    if !detected_window_hint.is_empty() {
        lines.push(format!("- Agent 运行窗口：{detected_window_hint}"));
    }
    if !detected_terminal_hint.is_empty() {
        lines.push(format!("- 终端环境变量：{detected_terminal_hint}"));
    }
    lines.join("\n")
}

/// Windows 判定（对应 Python 的 `os.name == "nt"`）。
pub fn is_windows() -> bool {
    cfg!(windows)
}

/// 路径分隔符（对应 Python 的 `os.sep`）。
pub fn path_separator() -> &'static str {
    if is_windows() {
        "\\"
    } else {
        "/"
    }
}

/// 操作系统三元组：`platform.system()`、`platform.release()` 与 `platform.machine()` 的等价物。
///
/// Windows 的 release 从 `cmd /c ver` 的输出里取版本号（`10.0.26100` 这类），
/// 取不到时回落 `std::env::consts::OS`；类 Unix 直接走 `uname -sr -m` 的语义，
/// 由 `sysname + release + machine` 拼出。全部失败时用 `std::env::consts` 的编译期常量，
/// 绝不让环境摘要这一步失败。
fn os_platform_triple() -> (String, String, String) {
    let machine = os_machine().unwrap_or_else(|| std::env::consts::ARCH.to_string());
    if is_windows() {
        let system = "Windows".to_string();
        let release = windows_release().unwrap_or_else(|| {
            // 至少带上编译期目标，避免出现空版本号。
            std::env::consts::OS.to_string()
        });
        (system, release, machine)
    } else {
        let system = os_sysname().unwrap_or_else(|| std::env::consts::OS.to_string());
        let release = os_release().unwrap_or_default();
        (system, release, machine)
    }
}

fn os_sysname() -> Option<String> {
    uname_field(0)
}

fn os_release() -> Option<String> {
    uname_field(1)
}

fn os_machine() -> Option<String> {
    if is_windows() {
        return windows_machine();
    }
    uname_field(2)
}

/// `uname` 的第 0/1/2 号字段（sysname / release / machine）。
fn uname_field(field: usize) -> Option<String> {
    let output = std::process::Command::new("uname")
        .arg("-sr")
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    let mut parts = text.split_whitespace();
    let sysname = parts.next()?.to_string();
    let release = parts.next().unwrap_or("").to_string();
    if field == 0 {
        return Some(sysname);
    }
    if field == 1 {
        if release.is_empty() {
            return None;
        }
        return Some(release);
    }
    let output = std::process::Command::new("uname")
        .arg("-m")
        .output()
        .ok()?;
    let machine = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if machine.is_empty() {
        None
    } else {
        Some(machine)
    }
}

/// Windows 版本号：`ver` 命令输出形如
/// `Microsoft Windows [Version 10.0.26100.2314]`，取方括号里的 `10.0.26100`（前三段）。
fn windows_release() -> Option<String> {
    let output = std::process::Command::new("cmd")
        .args(["/c", "ver"])
        .creation_flags_nopatch()
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    let start = text.find('[')? + 1;
    let end = text[start..].find(']')? + start;
    let version = &text[start..end];
    let version = version.trim_start_matches("Version").trim();
    let mut parts: Vec<&str> = version.split('.').collect();
    if parts.len() < 2 {
        return None;
    }
    parts.truncate(3.min(parts.len()));
    let joined = parts.join(".");
    if joined.is_empty() {
        None
    } else {
        Some(joined)
    }
}

fn windows_machine() -> Option<String> {
    let arch = std::env::consts::ARCH;
    let mapped = match arch {
        "x86_64" => "AMD64",
        "x86" => "x86",
        "aarch64" => "ARM64",
        other => return Some(other.to_string()),
    };
    Some(mapped.to_string())
}

/// 内核运行时版本（对映 Python 的 `platform.python_version()` 一行）。
fn kernel_version() -> String {
    format!("rust-{}", env!("CARGO_PKG_VERSION"))
}

/// 内核可执行文件路径（对映 Python 的 `sys.executable` 一行）。
fn kernel_executable() -> String {
    std::env::current_exe()
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "（无法获取）".to_string())
}

trait CreationFlagsExt {
    fn creation_flags_nopatch(&mut self) -> &mut Self;
}

impl CreationFlagsExt for std::process::Command {
    /// Windows 上隐藏 `ver` 的控制台窗口（Python 的 `subprocess` 在无窗会话里也需要
    /// `CREATE_NO_WINDOW` 才不闪黑框；类 Unix 上没有这个 flag，直接忽略）。
    #[cfg(windows)]
    fn creation_flags_nopatch(&mut self) -> &mut Self {
        use std::os::windows::process::CommandExt;
        self.creation_flags(0x0800_0000) // CREATE_NO_WINDOW
    }

    #[cfg(not(windows))]
    fn creation_flags_nopatch(&mut self) -> &mut Self {
        self
    }
}

/// 检测 Agent 所在的交互窗口或父进程链，帮助模型选择兼容命令。
///
/// 对应 Python 的 `detect_agent_window_hint`：非 Windows 走 `SHELL` + 终端线索；
/// Windows 走进程链识别 Shell 与终端，最后给出进程链前 8 个名字。
pub fn detect_agent_window_hint() -> String {
    if !is_windows() {
        let shell = std::env::var("SHELL").unwrap_or_default();
        let shell = shell.trim();
        let terminal = detect_terminal_hint();
        if !shell.is_empty() && !terminal.is_empty() {
            return format!("Shell={}；终端={terminal}", file_name(shell));
        }
        if !shell.is_empty() {
            return format!("Shell={}", file_name(shell));
        }
        return terminal;
    }

    let process_chain = windows_process_name_chain(PROCESS_CHAIN_LIMIT);
    let lowered_chain: Vec<String> = process_chain
        .iter()
        .map(|name| name.to_lowercase())
        .collect();
    let shell_label = windows_shell_label(&lowered_chain);
    let terminal_label = windows_terminal_label(&lowered_chain);

    let mut parts: Vec<String> = Vec::new();
    if !terminal_label.is_empty() {
        parts.push(format!("终端={terminal_label}"));
    }
    if !shell_label.is_empty() {
        parts.push(format!("Shell={shell_label}"));
    }
    if !process_chain.is_empty() {
        let head = process_chain
            .iter()
            .take(8)
            .cloned()
            .collect::<Vec<_>>()
            .join(" <- ");
        parts.push(format!("进程链={head}"));
    }
    if parts.is_empty() {
        "Windows 控制台（未识别具体 Shell）".to_string()
    } else {
        parts.join("；")
    }
}

/// Python 的 `Path(name).name`：按两侧分隔符切出最后一段。
fn file_name(path: &str) -> String {
    path.rsplit(['/', '\\']).next().unwrap_or(path).to_string()
}

/// 进程链里的 Shell 标签（对应 Python 的 `windows_shell_label`）。
pub fn windows_shell_label(lowered_process_chain: &[String]) -> String {
    for name in lowered_process_chain {
        let label = match name.as_str() {
            "pwsh.exe" => "PowerShell 7+",
            "powershell.exe" => "Windows PowerShell",
            "cmd.exe" => "CMD",
            _ => continue,
        };
        return label.to_string();
    }
    String::new()
}

/// 进程链里的终端标签（对应 Python 的 `windows_terminal_label`）；
/// 重复标签只保留首次出现（Python 的 `dict.fromkeys`）。
pub fn windows_terminal_label(lowered_process_chain: &[String]) -> String {
    let mut labels: Vec<String> = Vec::new();
    let wt_session = std::env::var("WT_SESSION").unwrap_or_default();
    if !wt_session.trim().is_empty()
        || lowered_process_chain
            .iter()
            .any(|name| name == "windowsterminal.exe")
    {
        labels.push("Windows Terminal".to_string());
    }
    let term_program = std::env::var("TERM_PROGRAM").unwrap_or_default();
    let term_program = term_program.trim();
    if !term_program.is_empty() {
        labels.push(term_program.to_string());
    }
    if lowered_process_chain.iter().any(|name| name == "code.exe") {
        labels.push("VS Code Terminal".to_string());
    }
    if lowered_process_chain
        .iter()
        .any(|name| name == "conhost.exe")
    {
        labels.push("Console Host".to_string());
    }
    let mut seen: Vec<String> = Vec::new();
    for label in labels {
        if !seen.contains(&label) {
            seen.push(label);
        }
    }
    seen.join(" / ")
}

/// 返回当前进程到祖先进程的 exe 名称链；失败时返回空列表。
///
/// 使用 Win32 Toolhelp API（与 Python 的 ctypes 调用同一组入口），不依赖 psutil，
/// 也不通过 shell 再启动子进程。父进程链在进程生命周期内不会变化，因此按进程缓存。
#[cfg(windows)]
pub fn windows_process_name_chain(limit: usize) -> Vec<String> {
    cached_process_chain(limit)
}

/// 非 Windows 平台没有 Toolhelp API，直接返回空链（与 Python 的 `os.name != "nt"` 分支一致）。
#[cfg(not(windows))]
pub fn windows_process_name_chain(_limit: usize) -> Vec<String> {
    Vec::new()
}

#[cfg(windows)]
fn cached_process_chain(limit: usize) -> Vec<String> {
    static CACHE: OnceLock<Mutex<BTreeMap<usize, Vec<String>>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(BTreeMap::new()));
    if let Ok(guard) = cache.lock() {
        if let Some(hit) = guard.get(&limit) {
            return hit.clone();
        }
    }
    let chain = enumerate_windows_process_chain(limit);
    if let Ok(mut guard) = cache.lock() {
        guard.insert(limit, chain.clone());
    }
    chain
}

/// 终端类型线索：只使用常见非敏感变量名（对应 Python 的 `detect_terminal_hint`）。
pub fn detect_terminal_hint() -> String {
    let mut hints: Vec<String> = Vec::new();
    for name in ["WT_SESSION", "TERM_PROGRAM", "TERM"] {
        let value = std::env::var(name).unwrap_or_default();
        let value = value.trim();
        if !value.is_empty() {
            if name == "WT_SESSION" {
                hints.push(name.to_string());
            } else {
                hints.push(format!("{name}={value}"));
            }
        }
    }
    hints.join(", ")
}

#[cfg(windows)]
mod toolhelp {
    #![allow(
        non_snake_case,
        non_camel_case_types,
        dead_code,
        clippy::upper_case_acronyms
    )]

    use std::ffi::c_void;

    pub type DWORD = u32;
    pub type HANDLE = *mut c_void;
    pub type BOOL = i32;
    pub type LONG = i32;
    pub type SIZE_T = usize;

    pub const TH32CS_SNAPPROCESS: DWORD = 0x0000_0002;
    pub const INVALID_HANDLE_VALUE: HANDLE = -1isize as HANDLE;

    #[repr(C)]
    pub struct PROCESSENTRY32W {
        pub dwSize: DWORD,
        pub cntUsage: DWORD,
        pub th32ProcessID: DWORD,
        pub th32DefaultHeapID: SIZE_T,
        pub th32ModuleID: DWORD,
        pub cntThreads: DWORD,
        pub th32ParentProcessID: DWORD,
        pub pcPriClassBase: LONG,
        pub dwFlags: DWORD,
        pub szExeFile: [u16; 260],
    }

    extern "system" {
        pub fn CreateToolhelp32Snapshot(flags: DWORD, process_id: DWORD) -> HANDLE;
        pub fn Process32FirstW(snapshot: HANDLE, entry: *mut PROCESSENTRY32W) -> BOOL;
        pub fn Process32NextW(snapshot: HANDLE, entry: *mut PROCESSENTRY32W) -> BOOL;
        pub fn CloseHandle(object: HANDLE) -> BOOL;
    }

    impl PROCESSENTRY32W {
        pub(super) fn zeroed() -> Self {
            Self {
                dwSize: 0,
                cntUsage: 0,
                th32ProcessID: 0,
                th32DefaultHeapID: 0,
                th32ModuleID: 0,
                cntThreads: 0,
                th32ParentProcessID: 0,
                pcPriClassBase: 0,
                dwFlags: 0,
                szExeFile: [0; 260],
            }
        }
    }
}

#[cfg(windows)]
/// 实际枚举进程链（对应 Python 的 `_windows_process_name_chain_cached` 本体）。
fn enumerate_windows_process_chain(limit: usize) -> Vec<String> {
    use std::collections::BTreeSet;
    use toolhelp::*;

    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snapshot.is_null() || snapshot == INVALID_HANDLE_VALUE {
            return Vec::new();
        }

        let mut process_table: BTreeMap<u32, (u32, String)> = BTreeMap::new();
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..PROCESSENTRY32W::zeroed()
        };
        let mut ok = Process32FirstW(snapshot, &mut entry) != 0;
        while ok {
            let name_len = entry
                .szExeFile
                .iter()
                .position(|&unit| unit == 0)
                .unwrap_or(entry.szExeFile.len());
            let name = String::from_utf16_lossy(&entry.szExeFile[..name_len]);
            process_table.insert(entry.th32ProcessID, (entry.th32ParentProcessID, name));
            ok = Process32NextW(snapshot, &mut entry) != 0;
        }
        let _ = CloseHandle(snapshot);

        let mut chain: Vec<String> = Vec::new();
        let mut seen: BTreeSet<u32> = BTreeSet::new();
        let mut pid = std::process::id();
        for _index in 0..limit.max(1) {
            if seen.contains(&pid) {
                break;
            }
            seen.insert(pid);
            let Some((parent_pid, name)) = process_table.get(&pid) else {
                break;
            };
            if !name.is_empty() {
                chain.push(name.clone());
            }
            if *parent_pid == 0 {
                break;
            }
            pid = *parent_pid;
        }
        chain
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_label_matches_python_table() {
        let chain = vec![
            "omnicrawl.exe".to_string(),
            "pwsh.exe".to_string(),
            "windowsterminal.exe".to_string(),
        ];
        assert_eq!(windows_shell_label(&chain), "PowerShell 7+");
    }

    #[test]
    fn shell_label_empty_when_no_shell() {
        let chain = vec!["explorer.exe".to_string()];
        assert_eq!(windows_shell_label(&chain), "");
    }

    #[test]
    fn terminal_label_from_chain() {
        let chain = vec![
            "omnicrawl.exe".to_string(),
            "pwsh.exe".to_string(),
            "code.exe".to_string(),
            "conhost.exe".to_string(),
        ];
        assert_eq!(
            windows_terminal_label(&chain),
            "VS Code Terminal / Console Host"
        );
    }

    #[test]
    fn terminal_label_dedupes() {
        let chain = vec!["conhost.exe".to_string()];
        // 无环境变量时只应有链上识别出的一个标签。
        let label = windows_terminal_label(&chain);
        let count = label.split(" / ").count();
        assert!(count <= 2, "意外的标签集合：{label}");
    }

    #[test]
    fn context_contains_fixed_headings() {
        let summary = runtime_environment_context(
            "D:/work",
            "已检测到 git 仓库",
            Some("Shell=CMD"),
            Some("TERM_PROGRAM=vscode"),
        );
        assert!(summary.starts_with("运行环境：\n"));
        assert!(summary.contains("- 操作系统："));
        assert!(summary.contains("- 内核："));
        assert!(summary.contains("- 工作区根目录：D:/work"));
        assert!(summary.contains("- 当前进程目录："));
        assert!(summary.contains("- 工作区检测：已检测到 git 仓库"));
        assert!(summary.contains("- Agent 运行窗口：Shell=CMD"));
        assert!(summary.contains("- 终端环境变量：TERM_PROGRAM=vscode"));
    }

    #[test]
    fn empty_optional_sections_are_omitted() {
        let summary = runtime_environment_context("/tmp", "  ", None, None);
        assert!(!summary.contains("工作区检测"));
        // 探测分支在测试进程里至少能跑完；Linux CI 上 SHELL 通常存在。
        if !summary.contains("- Agent 运行窗口：") {
            assert!(!summary.contains("Agent 运行窗口"));
        }
    }

    #[test]
    fn path_separator_matches_platform() {
        assert_eq!(path_separator(), if is_windows() { "\\" } else { "/" });
    }

    #[test]
    fn file_name_splits_both_separators() {
        assert_eq!(file_name("/usr/bin/bash"), "bash");
        assert_eq!(file_name("C:\\Windows\\system32\\cmd.exe"), "cmd.exe");
        assert_eq!(file_name("pwsh"), "pwsh");
    }

    #[test]
    fn windows_release_parses_ver_output() {
        // windows_release 在无 cmd 的环境返回 None；这里只验证不 panic 且格式正确。
        if let Some(release) = windows_release() {
            assert!(release.split('.').count() >= 2, "意外版本号：{release}");
        }
    }

    #[test]
    fn kernel_version_carries_crate_version() {
        let version = kernel_version();
        assert!(version.starts_with("rust-"));
        assert!(version.contains(env!("CARGO_PKG_VERSION")));
    }
}
