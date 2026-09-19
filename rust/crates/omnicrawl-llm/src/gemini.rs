//! Google Gemini Generate Content Provider：请求构建、流事件映射与错误文案。
//!
//! 语义基准：`omnicrawl/llm/providers/gemini.py` 的 `_sanitize_options`、`_to_gemini_contents`、
//! `_build_generate_config`、`_emit_chunk_parts`、`_read_finish_reason` 与 `_format_gemini_error`。
//! 纯逻辑：不建连、不重试；HTTP 传输与能力门禁由 runtime 与调用方负责。

use std::collections::{BTreeMap, BTreeSet};

use omnicrawl_protocol::{
    parse_arguments_object, tools_from_conversation_messages, ConversationMessage, MessageBlock,
    ModelStreamEvent, Role, TextDelta, TokenUsage, ToolCallCompleted, ToolCallStarted, ToolSpec,
    UsageReported,
};
use serde_json::{json, Map, Value};

use crate::json::{dumps, is_truthy, text_of};
use crate::request::{ChatRequestInput, RequestError};
use crate::usage::usage_from_gemini_payload;

const OPTION_ALLOWLIST: &[&str] = &[
    "top_p",
    "top_k",
    "candidate_count",
    "stop_sequences",
    "response_mime_type",
    "safety_settings",
];

const FORBIDDEN_OPTIONS: &[&str] = &[
    "model",
    "contents",
    "tools",
    "system_instruction",
    "stream",
    "api_key",
    "timeout",
];

/// 生成参数在 MLDev REST 里的键名（`generationConfig` 内部）。
const GENERATION_CONFIG_KEYS: &[(&str, &str)] = &[
    ("temperature", "temperature"),
    ("top_p", "topP"),
    ("top_k", "topK"),
    ("candidate_count", "candidateCount"),
    ("max_output_tokens", "maxOutputTokens"),
    ("stop_sequences", "stopSequences"),
    ("response_mime_type", "responseMimeType"),
];

/// 组装好的 Gemini 请求。`contents` 与 `config` 是 SDK kwargs 的形态（snake_case），
/// 由 [`generate_content_body`] 翻成 MLDev REST 的线上请求体。
pub struct GeminiRequest {
    pub model: String,
    pub contents: Vec<Value>,
    pub config: Map<String, Value>,
    pub timeout_seconds: f64,
}

/// `provider_options` 校验：Host 字段不可覆盖，白名单之外一律拒绝。
pub fn sanitize_gemini_options(
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
                message: format!("Gemini provider_options 不支持字段：{key}"),
            });
        }
        result.insert(key.clone(), value.clone());
    }
    Ok(result)
}

/// 会话消息 → Gemini `contents`。
///
/// Gemini 没有「消息内 tools」概念：system 消息携带的动态声明已并入请求级 `tools`，
/// 因此这里跳过这类 system 消息。工具结果与紧随其后的用户内容同属一次工具观察，
/// 合并进同一个 user content（否则会出现无模型回合分隔的连续 user turns）。
pub fn to_gemini_contents(messages: &[ConversationMessage]) -> Vec<Value> {
    let mut contents: Vec<Value> = Vec::new();
    let mut call_names: BTreeMap<String, String> = BTreeMap::new();

    for message in messages {
        if message.role == Role::System && !message.tools.is_empty() {
            continue;
        }
        if message.role == Role::Tool {
            let mut parts: Vec<Value> = Vec::new();
            for block in &message.blocks {
                if let MessageBlock::ToolResult(result) = block {
                    let mut response = Map::new();
                    response.insert("result".to_string(), Value::String(result.content.clone()));
                    if !result.ok {
                        response.insert("error".to_string(), Value::String(result.content.clone()));
                    }
                    let name = call_names
                        .get(&result.call_id)
                        .cloned()
                        .unwrap_or_else(|| "tool".to_string());
                    parts.push(json!({
                        "function_response": {"name": name, "response": Value::Object(response)},
                    }));
                }
            }
            if !parts.is_empty() {
                contents.push(json!({"role": "user", "parts": parts}));
            }
            continue;
        }

        let role = if message.role == Role::Assistant {
            "model"
        } else {
            "user"
        };
        let mut parts: Vec<Value> = Vec::new();
        for block in &message.blocks {
            match block {
                MessageBlock::Text(text) if !text.text.is_empty() => {
                    parts.push(json!({"text": text.text}));
                }
                MessageBlock::Image(image) => {
                    parts.push(json!({
                        "inline_data": {
                            "mime_type": image.media_type,
                            "data": image.data_base64,
                        },
                    }));
                }
                MessageBlock::ToolCall(call) => {
                    if !call.call_id.is_empty() {
                        call_names.insert(call.call_id.clone(), call.name.clone());
                    }
                    let arguments = if call.arguments.is_empty() {
                        json!({})
                    } else {
                        Value::Object(call.arguments.clone())
                    };
                    parts.push(json!({
                        "function_call": {"name": call.name, "args": arguments},
                    }));
                }
                _ => {}
            }
        }
        if parts.is_empty() {
            let text = message.text();
            if !text.is_empty() {
                parts.push(json!({"text": text}));
            }
        }
        if parts.is_empty() {
            continue;
        }
        let merge_into_previous = role == "user" && contents.last().is_some_and(is_user_content);
        if merge_into_previous {
            if let Some(Value::Array(existing)) = contents
                .last_mut()
                .and_then(|content| content.get_mut("parts"))
            {
                existing.extend(parts);
            }
        } else {
            contents.push(json!({"role": role, "parts": parts}));
        }
    }
    contents
}

fn is_user_content(content: &Value) -> bool {
    content.get("role").and_then(Value::as_str) == Some("user")
}

/// 组装 Generate Content 请求（kwargs 形态，不含建连）。
pub fn build_generate_content_request(
    input: &ChatRequestInput<'_>,
) -> Result<GeminiRequest, RequestError> {
    let provider_options = sanitize_gemini_options(&input.options.provider_options)?;

    let mut config = Map::new();
    if !input.system_prompt.trim().is_empty() {
        config.insert(
            "system_instruction".to_string(),
            Value::String(input.system_prompt.to_string()),
        );
    }
    if let Some(max_output_tokens) = input.options.max_output_tokens.filter(|value| *value != 0) {
        config.insert("max_output_tokens".to_string(), json!(max_output_tokens));
    }
    if let Some(temperature) = input.options.temperature {
        config.insert("temperature".to_string(), json!(temperature));
    }

    // 请求级 tools = 顶层全局声明 + system 消息里携带的动态声明（合并后语义为全局可见）。
    let mut tools: Vec<ToolSpec> = input.tools.to_vec();
    tools.extend(tools_from_conversation_messages(input.messages));
    if !tools.is_empty() {
        let declarations: Vec<Value> = tools.iter().map(tool_declaration).collect();
        config.insert(
            "tools".to_string(),
            Value::Array(vec![json!({"function_declarations": declarations})]),
        );
        // 只声明 schema，不注册本地可执行对象：SDK 的自动函数执行必须关掉。
        config.insert(
            "automatic_function_calling".to_string(),
            json!({"disable": true}),
        );
    }
    for (key, value) in provider_options {
        config.insert(key, value);
    }

    Ok(GeminiRequest {
        model: input.model.to_string(),
        contents: to_gemini_contents(input.messages),
        config,
        timeout_seconds: input.profile_request_timeout_seconds,
    })
}

fn tool_declaration(tool: &ToolSpec) -> Value {
    let parameters = if tool.parameters.is_empty() {
        json!({"type": "object", "properties": {}})
    } else {
        Value::Object(tool.parameters.clone())
    };
    json!({
        "name": tool.name,
        "description": tool.description,
        "parameters": parameters,
    })
}

/// kwargs 形态 → MLDev `generateContent` 的线上请求体。
///
/// `systemInstruction` 与 `tools` 提升到顶层，生成参数收进 `generationConfig`；
/// `automatic_function_calling` 是 SDK 的本地字段，不上行。
pub fn generate_content_body(request: &GeminiRequest) -> Value {
    let mut body = Map::new();
    body.insert(
        "contents".to_string(),
        Value::Array(request.contents.clone()),
    );

    let mut generation_config = Map::new();
    for (source, target) in GENERATION_CONFIG_KEYS {
        if let Some(value) = request.config.get(*source) {
            generation_config.insert((*target).to_string(), value.clone());
        }
    }
    if let Some(system) = request.config.get("system_instruction") {
        body.insert(
            "systemInstruction".to_string(),
            json!({"parts": [{"text": text_of(system)}], "role": "user"}),
        );
    }
    if let Some(tools) = request.config.get("tools") {
        body.insert("tools".to_string(), tools_for_wire(tools));
    }
    if let Some(safety) = request.config.get("safety_settings") {
        body.insert("safetySettings".to_string(), safety.clone());
    }
    // SDK 只要拿到非空 config 就会建出 generationConfig（哪怕里面没有生成参数）。
    if !request.config.is_empty() || !generation_config.is_empty() {
        body.insert(
            "generationConfig".to_string(),
            Value::Object(generation_config),
        );
    }
    Value::Object(body)
}

/// MLDev 里的 Schema 是 proto 类型：`type` 必须是大写枚举名（`object` → `OBJECT`）。
///
/// 其余键名保持 JSON Schema 原样：SDK 还会做 `additionalProperties` → `additional_properties`
/// 这类改写，内核不复刻（见 README 的已知差异）。
fn tools_for_wire(tools: &Value) -> Value {
    let Value::Array(items) = tools else {
        return tools.clone();
    };
    Value::Array(
        items
            .iter()
            .map(|item| {
                let Value::Object(fields) = item else {
                    return item.clone();
                };
                let mut entry = Map::new();
                for (key, value) in fields {
                    let name = if key == "function_declarations" {
                        "functionDeclarations"
                    } else {
                        key.as_str()
                    };
                    entry.insert(name.to_string(), uppercase_schema_types(value));
                }
                Value::Object(entry)
            })
            .collect(),
    )
}

fn uppercase_schema_types(value: &Value) -> Value {
    match value {
        Value::Object(entries) => {
            let mut result = Map::new();
            for (key, item) in entries {
                if key == "type" {
                    if let Value::String(name) = item {
                        result.insert(key.clone(), Value::String(name.to_uppercase()));
                        continue;
                    }
                }
                result.insert(key.clone(), uppercase_schema_types(item));
            }
            Value::Object(result)
        }
        Value::Array(items) => Value::Array(items.iter().map(uppercase_schema_types).collect()),
        other => other.clone(),
    }
}

/// MLDev 路径里的模型名：SDK 会把 `gemini-x` 归一化成 `models/gemini-x`。
pub fn gemini_model_path(model_id: &str) -> String {
    if model_id.starts_with("models/") {
        model_id.to_string()
    } else {
        format!("models/{model_id}")
    }
}

/// Gemini 的请求失败文案；输入是 SDK 异常等价文本与异常类型名。
pub fn format_gemini_error(message: &str, type_name: &str) -> String {
    let lowered = message.trim().to_lowercase();
    if lowered.contains("api key") || lowered.contains("401") || lowered.contains("unauthenticated")
    {
        return "Gemini 鉴权失败。请检查 GEMINI_API_KEY。".to_string();
    }
    if lowered.contains("permission") || lowered.contains("403") {
        return "当前 API Key 没有访问该 Gemini 模型的权限。".to_string();
    }
    if lowered.contains("not found") || lowered.contains("404") {
        return "Gemini 模型不存在或路径错误。".to_string();
    }
    if lowered.contains("resource exhausted")
        || lowered.contains("429")
        || lowered.contains("quota")
    {
        return "Gemini 服务限流或额度不足，请稍后重试。".to_string();
    }
    if lowered.contains("timeout") {
        return "Gemini 请求超时，请稍后重试。".to_string();
    }
    format!("Gemini 请求失败：{type_name}")
}

/// 一次流的映射状态：已发出调用的内容指纹去重与结束原因。
///
/// Gemini 不保证稳定的 `call_id`，因此内核按「名称 + 参数」的内容指纹去重，
/// 再按发出次序给出 `gemini_{n}` 作为稳定 id（Host 侧仍会再规范化一次）。
#[derive(Debug, Clone)]
pub struct GeminiStreamState {
    emitted_calls: BTreeSet<String>,
    finish_reason: String,
}

impl Default for GeminiStreamState {
    fn default() -> Self {
        Self::new()
    }
}

impl GeminiStreamState {
    pub fn new() -> Self {
        Self {
            emitted_calls: BTreeSet::new(),
            finish_reason: "stop".to_string(),
        }
    }

    /// 一条分片负载 → 内核事件。顺序与 Python 一致：用量 → 分片内容 → 结束原因。
    pub fn handle_chunk(&mut self, chunk: &Value, events: &mut Vec<ModelStreamEvent>) {
        if let Some(usage) = usage_from_gemini_payload(chunk) {
            events.push(usage_event(usage));
        }
        self.emit_parts(chunk, events);
        let reason = read_finish_reason(chunk);
        if !reason.is_empty() {
            self.finish_reason = reason;
        }
    }

    /// 流收尾：补结束事件（进程主据此归并回复）。
    pub fn finish(&mut self, events: &mut Vec<ModelStreamEvent>) {
        events.push(ModelStreamEvent::Finished {
            finish_reason: self.finish_reason.clone(),
        });
    }

    fn emit_parts(&mut self, chunk: &Value, events: &mut Vec<ModelStreamEvent>) {
        let Some(candidates) = chunk
            .get("candidates")
            .filter(|value| is_truthy(value))
            .and_then(Value::as_array)
        else {
            // 兼容 `chunk.text` 形态：没有 candidates 时按纯文本增量处理。
            if let Some(text) = non_empty_str(chunk.get("text")) {
                events.push(ModelStreamEvent::TextDelta(TextDelta::new(text)));
            }
            return;
        };

        for candidate in candidates {
            let Some(content) = candidate.get("content").filter(|value| is_truthy(value)) else {
                continue;
            };
            let Some(parts) = content
                .get("parts")
                .filter(|value| is_truthy(value))
                .and_then(Value::as_array)
            else {
                continue;
            };
            for part in parts {
                if let Some(text) = non_empty_str(part.get("text")) {
                    events.push(ModelStreamEvent::TextDelta(TextDelta::new(text)));
                    continue;
                }
                let function_call = part
                    .get("function_call")
                    .filter(|value| is_truthy(value))
                    .or_else(|| part.get("functionCall").filter(|value| is_truthy(value)));
                let Some(function_call) = function_call else {
                    continue;
                };
                self.emit_function_call(function_call, events);
            }
        }
    }

    fn emit_function_call(&mut self, function_call: &Value, events: &mut Vec<ModelStreamEvent>) {
        let name = function_call
            .get("name")
            .filter(|value| is_truthy(value))
            .map(text_of)
            .unwrap_or_default();
        let mut arguments = function_call
            .get("args")
            .filter(|value| is_truthy(value))
            .cloned()
            .unwrap_or_else(|| json!({}));
        if let Value::String(raw) = &arguments {
            arguments = Value::Object(parse_arguments_object(raw));
        }
        if !arguments.is_object() {
            arguments = json!({});
        }

        let fingerprint = format!("{name}:{}", dumps(&sorted_value(&arguments)));
        if name.is_empty() || self.emitted_calls.contains(&fingerprint) {
            return;
        }
        self.emitted_calls.insert(fingerprint);

        let call_id = format!("gemini_{}", self.emitted_calls.len());
        events.push(ModelStreamEvent::ToolCallStarted(ToolCallStarted::new(
            call_id.clone(),
            name.clone(),
        )));
        let arguments = match arguments {
            Value::Object(entries) => entries,
            _ => Map::new(),
        };
        events.push(ModelStreamEvent::ToolCallCompleted(ToolCallCompleted::new(
            call_id, name, arguments,
        )));
    }
}

fn usage_event(usage: TokenUsage) -> ModelStreamEvent {
    ModelStreamEvent::UsageReported(UsageReported {
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        cached_input_tokens: usage.cached_input_tokens,
        reasoning_tokens: usage.reasoning_tokens,
    })
}

/// Python 的去重键用 `json.dumps(..., sort_keys=True)`：键序必须按字典序。
fn sorted_value(value: &Value) -> Value {
    match value {
        Value::Object(entries) => {
            let mut sorted: Vec<(&String, Value)> = entries
                .iter()
                .map(|(key, item)| (key, sorted_value(item)))
                .collect();
            sorted.sort_by(|left, right| left.0.cmp(right.0));
            Value::Object(
                sorted
                    .into_iter()
                    .map(|(key, item)| (key.clone(), item))
                    .collect(),
            )
        }
        Value::Array(items) => Value::Array(items.iter().map(sorted_value).collect()),
        other => other.clone(),
    }
}

fn read_finish_reason(chunk: &Value) -> String {
    let Some(first) = chunk
        .get("candidates")
        .filter(|value| is_truthy(value))
        .and_then(Value::as_array)
        .and_then(|candidates| candidates.first())
    else {
        return String::new();
    };
    first
        .get("finish_reason")
        .filter(|value| is_truthy(value))
        .or_else(|| first.get("finishReason").filter(|value| is_truthy(value)))
        .map(text_of)
        .unwrap_or_default()
}

fn non_empty_str(value: Option<&Value>) -> Option<&str> {
    value
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
}
