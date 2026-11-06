//! cgroup v2 资源限额：给沙盒进程一个内存/进程数/CPU 的硬上限。
//!
//! 设计见 `sandbox_linux_design.md` §8 与 §22.1。以下都是在 **Debian 13 / 内核 7.0 /
//! systemd 会话** 上真机跑出来的约束，不是推测：
//!
//! 1. **限额是可用性手段，不是安全边界**。安全边界在 Landlock。因此本模块**从不**
//!    让命令失败——拿不到 cgroup 就如实说"没设限额"，命令照常执行。反过来（拿不到
//!    限额就拒绝执行）会让本装置在容器、CI、非 systemd 发行版上完全不可用。
//! 2. **不能建在本进程所在的 `session-N.scope` 里**。那是 systemd 的 leaf cgroup：
//!    能建子目录、能迁入，但 `pids.max`/`memory.max` 一律 `EPERM`——它的
//!    `cgroup.subtree_control` 不可写，子目录永远长不出限额文件。真机第一次实现就是
//!    这么错的：限额"写成功"了（其实没写成），20 个进程照样起得来。
//!    必须用它的**父级**（通常是 `user.slice/user-0.slice`）。
//! 3. **必须由 launcher 自己写 `cgroup.procs`**，不能在宿主 fork 前把子进程归组：
//!    cgroup 归属由写 `cgroup.procs` 的那一刻决定，宿主拿到 pid 时子进程可能已经
//!    `execvp` 并 fork 出后代，那些后代就落在别的 cgroup 里，限额形同虚设。
//!    真机验证：迁入后 fork 到第 8 个（`pids.max=8`）回 `EAGAIN`，裸跑 20 个全过。
//! 4. **同名复用，不按命令创建**。cgroup 名取宿主 pid：一个会话最多一个沙盒 cgroup，
//!    因此不会因为"某条命令忘了清理"而无限累积。
//! 5. **判定资格必须靠"真能写出限额文件"**，不能只看"能建目录"。见 `can_write_limits`。

use std::path::{Path, PathBuf};

use omnicrawl_config::features::sandbox::SandboxLimits;

/// cgroup v2 的固定挂载点。v1 或未挂载时这里不存在，整个模块降级。
const CGROUP_ROOT: &str = "/sys/fs/cgroup";

/// 沙盒子 cgroup 名字前缀（后面接宿主 pid）。
const SCOPE_PREFIX: &str = "omnicrawl-sandbox";

/// 一次成功建立的限额。
///
/// 字段各自独立：某项写不进去不影响其余几项。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Applied {
    /// 子 cgroup 的路径（launcher 要迁入这里）。
    pub path: PathBuf,
    /// 真正写进去的限额项。
    pub items: Vec<String>,
    /// 想设但没能设上的项及原因（如实告知，不假装限额生效）。
    pub skipped: Vec<String>,
}

impl Applied {
    /// 一句话摘要，供 `/sandbox` 与诊断展示。
    pub fn describe(&self) -> String {
        if self.items.is_empty() {
            return format!(
                "资源限额未启用（{}）",
                self.skipped
                    .first()
                    .cloned()
                    .unwrap_or_else(|| "无可用的 cgroup 控制器".to_string())
            );
        }
        let mut text = format!("资源限额：{}", self.items.join("、"));
        if !self.skipped.is_empty() {
            text.push_str(&format!("（未设：{}）", self.skipped.join("、")));
        }
        text
    }
}

/// 找一个能**设限额**的父目录，并在它上面启用控制器。
///
/// 真机（Debian 13 / 内核 7.0 / systemd）验证到的事实，与"就近取自己所在的 cgroup"相反：
///
/// | 位置 | 建子目录 | 写 `pids.max` | 写 `memory.max` | 迁入 |
/// | --- | --- | --- | --- | --- |
/// | `session-N.scope`（本进程所在） | 可以 | **EPERM** | **EPERM** | 可以 |
/// | `user-0.slice`（session 的父级） | 可以 | 可以 | 可以 | 可以 |
///
/// 原因：`session-N.scope` 是 systemd 建的 **leaf cgroup**，它的 `cgroup.subtree_control`
/// 不可写（`EPERM`），于是其子目录永远长不出 `pids.max`/`memory.max`。能建目录、
/// 能迁入、**就是不能设限额**——只看"能否建目录"会得到一次自我感觉良好的假成功。
///
/// 因此候选顺序是：本进程所在 cgroup 的**父级**优先（通常是 slice），再退到根。
/// 判定标准是"这个目录下的子 cgroup 真能写出限额文件"，不只是能建目录。
fn writable_parent(limits: &SandboxLimits) -> Option<PathBuf> {
    let root = Path::new(CGROUP_ROOT);
    // v2 的标志文件；v1 或未挂载时不存在。
    if !root.join("cgroup.controllers").exists() {
        return None;
    }
    let scope = current_scope();
    let candidates = [
        // 父级优先：systemd 的 slice 可设限额，scope 不行。
        scope.as_deref().and_then(Path::parent).map(Path::to_path_buf),
        scope,
        Some(root.to_path_buf()),
    ];
    for candidate in candidates.into_iter().flatten() {
        if !can_create_child(&candidate) {
            continue;
        }
        let available =
            std::fs::read_to_string(candidate.join("cgroup.controllers")).unwrap_or_default();
        // 先补启用控制器（已启用时幂等），再验证真的能写出限额文件。
        for controller in ["pids", "memory"] {
            if available.split_whitespace().any(|name| name == controller) {
                enable_controller(&candidate, controller);
            }
        }
        if limits.cpu_rate_percent > 0 && available.split_whitespace().any(|name| name == "cpu") {
            enable_controller(&candidate, "cpu");
        }
        if can_write_limits(&candidate) {
            return Some(candidate);
        }
    }
    None
}

/// 建一个临时子 cgroup，确认**限额文件真的可写**，然后立刻删掉。
///
/// 这是本模块唯一可靠的资格判定：`can_create_child` 只回答"能不能建目录"，
/// 而真机上"能建目录但设不了任何限额"是常态（leaf scope）。
fn can_write_limits(parent: &Path) -> bool {
    let probe = parent.join(".omnicrawl-limit-probe");
    if std::fs::create_dir(&probe).is_err() {
        return false;
    }
    let writable = std::fs::write(probe.join("pids.max"), "max").is_ok()
        || std::fs::write(probe.join("memory.max"), "max").is_ok();
    let _ = std::fs::remove_dir(&probe);
    writable
}

/// `/proc/self/cgroup` 给出的本进程所属 cgroup（v2 只有一行 `0::<path>`）。
fn current_scope() -> Option<PathBuf> {
    let text = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    let path = text.lines().find_map(|line| line.strip_prefix("0::"))?.trim();
    if path.is_empty() || path == "/" {
        return None;
    }
    Some(Path::new(CGROUP_ROOT).join(path.trim_start_matches('/')))
}

fn can_create_child(parent: &Path) -> bool {
    // 真正建一次再删比读权限位可靠：cgroupfs 的权限由内核按归属判定。
    let probe = parent.join(".omnicrawl-probe");
    match std::fs::create_dir(&probe) {
        Ok(()) => {
            let _ = std::fs::remove_dir(&probe);
            true
        }
        Err(_) => false,
    }
}

/// 在 `parent` 的子树控制里启用 `controller`，让子 cgroup 长出对应限额文件。
///
/// 必须在**创建子 cgroup 之前**做：内核只给"父级已启用该控制器"的子 cgroup 建接口文件。
fn enable_controller(parent: &Path, controller: &str) -> bool {
    let file = parent.join("cgroup.subtree_control");
    // 已经启用时再写一次是幂等的（内核接受重复的 `+memory`）。
    std::fs::write(&file, format!("+{controller}")).is_ok()
}

/// 建立（或复用）本会话的沙盒 cgroup，按 `limits` 写入限额。
///
/// 返回 `Err` 只表示"完全没能建起来"，调用方据此不设置任何限额并如实说明原因。
pub fn create(limits: &SandboxLimits) -> Result<Applied, String> {
    let parent = writable_parent(limits).ok_or_else(|| "本机没有可写的 cgroup v2 委派".to_string())?;
    // 名字取宿主 pid：同一会话复用同一个 cgroup，不随命令数累积。
    let path = parent.join(format!("{SCOPE_PREFIX}-{}", std::process::id()));

    let mut applied = Applied {
        path: path.clone(),
        items: Vec::new(),
        skipped: Vec::new(),
    };

    // 已存在（上一轮复用）时 `create_dir` 会失败，属正常。
    if let Err(error) = std::fs::create_dir(&path) {
        if error.kind() != std::io::ErrorKind::AlreadyExists {
            return Err(format!("无法在 {} 下建立子 cgroup：{error}", parent.display()));
        }
    }

    // 判定标准是**限额文件是否真的存在**，不是"控制器是否启用成功"。
    // 真机上这两者会不一致：控制器在父级启用成功、子目录也建起来了，
    // 但若中间层没启用，子目录里就根本没有 `pids.max`。只看启用结果会得到假限额。
    if limits.max_processes > 0 {
        write_limit(&path, &mut applied, "pids.max", limits.max_processes.to_string());
    }
    if limits.max_memory_mb > 0 {
        let bytes = limits.max_memory_mb.saturating_mul(1024 * 1024);
        write_limit(&path, &mut applied, "memory.max", bytes.to_string());
    } else {
        applied.skipped.push("内存（配置为不限）".to_string());
    }
    if limits.cpu_rate_percent > 0 {
        // 周期固定 100000 µs（内核默认），配额按百分比折算。
        let quota = (limits.cpu_rate_percent as u64).saturating_mul(1000);
        write_limit(&path, &mut applied, "cpu.max", format!("{quota} 100000"));
    }

    if applied.items.is_empty() {
        // 一个限额也没设上：留着空 cgroup 只会误导（`/sandbox` 会显示"已设限额"）。
        let _ = std::fs::remove_dir(&path);
        return Err(format!(
            "cgroup 建起来了但限额文件都不可写（{}）",
            applied.skipped.join("；")
        ));
    }
    Ok(applied)
}

/// 写一条限额文件。失败记进 `skipped`，不让整件事失败。
fn write_limit(path: &Path, applied: &mut Applied, file: &str, content: String) {
    // 判断"能不能写"的唯一可靠方式是写一次。文件可能压根不存在（控制器未在中间层启用），
    // 也可能存在但只读（父级未委派）——两种情况都要如实记进 `skipped`。
    match std::fs::write(path.join(file), &content) {
        Ok(()) => applied
            .items
            .push(format!("{file}={}", content.trim())),
        Err(error) => applied.skipped.push(format!("{file}（{error}）")),
    }
}

/// 把当前进程迁进这个 cgroup。launcher 在施加 Landlock 之前调用。
///
/// 迁移之后**本进程与它之后的全部后代**都受该 cgroup 限额约束，这正是要的效果：
/// `execvp` 之后真正跑起来的命令及其子进程都在里面。
pub fn join(path: &Path) -> Result<(), String> {
    let pid = std::process::id();
    std::fs::write(path.join("cgroup.procs"), pid.to_string())
        .map_err(|error| format!("无法把进程迁入 {}：{error}", path.display()))?;
    // 写入成功不等于迁入成功：本进程可能已是该 cgroup 的多线程组成员
    // （真机上迁入后内核回 `EOPNOTSUPP`，而写入本身"成功"）。回读确认，别报假成功。
    let actual = std::fs::read_to_string("/proc/self/cgroup").unwrap_or_default();
    let expected = path.to_string_lossy();
    let expected = expected.trim_start_matches(CGROUP_ROOT);
    if actual.lines().any(|line| {
        line.strip_prefix("0::")
            .is_some_and(|value| value.trim() == expected.trim_end_matches('/'))
    }) {
        Ok(())
    } else {
        Err(format!(
            "写入 cgroup.procs 后仍在原 cgroup（{actual}）。限额未生效"
        ))
    }
}

/// 删掉这个子 cgroup（测试与宿主退出前使用；正常路径靠同名复用，不依赖它）。
///
/// 里面还有进程时内核会拒绝删除，此时静默忽略：宿主不因此报错。
pub fn cleanup(path: &Path) {
    let _ = std::fs::remove_dir(path);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn describe_reports_what_was_actually_set() {
        let applied = Applied {
            path: PathBuf::from("/sys/fs/cgroup/x"),
            items: vec!["pids.max=64".to_string()],
            skipped: vec!["内存（memory 未委派）".to_string()],
        };
        let text = applied.describe();
        assert!(text.contains("pids.max=64"), "{text}");
        // 没设上的必须出现在文案里：假装限额生效比不设更糟。
        assert!(text.contains("未设"), "{text}");
        assert!(text.contains("memory"), "{text}");
    }

    #[test]
    fn describe_admits_a_total_miss() {
        let applied = Applied {
            path: PathBuf::from("/sys/fs/cgroup/x"),
            items: Vec::new(),
            skipped: vec!["本机没有可写的 cgroup v2 委派".to_string()],
        };
        let text = applied.describe();
        assert!(text.starts_with("资源限额未启用"), "{text}");
    }

    #[test]
    fn a_machine_without_cgroup_v2_degrades_instead_of_failing_the_command() {
        // 本函数在任何平台上都必须只表达"有没有"，绝不 panic：限额拿不到时要能照常执行。
        let outcome = create(&SandboxLimits::default());
        match outcome {
            Ok(applied) => {
                // 真跑在 cgroup v2 上时，至少写进去一项，且路径在预期前缀下。
                assert!(!applied.items.is_empty(), "成功时应有至少一项限额");
                assert!(applied
                    .path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with(SCOPE_PREFIX)));
            }
            Err(reason) => assert!(!reason.is_empty(), "失败也要给出可读原因"),
        }
    }
}
