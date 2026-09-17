//! 会话投影：把持久化的事件流还原成模型上下文消息、待办与运行闸状态。
//!
//! 语义基准是 Python `omnicrawl/state/session_projection.py` 的纯函数部分：
//! `active_session_events`（回退过滤）、`session_title_from_events`、
//! `recover_run_guard_state` / `apply_run_guard_event`、`event_to_model_message`、
//! 工具结果消息构造与协议配对补全。
//!
//! 有状态的那一层（`TurnHistoryProjector`：压缩边界、子任务结果、增量投影）留待后续切片。

use std::collections::{BTreeMap, BTreeSet};
use std::io;

use serde::Serialize;
use serde_json::{Map, Value};

use crate::event::SessionEvent;
use crate::naming::{clean_title, COMPACT_SUMMARY_PREFIX};

pub const TOOL_CALL_CONTEXT_PREFIX: &str = "工具调用请求：";
pub const TOOL_RESULT_CONTEXT_PREFIX: &str = "工具执行结果：";
pub const TURN_UNDONE_EVENT_TYPE: &str = "turn_undone";
pub const RUN_GUARD_TODO_TOOL_NAME: &str = "update_todos";
pub const CANCELLED_TURN_DEFAULT_SUMMARY: &str = "（上一回合被取消，未生成最终回复）";
pub const INTERRUPTED_TOOL_RESULT_TEXT: &str = "（已中断：回合结束前未返回结果，执行状态未知。）";
pub const INTERRUPTED_TURN_DEFAULT_SUMMARY: &str = "（上一回合因异常中断，未生成最终回复）";

const RUN_GUARD_PENDING_EVENTS: [&str; 5] = [
    "run_guard_continue",
    "run_guard_continue_exhausted",
    "run_guard_paused",
    "turn_cancelled",
    "session_interrupted",
];
const TODO_ITEM_LIMIT: usize = 20;
const TODO_STEP_LIMIT: usize = 240;
const TODO_ID_LIMIT: usize = 80;

/// 回退后的有效事件：`turn_undone` 自身不参与投影，它列出的 id 也被隐藏。
pub fn active_session_events(events: &[SessionEvent]) -> Vec<SessionEvent> {
    let mut hidden: BTreeSet<&str> = BTreeSet::new();
    for event in events {
        if event.event_type != TURN_UNDONE_EVENT_TYPE {
            continue;
        }
        let Some(event_ids) = event.payload.get("event_ids").and_then(Value::as_array) else {
            continue;
        };
        for event_id in event_ids {
            if let Some(text) = event_id.as_str() {
                let trimmed = text.trim();
                if !trimmed.is_empty() {
                    hidden.insert(trimmed);
                }
            }
        }
    }
    events
        .iter()
        .filter(|event| {
            event.event_type != TURN_UNDONE_EVENT_TYPE && !hidden.contains(event.event_id.as_str())
        })
        .cloned()
        .collect()
}

/// 按自动标题与显式重命名规则投影当前标题。
pub fn session_title_from_events(events: &[SessionEvent], fallback: &str) -> String {
    let mut title = fallback.to_string();
    let mut first_user_title_applied = false;
    for event in events {
        if event.event_type == "session_started" {
            if let Some(started) = event.payload.get("title").and_then(Value::as_str) {
                if !started.trim().is_empty() {
                    let cleaned = clean_title(started);
                    if !cleaned.is_empty() {
                        title = cleaned;
                    }
                }
            }
        }
        if !first_user_title_applied && event.event_type == "user_message" {
            if let Some(content) = event.payload.get("content").and_then(Value::as_str) {
                if !content.trim().is_empty() {
                    title = clean_title(content);
                    first_user_title_applied = true;
                }
            }
        } else if event.event_type == "session_renamed" {
            if let Some(renamed) = event.payload.get("title").and_then(Value::as_str) {
                if !renamed.trim().is_empty() {
                    title = clean_title(renamed);
                }
            }
        }
    }
    title
}

/// 恢复运行护栏需要的待续任务文本与最后一份待办。
pub fn recover_run_guard_state(events: &[SessionEvent]) -> (String, Vec<Value>) {
    let mut pending = String::new();
    let mut todos: Vec<Value> = Vec::new();
    for event in events {
        let (next_pending, next_todos) =
            apply_run_guard_event(pending, todos, &event.event_type, &event.payload);
        pending = next_pending;
        todos = next_todos;
    }
    (pending, todos)
}

/// 把一条新事件应用到运行护栏投影（与整体恢复共用同一规则）。
pub fn apply_run_guard_event(
    pending_user_text: String,
    todo_items: Vec<Value>,
    event_type: &str,
    payload: &Map<String, Value>,
) -> (String, Vec<Value>) {
    if event_type == "user_message" {
        let pending = pending_text_from_payload(payload, true);
        let todos = match payload.get("todo_items") {
            Some(Value::Array(items)) => normalize_todo_items(items),
            _ => Vec::new(),
        };
        return (pending, todos);
    }

    if event_type == "tool_call_requested" {
        if payload_tool(payload) == RUN_GUARD_TODO_TOOL_NAME {
            if let Some(arguments) = payload.get("arguments").and_then(Value::as_object) {
                return (
                    pending_user_text,
                    normalize_todo_items_value(arguments.get("todos")),
                );
            }
        }
        return (pending_user_text, todo_items);
    }

    if event_type == "tool_result" {
        if payload_tool(payload) == RUN_GUARD_TODO_TOOL_NAME {
            if let Some(recovered) = todo_items_from_tool_result(payload) {
                return (pending_user_text, recovered);
            }
        }
        return (pending_user_text, todo_items);
    }

    if RUN_GUARD_PENDING_EVENTS.contains(&event_type) {
        let candidate = pending_text_from_payload(payload, false);
        let next_todos = match payload.get("todo_items") {
            Some(Value::Array(items)) => normalize_todo_items(items),
            _ => todo_items,
        };
        return (
            if candidate.is_empty() {
                pending_user_text
            } else {
                candidate
            },
            next_todos,
        );
    }

    if event_type == "assistant_message" {
        return (pending_text_from_payload(payload, false), todo_items);
    }

    (pending_user_text, todo_items)
}

/// 一条事件对应的模型消息；不参与上下文的类型返回 `None`。
pub fn event_to_model_message(event: &SessionEvent) -> Option<Value> {
    let payload = &event.payload;
    match event.event_type.as_str() {
        "user_message" => {
            let content = payload.get("content").and_then(Value::as_str)?;
            if content.trim().is_empty() {
                return None;
            }
            Some(json_message("user", content))
        }
        "assistant_message" => {
            let content = payload.get("content").and_then(Value::as_str)?;
            if content.trim().is_empty() {
                return None;
            }
            let mut message = json_message("assistant", content);
            if let Some(reasoning) = payload.get("reasoning_content").and_then(Value::as_str) {
                if !reasoning.is_empty() {
                    if let Some(object) = message.as_object_mut() {
                        object.insert(
                            "reasoning_content".to_string(),
                            Value::String(reasoning.to_string()),
                        );
                    }
                }
            }
            Some(message)
        }
        "turn_cancelled" => {
            let summary = payload
                .get("summary")
                .and_then(Value::as_str)
                .filter(|text| !text.trim().is_empty())
                .unwrap_or(CANCELLED_TURN_DEFAULT_SUMMARY);
            Some(json_message("assistant", summary))
        }
        "run_guard_paused" => {
            let message = payload
                .get("message")
                .and_then(Value::as_str)
                .filter(|text| !text.trim().is_empty())
                .unwrap_or("（上一任务已暂停，用户发送“继续”后恢复。）");
            Some(json_message("assistant", message.trim()))
        }
        "compact_summary" => {
            let content = payload.get("content").and_then(Value::as_str)?;
            if content.trim().is_empty() {
                return None;
            }
            Some(json_message(
                "assistant",
                &format!("{COMPACT_SUMMARY_PREFIX}{content}"),
            ))
        }
        "tool_call_requested" => context_message(tool_call_context(payload)),
        "tool_call_denied" => context_message(tool_denied_context(payload)),
        "tool_result" => context_message(tool_result_context(payload)),
        _ => None,
    }
}

/// 按模型可见优先级取工具结果输出。
pub fn tool_result_output_text(payload: &Map<String, Value>) -> String {
    for key in ["model_output", "output_preview", "output"] {
        if let Some(value) = payload.get(key).and_then(Value::as_str) {
            if !value.trim().is_empty() {
                return value.to_string();
            }
        }
    }
    String::new()
}

/// 工具结果的模型可见正文；运行时代理与恢复投影共用同一份文案。
pub fn format_tool_result_content(tool: &str, ok: bool, output: &str) -> String {
    format!(
        "状态：{}\n工具：{tool}\n结果：\n{output}",
        if ok { "成功" } else { "失败" }
    )
}

pub fn tool_result_message(tool: &str, ok: bool, output: &str, tool_call_id: &str) -> Value {
    let mut message = Map::new();
    message.insert("role".to_string(), Value::String("tool".to_string()));
    message.insert(
        "tool_call_id".to_string(),
        Value::String(tool_call_id.to_string()),
    );
    message.insert(
        "content".to_string(),
        Value::String(format_tool_result_content(tool, ok, output)),
    );
    Value::Object(message)
}

/// 未返回结果的工具调用占位：只声明「已中断」，不伪装成功或失败结论。
pub fn interrupted_tool_result_message(tool: &str, tool_call_id: &str) -> Value {
    tool_result_message(tool, false, INTERRUPTED_TOOL_RESULT_TEXT, tool_call_id)
}

/// 为缺失结果的 assistant tool_calls 补齐「已中断」结果，保证协议配对。
pub fn complete_tool_pairing(messages: &[Value]) -> Vec<Value> {
    let mut completed: Vec<Value> = Vec::new();
    let mut index = 0;
    while index < messages.len() {
        let message = &messages[index];
        if message.get("role").and_then(Value::as_str) == Some("tool") {
            // 走到这里说明这条 tool 消息没有紧邻的前置 tool_calls：丢弃，避免非法协议。
            index += 1;
            continue;
        }
        completed.push(message.clone());

        let tool_calls = if message.get("role").and_then(Value::as_str) == Some("assistant") {
            message.get("tool_calls").and_then(Value::as_array).cloned()
        } else {
            None
        };
        let Some(tool_calls) = tool_calls.filter(|calls| !calls.is_empty()) else {
            index += 1;
            continue;
        };

        let mut expected: Vec<(String, String)> = Vec::new();
        for call in &tool_calls {
            let Some(call) = call.as_object() else {
                continue;
            };
            let name = call
                .get("function")
                .and_then(Value::as_object)
                .and_then(|function| function.get("name"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let call_id = call
                .get("id")
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| name.clone());
            if !call_id.is_empty() {
                expected.push((
                    call_id.clone(),
                    if name.is_empty() { call_id } else { name },
                ));
            }
        }
        let expected_ids: BTreeSet<&str> = expected.iter().map(|(id, _)| id.as_str()).collect();
        let mut seen: BTreeSet<String> = BTreeSet::new();
        let mut cursor = index + 1;
        while cursor < messages.len()
            && messages[cursor].get("role").and_then(Value::as_str) == Some("tool")
        {
            let call_id = messages[cursor]
                .get("tool_call_id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            if !call_id.is_empty() && !expected_ids.contains(call_id.as_str()) {
                // 多余或错配的结果：不能进入协议，否则 Provider 会拒收。
                cursor += 1;
                continue;
            }
            completed.push(messages[cursor].clone());
            if !call_id.is_empty() {
                seen.insert(call_id);
            }
            cursor += 1;
        }
        for (call_id, name) in expected {
            if !seen.contains(&call_id) {
                completed.push(interrupted_tool_result_message(&name, &call_id));
            }
        }
        index = cursor;
    }
    completed
}

/// 构造单条 OpenAI 形状的 tool_call（`arguments` 为已序列化的 JSON 字符串）。
pub fn function_tool_call(call_id: &str, function_name: &str, arguments: &str) -> Value {
    let mut function = Map::new();
    function.insert("name".to_string(), Value::String(function_name.to_string()));
    function.insert(
        "arguments".to_string(),
        Value::String(arguments.to_string()),
    );
    let mut call = Map::new();
    call.insert("id".to_string(), Value::String(call_id.to_string()));
    call.insert("type".to_string(), Value::String("function".to_string()));
    call.insert("function".to_string(), Value::Object(function));
    Value::Object(call)
}

fn json_message(role: &str, content: &str) -> Value {
    let mut message = Map::new();
    message.insert("role".to_string(), Value::String(role.to_string()));
    message.insert("content".to_string(), Value::String(content.to_string()));
    Value::Object(message)
}

fn context_message(content: String) -> Option<Value> {
    if content.is_empty() {
        None
    } else {
        Some(json_message("assistant", &content))
    }
}

fn payload_tool(payload: &Map<String, Value>) -> String {
    payload
        .get("tool")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string()
}

fn pending_text_from_payload(payload: &Map<String, Value>, include_content: bool) -> String {
    let keys: &[&str] = if include_content {
        &["pending_user_text", "user_text", "content"]
    } else {
        &["pending_user_text", "user_text"]
    };
    for key in keys {
        if let Some(value) = payload.get(*key).and_then(Value::as_str) {
            if !value.trim().is_empty() {
                return value.trim().to_string();
            }
        }
    }
    String::new()
}

fn normalize_todo_items_value(raw: Option<&Value>) -> Vec<Value> {
    match raw {
        Some(Value::Array(items)) => normalize_todo_items(items),
        _ => Vec::new(),
    }
}

fn normalize_todo_items(raw_todos: &[Value]) -> Vec<Value> {
    let mut normalized = Vec::new();
    for (offset, raw_item) in raw_todos.iter().take(TODO_ITEM_LIMIT).enumerate() {
        let Some(item) = raw_item.as_object() else {
            continue;
        };
        let step = ["step", "description", "title"]
            .iter()
            .find_map(|key| item.get(*key).map(python_text))
            .map(|text| text.trim().to_string())
            .unwrap_or_default();
        if step.is_empty() {
            continue;
        }
        let status = item
            .get("status")
            .map(python_text)
            .unwrap_or_default()
            .trim()
            .to_lowercase();
        let completed = item.get("completed").map(python_truthy).unwrap_or(false)
            || matches!(status.as_str(), "completed" | "done" | "complete");
        let index = offset + 1;
        let item_id = {
            let raw = match item.get("id") {
                Some(value) if python_truthy(value) => python_text(value),
                _ => index.to_string(),
            };
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                index.to_string()
            } else {
                trimmed.chars().take(TODO_ID_LIMIT).collect()
            }
        };
        let mut entry = Map::new();
        entry.insert("id".to_string(), Value::String(item_id));
        entry.insert(
            "step".to_string(),
            Value::String(step.chars().take(TODO_STEP_LIMIT).collect()),
        );
        entry.insert("completed".to_string(), Value::Bool(completed));
        normalized.push(Value::Object(entry));
    }
    normalized
}

fn todo_items_from_tool_result(payload: &Map<String, Value>) -> Option<Vec<Value>> {
    for key in ["model_output", "output"] {
        let Some(raw_output) = payload.get(key).and_then(Value::as_str) else {
            continue;
        };
        if raw_output.trim().is_empty() {
            continue;
        }
        let Ok(decoded) = serde_json::from_str::<Value>(raw_output) else {
            continue;
        };
        if let Some(object) = decoded.as_object() {
            if let Some(todos) = object.get("todos") {
                return Some(normalize_todo_items_value(Some(todos)));
            }
        }
    }
    None
}

fn tool_call_context(payload: &Map<String, Value>) -> String {
    let tool = payload_tool(payload);
    if tool.is_empty() {
        return String::new();
    }
    let arguments = payload.get("arguments").and_then(Value::as_object);
    let arguments_text = match arguments {
        Some(arguments) => dumps_sorted(arguments),
        None => dumps_sorted(&Map::new()),
    };
    format!("{TOOL_CALL_CONTEXT_PREFIX}{tool} 参数：{arguments_text}")
}

fn tool_denied_context(payload: &Map<String, Value>) -> String {
    let tool = payload_tool(payload);
    if tool.is_empty() {
        return String::new();
    }
    let reason = payload
        .get("reason")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .unwrap_or("未批准。");
    format!("{TOOL_RESULT_CONTEXT_PREFIX}{tool} 失败，原因：{reason}")
}

fn tool_result_context(payload: &Map<String, Value>) -> String {
    let tool = payload_tool(payload);
    if tool.is_empty() {
        return String::new();
    }
    let status = if payload.get("ok").map(python_truthy).unwrap_or(false) {
        "成功"
    } else {
        "失败"
    };
    let mut output = payload
        .get("model_output")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty());
    if output.is_none() {
        output = payload
            .get("output_preview")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty());
    }
    if output.is_none() {
        output = payload
            .get("output")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty());
    }
    let output = output.unwrap_or("");
    let artifact_hint = payload
        .get("artifact_path")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(|path| format!("\n完整输出 artifact：{path}"))
        .unwrap_or_default();
    format!("{TOOL_RESULT_CONTEXT_PREFIX}{tool} {status}\n{output}{artifact_hint}")
        .trim_end()
        .to_string()
}

/// Python 的 `json.dumps(..., ensure_ascii=False, sort_keys=True)` 等价物：
/// 键按字典序、分隔符带空格、非 ASCII 原样输出。
fn dumps_sorted(value: &Map<String, Value>) -> String {
    let sorted = sorted_value(&Value::Object(value.clone()));
    let mut buffer = Vec::new();
    let mut serializer = serde_json::Serializer::with_formatter(&mut buffer, PythonFormatter);
    if sorted.serialize(&mut serializer).is_err() {
        return "{}".to_string();
    }
    String::from_utf8(buffer).unwrap_or_else(|_| "{}".to_string())
}

fn sorted_value(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut sorted: BTreeMap<String, Value> = BTreeMap::new();
            for (key, item) in map {
                sorted.insert(key.clone(), sorted_value(item));
            }
            Value::Object(sorted.into_iter().collect())
        }
        Value::Array(items) => Value::Array(items.iter().map(sorted_value).collect()),
        other => other.clone(),
    }
}

struct PythonFormatter;

impl serde_json::ser::Formatter for PythonFormatter {
    fn begin_array_value<W>(&mut self, writer: &mut W, first: bool) -> io::Result<()>
    where
        W: ?Sized + io::Write,
    {
        if first {
            Ok(())
        } else {
            writer.write_all(b", ")
        }
    }

    fn begin_object_key<W>(&mut self, writer: &mut W, first: bool) -> io::Result<()>
    where
        W: ?Sized + io::Write,
    {
        if first {
            Ok(())
        } else {
            writer.write_all(b", ")
        }
    }

    fn begin_object_value<W>(&mut self, writer: &mut W) -> io::Result<()>
    where
        W: ?Sized + io::Write,
    {
        writer.write_all(b": ")
    }
}

/// Python 的 `str(value)`：字符串原样，布尔写 `True`/`False`。
fn python_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Bool(flag) => (if *flag { "True" } else { "False" }).to_string(),
        other => other.to_string(),
    }
}

/// Python 的真值语义（`bool(value)`），投影里用来判断 `completed` 等字段。
fn python_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64().map(|value| value != 0.0).unwrap_or(true),
        Value::String(text) => !text.is_empty(),
        Value::Array(items) => !items.is_empty(),
        Value::Object(map) => !map.is_empty(),
    }
}
