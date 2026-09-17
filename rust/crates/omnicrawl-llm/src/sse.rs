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

/// 单条 SSE 负载文本的语义。
#[derive(Debug, Clone, PartialEq)]
pub enum SseStep {
    /// 收到 `[DONE]`：本次回复的流到此结束。
    Terminated,
    /// 与本次回复无关：空白、不可解析、非对象负载。
    Skip,
    Payload(Value),
}

/// 从一行 SSE 里取出 data 负载文本。
///
/// 非 data 行（空行、`:` 注释、`event:`/`id:`/`retry:` 字段）返回 None；
/// 冒号后按 SSE 规范剥掉一个前导空格。
pub fn payload_of_line(line: &str) -> Option<&str> {
    // 行尾换行属于 SSE 的行分隔，不属于负载本身。
    let line = line.trim_end_matches(['\n', '\r']);
    let rest = line.strip_prefix("data:")?;
    Some(rest.strip_prefix(' ').unwrap_or(rest))
}

/// 解释单条负载文本：`[DONE]` 终止、`error` 负载转错误、非对象负载跳过。
pub fn step_payload(raw: &str) -> Result<SseStep, SseError> {
    if raw.starts_with("[DONE]") {
        return Ok(SseStep::Terminated);
    }
    let Some(data) = decode_sse_data(raw) else {
        return Ok(SseStep::Skip);
    };
    let Value::Object(entries) = &data else {
        return Ok(SseStep::Skip);
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
    Ok(SseStep::Payload(data))
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
        match step_payload(raw)? {
            SseStep::Terminated => break,
            SseStep::Skip => continue,
            SseStep::Payload(data) => events.push(data),
        }
    }
    Ok(events)
}
