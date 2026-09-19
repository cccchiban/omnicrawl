//! OpenAI Responses runtime 的本机回环验收：请求体、两路降级重试与收尾顺序。
//!
//! 全程离线：回环服务端按顺序接请求并记录请求体，因此能钉住「降级重发到底改了哪一处」。
//! 期望的降级语义来自 Python `openai_responses.py` 的 `_create_stream_with_retries` 与
//! 循环之后的收尾顺序。

mod common;

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::thread;

use omnicrawl_llm::{ChatEndpoint, ResponsesRuntime, RuntimeErrorKind, SinkFlow, TurnSink};
use omnicrawl_protocol::{
    ConversationMessage, MessageBlock, ModelStreamEvent, Role, TextBlock, ToolCallBlock,
    ToolResultBlock,
};
use serde_json::Map;

use common::{event_tags, CaseInput, RecordingSink};

const TEXT_STREAM: &str = concat!(
    "data: {\"type\":\"response.output_text.delta\",\"delta\":\"你好\"}\n\n",
    "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"output\":[]}}\n\n",
);

const CACHE_REJECTION: &str =
    "{\"error\":{\"message\":\"Unrecognized request argument supplied: prompt_cache_key\"}}";

const HISTORY_REJECTION: &str =
    "{\"error\":{\"message\":\"400 Bad Request: unsupported input item function_call\"}}";

/// 只带无关事件、没有任何产出的流：收尾时应判为提前耗尽。
const EMPTY_STREAM: &str = "data: {\"type\":\"response.in_progress\"}\n\n";

struct StubReply {
    status: u16,
    reason: &'static str,
    payload: &'static str,
}

struct Captured {
    request_line: String,
    body: serde_json::Value,
}

/// 按顺序服务若干次请求，并把每次的请求行与请求体记下来。
fn spawn_server(replies: Vec<StubReply>) -> (String, Arc<Mutex<Vec<Captured>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("无法监听回环端口");
    let addr = listener.local_addr().expect("无法取本地地址");
    let captured: Arc<Mutex<Vec<Captured>>> = Arc::new(Mutex::new(Vec::new()));
    let store = Arc::clone(&captured);

    thread::spawn(move || {
        for reply in replies {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let mut buffer: Vec<u8> = Vec::new();
            let mut chunk = [0u8; 4096];
            let head_end = loop {
                let read = stream.read(&mut chunk).unwrap_or(0);
                if read == 0 {
                    return;
                }
                buffer.extend_from_slice(&chunk[..read]);
                if let Some(position) = buffer.windows(4).position(|item| item == b"\r\n\r\n") {
                    break position + 4;
                }
            };
            let head = String::from_utf8_lossy(&buffer[..head_end]).to_string();
            let request_line = head.split("\r\n").next().unwrap_or_default().to_string();
            let content_length = head
                .split("\r\n")
                .filter_map(|line| line.split_once(':'))
                .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                .and_then(|(_, value)| value.trim().parse::<usize>().ok())
                .unwrap_or(0);
            while buffer.len() < head_end + content_length {
                let read = stream.read(&mut chunk).unwrap_or(0);
                if read == 0 {
                    break;
                }
                buffer.extend_from_slice(&chunk[..read]);
            }
            let body =
                serde_json::from_slice(&buffer[head_end..]).unwrap_or(serde_json::Value::Null);
            store
                .lock()
                .expect("记录锁")
                .push(Captured { request_line, body });

            let response = format!(
                "HTTP/1.1 {status} {reason}\r\nContent-Type: text/event-stream\r\n\
                 Content-Length: {length}\r\nConnection: close\r\n\r\n",
                status = reply.status,
                reason = reply.reason,
                length = reply.payload.len(),
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.write_all(reply.payload.as_bytes());
        }
    });

    (format!("http://{addr}"), captured)
}

fn runtime_for(base_url: String) -> ResponsesRuntime {
    ResponsesRuntime::new(ChatEndpoint {
        base_url,
        api_key: "responses-key".to_string(),
        user_agent: String::new(),
    })
}

/// 带工具历史的会话：assistant 的 function_call + 随后的 function_call_output。
fn tool_history_case() -> CaseInput {
    let mut case = CaseInput::simple("gpt-5.2", "读文件");
    case.messages = vec![
        ConversationMessage {
            role: Role::User,
            blocks: vec![MessageBlock::Text(TextBlock::new("读文件"))],
            reasoning: String::new(),
            tools: Vec::new(),
        },
        ConversationMessage {
            role: Role::Assistant,
            blocks: vec![MessageBlock::ToolCall(ToolCallBlock::new(
                "call_a",
                "read_file",
                Map::new(),
            ))],
            reasoning: String::new(),
            tools: Vec::new(),
        },
        ConversationMessage {
            role: Role::Tool,
            blocks: vec![MessageBlock::ToolResult(ToolResultBlock::new(
                "call_a", true, "print(1)",
            ))],
            reasoning: String::new(),
            tools: Vec::new(),
        },
    ];
    case
}

#[test]
fn streams_text_and_sends_responses_body() {
    let (base_url, captured) = spawn_server(vec![StubReply {
        status: 200,
        reason: "OK",
        payload: TEXT_STREAM,
    }]);
    let runtime = runtime_for(base_url);
    let mut case = CaseInput::simple("gpt-5.2", "你好");
    case.system_prompt = "你是助手".to_string();

    let mut sink = RecordingSink::default();
    let reply = runtime
        .run_turn(&case.chat_input(), &mut sink)
        .expect("回合应当成功");

    assert_eq!(event_tags(&sink.events), vec!["text", "finished"]);
    assert_eq!(reply.content, "你好");
    assert_eq!(reply.finish_reason, "completed");

    let requests = captured.lock().expect("记录锁");
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].request_line, "POST /responses HTTP/1.1");
    assert_eq!(requests[0].body["model"], "gpt-5.2");
    assert_eq!(requests[0].body["instructions"], "你是助手");
    assert_eq!(requests[0].body["stream"], true);
    assert_eq!(requests[0].body["input"][0]["content"][0]["text"], "你好");
    assert_eq!(
        requests[0].body["reasoning"],
        serde_json::json!({"effort": "none"})
    );
}

#[test]
fn drops_prompt_cache_key_when_gateway_rejects_it() {
    let (base_url, captured) = spawn_server(vec![
        StubReply {
            status: 400,
            reason: "Bad Request",
            payload: CACHE_REJECTION,
        },
        StubReply {
            status: 200,
            reason: "OK",
            payload: TEXT_STREAM,
        },
    ]);
    let runtime = runtime_for(base_url);
    let mut case = CaseInput::simple("gpt-5.2", "你好");
    case.prompt_cache_capable = true;
    case.prompt_cache_identity = [("profile_id".to_string(), "p1".to_string())]
        .into_iter()
        .collect();

    let mut sink = RecordingSink::default();
    let reply = runtime
        .run_turn(&case.chat_input(), &mut sink)
        .expect("摘掉字段后应当成功");

    assert_eq!(reply.content, "你好");
    assert_eq!(
        event_tags(&sink.events),
        vec!["text", "warning", "finished"],
        "告警在收尾时发，位于结束事件之前"
    );
    match &sink.events[1] {
        ModelStreamEvent::ProviderWarning(warning) => {
            assert_eq!(warning.code, "prompt_cache_unsupported");
        }
        other => panic!("第二个事件应是告警，实际 {other:?}"),
    }

    let requests = captured.lock().expect("记录锁");
    assert_eq!(requests.len(), 2, "应当重发一次");
    assert!(
        requests[0].body.get("prompt_cache_key").is_some(),
        "首次请求应带 prompt_cache_key"
    );
    assert!(
        requests[1].body.get("prompt_cache_key").is_none(),
        "重发请求不应再带 prompt_cache_key"
    );
}

#[test]
fn flattens_tool_history_and_remembers_the_combination() {
    let (base_url, captured) = spawn_server(vec![
        StubReply {
            status: 400,
            reason: "Bad Request",
            payload: HISTORY_REJECTION,
        },
        StubReply {
            status: 200,
            reason: "OK",
            payload: TEXT_STREAM,
        },
        StubReply {
            status: 200,
            reason: "OK",
            payload: TEXT_STREAM,
        },
    ]);
    let runtime = runtime_for(base_url);
    let case = tool_history_case();

    let mut sink = RecordingSink::default();
    runtime
        .run_turn(&case.chat_input(), &mut sink)
        .expect("展平工具历史后应当成功");

    assert!(
        sink.events.iter().any(|event| matches!(
            event,
            ModelStreamEvent::ProviderWarning(warning) if warning.code == "tool_history_flattened"
        )),
        "应当补发一条降级告警：{:?}",
        event_tags(&sink.events)
    );

    // 第二轮：同一个 runtime 记住该组合，直接发展平后的请求。
    let mut sink = RecordingSink::default();
    runtime
        .run_turn(&case.chat_input(), &mut sink)
        .expect("第二轮应当直接成功");

    let requests = captured.lock().expect("记录锁");
    assert_eq!(requests.len(), 3, "第二轮只应发一次请求");
    let has_tool_items = |body: &serde_json::Value| {
        body["input"].as_array().is_some_and(|items| {
            items.iter().any(|item| {
                matches!(
                    item["type"].as_str(),
                    Some("function_call") | Some("function_call_output")
                )
            })
        })
    };
    assert!(
        has_tool_items(&requests[0].body),
        "首次请求应带工具历史 item：{}",
        requests[0].body
    );
    assert!(
        !has_tool_items(&requests[1].body),
        "降级重发应把工具历史展平成纯文本：{}",
        requests[1].body
    );
    assert!(
        !has_tool_items(&requests[2].body),
        "记忆该组合后第二轮应直接展平：{}",
        requests[2].body
    );
    assert!(
        !sink
            .events
            .iter()
            .any(|event| matches!(event, ModelStreamEvent::ProviderWarning(_))),
        "直接展平的请求不再补发告警"
    );
}

#[test]
fn premature_eof_is_retryable_stream_interruption() {
    let (base_url, _captured) = spawn_server(vec![StubReply {
        status: 200,
        reason: "OK",
        payload: EMPTY_STREAM,
    }]);
    let runtime = runtime_for(base_url);
    let case = CaseInput::simple("gpt-5.2", "你好");

    let mut sink = RecordingSink::default();
    let error = runtime
        .run_turn(&case.chat_input(), &mut sink)
        .expect_err("没有产出的流必须失败");

    assert_eq!(error.kind, RuntimeErrorKind::StreamInterrupted);
    assert!(error.retryable, "提前耗尽按可重试处理");
    assert!(
        error.message.contains("response.completed"),
        "文案应指出缺少完成事件：{}",
        error.message
    );
}

#[test]
fn status_failures_map_like_python() {
    for (status, reason, retryable) in [
        (400, "Bad Request", false),
        (500, "Internal Server Error", true),
        (503, "Service Unavailable", true),
    ] {
        let (base_url, _captured) = spawn_server(vec![StubReply {
            status,
            reason,
            payload: "{\"error\":{\"message\":\"boom\"}}",
        }]);
        let runtime = runtime_for(base_url);
        let case = CaseInput::simple("gpt-5.2", "你好");

        let mut sink = RecordingSink::default();
        let error = runtime
            .run_turn(&case.chat_input(), &mut sink)
            .expect_err("非 2xx 必须失败");

        assert_eq!(
            error.kind,
            RuntimeErrorKind::RequestFailed,
            "状态码 {status}"
        );
        assert_eq!(error.retryable, retryable, "状态码 {status} 的可重试标记");
        assert!(
            error.message.starts_with("Responses 请求失败："),
            "状态码 {status} 的文案前缀：{}",
            error.message
        );
    }
}

#[test]
fn missing_api_key_is_configuration_error() {
    let (base_url, _captured) = spawn_server(Vec::new());
    let runtime = ResponsesRuntime::new(ChatEndpoint {
        base_url,
        api_key: String::new(),
        user_agent: String::new(),
    });
    let case = CaseInput::simple("gpt-5.2", "你好");

    let mut sink = RecordingSink::default();
    let error = runtime
        .run_turn(&case.chat_input(), &mut sink)
        .expect_err("缺少凭据必须直接失败");

    assert_eq!(error.kind, RuntimeErrorKind::Configuration);
    assert_eq!(sink.events.len(), 0);
}

#[test]
fn sink_cancel_stops_the_turn() {
    let (base_url, _captured) = spawn_server(vec![StubReply {
        status: 200,
        reason: "OK",
        payload: TEXT_STREAM,
    }]);
    let runtime = runtime_for(base_url);
    let case = CaseInput::simple("gpt-5.2", "你好");

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
