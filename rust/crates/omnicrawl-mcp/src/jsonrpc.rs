//! MCP 线上协议的纯函数：`Content-Length` 分帧、JSON-RPC 拆包、SSE 解析、
//! 能力列表分页与结果文本化。
//!
//! 两侧（客户端与本地 Server）共用同一套分帧与拆包逻辑：Python 侧分别是
//! `client.py` 与 `server.py` 各写一份，行为一致但文案不同，这里用 [`FrameKind`] 区分。

use std::fmt;

use omnicrawl_controllers::json::{python_dumps, python_repr};
use serde_json::{Map, Value};

/// 分帧所在方向：决定错误文案里的「请求/响应」用词。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameKind {
    Request,
    Response,
}

impl FrameKind {
    fn label(self) -> &'static str {
        match self {
            Self::Request => "请求",
            Self::Response => "响应",
        }
    }

    fn missing_content_length(self) -> String {
        format!("MCP {}缺少 Content-Length。", self.label())
    }
}

/// 帧头长度上限（Python 侧同一数值）。
pub const MAX_FRAME_HEADER_BYTES: usize = 8192;
/// `Content-Length` 上限（Python 侧同一数值）。
pub const MAX_CONTENT_LENGTH: usize = 50_000_000;
/// 能力列表分页上限：Server 返回重复游标时不至于死循环。
pub const MAX_LIST_PAGES: usize = 20;

/// MCP 连接、能力发现或工具调用失败。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpClientError {
    message: String,
}

impl McpClientError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for McpClientError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for McpClientError {}

/// 传输层失败的类型：与 Python 的「`TimeoutError` / `ValueError` / 其他异常」三分一致，
/// 因为归一化后的错误码（`TOOL_TIMEOUT` / `SCHEMA_INVALID` / `TOOL_FAILED`）跟着它走。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpCallError {
    /// 对应 Python `TimeoutError`。
    Timeout(McpClientError),
    /// 对应 Python `ValueError`。
    Invalid(McpClientError),
    /// 对应其余异常。
    Failed(McpClientError),
}

impl McpCallError {
    pub fn message(&self) -> &str {
        match self {
            Self::Timeout(error) | Self::Invalid(error) | Self::Failed(error) => error.message(),
        }
    }

    /// 归一化后的错误码与可重试标记（与 Python `call_tool` 的分支一致）。
    pub fn code(&self) -> (&'static str, bool) {
        match self {
            Self::Timeout(_) => ("TOOL_TIMEOUT", true),
            Self::Invalid(_) => ("SCHEMA_INVALID", false),
            Self::Failed(_) => ("TOOL_FAILED", true),
        }
    }
}

impl fmt::Display for McpCallError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message())
    }
}

impl From<McpClientError> for McpCallError {
    fn from(error: McpClientError) -> Self {
        Self::Failed(error)
    }
}

/// 编码一帧：`Content-Length: N\r\n\r\n` + JSON 正文（UTF-8）。
pub fn encode_frame(message: &Value) -> Vec<u8> {
    let body = python_dumps_compact(message);
    let mut frame = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
    frame.extend_from_slice(body.as_bytes());
    frame
}

/// 读一帧 JSON-RPC 消息（`Content-Length` 分帧）。
///
/// 客户端方向：读到 EOF 视为错误（附上子进程 stderr 尾部便于诊断）；本地 Server
/// 方向：读到 EOF 视为正常收尾（返回 `None`）。
pub fn read_frame(
    reader: &mut impl std::io::BufRead,
    kind: FrameKind,
    stderr_preview: impl FnOnce() -> String,
) -> Result<Option<Map<String, Value>>, McpClientError> {
    let mut header: Vec<u8> = Vec::new();
    loop {
        let mut line: Vec<u8> = Vec::new();
        let read = reader
            .read_until(b'\n', &mut line)
            .map_err(|error| McpClientError::new(format!("读取 MCP 帧失败：{error}")))?;
        if read == 0 {
            return match kind {
                FrameKind::Request => Ok(None),
                FrameKind::Response => {
                    let stderr = stderr_preview();
                    let suffix = if stderr.is_empty() {
                        String::new()
                    } else {
                        format!(" stderr: {stderr}")
                    };
                    Err(McpClientError::new(format!(
                        "MCP Server 已退出或关闭 stdout。{suffix}"
                    )))
                }
            };
        }
        if line == b"\r\n" || line == b"\n" {
            break;
        }
        header.extend_from_slice(&line);
        if header.len() > MAX_FRAME_HEADER_BYTES {
            return Err(McpClientError::new(format!(
                "MCP {}头超过 {MAX_FRAME_HEADER_BYTES} 字节。",
                kind.label()
            )));
        }
    }

    let content_length = parse_content_length(&header, kind)?;
    let mut body = vec![0u8; content_length];
    reader
        .read_exact(&mut body)
        .map_err(|_| McpClientError::new(format!("MCP {}体长度不完整。", kind.label())))?;

    let text = String::from_utf8_lossy(&body);
    let payload: Value = match serde_json::from_str(&text) {
        Ok(value) => value,
        Err(error) => {
            return Err(McpClientError::new(match kind {
                FrameKind::Response => format!("MCP 响应不是合法 JSON：{error}"),
                FrameKind::Request => "MCP 请求不是合法 JSON。".to_string(),
            }))
        }
    };
    match payload {
        Value::Object(map) => Ok(Some(map)),
        _ => Err(McpClientError::new(format!(
            "MCP {}必须是 JSON 对象。",
            kind.label()
        ))),
    }
}

/// Python `json.dumps(message, ensure_ascii=False, separators=(",", ":"))`。
fn python_dumps_compact(value: &Value) -> String {
    omnicrawl_controllers::json::python_dumps_compact(value)
}

/// 解析帧头里的 `Content-Length`。
pub fn parse_content_length(header: &[u8], kind: FrameKind) -> Result<usize, McpClientError> {
    let text = String::from_utf8_lossy(header);
    for line in text.lines() {
        if let Some((key, value)) = line.split_once(':') {
            if !key.trim().eq_ignore_ascii_case("content-length") {
                continue;
            }
            let length: i128 = value.trim().parse().map_err(|_| {
                // Python 客户端会归一化为 MCPClientError；本地 Server 侧会直接抛错，
                // 这里统一降级为可诊断的协议错误而不是让循环崩掉。
                McpClientError::new("MCP Content-Length 不是整数。")
            })?;
            if length < 0 || length > MAX_CONTENT_LENGTH as i128 {
                return Err(McpClientError::new("MCP Content-Length 超出允许范围。"));
            }
            return Ok(length as usize);
        }
    }
    Err(McpClientError::new(kind.missing_content_length()))
}

/// 拆开 JSON-RPC 响应：错误负载转文本，`result` 必须是对象。
pub fn unwrap_json_rpc_response(
    payload: &Map<String, Value>,
    method: &str,
) -> Result<Map<String, Value>, McpClientError> {
    if payload.contains_key("error") {
        return Err(McpClientError::new(json_rpc_failure_message(
            payload, method,
        )));
    }
    match payload.get("result") {
        // Python 侧对缺失的结果取默认空对象，只有显式非对象才报错。
        None => Ok(Map::new()),
        Some(Value::Object(map)) => Ok(map.clone()),
        Some(_) => Err(McpClientError::new(format!(
            "MCP 请求 {method} 返回结果必须是 JSON 对象。"
        ))),
    }
}

/// JSON-RPC 错误负载 → 文案。客户端两条路径（stdio 与 Streamable HTTP）共用同一句，
/// 与 Python 侧两处重复实现的文案一致。
pub fn json_rpc_failure_message(payload: &Map<String, Value>, method: &str) -> String {
    let error = payload.get("error").unwrap_or(&Value::Null);
    let message = match error {
        Value::Object(map) => match map.get("message") {
            Some(value) if truthy(value) => py_str(value),
            _ => python_dumps(error, 0),
        },
        other => py_str(other),
    };
    format!("MCP 请求 {method} 失败：{message}")
}

/// initialize 响应是否声明了指定能力（缺失声明时保守按支持处理）。
pub fn capability_declared(init_result: &Map<String, Value>, capability: &str) -> bool {
    match init_result.get("capabilities") {
        Some(Value::Object(capabilities)) => capabilities.contains_key(capability),
        _ => true,
    }
}

/// 按 MCP 分页协议完整拉取一个能力列表。
///
/// 只看第一页会让工具表静默缺项；游标为空、重复或超过页数上限时停止，
/// 避免异常 Server 造成死循环。请求失败即停止（Python 侧同样吞掉异常）。
pub fn list_capability_pages<F>(
    mut request: F,
    method: &str,
    result_key: &str,
) -> Vec<Map<String, Value>>
where
    F: FnMut(&str, &Map<String, Value>) -> Result<Map<String, Value>, McpClientError>,
{
    let mut items: Vec<Map<String, Value>> = Vec::new();
    let mut cursor: Option<String> = None;
    let mut seen_cursors: Vec<String> = Vec::new();
    for _ in 0..MAX_LIST_PAGES {
        let params = match cursor.as_deref() {
            Some(text) => {
                let mut params = Map::new();
                params.insert("cursor".to_string(), Value::String(text.to_string()));
                params
            }
            None => Map::new(),
        };
        let Ok(payload) = request(method, &params) else {
            break;
        };
        if let Some(Value::Array(values)) = payload.get(result_key) {
            items.extend(values.iter().filter_map(|value| match value {
                Value::Object(map) => Some(map.clone()),
                _ => None,
            }));
        }
        match payload.get("nextCursor") {
            Some(Value::String(next)) if !next.is_empty() && !seen_cursors.contains(next) => {
                seen_cursors.push(next.clone());
                cursor = Some(next.clone());
            }
            _ => break,
        }
    }
    items
}

/// 解析 Streamable HTTP 的 SSE data 事件，忽略非 JSON 事件。
pub fn parse_sse_json_payloads(text: &str) -> Vec<Map<String, Value>> {
    let mut payloads: Vec<Map<String, Value>> = Vec::new();
    let mut data_lines: Vec<String> = Vec::new();
    let mut lines: Vec<&str> = text.lines().collect();
    lines.push("");
    for line in lines {
        if let Some(rest) = line.strip_prefix("data:") {
            data_lines.push(rest.trim_start().to_string());
            continue;
        }
        if !line.trim().is_empty() || data_lines.is_empty() {
            continue;
        }
        let raw_data = data_lines.join("\n").trim().to_string();
        data_lines.clear();
        if raw_data.is_empty() {
            continue;
        }
        if let Ok(Value::Object(payload)) = serde_json::from_str::<Value>(&raw_data) {
            payloads.push(payload);
        }
    }
    payloads
}

/// MCP Tool 结果 → 文本（Python `_stringify_tool_result_payload`）。
pub fn stringify_tool_result_payload(payload: &Map<String, Value>) -> String {
    if let Some(Value::Array(content)) = payload.get("content") {
        let mut parts: Vec<String> = Vec::new();
        for item in content {
            let Value::Object(item) = item else {
                continue;
            };
            let item_type = item.get("type");
            if matches!(item_type, Some(Value::String(text)) if text == "text") {
                if let Some(Value::String(text)) = item.get("text") {
                    parts.push(text.clone());
                    continue;
                }
            }
            if item_type.map(truthy).unwrap_or(false) {
                parts.push(python_dumps(&Value::Object(item.clone()), 0));
            }
        }
        if !parts.is_empty() {
            return parts.join("\n");
        }
    }
    python_dumps(&Value::Object(payload.clone()), 2)
}

/// MCP Resource 结果 → 文本（Python `_stringify_resource_payload`）。
pub fn stringify_resource_payload(payload: &Map<String, Value>) -> String {
    if let Some(Value::Array(contents)) = payload.get("contents") {
        let mut parts: Vec<String> = Vec::new();
        for item in contents {
            let Value::Object(item) = item else {
                continue;
            };
            let uri = item.get("uri");
            let title = match uri {
                Some(Value::String(text)) => format!("Resource {text}:"),
                _ => "Resource:".to_string(),
            };
            if let Some(Value::String(text)) = item.get("text") {
                parts.push(format!("{title}\n{text}"));
            } else if let Some(Value::String(blob)) = item.get("blob") {
                parts.push(format!(
                    "{title}\n<base64 blob，字符数 {}>",
                    blob.chars().count()
                ));
            }
        }
        if !parts.is_empty() {
            return parts.join("\n\n");
        }
    }
    python_dumps(&Value::Object(payload.clone()), 2)
}

/// MCP Prompt 结果 → 文本（Python `_stringify_prompt_payload`）。
pub fn stringify_prompt_payload(payload: &Map<String, Value>) -> String {
    if let Some(Value::Array(messages)) = payload.get("messages") {
        let mut parts: Vec<String> = Vec::new();
        for item in messages {
            let Value::Object(item) = item else {
                continue;
            };
            let role = match item.get("role") {
                Some(value) => py_str(value),
                None => "unknown".to_string(),
            };
            match item.get("content") {
                Some(Value::Object(content)) => match content.get("text") {
                    Some(Value::String(text)) => parts.push(format!("{role}: {text}")),
                    _ => parts.push(format!(
                        "{role}: {}",
                        python_dumps(&Value::Object(content.clone()), 0)
                    )),
                },
                Some(other) => parts.push(format!("{role}: {}", py_str(other))),
                None => parts.push(format!("{role}: None")),
            }
        }
        if !parts.is_empty() {
            return parts.join("\n");
        }
    }
    python_dumps(&Value::Object(payload.clone()), 2)
}

/// Python truthiness（能力条目里只会出现字符串与非空对象）。
fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64().map(|value| value != 0.0).unwrap_or(false),
        Value::String(text) => !text.is_empty(),
        Value::Array(items) => !items.is_empty(),
        Value::Object(map) => !map.is_empty(),
    }
}

/// Python `str(value)` 的可用子集。
fn py_str(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => python_repr(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn frame_encoding_matches_content_length_framing() {
        let message = json!({"jsonrpc": "2.0", "id": 1});
        let frame = encode_frame(&message);
        let text = String::from_utf8(frame).expect("UTF-8");
        let body = r#"{"jsonrpc":"2.0","id":1}"#;
        assert_eq!(
            text,
            format!("Content-Length: {}\r\n\r\n{body}", body.len())
        );
    }

    #[test]
    fn content_length_errors_are_directional() {
        let missing = parse_content_length(b"X: 1\r\n", FrameKind::Request).unwrap_err();
        assert_eq!(missing.message(), "MCP 请求缺少 Content-Length。");
        let missing = parse_content_length(b"X: 1\r\n", FrameKind::Response).unwrap_err();
        assert_eq!(missing.message(), "MCP 响应缺少 Content-Length。");
        let range =
            parse_content_length(b"Content-Length: 50000001\r\n", FrameKind::Request).unwrap_err();
        assert_eq!(range.message(), "MCP Content-Length 超出允许范围。");
    }

    #[test]
    fn json_rpc_error_payload_becomes_message() {
        let payload =
            json!({"jsonrpc": "2.0", "id": 1, "error": {"code": -32601, "message": "未知"}});
        let error =
            unwrap_json_rpc_response(payload.as_object().expect("对象"), "tools/call").unwrap_err();
        assert_eq!(error.message(), "MCP 请求 tools/call 失败：未知");
    }

    #[test]
    fn pagination_stops_on_repeated_cursor() {
        let mut cursors: Vec<bool> = Vec::new();
        let items = list_capability_pages(
            |_method, params| {
                let has_cursor = params.contains_key("cursor");
                cursors.push(has_cursor);
                Ok(json!({"tools": [{"name": "t"}], "nextCursor": "same"})
                    .as_object()
                    .cloned()
                    .unwrap_or_default())
            },
            "tools/list",
            "tools",
        );
        assert_eq!(items.len(), 2, "重复游标应当停止：{items:?}");
        assert_eq!(cursors, vec![false, true]);
    }

    #[test]
    fn sse_parsing_keeps_json_events_only() {
        let text =
            "event: message\ndata: {\"a\": 1}\n\ndata: not json\n\ndata: {\"b\":\ndata: 2}\n\n";
        let payloads = parse_sse_json_payloads(text);
        assert_eq!(payloads.len(), 2);
        assert_eq!(payloads[0]["a"], json!(1));
        assert_eq!(payloads[1]["b"], json!(2));
    }

    #[test]
    fn tool_result_text_prefers_text_blocks() {
        let payload =
            json!({"content": [{"type": "text", "text": "hi"}, {"type": "image", "data": "x"}]});
        assert_eq!(
            stringify_tool_result_payload(payload.as_object().expect("对象")),
            "hi\n{\"type\": \"image\", \"data\": \"x\"}"
        );
    }

    #[test]
    fn empty_result_falls_back_to_indented_json() {
        let payload = json!({"content": []});
        assert_eq!(
            stringify_tool_result_payload(payload.as_object().expect("对象")),
            "{\n  \"content\": []\n}"
        );
    }
}
