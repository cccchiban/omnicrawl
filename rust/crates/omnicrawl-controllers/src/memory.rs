//! `omnicrawl/agent/controllers/memory/stores.py`：三类作用域记忆目录的解析与清理。

use crate::error::AgentError;
use crate::undo::{is_relative_to, resolve_path};
use std::fmt;
use std::path::{Path, PathBuf};

pub const USER_MEMORY_DIRECTORY: &str = "User_memory";

pub const SESSION_MEMORY_DIRECTORY: &str = "Session_memory";

pub const DEFAULT_MEMORY_DIRECTORY: &str = ".omnicrawl/.oclmemory";

/// 项目级记忆根：相对目录一律以工作区为基准，且必须落在工作区内。
pub fn project_memory_root(
    workspace_root: &Path,
    raw_directory: &str,
) -> Result<PathBuf, AgentError> {
    let raw_directory = raw_directory.trim();
    let candidate = PathBuf::from(raw_directory);
    let candidate = if candidate.is_absolute() {
        candidate
    } else {
        workspace_root.join(candidate)
    };
    let project_root = resolve_path(&candidate);
    if !is_relative_to(&project_root, &resolve_path(workspace_root)) {
        return Err(AgentError::new(format!(
            "项目级记忆目录必须位于工作区内：{raw_directory}"
        )));
    }
    Ok(project_root)
}

/// 用户级与会话级记忆共享的用户数据根目录。
pub fn user_data_root(home: &Path) -> PathBuf {
    resolve_path(&home.join(".omnicrawl"))
}

pub fn user_memory_root(user_data_root: &Path) -> PathBuf {
    user_data_root.join(USER_MEMORY_DIRECTORY)
}

/// 会话 ID 能否用作记忆目录名。
pub fn is_valid_session_id(session_id: &str) -> bool {
    let value = session_id.trim();
    let mut chars = value.chars();
    match chars.next() {
        Some(first) if first.is_ascii_alphanumeric() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// 当前会话级记忆存储根：会话未启用时为 None。
pub fn session_memory_root(user_data_root: &Path, session_id: &str) -> Result<PathBuf, AgentError> {
    if !is_valid_session_id(session_id) {
        return Err(AgentError::new(format!(
            "会话 ID 不能用于记忆目录：{session_id}"
        )));
    }
    Ok(user_data_root
        .join(SESSION_MEMORY_DIRECTORY)
        .join(session_id.trim()))
}

/// 删除已删除会话的专属记忆目录；未启用、ID 非法或目录不存在时不动作。
pub fn delete_session_memory(
    user_data_root: &Path,
    memory_enabled: bool,
    session_id: &str,
) -> Result<bool, AgentError> {
    if !memory_enabled || !is_valid_session_id(session_id) {
        return Ok(false);
    }
    let path = user_data_root
        .join(SESSION_MEMORY_DIRECTORY)
        .join(session_id.trim());
    if !path.is_dir() {
        return Ok(false);
    }
    std::fs::remove_dir_all(&path).map_err(|error| {
        AgentError::new(format!("删除会话级记忆失败：{}，{error}", path.display()))
    })?;
    Ok(true)
}

/// 清理结果条目的展示形态：`<scope>:<path>`。
pub fn cleaned_entry(scope: &str, path: &Path) -> String {
    format!("{scope}:{}", path.display())
}

pub fn memory_disabled_error() -> AgentError {
    AgentError::new("记忆系统未启用。")
}

/// 记忆存储层错误的文案面（对应 `MemoryStoreError`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryStoreError {
    message: String,
}

impl MemoryStoreError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for MemoryStoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for MemoryStoreError {}

/// 过期记忆清理用到的存储能力（`MemoryStore` 的这一面）。
pub trait ExpiredMemoryStore {
    fn clean_expired_memories(&self) -> Result<Vec<PathBuf>, MemoryStoreError>;
}

/// 清理三类作用域中的过期记忆；三类都没绑定时按「记忆系统未启用」拒绝。
pub fn clean_memory(
    stores: &[(&str, Option<&dyn ExpiredMemoryStore>)],
) -> Result<Vec<String>, AgentError> {
    if stores.iter().all(|(_, store)| store.is_none()) {
        return Err(memory_disabled_error());
    }
    let mut deleted: Vec<String> = Vec::new();
    for (scope, store) in stores {
        let Some(store) = store else {
            continue;
        };
        let expired = store
            .clean_expired_memories()
            .map_err(|error| AgentError::new(error.to_string()))?;
        deleted.extend(expired.iter().map(|path| cleaned_entry(scope, path)));
    }
    Ok(deleted)
}
