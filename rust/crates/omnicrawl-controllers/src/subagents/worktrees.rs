//! `omnicrawl/agent/controllers/subagents/worktrees.py` 的判定面。
//!
//! 登记键、查找键归一化、会话去重投影、丢弃前的变更保护判定与产物摘要渲染收进内核；
//! worktree 的创建/应用/清理以及会话字典本身（含锁）由宿主持有，内核只吃结构体输入。

use serde_json::{json, Value};

use crate::error::AgentError;

/// 单个 worktree 会话在内核侧用到的字段投影。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorktreeSession {
    pub task_id: String,
    pub branch_name: String,
    pub worktree_path: String,
    pub base_ref: String,
    pub repo_root: String,
}

/// git 侧 `collect_worktree_artifacts` 的结果。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorktreeArtifacts {
    pub branch_name: String,
    pub worktree_path: String,
    pub base_ref: String,
    pub has_changes: bool,
    pub changed_files: Vec<String>,
    pub diff_stat: String,
    pub diff_text: String,
}

/// git 侧 `summarize_worktree_changes` 的两个计数。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WorktreeChanges {
    pub uncommitted: usize,
    pub new_commits: usize,
}

/// 变更文件列表的展示条数上限。
pub const CHANGED_FILES_PREVIEW: usize = 20;
/// diff 文本的展示字符上限。
pub const DIFF_PREVIEW_CHARS: usize = 4000;

/// 登记 worktree 会话时写入的两个索引键：分支名与任务 ID。
pub fn registration_keys(session: &WorktreeSession) -> [String; 2] {
    [session.branch_name.clone(), session.task_id.clone()]
}

/// `_lookup_subagent_worktree_session` 的键归一化：`str(key or "").strip()`。
pub fn lookup_key(key: Option<&str>) -> String {
    key.unwrap_or("").trim().to_string()
}

/// `_collect_subagent_worktree_artifacts` 的行渲染；`None` 表示子任务没有 worktree 会话。
pub fn artifact_lines(artifacts: Option<&WorktreeArtifacts>) -> Vec<String> {
    let Some(artifacts) = artifacts else {
        return Vec::new();
    };
    let mut lines = vec![
        format!("branch={}", artifacts.branch_name),
        format!("worktree={}", artifacts.worktree_path),
        format!("base_ref={}", artifacts.base_ref),
        format!("has_changes={}", python_bool_text(artifacts.has_changes)),
    ];
    if !artifacts.changed_files.is_empty() {
        let preview: Vec<&str> = artifacts
            .changed_files
            .iter()
            .take(CHANGED_FILES_PREVIEW)
            .map(String::as_str)
            .collect();
        let mut joined = preview.join(", ");
        if artifacts.changed_files.len() > CHANGED_FILES_PREVIEW {
            joined.push_str(&format!(
                " ...(+{})",
                artifacts.changed_files.len() - CHANGED_FILES_PREVIEW
            ));
        }
        lines.push(format!("changed_files={joined}"));
    }
    if !artifacts.diff_stat.is_empty() {
        lines.push(format!("diff_stat={}", artifacts.diff_stat));
    }
    if !artifacts.diff_text.is_empty() {
        let mut preview: String = artifacts
            .diff_text
            .chars()
            .take(DIFF_PREVIEW_CHARS)
            .collect();
        if artifacts.diff_text.chars().count() > DIFF_PREVIEW_CHARS {
            preview.push_str("\n... diff 已截断 ...");
        }
        lines.push("diff_preview:".to_string());
        lines.push(preview);
    }
    lines
}

/// 产物收集失败时的单行文案。
pub fn artifact_failure_line(message: &str) -> String {
    format!("worktree 产物收集失败：{message}")
}

/// `list_subagent_worktrees` 的投影：按分支名去重，保持调用方给定的顺序。
pub fn list_items(sessions: &[WorktreeSession]) -> Vec<Value> {
    let mut seen: Vec<&str> = Vec::new();
    let mut items: Vec<Value> = Vec::new();
    for session in sessions {
        let branch = session.branch_name.as_str();
        if branch.is_empty() || seen.contains(&branch) {
            continue;
        }
        seen.push(branch);
        items.push(json!({
            "task_id": session.task_id,
            "branch": branch,
            "worktree_path": session.worktree_path,
            "base_ref": session.base_ref,
            "repo_root": session.repo_root,
        }));
    }
    items
}

/// `discard_subagent_worktree` 的变更保护判定（`force=false`）；`None` 表示可以丢弃。
pub fn discard_guard(changes: WorktreeChanges) -> Option<String> {
    if changes.uncommitted == 0 && changes.new_commits == 0 {
        return None;
    }
    let mut details: Vec<String> = Vec::new();
    if changes.uncommitted > 0 {
        details.push(format!("{} 个未提交/未跟踪文件", changes.uncommitted));
    }
    if changes.new_commits > 0 {
        details.push(format!(
            "{} 个新提交（尚未 apply 回主工作区）",
            changes.new_commits
        ));
    }
    Some(format!(
        "worktree 仍有未处理的变更，拒绝丢弃：{}；如需强制丢弃请设置 force=true。",
        details.join("、")
    ))
}

/// `未找到 SubAgent worktree 会话：{key}`。
pub fn missing_session_error(key: &str) -> AgentError {
    AgentError::new(format!("未找到 SubAgent worktree 会话：{key}"))
}

/// `应用 worktree 失败：{message}`。
pub fn apply_failure_error(message: &str) -> AgentError {
    AgentError::new(format!("应用 worktree 失败：{message}"))
}

/// `worktree 变更检查失败（可传 force=true 强制丢弃）：{message}`。
pub fn change_check_failure_error(message: &str) -> AgentError {
    AgentError::new(format!(
        "worktree 变更检查失败（可传 force=true 强制丢弃）：{message}"
    ))
}

/// `清理 worktree 失败：{message}`。
pub fn cleanup_failure_error(message: &str) -> AgentError {
    AgentError::new(format!("清理 worktree 失败：{message}"))
}

/// `已清理 worktree 会话：{branch}`。
pub fn cleanup_message(branch: &str) -> String {
    format!("已清理 worktree 会话：{branch}")
}

fn python_bool_text(value: bool) -> &'static str {
    if value {
        "True"
    } else {
        "False"
    }
}

/// 分支名片段：非安全字符折成一个 `-`，去掉首尾的 `-`/`.`/`/`，再截到 48 字符。
pub fn sanitize_branch_fragment(value: &str) -> String {
    collapse_branch_unsafe(value.trim())
        .trim_matches(|ch| ch == '-' || ch == '.' || ch == '/')
        .chars()
        .take(48)
        .collect()
}

/// 与 `_BRANCH_SAFE_RE = [^a-zA-Z0-9._/-]+` 等价：每段不安全字符整体折成一个 `-`。
fn collapse_branch_unsafe(value: &str) -> String {
    let mut out = String::new();
    let mut in_run = false;
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '/' | '-') {
            out.push(ch);
            in_run = false;
        } else if !in_run {
            out.push('-');
            in_run = true;
        }
    }
    out
}

/// 主工作区脏时的拒绝文案；干净（或只有空白）时返回 `None`。
///
/// 单写者约束：主树有未提交变更时既不静默创建 worktree，也不静默把子代理结果
/// checkout/merge 回主树。预览只取前 8 行，多余部分用 `; ...` 收尾。
pub fn dirty_main_tree_error(status_porcelain: &str) -> Option<String> {
    let dirty = status_porcelain.trim();
    if dirty.is_empty() {
        return None;
    }
    let lines: Vec<&str> = dirty.lines().collect();
    let mut preview = lines.iter().take(8).copied().collect::<Vec<_>>().join("; ");
    if lines.len() > 8 {
        preview.push_str("; ...");
    }
    Some(format!(
        "主工作区存在未提交变更，禁止静默创建或应用 worktree。请先提交、暂存或清理后再操作。脏项预览：{preview}"
    ))
}

/// worktree 变更统计；目录已不存在时返回 `None`（没有可丢失的内容）。
///
/// `query_failed` 对应 `rev-list` 查询失败：按「存在新提交」保守处理，宁可让丢弃多要一次
/// `force`，也不静默丢数据。
pub fn worktree_changes(
    directory_exists: bool,
    status_porcelain: &str,
    query_failed: bool,
    raw_commit_count: &str,
) -> Option<WorktreeChanges> {
    if !directory_exists {
        return None;
    }
    let uncommitted = status_porcelain
        .lines()
        .filter(|line| !line.trim().is_empty())
        .count();
    let new_commits = if query_failed {
        1
    } else {
        raw_commit_count.trim().parse::<i64>().unwrap_or(0).max(0) as usize
    };
    Some(WorktreeChanges {
        uncommitted,
        new_commits,
    })
}
