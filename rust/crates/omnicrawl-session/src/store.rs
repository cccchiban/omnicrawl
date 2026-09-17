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
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use serde_json::{json, Map, Value};

use crate::error::SessionStoreError;
use crate::event::SessionEvent;
use crate::index::SessionIndexEntry;
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
    lock_path: PathBuf,
    write_lock: Mutex<()>,
}

impl SessionStore {
    pub fn open(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        Self {
            index_path: root.join("index.json"),
            sessions_dir: root.join("sessions"),
            history_path: root.join("history.jsonl"),
            lock_path: root.join(".session_store.lock"),
            write_lock: Mutex::new(()),
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
        let _guard = self.lock_write();
        self.ensure()?;
        self.touch_lock_file()?;

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
        let _guard = self.lock_write();
        self.ensure()?;
        self.touch_lock_file()?;
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
        append_line(&path, &line).map_err(|error| {
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
        write_atomically(&self.index_path, &text)
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

    /// 锁文件记录持有者 pid（Python `.session_store.lock` 的同一约定）。
    ///
    /// 真正的跨进程互斥（超时、抢占失效锁）还没搬；这里先保证文件存在且内容一致。
    fn touch_lock_file(&self) -> Result<(), SessionStoreError> {
        let content = format!(
            "pid={}
",
            std::process::id()
        );
        if fs::read_to_string(&self.lock_path).ok().as_deref() == Some(content.as_str()) {
            return Ok(());
        }
        fs::write(&self.lock_path, content)
            .map_err(|error| io_error("写会话锁文件", &self.lock_path, &error))
    }

    /// 同进程串行；跨进程互斥待搬 `session_locking.py` 的语义。
    fn lock_write(&self) -> std::sync::MutexGuard<'_, ()> {
        self.write_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
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

/// 追加一行并 flush（对应 Python `_append_text_line`）。
fn append_line(path: &Path, line: &str) -> Result<(), String> {
    let mut handle = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|error| error.to_string())?;
    handle
        .write_all(line.as_bytes())
        .and_then(|()| handle.flush())
        .map_err(|error| error.to_string())
}

/// 原子替换文件内容（对应 Python `atomic_write_text`）：先写临时文件再改名。
fn write_atomically(path: &Path, text: &str) -> Result<(), SessionStoreError> {
    let temporary = path.with_extension("json.tmp");
    fs::write(&temporary, text).map_err(|error| io_error("写入临时文件", &temporary, &error))?;
    fs::rename(&temporary, path).map_err(|error| io_error("替换文件", path, &error))
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
