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

/// `tool_call_requested` 事件载荷（对映 Python `_normalize_tool_calls` 的落盘形状）。
///
/// `arguments` 传的是 `public_tool_arguments` 投影后的公开参数；`assistant_message` 是本批
/// 真正发往 Provider 的 assistant 原文。键序与 Python 一致：
/// `tool → arguments → tool_call_id → function_name → assistant_content →
/// assistant_reasoning_content`——真转录的采样与 `tool_events.field_keys` 都按这个顺序。
///
/// 带回来的 `assistant_content` / 思考回传字段 / `function_name` 是**恢复后写法一致**的前提：
/// 少了它们，同一段历史在重启后会换一种写法，前缀缓存必然失效。
/// `arguments_json`（可能含明文密钥的协议原文）由 [`strip_protocol_only_fields`] 剔除，绝不落盘。
pub fn requested_event_payload(
    tool: &str,
    arguments: Value,
    tool_call_id: &str,
    function_name: &str,
    assistant_message: Option<&Value>,
) -> Map<String, Value> {
    // Python 的 `tool_call.id or tool_call.name`：调用 ID 缺省时用工具名顶上，
    // 协议原文字段的匹配与事件载荷里的 ID 都用这个「有效调用 ID」。
    let effective_call_id = if tool_call_id.is_empty() {
        tool
    } else {
        tool_call_id
    };
    let mut raw_fields = raw_tool_call_event_fields(
        assistant_message.unwrap_or(&Value::Null),
        effective_call_id,
        tool,
    );
    strip_protocol_only_fields(&mut raw_fields);
    let raw_function_name = raw_fields
        .get("function_name")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();

    let mut payload = Map::new();
    payload.insert("tool".to_string(), Value::from(tool));
    payload.insert("arguments".to_string(), arguments);
    payload.insert(
        "tool_call_id".to_string(),
        Value::from(effective_call_id),
    );
    // `assistant_fields.get("function_name") or tool_call.function_name or tool_call.name`
    payload.insert(
        "function_name".to_string(),
        Value::from(effective_function_name(
            &raw_function_name,
            function_name,
            tool,
        )),
    );
    if let Some(message) = assistant_message.and_then(Value::as_object) {
        // 键序与 Python 的 `assistant_fields` 一致：内容在前，思考在后。
        payload.insert("assistant_content".to_string(), assistant_content(message));
        if let Some(reasoning) = assistant_reasoning(message) {
            payload.insert(
                "assistant_reasoning_content".to_string(),
                Value::from(reasoning),
            );
        }
    }
    // 协议原文之外的其余字段（当前只有 `function_name`）按提取顺序补上。
    for (key, value) in raw_fields {
        payload.insert(key, value);
    }
    payload
}

/// `tool_result` 事件载荷（对映 Python `_execute_tool_batch` 的落盘形状）。
///
/// `full_output` 是展示全文、`output` 是模型可见输出：事件里的 `output` 取展示全文优先
/// （空串回落模型输出），`model_output` 固定是模型可见输出。`output_sha256` /
/// `output_size_chars` / `storage` / `artifact_path` 与 `output_preview` 由会话存储按体积
/// 自动补齐，这里不写。`ui_artifact` 由调用方给（内核从宿主拿不到时用 Python 缺省的 `{}`）。
pub fn result_event_payload(
    tool: &str,
    tool_call_id: &str,
    ok: bool,
    full_output: &str,
    output: &str,
    ui_artifact: Value,
) -> Map<String, Value> {
    let mut payload = Map::new();
    payload.insert("tool".to_string(), Value::from(tool));
    payload.insert(
        "tool_call_id".to_string(),
        Value::from(if tool_call_id.is_empty() {
            tool
        } else {
            tool_call_id
        }),
    );
    payload.insert("ok".to_string(), Value::Bool(ok));
    payload.insert(
        "output".to_string(),
        Value::from(if full_output.is_empty() {
            output
        } else {
            full_output
        }),
    );
    payload.insert("model_output".to_string(), Value::from(output));
    payload.insert("ui_artifact".to_string(), ui_artifact);
    payload
}

/// `assistant_fields.get("function_name") or tool_call.function_name or tool_call.name`。
fn effective_function_name(raw: &str, function_name: &str, tool: &str) -> String {
    for candidate in [raw, function_name] {
        if !candidate.is_empty() {
            return candidate.to_string();
        }
    }
    tool.to_string()
}

/// Python 的 `content if isinstance(content, str) or content is None else None`。
///
/// 缺字段与 `null` 都写 `null`（不是不写字段）；非字符串的 `content` 也按 `null` 处理。
fn assistant_content(message: &Map<String, Value>) -> Value {
    match message.get("content") {
        Some(Value::String(text)) => Value::from(text.clone()),
        _ => Value::Null,
    }
}

/// Python 的 `reasoning_content` 优先、回落 `reasoning`；两者都不是非空字符串时**不写**该字段。
fn assistant_reasoning(message: &Map<String, Value>) -> Option<String> {
    for key in ["reasoning_content", "reasoning"] {
        if let Some(Value::String(text)) = message.get(key) {
            if !text.is_empty() {
                return Some(text.clone());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    //! 这两个载荷构造器在 Python 侧是 `loop.py` 里的**内联**字典字面量，没有可单独调用的函数，
    //! 因此不在 `controllers_parity.json` 的 `tool_events` 段里（那段只钉 `raw_tool_call_event_fields`）；
    //! 这里用真转录的采样键序与 Python 的同名字段规则做自检。

    use super::*;
    use serde_json::json;

    /// 带工具调用的 assistant 原文：content + reasoning_content + 一条调用。
    fn assistant_message() -> Value {
        json!({
            "role": "assistant",
            "content": "先看一下目录。",
            "reasoning_content": "用户问的是训练轮数，先列目录。",
            "tool_calls": [{
                "id": "call-1",
                "type": "function",
                "function": {"name": "list", "arguments": "{\"path\": \".\"}"},
            }],
        })
    }

    fn keys(map: &Map<String, Value>) -> Vec<String> {
        map.keys().cloned().collect()
    }

    #[test]
    fn requested_payload_matches_python_key_order_and_fields() {
        let payload = requested_event_payload(
            "list",
            json!({"path": "."}),
            "call-1",
            "list",
            Some(&assistant_message()),
        );
        assert_eq!(
            keys(&payload),
            vec![
                "tool",
                "arguments",
                "tool_call_id",
                "function_name",
                "assistant_content",
                "assistant_reasoning_content"
            ]
        );
        assert_eq!(payload["tool"], "list");
        assert_eq!(payload["arguments"], json!({"path": "."}));
        assert_eq!(payload["tool_call_id"], "call-1");
        assert_eq!(payload["function_name"], "list");
        assert_eq!(payload["assistant_content"], "先看一下目录。");
        assert_eq!(
            payload["assistant_reasoning_content"],
            "用户问的是训练轮数，先列目录。"
        );
        // 协议原文（可能含明文密钥）绝不落盘。
        assert!(!payload.contains_key("arguments_json"));
    }

    #[test]
    fn requested_payload_falls_back_to_reasoning_and_tool_name() {
        // 没有 reasoning_content 时用 reasoning；调用 ID 为空时用工具名，协议匹配也走工具名。
        let message = json!({
            "role": "assistant",
            "content": null,
            "reasoning": "思考",
            "tool_calls": [{
                "function": {"name": "read", "arguments": "{\"path\": \"a\"}"},
            }],
        });
        let payload = requested_event_payload("read", json!({"path": "a"}), "", "", Some(&message));
        assert_eq!(
            keys(&payload),
            vec![
                "tool",
                "arguments",
                "tool_call_id",
                "function_name",
                "assistant_content",
                "assistant_reasoning_content"
            ]
        );
        assert_eq!(payload["tool_call_id"], "read");
        assert_eq!(payload["function_name"], "read");
        assert_eq!(payload["assistant_content"], Value::Null);
        assert_eq!(payload["assistant_reasoning_content"], "思考");
    }

    #[test]
    fn requested_payload_without_assistant_message_keeps_only_protocol_fields() {
        let payload = requested_event_payload("bash", json!({"command": "ls"}), "call-9", "", None);
        assert_eq!(
            keys(&payload),
            vec!["tool", "arguments", "tool_call_id", "function_name"]
        );
        assert_eq!(payload["function_name"], "bash");
        assert_eq!(payload["tool_call_id"], "call-9");
    }

    #[test]
    fn requested_payload_prefers_the_raw_function_name() {
        // 原始消息里的函数名与规范化后的工具名不同（invoke_tool 一类）：协议名字优先。
        let message = json!({
            "tool_calls": [{
                "id": "call-2",
                "function": {"name": "invoke_tool", "arguments": "{\"tool_name\": \"read\"}"},
            }],
        });
        let payload =
            requested_event_payload("read", json!({"path": "a"}), "call-2", "read", Some(&message));
        assert_eq!(payload["function_name"], "invoke_tool");
        assert_eq!(payload["tool"], "read");
    }

    #[test]
    fn result_payload_prefers_full_output_and_keeps_model_output() {
        let payload = result_event_payload("list", "call-1", true, "全文", "预览", json!({}));
        assert_eq!(
            keys(&payload),
            vec![
                "tool",
                "tool_call_id",
                "ok",
                "output",
                "model_output",
                "ui_artifact"
            ]
        );
        assert_eq!(payload["output"], "全文");
        assert_eq!(payload["model_output"], "预览");
        assert_eq!(payload["ui_artifact"], json!({}));
        // `full_output or output`：展示全文为空串时回落到模型可见输出。
        let fallback = result_event_payload("list", "", false, "", "失败原因", json!({}));
        assert_eq!(fallback["output"], "失败原因");
        assert_eq!(fallback["model_output"], "失败原因");
        assert_eq!(fallback["tool_call_id"], "list");
        assert_eq!(fallback["ok"], false);
    }
}
