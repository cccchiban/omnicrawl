//! 会话 JSONL 事件：模型、校验与行编解码。
//!
//! 事件是会话转录的最小单位，也是两个实现之间唯一的契约面：字段名、`created_at` 写法、
//! 错误文案都与 Python `SessionEvent` 一致；`to_json_line` 的输出就是转录里的一行
//! （紧凑分隔符、不转义非 ASCII），与 Python 的 `json.dumps(..., ensure_ascii=False, separators=(",", ":"))` 同形。

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::{Map, Value};

use crate::error::SessionStoreError;
use crate::naming::{
    event_type_from_json, normalize_event_type, normalize_session_id, random_event_id,
    session_id_from_json, SESSION_EVENT_VERSION,
};
use crate::time::{datetime_from_json, format_datetime, truncate_to_micros};

const REQUIRED_FIELDS: [&str; 5] = ["version", "session_id", "event_id", "type", "created_at"];

#[derive(Debug, Clone, PartialEq)]
pub struct SessionEvent {
    pub version: u32,
    pub session_id: String,
    pub event_id: String,
    pub parent_id: Option<String>,
    pub event_type: String,
    pub created_at: DateTime<Utc>,
    pub payload: Map<String, Value>,
}

impl SessionEvent {
    pub fn create(
        session_id: &str,
        event_type: &str,
        payload: Map<String, Value>,
        parent_id: Option<&str>,
        now: DateTime<Utc>,
    ) -> Result<Self, SessionStoreError> {
        Ok(Self {
            version: SESSION_EVENT_VERSION,
            session_id: normalize_session_id(session_id)?,
            event_id: random_event_id(),
            parent_id: clean_parent(parent_id),
            event_type: normalize_event_type(event_type)?,
            created_at: truncate_to_micros(now.with_timezone(&Utc)),
            payload,
        })
    }

    pub fn from_dict(data: &Value) -> Result<Self, SessionStoreError> {
        let Some(object) = data.as_object() else {
            // Python 在这里会抛 TypeError；内核统一收敛成会话错误，文案按「缺字段」给出。
            return Err(missing_field(REQUIRED_FIELDS[0]));
        };
        for field in REQUIRED_FIELDS {
            if !object.contains_key(field) {
                return Err(missing_field(field));
            }
        }

        let version = object.get("version").expect("已检查字段存在");
        if version.as_u64() != Some(u64::from(SESSION_EVENT_VERSION)) {
            return Err(SessionStoreError::new(format!(
                "暂不支持的会话事件版本：{}。",
                scalar_text(version)
            )));
        }

        let event_id = match object.get("event_id").and_then(Value::as_str) {
            Some(text) if !text.trim().is_empty() => text.trim().to_string(),
            _ => {
                return Err(SessionStoreError::new(
                    "会话事件 event_id 必须是非空字符串。",
                ))
            }
        };

        let parent_id = match object.get("parent_id") {
            None | Some(Value::Null) => None,
            Some(Value::String(text)) => clean_parent(Some(text)),
            Some(_) => {
                return Err(SessionStoreError::new(
                    "会话事件 parent_id 必须是字符串或 null。",
                ))
            }
        };

        let payload = match object.get("payload") {
            None => Map::new(),
            Some(Value::Object(payload)) => payload.clone(),
            Some(_) => {
                return Err(SessionStoreError::new(
                    "会话事件 payload 必须是 JSON 对象。",
                ))
            }
        };

        Ok(Self {
            version: SESSION_EVENT_VERSION,
            session_id: session_id_from_json(object.get("session_id").expect("已检查字段存在"))?,
            event_id,
            parent_id,
            event_type: event_type_from_json(object.get("type").expect("已检查字段存在"))?,
            created_at: datetime_from_json(object.get("created_at").expect("已检查字段存在"))?,
            payload,
        })
    }

    /// 事件对象（字段名与 Python `to_dict()` 一致；对象内键序不参与语义）。
    pub fn to_dict(&self) -> Value {
        serde_json::to_value(self.wire()).expect("会话事件字段均可序列化")
    }

    /// 转录里的一行（不含换行符），字节布局与 Python 侧一致。
    pub fn to_json_line(&self) -> String {
        serde_json::to_string(&self.wire()).expect("会话事件字段均可序列化")
    }

    fn wire(&self) -> EventWire<'_> {
        EventWire {
            version: self.version,
            session_id: &self.session_id,
            event_id: &self.event_id,
            parent_id: self.parent_id.as_deref(),
            event_type: &self.event_type,
            created_at: format_datetime(self.created_at),
            payload: &self.payload,
        }
    }
}

/// 固定字段顺序：Python 的 dict 保留插入序，转录行因此有稳定布局，这里用结构体复刻。
#[derive(Serialize)]
struct EventWire<'a> {
    version: u32,
    session_id: &'a str,
    event_id: &'a str,
    parent_id: Option<&'a str>,
    #[serde(rename = "type")]
    event_type: &'a str,
    created_at: String,
    payload: &'a Map<String, Value>,
}

fn clean_parent(parent_id: Option<&str>) -> Option<String> {
    parent_id
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

fn missing_field(field: &str) -> SessionStoreError {
    SessionStoreError::new(format!("会话事件缺少字段：{field}。"))
}

/// Python 是 f-string 插值：字符串原样、其余按字面量。
fn scalar_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}
