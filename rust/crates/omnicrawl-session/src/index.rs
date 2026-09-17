//! `.agent_sessions/index.json` 的索引条目模型。
//!
//! 索引是会话目录的目录页：转录文件损坏时靠它恢复，`/sessions` 列表与恢复入口也读它，
//! 所以字段校验与错误文案必须与 Python `SessionIndexEntry` 完全一致。

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;

use crate::error::SessionStoreError;
use crate::naming::{path_from_json, session_id_from_json};
use crate::time::{datetime_from_json, format_datetime};

const REQUIRED_FIELDS: [&str; 6] = [
    "session_id",
    "title",
    "workspace_root",
    "path",
    "created_at",
    "updated_at",
];

#[derive(Debug, Clone, PartialEq)]
pub struct SessionIndexEntry {
    pub session_id: String,
    pub title: String,
    pub workspace_root: String,
    pub path: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub event_count: u64,
    pub message_count: u64,
    pub last_event_type: String,
    pub archived_at: Option<DateTime<Utc>>,
}

impl SessionIndexEntry {
    pub fn from_dict(data: &Value) -> Result<Self, SessionStoreError> {
        let Some(object) = data.as_object() else {
            return Err(missing_field(REQUIRED_FIELDS[0]));
        };
        for field in REQUIRED_FIELDS {
            if !object.contains_key(field) {
                return Err(missing_field(field));
            }
        }

        let title = match object.get("title").expect("已检查字段存在") {
            Value::String(title) => title.trim().to_string(),
            _ => return Err(SessionStoreError::new("会话索引 title 必须是字符串。")),
        };
        let workspace_root = match object.get("workspace_root").expect("已检查字段存在") {
            Value::String(root) if !root.trim().is_empty() => root.trim().to_string(),
            _ => {
                return Err(SessionStoreError::new(
                    "会话索引 workspace_root 必须是非空字符串。",
                ))
            }
        };
        let event_count = read_count(object.get("event_count"), "event_count")?;
        let message_count = read_count(object.get("message_count"), "message_count")?;
        let last_event_type = match object.get("last_event_type") {
            None => String::new(),
            Some(Value::String(value)) => value.trim().to_string(),
            Some(_) => {
                return Err(SessionStoreError::new(
                    "会话索引 last_event_type 必须是字符串。",
                ))
            }
        };
        let archived_at = match object.get("archived_at") {
            None | Some(Value::Null) => None,
            Some(value) => Some(datetime_from_json(value)?),
        };

        Ok(Self {
            session_id: session_id_from_json(object.get("session_id").expect("已检查字段存在"))?,
            title,
            workspace_root,
            path: path_from_json(object.get("path").expect("已检查字段存在"))?,
            created_at: datetime_from_json(object.get("created_at").expect("已检查字段存在"))?,
            updated_at: datetime_from_json(object.get("updated_at").expect("已检查字段存在"))?,
            event_count,
            message_count,
            last_event_type,
            archived_at,
        })
    }

    pub fn to_dict(&self) -> Value {
        serde_json::to_value(self.wire()).expect("索引字段均可序列化")
    }

    pub fn to_json_line(&self) -> String {
        serde_json::to_string(&self.wire()).expect("索引字段均可序列化")
    }

    fn wire(&self) -> IndexWire<'_> {
        IndexWire {
            session_id: &self.session_id,
            title: &self.title,
            workspace_root: &self.workspace_root,
            path: &self.path,
            created_at: format_datetime(self.created_at),
            updated_at: format_datetime(self.updated_at),
            event_count: self.event_count,
            message_count: self.message_count,
            last_event_type: &self.last_event_type,
            archived_at: self.archived_at.map(format_datetime),
        }
    }
}

#[derive(Serialize)]
struct IndexWire<'a> {
    session_id: &'a str,
    title: &'a str,
    workspace_root: &'a str,
    path: &'a str,
    created_at: String,
    updated_at: String,
    event_count: u64,
    message_count: u64,
    last_event_type: &'a str,
    archived_at: Option<String>,
}

fn read_count(value: Option<&Value>, name: &str) -> Result<u64, SessionStoreError> {
    match value {
        None => Ok(0),
        Some(value) if !value.is_boolean() => value.as_u64().ok_or_else(|| invalid_count(name)),
        Some(_) => Err(invalid_count(name)),
    }
}

fn invalid_count(name: &str) -> SessionStoreError {
    SessionStoreError::new(format!("会话索引 {name} 必须是非负整数。"))
}

fn missing_field(field: &str) -> SessionStoreError {
    SessionStoreError::new(format!("会话索引缺少字段：{field}。"))
}
