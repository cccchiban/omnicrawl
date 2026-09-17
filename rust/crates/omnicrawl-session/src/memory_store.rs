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
    dedupe_directories, format_memory_datetime, format_memory_markdown, normalize_content,
    normalize_directory, read_markdown_body, MemoryIndexEntry,
};
use crate::memory_ranking::{
    classify_storage_directory, directories_overlap, make_summary, merge_memory_content,
    normalize_for_compare, text_similarity,
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

/// 模型写入长期记忆时提交的最小结构。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MemoryWriteRequest {
    pub content: String,
    pub related_directories: Vec<String>,
    pub storage_directory: Option<String>,
    pub source_event: Option<String>,
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

    /// 写入或合并长期记忆，并在写入后执行一次过期清理。
    ///
    /// 批量请求先全部校验与归一化，再开始落盘——避免后续请求非法时前面已经写出
    /// Markdown 但索引没更新。落盘中途出错则尽力回滚，不留未索引记忆。
    pub fn write(
        &self,
        memories: &[MemoryWriteRequest],
    ) -> Result<Vec<MemoryRecord>, SessionStoreError> {
        if memories.is_empty() {
            return Ok(Vec::new());
        }

        let mut prepared = Vec::new();
        for request in memories {
            prepared.push(self.prepare_write_request(request)?);
        }
        std::fs::create_dir_all(&self.root).map_err(|error| {
            SessionStoreError::new(format!(
                "创建记忆目录失败：{}，{error}",
                self.root.display()
            ))
        })?;

        let mut entries = self.load_entries()?;
        let mut records = Vec::new();
        let mut created_paths: Vec<PathBuf> = Vec::new();
        let mut updated_backups: Vec<(PathBuf, Option<String>)> = Vec::new();

        let outcome = (|| -> Result<(), SessionStoreError> {
            for (content, storage_directory, related_directories) in &prepared {
                let existing = self.find_duplicate_entry(
                    &entries,
                    content,
                    storage_directory,
                    related_directories,
                )?;
                match existing {
                    Some(index) => {
                        let path = self.memory_path(&entries[index])?;
                        if !updated_backups.iter().any(|(known, _)| known == &path) {
                            let previous = if path.is_file() {
                                Some(std::fs::read_to_string(&path).map_err(|error| {
                                    SessionStoreError::new(format!(
                                        "读取记忆文件失败：{}，{error}",
                                        path.display()
                                    ))
                                })?)
                            } else {
                                None
                            };
                            updated_backups.push((path, previous));
                        }
                        let record = self.update_existing_memory(
                            &mut entries[index],
                            content,
                            related_directories,
                        )?;
                        records.push(record);
                    }
                    None => {
                        let record = self.create_memory(
                            content,
                            storage_directory,
                            related_directories,
                            &entries,
                        )?;
                        created_paths.push(
                            self.root
                                .join(storage_directory)
                                .join(format!("{}.md", record.id)),
                        );
                        entries.push(MemoryIndexEntry {
                            id: record.id.clone(),
                            path: format!("{storage_directory}/{}.md", record.id),
                            storage_directory: storage_directory.clone(),
                            timestamp: record.timestamp,
                            touch_count: 0,
                            related_directories: record.related_directories.clone(),
                            summary: make_summary(&record.content, 120),
                        });
                        records.push(record);
                    }
                }
            }
            self.save_entries(&mut entries)
        })();

        if let Err(error) = outcome {
            self.rollback_file_changes(&created_paths, &updated_backups);
            return Err(error);
        }

        self.clean_expired_memories()?;
        Ok(records)
    }

    /// 校验并归一化一条写入请求：正文、存储目录、关联目录。
    fn prepare_write_request(
        &self,
        request: &MemoryWriteRequest,
    ) -> Result<(String, String, Vec<String>), SessionStoreError> {
        let content = normalize_content(&request.content);
        if content.is_empty() {
            return Err(SessionStoreError::new("写入记忆的 content 不能为空。"));
        }
        let storage_directory =
            self.choose_storage_directory(&content, request.storage_directory.as_deref())?;
        let requested: Vec<Value> = request
            .related_directories
            .iter()
            .map(|item| Value::String(item.clone()))
            .collect();
        let mut related_directories = dedupe_directories(&requested);
        if !related_directories.contains(&storage_directory) {
            related_directories.push(storage_directory.clone());
        }
        Ok((content, storage_directory, related_directories))
    }

    fn choose_storage_directory(
        &self,
        content: &str,
        requested_directory: Option<&str>,
    ) -> Result<String, SessionStoreError> {
        match requested_directory {
            Some(directory) if !directory.trim().is_empty() => normalize_directory(directory),
            _ => Ok(classify_storage_directory(content)),
        }
    }

    fn create_memory(
        &self,
        content: &str,
        storage_directory: &str,
        related_directories: &[String],
        existing_entries: &[MemoryIndexEntry],
    ) -> Result<MemoryRecord, SessionStoreError> {
        let timestamp = Local::now().fixed_offset();
        let memory_id = self.make_memory_id(timestamp, existing_entries);
        let path = self
            .root
            .join(storage_directory)
            .join(format!("{memory_id}.md"));
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| {
                SessionStoreError::new(format!("创建记忆目录失败：{}，{error}", parent.display()))
            })?;
        }
        let markdown = format_memory_markdown(timestamp, related_directories, content);
        std::fs::write(&path, markdown).map_err(|error| {
            SessionStoreError::new(format!("写入记忆文件失败：{}，{error}", path.display()))
        })?;
        Ok(MemoryRecord {
            id: memory_id,
            timestamp,
            related_directories: related_directories.to_vec(),
            content: content.to_string(),
        })
    }

    fn update_existing_memory(
        &self,
        entry: &mut MemoryIndexEntry,
        content: &str,
        related_directories: &[String],
    ) -> Result<MemoryRecord, SessionStoreError> {
        let path = self.memory_path(entry)?;
        let old_content = if path.is_file() {
            read_markdown_body(&path)?
        } else {
            String::new()
        };
        let merged = merge_memory_content(&old_content, content);
        let timestamp = Local::now().fixed_offset();
        entry.timestamp = timestamp;
        entry.touch_count += 1;

        let mut combined: Vec<Value> = entry
            .related_directories
            .iter()
            .map(|item| Value::String(item.clone()))
            .collect();
        combined.extend(
            related_directories
                .iter()
                .map(|item| Value::String(item.clone())),
        );
        entry.related_directories = dedupe_directories(&combined);
        entry.summary = make_summary(&merged, 120);

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| {
                SessionStoreError::new(format!("创建记忆目录失败：{}，{error}", parent.display()))
            })?;
        }
        let markdown = format_memory_markdown(timestamp, &entry.related_directories, &merged);
        std::fs::write(&path, markdown).map_err(|error| {
            SessionStoreError::new(format!("写入记忆文件失败：{}，{error}", path.display()))
        })?;
        Ok(MemoryRecord {
            id: entry.id.clone(),
            timestamp,
            related_directories: entry.related_directories.clone(),
            content: merged,
        })
    }

    /// 找近似重复的既有记忆：内容归一化后相同，或相似度达到 0.9。
    fn find_duplicate_entry(
        &self,
        entries: &[MemoryIndexEntry],
        content: &str,
        storage_directory: &str,
        related_directories: &[String],
    ) -> Result<Option<usize>, SessionStoreError> {
        let normalized_content = normalize_for_compare(content);
        for (index, entry) in entries.iter().enumerate() {
            if entry.storage_directory != storage_directory
                && !directories_overlap(&entry.related_directories, related_directories)
            {
                continue;
            }
            let path = self.memory_path(entry)?;
            let old_content = if path.is_file() {
                read_markdown_body(&path)?
            } else {
                entry.summary.clone()
            };
            if normalize_for_compare(&old_content) == normalized_content {
                return Ok(Some(index));
            }
            if text_similarity(&old_content, content) >= 0.9 {
                return Ok(Some(index));
            }
        }
        Ok(None)
    }

    /// 批量写入失败时尽力回滚已落盘文件，避免未索引记忆残留。
    fn rollback_file_changes(
        &self,
        created_paths: &[PathBuf],
        updated_file_backups: &[(PathBuf, Option<String>)],
    ) {
        for path in created_paths {
            if path.is_file() {
                let _ = std::fs::remove_file(path);
            }
        }
        for (path, previous_text) in updated_file_backups {
            match previous_text {
                None => {
                    if path.is_file() {
                        let _ = std::fs::remove_file(path);
                    }
                }
                Some(text) => {
                    if let Some(parent) = path.parent() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                    let _ = std::fs::write(path, text);
                }
            }
        }
    }

    /// 记忆 id：时间戳到秒，冲突时追加三位序号。
    fn make_memory_id(
        &self,
        timestamp: DateTime<FixedOffset>,
        existing_entries: &[MemoryIndexEntry],
    ) -> String {
        let base = timestamp.format("%Y%m%d-%H%M%S").to_string();
        let mut memory_id = base.clone();
        let mut suffix = 1;
        while existing_entries.iter().any(|entry| entry.id == memory_id)
            || self.memory_file_exists(&memory_id)
        {
            suffix += 1;
            memory_id = format!("{base}-{suffix:03}");
        }
        memory_id
    }

    fn memory_file_exists(&self, memory_id: &str) -> bool {
        if !self.root.is_dir() {
            return false;
        }
        let target = format!("{memory_id}.md");
        let mut stack = vec![self.root.clone()];
        while let Some(directory) = stack.pop() {
            let Ok(items) = std::fs::read_dir(&directory) else {
                continue;
            };
            for item in items.flatten() {
                let path = item.path();
                if path.is_dir() {
                    stack.push(path);
                } else if path.file_name().and_then(|name| name.to_str()) == Some(target.as_str()) {
                    return true;
                }
            }
        }
        false
    }

    /// 按 7 + touch_count 天规则清理过期记忆，并移除空目录。
    pub fn clean_expired_memories(&self) -> Result<Vec<String>, SessionStoreError> {
        let mut entries = self.load_entries()?;
        if entries.is_empty() {
            return Ok(Vec::new());
        }

        let now = Local::now().fixed_offset();
        let mut kept: Vec<MemoryIndexEntry> = Vec::new();
        let mut deleted_paths: Vec<String> = Vec::new();
        for entry in std::mem::take(&mut entries) {
            let age_days =
                (now - entry.timestamp).num_microseconds().unwrap_or(0) as f64 / 1e6 / 86400.0;
            let expire_days = 7.0 + entry.touch_count as f64;
            if age_days < expire_days {
                kept.push(entry);
                continue;
            }

            let path = self.memory_path(&entry)?;
            deleted_paths.push(entry.path.clone());
            if path.is_file() {
                std::fs::remove_file(&path).map_err(|error| {
                    SessionStoreError::new(format!("删除过期记忆失败：{}，{error}", entry.path))
                })?;
            }
        }

        if !deleted_paths.is_empty() {
            self.save_entries(&mut kept)?;
            self.clean_empty_directories();
        }
        Ok(deleted_paths)
    }

    /// 递归删除记忆根目录下的空目录（失败忽略）。
    pub fn clean_empty_directories(&self) {
        if !self.root.is_dir() {
            return;
        }
        let mut directories: Vec<PathBuf> = Vec::new();
        let mut stack = vec![self.root.clone()];
        while let Some(directory) = stack.pop() {
            let Ok(items) = std::fs::read_dir(&directory) else {
                continue;
            };
            for item in items.flatten() {
                let path = item.path();
                if path.is_dir() {
                    directories.push(path.clone());
                    stack.push(path);
                }
            }
        }
        // 由深到浅删除，父目录才可能在子目录清空后也变空。
        directories.sort_by_key(|path| std::cmp::Reverse(path.components().count()));
        for directory in directories {
            let _ = std::fs::remove_dir(&directory);
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
