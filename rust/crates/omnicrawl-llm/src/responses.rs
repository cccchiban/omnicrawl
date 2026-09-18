//! OpenAI Responses 协议的请求构建。
//!
//! 语义基准是 `omnicrawl/llm/providers/openai_responses.py`：会话消息 → `input` items、
//! 请求级 `tools`（Responses 没有「消息内 tools」，动态声明合并为全局可见）、
//! 以及 `create()` 参数到**线上请求体**的摊平（`extra_body` 是 SDK 参数，SDK 会把它并进请求体顶层）。
//!
//! 未搬：流事件映射（`_stream_turn_events` 的 item 别名与参数分片）、回合运行（`stream_turn`）、
//! `_create_stream_with_retries` 的降级重试编排、`discover_models`。

use omnicrawl_protocol::{
    tools_from_conversation_messages, ConversationMessage, MessageBlock, Role, ToolSpec,
};
use serde_json::{json, Map, Value};

use crate::json::dumps;
use crate::request::{
    build_prompt_cache_key, sanitize_provider_options, ChatRequest, ChatRequestInput, RequestError,
};

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
