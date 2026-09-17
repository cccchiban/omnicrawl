#![allow(dead_code)]

//! 内核测试共用的脚手架：fixture 输入、回环 HTTP 服务端与事件接收端。
//!
//! 集成测试各自是独立 crate，共用代码只有放在这里才不会重复；
//! 未被某个测试用到的项由文件头的 `allow(dead_code)` 兜住。

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use omnicrawl_llm::{ChatRequestInput, SinkFlow, TurnSink};
use omnicrawl_protocol::{
    ConversationMessage, GenerationOptions, MessageBlock, ModelStreamEvent, Role, TextBlock,
    ToolSpec,
};
use serde::Deserialize;
use serde_json::Value;

/// 与 `ChatRequestInput` 同形的 fixture 输入。
#[derive(Debug, Deserialize)]
pub struct CaseInput {
    pub model: String,
    pub system_prompt: String,
    #[serde(default)]
    pub messages: Vec<ConversationMessage>,
    #[serde(default)]
    pub tools: Vec<ToolSpec>,
    #[serde(default)]
    pub options: GenerationOptions,
    pub profile_request_timeout_seconds: f64,
    #[serde(default)]
    pub prompt_cache_capable: bool,
    #[serde(default)]
    pub prompt_cache_identity: BTreeMap<String, String>,
}

impl CaseInput {
    /// 单条 user 文本、默认生成选项的最小用例。
    pub fn simple(model: &str, text: &str) -> Self {
        Self {
            model: model.to_string(),
            system_prompt: String::new(),
            messages: user_messages(text),
            tools: Vec::new(),
            options: GenerationOptions::default(),
            profile_request_timeout_seconds: 30.0,
            prompt_cache_capable: false,
            prompt_cache_identity: BTreeMap::new(),
        }
    }

    pub fn chat_input(&self) -> ChatRequestInput<'_> {
        ChatRequestInput {
            model: &self.model,
            system_prompt: &self.system_prompt,
            messages: &self.messages,
            tools: &self.tools,
            options: &self.options,
            profile_request_timeout_seconds: self.profile_request_timeout_seconds,
            prompt_cache_capable: self.prompt_cache_capable,
            prompt_cache_identity: &self.prompt_cache_identity,
        }
    }
}

pub fn user_messages(text: &str) -> Vec<ConversationMessage> {
    vec![ConversationMessage {
        role: Role::User,
        blocks: vec![MessageBlock::Text(TextBlock::new(text))],
        reasoning: String::new(),
        tools: Vec::new(),
    }]
}

/// 一次回环响应：先写出 `head`，等 `hold` 之后再写出 `tail`。
///
/// `Content-Length` 声明为两段之和，因此中途停住即模拟「连接还在但没数据」。
pub struct Reply {
    pub status: u16,
    pub head: String,
    pub tail: String,
    pub hold: Duration,
}

impl Reply {
    pub fn sse(status: u16, body: &str) -> Self {
        Self {
            status,
            head: body.to_string(),
            tail: String::new(),
            hold: Duration::ZERO,
        }
    }

    pub fn held(status: u16, head: &str, hold: Duration) -> Self {
        Self {
            status,
            head: head.to_string(),
            tail: String::new(),
            hold,
        }
    }
}

/// 按顺序给每次连接一个响应的本机 HTTP 服务端；响应用完线程即退出。
pub struct StubServer {
    addr: SocketAddr,
    requests: Arc<Mutex<Vec<Value>>>,
    handle: Option<JoinHandle<()>>,
}

impl StubServer {
    pub fn spawn(replies: Vec<Reply>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("无法监听回环端口");
        let addr = listener.local_addr().expect("无法取本地地址");
        let requests: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&requests);

        let handle = thread::spawn(move || {
            for reply in replies {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                let raw = read_request(&mut stream);
                if let Ok(value) = serde_json::from_str::<Value>(&raw) {
                    recorded.lock().expect("记录锁").push(value);
                }
                let length = reply.head.len() + reply.tail.len();
                let header = format!(
                    "HTTP/1.1 {status} {reason}\r\nContent-Type: text/event-stream\r\n\
                     Content-Length: {length}\r\nConnection: close\r\n\r\n",
                    status = reply.status,
                    reason = reason_of(reply.status),
                );
                let _ = stream.write_all(header.as_bytes());
                let _ = stream.write_all(reply.head.as_bytes());
                let _ = stream.flush();
                if !reply.hold.is_zero() {
                    thread::sleep(reply.hold);
                }
                let _ = stream.write_all(reply.tail.as_bytes());
                let _ = stream.flush();
            }
        });

        Self {
            addr,
            requests,
            handle: Some(handle),
        }
    }

    pub fn base_url(&self) -> String {
        format!("http://{}/v1", self.addr)
    }

    pub fn bodies(&self) -> Vec<Value> {
        self.requests.lock().expect("记录锁").clone()
    }
}

impl Drop for StubServer {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn reason_of(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        429 => "Too Many Requests",
        _ => "Service Unavailable",
    }
}

fn read_request(stream: &mut TcpStream) -> String {
    let mut buffer: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 1024];

    let header_end = loop {
        if let Some(index) = find_header_end(&buffer) {
            break index;
        }
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => return String::new(),
            Ok(read) => buffer.extend_from_slice(&chunk[..read]),
        }
    };

    let head = String::from_utf8_lossy(&buffer[..header_end]).to_string();
    let content_length: usize = head
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            if name.eq_ignore_ascii_case("content-length") {
                value.trim().parse().ok()
            } else {
                None
            }
        })
        .unwrap_or(0);

    let body_start = header_end + 4;
    while buffer.len() < body_start + content_length {
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(read) => buffer.extend_from_slice(&chunk[..read]),
        }
    }
    let body_end = (body_start + content_length).min(buffer.len());
    String::from_utf8_lossy(&buffer[body_start..body_end]).to_string()
}

fn find_header_end(buffer: &[u8]) -> Option<usize> {
    buffer.windows(4).position(|window| window == b"\r\n\r\n")
}

/// 记录流事件、并可按条件取消的接收端。
#[derive(Default)]
pub struct RecordingSink {
    pub events: Vec<ModelStreamEvent>,
    pub cancel_after: Option<usize>,
    pub cancel_now: bool,
}

impl TurnSink for RecordingSink {
    fn on_event(&mut self, event: ModelStreamEvent) -> SinkFlow {
        self.events.push(event);
        match self.cancel_after {
            Some(limit) if self.events.len() >= limit => SinkFlow::Cancel,
            _ => SinkFlow::Continue,
        }
    }

    fn cancelled(&self) -> bool {
        self.cancel_now
    }
}

/// 事件序列的紧凑投影，便于与 fixture 对照。
pub fn event_tags(events: &[ModelStreamEvent]) -> Vec<&'static str> {
    events
        .iter()
        .map(|event| match event {
            ModelStreamEvent::TextDelta(_) => "text",
            ModelStreamEvent::ReasoningDelta(_) => "reasoning",
            ModelStreamEvent::ToolCallStarted(_) => "tool_started",
            ModelStreamEvent::ToolCallArgumentsDelta(_) => "tool_arguments",
            ModelStreamEvent::ToolCallCompleted(_) => "tool_completed",
            ModelStreamEvent::UsageReported(_) => "usage",
            ModelStreamEvent::Finished { .. } => "finished",
            ModelStreamEvent::ProviderWarning(_) => "warning",
        })
        .collect()
}

/// 把内核事件投影成 fixture 的 JSON 形状；与生成脚本的 `event_to_json` 一一对应。
pub fn event_to_json(event: &ModelStreamEvent) -> Value {
    match event {
        ModelStreamEvent::TextDelta(delta) => {
            serde_json::json!({"kind": "text", "text": delta.text})
        }
        ModelStreamEvent::ReasoningDelta(delta) => {
            serde_json::json!({"kind": "reasoning", "text": delta.text})
        }
        ModelStreamEvent::ToolCallStarted(started) => serde_json::json!({
            "kind": "tool_started",
            "call_id": started.call_id,
            "name": started.name,
        }),
        ModelStreamEvent::ToolCallArgumentsDelta(delta) => serde_json::json!({
            "kind": "tool_arguments",
            "call_id": delta.call_id,
            "delta": delta.delta,
        }),
        ModelStreamEvent::ToolCallCompleted(completed) => serde_json::json!({
            "kind": "tool_completed",
            "call_id": completed.call_id,
            "name": completed.name,
            "arguments": completed.arguments,
        }),
        ModelStreamEvent::UsageReported(usage) => serde_json::json!({
            "kind": "usage",
            "input_tokens": usage.input_tokens,
            "output_tokens": usage.output_tokens,
            "cached_input_tokens": usage.cached_input_tokens,
            "reasoning_tokens": usage.reasoning_tokens,
        }),
        ModelStreamEvent::Finished { finish_reason } => {
            serde_json::json!({"kind": "finished", "finish_reason": finish_reason})
        }
        ModelStreamEvent::ProviderWarning(warning) => serde_json::json!({
            "kind": "warning",
            "code": warning.code,
            "message": warning.message,
        }),
    }
}
