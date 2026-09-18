//! OpenAI Responses 协议的请求构建。
//!
//! 语义基准是 `omnicrawl/llm/providers/openai_responses.py`：会话消息 → `input` items、
//! 请求级 `tools`（Responses 没有「消息内 tools」，动态声明合并为全局可见）、
//! 以及 `create()` 参数到**线上请求体**的摊平（`extra_body` 是 SDK 参数，SDK 会把它并进请求体顶层）。
//!
//! 未搬：流事件映射（`_stream_turn_events` 的 item 别名与参数分片）、回合运行（`stream_turn`）、
//! `_create_stream_with_retries` 的降级重试编排、`discover_models`。

use omnicrawl_protocol::{
    parse_arguments_object, tools_from_conversation_messages, ConversationMessage, MessageBlock,
    ModelStreamEvent, ReasoningDelta, Role, TextDelta, ToolCallCompleted, ToolCallStarted,
    ToolSpec, UsageReported,
};
use serde_json::{json, Map, Value};

use crate::errors::RuntimeError;
use crate::json::dumps;
use crate::openai_chat::arguments_json_complete;
use crate::request::{
    build_prompt_cache_key, sanitize_provider_options, ChatRequest, ChatRequestInput, RequestError,
};
use crate::usage::usage_from_openai_payload;

/// 会话消息 → `input` items（Python `messages_to_responses_input`）。
pub fn messages_to_responses_input(messages: &[ConversationMessage]) -> Vec<Value> {
    let mut items: Vec<Value> = Vec::new();
    for message in messages {
        // system 消息携带的动态声明已合并进请求级 tools，不再作为输入 item 下发。
        if message.role == Role::System && !message.tools.is_empty() {
            continue;
        }
        if message.role == Role::Tool {
            for block in &message.blocks {
                if let MessageBlock::ToolResult(result) = block {
                    items.push(json!({
                        "type": "function_call_output",
                        "call_id": result.call_id,
                        "output": result.content,
                    }));
                }
            }
            continue;
        }
        if message.role == Role::Assistant {
            let mut text_parts: Vec<&str> = Vec::new();
            let mut tool_call_items: Vec<Value> = Vec::new();
            for block in &message.blocks {
                match block {
                    MessageBlock::Text(text) => text_parts.push(text.text.as_str()),
                    MessageBlock::ToolCall(call) => {
                        let call_id = if call.call_id.is_empty() {
                            call.provider_call_id.clone()
                        } else {
                            call.call_id.clone()
                        };
                        tool_call_items.push(json!({
                            "type": "function_call",
                            "call_id": call_id,
                            "name": call.name,
                            "arguments": dumps(&Value::Object(call.arguments.clone())),
                        }));
                    }
                    _ => {}
                }
            }
            let text = text_parts.join("");
            if !text.is_empty() {
                items.push(json!({
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": text}],
                }));
            }
            // 带工具调用历史时必须回传 reasoning item（上游会校验存在性，不校验内容），
            // 没有工具调用时反而不能带。
            if !tool_call_items.is_empty() {
                let reasoning_text = if message.reasoning.is_empty() {
                    "…".to_string()
                } else {
                    message.reasoning.clone()
                };
                items.push(json!({
                    "type": "reasoning",
                    "id": format!("rs_{}", &sha1_hex(&reasoning_text)[..16]),
                    "summary": [{"type": "summary_text", "text": reasoning_text}],
                }));
            }
            items.extend(tool_call_items);
            continue;
        }
        let mut content: Vec<Value> = Vec::new();
        for block in &message.blocks {
            match block {
                MessageBlock::Text(text) if !text.text.is_empty() => {
                    content.push(json!({"type": "input_text", "text": text.text}));
                }
                MessageBlock::Image(image) => {
                    content.push(json!({
                        "type": "input_image",
                        "image_url": image.data_url(),
                        "detail": image.detail.as_str(),
                    }));
                }
                _ => {}
            }
        }
        if content.is_empty() {
            content.push(json!({"type": "input_text", "text": ""}));
        }
        let role = match message.role {
            Role::User | Role::System => message.role.as_str(),
            _ => "user",
        };
        items.push(json!({"role": role, "content": content}));
    }
    items
}

/// 请求级工具声明（Python `_tools_for_responses`）：请求 tools 在前，消息携带的声明在后。
pub fn tools_for_responses(input: &ChatRequestInput<'_>) -> Vec<Value> {
    let mut all: Vec<ToolSpec> = input.tools.to_vec();
    all.extend(tools_from_conversation_messages(input.messages));
    all.iter()
        .map(|tool| {
            let parameters = if tool.parameters.is_empty() {
                json!({"type": "object", "properties": {}})
            } else {
                Value::Object(tool.parameters.clone())
            };
            json!({
                "type": "function",
                "name": tool.name,
                "description": tool.description,
                "parameters": parameters,
            })
        })
        .collect()
}

/// 工具调用历史是否包含 Responses 专用的 history item（Python `_has_tool_history_items`）。
pub fn has_tool_history_items(items: &[Value]) -> bool {
    items.iter().any(|item| {
        matches!(
            item.get("type").and_then(Value::as_str),
            Some("function_call") | Some("function_call_output")
        )
    })
}

/// 兼容网关对「工具调用历史」返回 400 的判定（Python `_is_tool_history_rejection`）：
/// 请求确含工具历史 + 状态码 400，避免误伤其他 400。
pub fn is_tool_history_rejection(items: &[Value], status_code: Option<u16>) -> bool {
    has_tool_history_items(items) && status_code == Some(400)
}

/// 把工具调用历史展平为纯文本，保持消息顺序与上下文语义（Python `_flatten_tool_history_to_text`）。
///
/// `function_call` 追加到最近一条 assistant 文本之后（中间可能隔着 reasoning item），
/// `function_call_output` 作为独立 user 消息紧跟其后——不并入可能在前的普通 user 指令。
pub fn flatten_tool_history_to_text(items: &[Value]) -> Vec<Value> {
    let mut flat: Vec<Value> = Vec::new();
    for item in items {
        match item.get("type").and_then(Value::as_str) {
            Some("function_call") => {
                let text = format!(
                    "[工具调用: {}({})]",
                    item.get("name").and_then(Value::as_str).unwrap_or_default(),
                    item.get("arguments")
                        .and_then(Value::as_str)
                        .unwrap_or_default(),
                );
                let target = flat.iter_mut().rev().find(|message| {
                    message.get("role").and_then(Value::as_str) == Some("assistant")
                });
                match target {
                    Some(message) => append_message_text(message, &text, "output_text"),
                    None => flat.push(text_message("assistant", &text, "output_text")),
                }
            }
            Some("function_call_output") => {
                let text = format!(
                    "[工具结果: {}]",
                    item.get("output")
                        .and_then(Value::as_str)
                        .unwrap_or_default(),
                );
                flat.push(text_message("user", &text, "input_text"));
            }
            _ => flat.push(item.clone()),
        }
    }
    flat
}

fn text_message(role: &str, text: &str, block_type: &str) -> Value {
    json!({"role": role, "content": [{"type": block_type, "text": text}]})
}

/// 同类文本块追加到尾部（用换行隔开），否则整段替换 content。
fn append_message_text(message: &mut Value, text: &str, block_type: &str) {
    let matched = message
        .get("content")
        .and_then(Value::as_array)
        .and_then(|content| content.first())
        .and_then(|first| first.get("type"))
        .and_then(Value::as_str)
        == Some(block_type);
    if matched {
        if let Some(first) = message
            .get_mut("content")
            .and_then(Value::as_array_mut)
            .and_then(|content| content.first_mut())
        {
            let existing = first
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or_default();
            first["text"] = json!(format!("{existing}\n{text}"));
        }
        return;
    }
    message["content"] = json!([{"type": block_type, "text": text}]);
}

/// `create()` 参数 → 线上请求体（Python `_build_responses_kwargs`）。
pub fn build_responses_request(input: &ChatRequestInput<'_>) -> Result<ChatRequest, RequestError> {
    let options = input.options;
    let mut extra_body = sanitize_provider_options(&options.provider_options)?;
    let effort = if options.reasoning_effort.is_empty()
        || options.reasoning_effort == "none"
        || options.reasoning_effort == "disabled"
    {
        "none".to_string()
    } else {
        options.reasoning_effort.clone()
    };
    extra_body.remove("thinking");
    extra_body.remove("reasoning_effort");
    if !extra_body.contains_key("reasoning") {
        extra_body.insert("reasoning".to_string(), json!({"effort": effort}));
    }

    let tools = tools_for_responses(input);
    let input_items = messages_to_responses_input(input.messages);

    let mut body = Map::new();
    body.insert("model".to_string(), json!(input.model));
    body.insert("instructions".to_string(), json!(input.system_prompt));
    body.insert("input".to_string(), Value::Array(input_items));
    body.insert("stream".to_string(), Value::Bool(true));
    // `extra_body` 是 SDK 参数：SDK 会把它里面的键并进请求体顶层，这里照线上形态摊平。
    for (key, value) in extra_body {
        body.insert(key, value);
    }
    if !tools.is_empty() {
        body.insert("tools".to_string(), Value::Array(tools));
        if !options.tool_choice.is_empty() {
            body.insert("tool_choice".to_string(), json!(options.tool_choice));
        }
    }
    if let Some(max_output_tokens) = options.max_output_tokens {
        if max_output_tokens != 0 {
            body.insert("max_output_tokens".to_string(), json!(max_output_tokens));
        }
    }
    if let Some(temperature) = options.temperature {
        body.insert("temperature".to_string(), json!(temperature));
    }
    let prompt_cache_key = build_prompt_cache_key(input.prompt_cache_identity, input.model);
    if !prompt_cache_key.is_empty() {
        body.insert("prompt_cache_key".to_string(), json!(prompt_cache_key));
    }

    let timeout_seconds = if options.request_timeout_seconds != 0.0 {
        options.request_timeout_seconds
    } else {
        input.profile_request_timeout_seconds
    };
    Ok(ChatRequest {
        body: Value::Object(body),
        timeout_seconds,
    })
}

/// 一条工具调用的流式缓冲（Python 侧是 `dict[str, dict[str, str]]` 的值）。
#[derive(Debug, Clone, Default)]
struct CallBuffer {
    name: String,
    arguments: String,
}

/// Python `_arguments_json_complete` 的字符串入口：空串视为完整，非空必须能解析成 JSON。
///
/// 半截 JSON 不能当正常调用收尾，否则会被静默降级成空参数误执行。
fn arguments_text_complete(raw: &str) -> bool {
    arguments_json_complete(&Value::String(raw.to_string()))
}

/// Responses 流事件的状态机（Python `_stream_turn_events` 的循环体与收尾）。
///
/// 每个事件负载按 `type` 分流：文本 / 推理增量直接外发；工具调用只累进缓冲，
/// 直到 `arguments.done`、`output_item.done` 或 `response.completed` 才产出完成事件——
/// 参数分片不对外发增量（与 Chat Completions 的路径不同）。
#[derive(Debug, Default)]
pub struct ResponsesStreamState {
    /// 插入序即冲刷顺序，用 Vec 而不是映射（工具调用数量很小）。
    call_buffers: Vec<(String, CallBuffer)>,
    call_id_aliases: std::collections::BTreeMap<String, String>,
    emitted_call_ids: std::collections::BTreeSet<String>,
    started_call_ids: std::collections::BTreeSet<String>,
    finish_reason: String,
    stream_completed_seen: bool,
    output_text_delta_seen: bool,
}

impl ResponsesStreamState {
    pub fn new() -> Self {
        Self {
            finish_reason: "stop".to_string(),
            ..Self::default()
        }
    }

    /// 处理一个事件负载（Python 循环体）。
    pub fn handle_event(&mut self, event: &Value, events: &mut Vec<ModelStreamEvent>) {
        if let Some(usage) = usage_from_openai_payload(event) {
            events.push(ModelStreamEvent::UsageReported(UsageReported {
                input_tokens: usage.input_tokens,
                output_tokens: usage.output_tokens,
                cached_input_tokens: usage.cached_input_tokens,
                reasoning_tokens: usage.reasoning_tokens,
            }));
        }

        let event_type = event
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let delta = event.get("delta").and_then(Value::as_str);
        match event_type {
            "response.output_text.delta" => {
                if let Some(text) = delta {
                    self.output_text_delta_seen = true;
                    events.push(ModelStreamEvent::TextDelta(TextDelta {
                        text: text.to_string(),
                    }));
                }
            }
            "response.reasoning_text.delta" | "response.reasoning_summary_text.delta" => {
                if let Some(text) = delta {
                    events.push(ModelStreamEvent::ReasoningDelta(ReasoningDelta {
                        text: text.to_string(),
                    }));
                }
            }
            // 部分兼容网关只在 added 事件里携带函数名，之后直接给参数 delta；
            // 忽略它会让完整工具调用被误判成「名称截断」。
            "response.output_item.added" => {
                self.capture_function_call_added(event.get("item"), events);
            }
            "response.function_call_arguments.delta" => {
                let item_id = event
                    .get("item_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let provider_call_id = event
                    .get("call_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let call_id = self.canonical_call_id(&item_id, &provider_call_id);
                if call_id.is_empty() {
                    return;
                }
                let Some(text) = delta else {
                    return;
                };
                if !self.call_buffers.iter().any(|(id, _)| *id == call_id) {
                    self.call_buffers
                        .push((call_id.clone(), CallBuffer::default()));
                }
                let name_from_event = event
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let started = self.started_call_ids.contains(&call_id);
                if let Some((_, buffer)) =
                    self.call_buffers.iter_mut().find(|(id, _)| *id == call_id)
                {
                    if buffer.name.is_empty() && !name_from_event.is_empty() {
                        buffer.name = name_from_event;
                    }
                    if !buffer.name.is_empty() && !started {
                        self.started_call_ids.insert(call_id.clone());
                        events.push(ModelStreamEvent::ToolCallStarted(ToolCallStarted {
                            call_id: call_id.clone(),
                            name: buffer.name.clone(),
                        }));
                    }
                    buffer.arguments.push_str(text);
                }
            }
            // 标准事件在该事件顶层携带 name + 最终 arguments，且没有 item 字段。
            "response.function_call_arguments.done" => {
                if let Some(item) = event.get("item").filter(|value| !value.is_null()) {
                    self.emit_function_call_item(item, events);
                    return;
                }
                let item_id = event
                    .get("item_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let provider_call_id = event
                    .get("call_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let call_id = self.canonical_call_id(&item_id, &provider_call_id);
                let name = event
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .trim()
                    .to_string();
                let arguments = event
                    .get("arguments")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                if call_id.is_empty() {
                    return;
                }
                if !self.call_buffers.iter().any(|(id, _)| *id == call_id) {
                    self.call_buffers
                        .push((call_id.clone(), CallBuffer::default()));
                }
                let started = self.started_call_ids.contains(&call_id);
                let emitted = self.emitted_call_ids.contains(&call_id);
                if let Some((_, buffer)) =
                    self.call_buffers.iter_mut().find(|(id, _)| *id == call_id)
                {
                    if !name.is_empty() {
                        if buffer.name.is_empty() && !started {
                            self.started_call_ids.insert(call_id.clone());
                            events.push(ModelStreamEvent::ToolCallStarted(ToolCallStarted {
                                call_id: call_id.clone(),
                                name: name.clone(),
                            }));
                        }
                        buffer.name = name.clone();
                    }
                    if !arguments.is_empty() {
                        buffer.arguments = arguments.clone();
                    }
                    let final_name = if name.is_empty() {
                        buffer.name.clone()
                    } else {
                        name.clone()
                    };
                    let final_args = if arguments.is_empty() {
                        buffer.arguments.clone()
                    } else {
                        arguments.clone()
                    };
                    if !final_name.is_empty() && !emitted && arguments_text_complete(&final_args) {
                        events.push(ModelStreamEvent::ToolCallCompleted(ToolCallCompleted {
                            call_id: call_id.clone(),
                            name: final_name,
                            arguments: parse_arguments_object(&final_args),
                        }));
                        self.emitted_call_ids.insert(call_id.clone());
                        // 该事件已宣告调用完整：清掉缓冲，允许网关随后 EOF 丢 completed 时照样执行。
                        self.call_buffers.retain(|(id, _)| *id != call_id);
                    }
                }
            }
            "response.output_item.done" => {
                if let Some(item) = event.get("item").filter(|value| !value.is_null()) {
                    self.emit_function_call_item(item, events);
                }
            }
            "response.completed" => {
                self.stream_completed_seen = true;
                let response = event.get("response").filter(|value| !value.is_null());
                if let Some(status) = response
                    .and_then(|value| value.get("status"))
                    .and_then(Value::as_str)
                {
                    if !status.is_empty() {
                        self.finish_reason = status.to_string();
                    }
                }
                let output = response
                    .and_then(|value| value.get("output"))
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                for item in &output {
                    self.emit_function_call_item(item, events);
                }
            }
            _ => {}
        }
    }

    /// 流结束后的收尾（Python 循环之后的截断判定、缓冲冲刷与 `ResponseCompleted`）。
    ///
    /// 降级告警（`prompt_cache_unsupported` / `tool_history_flattened`）属于重试编排，不在这里发。
    pub fn finish(&mut self, events: &mut Vec<ModelStreamEvent>) -> Result<(), RuntimeError> {
        let complete_buffered_calls = !self.call_buffers.is_empty()
            && self.call_buffers.iter().all(|(_, buffer)| {
                !buffer.name.is_empty() && arguments_text_complete(&buffer.arguments)
            });
        let eof_has_complete_output = (self.call_buffers.is_empty()
            && (self.output_text_delta_seen || !self.emitted_call_ids.is_empty()))
            || complete_buffered_calls;
        if !self.stream_completed_seen && !eof_has_complete_output {
            return Err(RuntimeError::stream_interrupted(
                "Responses 流在收到 response.completed 前提前耗尽，疑似连接被网关截断。",
            ));
        }
        for (call_id, buffer) in std::mem::take(&mut self.call_buffers) {
            if self.emitted_call_ids.contains(&call_id) {
                continue;
            }
            if buffer.name.is_empty() {
                return Err(RuntimeError::stream_interrupted(
                    "Responses 流在工具调用名称完整到达前结束，疑似连接被网关截断。",
                ));
            }
            if !arguments_text_complete(&buffer.arguments) {
                return Err(RuntimeError::stream_interrupted(
                    "Responses 流在工具调用参数完整到达前结束，疑似连接被网关截断。",
                ));
            }
            events.push(ModelStreamEvent::ToolCallCompleted(ToolCallCompleted {
                call_id: call_id.clone(),
                name: buffer.name,
                arguments: parse_arguments_object(&buffer.arguments),
            }));
            self.emitted_call_ids.insert(call_id);
        }
        events.push(ModelStreamEvent::Finished {
            finish_reason: self.finish_reason.clone(),
        });
        Ok(())
    }

    /// 统一 `item_id` 与 Provider `call_id` 的别名（Python `_canonical_call_id`）。
    fn canonical_call_id(&mut self, item_id: &str, provider_call_id: &str) -> String {
        let item_id = item_id.to_string();
        let provider_call_id = provider_call_id.to_string();
        if !item_id.is_empty() && !provider_call_id.is_empty() && item_id != provider_call_id {
            self.call_id_aliases
                .insert(item_id.clone(), provider_call_id.clone());
            if let Some(index) = self.call_buffers.iter().position(|(id, _)| *id == item_id) {
                let (_, existing) = self.call_buffers.remove(index);
                match self
                    .call_buffers
                    .iter_mut()
                    .find(|(id, _)| *id == provider_call_id)
                {
                    Some((_, current)) => {
                        if current.name.is_empty() {
                            current.name = existing.name;
                        }
                        if current.arguments.is_empty() {
                            current.arguments = existing.arguments;
                        }
                    }
                    None => self.call_buffers.push((provider_call_id.clone(), existing)),
                }
            }
            if self.started_call_ids.contains(&item_id) {
                self.started_call_ids.remove(&item_id);
                self.started_call_ids.insert(provider_call_id.clone());
            }
            return provider_call_id;
        }
        let candidate = if provider_call_id.is_empty() {
            item_id
        } else {
            provider_call_id
        };
        self.call_id_aliases
            .get(&candidate)
            .cloned()
            .unwrap_or(candidate)
    }

    /// 从 `output_item.added` 里抓函数名并开缓冲（Python `_capture_function_call_added`）。
    fn capture_function_call_added(
        &mut self,
        item: Option<&Value>,
        events: &mut Vec<ModelStreamEvent>,
    ) {
        let Some(item) = item.filter(|value| value.is_object()) else {
            return;
        };
        if item.get("type").and_then(Value::as_str) != Some("function_call") {
            return;
        }
        let call_id = item
            .get("id")
            .or_else(|| item.get("call_id"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let name = item
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_string();
        if call_id.is_empty() || name.is_empty() {
            return;
        }
        if !self.call_buffers.iter().any(|(id, _)| *id == call_id) {
            self.call_buffers
                .push((call_id.clone(), CallBuffer::default()));
        }
        let started = self.started_call_ids.contains(&call_id);
        if let Some((_, buffer)) = self.call_buffers.iter_mut().find(|(id, _)| *id == call_id) {
            if buffer.name.is_empty() {
                buffer.name = name;
            }
            let arguments = item
                .get("arguments")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            if !arguments.is_empty() && buffer.arguments.is_empty() {
                buffer.arguments = arguments;
            }
            if !started {
                self.started_call_ids.insert(call_id.clone());
                events.push(ModelStreamEvent::ToolCallStarted(ToolCallStarted {
                    call_id,
                    name: buffer.name.clone(),
                }));
            }
        }
    }

    /// 从完整 item 产出工具调用（Python `_emit_function_call_item`）。
    fn emit_function_call_item(&mut self, item: &Value, events: &mut Vec<ModelStreamEvent>) {
        if item.get("type").and_then(Value::as_str) != Some("function_call") {
            return;
        }
        let raw_call_id = item
            .get("call_id")
            .or_else(|| item.get("id"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let call_id = self.canonical_call_id(&raw_call_id, "");
        let name = item
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let arguments = item
            .get("arguments")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if call_id.is_empty() || name.is_empty() || self.emitted_call_ids.contains(&call_id) {
            return;
        }
        self.call_buffers.retain(|(id, _)| *id != call_id);
        if !self.started_call_ids.contains(&call_id) {
            self.started_call_ids.insert(call_id.clone());
            events.push(ModelStreamEvent::ToolCallStarted(ToolCallStarted {
                call_id: call_id.clone(),
                name: name.clone(),
            }));
        }
        events.push(ModelStreamEvent::ToolCallCompleted(ToolCallCompleted {
            call_id: call_id.clone(),
            name,
            arguments: parse_arguments_object(&arguments),
        }));
        self.emitted_call_ids.insert(call_id);
    }
}

/// SHA-1 十六进制摘要：Python 侧用 `hashlib.sha1` 生成 reasoning item 的 id。
///
/// 固定算法，手写实现——项目不为单个哈希再引入一个依赖。
fn sha1_hex(text: &str) -> String {
    let mut state: [u32; 5] = [
        0x6745_2301,
        0xEFCD_AB89,
        0x98BA_DCFE,
        0x1032_5476,
        0xC3D2_E1F0,
    ];
    let mut message = text.as_bytes().to_vec();
    let bit_length = (message.len() as u64) * 8;
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&bit_length.to_be_bytes());

    for chunk in message.chunks(64) {
        let mut words = [0u32; 80];
        for (index, word) in words.iter_mut().take(16).enumerate() {
            *word = u32::from_be_bytes([
                chunk[index * 4],
                chunk[index * 4 + 1],
                chunk[index * 4 + 2],
                chunk[index * 4 + 3],
            ]);
        }
        for index in 16..80 {
            words[index] =
                (words[index - 3] ^ words[index - 8] ^ words[index - 14] ^ words[index - 16])
                    .rotate_left(1);
        }
        let (mut a, mut b, mut c, mut d, mut e) =
            (state[0], state[1], state[2], state[3], state[4]);
        for (index, word) in words.iter().enumerate() {
            let (f, k) = match index {
                0..=19 => ((b & c) | ((!b) & d), 0x5A82_7999),
                20..=39 => (b ^ c ^ d, 0x6ED9_EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1B_BCDC),
                _ => (b ^ c ^ d, 0xCA62_C1D6),
            };
            let temp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(*word);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = temp;
        }
        state[0] = state[0].wrapping_add(a);
        state[1] = state[1].wrapping_add(b);
        state[2] = state[2].wrapping_add(c);
        state[3] = state[3].wrapping_add(d);
        state[4] = state[4].wrapping_add(e);
    }
    state
        .iter()
        .map(|word| format!("{word:08x}"))
        .collect::<String>()
}
