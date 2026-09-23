//! 与 Python 实现（`omnicrawl/llm/protocol.py`）的对照测试。
//!
//! 期望值由 `rust/tools/gen_parity_fixture.py` 从 Python 侧直接产出：
//! `python rust/tools/gen_parity_fixture.py`。

use omnicrawl_protocol::{
    aggregate_stream_events, blocks_from_openai_content_parts, conversation_from_openai_messages,
    tool_spec_from_openai_item, tools_from_conversation_messages, ConversationMessage,
    GenerationOptions, ImageBlock, ImageDetail, MessageBlock, ModelIdentity, ModelReply,
    ModelStreamEvent, Protocol, Provider, ProviderWarning, ReasoningDelta, Role, TextBlock,
    TextDelta, TokenUsage, ToolCallArgumentsDelta, ToolCallBlock, ToolCallCompleted,
    ToolCallStarted, ToolResultBlock, ToolSpec, UsageReported,
};
use serde_json::{json, Map, Value};

const FIXTURE: &str = include_str!("fixtures/protocol_parity.json");

fn project_block(block: &MessageBlock) -> Value {
    match block {
        MessageBlock::Text(text) => json!({"kind": "text", "text": text.text}),
        MessageBlock::Image(image) => json!({
            "kind": "image",
            "media_type": image.media_type,
            "data_base64": image.data_base64,
            "detail": image.detail.as_str(),
        }),
        MessageBlock::ToolCall(call) => json!({
            "kind": "tool_call",
            "call_id": call.call_id,
            "name": call.name,
            "arguments": call.arguments,
        }),
        MessageBlock::ToolResult(result) => json!({
            "kind": "tool_result",
            "call_id": result.call_id,
            "ok": result.ok,
            "content": result.content,
        }),
    }
}

fn project_tool(tool: &ToolSpec) -> Value {
    json!({
        "name": tool.name,
        "description": tool.description,
        "parameters": tool.parameters,
    })
}

fn project_message(message: &ConversationMessage) -> Value {
    json!({
        "role": message.role.as_str(),
        "text": message.text(),
        "reasoning": message.reasoning,
        "blocks": message.blocks.iter().map(project_block).collect::<Vec<_>>(),
        "tools": message.tools.iter().map(project_tool).collect::<Vec<_>>(),
    })
}

fn project_reply(reply: &ModelReply) -> Value {
    json!({
        "content": reply.content,
        "reasoning": reply.reasoning,
        "finish_reason": reply.finish_reason,
        "content_streamed": reply.content_streamed,
        "usage": reply.usage.map(|usage| json!({
            "input_tokens": usage.input_tokens,
            "output_tokens": usage.output_tokens,
            "cached_input_tokens": usage.cached_input_tokens,
            "reasoning_tokens": usage.reasoning_tokens,
        })),
        "tool_calls": reply.tool_calls.iter().map(|call| json!({
            "call_id": call.call_id,
            "name": call.name,
            "arguments": call.arguments,
        })).collect::<Vec<_>>(),
        "warnings": reply.warnings.iter().map(|warning| json!({
            "code": warning.code,
            "message": warning.message,
        })).collect::<Vec<_>>(),
        "blocks": reply
            .assistant_message
            .blocks
            .iter()
            .map(project_block)
            .collect::<Vec<_>>(),
        "assistant_role": reply.assistant_message.role.as_str(),
        "message_reasoning": reply.assistant_message.reasoning,
        "message_tool_count": reply.assistant_message.tools.len(),
    })
}

fn str_field(value: &Value, key: &str) -> String {
    value[key].as_str().unwrap_or_default().to_string()
}

fn i64_field(value: &Value, key: &str) -> i64 {
    value[key].as_i64().unwrap_or_default()
}

fn obj_field(value: &Value, key: &str) -> Map<String, Value> {
    value[key].as_object().cloned().unwrap_or_default()
}

fn event_from_json(value: &Value) -> ModelStreamEvent {
    match value["kind"].as_str().unwrap_or_default() {
        "text_delta" => ModelStreamEvent::TextDelta(TextDelta::new(str_field(value, "text"))),
        "reasoning_delta" => {
            ModelStreamEvent::ReasoningDelta(ReasoningDelta::new(str_field(value, "text")))
        }
        "tool_call_started" => ModelStreamEvent::ToolCallStarted(ToolCallStarted::new(
            str_field(value, "call_id"),
            str_field(value, "name"),
        )),
        "tool_call_arguments_delta" => ModelStreamEvent::ToolCallArgumentsDelta(
            ToolCallArgumentsDelta::new(str_field(value, "call_id"), str_field(value, "delta")),
        ),
        "tool_call_completed" => ModelStreamEvent::ToolCallCompleted(ToolCallCompleted::new(
            str_field(value, "call_id"),
            str_field(value, "name"),
            obj_field(value, "arguments"),
        )),
        "usage" => ModelStreamEvent::UsageReported(UsageReported {
            input_tokens: i64_field(value, "input_tokens"),
            output_tokens: i64_field(value, "output_tokens"),
            cached_input_tokens: i64_field(value, "cached_input_tokens"),
            reasoning_tokens: i64_field(value, "reasoning_tokens"),
        }),
        "finished" => ModelStreamEvent::Finished {
            finish_reason: str_field(value, "finish_reason"),
        },
        "warning" => ModelStreamEvent::ProviderWarning(ProviderWarning::new(
            str_field(value, "code"),
            str_field(value, "message"),
        )),
        other => panic!("未知事件类型：{other}"),
    }
}

fn fixture() -> Value {
    serde_json::from_str(FIXTURE).expect("parity fixture 必须是合法 JSON")
}

#[test]
fn parity_conversation_messages() {
    let fixture = fixture();
    for case in fixture["conversation"]
        .as_array()
        .expect("conversation 用例")
    {
        let name = case["name"].as_str().unwrap_or_default();
        let messages = case["messages"].as_array().expect("messages 数组");

        let converted = conversation_from_openai_messages(messages);
        let actual: Vec<Value> = converted.iter().map(project_message).collect();
        assert_eq!(json!(actual), case["expected"], "conversation 用例 {name}");

        let tools: Vec<Value> = tools_from_conversation_messages(&converted)
            .iter()
            .map(project_tool)
            .collect();
        assert_eq!(json!(tools), case["expected_tools"], "tools 用例 {name}");
    }
}

#[test]
fn parity_aggregate_stream_events() {
    let fixture = fixture();
    for case in fixture["aggregate"].as_array().expect("aggregate 用例") {
        let name = case["name"].as_str().unwrap_or_default();
        let events: Vec<ModelStreamEvent> = case["events"]
            .as_array()
            .expect("events 数组")
            .iter()
            .map(event_from_json)
            .collect();

        let reply = aggregate_stream_events(events);
        assert_eq!(
            project_reply(&reply),
            case["expected"],
            "aggregate 用例 {name}"
        );
    }
}

#[test]
fn image_blocks_are_strict_about_data_urls() {
    let parts = json!([
        {"type": "text", "text": "看图"},
        {"type": "image_url", "image_url": {"url": "data:image/png;base64,QUJD", "detail": "high"}},
        {"type": "image_url", "image_url": {"url": "data:image/png;base64,"}},
        {"type": "image_url", "image_url": {"url": "data:image/png;base64,QU JD"}},
        {"type": "input_image", "image_url": {"url": "DATA:IMAGE/JPEG;BASE64,//4A"}},
        {"type": "image_url", "image_url": {"url": "data:image/png;base64,QUJD", "detail": "high"}},
    ]);

    let blocks = blocks_from_openai_content_parts(parts.as_array().unwrap());

    assert_eq!(blocks.len(), 4);
    assert_eq!(blocks[0], MessageBlock::Text(TextBlock::new("看图")));
    assert_eq!(
        blocks[1],
        MessageBlock::Image(ImageBlock::new("image/png", "QUJD", ImageDetail::High))
    );
    assert_eq!(
        blocks[2],
        MessageBlock::Image(ImageBlock::new("image/jpeg", "//4A", ImageDetail::Auto))
    );
    assert_eq!(
        blocks[3],
        MessageBlock::Image(ImageBlock::new("image/png", "QUJD", ImageDetail::High))
    );
}

#[test]
fn tool_spec_defaults_and_junk_input() {
    assert!(tool_spec_from_openai_item(&json!("not-a-dict")).is_none());
    assert!(tool_spec_from_openai_item(&json!({"function": {"name": "   "}})).is_none());

    let spec = tool_spec_from_openai_item(&json!({
        "type": "function",
        "function": {"name": " read "},
    }))
    .expect("缺 parameters 时回落空 object schema");

    assert_eq!(spec.name, "read");
    assert_eq!(spec.description, "");
    assert_eq!(spec.parameters["type"], json!("object"));
    assert_eq!(spec.parameters["properties"], json!({}));
}

#[test]
fn identity_and_enum_round_trip() {
    for provider in Provider::ALL {
        assert_eq!(Provider::parse(provider.as_str()), Some(provider));
    }
    for protocol in Protocol::ALL {
        assert_eq!(Protocol::parse(protocol.as_str()), Some(protocol));
    }
    assert_eq!(Provider::parse("unknown"), None);
    assert_eq!(Protocol::parse("unknown"), None);
    assert_eq!(
        Provider::Openai.default_protocol(),
        Protocol::OpenaiChatCompletions
    );
    assert_eq!(
        Provider::Anthropic.default_protocol(),
        Protocol::AnthropicMessages
    );
    assert_eq!(
        Provider::Gemini.default_protocol(),
        Protocol::GeminiGenerateContent
    );

    let identity = ModelIdentity::new(
        "profile",
        Provider::Openai,
        Protocol::OpenaiResponses,
        "gpt-x",
    );
    assert_eq!(identity.reference(), "profile/gpt-x");
    assert_eq!(
        identity.triple(),
        ("profile", Protocol::OpenaiResponses, "gpt-x")
    );

    let mut keyed = identity.clone();
    keyed.catalog_key = "openai/gpt-x".to_string();
    assert_eq!(keyed.reference(), "openai/gpt-x");
}

#[test]
fn message_helpers() {
    let message = ConversationMessage {
        role: Role::User,
        blocks: vec![
            MessageBlock::Text(TextBlock::new("a")),
            MessageBlock::ToolCall(ToolCallBlock::new("c1", "read", Map::new())),
            MessageBlock::ToolResult(ToolResultBlock::new("c1", true, "body")),
            MessageBlock::Text(TextBlock::new("b")),
        ],
        reasoning: String::new(),
        tools: Vec::new(),
    };

    assert_eq!(message.text(), "ab");
    assert_eq!(Role::parse("developer").as_str(), "developer");
    assert_eq!(Role::parse("user").as_str(), "user");
    assert_eq!(ImageDetail::parse("weird"), ImageDetail::Auto);
    assert_eq!(
        ImageBlock::new("image/png", "QUJD", ImageDetail::High).data_url(),
        "data:image/png;base64,QUJD"
    );

    let options = GenerationOptions::default();
    assert_eq!(options.request_timeout_seconds, 180.0);
    assert_eq!(options.request_retry_count, 5);
    assert!(options.max_output_tokens.is_none());
    assert_eq!(TokenUsage::new(1, 2).as_tuple(), (1, 2, 0));
}

#[test]
fn reply_serde_round_trip() {
    let reply = aggregate_stream_events([
        ModelStreamEvent::TextDelta(TextDelta::new("你好")),
        ModelStreamEvent::Finished {
            finish_reason: "stop".to_string(),
        },
    ]);

    let encoded = serde_json::to_value(&reply).expect("序列化 ModelReply");
    assert_eq!(encoded["assistant_message"]["role"], json!("assistant"));

    let decoded: ModelReply = serde_json::from_value(encoded).expect("反序列化 ModelReply");
    assert_eq!(project_reply(&decoded), project_reply(&reply));
}
