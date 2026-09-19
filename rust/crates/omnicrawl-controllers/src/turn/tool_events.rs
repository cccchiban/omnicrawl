//! `controllers/turn/loop.py` 的工具调用事件字段：协议原文的提取与落盘边界。
//!
//! 恢复投影优先用 `arguments_json` / `function_name`——它们就是真正发往 Provider 的字段，
//! 公开参数投影只用于 UI 与审计。而协议原文可能含明文密钥，只留在内存投影里，绝不落盘。

use serde_json::{Map, Value};

use crate::shared::{python_str, python_truthy};

/// 只留在内存投影、不随事件落盘的协议原文字段。
pub const PROTOCOL_ONLY_EVENT_FIELDS: [&str; 1] = ["arguments_json"];

/// 从事件字段里剔除协议原文（可能含明文密钥）。
pub fn strip_protocol_only_fields(fields: &mut Map<String, Value>) {
    for key in PROTOCOL_ONLY_EVENT_FIELDS {
        fields.remove(key);
    }
}

/// 从运行期原始 assistant 消息里取出该调用的 `arguments_json` 与 `function_name`。
///
/// 按「调用 ID 相等或函数名相等」匹配**首个**命中的调用；`arguments` 不是字符串时
/// 不带协议原文，函数名为空时也不写该字段。
pub fn raw_tool_call_event_fields(
    assistant_message: &Value,
    call_id: &str,
    tool_name: &str,
) -> Map<String, Value> {
    let Some(message) = assistant_message.as_object() else {
        return Map::new();
    };
    let Some(raw_calls) = message.get("tool_calls").and_then(Value::as_array) else {
        return Map::new();
    };
    for raw_call in raw_calls {
        let Some(call) = raw_call.as_object() else {
            continue;
        };
        let raw_id = raw_str(call.get("id"));
        let Some(function) = call.get("function").and_then(Value::as_object) else {
            continue;
        };
        let raw_name = raw_str(function.get("name"));
        if raw_id != call_id && raw_name != tool_name {
            continue;
        }
        let mut fields = Map::new();
        if let Some(Value::String(arguments)) = function.get("arguments") {
            fields.insert("arguments_json".to_string(), Value::from(arguments.clone()));
        }
        if !raw_name.is_empty() {
            fields.insert("function_name".to_string(), Value::from(raw_name));
        }
        return fields;
    }
    Map::new()
}

/// Python `str(value or "")`：falsy 一律成空串。
fn raw_str(value: Option<&Value>) -> String {
    match value {
        Some(item) if python_truthy(item) => python_str(item),
        _ => String::new(),
    }
}
