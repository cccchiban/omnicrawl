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
