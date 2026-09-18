//! 内核 runtime 的端到端测试：本机回环 HTTP 服务端喂固定 SSE，覆盖文本/推理/用量、
//! 工具调用收尾、截断拒绝、prompt_cache_key 回退、状态码映射与取消。全部离线，不碰外网。
//!
//! 与 Python 真实现的逐字段对照在 `runtime_parity.rs`；这里只钉内核自身的行为。

mod common;

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use common::{event_tags, CaseInput, RecordingSink, Reply, StubServer};
use omnicrawl_llm::{
    payload_of_line, ChatEndpoint, OpenAiChatRuntime, RuntimeErrorKind, SinkFlow, TurnSink,
};
use omnicrawl_protocol::ModelStreamEvent;

const TEXT_STREAM: &str = concat!(
    "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\"}}]}\n\n",
    "data: {\"choices\":[{\"delta\":{\"content\":\"你\"}}]}\n\n",
    "data: {\"choices\":[{\"delta\":{\"content\":\"好\"}}]}\n\n",
    "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"先想一下\"}}]}\n\n",
    "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":",
    "{\"prompt_tokens\":11,\"completion_tokens\":3,\"prompt_tokens_details\":{\"cached_tokens\":4},",
    "\"completion_tokens_details\":{\"reasoning_tokens\":2}}}\n\n",
    "data: [DONE]\n\n",
);

const TOOL_STREAM: &str = concat!(
    "data: {\"choices\":[{\"delta\":{\"tool_calls\":",
    "[{\"index\":0,\"id\":\"call_a\",\"function\":{\"name\":\"read_file\"}}]}}]}\n\n",
    "data: {\"choices\":[{\"delta\":{\"tool_calls\":",
    "[{\"index\":0,\"function\":{\"arguments\":\"{\\\"path\\\"\"}}]}}]}\n\n",
    "data: {\"choices\":[{\"delta\":{\"tool_calls\":",
    "[{\"index\":0,\"function\":{\"arguments\":\":\\\"a.py\\\"}\"}}]}}]}\n\n",
    "data: [DONE]\n\n",
);

fn runtime_for(server: &StubServer) -> OpenAiChatRuntime {
    OpenAiChatRuntime::new(ChatEndpoint {
        base_url: server.base_url(),
        api_key: "test-key".to_string(),
        user_agent: "omnicrawl-test".to_string(),
    })
}

#[test]
fn streams_text_reasoning_and_usage() {
    let server = StubServer::spawn(vec![Reply::sse(200, TEXT_STREAM)]);
    let runtime = runtime_for(&server);
    let mut case = CaseInput::simple("qwen-test", "你好");
    case.system_prompt = "你是助手。".to_string();

    let mut sink = RecordingSink::default();
    let reply = runtime
        .run_turn(&case.chat_input(), &mut sink)
        .expect("回合应当成功");

    assert_eq!(
        event_tags(&sink.events),
        vec!["text", "text", "reasoning", "usage", "finished"]
    );
    assert_eq!(reply.content, "你好");
    assert_eq!(reply.reasoning, "先想一下");
    assert_eq!(reply.finish_reason, "stop");
    let usage = reply.usage.expect("应当带上用量");
    assert_eq!(
        (
            usage.input_tokens,
            usage.output_tokens,
            usage.cached_input_tokens,
            usage.reasoning_tokens
        ),
        (11, 3, 4, 2)
    );

    // 发出去的请求体：stream=true、无 timeout 字段（timeout 归传输层）。
    let bodies = server.bodies();
    assert_eq!(bodies.len(), 1);
    assert_eq!(bodies[0]["model"], "qwen-test");
    assert_eq!(bodies[0]["stream"], true);
    assert!(bodies[0].get("timeout").is_none(), "timeout 不应进请求体");
    assert_eq!(bodies[0]["messages"][0]["role"], "system");
    assert_eq!(bodies[0]["messages"][0]["content"], "你是助手。");
    assert_eq!(bodies[0]["messages"][1]["role"], "user");
    assert_eq!(bodies[0]["messages"][1]["content"], "你好");
}

#[test]
fn completes_tool_calls_at_stream_end() {
    let server = StubServer::spawn(vec![Reply::sse(200, TOOL_STREAM)]);
    let runtime = runtime_for(&server);
    let case = CaseInput::simple("qwen-test", "读文件");

    let mut sink = RecordingSink::default();
    let reply = runtime
        .run_turn(&case.chat_input(), &mut sink)
        .expect("回合应当成功");

    assert_eq!(
        event_tags(&sink.events),
        vec![
            "tool_started",
            "tool_arguments",
            "tool_arguments",
            "tool_completed",
            "finished"
        ]
    );
    assert_eq!(reply.tool_calls.len(), 1);
    assert_eq!(reply.tool_calls[0].call_id, "call_a");
    assert_eq!(reply.tool_calls[0].name, "read_file");
    assert_eq!(reply.tool_calls[0].arguments["path"], "a.py");
}

#[test]
fn rejects_truncated_tool_arguments() {
    let stream = concat!(
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":",
        "[{\"index\":0,\"id\":\"call_a\",\"function\":{\"name\":\"read_file\"}}]}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":",
        "[{\"index\":0,\"function\":{\"arguments\":\"{\\\"path\\\"\"}}]}}]}\n\n",
        "data: [DONE]\n\n",
    );
    let server = StubServer::spawn(vec![Reply::sse(200, stream)]);
    let runtime = runtime_for(&server);
    let case = CaseInput::simple("qwen-test", "读文件");

    let mut sink = RecordingSink::default();
    let error = runtime
        .run_turn(&case.chat_input(), &mut sink)
        .expect_err("半截参数必须失败");

    assert_eq!(error.kind, RuntimeErrorKind::StreamInterrupted);
    assert!(error.retryable, "截断按可重试处理");
    assert!(
        error.message.contains("参数完整到达前结束"),
        "错误文案应说明参数被截断：{}",
        error.message
    );
}

#[test]
fn drops_prompt_cache_key_when_gateway_rejects_it() {
    let rejection =
        "{\"error\":{\"message\":\"Unrecognized request argument supplied: prompt_cache_key\"}}";
    let server = StubServer::spawn(vec![
        Reply::sse(400, rejection),
        Reply::sse(200, TEXT_STREAM),
    ]);
    let runtime = runtime_for(&server);
    let mut case = CaseInput::simple("gpt-5.2", "你好");
    case.prompt_cache_capable = true;
    case.prompt_cache_identity = BTreeMap::from([("profile_id".to_string(), "p1".to_string())]);

    let mut sink = RecordingSink::default();
    let reply = runtime
        .run_turn(&case.chat_input(), &mut sink)
        .expect("摘掉字段后应当成功");

    assert_eq!(reply.content, "你好");
    assert_eq!(event_tags(&sink.events)[0], "warning");
    match &sink.events[0] {
        ModelStreamEvent::ProviderWarning(warning) => {
            assert_eq!(warning.code, "prompt_cache_unsupported");
        }
        other => panic!("首个事件应是告警，实际 {other:?}"),
    }

    let bodies = server.bodies();
    assert_eq!(bodies.len(), 2, "应当重发一次");
    assert!(
        bodies[0].get("prompt_cache_key").is_some(),
        "首次请求应带 prompt_cache_key"
    );
    assert!(
        bodies[1].get("prompt_cache_key").is_none(),
        "重发请求不应再带 prompt_cache_key"
    );
}

#[test]
fn maps_http_status_like_python() {
    for (status, retryable, marker) in [
        (400, false, "HTTP 400"),
        (401, false, "HTTP 401"),
        (404, false, "HTTP 404"),
        // 429 的正文同样进了分类阶梯，先命中「限流」分支（与 Python 同一张阶梯）。
        (429, true, "限流"),
        (503, true, "HTTP 503"),
    ] {
        let server = StubServer::spawn(vec![Reply::sse(status, "{\"error\":\"boom\"}")]);
        let runtime = runtime_for(&server);
        let case = CaseInput::simple("qwen-test", "你好");

        let mut sink = RecordingSink::default();
        let error = runtime
            .run_turn(&case.chat_input(), &mut sink)
            .expect_err("非 2xx 必须失败");

        assert_eq!(
            error.kind,
            RuntimeErrorKind::RequestFailed,
            "状态码 {status}"
        );
        assert_eq!(error.status_code, Some(status));
        assert_eq!(error.retryable, retryable, "状态码 {status} 的可重试标记");
        assert!(
            error.message.contains(marker),
            "状态码 {status} 的文案应含 {marker}：{}",
            error.message
        );
    }
}

#[test]
fn stream_error_payload_uses_python_fallback_text() {
    let stream = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"半\"}}]}\n\n",
        "data: {\"error\":{\"message\":\"boom\"}}\n\n",
    );
    let server = StubServer::spawn(vec![Reply::sse(200, stream)]);
    let runtime = runtime_for(&server);
    let case = CaseInput::simple("qwen-test", "你好");

    let mut sink = RecordingSink::default();
    let error = runtime
        .run_turn(&case.chat_input(), &mut sink)
        .expect_err("流内错误负载必须中断回合");

    assert_eq!(error.kind, RuntimeErrorKind::StreamInterrupted);
    assert!(
        !error.retryable,
        "与 Python 一致：认不出的流错误不标记可重试"
    );
    assert!(
        error.message.contains("未能识别具体原因"),
        "与 Python 一致地使用通用文案：{}",
        error.message
    );
}

#[test]
fn sink_can_cancel_mid_stream() {
    let server = StubServer::spawn(vec![Reply::sse(200, TEXT_STREAM)]);
    let runtime = runtime_for(&server);
    let case = CaseInput::simple("qwen-test", "你好");

    let mut sink = RecordingSink {
        cancel_after: Some(1),
        ..RecordingSink::default()
    };
    let error = runtime
        .run_turn(&case.chat_input(), &mut sink)
        .expect_err("取消应中止回合");

    assert_eq!(error.kind, RuntimeErrorKind::Cancelled);
    assert_eq!(sink.events.len(), 1, "取消后不再产出事件");
}

#[test]
fn cancels_promptly_while_stream_is_idle() {
    let head = "data: {\"choices\":[{\"delta\":{\"content\":\"开\"}}]}\n\n";
    let server = StubServer::spawn(vec![Reply::held(200, head, Duration::from_millis(700))]);
    let runtime = runtime_for(&server);
    let case = CaseInput::simple("qwen-test", "你好");

    let mut sink = RecordingSink {
        cancel_now: true,
        ..RecordingSink::default()
    };
    let started = Instant::now();
    let error = runtime
        .run_turn(&case.chat_input(), &mut sink)
        .expect_err("取消应中止回合");
    let elapsed = started.elapsed();

    assert_eq!(error.kind, RuntimeErrorKind::Cancelled);
    assert!(
        elapsed < Duration::from_millis(500),
        "流静默时取消也要即时返回，实际耗时 {elapsed:?}"
    );
}

#[test]
fn extracts_sse_payload_from_lines() {
    assert_eq!(payload_of_line("data: {\"a\":1}\n"), Some("{\"a\":1}"));
    assert_eq!(payload_of_line("data:{\"a\":1}\r\n"), Some("{\"a\":1}"));
    assert_eq!(payload_of_line("data:\n"), Some(""));
    assert_eq!(payload_of_line("data: [DONE]\n"), Some("[DONE]"));
    assert_eq!(payload_of_line("\n"), None);
    assert_eq!(payload_of_line(": keep-alive\n"), None);
    assert_eq!(payload_of_line("event: message\n"), None);
    assert_eq!(payload_of_line("id: 42\n"), None);
}

#[test]
fn missing_api_key_is_configuration_error() {
    let server = StubServer::spawn(Vec::new());
    let runtime = OpenAiChatRuntime::new(ChatEndpoint {
        base_url: server.base_url(),
        api_key: String::new(),
        user_agent: String::new(),
    });
    let case = CaseInput::simple("qwen-test", "你好");

    let mut sink = RecordingSink::default();
    let error = runtime
        .run_turn(&case.chat_input(), &mut sink)
        .expect_err("缺少凭据必须直接失败");

    assert_eq!(error.kind, RuntimeErrorKind::Configuration);
    assert!(!error.retryable);
}

/// 确认取消之外还有一条独立入口：接收端返回 Cancel 时不再消费后续事件。
#[test]
fn cancel_flow_is_reported_to_caller() {
    let server = StubServer::spawn(vec![Reply::sse(200, TOOL_STREAM)]);
    let runtime = runtime_for(&server);
    let case = CaseInput::simple("qwen-test", "读文件");

    struct CancelFirst;
    impl TurnSink for CancelFirst {
        fn on_event(&mut self, _event: ModelStreamEvent) -> SinkFlow {
            SinkFlow::Cancel
        }
    }

    let error = runtime
        .run_turn(&case.chat_input(), &mut CancelFirst)
        .expect_err("接收端取消后必须中止");
    assert_eq!(error.kind, RuntimeErrorKind::Cancelled);
}
