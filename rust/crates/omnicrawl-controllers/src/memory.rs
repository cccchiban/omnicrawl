//! `omnicrawl/agent/controllers/memory/stores.py`：三类作用域记忆目录的解析与清理。

use crate::error::AgentError;
use crate::undo::{is_relative_to, resolve_path};
use std::fmt;
use std::path::{Path, PathBuf};

/// 用户级与会话级记忆的汇总根（`~/.omnicrawl/memory`）。
///
/// 它只是归口目录：三类作用域仍各在自己的子目录里，互不覆盖。用户数据根本身还装着配置、
/// 会话、知识库等无关内容，直接打开它并不是「记忆文件夹」。
pub const MEMORY_ROOT_DIRECTORY: &str = "memory";

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

/// 记忆汇总根：`~/.omnicrawl/memory`。用户级与会话级都归口在这里。
pub fn memory_root(user_data_root: &Path) -> PathBuf {
    user_data_root.join(MEMORY_ROOT_DIRECTORY)
}

pub fn user_memory_root(user_data_root: &Path) -> PathBuf {
    memory_root(user_data_root).join(USER_MEMORY_DIRECTORY)
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
    Ok(memory_root(user_data_root)
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
    let path = memory_root(user_data_root)
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

/// 把记忆归口到 `memory/` 之前的两个旧目录迁进来；返回实际迁移的条目（目录名）。
///
/// 旧布局把 `User_memory/`、`Session_memory/` 直接放在用户数据根下，与配置、会话、知识库
/// 混在一起，`[memory]` 打开的就是这一整片。迁移动词是**改名**：记忆正文、索引、时间戳与
/// 触碰次数一律原样保留，不做导入重写。
///
/// 目标是 `memory/<同名目录>` 已存在时不覆盖：旧目录改名成 `memory/<同名>.migrated`，
/// 让两批数据都留在用户手里。任何一步失败只报错、不动源目录，避免升级过程丢数据。
pub fn migrate_legacy_memory_directories(user_data_root: &Path) -> Result<Vec<String>, AgentError> {
    let target_root = memory_root(user_data_root);
    let mut migrated: Vec<String> = Vec::new();
    for name in [USER_MEMORY_DIRECTORY, SESSION_MEMORY_DIRECTORY] {
        let source = user_data_root.join(name);
        if !source.is_dir() {
            continue;
        }
        let target = target_root.join(name);
        let destination = if target.exists() {
            backup_memory_path(&target_root, name)
        } else {
            target
        };
        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent).map_err(|error| {
                AgentError::new(format!(
                    "创建记忆目录失败：{}，{error}",
                    parent.display()
                ))
            })?;
        }
        std::fs::rename(&source, &destination).map_err(|error| {
            AgentError::new(format!(
                "迁移旧记忆目录失败：{} -> {}，{error}",
                source.display(),
                destination.display()
            ))
        })?;
        migrated.push(name.to_string());
    }
    Ok(migrated)
}

/// 迁移冲突时的落点：`memory/<name>.migrated`，占用时追加序号。
fn backup_memory_path(target_root: &Path, name: &str) -> PathBuf {
    let mut candidate = target_root.join(format!("{name}.migrated"));
    let mut index = 1;
    while candidate.exists() {
        candidate = target_root.join(format!("{name}.migrated.{index}"));
        index += 1;
    }
    candidate
}

/// 准备记忆汇总根：建出 `memory/`，并把旧的顶层记忆目录迁进来。
///
/// 启动与「打开记忆文件夹」都走这里，因此用户第一次点开目录时看到的已经是归口后的布局。
/// 迁移是幂等的：迁完之后用户数据根下不再有那两个目录，后续调用什么都不做。
pub fn ensure_memory_root(user_data_root: &Path) -> Result<Vec<String>, AgentError> {
    let root = memory_root(user_data_root);
    std::fs::create_dir_all(&root).map_err(|error| {
        AgentError::new(format!("创建记忆文件夹失败：{}，{error}", root.display()))
    })?;
    migrate_legacy_memory_directories(user_data_root)
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

/// 清空记忆用到的存储能力（`MemoryStore` 的这一面）。
pub trait ClearableMemoryStore {
    fn clear_all(&self) -> Result<Vec<String>, MemoryStoreError>;
}

/// 清空三类作用域中的全部记忆；三类都没绑定时按「记忆系统未启用」拒绝。
///
/// 返回被清掉的 id 列表（`<scope>:<id>`）。这个动作不可逆，调用方必须先拿到用户确认。
pub fn clear_memory(
    stores: &[(&str, Option<&dyn ClearableMemoryStore>)],
) -> Result<Vec<String>, AgentError> {
    if stores.iter().all(|(_, store)| store.is_none()) {
        return Err(memory_disabled_error());
    }
    let mut removed: Vec<String> = Vec::new();
    for (scope, store) in stores {
        let Some(store) = store else {
            continue;
        };
        let ids = store
            .clear_all()
            .map_err(|error| AgentError::new(error.to_string()))?;
        removed.extend(ids.iter().map(|id| format!("{scope}:{id}")));
    }
    Ok(removed)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("omnicrawl-memory-root-{name}"));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("建临时目录");
        root
    }

    #[test]
    fn the_two_shared_scopes_live_under_the_memory_root() {
        let user_root = PathBuf::from("C:/data");
        assert_eq!(memory_root(&user_root), PathBuf::from("C:/data/memory"));
        assert_eq!(
            user_memory_root(&user_root),
            PathBuf::from("C:/data/memory/User_memory")
        );
        assert_eq!(
            session_memory_root(&user_root, "abc").expect("合法 ID"),
            PathBuf::from("C:/data/memory/Session_memory/abc")
        );
    }

    #[test]
    fn legacy_directories_move_into_the_memory_root() {
        let root = temp_root("migrate");
        let user = root.join(USER_MEMORY_DIRECTORY);
        std::fs::create_dir_all(user.join("general")).expect("旧用户级目录");
        std::fs::write(user.join("general/20260101-000000.md"), "旧用户记忆").expect("写正文");
        let session = root.join(SESSION_MEMORY_DIRECTORY).join("s1");
        std::fs::create_dir_all(&session).expect("旧会话级目录");

        let migrated = ensure_memory_root(&root).expect("迁移应当成功");
        assert_eq!(
            migrated,
            vec![
                USER_MEMORY_DIRECTORY.to_string(),
                SESSION_MEMORY_DIRECTORY.to_string()
            ]
        );
        // 迁移是改名：正文原样保留在新位置，旧的顶层目录不再存在。
        assert!(memory_root(&root)
            .join(USER_MEMORY_DIRECTORY)
            .join("general/20260101-000000.md")
            .is_file());
        assert!(memory_root(&root)
            .join(SESSION_MEMORY_DIRECTORY)
            .join("s1")
            .is_dir());
        assert!(!user.exists());
        assert!(!root.join(SESSION_MEMORY_DIRECTORY).exists());

        // 幂等：再调一次什么都不做。
        assert!(ensure_memory_root(&root).expect("二次调用应当成功").is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_existing_target_is_never_overwritten() {
        let root = temp_root("conflict");
        let target = memory_root(&root).join(USER_MEMORY_DIRECTORY);
        std::fs::create_dir_all(&target).expect("新布局目录");
        std::fs::write(target.join("new.md"), "新").expect("写新正文");
        let legacy = root.join(USER_MEMORY_DIRECTORY);
        std::fs::create_dir_all(&legacy).expect("旧目录");
        std::fs::write(legacy.join("old.md"), "旧").expect("写旧正文");

        ensure_memory_root(&root).expect("迁移应当成功");
        // 新布局原封不动，旧目录改名到 `.migrated` 旁边。
        assert_eq!(
            std::fs::read_to_string(target.join("new.md")).expect("读新正文"),
            "新"
        );
        assert!(std::fs::read_to_string(target.join("old.md")).is_err());
        assert_eq!(
            std::fs::read_to_string(memory_root(&root).join("User_memory.migrated/old.md"))
                .expect("读旧正文"),
            "旧"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
