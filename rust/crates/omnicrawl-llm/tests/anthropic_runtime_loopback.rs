//! Anthropic runtime 的端到端测试：本机回环喂固定 SSE，覆盖文本与思考增量、用量、
//! 工具调用收尾、请求体一致性、缺凭据与状态码映射。全部离线，不碰外网。
//!
//! 与 Python 真实现的逐字段对照在 `anthropic_parity.rs`；这里只钉内核自身的行为。

mod common;

use std::collections::BTreeMap;

use common::{event_tags, RecordingSink, Reply, StubServer};
use omnicrawl_llm::{
    build_anthropic_request, AnthropicRuntime, ChatEndpoint, ChatRequestInput, RuntimeErrorKind,
};
use omnicrawl_protocol::{ConversationMessage, GenerationOptions, MessageBlock, Role, TextBlock};

const TEXT_STREAM: &str = concat!(
    "event: message_start\n",
    "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":11,\"output_tokens\":1,",
    "\"cache_read_input_tokens\":4}}}\n\n",
    "event: content_block_start\n",
    "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
    "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"你\"}}\n\n",
    "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"好\"}}\n\n",
    "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",",
    "\"thinking\":\"先想一下\"}}\n\n",
    "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
    "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},",
    "\"usage\":{\"output_tokens\":7}}\n\n",
    "data: {\"type\":\"message_stop\"}\n\n",
);

const TOOL_STREAM: &str = concat!(
    "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":20}}}\n\n",
    "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",",
    "\"id\":\"toolu_a\",\"name\":\"read_file\"}}\n\n",
    "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",",
    "\"partial_json\":\"{\\\"path\\\"\"}}\n\n",
    "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",",
    "\"partial_json\":\": \\\"a.py\\\"}\"}}\n\n",
    "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
    "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"}}\n\n",
    "data: {\"type\":\"message_stop\"}\n\n",
);

/// 一次回合的输入装配：消息与选项需要活到来借它们的 `ChatRequestInput`。
struct Turn {
    messages: Vec<ConversationMessage>,
    options: GenerationOptions,
    identity: BTreeMap<String, String>,
}

impl Turn {
    fn new(text: &str) -> Self {
        Self {
            messages: vec![ConversationMessage {
                role: Role::User,
                blocks: vec![MessageBlock::Text(TextBlock::new(text))],
                reasoning: String::new(),
                tools: Vec::new(),
            }],
            options: GenerationOptions::default(),
            identity: BTreeMap::new(),
        }
    }

    fn input<'a>(&'a self, model: &'a str, system_prompt: &'a str) -> ChatRequestInput<'a> {
        ChatRequestInput {
            model,
            system_prompt,
            messages: &self.messages,
            tools: &[],
            options: &self.options,
            profile_request_timeout_seconds: 180.0,
            prompt_cache_capable: false,
            prompt_cache_identity: &self.identity,
        }
    }
}

fn endpoint(server: &StubServer) -> ChatEndpoint {
    ChatEndpoint {
        base_url: server.base_url(),
        api_key: "test-key".to_string(),
        user_agent: "omnicrawl-test".to_string(),
    }
}

#[test]
fn streams_text_reasoning_and_usage() {
    let server = StubServer::spawn(vec![Reply::sse(200, TEXT_STREAM)]);
    let runtime = AnthropicRuntime::new(endpoint(&server), None);
    let turn = Turn::new("你好");
    let input = turn.input("claude-sonnet-4-5", "你是助手");

    let mut sink = RecordingSink::default();
    let reply = runtime.run_turn(&input, &mut sink).expect("回合应成功");

    assert_eq!(reply.content, "你好");
    assert_eq!(reply.reasoning, "先想一下");
    assert_eq!(reply.finish_reason, "end_turn");
    assert_eq!(reply.usage.expect("应有用量").output_tokens, 7);
    assert_eq!(
        event_tags(&sink.events),
        vec!["usage", "text", "text", "reasoning", "usage", "finished"]
    );

    // HTTP 路径与纯构建共用同一套请求体组装。
    let built = build_anthropic_request(&input, None).expect("请求构建");
    assert_eq!(server.bodies()[0], built.body);
    assert_eq!(server.bodies()[0]["system"], "你是助手");
}

#[test]
fn completes_tool_call_and_reports_finish_reason() {
    let server = StubServer::spawn(vec![Reply::sse(200, TOOL_STREAM)]);
    let runtime = AnthropicRuntime::new(endpoint(&server), Some(8192));
    let turn = Turn::new("读一下 a.py");
    let input = turn.input("claude-sonnet-4-5", "");

    let mut sink = RecordingSink::default();
    let reply = runtime.run_turn(&input, &mut sink).expect("回合应成功");

    assert_eq!(reply.finish_reason, "tool_use");
    assert_eq!(reply.tool_calls.len(), 1);
    assert_eq!(reply.tool_calls[0].call_id, "toolu_a");
    assert_eq!(reply.tool_calls[0].name, "read_file");
    assert_eq!(reply.tool_calls[0].arguments["path"], "a.py");
    assert_eq!(
        event_tags(&sink.events),
        vec!["usage", "tool_started", "tool_completed", "finished"]
    );
    // 描述上限参与 max_tokens 兜底。
    assert_eq!(server.bodies()[0]["max_tokens"], 8192);
}

#[test]
fn missing_api_key_is_configuration_error() {
    let runtime = AnthropicRuntime::new(
        ChatEndpoint {
            base_url: "https://api.anthropic.com".to_string(),
            api_key: String::new(),
            user_agent: String::new(),
        },
        None,
    );
    let turn = Turn::new("你好");
    let input = turn.input("claude-sonnet-4-5", "");

    let mut sink = RecordingSink::default();
    let error = runtime
        .run_turn(&input, &mut sink)
        .expect_err("应拒绝无凭据的回合");
    assert_eq!(error.kind, RuntimeErrorKind::Configuration);
    assert_eq!(sink.events.len(), 0);
}

#[test]
fn status_failure_uses_claude_text() {
    let server = StubServer::spawn(vec![Reply::sse(
        401,
        "{\"error\":{\"message\":\"authentication_error: invalid x-api-key\"}}",
    )]);
    let runtime = AnthropicRuntime::new(endpoint(&server), None);
    let turn = Turn::new("你好");
    let input = turn.input("claude-sonnet-4-5", "");

    let mut sink = RecordingSink::default();
    let error = runtime.run_turn(&input, &mut sink).expect_err("应失败");
    assert_eq!(error.kind, RuntimeErrorKind::RequestFailed);
    assert!(
        error
            .message
            .contains("Claude 鉴权失败。请检查 ANTHROPIC_API_KEY。"),
        "实际文案：{}",
        error.message
    );
}

#[test]
fn stream_error_payload_uses_kernel_fallback_text() {
    let server = StubServer::spawn(vec![Reply::sse(
        200,
        concat!(
            "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":1}}}\n\n",
            "data: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"boom\"}}\n\n",
        ),
    )]);
    let runtime = AnthropicRuntime::new(endpoint(&server), None);
    let turn = Turn::new("你好");
    let input = turn.input("claude-sonnet-4-5", "");

    let mut sink = RecordingSink::default();
    let error = runtime.run_turn(&input, &mut sink).expect_err("应失败");
    assert_eq!(error.kind, RuntimeErrorKind::StreamInterrupted);
    assert!(!error.retryable);
}
