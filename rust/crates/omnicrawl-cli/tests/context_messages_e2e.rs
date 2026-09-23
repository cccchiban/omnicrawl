//! 上下文消息进入真实请求路径的端到端验收。
//!
//! 阶段一的要害不在「能不能拼出这几条消息」，而在**宿主装配的文本真的进了模型请求**：
//! 这里真拉起内核进程 + 本机回环模型服务端，宿主按协议送 `initialize.model`（含
//! `system_prompt` 与 `context_messages`），然后断言请求体里的消息顺序是
//! 「system → 上下文消息（原顺序）→ 历史/用户输入」，且上下文消息**没有**被写进会话转录。

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

const WAIT: Duration = Duration::from_secs(15);
const TEST_KEY: &str = "test-key";
const TEXT_STREAM: &str = concat!(
    "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"你好\"}}]}\n\n",
    "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
    "data: [DONE]\n\n",
);

/// 记录请求体的假模型服务端：固定回一段文本。
struct StubServer {
    base_url: String,
    bodies: Arc<Mutex<Vec<Value>>>,
}

impl StubServer {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("绑定回环端口");
        let port = listener.local_addr().expect("本地地址").port();
        let bodies = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&bodies);
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let sink = Arc::clone(&sink);
                thread::spawn(move || serve(&mut stream, &sink));
            }
        });
        Self {
            base_url: format!("http://127.0.0.1:{port}/v1"),
            bodies,
        }
    }

    fn bodies(&self) -> Vec<Value> {
        self.bodies.lock().expect("请求体锁").clone()
    }
}

fn serve(stream: &mut TcpStream, sink: &Arc<Mutex<Vec<Value>>>) {
    let mut reader = BufReader::new(stream.try_clone().expect("克隆连接"));
    let mut content_length = 0usize;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            return;
        }
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            break;
        }
        if let Some(value) = trimmed
            .strip_prefix("content-length: ")
            .or_else(|| trimmed.strip_prefix("Content-Length: "))
        {
            content_length = value.trim().parse().unwrap_or(0);
        }
    }
    let mut body = vec![0u8; content_length];
    if reader.read_exact(&mut body).is_err() {
        return;
    }
    if let Ok(value) = serde_json::from_slice::<Value>(&body) {
        sink.lock().expect("请求体锁").push(value);
    }
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{TEXT_STREAM}",
        TEXT_STREAM.len()
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}

struct Kernel {
    child: Child,
    stdin: ChildStdin,
    frames: Receiver<Value>,
}

impl Kernel {
    fn spawn() -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_omnicrawl"))
            .env("OMNICRAWL_TEST_KEY", TEST_KEY)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("拉起内核进程");
        let stdin = child.stdin.take().expect("内核 stdin 可用");
        let stdout = child.stdout.take().expect("内核 stdout 可用");
        let (sender, frames) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if let Ok(value) = serde_json::from_str::<Value>(&line) {
                    let _ = sender.send(value);
                }
            }
        });
        Self {
            child,
            stdin,
            frames,
        }
    }

    fn send(&mut self, frame: Value) {
        let line = serde_json::to_string(&frame).expect("序列化帧");
        writeln!(self.stdin, "{line}").expect("写帧");
        self.stdin.flush().expect("刷新帧");
    }

    fn next_frame(&mut self) -> Value {
        self.frames.recv_timeout(WAIT).expect("在超时前收到内核帧")
    }
}

impl Drop for Kernel {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn initialize(kernel: &mut Kernel, model: Value, session_root: Option<&str>) {
    let mut params = json!({"protocol_version": "1.0", "model": model});
    if let Some(root) = session_root {
        params["session"] = json!({"root": root, "workspace_root": root});
    }
    kernel.send(json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": params}));
    let frame = kernel.next_frame();
    assert_eq!(frame["id"], 1, "握手应回同一 id：{frame}");
    assert!(frame.get("error").is_none(), "握手不应失败：{frame}");
}

fn submit(kernel: &mut Kernel, turn_id: &str, text: &str) {
    kernel.send(json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "turn.submit",
        "params": {"turn_id": turn_id, "user_text": text},
    }));
    let deadline = Instant::now() + WAIT;
    while Instant::now() < deadline {
        let frame = kernel.next_frame();
        if frame["method"] == "turn.finished" {
            assert_eq!(frame["params"]["final_text"], "你好");
            return;
        }
    }
    panic!("等不到 turn.finished");
}

#[test]
fn context_messages_reach_the_model_request_in_order() {
    let server = StubServer::start();
    let mut kernel = Kernel::spawn();
    let context_messages = vec![
        json!({"role": "user", "content": "<project_instructions>项目规范</project_instructions>"}),
        json!({"role": "user", "content": "<runtime_context>运行环境</runtime_context>"}),
    ];
    initialize(
        &mut kernel,
        json!({
            "model": "e2e-model",
            "base_url": server.base_url,
            "api_key_env": "OMNICRAWL_TEST_KEY",
            "user_agent": "omnicrawl-e2e",
            "system_prompt": "你是助手。",
            "context_messages": context_messages,
            "options": {"temperature": 0.2},
        }),
        None,
    );
    submit(&mut kernel, "turn-1", "你好");

    let bodies = server.bodies();
    assert_eq!(bodies.len(), 1, "一次回合只应发一次请求");
    let messages = bodies[0]["messages"].as_array().expect("消息数组");
    assert_eq!(messages[0]["role"], "system");
    assert_eq!(messages[0]["content"], "你是助手。");
    assert_eq!(messages[1]["role"], "user");
    assert_eq!(
        messages[1]["content"],
        "<project_instructions>项目规范</project_instructions>"
    );
    assert_eq!(
        messages[2]["content"],
        "<runtime_context>运行环境</runtime_context>"
    );
    assert_eq!(messages[3]["content"], "你好");
    assert_eq!(
        messages.len(),
        4,
        "上下文消息必须插在历史之前：{messages:?}"
    );
}

#[test]
fn context_messages_are_not_persisted_into_the_transcript() {
    let server = StubServer::start();
    let root = std::env::temp_dir().join(format!(
        "oc-context-e2e-{}-{}",
        std::process::id(),
        Instant::now().elapsed().as_nanos()
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("建立会话根");

    let mut kernel = Kernel::spawn();
    initialize(
        &mut kernel,
        json!({
            "model": "e2e-model",
            "base_url": server.base_url,
            "api_key_env": "OMNICRAWL_TEST_KEY",
            "user_agent": "omnicrawl-e2e",
            "system_prompt": "你是助手。",
            "context_messages": [
                {"role": "user", "content": "<runtime_context>运行环境</runtime_context>"}
            ],
        }),
        Some(&root.to_string_lossy()),
    );
    submit(&mut kernel, "turn-1", "你好");

    // 转录里只应有 user_message 与 assistant_message，没有上下文消息。
    let mut transcript = String::new();
    collect_jsonl(&root, &mut transcript);
    assert!(
        transcript.contains("user_message"),
        "转录应写入用户消息：{transcript}"
    );
    assert!(
        !transcript.contains("runtime_context"),
        "上下文消息不应落盘：{transcript}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

fn collect_jsonl(directory: &std::path::Path, sink: &mut String) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_jsonl(&path, sink);
        } else if path.extension().and_then(|value| value.to_str()) == Some("jsonl") {
            sink.push_str(&std::fs::read_to_string(&path).unwrap_or_default());
        }
    }
}
