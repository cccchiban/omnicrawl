//! 会话存储的磁盘读写：目录骨架、索引维护、转录追加与读取、会话生命周期编排。
//!
//! 语义基准是 Python `omnicrawl/state/session.py` 的 `ensure` / `start_session` / `append_event` /
//! `read_session_events` / `list_sessions` / `rename_session` / `export_session_markdown` /
//! `archive_session` / `unarchive_session` / `delete_session` / `discard_empty_session` /
//! `list_project_paths`。两个实现读写同一批文件，所以文件布局、JSON 字节与错误文案都必须一致：
//! 索引是 `index.json`（紧凑 JSON + 换行），转录是 `sessions/<id>.jsonl`（一行一事件），
//! 归档转录落在 `archive/<id>.jsonl`，导出落在 `exports/`。
//!
//! 载荷整理（超长工具输出转 artifact、值级脱敏）委托 `crate::artifact::SessionArtifactStore`，
//! 与 Python `_prepare_event_payload` 同源；跨进程写锁在 `crate::locking`。

use std::collections::BTreeSet;
use std::fs::{self, File};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use chrono::{DateTime, Utc};
use memmap2::Mmap;
use serde_json::{json, Map, Value};

use crate::artifact::SessionArtifactStore;
use crate::error::SessionStoreError;
use crate::event::SessionEvent;
use crate::index::SessionIndexEntry;
use crate::locking::{
    append_text_line, atomic_write_text, process_lock_for_root, DurableWritePolicy,
    ProcessFileLock, ProcessLockGuard,
};
use crate::naming::{
    clean_title, new_session_id, normalize_relative_file_path, normalize_session_id,
    EMPTY_SESSION_EVENT_TYPES,
};
use crate::projection::active_session_events;

/// `session_started` 载荷里的运行时身份。
///
/// Python 记的是源码哈希与已加载模块（依赖解释器环境），内核记自己的实现版本；
/// 这个字段不参与跨实现比对。
pub fn kernel_runtime_identity() -> Value {
    json!({
        "implementation": "omnicrawl-kernel",
        "version": env!("CARGO_PKG_VERSION"),
    })
}

/// 新建会话的返回值（对应 Python 那边从 `SessionState` 取用的几个字段）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreatedSession {
    pub session_id: String,
    pub title: String,
    pub event_count: u64,
    pub last_event_type: String,
}

/// `list_sessions` 的筛选条件：工作区/项目路径、归档可见性与条数。
///
/// `project_path` 优先于 `workspace_root`（与 Python 的关键字参数一致）。
#[derive(Debug, Clone)]
pub struct SessionListQuery<'a> {
    pub workspace_root: Option<&'a str>,
    pub project_path: Option<&'a str>,
    pub limit: usize,
    pub include_archived: bool,
    pub archived_only: bool,
}

impl Default for SessionListQuery<'_> {
    fn default() -> Self {
        Self {
            workspace_root: None,
            project_path: None,
            limit: 10,
            include_archived: false,
            archived_only: false,
        }
    }
}

pub struct SessionStore {
    root: PathBuf,
    index_path: PathBuf,
    sessions_dir: PathBuf,
    history_path: PathBuf,
    write_lock: Mutex<()>,
    process_lock: Arc<ProcessFileLock>,
    policy: DurableWritePolicy,
}

/// 写路径的持锁凭据：先进程内锁、再跨进程文件锁（顺序固定，避免交叉死锁）。
struct WriteAccess<'a> {
    _thread: MutexGuard<'a, ()>,
    _process: ProcessLockGuard<'a>,
}

impl SessionStore {
    pub fn open(root: impl Into<PathBuf>) -> Self {
        Self::open_with_policy(root, DurableWritePolicy::default())
    }

    pub fn open_with_policy(root: impl Into<PathBuf>, policy: DurableWritePolicy) -> Self {
        let root = root.into();
        Self {
            index_path: root.join("index.json"),
            sessions_dir: root.join("sessions"),
            history_path: root.join("history.jsonl"),
            process_lock: process_lock_for_root(&root),
            write_lock: Mutex::new(()),
            policy,
            root,
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 建目录骨架与索引文件；已就绪时直接返回。
    pub fn ensure(&self) -> Result<(), SessionStoreError> {
        for relative in [
            "sessions",
            "artifacts",
            "summaries",
            "exports",
            "archive",
            "archive/compacted",
        ] {
            let path = self.root.join(relative);
            fs::create_dir_all(&path).map_err(|error| io_error("创建会话目录", &path, &error))?;
        }
        if !self.index_path.exists() {
            self.save_entries(&[])?;
        }
        if !self.history_path.exists() {
            File::create(&self.history_path)
                .map_err(|error| io_error("创建历史文件", &self.history_path, &error))?;
        }
        Ok(())
    }

    /// 新建会话并立即写入 `session_started` 事件。
    pub fn start_session(
        &self,
        workspace_root: &str,
        title: &str,
        now: DateTime<Utc>,
    ) -> Result<CreatedSession, SessionStoreError> {
        let _access = self.exclusive_write()?;
        self.ensure()?;

        let entries = self.load_entries()?;
        let session_id = self.allocate_session_id(&entries, now)?;
        let workspace = resolve_path_text(workspace_root);
        let title = {
            let cleaned = clean_title(title);
            if cleaned.is_empty() {
                "新会话".to_string()
            } else {
                cleaned
            }
        };
        let entry = SessionIndexEntry {
            session_id: session_id.clone(),
            title: title.clone(),
            workspace_root: workspace.clone(),
            path: format!("sessions/{session_id}.jsonl"),
            created_at: now,
            updated_at: now,
            event_count: 0,
            message_count: 0,
            last_event_type: String::new(),
            archived_at: None,
        };
        let mut entries = entries;
        entries.push(entry);
        self.save_entries(&entries)?;

        let payload = json!({
            "workspace_root": workspace,
            "title": title,
            "runtime": kernel_runtime_identity(),
        });
        let event = self.append_event_locked(
            &session_id,
            "session_started",
            as_object(payload),
            None,
            now,
        )?;

        Ok(CreatedSession {
            session_id: event.session_id,
            title,
            event_count: 1,
            last_event_type: "session_started".to_string(),
        })
    }

    /// 追加一个事件并更新索引。
    pub fn append_event(
        &self,
        session_id: &str,
        event_type: &str,
        payload: Map<String, Value>,
        parent_id: Option<&str>,
        now: DateTime<Utc>,
    ) -> Result<SessionEvent, SessionStoreError> {
        let _access = self.exclusive_write()?;
        self.ensure()?;
        self.append_event_locked(session_id, event_type, payload, parent_id, now)
    }

    fn append_event_locked(
        &self,
        session_id: &str,
        event_type: &str,
        payload: Map<String, Value>,
        parent_id: Option<&str>,
        now: DateTime<Utc>,
    ) -> Result<SessionEvent, SessionStoreError> {
        let session_id = normalize_session_id(session_id)?;
        let mut entries = self.load_entries()?;
        let index = self.entry_index(&entries, &session_id)?;
        let payload = as_object(self.artifact_store().prepare_event_payload(
            &session_id,
            event_type,
            Some(&Value::Object(payload)),
        )?);
        let event = SessionEvent::create(&session_id, event_type, payload, parent_id, now)?;

        let path = self.session_path(&entries[index].path)?;
        let line = format!("{}\n", event.to_json_line());
        append_text_line(&path, &line, self.policy.fsync).map_err(|error| {
            SessionStoreError::new(format!("写入会话转录失败：{}，{error}", path.display()))
        })?;

        update_entry_after_event(&mut entries[index], &event);
        self.save_entries(&entries)?;
        Ok(event)
    }

    /// 读取会话转录；坏行记为诊断并跳过（与 Python 一致）。
    pub fn read_events(&self, session_id: &str) -> Result<Vec<SessionEvent>, SessionStoreError> {
        let session_id = normalize_session_id(session_id)?;
        let entries = self.load_entries()?;
        let index = self.entry_index(&entries, &session_id)?;
        let path = self.session_path(&entries[index].path)?;
        read_events_from_path(&path)
    }

    /// 把被压缩窗口的原始事件写入二级归档，返回 archive_id。
    ///
    /// 归档目录为 `archive/compacted/<session_id>/<archive_id>.jsonl`，与整会话归档隔离；
    /// 每条事件一行 JSON，调用方传入与 Session 事件同构的 dict。归档失败不阻塞压缩主流程，
    /// 由调用方按 warning 处理。
    pub fn archive_compacted_events(
        &self,
        session_id: &str,
        events: &[Value],
        archive_id: Option<&str>,
        now: DateTime<Utc>,
    ) -> Result<String, SessionStoreError> {
        let _access = self.exclusive_write()?;
        self.ensure()?;
        let session_id = normalize_session_id(session_id)?;
        let entries = self.load_entries()?;
        // 不存在时抛错：归档只允许落在已有会话名下。
        self.entry_index(&entries, &session_id)?;

        let safe_archive_id = match archive_id {
            None => generated_archive_id(now),
            Some("") => generated_archive_id(now),
            Some(raw) => {
                let trimmed = raw.trim();
                if trimmed.is_empty() {
                    return Err(SessionStoreError::new("archive_id 必须是非空字符串。"));
                }
                if trimmed.contains('/')
                    || trimmed.contains(char::from(92))
                    || trimmed.contains(char::from(0))
                {
                    return Err(SessionStoreError::new("archive_id 不能包含路径分隔符。"));
                }
                trimmed.to_string()
            }
        };

        let compacted_dir = self.root.join("archive").join("compacted");
        let session_dir = compacted_dir.join(&session_id);
        if !is_within(&session_dir, &compacted_dir) {
            return Err(SessionStoreError::new(format!(
                "会话归档目录越界：{session_id}"
            )));
        }
        fs::create_dir_all(&session_dir)
            .map_err(|error| io_error("创建压缩归档目录", &session_dir, &error))?;
        let path = session_dir.join(format!("{safe_archive_id}.jsonl"));
        if !is_within(&path, &session_dir) {
            return Err(SessionStoreError::new("归档路径越界。"));
        }

        let lines: Vec<String> = events
            .iter()
            .map(|event| serde_json::to_string(event).unwrap_or_default())
            .collect();
        append_text_line(
            &path,
            &lines.join(
                "
",
            ),
            self.policy.fsync,
        )
        .map_err(|error| {
            SessionStoreError::new(format!("写入压缩事件归档失败：{}，{error}", path.display()))
        })?;
        Ok(safe_archive_id)
    }

    /// 读取该会话全部压缩归档事件，按 archive_id 字典序合并。
    ///
    /// 返回的事件 dict 与 `SessionEvent::to_dict()` 同构，可转回 SourceEvent
    /// 供精确证据恢复使用；不含归档则返回空列表。
    pub fn read_compacted_events(&self, session_id: &str) -> Result<Vec<Value>, SessionStoreError> {
        self.ensure()?;
        let session_id = normalize_session_id(session_id)?;
        let compacted_dir = self.root.join("archive").join("compacted");
        let session_dir = compacted_dir.join(&session_id);
        if !is_within(&session_dir, &compacted_dir) {
            return Err(SessionStoreError::new(format!(
                "会话归档目录越界：{session_id}"
            )));
        }
        if !session_dir.is_dir() {
            return Ok(Vec::new());
        }

        let mut paths: Vec<PathBuf> = fs::read_dir(&session_dir)
            .map_err(|error| io_error("读取压缩归档目录", &session_dir, &error))?
            .filter_map(|item| item.ok())
            .map(|item| item.path())
            .filter(|path| path.extension().map(|ext| ext == "jsonl").unwrap_or(false))
            .collect();
        paths.sort();

        let mut events: Vec<Value> = Vec::new();
        for path in paths {
            let Ok(text) = fs::read_to_string(&path) else {
                continue;
            };
            for line in text.lines() {
                if line.trim().is_empty() {
                    continue;
                }
                if let Ok(parsed) = serde_json::from_str::<Value>(line) {
                    if parsed.is_object() {
                        events.push(parsed);
                    }
                }
            }
        }
        Ok(events)
    }

    /// 列出索引里的全部会话：按更新时间倒序（同刻按创建时间倒序），不过滤归档也不截断。
    ///
    /// 这是给内核内部扫描用的「全量视图」；要 Python `list_sessions()` 的语义
    /// （默认隐藏归档、上限 10 条）用 `list_sessions_filtered`。
    pub fn list_sessions(&self) -> Result<Vec<SessionIndexEntry>, SessionStoreError> {
        let mut entries = self.load_entries()?;
        entries.sort_by(|left, right| {
            right
                .updated_at
                .cmp(&left.updated_at)
                .then_with(|| right.created_at.cmp(&left.created_at))
        });
        Ok(entries)
    }

    /// 带筛选的会话列表：对齐 Python `list_sessions` 的工作区/项目、归档与条数语义。
    ///
    /// 排序只按更新时间倒序且保持稳定（同刻按索引顺序），与 Python 的 `reverse=True` 一致；
    /// `limit` 按 `max(1, min(100, limit))` 收敛，避免调用方传 0 或超大值。
    pub fn list_sessions_filtered(
        &self,
        query: &SessionListQuery<'_>,
    ) -> Result<Vec<SessionIndexEntry>, SessionStoreError> {
        self.ensure()?;
        let mut entries = self.load_entries()?;
        let filter_path = query.project_path.or(query.workspace_root);
        if let Some(raw) = filter_path {
            let workspace = resolve_path_text(raw);
            entries.retain(|entry| entry.workspace_root == workspace);
        }
        if query.archived_only {
            entries.retain(|entry| entry.archived_at.is_some());
        } else if !query.include_archived {
            entries.retain(|entry| entry.archived_at.is_none());
        }
        entries.sort_by_key(|entry| std::cmp::Reverse(entry.updated_at));
        let limit = query.limit.clamp(1, 100);
        entries.truncate(limit);
        Ok(entries)
    }

    /// 列出索引中出现过的项目路径：按大小写折叠去重，再按同一折叠键排序。
    pub fn list_project_paths(
        &self,
        include_archived: bool,
    ) -> Result<Vec<String>, SessionStoreError> {
        self.ensure()?;
        let entries = self.load_entries()?;
        let mut seen: Vec<(String, String)> = Vec::new();
        for entry in entries {
            if !include_archived && entry.archived_at.is_some() {
                continue;
            }
            let key = entry.workspace_root.to_lowercase();
            if seen.iter().all(|(existing, _)| existing != &key) {
                seen.push((key, entry.workspace_root));
            }
        }
        seen.sort_by(|left, right| left.0.cmp(&right.0));
        Ok(seen.into_iter().map(|(_, value)| value).collect())
    }

    /// 更新会话标题：先写 `session_renamed` 事件，再刷新索引条目。
    ///
    /// 标题是可恢复元数据，不能只改 `index.json`；索引重建时靠这条事件还原最后一次命名。
    pub fn rename_session(
        &self,
        session_id: &str,
        title: &str,
        now: DateTime<Utc>,
    ) -> Result<SessionIndexEntry, SessionStoreError> {
        let cleaned = clean_title(title);
        if cleaned.is_empty() {
            return Err(SessionStoreError::new("会话标题不能为空。"));
        }
        self.append_event(
            session_id,
            "session_renamed",
            as_object(json!({ "title": cleaned })),
            None,
            now,
        )?;
        self.entry_of(session_id)
    }

    /// 把用户主动导出的会话 Markdown 写进 `exports/`，并记一条 `session_exported` 事件。
    ///
    /// 事件只存相对路径，不把整份 Markdown 写回转录，避免转录重复膨胀。
    pub fn export_session_markdown(
        &self,
        session_id: &str,
        markdown_text: &str,
        now: DateTime<Utc>,
    ) -> Result<PathBuf, SessionStoreError> {
        if markdown_text.trim().is_empty() {
            return Err(SessionStoreError::new("导出内容不能为空。"));
        }
        self.ensure()?;
        let session_id = normalize_session_id(session_id)?;
        let entries = self.load_entries()?;
        self.entry_index(&entries, &session_id)?;

        let filename = format!(
            "chat_export_{session_id}_{}.md",
            now.format("%Y%m%d_%H%M%S")
        );
        let path = self.root.join("exports").join(&filename);
        if !is_within(&path, &self.root) {
            return Err(SessionStoreError::new(format!("导出路径越界：{filename}")));
        }
        fs::write(&path, markdown_text).map_err(|error| io_error("写入会话导出", &path, &error))?;
        let relative = relative_posix(&path, &self.root)?;
        self.append_event(
            &session_id,
            "session_exported",
            as_object(json!({ "path": relative, "format": "markdown" })),
            None,
            now,
        )?;
        Ok(path)
    }

    /// 归档会话：把转录移进 `archive/` 并从默认列表隐藏。
    ///
    /// 只移动 JSONL，不动 artifact、summary 与 export；事件里同时留下旧路径与归档路径，
    /// 便于索引重建时看出归档动作。
    pub fn archive_session(
        &self,
        session_id: &str,
        now: DateTime<Utc>,
    ) -> Result<SessionIndexEntry, SessionStoreError> {
        let _access = self.exclusive_write()?;
        self.ensure()?;
        let session_id = normalize_session_id(session_id)?;
        let entries = self.load_entries()?;
        let index = self.entry_index(&entries, &session_id)?;
        if entries[index].archived_at.is_some() {
            return Err(SessionStoreError::new(format!(
                "会话已在归档中：{session_id}"
            )));
        }
        let archive_path = format!("archive/{session_id}.jsonl");
        let payload = json!({
            "previous_path": entries[index].path,
            "archive_path": archive_path,
        });
        self.append_event_locked(
            &session_id,
            "session_archived",
            as_object(payload),
            None,
            now,
        )?;
        let entries = self.load_entries()?;
        let index = self.entry_index(&entries, &session_id)?;
        let entry = entries[index].clone();
        self.move_session_file(&entry, &archive_path)?;
        self.replace_entry(&entry, &archive_path, Some(now), now)?;
        self.entry_of(&session_id)
    }

    /// 取消归档：把转录移回 `sessions/`，供 `/resume` 继续写入。
    pub fn unarchive_session(
        &self,
        session_id: &str,
        now: DateTime<Utc>,
    ) -> Result<SessionIndexEntry, SessionStoreError> {
        let _access = self.exclusive_write()?;
        self.ensure()?;
        let session_id = normalize_session_id(session_id)?;
        let entries = self.load_entries()?;
        let index = self.entry_index(&entries, &session_id)?;
        if entries[index].archived_at.is_none() {
            return Ok(entries[index].clone());
        }
        let active_path = format!("sessions/{session_id}.jsonl");
        let payload = json!({
            "previous_path": entries[index].path,
            "active_path": active_path,
        });
        self.append_event_locked(
            &session_id,
            "session_unarchived",
            as_object(payload),
            None,
            now,
        )?;
        let entries = self.load_entries()?;
        let index = self.entry_index(&entries, &session_id)?;
        let entry = entries[index].clone();
        self.move_session_file(&entry, &active_path)?;
        self.replace_entry(&entry, &active_path, None, now)?;
        self.entry_of(&session_id)
    }

    /// 彻底删除会话：移除转录、关联 artifact 目录与索引条目。不可逆，调用方自行确认。
    pub fn delete_session(&self, session_id: &str) -> Result<(), SessionStoreError> {
        let _access = self.exclusive_write()?;
        self.ensure()?;
        let session_id = normalize_session_id(session_id)?;
        let entries = self.load_entries()?;
        let index = self.entry_index(&entries, &session_id)?;
        self.remove_session_files(&entries[index], &session_id)?;
        Ok(())
    }

    /// 删除还没有真实聊天内容的空会话，返回是否真的删掉。
    ///
    /// 只有「索引计数为 0、未归档、且事件流全部属于空会话事件类型」才删：占位会话不该出现在历史里，
    /// 但任何业务事件（用户消息、回复、工具结果、重命名、导出、归档）都会让它留下。
    pub fn discard_empty_session(&self, session_id: &str) -> Result<bool, SessionStoreError> {
        let _access = self.exclusive_write()?;
        self.ensure()?;
        let session_id = normalize_session_id(session_id)?;
        let entries = self.load_entries()?;
        let index = self.entry_index(&entries, &session_id)?;
        let entry = entries[index].clone();
        if entry.archived_at.is_some() || entry.message_count > 0 {
            return Ok(false);
        }
        let events = active_session_events(&self.read_events_locked(&entry)?);
        let has_business_event = events
            .iter()
            .any(|event| !EMPTY_SESSION_EVENT_TYPES.contains(&event.event_type.as_str()));
        if has_business_event {
            return Ok(false);
        }
        self.remove_session_files(&entry, &session_id)?;
        Ok(true)
    }

    /// 读取会话 artifact 文本：路径规范化、会话归属与目录边界校验后返回内容。
    pub fn read_artifact_text(
        &self,
        session_id: &str,
        artifact_path: &str,
    ) -> Result<String, SessionStoreError> {
        let session_id = normalize_session_id(session_id)?;
        self.artifact_store().read_text(&session_id, artifact_path)
    }

    /// 把完整工具输出写进当前会话 artifact，返回相对路径（供批次输出预算使用）。
    pub fn write_tool_result_artifact(
        &self,
        session_id: &str,
        output: &str,
    ) -> Result<String, SessionStoreError> {
        let _access = self.exclusive_write()?;
        self.ensure()?;
        let session_id = normalize_session_id(session_id)?;
        let entries = self.load_entries()?;
        self.entry_index(&entries, &session_id)?;
        self.artifact_store()
            .write_tool_result_artifact(&session_id, output)
    }

    /// 按会话 id 取索引条目；不存在时报「未找到会话」。
    fn entry_of(&self, session_id: &str) -> Result<SessionIndexEntry, SessionStoreError> {
        let session_id = normalize_session_id(session_id)?;
        let entries = self.load_entries()?;
        let index = self.entry_index(&entries, &session_id)?;
        Ok(entries[index].clone())
    }

    /// 移动转录到目标相对路径（归档/取消归档共用）：目标必须不存在，源必须存在。
    fn move_session_file(
        &self,
        entry: &SessionIndexEntry,
        destination: &str,
    ) -> Result<(), SessionStoreError> {
        let source_path = self.session_path(&entry.path)?;
        let destination_relative = normalize_relative_file_path(destination)?;
        let destination_path = self.root.join(&destination_relative);
        if !is_within(&destination_path, &self.root) {
            return Err(SessionStoreError::new(format!(
                "会话归档路径越界：{destination}"
            )));
        }
        if let Some(parent) = destination_path.parent() {
            fs::create_dir_all(parent)
                .map_err(|error| io_error("创建会话归档目录", parent, &error))?;
        }
        if destination_path.exists() {
            return Err(SessionStoreError::new(format!(
                "会话归档目标已存在：{destination}"
            )));
        }
        if !source_path.exists() {
            return Err(SessionStoreError::new(format!(
                "会话转录不存在：{}",
                source_path.display()
            )));
        }
        fs::rename(&source_path, &destination_path).map_err(|error| {
            SessionStoreError::new(format!(
                "移动会话转录失败：{} -> {}，{error}",
                source_path.display(),
                destination_path.display()
            ))
        })
    }

    /// 替换索引里的转录路径与归档状态，保留标题、计数等业务元数据。
    fn replace_entry(
        &self,
        entry: &SessionIndexEntry,
        path: &str,
        archived_at: Option<DateTime<Utc>>,
        updated_at: DateTime<Utc>,
    ) -> Result<(), SessionStoreError> {
        let normalized_path = normalize_relative_file_path(path)?;
        let mut entries = self.load_entries()?;
        let index = self.entry_index(&entries, &entry.session_id)?;
        entries[index].path = normalized_path;
        entries[index].archived_at = archived_at;
        entries[index].updated_at = updated_at;
        self.save_entries(&entries)
    }

    /// 删除转录文件、artifact 目录与索引条目（删除与丢弃空会话共用）。
    fn remove_session_files(
        &self,
        entry: &SessionIndexEntry,
        session_id: &str,
    ) -> Result<(), SessionStoreError> {
        let session_file = self.session_path(&entry.path)?;
        if session_file.exists() {
            fs::remove_file(&session_file)
                .map_err(|error| io_error("删除会话转录", &session_file, &error))?;
        }
        let artifact_dir = self.root.join("artifacts").join(session_id);
        if artifact_dir.is_dir() {
            // 与 Python 的 `ignore_errors=True` 一致：清理失败不影响会话删除。
            let _ = fs::remove_dir_all(&artifact_dir);
        }
        let entries = self.load_entries()?;
        let remaining: Vec<SessionIndexEntry> = entries
            .into_iter()
            .filter(|item| item.session_id != session_id)
            .collect();
        self.save_entries(&remaining)
    }

    fn artifact_store(&self) -> SessionArtifactStore {
        SessionArtifactStore::new(&self.root, self.root.join("artifacts"))
    }

    /// 读持有写锁期间的转录（不重复取锁）。
    fn read_events_locked(
        &self,
        entry: &SessionIndexEntry,
    ) -> Result<Vec<SessionEvent>, SessionStoreError> {
        let path = self.session_path(&entry.path)?;
        read_events_from_path(&path)
    }

    fn load_entries(&self) -> Result<Vec<SessionIndexEntry>, SessionStoreError> {
        if !self.index_path.exists() {
            return Ok(Vec::new());
        }
        let text = fs::read_to_string(&self.index_path)
            .map_err(|error| io_error("读取会话索引", &self.index_path, &error))?;
        if text.trim().is_empty() {
            return Ok(Vec::new());
        }
        let value: Value = serde_json::from_str(&text).map_err(|error| {
            SessionStoreError::new(format!(
                "会话索引不是合法 JSON：{}，{error}",
                self.index_path.display()
            ))
        })?;
        if !value.is_object() {
            return Err(SessionStoreError::new("会话索引顶层必须是 JSON 对象。"));
        }
        if let Some(version) = value.get("schema_version") {
            if version.as_u64() != Some(u64::from(INDEX_SCHEMA_VERSION)) {
                return Err(SessionStoreError::new(format!(
                    "暂不支持的会话索引版本：{version}。"
                )));
            }
        }
        let Some(items) = value.get("sessions").and_then(Value::as_array) else {
            return Err(SessionStoreError::new(format!(
                "会话索引缺少 sessions 数组：{}",
                self.index_path.display()
            )));
        };
        items.iter().map(SessionIndexEntry::from_dict).collect()
    }

    fn save_entries(&self, entries: &[SessionIndexEntry]) -> Result<(), SessionStoreError> {
        let value = json!({
            "schema_version": INDEX_SCHEMA_VERSION,
            "sessions": entries.iter().map(SessionIndexEntry::to_dict).collect::<Vec<Value>>(),
        });
        let text = format!(
            "{}\n",
            serde_json::to_string(&value).expect("索引条目均可序列化")
        );
        atomic_write_text(&self.index_path, &text, self.policy.fsync).map_err(|error| {
            SessionStoreError::new(format!(
                "原子写入失败：{}，{error}",
                self.index_path.display()
            ))
        })
    }

    fn entry_index(
        &self,
        entries: &[SessionIndexEntry],
        session_id: &str,
    ) -> Result<usize, SessionStoreError> {
        entries
            .iter()
            .position(|entry| entry.session_id == session_id)
            .ok_or_else(|| SessionStoreError::new(format!("未找到会话：{session_id}")))
    }

    /// 转录路径必须落在会话目录内，越界即拒绝（索引由外部写入，不能当作可信输入）。
    fn session_path(&self, relative: &str) -> Result<PathBuf, SessionStoreError> {
        let relative = normalize_relative_file_path(relative)?;
        let path = self.root.join(&relative);
        if !is_within(&path, &self.root) {
            return Err(SessionStoreError::new(format!("会话路径越界：{relative}")));
        }
        Ok(path)
    }

    fn allocate_session_id(
        &self,
        entries: &[SessionIndexEntry],
        now: DateTime<Utc>,
    ) -> Result<String, SessionStoreError> {
        let existing: BTreeSet<&str> = entries
            .iter()
            .map(|entry| entry.session_id.as_str())
            .collect();
        for _ in 0..64 {
            let session_id = new_session_id(now);
            let taken = existing.contains(session_id.as_str())
                || self
                    .sessions_dir
                    .join(format!("{session_id}.jsonl"))
                    .exists();
            if !taken {
                return Ok(session_id);
            }
        }
        Err(SessionStoreError::new("无法分配会话 id：连续撞车。"))
    }

    /// 写路径统一入口：先进程内串行，再取跨进程文件锁；顺序固定，避免交叉死锁。
    ///
    /// 不可重入：内部步骤一律走 `*_locked` 方法，不要在持锁期间再调公开写方法。
    fn exclusive_write(&self) -> Result<WriteAccess<'_>, SessionStoreError> {
        let thread = self
            .write_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let process = self.process_lock.acquire(
            self.policy.lock_timeout_seconds,
            self.policy.lock_poll_seconds,
        )?;
        Ok(WriteAccess {
            _thread: thread,
            _process: process,
        })
    }
}

/// 归档 id：`compact-<UTC 时间戳>-<随机后缀>`；随机后缀复用会话 id 的熵源。
fn generated_archive_id(now: DateTime<Utc>) -> String {
    format!(
        "compact-{}-{}",
        now.format("%Y%m%d-%H%M%S"),
        crate::naming::random_suffix()
    )
}

/// 索引信封版本；与 Python 侧一致，读的时候只认 sessions 数组。
const INDEX_SCHEMA_VERSION: u32 = 1;

/// 读任意转录路径：大文件走只读内存映射，坏行只留诊断。
fn read_events_from_path(path: &Path) -> Result<Vec<SessionEvent>, SessionStoreError> {
    let file = File::open(path).map_err(|error| io_error("读取会话转录", path, &error))?;
    let size = file
        .metadata()
        .map_err(|error| io_error("读取会话转录", path, &error))?
        .len();

    let mut events = Vec::new();
    if size >= TRANSCRIPT_MMAP_THRESHOLD_BYTES {
        // 只读映射：整份转录不复制进堆，逐行解析只借用映射内的切片。
        let mapped =
            unsafe { Mmap::map(&file) }.map_err(|error| io_error("读取会话转录", path, &error))?;
        let text = std::str::from_utf8(&mapped).map_err(|error| {
            io_error(
                "读取会话转录",
                path,
                &std::io::Error::new(std::io::ErrorKind::InvalidData, error),
            )
        })?;
        collect_events(text, &mut events)?;
    } else {
        let text =
            fs::read_to_string(path).map_err(|error| io_error("读取会话转录", path, &error))?;
        collect_events(&text, &mut events)?;
    }
    Ok(events)
}

/// 路径相对 `root` 的 posix 写法（事件载荷里的路径统一用正斜杠）。
fn relative_posix(path: &Path, root: &Path) -> Result<String, SessionStoreError> {
    let relative = path
        .strip_prefix(root)
        .map_err(|_| SessionStoreError::new(format!("路径不在会话目录内：{}", path.display())))?;
    let parts: Vec<String> = relative
        .components()
        .map(|component| component.as_os_str().to_string_lossy().to_string())
        .collect();
    if parts.is_empty() {
        return Err(SessionStoreError::new(format!(
            "路径不在会话目录内：{}",
            path.display()
        )));
    }
    Ok(parts.join("/"))
}

/// 索引计数与标题随事件演进，规则与 Python `_update_entry_after_event` 一致。
fn update_entry_after_event(entry: &mut SessionIndexEntry, event: &SessionEvent) {
    entry.event_count += 1;
    entry.updated_at = event.created_at;
    entry.last_event_type = event.event_type.clone();
    if crate::naming::MESSAGE_EVENT_TYPES.contains(&event.event_type.as_str()) {
        entry.message_count += 1;
    }
    if entry.message_count == 1 && event.event_type == "user_message" {
        if let Some(content) = event.payload.get("content").and_then(Value::as_str) {
            if !content.trim().is_empty() {
                entry.title = clean_title(content);
            }
        }
    }
    if event.event_type == "session_renamed" {
        if let Some(title) = event.payload.get("title").and_then(Value::as_str) {
            if !title.trim().is_empty() {
                entry.title = clean_title(title);
            }
        }
    }
}

fn as_object(value: Value) -> Map<String, Value> {
    value.as_object().cloned().unwrap_or_default()
}

/// 超过该体量的转录走只读内存映射，避免整份内容复制进内核堆。
const TRANSCRIPT_MMAP_THRESHOLD_BYTES: u64 = 1 << 20;

/// 逐行解析转录文本：空白行跳过，坏行只留一行 stderr 诊断（与 Python 一致）。
fn collect_events(text: &str, events: &mut Vec<SessionEvent>) -> Result<(), SessionStoreError> {
    for (number, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            eprintln!("[kernel] 会话转录 JSON 损坏：第 {} 行。", number + 1);
            continue;
        };
        events.push(SessionEvent::from_dict(&value)?);
    }
    Ok(())
}

fn io_error(action: &str, path: &Path, error: &std::io::Error) -> SessionStoreError {
    SessionStoreError::new(format!("{action}失败：{}，{error}", path.display()))
}

/// 路径是否位于 `root` 之内（逐组件比较，避免前缀字符串误判）。
fn is_within(path: &Path, root: &Path) -> bool {
    let mut components = path.components();
    for root_component in root.components() {
        if components.next() != Some(root_component) {
            return false;
        }
    }
    true
}

/// 把工作区路径规范化成 Python `str(Path(...).resolve())` 的形状：
/// 绝对化、折叠 `.` 与 `..`、使用平台分隔符。
fn resolve_path_text(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let path = Path::new(trimmed);
    let (prefix, parts): (String, Vec<String>) = if path.is_absolute() {
        let mut components = path.components();
        let prefix = components
            .next()
            .map(|component| component.as_os_str().to_string_lossy().to_string())
            .unwrap_or_default();
        (prefix, Vec::new())
    } else {
        let cwd = std::env::current_dir().unwrap_or_default();
        let mut parts: Vec<String> = cwd
            .components()
            .map(|component| component.as_os_str().to_string_lossy().to_string())
            .collect();
        let prefix = parts.remove(0);
        (prefix, parts)
    };
    let mut collected = parts;
    for component in path.components() {
        match component {
            Component::RootDir | Component::Prefix(_) => {}
            Component::CurDir => {}
            Component::ParentDir => {
                collected.pop();
            }
            Component::Normal(part) => collected.push(part.to_string_lossy().to_string()),
        }
    }
    let separator = std::path::MAIN_SEPARATOR.to_string();
    let joined = collected.join(&separator);
    if prefix.ends_with(&separator) {
        format!("{prefix}{joined}")
    } else {
        format!("{prefix}{separator}{joined}")
    }
}
