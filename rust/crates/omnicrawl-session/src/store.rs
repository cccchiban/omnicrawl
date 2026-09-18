//! 会话存储的磁盘读写：目录骨架、索引维护、转录追加与读取。
//!
//! 语义基准是 Python `omnicrawl/state/session.py` 的 `ensure` / `start_session` / `append_event` /
//! `read_session_events` / `list_sessions`。两个实现读写同一批文件，所以文件布局、JSON 字节与
//! 错误文案都必须一致：索引是 `index.json`（紧凑 JSON + 换行），转录是 `sessions/<id>.jsonl`
//! （一行一事件）。
//!
//! 尚未搬运的部分：跨进程锁（Python 用 `.session_store.lock`，本实现只保证同进程串行并创建该文件）、
//! 载荷脱敏与 artifact 改写、归档/导出/一致性诊断、以及从事件恢复上下文的投影。

use std::collections::BTreeSet;
use std::fs::{self, File};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use chrono::{DateTime, Utc};
use serde_json::{json, Map, Value};

use crate::error::SessionStoreError;
use crate::event::SessionEvent;
use crate::index::SessionIndexEntry;
use crate::locking::{
    append_text_line, atomic_write_text, process_lock_for_root, DurableWritePolicy,
    ProcessFileLock, ProcessLockGuard,
};
use crate::naming::{
    clean_title, new_session_id, normalize_relative_file_path, normalize_session_id,
};

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
        let payload = prepare_payload(event_type, payload);
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
        let text =
            fs::read_to_string(&path).map_err(|error| io_error("读取会话转录", &path, &error))?;

        let mut events = Vec::new();
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
        Ok(events)
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

    /// 列出索引里的会话：按更新时间倒序（同刻按创建时间倒序）。
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

/// 落盘前的载荷整理。
///
/// `tool_result` 会补上去重与展示需要的字段（sha256、字符数、存储位置），
/// 与 Python `_prepare_tool_result_payload` 的 inline 分支一致；超长输出转 artifact
/// 文件、以及敏感值脱敏都尚未搬运。
fn prepare_payload(event_type: &str, payload: Map<String, Value>) -> Map<String, Value> {
    if event_type != "tool_result" {
        return payload;
    }
    let Some(output) = payload
        .get("output")
        .and_then(Value::as_str)
        .map(str::to_string)
    else {
        return payload;
    };
    let mut prepared = payload;
    prepared.insert(
        "output_sha256".to_string(),
        Value::String(sha256_hex(&output)),
    );
    prepared.insert(
        "output_size_chars".to_string(),
        json!(output.chars().count()),
    );
    prepared.insert("storage".to_string(), json!("inline"));
    prepared
}

fn sha256_hex(text: &str) -> String {
    use sha2::{Digest, Sha256};

    let mut hasher = Sha256::new();
    hasher.update(text.as_bytes());
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
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
