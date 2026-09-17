//! OpenAI Chat 请求构建：会话消息 → 请求体、工具声明、provider_options 校验、prompt_cache_key。
//!
//! 语义基准：`omnicrawl/llm/providers/openai_chat.py` 的 `_to_openai_messages` 与
//! `_stream_turn_events` 的请求组装，以及 `omnicrawl/llm/providers/openai_common.py` 的
//! `tool_specs_to_openai_functions` / `sanitize_provider_options` / `build_prompt_cache_key`。
//! 全部为纯逻辑：不建连、不重试，HTTP 传输与能力门禁由调用方负责。

use std::collections::{BTreeMap, BTreeSet};
use std::io;

use omnicrawl_protocol::{ConversationMessage, GenerationOptions, MessageBlock, Role, ToolSpec};
use serde::Serialize;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

/// 请求构建失败；对应 Python 侧 `ModelError(code=CONFIGURATION_ERROR, ...)`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestError {
    pub message: String,
}

impl RequestError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl std::fmt::Display for RequestError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for RequestError {}

const FORBIDDEN_PROVIDER_OPTION_KEYS: &[&str] = &[
    "model",
    "messages",
    "input",
    "tools",
    "tool_choice",
    "stream",
    "timeout",
    "api_key",
    "base_url",
    "system",
];

const OPENAI_PROVIDER_OPTION_ALLOWLIST: &[&str] = &[
    "thinking",
    "reasoning_effort",
    "top_p",
    "presence_penalty",
    "frequency_penalty",
    "logit_bias",
    "user",
    "seed",
    "response_format",
    "metadata",
    "store",
    "service_tier",
];

/// GPT 系列判定：`gpt-` / `chatgpt-` 前缀，或 `o` + 数字；大小写敏感。
pub fn is_openai_gpt_model(model: &str) -> bool {
    if model.starts_with("gpt-") || model.starts_with("chatgpt-") {
        return true;
    }
    let mut chars = model.chars();
    matches!(chars.next(), Some('o')) && chars.next().is_some_and(|c| c.is_ascii_digit())
}

/// 把统一 ToolSpec 转成 Chat Completions 的 functions 声明；空 parameters 补成空对象模式。
pub fn tool_specs_to_openai_functions(tools: &[ToolSpec]) -> Vec<Value> {
    tools
        .iter()
        .map(|tool| {
            let parameters = if tool.parameters.is_empty() {
                json!({"type": "object", "properties": {}})
            } else {
                Value::Object(tool.parameters.clone())
            };
            json!({
                "type": "function",
                "function": {
                    "name": tool.name,
                    "description": tool.description,
                    "parameters": parameters,
                }
            })
        })
        .collect()
}

/// 校验并收敛 provider_options：禁止覆盖 Host 字段，未知字段直接拒绝。
pub fn sanitize_provider_options(
    options: &Map<String, Value>,
) -> Result<Map<String, Value>, RequestError> {
    let mut result = Map::new();
    for (key, value) in options {
        if FORBIDDEN_PROVIDER_OPTION_KEYS.contains(&key.as_str()) {
            return Err(RequestError::new(format!(
                "provider_options 不允许覆盖 Host 字段：{key}"
            )));
        }
        if !OPENAI_PROVIDER_OPTION_ALLOWLIST.contains(&key.as_str()) {
            return Err(RequestError::new(format!(
                "OpenAI provider_options 不支持字段：{key}"
            )));
        }
        result.insert(key.clone(), value.clone());
    }
    Ok(result)
}

/// 会话消息 → Chat Completions messages。
///
/// system 提示词非空才下发；动态工具声明按工具名跨消息去重；assistant 的 tool_calls 消息
/// 始终带 `reasoning_content`（上游只校验字段存在性）；视觉图片只走 user 观察消息。
pub fn to_openai_messages(system_prompt: &str, messages: &[ConversationMessage]) -> Vec<Value> {
    let mut result: Vec<Value> = Vec::new();
    if !system_prompt.trim().is_empty() {
        result.push(json!({"role": "system", "content": system_prompt}));
    }

    let mut seen_dynamic_tools: BTreeSet<String> = BTreeSet::new();
    for message in messages {
        match message.role {
            Role::Tool => {
                for block in &message.blocks {
                    if let MessageBlock::ToolResult(tool_result) = block {
                        result.push(json!({
                            "role": "tool",
                            "tool_call_id": tool_result.call_id,
                            "content": tool_result.content,
                        }));
                    }
                }
                continue;
            }
            Role::Assistant => {
                let mut content = String::new();
                let mut tool_calls: Vec<Value> = Vec::new();
                for block in &message.blocks {
                    match block {
                        MessageBlock::Text(text) => content.push_str(&text.text),
                        MessageBlock::ToolCall(call) => {
                            let call_id = if call.call_id.is_empty() {
                                call.provider_call_id.as_str()
                            } else {
                                call.call_id.as_str()
                            };
                            tool_calls.push(json!({
                                "id": call_id,
                                "type": "function",
                                "function": {
                                    "name": call.name,
                                    "arguments": python_json_dumps(&Value::Object(call.arguments.clone())),
                                },
                            }));
                        }
                        _ => {}
                    }
                }

                let mut payload = Map::new();
                payload.insert("role".to_string(), json!("assistant"));
                payload.insert(
                    "content".to_string(),
                    if content.is_empty() {
                        Value::Null
                    } else {
                        json!(content)
                    },
                );
                if !tool_calls.is_empty() {
                    payload.insert("reasoning_content".to_string(), json!(message.reasoning));
                    payload.insert("tool_calls".to_string(), Value::Array(tool_calls));
                } else if !message.reasoning.is_empty() {
                    payload.insert("reasoning_content".to_string(), json!(message.reasoning));
                }
                result.push(Value::Object(payload));
                continue;
            }
            _ => {}
        }

        if message.role == Role::System && !message.tools.is_empty() {
            let fresh_tools: Vec<ToolSpec> = message
                .tools
                .iter()
                .filter(|tool| !seen_dynamic_tools.contains(&tool.name))
                .cloned()
                .collect();
            if fresh_tools.is_empty() {
                continue;
            }
            for tool in &fresh_tools {
                seen_dynamic_tools.insert(tool.name.clone());
            }
            result.push(json!({
                "role": "system",
                "tools": tool_specs_to_openai_functions(&fresh_tools),
            }));
            continue;
        }

        // user 与不带动态工具声明的 system：图片存在时整体走 content 数组。
        let mut parts: Vec<Value> = Vec::new();
        let mut has_image = false;
        for block in &message.blocks {
            match block {
                MessageBlock::Text(text) if !text.text.is_empty() => {
                    parts.push(json!({"type": "text", "text": text.text}));
                }
                MessageBlock::Image(image) => {
                    has_image = true;
                    parts.push(json!({
                        "type": "image_url",
                        "image_url": {
                            "url": image.data_url(),
                            "detail": image.detail.as_str(),
                        },
                    }));
                }
                _ => {}
            }
        }
        let content = if has_image && !parts.is_empty() {
            Value::Array(parts)
        } else {
            let text: String = parts
                .iter()
                .map(|part| part.get("text").and_then(Value::as_str).unwrap_or_default())
                .collect();
            json!(text)
        };
        result.push(json!({"role": message.role.as_str(), "content": content}));
    }
    result
}

/// prompt_cache_key：`sha256(排序后的身份 JSON + model)[..32]`，非 GPT 系列不生成。
pub fn build_prompt_cache_key(identity: &BTreeMap<String, String>, model: &str) -> String {
    if !is_openai_gpt_model(&model.trim().to_lowercase()) {
        return String::new();
    }
    let mut stable_identity = identity.clone();
    stable_identity.insert("model".to_string(), model.trim().to_string());
    let payload =
        serde_json::to_string(&stable_identity).expect("身份映射只含字符串，必须可序列化");
    let digest = Sha256::digest(payload.as_bytes());
    let hex = digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("local-agent-{}", &hex[..32])
}

/// 是否把 prompt_cache_key 带进请求体：有能力声明则带，否则仅 GPT 系列尝试。
pub fn should_send_prompt_cache_key(
    cache_key: &str,
    prompt_cache_capable: bool,
    model: &str,
) -> bool {
    !cache_key.is_empty() && (prompt_cache_capable || is_openai_gpt_model(model))
}

/// 请求体组装所需的输入；`profile_request_timeout_seconds` 是生成选项未声明超时时的回落值。
pub struct ChatRequestInput<'a> {
    pub model: &'a str,
    pub system_prompt: &'a str,
    pub messages: &'a [ConversationMessage],
    pub tools: &'a [ToolSpec],
    pub options: &'a GenerationOptions,
    pub profile_request_timeout_seconds: f64,
    pub prompt_cache_capable: bool,
    pub prompt_cache_identity: &'a BTreeMap<String, String>,
}

/// 组装好的请求：`body` 直接作为 HTTP JSON 体；`timeout_seconds` 归传输层使用，
/// 不进请求体（Python 侧它是 SDK 参数，由 SDK 消费）。
#[derive(Debug, Clone, PartialEq)]
pub struct ChatRequest {
    pub body: Value,
    pub timeout_seconds: f64,
}

/// 组装 Chat Completions 请求（不含重试与建连）。
pub fn build_chat_request(input: &ChatRequestInput<'_>) -> Result<ChatRequest, RequestError> {
    let mut body = Map::new();
    body.insert("model".to_string(), json!(input.model));
    body.insert(
        "messages".to_string(),
        Value::Array(to_openai_messages(input.system_prompt, input.messages)),
    );
    body.insert("stream".to_string(), Value::Bool(true));
    let timeout_seconds = if input.options.request_timeout_seconds != 0.0 {
        input.options.request_timeout_seconds
    } else {
        input.profile_request_timeout_seconds
    };

    if !input.tools.is_empty() {
        body.insert(
            "tools".to_string(),
            Value::Array(tool_specs_to_openai_functions(input.tools)),
        );
        let tool_choice = if input.options.tool_choice.is_empty() {
            "auto".to_string()
        } else {
            input.options.tool_choice.clone()
        };
        body.insert("tool_choice".to_string(), json!(tool_choice));
    }
    if let Some(max_output_tokens) = input.options.max_output_tokens {
        if max_output_tokens != 0 {
            body.insert("max_tokens".to_string(), json!(max_output_tokens));
        }
    }
    if let Some(temperature) = input.options.temperature {
        body.insert("temperature".to_string(), json!(temperature));
    }

    // Python 是把这些扩展键放进 SDK 的 `extra_body` 传的，SDK 再把它们并进请求体的顶层；
    // 内核直接发 HTTP，所以这里就按线上形态摊平，绝不能出现 `extra_body` 这个非标准字段。
    let mut extra_body = sanitize_provider_options(&input.options.provider_options)?;
    let reasoning_effort = input.options.reasoning_effort.as_str();
    if !reasoning_effort.is_empty() && !matches!(reasoning_effort, "none" | "disabled") {
        if !extra_body.contains_key("thinking") {
            extra_body.insert("thinking".to_string(), json!({"type": "enabled"}));
        }
        if !extra_body.contains_key("reasoning_effort") {
            extra_body.insert("reasoning_effort".to_string(), json!(reasoning_effort));
        }
    } else if !extra_body.contains_key("thinking") {
        extra_body.insert("thinking".to_string(), json!({"type": "disabled"}));
    }
    for (key, value) in extra_body {
        body.insert(key, value);
    }

    let cache_key = build_prompt_cache_key(input.prompt_cache_identity, input.model);
    if should_send_prompt_cache_key(&cache_key, input.prompt_cache_capable, input.model) {
        body.insert("prompt_cache_key".to_string(), json!(cache_key));
    }
    Ok(ChatRequest {
        body: Value::Object(body),
        timeout_seconds,
    })
}

/// Python `json.dumps(value, ensure_ascii=False)` 的等价写法：项分隔符为 `", "` 与 `": "`。
fn python_json_dumps(value: &Value) -> String {
    let mut buffer: Vec<u8> = Vec::new();
    let mut serializer = serde_json::Serializer::with_formatter(&mut buffer, PythonStyleFormatter);
    value
        .serialize(&mut serializer)
        .expect("JSON 值写入内存缓冲不会失败");
    String::from_utf8(buffer).expect("JSON 序列化输出恒为 UTF-8")
}

struct PythonStyleFormatter;

impl serde_json::ser::Formatter for PythonStyleFormatter {
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
