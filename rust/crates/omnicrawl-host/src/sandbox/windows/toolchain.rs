use std::ffi::c_void;
use std::path::{Path, PathBuf};
use windows_sys::Win32::Storage::FileSystem::FILE_GENERIC_READ;

use super::acl::*;
use super::base::*;
use crate::sandbox::runtime::RuntimeRoot;

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
pub(super) fn prepare_runtimes(
    sid: *mut c_void,
) -> (Vec<RuntimeRoot>, Vec<(PathBuf, String)>) {
    let env = |name: &str| std::env::var(name).ok();
    // 先收集**全部**候选（不做可读性判定）：auto-discovery 的两类来源合在一起，
    // 后面的授权环节再决定谁能真正可用。
    let mut candidates: Vec<RuntimeRoot> = crate::sandbox::runtime::discover(&env);
    for runtime in crate::sandbox::runtime::rust_roots(&env) {
        if !candidates.iter().any(|entry| entry.path == runtime.path) {
            candidates.push(runtime);
        }
    }
    let mut skipped: Vec<(PathBuf, String)> = Vec::new();
    let mut available: Vec<RuntimeRoot> = Vec::new();
    grant_runtime_roots(&mut candidates, &mut skipped, sid);
    for runtime in candidates {
        if runtime_ready(&runtime, sid) {
            available.push(runtime);
        } else {
            skipped.push((
                runtime.path.clone(),
                "沙盒内读不到该目录；如需使用，请把该目录加入 `[sandbox.fs] readable`".to_string(),
            ));
        }
    }
    (available, skipped)
}

/// 该解释器目录里的可执行文件是否真的能被容器读到。
///
/// 只看「至少有一个已知入口可读」——沙盒里要跑的就是这些入口。
pub(super) fn probe_runtime(reason: &str, directory: &Path, sid: *mut c_void) -> bool {
    let candidates: &[&str] = match reason {
        "node" => &["node.exe"],
        "npm" => &["npm.cmd"],
        "python" => &["python.exe", "python3.exe"],
        "cargo" => &["cargo.exe", "rustup.exe"],
        _ => &[],
    };
    candidates.iter().any(|name| {
        let path = directory.join(name);
        path.is_file() && has_access(&path, sid, FILE_GENERIC_READ)
    })
}

/// `~/.rustup` 是否真的能被容器读到：`settings.toml` 与至少一个 toolchain 的
/// `cargo.exe` 都要可读。
///
/// 只看根目录不够：可继承 ACE 传播失败时目录能列、文件读不到，`cargo` 会以
/// 「rustup could not choose a version to run」失败，与权限毫无相似之处（实测）。
pub(super) fn probe_rustup(home: &Path, sid: *mut c_void) -> bool {
    let settings = home.join("settings.toml");
    if !(settings.is_file() && has_access(&settings, sid, FILE_GENERIC_READ)) {
        return false;
    }
    let Ok(entries) = std::fs::read_dir(home.join("toolchains")) else {
        return false;
    };
    entries
        .flatten()
        .map(|entry| entry.path().join("bin").join("cargo.exe"))
        .any(|cargo| cargo.is_file() && has_access(&cargo, sid, FILE_GENERIC_READ))
}

/// 给「自动发现的解释器目录」授权，按子树规模设上限。
///
/// 授权代价与子树条目数成正比（`SetNamedSecurityInfoW` 会重算整棵子树的继承：实测
/// `C:\Program Files\nodejs` 0.29 万项 0.26 秒、`D:\ProgramData\Anaconda3` 22 万项
/// 18.4 秒）。因此这里先按 [`GRANT_ENTRY_BUDGET`] 判规模：**已经授过的零成本跳过**，
/// 没授过的只有小目录才自动授，大目录如实报告给用户用 `[sandbox.fs] readable` 显式授权。
///
/// 这条是可用性要求而不是优化：`~/.cargo/bin`（23 项）与 `~/.rustup`（本机 5221 项）
/// 都能落在预算内，于是 `cargo`/`rustc`/`rustup` 开箱即可用；而 `~/.cargo/registry`
/// （7.1 万项）这类目录不会被悄悄拖进来卡住启动。
pub(super) fn grant_runtime_roots(
    runtimes: &mut Vec<RuntimeRoot>,
    skipped: &mut Vec<(PathBuf, String)>,
    sid: *mut c_void,
) {
    let mut kept: Vec<RuntimeRoot> = Vec::with_capacity(runtimes.len());
    for runtime in runtimes.drain(..) {
        // 已经可读的（上次启动授过）零成本跳过：重新写一次 DACL 同样要重算整棵子树。
        if runtime_ready(&runtime, sid) {
            kept.push(runtime);
            continue;
        }
        match subtree_size(&runtime.path) {
            Some(entries) if entries > GRANT_ENTRY_BUDGET => skipped.push((
                runtime.path.clone(),
                format!(
                    "目录过大（超过 {GRANT_ENTRY_BUDGET} 项，自动授权会明显拖慢启动）；\
                     如需使用，请把该目录加入 `[sandbox.fs] readable`，那时代价由你显式选择承担"
                ),
            )),
            _ => match grant(&runtime.path, sid, READ_EXECUTE, true) {
                Ok(()) => kept.push(runtime),
                Err(error) => skipped.push((
                    runtime.path.clone(),
                    format!(
                        "无法授予沙盒访问权（{error}）；如需使用，请把该目录加入 `[sandbox.fs] readable`"
                    ),
                )),
            },
        }
    }
    *runtimes = kept;
}

/// 该解释器根是否已经能被容器读到（与 [`probe_runtime`] 同口径，但按用途分派）。
pub(super) fn runtime_ready(runtime: &RuntimeRoot, sid: *mut c_void) -> bool {
    match runtime.reason {
        "rustup" => probe_rustup(&runtime.path, sid),
        reason => probe_runtime(reason, &runtime.path, sid),
    }
}

/// 子树条目数；超预算就提前返回，读不到时给 `None`（按「不确定」处理）。
///
/// 只在**首次**判定一个未授权的目录时调用一次：预算内的目录随后就被授权，
/// 下次启动 `runtime_ready` 直接命中、不再数。
pub(super) fn subtree_size(root: &Path) -> Option<u64> {
    let mut count: u64 = 0;
    let mut pending: Vec<PathBuf> = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let entries = std::fs::read_dir(&directory).ok()?;
        for entry in entries.flatten() {
            count += 1;
            if count > GRANT_ENTRY_BUDGET {
                return Some(count);
            }
            let path = entry.path();
            if entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false) {
                pending.push(path);
            }
        }
    }
    Some(count)
}
