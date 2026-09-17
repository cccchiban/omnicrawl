//! SSE 层：把 Provider 的 SSE 负载文本解成原生 JSON 负载。

use serde_json::Value;

use crate::json::is_truthy;

/// SSE 层错误。
#[derive(Debug, Clone, PartialEq)]
pub enum SseError {
    /// Provider 在流中下发了 error 负载（对应 Python 侧抛出的 APIError）。
    Provider { message: String, body: Value },
}

/// 单个 SSE 事件的 JSON 负载；空白或不可解析时返回 None。
///
/// 对应 Python `_decode_sse_data` 的原始字符串分支：内核只吃负载文本，
/// SDK 对象上的 `.json()` 快捷分支属于宿主传输层，不在本 crate 范围内。
pub fn decode_sse_data(raw: &str) -> Option<Value> {
    if raw.trim().is_empty() {
        return None;
    }
    serde_json::from_str(raw).ok()
}

/// 直接消费 SSE 负载流，产出原生 JSON 对象。
///
/// 与 Python `_iter_raw_sse_events` 同语义：`[DONE]` 前缀终止、`error` 负载转为错误、
/// 非对象负载跳过。底层 HTTP 连接的关闭由宿主传输层负责。
pub fn iter_raw_sse_events<'a, I>(payloads: I) -> Result<Vec<Value>, SseError>
where
    I: IntoIterator<Item = &'a str>,
{
    let mut events = Vec::new();
    for raw in payloads {
        if raw.starts_with("[DONE]") {
            break;
        }
        let Some(data) = decode_sse_data(raw) else {
            continue;
        };
        let Value::Object(entries) = &data else {
            continue;
        };
        if let Some(error) = entries.get("error") {
            if is_truthy(error) {
                let message = error
                    .get("message")
                    .and_then(Value::as_str)
                    .filter(|text| !text.is_empty())
                    .unwrap_or("An error occurred during streaming")
                    .to_string();
                return Err(SseError::Provider {
                    message,
                    body: error.clone(),
                });
            }
        }
        events.push(data);
    }
    Ok(events)
}
