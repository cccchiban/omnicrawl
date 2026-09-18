//! Anthropic Claude Messages Provider：请求构建与流事件映射。
//!
//! 语义基准：`omnicrawl/llm/providers/anthropic.py` 的 `_sanitize_options`、
//! `_to_anthropic_messages`、`_format_anthropic_error` 与
//! `AnthropicMessagesRuntime._stream_turn_events`。
//! 纯逻辑：不建连、不重试，HTTP 传输与能力门禁由 runtime 与调用方负责。

use std::collections::BTreeMap;

use omnicrawl_protocol::{
    parse_arguments_object, tools_from_conversation_messages, ConversationMessage, MessageBlock,
    ModelStreamEvent, ReasoningDelta, Role, TextDelta, ToolCallCompleted, ToolCallStarted,
    ToolSpec, UsageReported,
};
use serde_json::{json, Map, Value};

use crate::json::{is_truthy, text_of};
use crate::request::{ChatRequestInput, RequestError};
use crate::usage::usage_from_anthropic_payload;

/// Anthropic API 版本头，与 SDK 默认一致。
pub const ANTHROPIC_VERSION: &str = "2023-06-01";

/// 未声明 `max_tokens` 时的兜底值（Python 侧 `… or 4096`）。
const DEFAULT_MAX_TOKENS: u32 = 4096;

const OPTION_ALLOWLIST: &[&str] = &["top_p", "top_k", "metadata", "stop_sequences", "thinking"];

const FORBIDDEN_OPTIONS: &[&str] = &[
    "model",
    "messages",
    "tools",
    "system",
    "stream",
    "max_tokens",
    "api_key",
    "base_url",
    "timeout",
];

/// 组装好的 Claude Messages 请求。
///
/// `timeout_seconds` 取自 Profile：Python 侧超时挂在 SDK 客户端上，请求体里没有这个字段。
pub struct AnthropicRequest {
    pub body: Value,
    pub timeout_seconds: f64,
}

/// `provider_options` 校验：Host 字段不可覆盖，白名单之外一律拒绝。
pub fn sanitize_anthropic_options(
    options: &Map<String, Value>,
) -> Result<Map<String, Value>, RequestError> {
    let mut result = Map::new();
    for (key, value) in options {
        if FORBIDDEN_OPTIONS.contains(&key.as_str()) {
            return Err(RequestError {
                message: format!("provider_options 不允许覆盖 Host 字段：{key}"),
            });
        }
        if !OPTION_ALLOWLIST.contains(&key.as_str()) {
            return Err(RequestError {
                message: format!("Anthropic provider_options 不支持字段：{key}"),
            });
        }
        result.insert(key.clone(), value.clone());
    }
    Ok(result)
}

/// 会话消息 → Anthropic messages。
///
/// Anthropic 没有「消息内 tools」概念：system 消息携带的动态声明已并入请求级 `tools`，
/// 因此这里跳过这类 system 消息。工具结果必须紧邻 tool_use 之后，
/// 所以待发的 tool_result 与紧随其后的用户内容合并成同一条 user 消息。
pub fn to_anthropic_messages(messages: &[ConversationMessage]) -> Vec<Value> {
    let mut result: Vec<Value> = Vec::new();
    let mut pending: Vec<Value> = Vec::new();

    for message in messages {
        if message.role == Role::System && !message.tools.is_empty() {
            continue;
        }
        if message.role == Role::Tool {
            for block in &message.blocks {
                if let MessageBlock::ToolResult(tool_result) = block {
                    pending.push(json!({
                        "type": "tool_result",
                        "tool_use_id": tool_result.call_id,
                        "content": tool_result.content,
                        "is_error": !tool_result.ok,
                    }));
                }
            }
            continue;
        }

        if message.role == Role::Assistant {
            flush_tool_results(&mut result, &mut pending);
            let mut content: Vec<Value> = Vec::new();
            for block in &message.blocks {
                match block {
                    MessageBlock::Text(text) if !text.text.is_empty() => {
                        content.push(json!({"type": "text", "text": text.text}));
                    }
                    MessageBlock::ToolCall(call) => {
                        let call_id = if call.call_id.is_empty() {
                            call.provider_call_id.clone()
                        } else {
                            call.call_id.clone()
                        };
                        let arguments = if call.arguments.is_empty() {
                            json!({})
                        } else {
                            Value::Object(call.arguments.clone())
                        };
                        content.push(json!({
                            "type": "tool_use",
                            "id": call_id,
                            "name": call.name,
                            "input": arguments,
                        }));
                    }
                    _ => {}
                }
            }
            if content.is_empty() {
                content.push(json!({"type": "text", "text": ""}));
            }
            result.push(json!({"role": "assistant", "content": content}));
            continue;
        }

        // user：截图以原生 Base64 image source 发送，不把临时路径交给远端读取。
        let mut content: Vec<Value> = Vec::new();
        for block in &message.blocks {
            match block {
                MessageBlock::Text(text) if !text.text.is_empty() => {
                    content.push(json!({"type": "text", "text": text.text}));
                }
                MessageBlock::Image(image) => {
                    content.push(json!({
                        "type": "image",
                        "source": {
                            "type": "base64",
                            "media_type": image.media_type,
                            "data": image.data_base64,
                        },
                    }));
                }
                _ => {}
            }
        }
        if !pending.is_empty() {
            let mut merged = std::mem::take(&mut pending);
            merged.extend(content);
            content = merged;
        }
        result.push(json!({
            "role": "user",
            "content": if content.is_empty() { Value::String(String::new()) } else { Value::Array(content) },
        }));
    }

    flush_tool_results(&mut result, &mut pending);
    result
}

fn flush_tool_results(result: &mut Vec<Value>, pending: &mut Vec<Value>) {
    if pending.is_empty() {
        return;
    }
    let content = std::mem::take(pending);
    result.push(json!({"role": "user", "content": content}));
}

/// 组装 Claude Messages 请求体（不含建连与重试）。
///
/// `descriptor_max_output_tokens` 是模型描述里的上限，仅在生成选项未声明时参与兜底。
pub fn build_anthropic_request(
    input: &ChatRequestInput<'_>,
    descriptor_max_output_tokens: Option<u32>,
) -> Result<AnthropicRequest, RequestError> {
    let provider_options = sanitize_anthropic_options(&input.options.provider_options)?;
    let max_tokens = input
        .options
        .max_output_tokens
        .filter(|value| *value != 0)
        .or_else(|| descriptor_max_output_tokens.filter(|value| *value != 0))
        .unwrap_or(DEFAULT_MAX_TOKENS);

    let mut body = Map::new();
    body.insert("model".to_string(), json!(input.model));
    body.insert("max_tokens".to_string(), json!(max_tokens));
    body.insert(
        "messages".to_string(),
        Value::Array(to_anthropic_messages(input.messages)),
    );
    body.insert("stream".to_string(), Value::Bool(true));
    if !input.system_prompt.trim().is_empty() {
        body.insert("system".to_string(), json!(input.system_prompt));
    }

    // 请求级 tools = 顶层全局声明 + system 消息里携带的动态声明（合并后语义为全局可见）。
    let mut tools: Vec<ToolSpec> = input.tools.to_vec();
    tools.extend(tools_from_conversation_messages(input.messages));
    if !tools.is_empty() {
        body.insert(
            "tools".to_string(),
            Value::Array(tools.iter().map(tool_schema).collect()),
        );
    }

    if let Some(temperature) = input.options.temperature {
        body.insert("temperature".to_string(), json!(temperature));
    }
    for (key, value) in provider_options {
        body.insert(key, value);
    }

    Ok(AnthropicRequest {
        body: Value::Object(body),
        timeout_seconds: input.profile_request_timeout_seconds,
    })
}

fn tool_schema(tool: &ToolSpec) -> Value {
    let input_schema = if tool.parameters.is_empty() {
        json!({"type": "object", "properties": {}})
    } else {
        Value::Object(tool.parameters.clone())
    };
    json!({
        "name": tool.name,
        "description": tool.description,
        "input_schema": input_schema,
    })
}

/// Anthropic 的请求失败文案；输入是 SDK 异常等价文本与异常类型名。
pub fn format_anthropic_error(message: &str, type_name: &str) -> String {
    let lowered = message.trim().to_lowercase();
    if lowered.contains("authentication") || lowered.contains("api key") || lowered.contains("401")
    {
        return "Claude 鉴权失败。请检查 ANTHROPIC_API_KEY。".to_string();
    }
    if lowered.contains("permission") || lowered.contains("403") {
        return "当前 API Key 没有访问该 Claude 模型的权限。".to_string();
    }
    if lowered.contains("not_found") || lowered.contains("404") {
        return "Claude 模型不存在或路径错误。".to_string();
    }
    if lowered.contains("rate") || lowered.contains("429") {
        return "Claude 服务限流，请稍后重试。".to_string();
    }
    if lowered.contains("timeout") {
        return "Claude 请求超时，请稍后重试。".to_string();
    }
    format!("Claude 请求失败：{type_name}")
}

/// 未闭合的 tool_use 分片缓冲。
#[derive(Debug, Clone, Default, PartialEq)]
struct ToolUseBuffer {
    id: String,
    name: String,
    input_json: String,
}

/// 一次流的映射状态：`content_block_*` 的 tool_use 缓冲与结束原因。
///
/// 语义基准是 Python 的 `tool_buffers` 与局部变量 `finish_reason`；
/// `index` 直接读负载字段（Python 走 SDK 对象的 `event.index`，同源同值）。
#[derive(Debug, Clone)]
pub struct AnthropicStreamState {
    buffers: BTreeMap<u64, ToolUseBuffer>,
    finish_reason: String,
}

impl Default for AnthropicStreamState {
    fn default() -> Self {
        Self::new()
    }
}

impl AnthropicStreamState {
    pub fn new() -> Self {
        Self {
            buffers: BTreeMap::new(),
            finish_reason: "stop".to_string(),
        }
    }

    /// 一条 SSE 负载 → 内核事件；不认识的 `type` 一律忽略。
    pub fn handle_event(&mut self, event: &Value, events: &mut Vec<ModelStreamEvent>) {
        match event.get("type").and_then(Value::as_str) {
            Some("message_start") => {
                if let Some(usage) = event.get("message").and_then(usage_from_anthropic_payload) {
                    events.push(usage_event(usage));
                }
            }
            Some("content_block_start") => self.start_block(event, events),
            Some("content_block_delta") => self.block_delta(event, events),
            Some("content_block_stop") => {
                let index = event_index(event);
                if let Some(buffer) = self.buffers.remove(&index) {
                    if !buffer.name.is_empty() {
                        events.push(completed_event(&buffer));
                    }
                }
            }
            Some("message_delta") => {
                if let Some(reason) = event
                    .get("delta")
                    .and_then(|delta| delta.get("stop_reason"))
                    .and_then(Value::as_str)
                    .filter(|text| !text.is_empty())
                {
                    self.finish_reason = reason.to_string();
                }
                if let Some(usage) = usage_from_anthropic_payload(event) {
                    events.push(usage_event(usage));
                }
            }
            _ => {}
        }
    }

    fn start_block(&mut self, event: &Value, events: &mut Vec<ModelStreamEvent>) {
        let Some(block) = event.get("content_block") else {
            return;
        };
        if block.get("type").and_then(Value::as_str) != Some("tool_use") {
            return;
        }
        let index = event_index(event);
        let call_id = block
            .get("id")
            .filter(|value| is_truthy(value))
            .map(text_of)
            .unwrap_or_else(|| format!("toolu_{index}"));
        let name = block
            .get("name")
            .filter(|value| is_truthy(value))
            .map(text_of)
            .unwrap_or_default();
        self.buffers.insert(
            index,
            ToolUseBuffer {
                id: call_id.clone(),
                name: name.clone(),
                input_json: String::new(),
            },
        );
        if !name.is_empty() {
            events.push(ModelStreamEvent::ToolCallStarted(ToolCallStarted::new(
                call_id, name,
            )));
        }
    }

    fn block_delta(&mut self, event: &Value, events: &mut Vec<ModelStreamEvent>) {
        let Some(delta) = event.get("delta") else {
            return;
        };
        match delta.get("type").and_then(Value::as_str) {
            Some("text_delta") => {
                if let Some(text) = non_empty_str(delta.get("text")) {
                    events.push(ModelStreamEvent::TextDelta(TextDelta::new(text)));
                }
            }
            Some("thinking_delta") => {
                if let Some(text) = non_empty_str(delta.get("thinking")) {
                    events.push(ModelStreamEvent::ReasoningDelta(ReasoningDelta::new(text)));
                }
            }
            Some("input_json_delta") => {
                let index = event_index(event);
                if let Some(partial) = non_empty_str(delta.get("partial_json")) {
                    if let Some(buffer) = self.buffers.get_mut(&index) {
                        buffer.input_json.push_str(partial);
                    }
                }
            }
            _ => {}
        }
    }

    /// 流收尾：把没等到 `content_block_stop` 的工具调用按缓冲顺序补齐，再补结束事件。
    pub fn finish(&mut self, events: &mut Vec<ModelStreamEvent>) {
        for buffer in self.buffers.values() {
            if !buffer.name.is_empty() {
                events.push(completed_event(buffer));
            }
        }
        self.buffers.clear();
        events.push(ModelStreamEvent::Finished {
            finish_reason: self.finish_reason.clone(),
        });
    }
}

fn completed_event(buffer: &ToolUseBuffer) -> ModelStreamEvent {
    ModelStreamEvent::ToolCallCompleted(ToolCallCompleted::new(
        buffer.id.clone(),
        buffer.name.clone(),
        parse_arguments_object(&buffer.input_json),
    ))
}

fn usage_event(usage: omnicrawl_protocol::TokenUsage) -> ModelStreamEvent {
    ModelStreamEvent::UsageReported(UsageReported {
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        cached_input_tokens: usage.cached_input_tokens,
        reasoning_tokens: usage.reasoning_tokens,
    })
}

fn event_index(event: &Value) -> u64 {
    event.get("index").and_then(Value::as_u64).unwrap_or(0)
}

fn non_empty_str(value: Option<&Value>) -> Option<&str> {
    value
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
}
