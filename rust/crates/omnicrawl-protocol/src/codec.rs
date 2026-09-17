//! OpenAI 风格历史与会话消息、工具声明之间的编解码（兼容迁移期）。

use std::collections::HashSet;

use serde_json::{Map, Value};

use crate::message::{
    ConversationMessage, ImageBlock, ImageDetail, MessageBlock, Role, TextBlock, ToolCallBlock,
    ToolResultBlock, ToolSpec,
};

/// 把现有 OpenAI Chat 风格历史转换为统一消息块。
pub fn conversation_from_openai_messages(messages: &[Value]) -> Vec<ConversationMessage> {
    let mut converted: Vec<ConversationMessage> = Vec::new();

    for message in messages {
        let Some(message) = message.as_object() else {
            continue;
        };
        let role = message_role(message);

        if role == "tool" {
            let call_id = first_non_empty_str(message, &["tool_call_id", "id"]);
            let text = message.get("content").and_then(Value::as_str).unwrap_or("");
            converted.push(ConversationMessage {
                role: Role::Tool,
                blocks: vec![MessageBlock::ToolResult(ToolResultBlock::new(
                    call_id, true, text,
                ))],
                reasoning: String::new(),
                tools: Vec::new(),
            });
            continue;
        }

        let mut tools: Vec<ToolSpec> = Vec::new();
        if role == "system" {
            if let Some(Value::Array(raw_tools)) = message.get("tools") {
                tools = raw_tools
                    .iter()
                    .filter_map(tool_spec_from_openai_item)
                    .collect();
            }
        }

        let mut blocks: Vec<MessageBlock> = Vec::new();
        match message.get("content") {
            Some(Value::String(content)) if !content.is_empty() => {
                blocks.push(MessageBlock::Text(TextBlock::new(content.clone())));
            }
            Some(Value::Array(parts)) => blocks.extend(blocks_from_openai_content_parts(parts)),
            _ => {}
        }

        if let Some(Value::Array(tool_calls)) = message.get("tool_calls") {
            for item in tool_calls {
                if let Some(block) = tool_call_block_from_openai_item(item) {
                    blocks.push(MessageBlock::ToolCall(block));
                }
            }
        }

        if !blocks.is_empty() || matches!(role.as_str(), "user" | "assistant" | "system") {
            // 思考模式网关要求历史 assistant 消息回传 reasoning_content，丢弃会被拒（HTTP 400）。
            let reasoning = message
                .get("reasoning_content")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            converted.push(ConversationMessage {
                role: Role::parse(&role),
                blocks,
                reasoning,
                tools,
            });
        }
    }

    converted
}

/// 收集 system 消息中携带的动态工具声明，供不支持消息内 tools 的 Provider 合并。
///
/// 按工具名去重：同一工具在多次搜索加载中重复出现时只合并一次，避免请求级 tools
/// 出现重复函数声明。
pub fn tools_from_conversation_messages(messages: &[ConversationMessage]) -> Vec<ToolSpec> {
    let mut seen: HashSet<&str> = HashSet::new();
    let mut result: Vec<ToolSpec> = Vec::new();

    for message in messages {
        if message.role != Role::System || message.tools.is_empty() {
            continue;
        }
        for tool in &message.tools {
            if !seen.insert(tool.name.as_str()) {
                continue;
            }
            result.push(tool.clone());
        }
    }

    result
}

/// 把 Chat Completions 风格 tools 数组项转成统一 `ToolSpec`。
pub fn tool_spec_from_openai_item(item: &Value) -> Option<ToolSpec> {
    let item = item.as_object()?;
    let function = match item.get("function") {
        Some(Value::Object(function)) => function,
        _ => item,
    };

    let name = function
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    if name.is_empty() {
        return None;
    }

    let parameters = match function.get("parameters") {
        Some(Value::Object(parameters)) => parameters.clone(),
        _ => default_parameters(),
    };
    let description = function
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or("");

    Some(ToolSpec::new(name, description, parameters))
}

/// 解析 Host 内部使用的 OpenAI 风格文本/图片内容块。
pub fn blocks_from_openai_content_parts(parts: &[Value]) -> Vec<MessageBlock> {
    let mut blocks: Vec<MessageBlock> = Vec::new();

    for part in parts {
        let Some(part) = part.as_object() else {
            continue;
        };
        let part_type = part
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_lowercase();

        if part_type == "text" || part_type == "input_text" {
            if let Some(Value::String(text)) = part.get("text") {
                if !text.is_empty() {
                    blocks.push(MessageBlock::Text(TextBlock::new(text.clone())));
                }
            }
            continue;
        }
        if part_type != "image_url" && part_type != "input_image" {
            continue;
        }

        let detail = match part.get("detail") {
            Some(Value::String(detail)) if !detail.is_empty() => detail.clone(),
            _ => "auto".to_string(),
        };
        let (image_url, detail) = match part.get("image_url") {
            Some(Value::Object(image_url)) => {
                let detail = match image_url.get("detail") {
                    Some(Value::String(detail)) if !detail.is_empty() => detail.clone(),
                    _ => detail,
                };
                match image_url.get("url") {
                    Some(Value::String(url)) => (url.clone(), detail),
                    _ => continue,
                }
            }
            Some(Value::String(url)) => (url.clone(), detail),
            _ => continue,
        };

        let Some((media_type, data_base64)) = parse_data_image_url(image_url.trim()) else {
            continue;
        };
        blocks.push(MessageBlock::Image(ImageBlock::new(
            media_type,
            data_base64,
            ImageDetail::parse(&detail),
        )));
    }

    blocks
}

/// Python 侧只接受能解析成对象的 JSON 参数，其余（数组、标量、语法错误）都退化为空对象。
pub(crate) fn parse_arguments_object(raw: &str) -> Map<String, Value> {
    if raw.trim().is_empty() {
        return Map::new();
    }
    match serde_json::from_str::<Value>(raw) {
        Ok(Value::Object(arguments)) => arguments,
        _ => Map::new(),
    }
}

/// Python 侧 `str(message.get("role") or "user")`：缺失或空字符串回落 `user`。
fn message_role(message: &Map<String, Value>) -> String {
    match message.get("role") {
        Some(Value::String(role)) if !role.is_empty() => role.clone(),
        _ => "user".to_string(),
    }
}

fn first_non_empty_str(message: &Map<String, Value>, keys: &[&str]) -> String {
    keys.iter()
        .find_map(|key| match message.get(*key) {
            Some(Value::String(value)) if !value.is_empty() => Some(value.clone()),
            _ => None,
        })
        .unwrap_or_default()
}

fn tool_call_block_from_openai_item(item: &Value) -> Option<ToolCallBlock> {
    let item = item.as_object()?;
    let function = item.get("function").and_then(Value::as_object);
    let name = function
        .and_then(|function| function.get("name"))
        .and_then(Value::as_str)
        .unwrap_or("");
    if name.is_empty() {
        return None;
    }

    let arguments = match function.and_then(|function| function.get("arguments")) {
        Some(Value::String(raw)) if !raw.trim().is_empty() => parse_arguments_object(raw),
        Some(Value::Object(arguments)) => arguments.clone(),
        _ => Map::new(),
    };
    let call_id = item.get("id").and_then(Value::as_str).unwrap_or("");

    Some(ToolCallBlock::new(call_id, name, arguments))
}

fn default_parameters() -> Map<String, Value> {
    let mut parameters = Map::new();
    parameters.insert("type".to_string(), Value::String("object".to_string()));
    parameters.insert("properties".to_string(), Value::Object(Map::new()));
    parameters
}

/// 解析 `data:image/<png|jpeg|webp|gif>;base64,<payload>`（大小写不敏感、整体锚定）。
fn parse_data_image_url(url: &str) -> Option<(String, String)> {
    let rest = strip_prefix_ignore_ascii_case(url, "data:")?;
    let (media_type, payload) = split_once_ignore_ascii_case(rest, ";base64,")?;
    let media_type = media_type.to_lowercase();
    if !matches!(
        media_type.as_str(),
        "image/png" | "image/jpeg" | "image/webp" | "image/gif"
    ) {
        return None;
    }
    if payload.is_empty()
        || !payload
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/' | b'='))
    {
        return None;
    }
    Some((media_type, payload.to_string()))
}

fn strip_prefix_ignore_ascii_case<'a>(value: &'a str, prefix: &str) -> Option<&'a str> {
    let head = value.as_bytes().get(..prefix.len())?;
    if !head.eq_ignore_ascii_case(prefix.as_bytes()) {
        return None;
    }
    Some(&value[prefix.len()..])
}

fn split_once_ignore_ascii_case<'a>(haystack: &'a str, needle: &str) -> Option<(&'a str, &'a str)> {
    let haystack_bytes = haystack.as_bytes();
    let needle_bytes = needle.as_bytes();
    if needle_bytes.is_empty() || haystack_bytes.len() < needle_bytes.len() {
        return None;
    }
    let index = (0..=haystack_bytes.len() - needle_bytes.len()).find(|&index| {
        haystack_bytes[index..index + needle_bytes.len()].eq_ignore_ascii_case(needle_bytes)
    })?;
    Some((&haystack[..index], &haystack[index + needle_bytes.len()..]))
}
