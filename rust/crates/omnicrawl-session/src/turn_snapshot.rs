//! 基于 git diff 的工作区轮次快照与原子恢复：对齐 Python `omnicrawl/state/turn_snapshot.py`。
//!
//! 只依赖用户仓库自身的 Git 状态，不创建任何对象库：
//!
//! - 轮次起点/终点各执行一次 `git diff HEAD --binary --full-index`，把「未提交的已跟踪修改」
//!   落盘为补丁；同时用 `git ls-files --others --exclude-standard` 记录未跟踪文件清单；
//! - 恢复时先校验当前状态仍等于轮次终点（冲突检查），再 `git reset --hard HEAD` 复位、
//!   `git apply` 轮次起点补丁，最后删除本轮新增的未跟踪文件；
//! - 非 Git 工作区返回 `has_head = false` 的空快照，由调用方降级为「本轮禁用 undo」。
//!
//! 已知限制（与 Python 一致）：轮次中被删除的未跟踪文件没有内容副本，只能提示；
//! 被 `.gitignore` 忽略的受控运行态不参与快照；旧版 shadow.git 快照事件无法解析。
//!
//! 与 Python 的差异：`git` 子进程的超时由内核自己轮询实现（`std::process` 没有内置超时），
//! 超时文案里的参数表按 Rust 的 `Debug` 形式给出。

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// 单条 Git 子命令的最长等待时间。
///
/// 快照只是轮次 undo 的前置，绝不允许工作区遍历把整轮对话卡死；超时按失败处理，
/// 由上层降级为「本轮禁用 undo」。
pub const GIT_COMMAND_TIMEOUT_SECONDS: u64 = 120;

/// `has_head` 探测结果的缓存存活时间：避免每轮都支付一次 `rev-parse` 的进程启动成本。
const HEAD_CACHE_TTL_SECONDS: f64 = 60.0;

/// 快照创建或恢复失败。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotError {
    message: String,
    conflict: bool,
}

impl SnapshotError {
    pub fn failed(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            conflict: false,
        }
    }

    pub fn conflict(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            conflict: true,
        }
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    /// 是否为「当前文件树不再等于轮次结束状态」的冲突。
    pub fn is_conflict(&self) -> bool {
        self.conflict
    }
}

impl std::fmt::Display for SnapshotError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for SnapshotError {}

/// 工作区在某一时刻的状态：HEAD 之上的未提交修改 + 未跟踪文件清单。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeSnapshot {
    /// `git diff HEAD --binary --full-index` 的输出（含暂存区与工作区修改的合并视图）。
    pub patch: Vec<u8>,
    /// `git ls-files --others --exclude-standard` 的文件列表（POSIX 相对路径）。
    pub untracked: Vec<String>,
    /// 为假表示工作区不是有 HEAD 的 Git 仓库，无法快照。
    pub has_head: bool,
}

impl WorktreeSnapshot {
    fn empty() -> Self {
        Self {
            patch: Vec::new(),
            untracked: Vec::new(),
            has_head: false,
        }
    }
}

/// 用 git 差分捕获/恢复单个 Git 工作区，不触碰用户仓库的引用与历史。
pub struct WorktreeSnapshotStore;

impl Default for WorktreeSnapshotStore {
    fn default() -> Self {
        Self::new()
    }
}

impl WorktreeSnapshotStore {
    pub fn new() -> Self {
        Self
    }

    /// 带短 TTL 缓存地探测工作区是否有可回退的 HEAD。
    pub fn has_head(&self, workspace: &Path) -> bool {
        let workspace = resolve_workspace(workspace);
        let now = Instant::now();
        if let Some(cached) = head_cache().lock().expect("HEAD 缓存锁").get(&workspace) {
            if now.duration_since(cached.1).as_secs_f64() < HEAD_CACHE_TTL_SECONDS {
                return cached.0;
            }
        }
        let detected = self.detect_head(&workspace).unwrap_or(false);
        head_cache()
            .lock()
            .expect("HEAD 缓存锁")
            .insert(workspace, (detected, now));
        detected
    }

    /// 捕获工作区当前状态；非 Git 仓库返回 `has_head = false` 的空快照。
    pub fn capture(&self, workspace: &Path) -> Result<WorktreeSnapshot, SnapshotError> {
        let workspace = resolve_workspace(workspace);
        if !workspace.is_dir() {
            return Ok(WorktreeSnapshot::empty());
        }
        if !self.has_head(&workspace) {
            return Ok(WorktreeSnapshot::empty());
        }
        // quotepath=false：非 ASCII 路径输出原生 UTF-8，diff/apply 两侧一致。
        // 不强制 core.autocrlf：Windows 默认 autocrlf=true 下工作区是 CRLF、HEAD 是 LF，
        // 强制关掉会把行尾差异误判成修改。
        let patch = self.git_bytes(
            &[
                "-c".to_string(),
                "core.quotepath=false".to_string(),
                "diff".to_string(),
                "--binary".to_string(),
                "--full-index".to_string(),
                "HEAD".to_string(),
                "--".to_string(),
            ],
            None,
            Some(&workspace),
        )?;
        let untracked = self.git_stdout(
            &[
                "ls-files".to_string(),
                "--others".to_string(),
                "--exclude-standard".to_string(),
            ],
            Some(&workspace),
        )?;
        let lines: Vec<String> = crate::records::split_lines_python(&untracked)
            .into_iter()
            .filter(|line| !line.trim().is_empty())
            .collect();
        Ok(WorktreeSnapshot {
            patch,
            untracked: lines,
            has_head: true,
        })
    }

    /// 当前状态完全匹配 `expected` 时，把工作区切换到 `target`。
    ///
    /// 冲突检查与补丁应用在首次内容写入前完成；返回「无法恢复」的提示列表
    /// （轮次中被删除、且没有内容副本的未跟踪文件路径）。
    pub fn transition(
        &self,
        workspace: &Path,
        expected: &WorktreeSnapshot,
        target: &WorktreeSnapshot,
    ) -> Result<Vec<String>, SnapshotError> {
        let workspace = resolve_workspace(workspace);
        if !expected.has_head || !target.has_head {
            return Err(SnapshotError::failed("工作区不是 Git 仓库，无法回退。"));
        }

        let current = self.capture(&workspace)?;
        if !current.has_head {
            return Err(SnapshotError::conflict("工作区不再是 Git 仓库，拒绝回退。"));
        }
        if current.patch != expected.patch
            || sorted_unique(&current.untracked) != sorted_unique(&expected.untracked)
        {
            return Err(SnapshotError::conflict(
                "工作区在轮次结束后又被修改，拒绝回退。",
            ));
        }

        // 复位到 HEAD 干净状态。用 reset --hard 而非 checkout --force：前者会同步清掉
        // index 中 HEAD 不存在的已暂存文件（它们会在 apply 起点补丁时被重新创建）。
        self.git(
            &[
                "reset".to_string(),
                "--hard".to_string(),
                "HEAD".to_string(),
            ],
            None,
            Some(&workspace),
        )?;
        if !target.patch.is_empty() {
            self.git(
                &[
                    "-c".to_string(),
                    "core.quotepath=false".to_string(),
                    "apply".to_string(),
                    "--binary".to_string(),
                    "--whitespace=nowarn".to_string(),
                    "-".to_string(),
                ],
                Some(&target.patch),
                Some(&workspace),
            )?;
        }

        let created: Vec<String> = difference(&expected.untracked, &target.untracked);
        for relative in created {
            let path = workspace_path(&workspace, &relative)?;
            if path.is_file() || path.is_symlink() {
                std::fs::remove_file(&path).map_err(|error| {
                    SnapshotError::failed(format!("无法删除未跟踪文件 {relative}：{error}"))
                })?;
            }
        }

        Ok(difference(&target.untracked, &expected.untracked))
    }

    fn detect_head(&self, workspace: &Path) -> Result<bool, SnapshotError> {
        match self.git_stdout(
            &[
                "rev-parse".to_string(),
                "--verify".to_string(),
                "--quiet".to_string(),
                "HEAD^{commit}".to_string(),
            ],
            Some(workspace),
        ) {
            Ok(_) => Ok(true),
            Err(_) => Ok(false),
        }
    }

    fn git(
        &self,
        arguments: &[String],
        input: Option<&[u8]>,
        cwd: Option<&Path>,
    ) -> Result<GitOutput, SnapshotError> {
        run_git(arguments, input, cwd, GIT_COMMAND_TIMEOUT_SECONDS)
    }

    fn git_stdout(
        &self,
        arguments: &[String],
        cwd: Option<&Path>,
    ) -> Result<String, SnapshotError> {
        Ok(String::from_utf8_lossy(&self.git(arguments, None, cwd)?.stdout).to_string())
    }

    fn git_bytes(
        &self,
        arguments: &[String],
        input: Option<&[u8]>,
        cwd: Option<&Path>,
    ) -> Result<Vec<u8>, SnapshotError> {
        Ok(self.git(arguments, input, cwd)?.stdout)
    }
}

struct GitOutput {
    stdout: Vec<u8>,
}

fn run_git(
    arguments: &[String],
    input: Option<&[u8]>,
    cwd: Option<&Path>,
    timeout_seconds: u64,
) -> Result<GitOutput, SnapshotError> {
    let mut command = Command::new("git");
    command
        .args(arguments)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(directory) = cwd {
        command.current_dir(directory);
    }
    let mut child = command
        .spawn()
        .map_err(|error| SnapshotError::failed(format!("无法启动 Git：{error}")))?;

    let mut stdin = child.stdin.take();
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let payload = input.map(|bytes| bytes.to_vec());
    let writer = std::thread::spawn(move || {
        if let Some(mut handle) = stdin.take() {
            if let Some(bytes) = payload.as_ref() {
                let _ = handle.write_all(bytes);
            }
            let _ = handle.flush();
        }
    });
    let stdout_reader = std::thread::spawn(move || read_all(stdout));
    let stderr_reader = std::thread::spawn(move || read_all(stderr));

    let code = wait_with_timeout(&mut child, arguments, timeout_seconds)?;
    let _ = writer.join();
    let stdout = stdout_reader.join().unwrap_or_default();
    let stderr = stderr_reader.join().unwrap_or_default();

    if code != 0 {
        let detail = String::from_utf8_lossy(&stderr).trim().to_string();
        let argument = arguments.first().cloned().unwrap_or_default();
        let detail = if detail.is_empty() {
            format!("退出码 {code}")
        } else {
            detail
        };
        return Err(SnapshotError::failed(format!(
            "Git 命令失败（{argument}）：{detail}"
        )));
    }
    Ok(GitOutput { stdout })
}

fn read_all(stream: Option<impl Read>) -> Vec<u8> {
    let mut buffer = Vec::new();
    if let Some(mut stream) = stream {
        let _ = stream.read_to_end(&mut buffer);
    }
    buffer
}

/// 返回子进程退出码；超时会杀掉子进程并报错。
fn wait_with_timeout(
    child: &mut Child,
    arguments: &[String],
    timeout_seconds: u64,
) -> Result<i32, SnapshotError> {
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status.code().unwrap_or(-1)),
            Ok(None) => {
                if started.elapsed() > Duration::from_secs(timeout_seconds) {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(SnapshotError::failed(format!(
                        "Git 命令超时（>{timeout_seconds}s）：{arguments:?}"
                    )));
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(error) => {
                return Err(SnapshotError::failed(format!("无法等待 Git：{error}")));
            }
        }
    }
}

fn resolve_workspace(workspace: &Path) -> PathBuf {
    match crate::project::normalize_project_path(&workspace.to_string_lossy()) {
        Ok(resolved) => PathBuf::from(resolved),
        Err(_) => workspace.to_path_buf(),
    }
}

/// 把 `ls-files` 输出的 POSIX 相对路径安全解析为工作区内的绝对路径。
fn workspace_path(workspace: &Path, relative: &str) -> Result<PathBuf, SnapshotError> {
    if relative.is_empty() || relative.starts_with('/') {
        return Err(SnapshotError::failed(format!(
            "未跟踪文件路径无效：{relative}"
        )));
    }
    let mut path = workspace.to_path_buf();
    for part in relative.split('/') {
        if part.is_empty() || part == "." {
            continue;
        }
        if part == ".." {
            return Err(SnapshotError::failed(format!(
                "未跟踪文件路径无效：{relative}"
            )));
        }
        path.push(part);
    }
    Ok(path)
}

fn sorted_unique(values: &[String]) -> Vec<String> {
    let mut sorted: Vec<String> = values.to_vec();
    sorted.sort();
    sorted.dedup();
    sorted
}

/// `set(left) - set(right)`，按 Python 的 `sorted()` 结果排序。
fn difference(left: &[String], right: &[String]) -> Vec<String> {
    let right_set: std::collections::BTreeSet<&String> = right.iter().collect();
    let mut result: Vec<String> = left
        .iter()
        .filter(|value| !right_set.contains(value))
        .cloned()
        .collect();
    result.sort();
    result.dedup();
    result
}

fn head_cache() -> &'static Mutex<HashMap<PathBuf, (bool, Instant)>> {
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, (bool, Instant)>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}
