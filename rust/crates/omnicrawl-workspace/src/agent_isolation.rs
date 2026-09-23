//! 主 Agent 隔离工作区（对应 `omnicrawl/workspace/agent_isolation.py`）：创建、应用变更、清理与启动清扫。
//!
//! 设计目标与 Python 一致：让多个主 Agent 进程（TUI / API / 飞书连接器）各自在独立的
//! git worktree 或本地目录中读写，互不写穿主工作区，结束时可把变更安全应用回主工作区，
//! 并自动清理不再需要的隔离区。
//!
//! - `worktree`：`~/.omnicrawl/agent-worktrees/aw-<实例ID>/` 下建 git worktree（Detached
//!   HEAD，不创建临时分支；共享物理 `.git`，每个实例独立 HEAD / Index / 工作目录）；
//!   可选把主工作区未提交改动同步进来、复制 gitignore 的目录、跑环境脚本；
//! - `local`：把主工作区复制为普通目录（排除 `.git`，不依赖 git），退出时把隔离区内容
//!   镜像回主工作区（只新增/覆盖，不删除）。
//!
//! 清理统一走四层门禁（[`cleanup_eligible`]）；退出收尾与启动清扫共用同一门禁。
//!
//! 与 Python 的差异（详见 `README.md`）：
//!
//! - **进程外信息**：主目录来自 `HOME` / `USERPROFILE`（与 `crates/omnicrawl-cli/src/worktree.rs`
//!   同一套口径），模块级常量换成 [`default_worktrees_root`]；
//! - **日志**：Python 的 `LOGGER.warning/info` 一律丢弃（宿主日志不在本层职责内），
//!   失败仍通过返回值或门禁原因暴露，不改变任何判定；
//! - **实例 ID**：Python 用 `hash(str(path))`（每进程随机加盐），内核侧改用 FNV-1a 稳定散列，
//!   两侧数值不同但语义一致（同进程内同工作区稳定、不同进程不同）；
//! - **超时**：`env_scripts` 的超时用轮询 + 强杀实现（std 没有 `subprocess.run(timeout=)`）。

use std::collections::HashSet;
use std::ffi::OsStr;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use omnicrawl_config::features::agent_workspace::AgentWorkspaceConfig;
use serde_json::{Map, Value};

use crate::paths::{expand_user, home_directory, resolve_path};
use crate::slug::{is_safe_slug, validate_slug, SlugSafetyError, DEFAULT_MAX_LENGTH};

/// 隔离区根目录名：`~/.omnicrawl/agent-worktrees`（仓库外，避免污染 git 状态）。
pub const WORKTREES_DIRNAME: &str = "agent-worktrees";
/// 完整门禁下的保留期（秒）：隔离区创建后未满此时间不清理。
pub const MIN_KEEP_SECONDS: f64 = 3600.0;
/// 启动清扫保留期（秒）：孤儿/过期隔离区超过该时长才会被启动扫描回收。
pub const DEFAULT_SWEEP_MAX_AGE_SECONDS: f64 = 7.0 * 24.0 * 3600.0;
/// 环境脚本的超时（秒）。
pub const ENV_SCRIPT_TIMEOUT_SECONDS: u64 = 1800;
/// 冲突 patch 文件名模板里的实例 ID 前缀。
const PATCH_PREFIX: &str = "agent-";
/// 元数据文件名后缀（`aw-<id>.json` / `sw-<id>.json`）。
const METADATA_SUFFIX: &str = ".json";
/// 提交哈希最短 / 最长长度（SHA-1 / SHA-256）。
const SHA_MIN_LENGTH: usize = 40;
const SHA_MAX_LENGTH: usize = 64;

/// 隔离区根目录：`~/.omnicrawl/agent-worktrees`。
pub fn default_worktrees_root() -> PathBuf {
    home_directory().join(".omnicrawl").join(WORKTREES_DIRNAME)
}

/// 冲突 patch 的默认落盘目录。
pub fn default_patch_dir() -> PathBuf {
    default_worktrees_root().join("patches")
}

/// 隔离区创建、应用或清理失败时的稳定错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentIsolationError {
    message: String,
}

impl AgentIsolationError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

impl std::fmt::Display for AgentIsolationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for AgentIsolationError {}

/// 隔离区会话（主 Agent 隔离区 / SubAgent worktree 共用）。
#[derive(Debug, Clone, PartialEq)]
pub struct IsolationSession {
    pub instance_id: String,
    /// `worktree` / `local` / `subagent`。
    pub mode: String,
    /// 主工作区 git 仓库根（local 模式为工作区根）。
    pub repo_root: PathBuf,
    /// 隔离区实际路径。
    pub worktree_path: PathBuf,
    /// 基线提交（worktree / subagent 模式）。
    pub base_ref: String,
    /// 主工作区路径。
    pub main_workspace: PathBuf,
    /// 创建时间戳（清理保留期用）。
    pub created_at: f64,
    /// SubAgent worktree 分支（主隔离区为空）。
    pub branch_name: String,
}

impl IsolationSession {
    /// 目录名（`aw-<id>` / `sw-<id>`）。
    pub fn entry_name(&self) -> String {
        self.worktree_path
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_default()
    }

    fn with_base_ref(mut self, base_ref: String) -> Self {
        self.base_ref = base_ref;
        self
    }

    fn with_created_at(mut self, created_at: f64) -> Self {
        self.created_at = created_at;
        self
    }
}

/// 进程级隔离会话注册表：同一进程内多个 Agent 创建点（TUI / API / 飞书）共享。
fn registry() -> &'static Mutex<Vec<IsolationSession>> {
    static REGISTRY: OnceLock<Mutex<Vec<IsolationSession>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(Vec::new()))
}

/// 登记隔离会话，供同一进程内其他创建点 / 退出回调复用（按实例 ID 覆盖）。
pub fn register_isolation_session(session: &IsolationSession) {
    let mut guard = registry().lock().unwrap_or_else(|item| item.into_inner());
    match guard
        .iter_mut()
        .find(|item| item.instance_id == session.instance_id)
    {
        Some(slot) => *slot = session.clone(),
        None => guard.push(session.clone()),
    }
}

/// 返回全部已登记隔离会话（副本，按登记顺序）。
pub fn registered_isolation_sessions() -> Vec<IsolationSession> {
    registry()
        .lock()
        .unwrap_or_else(|item| item.into_inner())
        .clone()
}

/// 注销隔离会话（清理后调用）。
pub fn unregister_isolation_session(instance_id: &str) {
    let mut guard = registry().lock().unwrap_or_else(|item| item.into_inner());
    guard.retain(|item| item.instance_id != instance_id);
}

/// 一次 git 调用的结果。
#[derive(Debug, Clone)]
struct GitOutput {
    code: i32,
    stdout: String,
    stderr: String,
}

impl GitOutput {
    /// Python 侧的 `result.stderr or result.stdout`：先 stderr，空则 stdout。
    fn detail(&self) -> String {
        let stderr = self.stderr.trim();
        if stderr.is_empty() {
            self.stdout.trim().to_string()
        } else {
            stderr.to_string()
        }
    }
}

fn run_git(args: &[&str], cwd: &Path, check: bool) -> Result<GitOutput, AgentIsolationError> {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .output()
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                AgentIsolationError::new("未找到 git 可执行文件。")
            } else {
                AgentIsolationError::new(format!("执行 git 失败：{error}"))
            }
        })?;
    let result = GitOutput {
        code: output.status.code().unwrap_or(1),
        stdout: String::from_utf8_lossy(&output.stdout).to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).to_string(),
    };
    if check && result.code != 0 {
        let detail = result.detail();
        let message = if detail.is_empty() {
            format!("git {} 失败。", args.join(" "))
        } else {
            detail
        };
        return Err(AgentIsolationError::new(message));
    }
    Ok(result)
}

/// 判断路径是否位于 git 工作树中。
pub fn is_git_repository(path: &Path) -> bool {
    match run_git(&["rev-parse", "--is-inside-work-tree"], path, false) {
        Ok(result) => result.code == 0 && result.stdout.trim() == "true",
        Err(_) => false,
    }
}

/// 解析 path 所属的 git 仓库根目录。
pub fn resolve_repo_root(path: &Path) -> Result<PathBuf, AgentIsolationError> {
    let result = run_git(&["rev-parse", "--show-toplevel"], path, true)?;
    Ok(resolve_path(Path::new(result.stdout.trim())))
}

/// 解析基线提交：base_branch 优先，其次 base_ref，默认当前 HEAD。
fn resolve_base_ref(
    config: &AgentWorkspaceConfig,
    repo_root: &Path,
) -> Result<String, AgentIsolationError> {
    let branch = config.base_branch.trim().to_string();
    if !branch.is_empty() {
        let reference = format!("refs/heads/{branch}");
        let result = run_git(
            &["rev-parse", "--verify", reference.as_str()],
            repo_root,
            true,
        )?;
        return Ok(result.stdout.trim().to_string());
    }
    let reference = if config.base_ref.trim().is_empty() {
        "HEAD".to_string()
    } else {
        config.base_ref.trim().to_string()
    };
    let result = run_git(
        &["rev-parse", "--verify", reference.as_str()],
        repo_root,
        true,
    )?;
    Ok(result.stdout.trim().to_string())
}

/// 把主工作区当前内容带入新 worktree，并建立新的变更基线。
///
/// `git diff HEAD` 同时包含已暂存和未暂存的 tracked 文件；未跟踪但未被 `.gitignore`
/// 排除的文件另行复制。同步完成后在隔离区创建一个内部基线 commit，避免退出时把主工作区
/// 原有改动重复应用回主树。
fn sync_uncommitted(session: IsolationSession) -> IsolationSession {
    let mut session = session;
    let diff = match run_git(&["diff", "--binary", "HEAD"], &session.repo_root, true) {
        Ok(result) => result.stdout,
        Err(_) => return session,
    };
    if !diff.trim().is_empty() {
        // 显式 LF：Windows 文本模式会写 CRLF，而 git apply 对 CRLF 补丁会整体失败。
        let patch_path = unique_temp_path("patch");
        let written = std::fs::File::create(&patch_path).and_then(|mut handle| {
            handle.write_all(diff.as_bytes())?;
            handle.flush()
        });
        if written.is_ok() {
            let patch_text = patch_path.to_string_lossy().to_string();
            let _ = run_git(
                &["apply", "--whitespace=nowarn", patch_text.as_str()],
                &session.worktree_path,
                false,
            );
        }
        let _ = std::fs::remove_file(&patch_path);
    }

    // git diff 不包含未跟踪文件；只复制未被 .gitignore 排除的项目文件，
    // .env / node_modules 等显式依赖仍由 copy_dirs 负责，避免扩大复制范围。
    if let Ok(result) = run_git(
        &["ls-files", "--others", "--exclude-standard"],
        &session.repo_root,
        true,
    ) {
        for relative_name in result.stdout.lines() {
            let relative_name = relative_name.trim();
            if relative_name.is_empty() {
                continue;
            }
            let source = session.repo_root.join(relative_name);
            let target = session.worktree_path.join(relative_name);
            if !source.exists() || is_symlink(&source) || !source.is_file() {
                continue;
            }
            if let Some(parent) = target.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::copy(&source, &target);
        }
    }

    // 仅在实际同步出内容时创建基线 commit；空 worktree 不制造无意义 commit。
    let status = match run_git(&["status", "--porcelain"], &session.worktree_path, true) {
        Ok(result) => result.stdout,
        Err(_) => return session,
    };
    if !status.trim().is_empty() {
        let _ = run_git(&["add", "-A"], &session.worktree_path, true);
        let message = format!("omnicrawl-sync:{}", session.instance_id);
        let committed = run_git(
            &["commit", "-m", message.as_str(), "--allow-empty-message"],
            &session.worktree_path,
            false,
        );
        let committed = matches!(committed, Ok(result) if result.code == 0);
        if committed {
            if let Ok(head) = run_git(&["rev-parse", "HEAD"], &session.worktree_path, true) {
                // 退出时只回传同步之后的增量；基线提交是内部实现细节，不创建分支。
                session = session.with_base_ref(head.stdout.trim().to_string());
            }
        }
    }
    session
}

/// 复制 gitignore 的目录/文件（如 .env、node_modules）到隔离区。
fn copy_dirs(session: &IsolationSession, copy_dirs: &[String]) {
    for item in copy_dirs {
        let source = session.main_workspace.join(item);
        let target = session.worktree_path.join(item);
        if !source.exists() {
            continue;
        }
        if source.is_dir() {
            let _ = copy_tree(&source, &target, false, false);
        } else {
            if let Some(parent) = target.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::copy(&source, &target);
        }
    }
}

/// 在隔离区内运行环境脚本（如 npm install）。
fn run_env_scripts(session: &IsolationSession, env_scripts: &[String]) {
    for script in env_scripts {
        let _ = run_shell_script(
            script,
            &session.worktree_path,
            Duration::from_secs(ENV_SCRIPT_TIMEOUT_SECONDS),
        );
    }
}

/// local 模式：把主工作区复制为普通目录（排除 .git，不依赖 git）。
fn create_local_isolation(session: &IsolationSession) -> Result<(), AgentIsolationError> {
    std::fs::create_dir_all(&session.worktree_path).map_err(|error| {
        AgentIsolationError::new(format!(
            "复制主工作区到隔离区失败（可改用 mode=worktree 或关闭隔离工作区）：{error}"
        ))
    })?;
    copy_tree(&session.main_workspace, &session.worktree_path, true, true).map_err(|error| {
        AgentIsolationError::new(format!(
            "复制主工作区到隔离区失败（可改用 mode=worktree 或关闭隔离工作区）：{error}"
        ))
    })
}

/// 读取 worktree 目录的 `.git` 指针文件，返回 linked gitdir 路径。
///
/// linked worktree 的 `.git` 是文件而非目录，内容形如
/// `gitdir: <repo>/.git/worktrees/<name>`；路径可能是绝对路径，也可能是相对 worktree
/// 目录的相对路径（相对路径必须以此目录为基准解析，不能相对进程 CWD）。
/// 读取失败 / 格式不符返回 `None`。
fn read_worktree_gitdir(worktree_path: &Path) -> Option<PathBuf> {
    let raw = std::fs::read_to_string(worktree_path.join(".git")).ok()?;
    let raw = raw.trim();
    let rest = raw.strip_prefix("gitdir:")?;
    let gitdir = expand_user(rest.trim());
    let gitdir = if gitdir.is_absolute() {
        gitdir
    } else {
        worktree_path.join(gitdir)
    };
    Some(resolve_path(&gitdir))
}

/// 读取 `gitdir/commondir` 定位 common git dir（路径可相对 gitdir）。
///
/// linked worktree 的 gitdir 只保存 per-worktree refs（HEAD 等），普通分支 ref 与
/// packed-refs 都在 common git dir 中；标准布局下 commondir 为 `..`，
/// 但必须读文件而不是硬编码相对层级。
fn resolve_common_dir(gitdir: &Path) -> Option<PathBuf> {
    let raw = std::fs::read_to_string(gitdir.join("commondir")).ok()?;
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let common = PathBuf::from(raw);
    let common = if common.is_absolute() {
        common
    } else {
        gitdir.join(common)
    };
    Some(resolve_path(&common))
}

/// 解析 common git dir 中 ref 的提交 SHA：loose ref，其次 packed-refs。
fn resolve_ref_sha(common_dir: &Path, ref_name: &str) -> Option<String> {
    if let Ok(text) = std::fs::read_to_string(common_dir.join(ref_name)) {
        let sha = text.trim();
        if is_sha(sha) {
            return Some(sha.to_string());
        }
    }
    let packed = std::fs::read_to_string(common_dir.join("packed-refs")).ok()?;
    let suffix = format!(" {ref_name}");
    for line in packed.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with('^') {
            continue;
        }
        if let Some(sha) = line.strip_suffix(suffix.as_str()) {
            let sha = sha.trim();
            if is_sha(sha) {
                return Some(sha.to_string());
            }
        }
    }
    None
}

/// 纯文件系统解析 worktree gitdir 的 HEAD 提交 SHA。
///
/// - detached HEAD：HEAD 文件内容就是 commit SHA，直接返回；
/// - 符号引用 `ref: refs/heads/<branch>`：先读 commondir 定位 common git dir，
///   再依次查 loose ref 与 packed-refs；
/// - reftable 仓库（refs 不是普通文件）无法纯文件解析，返回 `None`（fail-closed）。
pub fn resolve_worktree_head(gitdir: &Path) -> Option<String> {
    let head_raw = std::fs::read_to_string(gitdir.join("HEAD")).ok()?;
    let head_raw = head_raw.trim();
    if head_raw.is_empty() {
        return None;
    }
    if let Some(ref_name) = head_raw.strip_prefix("ref: ") {
        let ref_name = ref_name.trim();
        if ref_name.is_empty() {
            return None;
        }
        let common_dir = resolve_common_dir(gitdir)?;
        return resolve_ref_sha(&common_dir, ref_name);
    }
    if is_sha(head_raw) {
        Some(head_raw.to_string())
    } else {
        None
    }
}

/// 校验 linked gitdir 在主仓库 `worktrees/` 中仍有反向注册。
fn worktree_registered(gitdir: &Path, worktree_path: &Path) -> bool {
    let Some(common_dir) = resolve_common_dir(gitdir) else {
        return false;
    };
    let Some(name) = gitdir.file_name() else {
        return false;
    };
    let Ok(raw) = std::fs::read_to_string(common_dir.join("worktrees").join(name).join("gitdir"))
    else {
        return false;
    };
    let raw = raw.trim();
    if raw.is_empty() {
        return false;
    }
    let reverse = PathBuf::from(raw);
    let reverse = if reverse.is_absolute() {
        reverse
    } else {
        common_dir.join(reverse)
    };
    resolve_path(&reverse) == resolve_path(&worktree_path.join(".git"))
}

/// 复用已存在的 worktree 隔离区目录，全程不调用 git 子进程。
///
/// 纯文件系统校验（fail-closed），任何一步不满足返回 `None`，由调用方回退到正常创建路径。
fn reuse_worktree_isolation(
    session: &IsolationSession,
    worktrees_root: &Path,
) -> Option<IsolationSession> {
    let worktree_path = session.worktree_path.clone();
    let gitdir = read_worktree_gitdir(&worktree_path)?;
    let expected = resolve_path(&session.repo_root.join(".git").join("worktrees"));
    if !is_relative_to(&gitdir, &expected) {
        return None;
    }
    resolve_worktree_head(&gitdir)?;
    if !worktree_registered(&gitdir, &worktree_path) {
        return None;
    }
    let entry_name = worktree_path.file_name()?.to_string_lossy().to_string();
    let meta = read_isolation_metadata(worktrees_root, &entry_name)?;
    if meta.get("mode").and_then(Value::as_str) != Some("worktree") {
        return None;
    }
    let base_ref = meta.get("base_ref").and_then(Value::as_str)?;
    if base_ref.is_empty() {
        return None;
    }
    let created_at = match meta.get("created_at") {
        None => 0.0,
        Some(value) => python_float(value)?,
    };
    Some(
        session
            .clone()
            .with_base_ref(base_ref.to_string())
            .with_created_at(created_at),
    )
}

/// worktree 模式：创建 Detached HEAD worktree（不建分支）。
fn create_worktree_isolation(
    session: IsolationSession,
    config: &AgentWorkspaceConfig,
) -> Result<IsolationSession, AgentIsolationError> {
    let repo_root = session.repo_root.clone();
    let base_ref = resolve_base_ref(config, &repo_root)?;
    let session = session.with_base_ref(base_ref.clone());
    let path_text = session.worktree_path.to_string_lossy().to_string();
    if let Some(parent) = session.worktree_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| AgentIsolationError::new(format!("创建隔离区父目录失败：{error}")))?;
    }
    let created = if config.detached {
        run_git(
            &[
                "worktree",
                "add",
                "--detach",
                path_text.as_str(),
                base_ref.as_str(),
            ],
            &repo_root,
            true,
        )
    } else {
        let branch_name = format!("omnicrawl/agent/{}", session.instance_id);
        run_git(
            &[
                "worktree",
                "add",
                "-b",
                branch_name.as_str(),
                path_text.as_str(),
                base_ref.as_str(),
            ],
            &repo_root,
            true,
        )
    };
    if let Err(error) = created {
        if session.worktree_path.exists() {
            let _ = std::fs::remove_dir_all(&session.worktree_path);
        }
        return Err(error);
    }
    Ok(session)
}

/// 隔离区元数据文件路径（与隔离区目录名一一对应）。
pub fn isolation_metadata_path(worktrees_root: &Path, entry_name: &str) -> PathBuf {
    worktrees_root.join(format!("{entry_name}{METADATA_SUFFIX}"))
}

/// 把创建时的会话信息与收尾策略持久化，供启动清扫恢复孤儿隔离区。
fn write_isolation_metadata(
    session: &IsolationSession,
    config: &AgentWorkspaceConfig,
    worktrees_root: &Path,
) {
    let mut payload = Map::new();
    payload.insert(
        "instance_id".to_string(),
        Value::from(session.instance_id.clone()),
    );
    payload.insert("mode".to_string(), Value::from(session.mode.clone()));
    payload.insert(
        "repo_root".to_string(),
        Value::from(session.repo_root.to_string_lossy().to_string()),
    );
    payload.insert(
        "main_workspace".to_string(),
        Value::from(session.main_workspace.to_string_lossy().to_string()),
    );
    payload.insert(
        "worktree_path".to_string(),
        Value::from(session.worktree_path.to_string_lossy().to_string()),
    );
    payload.insert(
        "base_ref".to_string(),
        Value::from(session.base_ref.clone()),
    );
    payload.insert(
        "branch_name".to_string(),
        Value::from(session.branch_name.clone()),
    );
    payload.insert("created_at".to_string(), Value::from(session.created_at));
    payload.insert(
        "apply_on_exit".to_string(),
        Value::from(config.apply_on_exit),
    );
    payload.insert(
        "cleanup_on_exit".to_string(),
        Value::from(config.cleanup_on_exit.clone()),
    );
    let text = serde_json::to_string_pretty(&Value::Object(payload)).unwrap_or_default();
    let path = isolation_metadata_path(worktrees_root, &session.entry_name());
    let _ = std::fs::write(path, text);
}

/// 读取隔离区元数据（按目录名定位）；缺失或损坏返回 `None`（不阻断清扫）。
pub fn read_isolation_metadata(
    worktrees_root: &Path,
    entry_name: &str,
) -> Option<Map<String, Value>> {
    let raw = std::fs::read_to_string(isolation_metadata_path(worktrees_root, entry_name)).ok()?;
    let data: Value = serde_json::from_str(&raw).ok()?;
    data.as_object().cloned()
}

/// 删除隔离区元数据文件（目录清理成功后调用）。
fn remove_isolation_metadata(session: &IsolationSession) {
    let root = session
        .worktree_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default();
    let _ = std::fs::remove_file(isolation_metadata_path(&root, &session.entry_name()));
}

/// 创建隔离区会话的参数（Python 的关键字参数面）。
#[derive(Debug, Clone)]
pub struct IsolationOptions {
    /// 主工作区路径（Agent 本来的 workspace_root）。
    pub main_workspace: PathBuf,
    /// 实例 ID（空则随机 UUID 前 12 位）。
    pub instance_id: Option<String>,
    /// 隔离区配置（缺省使用默认配置）。
    pub config: AgentWorkspaceConfig,
    /// 隔离区根目录（默认 `~/.omnicrawl/agent-worktrees`）。
    pub worktrees_root: Option<PathBuf>,
}

impl IsolationOptions {
    pub fn new(main_workspace: impl Into<PathBuf>) -> Self {
        Self {
            main_workspace: main_workspace.into(),
            instance_id: None,
            config: AgentWorkspaceConfig::default(),
            worktrees_root: None,
        }
    }

    pub fn with_instance_id(mut self, instance_id: impl Into<String>) -> Self {
        self.instance_id = Some(instance_id.into());
        self
    }

    pub fn with_config(mut self, config: AgentWorkspaceConfig) -> Self {
        self.config = config;
        self
    }

    pub fn with_worktrees_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.worktrees_root = Some(root.into());
        self
    }
}

/// 创建主 Agent 隔离区会话。
///
/// 返回的 worktree 模式会话已完成未提交改动同步、目录复制与环境脚本执行（目录已存在且
/// 纯文件系统校验通过时直接复用，跳过 `git worktree add` 与上述准备步骤）；local 模式
/// 已复制主工作区。
pub fn create_isolation_session(
    options: IsolationOptions,
) -> Result<IsolationSession, AgentIsolationError> {
    let config = options.config;
    let main_workspace = resolve_path(&expand_user(&options.main_workspace.to_string_lossy()));
    let raw_instance_id = options.instance_id.unwrap_or_default();
    let trimmed = raw_instance_id.trim();
    let instance_id = if trimmed.is_empty() {
        random_instance_id()
    } else {
        trimmed.to_string()
    };
    let instance_id = validate_slug(&instance_id, "隔离区实例 ID", DEFAULT_MAX_LENGTH)
        .map_err(|error: SlugSafetyError| AgentIsolationError::new(error.message()))?;
    let worktrees_root = resolve_path(
        &options
            .worktrees_root
            .unwrap_or_else(default_worktrees_root),
    );
    std::fs::create_dir_all(&worktrees_root).map_err(|error| {
        AgentIsolationError::new(format!(
            "创建隔离区根目录失败：{}，{error}",
            worktrees_root.display()
        ))
    })?;

    let mode = config.mode.clone();
    let worktree_path = worktrees_root.join(format!("aw-{instance_id}"));
    let mut session = IsolationSession {
        instance_id: instance_id.clone(),
        mode: mode.clone(),
        repo_root: main_workspace.clone(),
        worktree_path: worktree_path.clone(),
        base_ref: String::new(),
        main_workspace: main_workspace.clone(),
        created_at: now_seconds(),
        branch_name: String::new(),
    };

    let mut reused = false;
    if mode == "worktree" {
        if !is_git_repository(&main_workspace) {
            return Err(AgentIsolationError::new(
                "worktree 模式要求主工作区位于 git 仓库中；当前不是 git 仓库，\
                 请改用 mode=local 或关闭隔离工作区。",
            ));
        }
        let repo_root = resolve_repo_root(&main_workspace)?;
        session.repo_root = repo_root;
        if worktree_path.exists() {
            // 目录已存在（上一进程 / 上次运行的残留）：纯文件系统校验通过后直接复用，
            // 跳过 git worktree add 与同步 / 复制 / 环境脚本。
            if let Some(reused_session) = reuse_worktree_isolation(&session, &worktrees_root) {
                session = reused_session;
                reused = true;
            }
        }
        if !reused {
            session = create_worktree_isolation(session, &config)?;
            // worktree 首次创建后才同步主树；复用既有会话时不重复同步。
            if config.sync_uncommitted {
                session = sync_uncommitted(session);
            }
            if !config.copy_dirs.is_empty() {
                copy_dirs(&session, &config.copy_dirs);
            }
        }
    } else if mode == "local" {
        // local 模式本身就是主工作区的完整复制，无需额外执行 sync / copy_dirs。
        create_local_isolation(&session)?;
    } else {
        return Err(AgentIsolationError::new(format!(
            "agent_workspace.mode 不受支持：{mode}"
        )));
    }

    if !config.env_scripts.is_empty() && !reused {
        run_env_scripts(&session, &config.env_scripts);
    }

    write_isolation_metadata(&session, &config, &worktrees_root);
    Ok(session)
}

/// local 模式：把隔离区目录内容镜像回主工作区（只新增/覆盖，不删除）。
fn apply_local_isolation_changes(
    session: &IsolationSession,
) -> Result<(usize, Vec<String>), AgentIsolationError> {
    if !session.worktree_path.exists() {
        return Err(AgentIsolationError::new(format!(
            "隔离区目录不存在：{}",
            session.worktree_path.display()
        )));
    }
    let copied = mirror_directory(&session.worktree_path, &session.main_workspace)?;
    Ok((copied, Vec::new()))
}

/// 解析把隔离区变更应用回主工作区时的有效基线。
///
/// 隔离区提交可能已被主工作区分支包含（模型主动把成果同步到主仓库），此时
/// `base_ref..HEAD` 会把已进入主分支的提交再回放一遍。取隔离区 HEAD 与主工作区 HEAD 的
/// 共同祖先作为候选基线，只接受比 `base_ref` 更近（`base_ref` 是其祖先）的候选。
fn effective_apply_base(session: &IsolationSession) -> String {
    let base = if session.base_ref.is_empty() {
        "HEAD".to_string()
    } else {
        session.base_ref.clone()
    };
    let Ok(source_head) = run_git(&["rev-parse", "HEAD"], &session.worktree_path, false) else {
        return base;
    };
    let source_head = source_head.stdout.trim().to_string();
    if source_head.is_empty() {
        return base;
    }
    let common = run_git(
        &["merge-base", source_head.as_str(), "HEAD"],
        &session.repo_root,
        false,
    );
    let candidate = match common {
        Ok(result) if result.code == 0 => result.stdout.trim().to_string(),
        _ => String::new(),
    };
    if candidate.is_empty() || candidate == base {
        return base;
    }
    let ancestor = run_git(
        &[
            "merge-base",
            "--is-ancestor",
            base.as_str(),
            candidate.as_str(),
        ],
        &session.worktree_path,
        false,
    );
    match ancestor {
        Ok(result) if result.code == 0 => candidate,
        _ => base,
    }
}

/// 把隔离区变更安全应用到主工作区。
///
/// 生成相对有效基线的 patch，在主工作区 `git apply --3way` 应用；冲突时由 git 原生写入
/// 冲突标记（Unmerged），用户按标准 Git 冲突流程解决。返回
/// `(实际应用的文件数, 冲突文件列表)`；应用失败时应用文件数为 0（三方应用不是原子操作），
/// 冲突文件列表优先取真正处于未合并状态的文件。
pub fn apply_isolation_changes(
    session: &IsolationSession,
    conflict_patch_dir: Option<&Path>,
) -> Result<(usize, Vec<String>), AgentIsolationError> {
    if !session.worktree_path.exists() {
        return Err(AgentIsolationError::new(format!(
            "隔离区目录不存在：{}",
            session.worktree_path.display()
        )));
    }
    if session.mode == "local" {
        return apply_local_isolation_changes(session);
    }

    // worktree 模式：先把未提交改动 commit 到隔离区分支，确保 diff 基线干净。
    let _ = run_git(&["add", "-A"], &session.worktree_path, false);
    let message = format!("omnicrawl-agent:{}", session.instance_id);
    let _ = run_git(
        &["commit", "-m", message.as_str(), "--allow-empty-message"],
        &session.worktree_path,
        false,
    );
    let base = effective_apply_base(session);
    let diff = run_git(
        &["diff", base.as_str(), "HEAD"],
        &session.worktree_path,
        false,
    )?
    .stdout;
    if diff.trim().is_empty() {
        return Ok((0, Vec::new()));
    }

    let patch_dir = conflict_patch_dir
        .map(Path::to_path_buf)
        .unwrap_or_else(default_patch_dir);
    std::fs::create_dir_all(&patch_dir).map_err(|error| {
        AgentIsolationError::new(format!(
            "创建冲突 patch 目录失败：{}，{error}",
            patch_dir.display()
        ))
    })?;
    let patch_file = patch_dir.join(format!("{PATCH_PREFIX}{}.patch", session.instance_id));
    // 与 sync_uncommitted 相同：patch 必须写 LF，Windows 默认文本模式会转成 CRLF。
    {
        let mut handle = std::fs::File::create(&patch_file).map_err(|error| {
            AgentIsolationError::new(format!(
                "写入冲突 patch 失败：{}，{error}",
                patch_file.display()
            ))
        })?;
        handle.write_all(diff.as_bytes()).map_err(|error| {
            AgentIsolationError::new(format!(
                "写入冲突 patch 失败：{}，{error}",
                patch_file.display()
            ))
        })?;
    }

    // Git 的三方应用以 index 作为 ours：先把当前主工作区内容纳入 index，
    // 这样主树已有未提交修改会成为 ours；应用成功时保留主修改并叠加 AI 增量。
    let _ = run_git(&["add", "-A"], &session.repo_root, true)?;
    let patch_text = patch_file.to_string_lossy().to_string();
    let result = run_git(
        &[
            "apply",
            "--3way",
            "--whitespace=nowarn",
            patch_text.as_str(),
        ],
        &session.repo_root,
        false,
    )?;
    if result.code != 0 {
        let conflicts = unmerged_conflicts(&diff, &session.repo_root);
        return Ok((0, conflicts));
    }

    let changed = count_changed(&diff);
    let _ = std::fs::remove_file(&patch_file);
    Ok((changed, Vec::new()))
}

/// 统计 patch 涉及的文件数。
pub fn count_changed(diff: &str) -> usize {
    diff.lines()
        .filter(|line| line.starts_with("diff --git "))
        .count()
}

/// 从 patch 中提取涉及的文件路径。
pub fn patch_files(diff: &str) -> Vec<String> {
    let mut files: Vec<String> = Vec::new();
    for line in diff.lines() {
        if !line.starts_with("diff --git ") {
            continue;
        }
        if let Some((_, path)) = line.split_once(" b/") {
            files.push(path.trim().to_string());
        }
    }
    files
}

/// 报告三方应用失败后的冲突文件：优先取真正处于未合并状态的文件。
///
/// 三方应用不是原子操作：能合并的文件已写入主工作区，只有真正分叉的文件会留下未合并
/// 条目（UU / AA）。未产生未合并条目（如新增 / 重命名目标已存在）时退回补丁涉及的全部
/// 文件，交由用户按保留的 patch 处理。
pub fn unmerged_conflicts(diff: &str, repo_root: &Path) -> Vec<String> {
    let files = patch_files(diff);
    let output = match run_git(
        &["diff", "--name-only", "--diff-filter=U"],
        repo_root,
        false,
    ) {
        Ok(result) if result.code == 0 => result.stdout,
        _ => String::new(),
    };
    let unmerged: HashSet<String> = output
        .lines()
        .map(|line| line.trim().to_string())
        .filter(|line| !line.is_empty())
        .collect();
    let filtered: Vec<String> = files
        .iter()
        .filter(|path| unmerged.contains(path.as_str()))
        .cloned()
        .collect();
    if filtered.is_empty() {
        files
    } else {
        filtered
    }
}

/// 四层清理门禁判定（fail-closed）：四层全部通过才可安全自动清理。
///
/// 第一层：只清理标记为临时的隔离区（目录名以 `aw-` / `sw-` 开头）。
/// 第二层：跳过当前使用中（`in_use` 实例 ID）与未过期（未到保留期）的隔离区。
/// 第三层：fail-closed 变更检查——目录内有未提交/未跟踪改动则不删。
/// 第四层：有未推送远端 commit（本地领先 origin）也不删。
pub fn cleanup_eligible(
    session: &IsolationSession,
    in_use: Option<&HashSet<String>>,
    now: Option<f64>,
    remote_ref: &str,
) -> (bool, String) {
    let now = now.unwrap_or_else(now_seconds);

    // 第一层：只清理临时隔离区。
    let name = session.entry_name();
    if !name.starts_with("aw-") && !name.starts_with("sw-") {
        return (false, "非临时隔离区（目录名不以 aw-/sw- 开头）".to_string());
    }

    // 第二层：跳过当前使用中。
    if let Some(in_use) = in_use {
        if in_use.contains(&session.instance_id) {
            return (false, "隔离区当前正在使用中".to_string());
        }
    }

    // 第二层：跳过未过期。
    if session.created_at > 0.0 && now - session.created_at < MIN_KEEP_SECONDS {
        let remaining = (MIN_KEEP_SECONDS - (now - session.created_at)) as i64;
        return (false, format!("隔离区未过保留期（还需 {remaining}s）"));
    }

    if (session.mode == "worktree" || session.mode == "subagent") && session.worktree_path.exists()
    {
        // 第三层：fail-closed 变更检查。
        let status = match run_git(&["status", "--porcelain"], &session.worktree_path, false) {
            Ok(result) => result.stdout.trim().to_string(),
            Err(_) => String::new(),
        };
        if !status.is_empty() {
            return (false, "隔离区存在未提交改动".to_string());
        }

        // 第四层：未推送远端 / 未审查的新提交也不删。
        if !remote_ref.is_empty() {
            let base = if session.base_ref.is_empty() {
                "HEAD".to_string()
            } else {
                session.base_ref.clone()
            };
            let range = if session.mode == "subagent" {
                format!("{base}..HEAD")
            } else {
                format!("{remote_ref}..HEAD")
            };
            let keep_reason = if session.mode == "subagent" {
                "隔离区存在未审查的新提交（尚未 apply 回主工作区）"
            } else {
                "隔离区存在未推送远端的 commit"
            };
            let ahead = match run_git(
                &["rev-list", "--count", range.as_str()],
                &session.worktree_path,
                false,
            ) {
                Ok(result) => result.stdout.trim().to_string(),
                Err(_) => String::new(),
            };
            if !ahead.is_empty() && ahead != "0" {
                return (false, keep_reason.to_string());
            }
        }
    }

    (true, "可安全清理".to_string())
}

/// 清理隔离区目录（四层门禁 + worktree 移除 + 元数据删除）。
///
/// `force=true` 跳过门禁，仅用于显式手动清理。
pub fn cleanup_isolation_session(
    session: &IsolationSession,
    in_use: Option<&HashSet<String>>,
    force: bool,
) -> (bool, String) {
    let (eligible, reason) = cleanup_eligible(session, in_use, None, "origin");
    if !force && !eligible {
        return (false, reason);
    }

    if (session.mode == "worktree" || session.mode == "subagent")
        && session.repo_root.exists()
        && session.worktree_path.exists()
    {
        // 先通过 git 移除 worktree 注册信息，再兜底删除目录并 prune。
        let path_text = session.worktree_path.to_string_lossy().to_string();
        let _ = run_git(
            &["worktree", "remove", "--force", path_text.as_str()],
            &session.repo_root,
            false,
        );
        let _ = run_git(&["worktree", "prune"], &session.repo_root, false);
    }

    if session.worktree_path.exists() {
        let _ = std::fs::remove_dir_all(&session.worktree_path);
    }
    if session.worktree_path.exists() {
        return (false, "清理失败：目录仍存在".to_string());
    }
    if !session.branch_name.is_empty() && session.repo_root.exists() {
        // SubAgent worktree 分支在目录移除后一并删除（门禁只放行无变更 / 无新提交的会话）。
        let _ = run_git(
            &["branch", "-D", session.branch_name.as_str()],
            &session.repo_root,
            false,
        );
    }
    remove_isolation_metadata(session);
    (true, "已清理隔离工作区".to_string())
}

/// 生成进程内稳定的实例 ID。
///
/// 以「工作区路径哈希 + 进程启动时刻」组合，保证同一进程内多次调用返回同一 ID、
/// 不同进程各自独立。Python 的工作区哈希是每进程随机加盐的 `hash(str(path))`，
/// 内核侧改用 FNV-1a，因此两侧数值不同（语义一致）。
pub fn process_instance_id(main_workspace: &Path) -> String {
    let main_workspace = resolve_path(&expand_user(&main_workspace.to_string_lossy()));
    let path_hash = stable_path_hash(&main_workspace.to_string_lossy()) % 0xFFFFF;
    format!("w{path_hash:05x}-{:x}", process_boot_seconds())
}

/// 主 Agent 启动时准备隔离工作区。
///
/// 返回 `(Agent 实际 workspace_root, 隔离会话或 None)`；隔离功能未启用或创建失败时返回
/// `(main_workspace, None)`，不阻断启动。
pub fn prepare_isolated_workspace(
    options: IsolationOptions,
) -> (PathBuf, Option<IsolationSession>) {
    let config = options.config.clone();
    let main_workspace = resolve_path(&expand_user(&options.main_workspace.to_string_lossy()));
    if !config.enabled {
        return (main_workspace, None);
    }
    let instance_id = options
        .instance_id
        .clone()
        .unwrap_or_else(|| process_instance_id(&main_workspace));
    let create_options = IsolationOptions {
        main_workspace: main_workspace.clone(),
        instance_id: Some(instance_id),
        config,
        worktrees_root: options.worktrees_root.clone(),
    };
    match create_isolation_session(create_options) {
        Ok(session) => {
            register_isolation_session(&session);
            (session.worktree_path.clone(), Some(session))
        }
        Err(_) => (main_workspace, None),
    }
}

/// 主 Agent 退出时收尾：apply 变更 + 按策略清理，返回人类可读的收尾摘要。
///
/// `cleanup_on_exit=auto` 时统一走四层门禁：四层全部通过的隔离区才删除，
/// 其余保留并在摘要中说明原因。
pub fn finalize_isolation_session(
    session: &IsolationSession,
    apply_on_exit: bool,
    cleanup_on_exit: &str,
    conflict_patch_dir: Option<&Path>,
    in_use: Option<&HashSet<String>>,
) -> String {
    let mut messages: Vec<String> = Vec::new();
    if apply_on_exit {
        match apply_isolation_changes(session, conflict_patch_dir) {
            Ok((changed, conflicts)) => {
                if changed > 0 {
                    messages.push(format!("已应用 {changed} 个文件的变更到主工作区"));
                }
                if !conflicts.is_empty() {
                    let preview: Vec<&str> = conflicts.iter().take(5).map(String::as_str).collect();
                    messages.push(format!(
                        "⚠ {} 个文件冲突，请手动解决：{}",
                        conflicts.len(),
                        preview.join(", ")
                    ));
                }
            }
            Err(error) => messages.push(format!("应用隔离区变更失败：{error}")),
        }
    }
    match cleanup_on_exit {
        "auto" => {
            let (removed, reason) = cleanup_isolation_session(session, in_use, false);
            if removed {
                messages.push("隔离区已清理".to_string());
            } else {
                messages.push(format!("隔离区保留（{reason}）"));
            }
        }
        "keep" => messages.push("隔离区已保留（cleanup_on_exit=keep）".to_string()),
        "never" => messages.push("隔离区已保留（cleanup_on_exit=never）".to_string()),
        _ => {}
    }
    unregister_isolation_session(&session.instance_id);
    if messages.is_empty() {
        "无变更".to_string()
    } else {
        messages.join("；")
    }
}

/// 退出时按 auto 策略收尾 SubAgent worktree 会话，返回人类可读的收尾摘要。
///
/// 与主 Agent 隔离区共用四层门禁：只有无任何变更（未提交 / 未审查新提交）的会话才清理；
/// SubAgent 成果必须由父 Agent 显式审查，绝不在此自动 apply 回主工作区。
pub fn finalize_subagent_worktrees(in_use: Option<&HashSet<String>>, remote_ref: &str) -> String {
    let mut messages: Vec<String> = Vec::new();
    for session in registered_isolation_sessions() {
        if session.mode != "subagent" {
            continue;
        }
        let (eligible, reason) = cleanup_eligible(&session, in_use, None, remote_ref);
        if !eligible {
            messages.push(format!("{} 保留（{reason}）", session.entry_name()));
            continue;
        }
        let (removed_ok, remove_reason) = cleanup_isolation_session(&session, in_use, true);
        if removed_ok {
            unregister_isolation_session(&session.instance_id);
            messages.push(format!("{} 已清理", session.entry_name()));
        } else {
            messages.push(format!(
                "{} 清理失败（{remove_reason}）",
                session.entry_name()
            ));
        }
    }
    messages.join("；")
}

/// 一次启动清扫的汇总结果。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IsolationSweepResult {
    pub removed: Vec<String>,
    /// `(instance_id, 原因)`
    pub kept: Vec<(String, String)>,
    /// 已把变更应用回主工作区的实例。
    pub applied: Vec<String>,
}

/// 启动清扫的参数（Python 的关键字参数面）。
#[derive(Debug, Clone)]
pub struct SweepOptions {
    pub worktrees_root: Option<PathBuf>,
    pub max_age_seconds: f64,
    /// `None` 表示取当前时间。
    pub now: Option<f64>,
    pub in_use: HashSet<String>,
    pub remote_ref: String,
}

impl Default for SweepOptions {
    fn default() -> Self {
        Self {
            worktrees_root: None,
            max_age_seconds: DEFAULT_SWEEP_MAX_AGE_SECONDS,
            now: None,
            in_use: HashSet::new(),
            remote_ref: "origin".to_string(),
        }
    }
}

/// 从清扫条目重建会话：优先元数据，其次用 `.git` 文件推断（仅 worktree）。
///
/// 名称与元数据均做 fail-closed 校验：实例 ID 必须是安全 slug；元数据里的 `worktree_path`
/// 必须解析为本次扫描到的条目目录本身（且在隔离区根目录内），防止被篡改的元数据把清理
/// 或镜像回写重定向到任意路径。
pub fn session_from_sweep_entry(
    worktrees_root: &Path,
    entry: &Path,
    instance_id: &str,
) -> Option<IsolationSession> {
    if !is_safe_slug(instance_id, DEFAULT_MAX_LENGTH) {
        return None;
    }

    let entry_name = entry.file_name()?.to_string_lossy().to_string();
    if let Some(meta) = read_isolation_metadata(worktrees_root, &entry_name) {
        let raw_path = meta.get("worktree_path").and_then(Value::as_str)?;
        let worktree_path = expand_user(raw_path);
        let worktree_path = if worktree_path.is_absolute() {
            worktree_path
        } else {
            worktrees_root.join(worktree_path)
        };
        let resolved = resolve_path(&worktree_path);
        if resolved != resolve_path(entry) || !is_relative_to(&resolved, worktrees_root) {
            return None;
        }
        let repo_root = resolve_path(&expand_user(meta.get("repo_root")?.as_str()?));
        let main_workspace = resolve_path(&expand_user(meta.get("main_workspace")?.as_str()?));
        let created_at = match meta.get("created_at") {
            None => 0.0,
            Some(value) => python_float(value)?,
        };
        let resolved_instance_id = meta
            .get("instance_id")
            .and_then(Value::as_str)
            .unwrap_or(instance_id);
        let mode = meta
            .get("mode")
            .and_then(Value::as_str)
            .unwrap_or("worktree");
        let branch_name = meta
            .get("branch_name")
            .and_then(Value::as_str)
            .unwrap_or_default();
        return Some(IsolationSession {
            instance_id: resolved_instance_id.to_string(),
            mode: mode.to_string(),
            repo_root,
            worktree_path: resolved,
            base_ref: meta
                .get("base_ref")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            main_workspace,
            created_at,
            branch_name: branch_name.to_string(),
        });
    }

    // SubAgent worktree 必须带元数据（branch_name 用于安全清分支）；无元数据的 sw-
    // 目录按「无法识别」保留，不猜测清理。
    if !entry_name.starts_with("aw-") {
        return None;
    }

    // 旧版本留下的无元数据目录：凡带 .git 指针文件的按 worktree 处理。
    let gitdir = read_worktree_gitdir(entry)?;
    let common_dir = resolve_common_dir(&gitdir)?;
    let repo_root = common_dir.parent()?.to_path_buf();
    let created_at = entry
        .metadata()
        .ok()
        .and_then(|metadata| metadata.modified().ok())
        .map(system_time_seconds)
        .unwrap_or(0.0);
    Some(IsolationSession {
        instance_id: instance_id.to_string(),
        mode: "worktree".to_string(),
        repo_root: repo_root.clone(),
        worktree_path: resolve_path(entry),
        base_ref: String::new(),
        main_workspace: repo_root,
        created_at,
        branch_name: String::new(),
    })
}

/// 启动清扫：回收超过保留期的孤儿/过期隔离区。
///
/// - 只处理 `cleanup_on_exit=auto` 的会话；keep / never 一律跳过（尊重配置）。
/// - 创建时 `apply_on_exit=true` 的会话先尝试把变更应用回主工作区（崩溃 / 被强杀时的
///   延迟收尾），随后统一走四层门禁决定是否删除。
/// - 无元数据的旧目录只做可回收判定删除，不做 apply 回写（无法还原基线）。
pub fn sweep_expired_isolation_sessions(options: SweepOptions) -> IsolationSweepResult {
    let root = resolve_path(
        &options
            .worktrees_root
            .unwrap_or_else(default_worktrees_root),
    );
    let now = options.now.unwrap_or_else(now_seconds);
    let mut guarded: HashSet<String> = options.in_use.clone();
    // 同一进程内仍存活的会话全部视为使用中。
    guarded.extend(
        registered_isolation_sessions()
            .into_iter()
            .map(|session| session.instance_id),
    );
    if !root.exists() {
        return IsolationSweepResult::default();
    }

    let mut removed: Vec<String> = Vec::new();
    let mut applied: Vec<String> = Vec::new();
    let mut kept: Vec<(String, String)> = Vec::new();

    let mut entries: Vec<PathBuf> = Vec::new();
    if let Ok(read_dir) = std::fs::read_dir(&root) {
        for entry in read_dir.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            if !name.starts_with("aw-") && !name.starts_with("sw-") {
                continue;
            }
            entries.push(path);
        }
    }
    entries.sort();

    for entry in entries {
        let Some(name) = entry
            .file_name()
            .map(|item| item.to_string_lossy().to_string())
        else {
            continue;
        };
        // 目录名以 `aw-` / `sw-` 开头（各 3 字节），去掉前缀即实例 ID。
        let instance_id = name.get(3..).unwrap_or_default().to_string();
        let Some(session) = session_from_sweep_entry(&root, &entry, &instance_id) else {
            kept.push((instance_id, "无法识别隔离区".to_string()));
            continue;
        };
        if !guarded.is_empty() && guarded.contains(&session.instance_id) {
            kept.push((instance_id, "隔离区当前正在使用中".to_string()));
            continue;
        }
        if session.created_at > 0.0 && now - session.created_at < options.max_age_seconds {
            let remaining = (options.max_age_seconds - (now - session.created_at)) as i64;
            kept.push((
                instance_id,
                format!("隔离区未过清扫保留期（还需 {remaining}s）"),
            ));
            continue;
        }

        let meta = read_isolation_metadata(&root, &name);
        let cleanup_policy = match meta.as_ref().and_then(|item| item.get("cleanup_on_exit")) {
            Some(value) => python_str(value),
            None => "auto".to_string(),
        };
        if cleanup_policy != "auto" {
            kept.push((
                instance_id,
                format!("cleanup_on_exit={cleanup_policy}，跳过清扫"),
            ));
            continue;
        }

        let apply_on_exit = match meta.as_ref() {
            None => false,
            Some(item) => match item.get("apply_on_exit") {
                None => true,
                Some(value) => json_truthy(value),
            },
        };
        // SubAgent worktree 绝不自动 apply 回主工作区：成果必须由父 Agent 显式审查。
        if apply_on_exit && session.mode != "subagent" {
            match apply_isolation_changes(&session, None) {
                Ok((changed, _conflicts)) => {
                    if changed > 0 {
                        applied.push(instance_id.clone());
                    }
                }
                Err(error) => {
                    kept.push((instance_id, format!("应用变更失败：{error}")));
                    continue;
                }
            }
        }

        // 延迟收尾之后仍必须四层门禁全过才能安全自动清理。
        let (eligible, reason) = cleanup_eligible(
            &session,
            Some(&guarded),
            Some(now),
            options.remote_ref.as_str(),
        );
        if !eligible {
            kept.push((instance_id, reason));
            continue;
        }
        let (removed_ok, remove_reason) = cleanup_isolation_session(&session, Some(&guarded), true);
        if removed_ok {
            removed.push(instance_id);
        } else {
            kept.push((instance_id, remove_reason));
        }
    }

    IsolationSweepResult {
        removed,
        kept,
        applied,
    }
}

/// 在守护线程中执行过期隔离区清扫，不阻塞启动关键路径。
pub fn start_background_isolation_sweep(
    worktrees_root: Option<PathBuf>,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("omnicrawl-isolation-sweep".to_string())
        .spawn(move || {
            let options = SweepOptions {
                worktrees_root,
                ..SweepOptions::default()
            };
            let _ = sweep_expired_isolation_sessions(options);
        })
        .expect("创建隔离区清扫线程")
}

/// 判断 path 是否位于 parent 之内（或相等）。
pub fn is_relative_to(path: &Path, parent: &Path) -> bool {
    path.strip_prefix(parent).is_ok()
}

/// 当前时间（秒，含小数）。
pub fn now_seconds() -> f64 {
    system_time_seconds(SystemTime::now())
}

fn system_time_seconds(time: SystemTime) -> f64 {
    time.duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs_f64())
        .unwrap_or(0.0)
}

/// `^[0-9a-f]{40,64}$`：完整提交哈希（SHA-1 / SHA-256）。
fn is_sha(text: &str) -> bool {
    let length = text.chars().count();
    (SHA_MIN_LENGTH..=SHA_MAX_LENGTH).contains(&length)
        && text
            .chars()
            .all(|ch| ch.is_ascii_digit() || ('a'..='f').contains(&ch))
}

/// `float(value)` 的可用子集；无法折算为浮点数时返回 `None`（Python 侧会抛 ValueError）。
fn python_float(value: &Value) -> Option<f64> {
    match value {
        Value::Number(number) => number.as_f64(),
        Value::String(text) => text.trim().parse::<f64>().ok(),
        Value::Bool(flag) => Some(if *flag { 1.0 } else { 0.0 }),
        Value::Null => Some(0.0),
        _ => None,
    }
}

/// Python 的 `bool(value)`（truthiness）。
fn json_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64().map(|item| item != 0.0).unwrap_or(true),
        Value::String(text) => !text.is_empty(),
        Value::Array(items) => !items.is_empty(),
        Value::Object(map) => !map.is_empty(),
    }
}

/// Python 的 `str(value)`：字符串原样，其余按 JSON 形状给出可读文本。
fn python_str(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => "None".to_string(),
        Value::Bool(flag) => {
            if *flag {
                "True".to_string()
            } else {
                "False".to_string()
            }
        }
        Value::Number(number) => number.to_string(),
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

/// 进程启动时刻（秒）：std 没有 `GetProcessTimes`，退化为「进程内首次调用的时刻」，
/// 同一进程内稳定、不同进程不同（与 Python 语义一致）。
fn process_boot_seconds() -> u64 {
    static BOOT: OnceLock<u64> = OnceLock::new();
    *BOOT.get_or_init(|| now_seconds().max(0.0) as u64)
}

/// FNV-1a 64 位散列：跨进程稳定的路径哈希。
fn stable_path_hash(text: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// `uuid4().hex[:12]` 的等价物。
fn random_instance_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let pid = std::process::id() as u128;
    let mixed = nanos
        .wrapping_mul(0x9e37_79b9_7f4a_7c15)
        .wrapping_add(pid.wrapping_mul(0x2545_f491_4f6c_dd1d));
    format!("{:012x}", mixed & 0xFFFF_FFFF_FFFF)
}

fn unique_temp_path(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!(
        "omnicrawl-{tag}-{}-{nanos:x}.patch",
        std::process::id()
    ))
}

fn is_symlink(path: &Path) -> bool {
    std::fs::symlink_metadata(path)
        .map(|metadata| metadata.file_type().is_symlink())
        .unwrap_or(false)
}

/// 递归复制目录树；`exclude_git` 跳过任意层级的 `.git`（对映 `shutil.ignore_patterns(".git")`）。
fn copy_tree(
    source: &Path,
    target: &Path,
    exclude_git: bool,
    preserve_symlinks: bool,
) -> std::io::Result<()> {
    std::fs::create_dir_all(target)?;
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let name = entry.file_name();
        if exclude_git && name == OsStr::new(".git") {
            continue;
        }
        let path = entry.path();
        let destination = target.join(&name);
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            if preserve_symlinks {
                recreate_symlink(&path, &destination)?;
            } else {
                let metadata = std::fs::metadata(&path)?;
                if metadata.is_dir() {
                    copy_tree(&path, &destination, exclude_git, preserve_symlinks)?;
                } else {
                    copy_file(&path, &destination)?;
                }
            }
        } else if file_type.is_dir() {
            copy_tree(&path, &destination, exclude_git, preserve_symlinks)?;
        } else {
            copy_file(&path, &destination)?;
        }
    }
    Ok(())
}

fn copy_file(source: &Path, target: &Path) -> std::io::Result<()> {
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::copy(source, target)?;
    Ok(())
}

fn recreate_symlink(source: &Path, target: &Path) -> std::io::Result<()> {
    let link = std::fs::read_link(source)?;
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(link, target)
    }
    #[cfg(windows)]
    {
        let is_dir = std::fs::metadata(source)
            .map(|metadata| metadata.is_dir())
            .unwrap_or(false);
        if is_dir {
            std::os::windows::fs::symlink_dir(link, target)
        } else {
            std::os::windows::fs::symlink_file(link, target)
        }
    }
}

/// 把隔离区目录内容镜像回主工作区（只新增/覆盖，不删除），返回复制的文件数。
///
/// 对映 `os.walk(followlinks=False)`：不跟随符号链接目录（只补建同名空目录），
/// 符号链接文件一律跳过。
fn mirror_directory(source_root: &Path, target_root: &Path) -> Result<usize, AgentIsolationError> {
    let mut copied = 0usize;
    let mut stack: Vec<PathBuf> = vec![source_root.to_path_buf()];
    while let Some(current) = stack.pop() {
        let entries = std::fs::read_dir(&current).map_err(|error| {
            AgentIsolationError::new(format!(
                "读取隔离区目录失败：{}，{error}",
                current.display()
            ))
        })?;
        for entry in entries {
            let entry = entry.map_err(|error| {
                AgentIsolationError::new(format!(
                    "读取隔离区目录失败：{}，{error}",
                    current.display()
                ))
            })?;
            let path = entry.path();
            let relative = path
                .strip_prefix(source_root)
                .unwrap_or(&path)
                .to_path_buf();
            let target = target_root.join(&relative);
            let file_type = entry.file_type().map_err(|error| {
                AgentIsolationError::new(format!("读取隔离区目录失败：{}，{error}", path.display()))
            })?;
            if file_type.is_symlink() {
                let points_to_dir = std::fs::metadata(&path)
                    .map(|metadata| metadata.is_dir())
                    .unwrap_or(false);
                if points_to_dir {
                    std::fs::create_dir_all(&target).map_err(|error| {
                        AgentIsolationError::new(format!(
                            "镜像隔离区目录回主工作区失败：{}，{error}",
                            target.display()
                        ))
                    })?;
                }
                continue;
            }
            if file_type.is_dir() {
                std::fs::create_dir_all(&target).map_err(|error| {
                    AgentIsolationError::new(format!(
                        "镜像隔离区目录回主工作区失败：{}，{error}",
                        target.display()
                    ))
                })?;
                stack.push(path);
                continue;
            }
            copy_file(&path, &target).map_err(|error| {
                AgentIsolationError::new(format!(
                    "镜像隔离区文件回主工作区失败：{}，{error}",
                    target.display()
                ))
            })?;
            copied += 1;
        }
    }
    Ok(copied)
}

/// 在隔离区内运行一条 shell 脚本（超时后强杀），返回退出码与输出。
fn run_shell_script(
    script: &str,
    cwd: &Path,
    timeout: Duration,
) -> Result<GitOutput, AgentIsolationError> {
    let mut command = shell_command(script);
    let mut child = command
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| {
            AgentIsolationError::new(format!("运行环境脚本失败：{script} → {error}"))
        })?;

    // 先起读取线程，避免脚本输出写满管道导致子进程阻塞（等价 subprocess.run 的 communicate）。
    let stdout_handle = child.stdout.take().map(spawn_reader);
    let stderr_handle = child.stderr.take().map(spawn_reader);
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(AgentIsolationError::new(format!(
                        "运行环境脚本超时：{script} → {}s",
                        timeout.as_secs()
                    )));
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(error) => {
                return Err(AgentIsolationError::new(format!(
                    "运行环境脚本失败：{script} → {error}"
                )))
            }
        }
    };
    Ok(GitOutput {
        code: status.code().unwrap_or(1),
        stdout: join_reader(stdout_handle),
        stderr: join_reader(stderr_handle),
    })
}

fn spawn_reader<R: Read + Send + 'static>(mut reader: R) -> std::thread::JoinHandle<String> {
    std::thread::spawn(move || {
        let mut text = String::new();
        let _ = reader.read_to_string(&mut text);
        text
    })
}

fn join_reader(handle: Option<std::thread::JoinHandle<String>>) -> String {
    match handle {
        Some(handle) => handle.join().unwrap_or_default(),
        None => String::new(),
    }
}

fn shell_command(script: &str) -> Command {
    #[cfg(windows)]
    {
        let mut command = Command::new("cmd");
        command.arg("/C").arg(script);
        command
    }
    #[cfg(not(windows))]
    {
        let mut command = Command::new("/bin/sh");
        command.arg("-c").arg(script);
        command
    }
}
