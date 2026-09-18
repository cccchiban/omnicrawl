//! `omnicrawl/agent/controllers/workspace/`：运行中工作区切换的校验与文案、工具目录保护。

use crate::error::AgentError;
use crate::undo::{is_relative_to, resolve_path};
use std::path::{Path, PathBuf};

/// `list_subagent_worktrees` 的条目里切换判定读到的两个字段。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorktreeRef {
    pub branch: String,
    pub task_id: String,
}

impl WorktreeRef {
    fn label(&self) -> String {
        if !self.branch.is_empty() {
            self.branch.clone()
        } else if !self.task_id.is_empty() {
            self.task_id.clone()
        } else {
            "unknown".to_string()
        }
    }
}

/// 切换前仍有 SubAgent 子任务未退出时的拒绝文案。
pub fn subagent_drain_error() -> AgentError {
    AgentError::new(
        "工作区切换失败：仍有 SubAgent 子任务未在期限内退出，已保留原工作区和共享资源。",
    )
}

/// 切换前仍有未处理 worktree 时的拒绝文案（最多点名三个分支）。
pub fn pending_worktrees_error(items: &[WorktreeRef]) -> AgentError {
    let mut branches = items
        .iter()
        .take(3)
        .map(WorktreeRef::label)
        .collect::<Vec<_>>()
        .join(", ");
    if items.len() > 3 {
        branches.push_str(&format!(" ...(+{})", items.len() - 3));
    }
    AgentError::new(format!(
        "工作区切换失败：仍有未处理的 SubAgent worktree。\
         请先 apply_worktree 或 discard_worktree：{branches}"
    ))
}

/// 解析并校验切换目标：必须存在且是目录。
pub fn resolve_switch_target(new_path: &str) -> Result<PathBuf, AgentError> {
    let expanded = expand_user(new_path);
    let candidate = resolve_path(Path::new(&expanded));
    let metadata = std::fs::metadata(&candidate).map_err(|error| {
        AgentError::new(format!("工作区切换失败：{new_path} 无法解析，{error}"))
    })?;
    if !metadata.is_dir() {
        return Err(AgentError::new(format!(
            "工作区切换失败：{} 不是目录。",
            candidate.display()
        )));
    }
    Ok(candidate)
}

/// `Path.expanduser()` 的可用子集：`~` 与 `~/...` 展开为用户主目录。
fn expand_user(path: &str) -> String {
    if path == "~" {
        return home_directory().to_string_lossy().to_string();
    }
    match path.strip_prefix("~/").or_else(|| path.strip_prefix("~\\")) {
        Some(rest) => home_directory().join(rest).to_string_lossy().to_string(),
        None => path.to_string(),
    }
}

fn home_directory() -> PathBuf {
    for name in ["HOME", "USERPROFILE"] {
        if let Ok(value) = std::env::var(name) {
            if !value.trim().is_empty() {
                return PathBuf::from(value);
            }
        }
    }
    PathBuf::from(".")
}

/// 为内置文件工具补充内部目录保护；MCP Server 不共享这条业务限制。
pub fn workspace_extra_protection_message(
    is_memory_path: bool,
    is_session_path: bool,
    relative_path: &str,
) -> Option<String> {
    if is_memory_path {
        return Some(format!("请使用 memory_* 工具访问记忆目录：{relative_path}"));
    }
    if is_session_path {
        return Some(format!("请使用会话命令访问会话目录：{relative_path}"));
    }
    None
}

/// 记忆目录保护判定：路径解析后等于或位于记忆根之内。
pub fn is_memory_path(path: &Path, memory_root: &Path) -> bool {
    let resolved = resolve_path(path);
    let root = resolve_path(memory_root);
    resolved == root || is_relative_to(&resolved, &root)
}

/// 会话目录保护判定：Agent 未启用会话时（root 为 None）恒为假。
pub fn is_session_path(path: &Path, session_root: Option<&Path>) -> bool {
    let root = match session_root {
        Some(root) => root,
        None => return false,
    };
    let resolved = resolve_path(path);
    let root = resolve_path(root);
    resolved == root || is_relative_to(&resolved, &root)
}

/// 截图目录：仅当临时目录启用时给出 `<workspace>/<directory>/images`。
pub fn screenshot_directory(
    workspace_root: &Path,
    temp_workspace_enabled: bool,
    directory: &str,
) -> Option<PathBuf> {
    if !temp_workspace_enabled {
        return None;
    }
    Some(workspace_root.join(directory).join("images"))
}
