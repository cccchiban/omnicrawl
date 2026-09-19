//! SubAgent 的 git worktree 隔离生命周期（对应 `omnicrawl/agent/subagents/worktree.py` 的 git 面）。
//!
//! 判定面（登记键、脏树门禁、变更保护、产物渲染）早在 `controllers::subagents::worktrees`；
//! 这里补上要起子进程的那一半：创建、收集、统计、应用与清理，并把创建时的会话信息按
//! `agent_isolation` 的元数据格式落盘（`sw-<key>.json`），供人工/清扫识别残留。
//!
//! 与 Python 的差异（有意为之）：**不做复用**——目录已存在时直接报错，让父 Agent 看到残留，
//! 而不是靠文件系统校验去猜它是否可用；共享注册表（多进程 in-use 保护）也没搬，
//! 因此崩溃残留不会被启动清扫自动回收。

use std::path::{Path, PathBuf};
use std::process::Command;

use omnicrawl_controllers::subagents::worktrees::{
    dirty_main_tree_error, sanitize_branch_fragment, worktree_changes, WorktreeArtifacts,
    WorktreeChanges,
};
use serde_json::{json, Value};

/// 托管根目录名（与 `workspace/agent_isolation.py` 的 `DEFAULT_WORKTREES_ROOT` 同址）。
pub const WORKTREES_DIR: &str = "agent-worktrees";
/// diff 文本上限。
pub const MAX_DIFF_CHARS: usize = 200_000;
/// 变更文件列表上限。
pub const MAX_CHANGED_FILES: usize = 200;

/// 一次 SubAgent 任务绑定的 worktree 会话。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeSession {
    pub task_id: String,
    pub instance_id: String,
    pub repo_root: PathBuf,
    pub worktree_path: PathBuf,
    pub branch_name: String,
    pub base_ref: String,
}

/// worktree 创建、查询或清理失败时的稳定错误。
#[derive(Debug, Clone)]
pub struct WorktreeError {
    message: String,
}

impl WorktreeError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

impl std::fmt::Display for WorktreeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for WorktreeError {}

/// 托管根：`~/.omnicrawl/agent-worktrees`。
pub fn default_worktrees_root() -> PathBuf {
    home_directory().join(".omnicrawl").join(WORKTREES_DIR)
}

fn home_directory() -> PathBuf {
    if let Some(profile) = std::env::var_os("USERPROFILE") {
        return PathBuf::from(profile);
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home);
    }
    PathBuf::from(".")
}

fn run_git(args: &[&str], cwd: &Path, check: bool) -> Result<(i32, String), WorktreeError> {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .map_err(|error| WorktreeError::new(format!("未找到 git 可执行文件：{error}")))?;
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let code = output.status.code().unwrap_or(-1);
    if check && code != 0 {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(WorktreeError::new(format!(
            "git {} 失败：{}",
            args.join(" "),
            if stderr.is_empty() {
                stdout.trim()
            } else {
                &stderr
            }
        )));
    }
    Ok((code, stdout))
}

pub fn is_git_repository(path: &Path) -> bool {
    matches!(
        run_git(&["rev-parse", "--is-inside-work-tree"], path, false),
        Ok((0, text)) if text.trim() == "true"
    )
}

pub fn resolve_repo_root(path: &Path) -> Result<PathBuf, WorktreeError> {
    let (_, text) = run_git(&["rev-parse", "--show-toplevel"], path, true)?;
    Ok(PathBuf::from(text.trim()))
}

/// 为任务创建独立 worktree 与本地分支；目录已存在时拒绝（不做复用猜测）。
pub fn create_session(
    workspace_root: &Path,
    task_id: &str,
    base_ref: &str,
    worktree_parent: Option<&Path>,
) -> Result<WorktreeSession, WorktreeError> {
    let root = workspace_root.to_path_buf();
    if !is_git_repository(&root) {
        return Err(WorktreeError::new(
            "当前工作区不是 git 仓库，无法启用 isolation=worktree。",
        ));
    }
    let repo_root = resolve_repo_root(&root)?;
    // 脏主树禁止创建：避免后续 apply 时与用户未提交改动互相覆盖。
    let (_, status) = run_git(&["status", "--porcelain"], &repo_root, true)?;
    if let Some(message) = dirty_main_tree_error(&status) {
        return Err(WorktreeError::new(message));
    }

    let fragment = sanitize_branch_fragment(task_id);
    let instance_id = if fragment.is_empty() {
        format!("{:012x}", std::process::id() as u64)
    } else {
        fragment
    };
    let branch_name = format!("omnicrawl/subagent/{instance_id}");
    let parent_dir = worktree_parent
        .map(Path::to_path_buf)
        .unwrap_or_else(default_worktrees_root);
    std::fs::create_dir_all(&parent_dir)
        .map_err(|error| WorktreeError::new(format!("创建 worktree 托管根失败：{error}")))?;
    let worktree_path = parent_dir.join(format!("sw-{instance_id}"));
    if worktree_path.exists() {
        return Err(WorktreeError::new(format!(
            "worktree 目录已存在，请先清理残留：{}",
            worktree_path.display()
        )));
    }

    // 先解析 base_ref，避免 git worktree 在错误 ref 上创建半成品目录。
    let (_, resolved) = run_git(&["rev-parse", "--verify", base_ref], &repo_root, true)?;
    let resolved_base = resolved.trim().to_string();
    let path_text = worktree_path.to_string_lossy().to_string();
    if let Err(error) = run_git(
        &[
            "worktree",
            "add",
            "-b",
            &branch_name,
            &path_text,
            &resolved_base,
        ],
        &repo_root,
        true,
    ) {
        // 创建失败时尽量回收可能残留的目录，避免下次撞名。
        if worktree_path.exists() {
            let _ = std::fs::remove_dir_all(&worktree_path);
        }
        return Err(error);
    }

    let session = WorktreeSession {
        task_id: task_id.to_string(),
        instance_id,
        repo_root,
        worktree_path,
        branch_name,
        base_ref: resolved_base,
    };
    write_metadata(&session, &parent_dir)?;
    Ok(session)
}

/// 按 `agent_isolation` 的格式落盘会话元数据（`sw-<key>.json`）。
fn write_metadata(session: &WorktreeSession, worktrees_root: &Path) -> Result<(), WorktreeError> {
    let payload = json!({
        "instance_id": session.instance_id,
        "task_id": session.task_id,
        "mode": "subagent",
        "repo_root": session.repo_root.to_string_lossy(),
        "main_workspace": session.repo_root.to_string_lossy(),
        "worktree_path": session.worktree_path.to_string_lossy(),
        "base_ref": session.base_ref,
        "branch_name": session.branch_name,
        "created_at": now_seconds(),
        // SubAgent 成果必须由父 Agent 审查后才 apply，清扫/退出不自动写回。
        "apply_on_exit": false,
        "cleanup_on_exit": "auto",
    });
    let path = metadata_path(worktrees_root, &session.worktree_path);
    let text = serde_json::to_string_pretty(&payload)
        .map_err(|error| WorktreeError::new(format!("序列化隔离元数据失败：{error}")))?;
    std::fs::write(&path, text)
        .map_err(|error| WorktreeError::new(format!("写入隔离元数据失败：{error}")))
}

fn metadata_path(worktrees_root: &Path, worktree_path: &Path) -> PathBuf {
    let entry = worktree_path
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_default();
    worktrees_root.join(format!("{entry}.json"))
}

fn now_seconds() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs_f64())
        .unwrap_or(0.0)
}

/// 收集 worktree 相对基线的变更摘要（含未跟踪文件；有变更则先提交到隔离分支）。
pub fn collect_artifacts(session: &WorktreeSession) -> Result<WorktreeArtifacts, WorktreeError> {
    if !session.worktree_path.exists() {
        return Err(WorktreeError::new(format!(
            "worktree 目录不存在：{}",
            session.worktree_path.display()
        )));
    }
    let cwd = session.worktree_path.clone();
    let _ = run_git(&["add", "-A"], &cwd, false);
    let (_, status) = run_git(&["status", "--porcelain"], &cwd, true)?;
    if !status.trim().is_empty() {
        let message = format!("omnicrawl-subagent:{}", session.task_id);
        let _ = run_git(
            &["commit", "-m", &message, "--allow-empty-message"],
            &cwd,
            false,
        );
    }
    let (_, names) = run_git(
        &["diff", "--name-only", &session.base_ref, "HEAD"],
        &cwd,
        false,
    )?;
    let changed_files: Vec<String> = names
        .lines()
        .map(|line| line.trim().to_string())
        .filter(|line| !line.is_empty())
        .take(MAX_CHANGED_FILES)
        .collect();
    let (_, stat) = run_git(&["diff", "--stat", &session.base_ref, "HEAD"], &cwd, false)?;
    let (_, diff) = run_git(&["diff", &session.base_ref, "HEAD"], &cwd, false)?;
    let diff_text = if diff.chars().count() > MAX_DIFF_CHARS {
        let mut truncated: String = diff.chars().take(MAX_DIFF_CHARS).collect();
        truncated.push_str("\n... diff 已截断，完整内容请在 worktree 分支上查看。");
        truncated
    } else {
        diff
    };
    let has_changes = !changed_files.is_empty() || !diff_text.trim().is_empty();
    Ok(WorktreeArtifacts {
        branch_name: session.branch_name.clone(),
        worktree_path: session.worktree_path.to_string_lossy().to_string(),
        base_ref: session.base_ref.clone(),
        changed_files,
        diff_stat: stat.trim().to_string(),
        diff_text,
        has_changes,
    })
}

/// 统计 worktree 相对基线的变更（丢弃前的变更保护用）。
pub fn summarize_changes(
    session: &WorktreeSession,
) -> Result<Option<WorktreeChanges>, WorktreeError> {
    if !session.worktree_path.exists() {
        return Ok(None);
    }
    let cwd = session.worktree_path.clone();
    let (_, status) = run_git(&["status", "--porcelain"], &cwd, true)?;
    let base = if session.base_ref.trim().is_empty() {
        "HEAD".to_string()
    } else {
        session.base_ref.trim().to_string()
    };
    let range = format!("{base}..HEAD");
    let (code, count) = run_git(&["rev-list", "--count", &range], &cwd, false)?;
    let changes = worktree_changes(session.worktree_path.exists(), &status, code != 0, &count);
    Ok(changes)
}

/// 把隔离分支的变更应用到主工作区；主工作区必须干净。
pub fn apply_to_main(session: &WorktreeSession, strategy: &str) -> Result<String, WorktreeError> {
    if !matches!(strategy, "checkout" | "merge") {
        return Err(WorktreeError::new(format!(
            "不支持的 apply strategy：{strategy}"
        )));
    }
    let (_, status) = run_git(&["status", "--porcelain"], &session.repo_root, true)?;
    if let Some(message) = dirty_main_tree_error(&status) {
        return Err(WorktreeError::new(message));
    }
    let artifacts = collect_artifacts(session)?;
    if !artifacts.has_changes {
        return Ok("worktree 无变更，无需应用。".to_string());
    }
    if strategy == "checkout" {
        // 先确保分支上有最新提交（collect 会 commit；若调用方未 collect 再补一次）。
        collect_artifacts(session)?;
        run_git(
            &["checkout", &session.branch_name, "--", "."],
            &session.repo_root,
            true,
        )?;
        return Ok(format!(
            "已将分支 {} 的文件变更检出到主工作区。 变更文件数：{}。",
            session.branch_name,
            artifacts.changed_files.len()
        ));
    }
    run_git(
        &["merge", "--no-ff", "--no-edit", &session.branch_name],
        &session.repo_root,
        true,
    )?;
    Ok(format!(
        "已将分支 {} merge 到主工作区当前分支。",
        session.branch_name
    ))
}

/// 清理 worktree 目录与本地分支，并删除托管元数据。
pub fn cleanup_session(
    session: &WorktreeSession,
    remove_branch: bool,
) -> Result<(), WorktreeError> {
    // 先尝试 git worktree remove；失败时回退到目录删除。
    if session.repo_root.exists() {
        let path_text = session.worktree_path.to_string_lossy().to_string();
        let (code, _) = run_git(
            &["worktree", "remove", "--force", &path_text],
            &session.repo_root,
            false,
        )?;
        if code != 0 && session.worktree_path.exists() {
            let _ = std::fs::remove_dir_all(&session.worktree_path);
            let _ = run_git(&["worktree", "prune"], &session.repo_root, false);
        }
        if remove_branch {
            let _ = run_git(
                &["branch", "-D", &session.branch_name],
                &session.repo_root,
                false,
            );
        }
    } else if session.worktree_path.exists() {
        let _ = std::fs::remove_dir_all(&session.worktree_path);
    }
    let metadata = metadata_path(
        session
            .worktree_path
            .parent()
            .unwrap_or_else(|| Path::new(".")),
        &session.worktree_path,
    );
    let _ = std::fs::remove_file(metadata);
    Ok(())
}

/// 列出托管根下现存的 SubAgent worktree 会话（按元数据文件）。
pub fn list_sessions(worktrees_root: Option<&Path>) -> Vec<WorktreeSession> {
    let root = worktrees_root
        .map(Path::to_path_buf)
        .unwrap_or_else(default_worktrees_root);
    let Ok(entries) = std::fs::read_dir(&root) else {
        return Vec::new();
    };
    let mut sessions = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.starts_with("sw-") || !name.ends_with(".json") {
            continue;
        }
        if let Some(session) = read_metadata(&entry.path()) {
            sessions.push(session);
        }
    }
    sessions.sort_by(|left, right| left.task_id.cmp(&right.task_id));
    sessions
}

/// 按分支名或任务号找一个会话。
pub fn find_session(key: &str, worktrees_root: Option<&Path>) -> Option<WorktreeSession> {
    let key = key.trim();
    if key.is_empty() {
        return None;
    }
    list_sessions(worktrees_root)
        .into_iter()
        .find(|session| session.branch_name == key || session.task_id == key)
}

fn read_metadata(path: &Path) -> Option<WorktreeSession> {
    let raw = std::fs::read_to_string(path).ok()?;
    let payload: Value = serde_json::from_str(&raw).ok()?;
    if payload.get("mode").and_then(Value::as_str) != Some("subagent") {
        return None;
    }
    let worktree_path = PathBuf::from(payload.get("worktree_path")?.as_str()?);
    let branch_name = payload.get("branch_name")?.as_str()?.to_string();
    if branch_name.is_empty() {
        return None;
    }
    let task_id = payload
        .get("task_id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    Some(WorktreeSession {
        task_id,
        instance_id: payload
            .get("instance_id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        repo_root: PathBuf::from(payload.get("repo_root")?.as_str()?),
        worktree_path: worktree_path.clone(),
        branch_name,
        base_ref: payload
            .get("base_ref")
            .and_then(Value::as_str)
            .unwrap_or("HEAD")
            .to_string(),
    })
}

/// 会话的公开投影（list_worktrees 的返回项）。
pub fn session_value(session: &WorktreeSession) -> Value {
    json!({
        "task_id": session.task_id,
        "branch": session.branch_name,
        "worktree_path": session.worktree_path.to_string_lossy(),
        "base_ref": session.base_ref,
        "repo_root": session.repo_root.to_string_lossy(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(cwd: &Path, args: &[&str]) {
        let output = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .expect("git 可执行");
        assert!(
            output.status.success(),
            "git {args:?} 失败：{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn setup_repo(tag: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("omnicrawl-worktree-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("建仓库目录");
        git(&root, &["init", "-q"]);
        git(&root, &["config", "user.email", "test@example.com"]);
        git(&root, &["config", "user.name", "test"]);
        std::fs::write(root.join("a.txt"), "one\n").expect("写文件");
        git(&root, &["add", "-A"]);
        git(&root, &["commit", "-q", "-m", "init"]);
        root
    }

    /// 托管根放在仓库**外**：仓库内的目录会让 `git status` 报未跟踪项，跟真实场景不符。
    fn external_host(tag: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "omnicrawl-worktree-host-{tag}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("建托管根");
        path
    }

    #[test]
    fn creates_collects_and_cleans_worktree() {
        let repo = setup_repo("lifecycle");
        let parent = external_host("lifecycle");
        let session = create_session(&repo, "task-000000000001", "HEAD", Some(&parent))
            .expect("创建 worktree");
        assert!(session.worktree_path.exists(), "worktree 目录应存在");
        assert!(session.branch_name.starts_with("omnicrawl/subagent/"));
        assert_eq!(session.base_ref.len(), 40, "base_ref 应是完整提交号");

        // 元数据按 agent_isolation 的格式落盘。
        let metadata = metadata_path(&parent, &session.worktree_path);
        let payload: Value =
            serde_json::from_str(&std::fs::read_to_string(&metadata).expect("读元数据"))
                .expect("元数据是 JSON");
        assert_eq!(payload["mode"], "subagent");
        assert_eq!(payload["apply_on_exit"], false);
        assert_eq!(payload["branch_name"], session.branch_name.as_str());

        // 隔离区里写文件后收集：能看到变更文件与 diff。
        std::fs::write(session.worktree_path.join("b.txt"), "two\n").expect("写隔离区文件");
        let artifacts = collect_artifacts(&session).expect("收集产物");
        assert!(artifacts.has_changes, "应有变更");
        assert_eq!(artifacts.changed_files, vec!["b.txt".to_string()]);
        assert_eq!(artifacts.branch_name, session.branch_name);

        let changes = summarize_changes(&session).expect("统计变更");
        assert_eq!(changes.expect("有变更").new_commits, 1);

        // 主工作区干净时按 checkout 应用，拿到 b.txt。
        let message = apply_to_main(&session, "checkout").expect("应用变更");
        assert!(message.contains("变更文件数：1"), "{message}");
        assert!(repo.join("b.txt").exists(), "主工作区应拿到文件");

        cleanup_session(&session, true).expect("清理");
        assert!(!session.worktree_path.exists(), "目录应被清理");
        assert!(!metadata.exists(), "元数据应被删除");
        let _ = std::fs::remove_dir_all(&repo);
        let _ = std::fs::remove_dir_all(&parent);
    }

    #[test]
    fn refuses_dirty_main_tree_and_existing_directory() {
        let repo = setup_repo("refuse");
        let parent = external_host("refuse");
        std::fs::write(repo.join("dirty.txt"), "x\n").expect("写脏文件");
        let error = create_session(&repo, "task-000000000002", "HEAD", Some(&parent))
            .expect_err("脏主树应拒绝");
        assert!(
            error.message().contains("未提交变更"),
            "{}",
            error.message()
        );
        git(&repo, &["checkout", "--", "."]);
        let _ = std::fs::remove_file(repo.join("dirty.txt"));

        let session =
            create_session(&repo, "task-000000000003", "HEAD", Some(&parent)).expect("创建");
        let again = create_session(&repo, "task-000000000003", "HEAD", Some(&parent))
            .expect_err("目录已存在应拒绝");
        assert!(again.message().contains("已存在"), "{}", again.message());
        cleanup_session(&session, true).expect("清理");
        let _ = std::fs::remove_dir_all(&repo);
        let _ = std::fs::remove_dir_all(&parent);
    }

    #[test]
    fn rejects_non_git_workspace() {
        let root =
            std::env::temp_dir().join(format!("omnicrawl-worktree-plain-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("建目录");
        let error = create_session(&root, "task-000000000004", "HEAD", None)
            .expect_err("非 git 仓库应拒绝");
        assert!(
            error.message().contains("不是 git 仓库"),
            "{}",
            error.message()
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
