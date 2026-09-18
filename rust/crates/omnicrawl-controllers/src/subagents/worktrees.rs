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
