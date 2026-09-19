//! 会话 JSONL 与提示历史的解码、版本迁移和损坏诊断。
//!
//! 对齐 Python `omnicrawl/state/session_records.py`：按版本分发解码，把可识别的旧格式纯函数
//! 迁移到当前模型；未知版本、JSON 损坏、字段错误产生结构化诊断，不再静默吞掉；默认**不改写**
//! 磁盘上的原始转录或 `history.jsonl`。
//!
//! 两处已知差异（crate README 有记录）：`json_error` 明细是各自 JSON 库的错误文本，不参与逐字
//! 比对；大转录的只读内存映射快路径未搬，一律整份读取后按 Python `splitlines()` 的规则切行。

use std::fs;
use std::path::Path;

use serde_json::{json, Map, Value};

use crate::error::SessionStoreError;
use crate::event::SessionEvent;
use crate::naming::SESSION_EVENT_VERSION;
use crate::redaction::redact_sensitive_text;

/// 诊断码保持稳定，便于 API/TUI 与测试断言。
pub const DIAG_INVALID_JSON: &str = "invalid_json";
pub const DIAG_NOT_OBJECT: &str = "not_object";
pub const DIAG_INVALID_FIELDS: &str = "invalid_fields";
pub const DIAG_UNSUPPORTED_VERSION: &str = "unsupported_event_version";
pub const DIAG_SESSION_ID_MISMATCH: &str = "session_id_mismatch";
pub const DIAG_TRAILING_INCOMPLETE: &str = "trailing_incomplete";
pub const DIAG_LEGACY_MIGRATED: &str = "legacy_event_migrated";
pub const DIAG_PROMPT_INVALID: &str = "invalid_prompt_history";

/// 严重级沿用一致性诊断的同一套取值（`consistency.rs`）。
pub use crate::consistency::{SEVERITY_ERROR, SEVERITY_WARNING};
pub const SEVERITY_INFO: &str = "info";

/// 转录达到这个体量后逐行读取；内核这一片一律整份读取（见模块头差异说明）。
pub const TRANSCRIPT_MMAP_THRESHOLD_BYTES: usize = 1 << 20;

/// 索引 schema 版本与事件版本独立演进。
pub const SESSION_INDEX_SCHEMA_VERSION: u64 = 1;
pub const SUPPORTED_EVENT_VERSIONS: [u64; 2] = [0, 1];

/// 单条 JSONL 记录的结构化诊断。
#[derive(Debug, Clone, PartialEq)]
pub struct SessionRecordDiagnostic {
    pub code: String,
    pub severity: String,
    pub message: String,
    pub path: Option<String>,
    pub line_no: Option<usize>,
    pub session_id: Option<String>,
    pub recoverable: bool,
    pub details: Map<String, Value>,
}

impl SessionRecordDiagnostic {
    fn new(code: &str, severity: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.to_string(),
            severity: severity.to_string(),
            message: message.into(),
            path: None,
            line_no: None,
            session_id: None,
            recoverable: true,
            details: Map::new(),
        }
    }

    fn with_path(mut self, path: Option<&str>, line_no: Option<usize>) -> Self {
        self.path = path.map(str::to_string);
        self.line_no = line_no;
        self
    }

    fn with_session(mut self, session_id: Option<String>) -> Self {
        self.session_id = session_id;
        self
    }

    fn with_details(mut self, details: Value) -> Self {
        self.details = match details {
            Value::Object(map) => map,
            _ => Map::new(),
        };
        self
    }

    pub fn to_dict(&self) -> Value {
        json!({
            "code": self.code,
            "severity": self.severity,
            "message": self.message,
            "path": self.path,
            "line_no": self.line_no,
            "session_id": self.session_id,
            "recoverable": self.recoverable,
            "details": Value::Object(self.details.clone()),
        })
    }
}

/// 一次转录读取结果：有效事件 + 诊断。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SessionEventReadResult {
    pub events: Vec<SessionEvent>,
    pub diagnostics: Vec<SessionRecordDiagnostic>,
}

impl SessionEventReadResult {
    pub fn has_errors(&self) -> bool {
        self.diagnostics
            .iter()
            .any(|item| item.severity == SEVERITY_ERROR)
    }

    pub fn to_dict(&self) -> Value {
        json!({
            "event_count": self.events.len(),
            "diagnostic_count": self.diagnostics.len(),
            "has_errors": self.has_errors(),
            "diagnostics": self.diagnostics.iter().map(SessionRecordDiagnostic::to_dict).collect::<Vec<Value>>(),
            "events": self.events.iter().map(SessionEvent::to_dict).collect::<Vec<Value>>(),
        })
    }
}

/// 把可识别的旧事件字典迁移为当前版本字段。
///
/// 返回 `(迁移后的字典, 迁移标记)`；标记为 `None` 表示已是当前版本。非 `None` 表示发生了
/// 纯内存迁移，磁盘文件保持不变。
///
/// 支持范围：`version == 1`（当前格式）、`version == 0`（原型格式，字段与 v1 相同仅版本号不同）、
/// 以及缺失 `version` 但具备 v1 核心字段的遗留格式。
pub fn migrate_event_dict(data: &Value) -> Result<(Value, Option<String>), SessionStoreError> {
    let Some(object) = data.as_object() else {
        return Err(SessionStoreError::new("会话事件必须是 JSON 对象。"));
    };

    let raw_version = object.get("version");
    match raw_version {
        None => migrate_unversioned(object),
        Some(Value::Number(number))
            if number.as_u64() == Some(u64::from(SESSION_EVENT_VERSION)) =>
        {
            Ok((data.clone(), None))
        }
        Some(Value::Number(number)) if number.as_u64() == Some(0) => {
            let mut migrated = object.clone();
            migrated.insert("version".to_string(), Value::from(SESSION_EVENT_VERSION));
            // 原型格式曾用 id 作为事件主键；兼容读入后统一为 event_id。
            if !migrated.contains_key("event_id")
                && matches!(migrated.get("id"), Some(Value::String(_)))
            {
                if let Some(value) = migrated.remove("id") {
                    migrated.insert("event_id".to_string(), value);
                }
            }
            Ok((Value::Object(migrated), Some("v0".to_string())))
        }
        Some(other) => Err(SessionStoreError::new(format!(
            "暂不支持的会话事件版本：{}。",
            scalar_text(other)
        ))),
    }
}

fn migrate_unversioned(
    object: &Map<String, Value>,
) -> Result<(Value, Option<String>), SessionStoreError> {
    let required = ["session_id", "event_id", "type", "created_at"];
    if required.iter().all(|key| object.contains_key(*key)) {
        let mut migrated = object.clone();
        migrated.insert("version".to_string(), Value::from(SESSION_EVENT_VERSION));
        return Ok((Value::Object(migrated), Some("unversioned".to_string())));
    }
    Err(SessionStoreError::new(
        "会话事件缺少 version，且不具备可迁移的遗留字段。",
    ))
}

/// 解码单个事件对象，返回事件或诊断。
pub fn decode_session_event_dict(
    data: &Value,
    path: Option<&str>,
    line_no: Option<usize>,
    expected_session_id: Option<&str>,
) -> (Option<SessionEvent>, Vec<SessionRecordDiagnostic>) {
    if !data.is_object() {
        let diagnostic = SessionRecordDiagnostic::new(
            DIAG_NOT_OBJECT,
            SEVERITY_ERROR,
            "会话事件 JSON 顶层必须是对象。",
        )
        .with_path(path, line_no);
        return (None, vec![diagnostic]);
    }

    let (migrated, migration_tag) = match migrate_event_dict(data) {
        Ok(value) => value,
        Err(error) => {
            let message = error.to_string();
            let code = if message.contains("暂不支持的会话事件版本") {
                DIAG_UNSUPPORTED_VERSION
            } else {
                DIAG_INVALID_FIELDS
            };
            let diagnostic = SessionRecordDiagnostic::new(code, SEVERITY_ERROR, message)
                .with_path(path, line_no)
                .with_session(safe_session_id(data))
                .with_details(
                    json!({"raw_version": data.get("version").cloned().unwrap_or(Value::Null)}),
                );
            return (None, vec![diagnostic]);
        }
    };

    let event = match SessionEvent::from_dict(&migrated) {
        Ok(event) => event,
        Err(error) => {
            let message = error.to_string();
            let code = if message.contains("暂不支持的会话事件版本") {
                DIAG_UNSUPPORTED_VERSION
            } else {
                DIAG_INVALID_FIELDS
            };
            let diagnostic = SessionRecordDiagnostic::new(code, SEVERITY_ERROR, message)
                .with_path(path, line_no)
                .with_session(safe_session_id(data))
                .with_details(
                    json!({"raw_version": data.get("version").cloned().unwrap_or(Value::Null)}),
                );
            return (None, vec![diagnostic]);
        }
    };

    let mut diagnostics: Vec<SessionRecordDiagnostic> = Vec::new();
    if let Some(tag) = migration_tag {
        diagnostics.push(
            SessionRecordDiagnostic::new(
                DIAG_LEGACY_MIGRATED,
                SEVERITY_INFO,
                format!("已将遗留事件格式迁移到 version={SESSION_EVENT_VERSION}（仅内存，未改写磁盘）。"),
            )
            .with_path(path, line_no)
            .with_session(Some(event.session_id.clone()))
            .with_details(json!({
                "migration": tag,
                "from_version": data.get("version").cloned().unwrap_or(Value::Null),
            })),
        );
    }

    if let Some(expected) = expected_session_id {
        if event.session_id != expected {
            diagnostics.push(
                SessionRecordDiagnostic::new(
                    DIAG_SESSION_ID_MISMATCH,
                    SEVERITY_WARNING,
                    format!(
                        "事件 session_id 与转录归属不一致：事件为 {}，期望为 {expected}。",
                        event.session_id
                    ),
                )
                .with_path(path, line_no)
                .with_session(Some(event.session_id.clone()))
                .with_details(json!({
                    "event_session_id": event.session_id,
                    "expected_session_id": expected,
                })),
            );
            return (None, diagnostics);
        }
    }

    (Some(event), diagnostics)
}

/// 解码 JSONL 单行。
pub fn decode_session_event_line(
    line: &str,
    path: Option<&str>,
    line_no: Option<usize>,
    expected_session_id: Option<&str>,
    is_last_nonempty_line: bool,
    file_ends_with_newline: bool,
) -> (Option<SessionEvent>, Vec<SessionRecordDiagnostic>) {
    let stripped = line.trim();
    if stripped.is_empty() {
        return (None, Vec::new());
    }

    let data: Value = match serde_json::from_str(line) {
        Ok(value) => value,
        Err(error) => {
            // 尾部半行通常来自崩溃中断写入；中间损坏更可能是真实损坏。
            let trailing = is_last_nonempty_line && !file_ends_with_newline;
            let (code, severity) = if trailing {
                (DIAG_TRAILING_INCOMPLETE, SEVERITY_WARNING)
            } else {
                (DIAG_INVALID_JSON, SEVERITY_ERROR)
            };
            let message = if trailing {
                "转录末尾存在未完成的 JSON 行，可能由写入中断导致。"
            } else {
                "会话转录 JSON 损坏：第 {line} 行。"
            };
            let message = message.replace("{line}", &line_no.unwrap_or_default().to_string());
            let snippet = redact_sensitive_text(&truncate_chars(stripped, 120));
            let diagnostic = SessionRecordDiagnostic::new(code, severity, message)
                .with_path(path, line_no)
                .with_session(expected_session_id.map(str::to_string))
                .with_details(json!({
                    "json_error": error.to_string(),
                    "snippet": snippet,
                    "trailing": trailing,
                }));
            return (None, vec![diagnostic]);
        }
    };

    decode_session_event_dict(&data, path, line_no, expected_session_id)
}

/// 读取转录文件并收集诊断；坏行不阻断其余有效事件。
pub fn read_session_events_with_diagnostics(
    path: &Path,
    session_id: Option<&str>,
    relative_path: Option<&str>,
) -> Result<SessionEventReadResult, SessionStoreError> {
    if !path.exists() {
        return Ok(SessionEventReadResult::default());
    }

    let display_path = relative_path
        .map(str::to_string)
        .unwrap_or_else(|| path.to_string_lossy().to_string());
    let text = fs::read(path).map_err(|error| {
        SessionStoreError::new(format!("读取会话转录失败：{}，{error}", path.display()))
    })?;
    let text = String::from_utf8(text).map_err(|_| {
        SessionStoreError::new(format!("会话转录不是 UTF-8 文本：{}", path.display()))
    })?;

    let file_ends_with_newline = text.ends_with('\n') || text.is_empty();
    let mut events: Vec<SessionEvent> = Vec::new();
    let mut diagnostics: Vec<SessionRecordDiagnostic> = Vec::new();

    // 末行是否「最后一个非空行」要读到下一行才知道，因此延后一拍处理。
    let mut pending: Option<(usize, String)> = None;
    for (index, line) in split_lines_python(&text).into_iter().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        if let Some((pending_index, pending_line)) = pending.take() {
            decode_into(
                &pending_line,
                pending_index,
                false,
                &display_path,
                session_id,
                file_ends_with_newline,
                &mut events,
                &mut diagnostics,
            );
        }
        pending = Some((index, line));
    }
    if let Some((index, line)) = pending {
        decode_into(
            &line,
            index,
            true,
            &display_path,
            session_id,
            file_ends_with_newline,
            &mut events,
            &mut diagnostics,
        );
    }

    Ok(SessionEventReadResult {
        events,
        diagnostics,
    })
}

#[allow(clippy::too_many_arguments)]
fn decode_into(
    line: &str,
    index: usize,
    is_last_nonempty: bool,
    display_path: &str,
    session_id: Option<&str>,
    file_ends_with_newline: bool,
    events: &mut Vec<SessionEvent>,
    diagnostics: &mut Vec<SessionRecordDiagnostic>,
) {
    let (event, line_diagnostics) = decode_session_event_line(
        line,
        Some(display_path),
        Some(index + 1),
        session_id,
        is_last_nonempty,
        file_ends_with_newline,
    );
    diagnostics.extend(line_diagnostics);
    if let Some(event) = event {
        events.push(event);
    }
}

/// 解析 `index.json` 顶层文档，返回 sessions 列表与 schema 版本。
///
/// 缺失 `schema_version` 时按 1 处理，兼容既有磁盘数据。
pub fn parse_index_document(data: &Value) -> Result<(Vec<Value>, u64), SessionStoreError> {
    let Some(object) = data.as_object() else {
        return Err(SessionStoreError::new("会话索引顶层必须是 JSON 对象。"));
    };
    let raw_version = object
        .get("schema_version")
        .cloned()
        .unwrap_or_else(|| Value::from(SESSION_INDEX_SCHEMA_VERSION));
    let version = match &raw_version {
        Value::Number(number) => number.as_u64(),
        _ => None,
    };
    let Some(version) = version else {
        return Err(SessionStoreError::new(
            "会话索引 schema_version 必须是整数。",
        ));
    };
    if version != SESSION_INDEX_SCHEMA_VERSION {
        return Err(SessionStoreError::new(format!(
            "暂不支持的会话索引 schema 版本：{version}。当前支持版本：{SESSION_INDEX_SCHEMA_VERSION}。"
        )));
    }
    let sessions = object
        .get("sessions")
        .cloned()
        .unwrap_or(Value::Array(vec![]));
    let Value::Array(items) = sessions else {
        return Err(SessionStoreError::new(
            "会话索引顶层字段 sessions 必须是列表。",
        ));
    };
    Ok((
        items.into_iter().filter(Value::is_object).collect(),
        version,
    ))
}

/// 构造带 `schema_version` 的索引文档。
pub fn build_index_document(entries: &[Value]) -> Value {
    json!({
        "schema_version": SESSION_INDEX_SCHEMA_VERSION,
        "sessions": entries,
    })
}

/// 把诊断交给日志：内核没有 Python 的 logging 设施，调用方拿到诊断自行处理。
///
/// 保留这个入口是为了让调用点与 Python 一一对应；`sink` 决定怎么记。
pub fn log_record_diagnostics(
    diagnostics: &[SessionRecordDiagnostic],
    sink: &mut dyn FnMut(&SessionRecordDiagnostic),
) {
    for item in diagnostics {
        sink(item);
    }
}

fn safe_session_id(data: &Value) -> Option<String> {
    data.get("session_id")
        .and_then(Value::as_str)
        .filter(|text| !text.trim().is_empty())
        .map(str::to_string)
}

fn scalar_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Bool(flag) => if *flag { "True" } else { "False" }.to_string(),
        Value::Null => "None".to_string(),
        other => other.to_string(),
    }
}

fn truncate_chars(text: &str, limit: usize) -> String {
    text.chars().take(limit).collect()
}

/// Python `str.splitlines()` 的等价切分：除 `\n` 外还认 `\r`、`\r\n`、`\v`、`\f`、
/// `\x1c`–`\x1e`、`\u{85}`、`\u{2028}`、`\u{2029}`，且不保留行尾分隔符。
pub fn split_lines_python(text: &str) -> Vec<String> {
    let characters: Vec<char> = text.chars().collect();
    let mut lines: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut index = 0usize;
    while index < characters.len() {
        let character = characters[index];
        if is_line_break(character) {
            lines.push(std::mem::take(&mut current));
            if character == '\r' && characters.get(index + 1) == Some(&'\n') {
                index += 2;
            } else {
                index += 1;
            }
            continue;
        }
        current.push(character);
        index += 1;
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

fn is_line_break(character: char) -> bool {
    matches!(
        character,
        '\n' | '\r'
            | '\u{0b}'
            | '\u{0c}'
            | '\u{1c}'
            | '\u{1d}'
            | '\u{1e}'
            | '\u{85}'
            | '\u{2028}'
            | '\u{2029}'
    )
}
