//! 长期记忆的存储：索引读写与读取加深（对齐 Python `omnicrawl/state/memory.py`）。
//!
//! 本片落地「索引 + 读取」这条链路：`index.json` 的解析与落盘（含排序与原子替换、
//! Windows 短重试）、`read` 的读取加深（刷新时间戳与 touch_count、重写 Markdown）、
//! 以及记忆路径的越界防护。写入、检索入口与过期清理在后续切片接上。
//!
//! 未搬：`write` / `search` / `expand_related` / `clean_expired_memories` 与旧目录迁移。

use std::path::{Path, PathBuf};

use chrono::{DateTime, FixedOffset, Local};
use serde_json::{json, Value};

use crate::error::SessionStoreError;
use crate::locking::{atomic_write_text, DurableWritePolicy};
use crate::memory::{
    format_memory_datetime, format_memory_markdown, read_markdown_body, MemoryIndexEntry,
};

/// 候选记忆摘要，供模型先低成本判断是否需要读取全文。
#[derive(Debug, Clone, PartialEq)]
pub struct MemorySearchResult {
    pub id: String,
    pub summary: String,
    pub storage_directory: String,
    pub related_directories: Vec<String>,
    pub timestamp: DateTime<FixedOffset>,
}

/// 完整记忆内容，只有模型明确需要细节时才返回。
#[derive(Debug, Clone, PartialEq)]
pub struct MemoryRecord {
    pub id: String,
    pub timestamp: DateTime<FixedOffset>,
    pub related_directories: Vec<String>,
    pub content: String,
}

pub struct MemoryStore {
    root: PathBuf,
    index_path: PathBuf,
    policy: DurableWritePolicy,
}

impl MemoryStore {
    pub fn open(root: impl Into<PathBuf>) -> Self {
        Self::open_with_policy(root, DurableWritePolicy::default())
    }

    pub fn open_with_policy(root: impl Into<PathBuf>, policy: DurableWritePolicy) -> Self {
        let root = root.into();
        Self {
            index_path: root.join("index.json"),
            root,
            policy,
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 读取指定记忆全文，并对实际读到的记忆执行加深回忆。
    pub fn read(&self, memory_ids: &[Value]) -> Result<Vec<MemoryRecord>, SessionStoreError> {
        let ids = crate::memory::dedupe_strings(memory_ids);
        if ids.is_empty() {
            return Ok(Vec::new());
        }

        let touched = self.touch_entries(&ids)?;
        let mut records = Vec::new();
        for memory_id in &ids {
            let Some(entry) = touched
                .iter()
                .find(|(key, _)| key == memory_id)
                .map(|(_, entry)| entry)
            else {
                continue;
            };
            let path = self.memory_path(entry)?;
            if !path.is_file() {
                continue;
            }
            records.push(MemoryRecord {
                id: entry.id.clone(),
                timestamp: entry.timestamp,
                related_directories: entry.related_directories.clone(),
                content: read_markdown_body(&path)?,
            });
        }
        Ok(records)
    }

    /// 读取索引；索引文件不存在时返回空列表。
    pub fn load_entries(&self) -> Result<Vec<MemoryIndexEntry>, SessionStoreError> {
        if !self.index_path.is_file() {
            return Ok(Vec::new());
        }
        let text = std::fs::read_to_string(&self.index_path).map_err(|error| {
            SessionStoreError::new(format!(
                "读取记忆索引失败：{}，{error}",
                self.index_path.display()
            ))
        })?;
        let data: Value = serde_json::from_str(&text).map_err(|error| {
            SessionStoreError::new(format!(
                "记忆索引 JSON 解析失败：{}，第 {} 行第 {} 列：{error}",
                self.index_path.display(),
                error.line(),
                error.column()
            ))
        })?;
        let memories = match data.get("memories") {
            None => &Vec::new(),
            Some(Value::Array(items)) => items,
            Some(_) => {
                return Err(SessionStoreError::new(
                    "记忆索引顶层字段 memories 必须是列表。",
                ))
            }
        };
        let mut entries = Vec::new();
        for item in memories {
            if item.is_object() {
                entries.push(MemoryIndexEntry::from_dict(item)?);
            }
        }
        Ok(entries)
    }

    /// 落盘索引：先按（存储目录、路径）排序，再走原子替换。
    pub fn save_entries(&self, entries: &mut [MemoryIndexEntry]) -> Result<(), SessionStoreError> {
        std::fs::create_dir_all(&self.root).map_err(|error| {
            SessionStoreError::new(format!(
                "创建记忆目录失败：{}，{error}",
                self.root.display()
            ))
        })?;
        entries.sort_by(|left, right| {
            (left.storage_directory.as_str(), left.path.as_str())
                .cmp(&(right.storage_directory.as_str(), right.path.as_str()))
        });
        let payload = json!({
            "memories": entries.iter().map(MemoryIndexEntry::to_dict).collect::<Vec<Value>>(),
        });
        let text = serde_json::to_string_pretty(&payload).expect("记忆索引可序列化");
        atomic_write_text(&self.index_path, &text, self.policy.fsync).map_err(|error| {
            SessionStoreError::new(format!(
                "写入记忆索引失败：{}，{error}",
                self.index_path.display()
            ))
        })
    }

    /// 记忆条目对应的绝对路径；越出记忆根目录时报错。
    pub fn memory_path(&self, entry: &MemoryIndexEntry) -> Result<PathBuf, SessionStoreError> {
        let candidate = self.root.join(&entry.path);
        let path = normalize_path(&candidate)?;
        let root = normalize_path(&self.root)?;
        if !path.starts_with(&root) {
            return Err(SessionStoreError::new(format!(
                "记忆路径越界：{}",
                entry.path
            )));
        }
        Ok(path)
    }

    /// 读出这些记忆并把「最近用过」记下来：刷新时间戳、touch_count + 1、重写 Markdown。
    fn touch_entries(
        &self,
        memory_ids: &[String],
    ) -> Result<Vec<(String, MemoryIndexEntry)>, SessionStoreError> {
        let mut entries = self.load_entries()?;
        let mut touched: Vec<(String, MemoryIndexEntry)> = Vec::new();
        let now = Local::now().fixed_offset();
        for entry in entries.iter_mut() {
            if !memory_ids.iter().any(|memory_id| memory_id == &entry.id) {
                continue;
            }
            let path = self.memory_path(entry)?;
            if !path.is_file() {
                continue;
            }
            entry.timestamp = now;
            entry.touch_count += 1;
            let content = read_markdown_body(&path)?;
            let markdown = format_memory_markdown(now, &entry.related_directories, &content);
            std::fs::write(&path, markdown).map_err(|error| {
                SessionStoreError::new(format!("写入记忆文件失败：{}，{error}", path.display()))
            })?;
            touched.push((entry.id.clone(), entry.clone()));
        }

        if !touched.is_empty() {
            self.save_entries(&mut entries)?;
        }
        Ok(touched)
    }

    pub fn to_search_result(&self, entry: &MemoryIndexEntry) -> MemorySearchResult {
        MemorySearchResult {
            id: entry.id.clone(),
            summary: entry.summary.clone(),
            storage_directory: entry.storage_directory.clone(),
            related_directories: entry.related_directories.clone(),
            timestamp: entry.timestamp,
        }
    }

    /// 渲染用：把时间戳按本地时区格式化（供调用方与对照测试使用）。
    pub fn render_timestamp(timestamp: DateTime<FixedOffset>) -> String {
        format_memory_datetime(timestamp)
    }
}

/// 折叠 `.` 与 `..` 得到绝对路径（不要求路径已存在）。
fn normalize_path(path: &Path) -> Result<PathBuf, SessionStoreError> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|error| SessionStoreError::new(format!("取当前目录失败：{error}")))?
            .join(path)
    };
    let mut result = PathBuf::new();
    for component in absolute.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                result.pop();
            }
            other => result.push(other.as_os_str()),
        }
    }
    Ok(result)
}
