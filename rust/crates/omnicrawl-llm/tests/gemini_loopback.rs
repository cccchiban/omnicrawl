//! Gemini runtime 的本机回环验收：请求路径、鉴权头、请求体与流事件。
//!
//! 全程离线：回环服务端自己读原始请求行与头部，因此能钉住 `omnicrawl-cli`
//! 之外的调用方真正发出去的东西（`omnicrawl-llm` 的 `StubServer` 只记录请求体）。

mod common;

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::thread;

use omnicrawl_llm::{ChatEndpoint, GeminiRuntime, RuntimeErrorKind, SinkFlow, TurnSink};
use omnicrawl_protocol::ModelStreamEvent;

use common::{event_tags, CaseInput, RecordingSink};

const TEXT_AND_TOOL_STREAM: &str = concat!(
    "data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"你好\"}]}}],",
    "\"usageMetadata\":{\"promptTokenCount\":3,\"candidatesTokenCount\":1}}\n\n",
    "data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":",
    "[{\"function_call\":{\"name\":\"read_file\",\"args\":{\"path\":\"a.py\"}}}]},",
    "\"finishReason\":\"STOP\"}]}\n\n",
);

struct Captured {
    request_line: String,
    headers: Vec<(String, String)>,
    body: serde_json::Value,
}

impl Captured {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

/// 记录一条原始请求（请求行 + 头 + 体）并回一段固定 SSE 的极简服务端。
fn spawn_capture_server(
    status: u16,
    reason: &'static str,
    payload: &'static str,
) -> (String, Arc<Mutex<Vec<Captured>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("无法监听回环端口");
    let addr = listener.local_addr().expect("无法取本地地址");
    let captured: Arc<Mutex<Vec<Captured>>> = Arc::new(Mutex::new(Vec::new()));
    let store = Arc::clone(&captured);

    thread::spawn(move || {
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
        let mut lines = head.split("\r\n");
        let request_line = lines.next().unwrap_or_default().to_string();
        let mut headers: Vec<(String, String)> = Vec::new();
        let mut content_length = 0usize;
        for line in lines {
            if line.is_empty() {
                continue;
            }
            if let Some((name, value)) = line.split_once(':') {
                let name = name.trim().to_string();
                let value = value.trim().to_string();
                if name.eq_ignore_ascii_case("content-length") {
                    content_length = value.parse().unwrap_or(0);
                }
                headers.push((name, value));
            }
        }
        while buffer.len() < head_end + content_length {
            let read = stream.read(&mut chunk).unwrap_or(0);
            if read == 0 {
                break;
            }
            buffer.extend_from_slice(&chunk[..read]);
        }
        let body = serde_json::from_slice(&buffer[head_end..]).unwrap_or(serde_json::Value::Null);
        store.lock().expect("记录锁").push(Captured {
            request_line,
            headers,
            body,
        });

        let response = format!(
            "HTTP/1.1 {status} {reason}\r\nContent-Type: text/event-stream\r\n\
             Content-Length: {length}\r\nConnection: close\r\n\r\n",
            length = payload.len(),
        );
        let _ = stream.write_all(response.as_bytes());
        let _ = stream.write_all(payload.as_bytes());
    });

    (format!("http://{addr}"), captured)
}

fn endpoint(base_url: String) -> ChatEndpoint {
    ChatEndpoint {
        base_url,
        api_key: "gemini-key".to_string(),
        user_agent: String::new(),
    }
}

#[test]
fn streams_text_tool_call_and_wire_request() {
    let (base_url, captured) = spawn_capture_server(200, "OK", TEXT_AND_TOOL_STREAM);
    let runtime = GeminiRuntime::new(endpoint(base_url));
    let mut case = CaseInput::simple("gemini-2.5-flash", "你好");
    case.system_prompt = "你是助手".to_string();

    let mut sink = RecordingSink::default();
    let reply = runtime
        .run_turn(&case.chat_input(), &mut sink)
        .expect("回合应当成功");

    assert_eq!(
        event_tags(&sink.events),
        vec![
            "usage",
            "text",
            "tool_started",
            "tool_completed",
            "finished"
        ]
    );
    assert_eq!(reply.content, "你好");
    assert_eq!(reply.finish_reason, "STOP");
    assert_eq!(reply.usage.expect("应有用量").input_tokens, 3);
    assert_eq!(reply.tool_calls.len(), 1);
    assert_eq!(reply.tool_calls[0].call_id, "gemini_1");
    assert_eq!(reply.tool_calls[0].name, "read_file");
    assert_eq!(reply.tool_calls[0].arguments["path"], "a.py");

    let requests = captured.lock().expect("记录锁");
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(
        request.request_line,
        "POST /v1beta/models/gemini-2.5-flash:streamGenerateContent?alt=sse HTTP/1.1"
    );
    assert_eq!(request.header("x-goog-api-key"), Some("gemini-key"));
    assert_eq!(
        request.body["systemInstruction"]["parts"][0]["text"],
        "你是助手"
    );
    assert_eq!(request.body["contents"][0]["parts"][0]["text"], "你好");
    assert!(
        request.body["generationConfig"].is_object(),
        "非空 config 应带出 generationConfig：{}",
        request.body
    );
}

#[test]
fn model_path_gets_models_prefix() {
    let (base_url, captured) = spawn_capture_server(200, "OK", TEXT_AND_TOOL_STREAM);
    let runtime = GeminiRuntime::new(endpoint(base_url));
    let case = CaseInput::simple("models/gemini-2.5-flash", "你好");

    let mut sink = RecordingSink::default();
    runtime
        .run_turn(&case.chat_input(), &mut sink)
        .expect("回合应当成功");

    let requests = captured.lock().expect("记录锁");
    assert!(
        requests[0]
            .request_line
            .contains("/v1beta/models/gemini-2.5-flash:streamGenerateContent?alt=sse")
            && !requests[0].request_line.contains("models/models/"),
        "带前缀的模型名不应再加一层 models/：{}",
        requests[0].request_line
    );
}

#[test]
fn status_failure_uses_gemini_text() {
    let (base_url, _captured) = spawn_capture_server(
        401,
        "Unauthorized",
        "{\"error\":{\"message\":\"401 UNAUTHENTICATED\"}}",
    );
    let runtime = GeminiRuntime::new(endpoint(base_url));
    let case = CaseInput::simple("gemini-2.5-flash", "你好");

    let mut sink = RecordingSink::default();
    let error = runtime
        .run_turn(&case.chat_input(), &mut sink)
        .expect_err("非 2xx 必须失败");

    assert_eq!(error.kind, RuntimeErrorKind::RequestFailed);
    assert!(
        error.message.contains("Gemini 鉴权失败"),
        "错误文案应走 Gemini 阶梯：{}",
        error.message
    );
}

#[test]
fn missing_api_key_is_configuration_error() {
    let (base_url, _captured) = spawn_capture_server(200, "OK", TEXT_AND_TOOL_STREAM);
    let mut endpoint = endpoint(base_url);
    endpoint.api_key = String::new();
    let runtime = GeminiRuntime::new(endpoint);
    let case = CaseInput::simple("gemini-2.5-flash", "你好");

    let mut sink = RecordingSink::default();
    let error = runtime
        .run_turn(&case.chat_input(), &mut sink)
        .expect_err("缺少凭据必须直接失败");

    assert_eq!(error.kind, RuntimeErrorKind::Configuration);
    assert_eq!(sink.events.len(), 0);
}

#[test]
fn sink_cancel_stops_the_turn() {
    let (base_url, _captured) = spawn_capture_server(200, "OK", TEXT_AND_TOOL_STREAM);
    let runtime = GeminiRuntime::new(endpoint(base_url));
    let case = CaseInput::simple("gemini-2.5-flash", "你好");

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
