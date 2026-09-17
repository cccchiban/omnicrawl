//! openai_chat Provider 的分片级映射：chunk → 归一化流事件。
//!
//! 语义基准：`omnicrawl/llm/providers/openai_chat.py` 的 `_first_choice` /
//! `_emit_tool_call_deltas` / `_arguments_json_complete`。

use std::collections::{BTreeMap, BTreeSet};

use omnicrawl_protocol::{ModelStreamEvent, ToolCallArgumentsDelta, ToolCallStarted};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::json::{is_truthy, text_of};

/// 单个工具调用的分片累积缓冲（对应 Python 侧 `{"id", "name", "arguments"}` dict）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ToolCallBuffer {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

/// 取首个 choice；无 choice 时返回 None。
pub fn first_choice(chunk: &Value) -> Option<&Value> {
    chunk
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
}

/// 工具调用参数是否为完整可解析的 JSON。
///
/// 非字符串与空字符串视为完整（模型可不带参数）；非空字符串必须能被解析，
/// 否则说明参数流被网关截断（半截 JSON），不能当正常调用收尾。
pub fn arguments_json_complete(raw: &Value) -> bool {
    let Some(text) = raw.as_str() else {
        return true;
    };
    if text.trim().is_empty() {
        return true;
    }
    serde_json::from_str::<Value>(text).is_ok()
}

/// 把一批工具调用分片归并进缓冲，并产出增量事件。
///
/// 与 Python `_emit_tool_call_deltas` 同语义：`index` 缺失按 0；`id` 后到可覆盖；
/// 名SyntaxError非空且该下标未 start 过时补一发 started；参数分片非空时补一发参数增量。
/// `call_id` 用缓冲里的 id，缺失时回落到 `call_{index}`。
pub fn emit_tool_call_deltas(
    deltas: &[Value],
    buffers: &mut BTreeMap<u64, ToolCallBuffer>,
    started: &mut BTreeSet<u64>,
) -> Vec<ModelStreamEvent> {
    let mut events = Vec::new();
    for delta in deltas {
        let index = delta.get("index").and_then(Value::as_u64).unwrap_or(0);
        let buffer = buffers.entry(index).or_default();

        if let Some(id) = delta.get("id") {
            if is_truthy(id) {
                buffer.id = text_of(id);
            }
        }

        let mut arguments_delta = String::new();
        if let Some(function) = delta.get("function") {
            if let Some(name) = function.get("name") {
                if is_truthy(name) {
                    buffer.name.push_str(&text_of(name));
                }
            }
            if let Some(arguments) = function.get("arguments") {
                if is_truthy(arguments) {
                    arguments_delta = text_of(arguments);
                    buffer.arguments.push_str(&arguments_delta);
                }
            }
        }

        let call_id = if buffer.id.is_empty() {
            format!("call_{index}")
        } else {
            buffer.id.clone()
        };
        if !buffer.name.is_empty() && !started.contains(&index) {
            started.insert(index);
            events.push(ModelStreamEvent::ToolCallStarted(ToolCallStarted {
                call_id: call_id.clone(),
                name: buffer.name.clone(),
            }));
        }
        if !arguments_delta.is_empty() {
            events.push(ModelStreamEvent::ToolCallArgumentsDelta(
                ToolCallArgumentsDelta {
                    call_id,
                    delta: arguments_delta,
                },
            ));
        }
    }
    events
}
