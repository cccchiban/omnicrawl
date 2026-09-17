//! NDJSON 帧层：一行一个 JSON-RPC 2.0 对象。

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const JSONRPC_VERSION: &str = "2.0";

/// 请求与响应的关联标识；整数与字符串都合法，缺失即通知。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Id {
    Number(i64),
    Text(String),
}

impl From<i64> for Id {
    fn from(value: i64) -> Self {
        Self::Number(value)
    }
}

impl From<String> for Id {
    fn from(value: String) -> Self {
        Self::Text(value)
    }
}

impl From<&str> for Id {
    fn from(value: &str) -> Self {
        Self::Text(value.to_string())
    }
}

/// JSON-RPC 错误对象。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ErrorObject {
    pub code: i64,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl ErrorObject {
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }

    pub fn with_data(code: i64, message: impl Into<String>, data: Value) -> Self {
        Self {
            code,
            message: message.into(),
            data: Some(data),
        }
    }
}

/// 协议错误码；前五个沿用 JSON-RPC 2.0 标准码，其余为本协议的应用码。
pub mod error_code {
    pub const PARSE_ERROR: i64 = -32700;
    pub const INVALID_REQUEST: i64 = -32600;
    pub const METHOD_NOT_FOUND: i64 = -32601;
    pub const INVALID_PARAMS: i64 = -32602;
    pub const INTERNAL_ERROR: i64 = -32603;
    pub const UNSUPPORTED_PROTOCOL_VERSION: i64 = -32001;
    pub const TURN_BUSY: i64 = -32002;
    /// 回合被宿主取消。
    pub const TURN_CANCELLED: i64 = -32003;
    /// 回合失败（预算超限、模型回复来源失败、工具批次失败等）。
    pub const TURN_FAILED: i64 = -32004;
}

/// 帧格式错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameError {
    /// 空行或只有空白。
    Empty,
    /// 不是合法 JSON，或字段类型不符。
    Json(String),
    /// JSON 合法，但不是本协议允许的帧形状。
    Invalid(&'static str),
}

impl std::fmt::Display for FrameError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => write!(formatter, "空行不是合法帧。"),
            Self::Json(detail) => write!(formatter, "帧不是合法 JSON：{detail}"),
            Self::Invalid(reason) => write!(formatter, "帧形状非法：{reason}"),
        }
    }
}

impl std::error::Error for FrameError {}

/// 一个 NDJSON 帧。
///
/// 用单一结构而不是枚举：JSON-RPC 的三种形状共享 `id` 与 `jsonrpc`，枚举要在反序列化后
/// 二次校验，错误信息也更含糊；这里把形状校验集中在 `validate`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Frame {
    pub jsonrpc: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<Id>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorObject>,
}

impl Frame {
    /// 请求：带 `id`、等对方回响应。
    pub fn request(id: impl Into<Id>, method: impl Into<String>, params: Value) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id: Some(id.into()),
            method: Some(method.into()),
            params: Some(params),
            result: None,
            error: None,
        }
    }

    /// 通知：不带 `id`，对方不需要回响应。
    pub fn notification(method: impl Into<String>, params: Value) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id: None,
            method: Some(method.into()),
            params: Some(params),
            result: None,
            error: None,
        }
    }

    /// 成功响应。
    pub fn response(id: impl Into<Id>, result: Value) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id: Some(id.into()),
            method: None,
            params: None,
            result: Some(result),
            error: None,
        }
    }

    /// 失败响应。
    pub fn error_response(id: impl Into<Id>, error: ErrorObject) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id: Some(id.into()),
            method: None,
            params: None,
            result: None,
            error: Some(error),
        }
    }

    /// 解析一行文本。
    pub fn parse(line: &str) -> Result<Self, FrameError> {
        if line.trim().is_empty() {
            return Err(FrameError::Empty);
        }
        let frame: Self =
            serde_json::from_str(line).map_err(|error| FrameError::Json(error.to_string()))?;
        frame.validate()?;
        Ok(frame)
    }

    /// 序列化成一行（不含换行符）。
    ///
    /// JSON 字符串里的换行会被转义，因此结果必然是单行；宿主写回时自行补 `\n`。
    pub fn to_line(&self) -> String {
        serde_json::to_string(self).expect("帧只含标题字段与 JSON 负载，必须可序列化")
    }

    pub fn id(&self) -> Option<&Id> {
        self.id.as_ref()
    }

    pub fn method(&self) -> Option<&str> {
        self.method.as_deref()
    }

    pub fn is_request(&self) -> bool {
        self.method.is_some() && self.id.is_some()
    }

    pub fn is_notification(&self) -> bool {
        self.method.is_some() && self.id.is_none()
    }

    pub fn is_response(&self) -> bool {
        self.method.is_none() && self.id.is_some()
    }

    fn validate(&self) -> Result<(), FrameError> {
        if self.jsonrpc != JSONRPC_VERSION {
            return Err(FrameError::Invalid("jsonrpc 字段必须是 \"2.0\""));
        }
        if self.method.is_some() {
            if self.result.is_some() || self.error.is_some() {
                return Err(FrameError::Invalid("请求/通知不得携带 result 或 error"));
            }
            return Ok(());
        }
        if self.id.is_none() {
            return Err(FrameError::Invalid("响应必须携带 id"));
        }
        match (self.result.is_some(), self.error.is_some()) {
            (true, false) | (false, true) => Ok(()),
            _ => Err(FrameError::Invalid(
                "响应必须且只能携带 result 或 error 之一",
            )),
        }
    }
}
