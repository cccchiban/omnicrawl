//! 会话转录与 `index.json` 的一致性诊断（Python `omnicrawl/state/session_consistency.py` 的对照实现）。
//!
//! 本切片覆盖两个纯函数：从完整事件流重建索引条目 `build_index_entry_from_events`，以及把现有
//! 条目与重建值对照 `compare_index_entry`。诊断码、严重级别、文案与 `details` 键序都是对外契约，
//! 逐字段对照见 `tests/consistency_parity.rs`。

use chrono::{DateTime, Utc};
use serde_json::{Map, Value};

use crate::error::SessionStoreError;
use crate::index::SessionIndexEntry;
use crate::naming::{normalize_relative_file_path, normalize_session_id, MESSAGE_EVENT_TYPES};
use crate::projection::{active_session_events, session_title_from_events};
use crate::time::format_datetime;

// 诊断码保持稳定，便于测试、日志与将来的 API/TUI 展示复用。
pub const ISSUE_PATH_MISMATCH: &str = "path_mismatch";
pub const ISSUE_EVENT_COUNT_MISMATCH: &str = "event_count_mismatch";
pub const ISSUE_MESSAGE_COUNT_MISMATCH: &str = "message_count_mismatch";
pub const ISSUE_TITLE_MISMATCH: &str = "title_mismatch";
pub const ISSUE_LAST_EVENT_MISMATCH: &str = "last_event_type_mismatch";
pub const ISSUE_UPDATED_AT_MISMATCH: &str = "updated_at_mismatch";
pub const ISSUE_WORKSPACE_MISMATCH: &str = "workspace_root_mismatch";
pub const ISSUE_ARCHIVED_MISMATCH: &str = "archived_at_mismatch";

pub const SEVERITY_ERROR: &str = "error";
pub const SEVERITY_WARNING: &str = "warning";

/// 重建标题时的缺省值，与 Python `session_title_from_events` 的 fallback 一致。
const DEFAULT_TITLE: &str = "新会话";
/// 索引模型要求非空工作区；转录里给不出时用这个可识别占位值，扫描阶段会单独告警。
const UNKNOWN_WORKSPACE: &str = "(unknown)";
/// 归档态的判定基准：转录位于该前缀下即视为已归档。
const ARCHIVE_PATH_PREFIX: &str = "archive/";

#[derive(Debug, Clone, PartialEq)]
pub struct SessionConsistencyIssue {
    pub code: String,
    pub severity: String,
    pub message: String,
    pub session_id: Option<String>,
    pub path: Option<String>,
    pub details: Map<String, Value>,
    /// 为真表示重写 `index.json` 即可消除该问题，不删用户数据。
    pub repairable: bool,
}

impl SessionConsistencyIssue {
    pub fn to_dict(&self) -> Value {
        let mut object = Map::new();
        object.insert("code".into(), Value::String(self.code.clone()));
        object.insert("severity".into(), Value::String(self.severity.clone()));
        object.insert("message".into(), Value::String(self.message.clone()));
        object.insert("session_id".into(), optional_string(&self.session_id));
        object.insert("path".into(), optional_string(&self.path));
        object.insert("details".into(), Value::Object(self.details.clone()));
        object.insert("repairable".into(), Value::Bool(self.repairable));
        Value::Object(object)
    }
}

fn optional_string(value: &Option<String>) -> Value {
    match value {
        Some(text) => Value::String(text.clone()),
        None => Value::Null,
    }
}

/// Python `repr()` 的字符串分支：默认单引号，串内只有单引号时改用双引号。
fn python_repr(value: &str) -> String {
    let quote = if value.contains('\'') && !value.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::new();
    out.push(quote);
    for character in value.chars() {
        match character {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            other if other == quote => {
                out.push('\\');
                out.push(other);
            }
            other => out.push(other),
        }
    }
    out.push(quote);
    out
}

/// 根据完整事件流重建索引核心字段。
///
/// 标题规则与 `SessionStore` 的事件后更新保持一致：首条用户消息可覆盖默认标题，随后的
/// `session_renamed` 覆盖最终标题。归档状态以文件实际位置为准：位于 `archive/` 即视为已归档。
pub fn build_index_entry_from_events(
    session_id: &str,
    relative_path: &str,
    events: &[crate::SessionEvent],
) -> Result<SessionIndexEntry, SessionStoreError> {
    let normalized_id = normalize_session_id(session_id)?;
    let normalized_path = normalize_relative_file_path(relative_path)?;
    let Some(last_event) = events.last() else {
        return Err(SessionStoreError::new(format!(
            "无法从空转录重建索引：{normalized_id}"
        )));
    };

    let active_events = active_session_events(events);
    let title = session_title_from_events(&active_events, DEFAULT_TITLE);
    let message_count = active_events
        .iter()
        .filter(|event| MESSAGE_EVENT_TYPES.contains(&event.event_type.as_str()))
        .count() as u64;

    let mut workspace_root = String::new();
    let mut archived_at: Option<DateTime<Utc>> = None;
    for event in &active_events {
        if event.event_type == "session_started" {
            if let Some(workspace) = event.payload.get("workspace_root").and_then(Value::as_str) {
                if !workspace.trim().is_empty() {
                    workspace_root = workspace.trim().to_string();
                }
            }
        }
        if event.event_type == "session_archived" {
            archived_at = Some(event.created_at);
        } else if event.event_type == "session_unarchived" {
            archived_at = None;
        }
    }

    if normalized_path.starts_with(ARCHIVE_PATH_PREFIX) {
        if archived_at.is_none() {
            archived_at = Some(last_event.created_at);
        }
    } else {
        // 活跃目录中的文件视为未归档，即使历史中出现过归档事件。
        archived_at = None;
    }

    if workspace_root.is_empty() {
        workspace_root = UNKNOWN_WORKSPACE.to_string();
    }

    Ok(SessionIndexEntry {
        session_id: normalized_id,
        title,
        workspace_root,
        path: normalized_path,
        created_at: events[0].created_at,
        updated_at: last_event.created_at,
        event_count: events.len() as u64,
        message_count,
        last_event_type: last_event.event_type.clone(),
        archived_at,
    })
}

/// 比较现有索引条目与从转录重建的期望值，按 Python 的检查顺序返回诊断列表。
pub fn compare_index_entry(
    current: &SessionIndexEntry,
    expected: &SessionIndexEntry,
) -> Vec<SessionConsistencyIssue> {
    let mut issues = Vec::new();
    let mut add = |code: &str,
                   message: String,
                   current_value: Value,
                   expected_value: Value,
                   severity: &str| {
        let mut details = Map::new();
        details.insert("current".into(), current_value);
        details.insert("expected".into(), expected_value);
        issues.push(SessionConsistencyIssue {
            code: code.to_string(),
            severity: severity.to_string(),
            message,
            session_id: Some(current.session_id.clone()),
            path: Some(current.path.clone()),
            details,
            repairable: true,
        });
    };

    if current.path != expected.path {
        add(
            ISSUE_PATH_MISMATCH,
            format!(
                "会话路径不一致：索引为 {}，转录位于 {}。",
                current.path, expected.path
            ),
            Value::String(current.path.clone()),
            Value::String(expected.path.clone()),
            SEVERITY_ERROR,
        );
    }
    if current.event_count != expected.event_count {
        add(
            ISSUE_EVENT_COUNT_MISMATCH,
            format!(
                "事件数不一致：索引 {}，转录 {}。",
                current.event_count, expected.event_count
            ),
            Value::from(current.event_count),
            Value::from(expected.event_count),
            SEVERITY_ERROR,
        );
    }
    if current.message_count != expected.message_count {
        add(
            ISSUE_MESSAGE_COUNT_MISMATCH,
            format!(
                "消息数不一致：索引 {}，转录 {}。",
                current.message_count, expected.message_count
            ),
            Value::from(current.message_count),
            Value::from(expected.message_count),
            SEVERITY_ERROR,
        );
    }
    if current.title != expected.title {
        add(
            ISSUE_TITLE_MISMATCH,
            format!(
                "标题不一致：索引为 {}，转录推导为 {}。",
                python_repr(&current.title),
                python_repr(&expected.title)
            ),
            Value::String(current.title.clone()),
            Value::String(expected.title.clone()),
            SEVERITY_WARNING,
        );
    }
    if current.last_event_type != expected.last_event_type {
        add(
            ISSUE_LAST_EVENT_MISMATCH,
            format!(
                "最后事件类型不一致：索引为 {}，转录为 {}。",
                python_repr(&current.last_event_type),
                python_repr(&expected.last_event_type)
            ),
            Value::String(current.last_event_type.clone()),
            Value::String(expected.last_event_type.clone()),
            SEVERITY_ERROR,
        );
    }
    if current.updated_at != expected.updated_at {
        add(
            ISSUE_UPDATED_AT_MISMATCH,
            "更新时间与转录最后事件时间不一致。".to_string(),
            Value::String(format_datetime(current.updated_at)),
            Value::String(format_datetime(expected.updated_at)),
            SEVERITY_WARNING,
        );
    }
    if current.workspace_root != expected.workspace_root {
        add(
            ISSUE_WORKSPACE_MISMATCH,
            "工作区路径与 session_started 事件不一致。".to_string(),
            Value::String(current.workspace_root.clone()),
            Value::String(expected.workspace_root.clone()),
            SEVERITY_WARNING,
        );
    }
    if current.archived_at != expected.archived_at {
        add(
            ISSUE_ARCHIVED_MISMATCH,
            "归档状态与转录路径/事件不一致。".to_string(),
            archived_value(current.archived_at),
            archived_value(expected.archived_at),
            SEVERITY_ERROR,
        );
    }
    issues
}

fn archived_value(value: Option<DateTime<Utc>>) -> Value {
    match value {
        Some(stamp) => Value::String(format_datetime(stamp)),
        None => Value::Null,
    }
}

// ---------------------------------------------------------------------------
// 目录扫描：一致性报告的两块地基（磁盘转录发现、artifact 目录发现）。
// ---------------------------------------------------------------------------

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

/// 路径是否位于 `root` 之内（逐组件比较，避免前缀字符串误判）。
fn is_relative_to(path: &Path, root: &Path) -> bool {
    let mut components = path.components();
    for root_component in root.components() {
        if components.next() != Some(root_component) {
            return false;
        }
    }
    true
}

/// 磁盘上发现的一份会话转录。
#[derive(Debug, Clone, PartialEq)]
pub struct TranscriptLocation {
    pub session_id: String,
    pub relative_path: String,
    pub absolute_path: PathBuf,
}

/// 从文件名取出会话 id：`<会话 id>.jsonl`。
///
/// Python 侧文件名正则带 `IGNORECASE`，但紧接着用会话 id 规则（区分大小写）规范化，
/// 所以大写十六进制的文件名最终会被丢弃；这里保持同样的两步语义。
fn transcript_id_from_file_name(file_name: &str) -> Option<String> {
    let stem_len = file_name.len().checked_sub(".jsonl".len())?;
    if !file_name.is_char_boundary(stem_len) {
        return None;
    }
    if !file_name[stem_len..].eq_ignore_ascii_case(".jsonl") {
        return None;
    }
    normalize_session_id(&file_name[..stem_len]).ok()
}

/// 扫描 `sessions/` 与 `archive/` 下的 JSONL 转录。
///
/// 只认符合会话 id 命名规则的文件名，忽略导出、临时文件与其他非会话数据；目录按
/// `sessions`、`archive` 顺序，目录内按文件名升序，保证结果可复现。
pub fn discover_transcripts(root: &Path) -> Vec<TranscriptLocation> {
    let mut found = Vec::new();
    for relative_dir in ["sessions", "archive"] {
        let directory = root.join(relative_dir);
        if !directory.is_dir() {
            continue;
        }
        let Ok(entries) = fs::read_dir(&directory) else {
            continue;
        };
        let mut names: Vec<String> = entries
            .flatten()
            .filter(|entry| entry.path().is_file())
            .filter_map(|entry| entry.file_name().into_string().ok())
            .collect();
        names.sort();

        for name in names {
            let Some(session_id) = transcript_id_from_file_name(&name) else {
                continue;
            };
            let absolute = directory.join(&name);
            found.push(TranscriptLocation {
                session_id,
                relative_path: format!("{relative_dir}/{name}"),
                // Python 用 Path.resolve()；这里用 canonicalize，Windows 上会带 \\?\ 前缀。
                // 对外契约是「会话 id + 相对路径」，绝对路径不进对照数据集。
                absolute_path: fs::canonicalize(&absolute).unwrap_or(absolute),
            });
        }
    }
    found
}

/// 列出 artifact 目录下看起来像 session_id 的一级子目录（按名字升序）。
pub fn discover_artifact_session_ids(artifacts_dir: &Path) -> Vec<String> {
    if !artifacts_dir.is_dir() {
        return Vec::new();
    }
    let Ok(entries) = fs::read_dir(artifacts_dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter(|entry| entry.path().is_dir())
        .filter_map(|entry| entry.file_name().into_string().ok())
        .collect();
    names.sort();
    names
        .iter()
        .filter_map(|name| normalize_session_id(name).ok())
        .collect()
}

pub const ISSUE_MISSING_TRANSCRIPT: &str = "missing_transcript";
pub const ISSUE_ORPHAN_TRANSCRIPT: &str = "orphan_transcript";
pub const ISSUE_ORPHAN_INDEX: &str = "orphan_index";
pub const ISSUE_ORPHAN_ARTIFACT: &str = "orphan_artifact";
pub const ISSUE_UNREADABLE_TRANSCRIPT: &str = "unreadable_transcript";
pub const ISSUE_EMPTY_TRANSCRIPT: &str = "empty_transcript";
pub const ISSUE_MISSING_WORKSPACE: &str = "missing_workspace_root";
pub const SEVERITY_INFO: &str = "info";

/// 一次一致性扫描或索引重建预览的结果。
#[derive(Debug, Clone, PartialEq)]
pub struct SessionConsistencyReport {
    pub issues: Vec<SessionConsistencyIssue>,
    pub scanned_index_entries: usize,
    pub scanned_transcripts: usize,
    pub scanned_artifact_dirs: usize,
    pub proposed_entries: Vec<SessionIndexEntry>,
    pub applied: bool,
    pub backup_path: Option<String>,
}

impl SessionConsistencyReport {
    /// 没有 `error` 级问题即为「健康」。
    pub fn ok(&self) -> bool {
        !self
            .issues
            .iter()
            .any(|issue| issue.severity == SEVERITY_ERROR)
    }

    pub fn repairable_issues(&self) -> Vec<&SessionConsistencyIssue> {
        self.issues
            .iter()
            .filter(|issue| issue.repairable)
            .collect()
    }

    pub fn to_dict(&self) -> Value {
        let mut object = Map::new();
        object.insert("ok".into(), Value::Bool(self.ok()));
        object.insert("applied".into(), Value::Bool(self.applied));
        object.insert(
            "backup_path".into(),
            match self.backup_path.as_deref() {
                Some(path) => Value::String(path.to_string()),
                None => Value::Null,
            },
        );
        object.insert(
            "scanned_index_entries".into(),
            Value::from(self.scanned_index_entries),
        );
        object.insert(
            "scanned_transcripts".into(),
            Value::from(self.scanned_transcripts),
        );
        object.insert(
            "scanned_artifact_dirs".into(),
            Value::from(self.scanned_artifact_dirs),
        );
        object.insert("issue_count".into(), Value::from(self.issues.len()));
        object.insert(
            "repairable_count".into(),
            Value::from(self.repairable_issues().len()),
        );
        object.insert(
            "issues".into(),
            Value::Array(self.issues.iter().map(|issue| issue.to_dict()).collect()),
        );
        object.insert(
            "proposed_entries".into(),
            Value::Array(
                self.proposed_entries
                    .iter()
                    .map(|entry| entry.to_dict())
                    .collect(),
            ),
        );
        Value::Object(object)
    }

    /// 重建索引落盘后的同一份报告：`applied` 为真并带上备份路径。
    pub fn with_applied(&self, backup_path: Option<String>) -> Self {
        Self {
            issues: self.issues.clone(),
            scanned_index_entries: self.scanned_index_entries,
            scanned_transcripts: self.scanned_transcripts,
            scanned_artifact_dirs: self.scanned_artifact_dirs,
            proposed_entries: self.proposed_entries.clone(),
            applied: true,
            backup_path,
        }
    }
}

fn issue(
    code: &str,
    severity: &str,
    message: String,
    session_id: Option<&str>,
    path: Option<&str>,
    details: Map<String, Value>,
    repairable: bool,
) -> SessionConsistencyIssue {
    SessionConsistencyIssue {
        code: code.to_string(),
        severity: severity.to_string(),
        message,
        session_id: session_id.map(str::to_string),
        path: path.map(str::to_string),
        details,
        repairable,
    }
}

/// 扫描索引、转录与 artifact 目录，生成诊断与建议索引。
///
/// `read_events(path, session_id)` 由调用方注入（复用 `SessionStore` 的路径边界检查）；
/// 读失败会转成 `unreadable_transcript` 问题而不是中断扫描——诊断工具必须能跑完坏目录。
pub fn build_consistency_report<F>(
    root: &Path,
    index_entries: &[SessionIndexEntry],
    read_events: F,
) -> Result<SessionConsistencyReport, SessionStoreError>
where
    F: Fn(&Path, &str) -> Result<Vec<crate::SessionEvent>, SessionStoreError>,
{
    let index_by_id: BTreeMap<String, SessionIndexEntry> = index_entries
        .iter()
        .map(|entry| (entry.session_id.clone(), entry.clone()))
        .collect();
    let transcripts = discover_transcripts(root);
    let mut transcripts_by_id: BTreeMap<String, TranscriptLocation> = BTreeMap::new();
    let mut issues: Vec<SessionConsistencyIssue> = Vec::new();

    for location in &transcripts {
        let Some(previous) = transcripts_by_id.get(&location.session_id).cloned() else {
            transcripts_by_id.insert(location.session_id.clone(), location.clone());
            continue;
        };
        // 同一 id 同时出现在 sessions 与 archive：保留活跃副本，报告冲突但不删任何文件。
        let prefer_new = location.relative_path.starts_with("sessions/");
        let (preferred, other) = if prefer_new {
            (location.clone(), previous.clone())
        } else {
            (previous.clone(), location.clone())
        };
        transcripts_by_id.insert(location.session_id.clone(), preferred.clone());
        let mut details = Map::new();
        details.insert(
            "preferred_path".into(),
            Value::String(preferred.relative_path.clone()),
        );
        details.insert(
            "other_path".into(),
            Value::String(other.relative_path.clone()),
        );
        issues.push(issue(
            ISSUE_PATH_MISMATCH,
            SEVERITY_ERROR,
            format!(
                "会话 {} 同时存在多份转录：{} 与 {}。",
                location.session_id, previous.relative_path, location.relative_path
            ),
            Some(&location.session_id),
            Some(&other.relative_path),
            details,
            false,
        ));
    }

    let mut rebuilt_entries: Vec<SessionIndexEntry> = Vec::new();
    let mut known_session_ids: BTreeSet<String> = BTreeSet::new();

    // 1) 以磁盘转录为权威来源重建可恢复条目。
    for (session_id, location) in &transcripts_by_id {
        known_session_ids.insert(session_id.clone());
        let events = match read_events(&location.absolute_path, session_id) {
            Ok(events) => events,
            Err(error) => {
                issues.push(issue(
                    ISSUE_UNREADABLE_TRANSCRIPT,
                    SEVERITY_ERROR,
                    error.message().to_string(),
                    Some(session_id),
                    Some(&location.relative_path),
                    Map::new(),
                    false,
                ));
                // 读失败时保留原索引条目，避免修复时误删。
                if let Some(current) = index_by_id.get(session_id) {
                    rebuilt_entries.push(current.clone());
                }
                continue;
            }
        };

        if events.is_empty() {
            issues.push(issue(
                ISSUE_EMPTY_TRANSCRIPT,
                SEVERITY_WARNING,
                format!("转录文件无可解析事件：{}", location.relative_path),
                Some(session_id),
                Some(&location.relative_path),
                Map::new(),
                false,
            ));
            if let Some(current) = index_by_id.get(session_id) {
                rebuilt_entries.push(current.clone());
            }
            continue;
        }

        let expected = build_index_entry_from_events(session_id, &location.relative_path, &events)?;
        if expected.workspace_root == UNKNOWN_WORKSPACE {
            issues.push(issue(
                ISSUE_MISSING_WORKSPACE,
                SEVERITY_WARNING,
                format!("转录缺少 session_started.workspace_root：{session_id}"),
                Some(session_id),
                Some(&location.relative_path),
                Map::new(),
                false,
            ));
        }

        match index_by_id.get(session_id) {
            None => {
                let mut details = Map::new();
                details.insert("event_count".into(), Value::from(expected.event_count));
                issues.push(issue(
                    ISSUE_ORPHAN_TRANSCRIPT,
                    SEVERITY_ERROR,
                    format!("发现未进入索引的转录：{}", location.relative_path),
                    Some(session_id),
                    Some(&location.relative_path),
                    details,
                    true,
                ));
            }
            Some(current) => issues.extend(compare_index_entry(current, &expected)),
        }
        rebuilt_entries.push(expected);
    }

    // 2) 索引指向不存在或无法匹配磁盘转录的条目。
    for entry in index_entries {
        if transcripts_by_id.contains_key(&entry.session_id) {
            continue;
        }
        known_session_ids.insert(entry.session_id.clone());
        let absolute = root.join(&entry.path);
        if !is_relative_to(&absolute, root) {
            issues.push(issue(
                ISSUE_ORPHAN_INDEX,
                SEVERITY_ERROR,
                format!("索引路径越界：{}", entry.path),
                Some(&entry.session_id),
                Some(&entry.path),
                Map::new(),
                false,
            ));
            continue;
        }
        if absolute.exists() {
            // 文件在，但不在 sessions/archive 的标准命名扫描结果里。
            issues.push(issue(
                ISSUE_PATH_MISMATCH,
                SEVERITY_ERROR,
                format!("索引路径不在标准会话目录扫描结果中：{}", entry.path),
                Some(&entry.session_id),
                Some(&entry.path),
                Map::new(),
                false,
            ));
            rebuilt_entries.push(entry.clone());
            continue;
        }
        issues.push(issue(
            ISSUE_MISSING_TRANSCRIPT,
            SEVERITY_ERROR,
            format!("索引指向的转录不存在：{}", entry.path),
            Some(&entry.session_id),
            Some(&entry.path),
            Map::new(),
            true,
        ));
        // 缺失转录的条目默认不进 proposed_entries（修复只对齐索引，不动磁盘文件）。
    }

    // 3) 孤立 artifact 目录只报告，不自动清理。
    let artifact_ids = discover_artifact_session_ids(&root.join("artifacts"));
    for artifact_session_id in &artifact_ids {
        if known_session_ids.contains(artifact_session_id)
            || transcripts_by_id.contains_key(artifact_session_id)
            || index_by_id.contains_key(artifact_session_id)
        {
            continue;
        }
        issues.push(issue(
            ISSUE_ORPHAN_ARTIFACT,
            SEVERITY_WARNING,
            format!("发现无对应会话索引/转录的 artifact 目录：{artifact_session_id}"),
            Some(artifact_session_id),
            Some(&format!("artifacts/{artifact_session_id}")),
            Map::new(),
            false,
        ));
    }

    // 与 list_sessions 一致的稳定顺序：按 updated_at 倒序。
    rebuilt_entries.sort_by_key(|entry| std::cmp::Reverse(entry.updated_at));

    Ok(SessionConsistencyReport {
        issues,
        scanned_index_entries: index_entries.len(),
        scanned_transcripts: transcripts.len(),
        scanned_artifact_dirs: artifact_ids.len(),
        proposed_entries: rebuilt_entries,
        applied: false,
        backup_path: None,
    })
}

/// 覆盖 `index.json` 前的同目录备份：文件名带时间戳，撞车时追加序号；索引不存在返回 `None`。
pub fn write_index_backup(
    index_path: &Path,
    now: DateTime<Utc>,
) -> Result<Option<PathBuf>, SessionStoreError> {
    if !index_path.exists() {
        return Ok(None);
    }
    let timestamp = now.format("%Y%m%d_%H%M%S").to_string();
    let file_name = index_path
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| "index.json".to_string());
    let mut backup_path = index_path.with_file_name(format!("{file_name}.bak.{timestamp}"));
    let mut suffix = 1;
    while backup_path.exists() {
        backup_path = index_path.with_file_name(format!("{file_name}.bak.{timestamp}.{suffix}"));
        suffix += 1;
    }
    fs::copy(index_path, &backup_path).map_err(|error| {
        SessionStoreError::new(format!(
            "备份会话索引失败：{}，{error}",
            backup_path.display()
        ))
    })?;
    Ok(Some(backup_path))
}
