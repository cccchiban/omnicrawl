//! 内核接线的端到端测试：真拉起 `omnicrawl` 进程，在 stdio 上跑协议 v1。
//!
//! 两件事必须成立：
//! 1. 宿主给了模型配置时，内核自己发模型请求（全程不出现 `model.reply`），增量经协议外发；
//! 2. 没给模型配置时，仍走 `model.reply` 代答的兼容路径——旧宿主不会因为这次改动失效。

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use serde_json::{json, Value};

const WAIT: Duration = Duration::from_secs(15);
const TEST_KEY: &str = "test-key";

/// 固定 SSE 响应：一段文本 + 结束原因，随后 [DONE]。
const TEXT_STREAM: &str = concat!(
    "data: {\"choices\":[{\"delta\":{\"content\":\"你好\"}}]}\n\n",
    "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
    "data: [DONE]\n\n",
);

/// 本机回环服务端：把固定 SSE 发给每个连接，并记录收到的请求体。
struct StubServer {
    addr: String,
    requests: Arc<Mutex<Vec<Value>>>,
}

impl StubServer {
    fn spawn(body: &'static str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("无法监听回环端口");
        let addr = listener.local_addr().expect("无法取本地地址");
        let requests: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&requests);

        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let raw = read_request(&mut stream);
                if let Ok(value) = serde_json::from_str::<Value>(&raw) {
                    recorded.lock().expect("记录锁").push(value);
                }
                let reply = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\n\
                     Connection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(reply.as_bytes());
                let _ = stream.flush();
            }
        });

        Self {
            addr: format!("http://{addr}/v1"),
            requests,
        }
    }

    fn bodies(&self) -> Vec<Value> {
        self.requests.lock().expect("记录锁").clone()
    }
}

fn read_request(stream: &mut TcpStream) -> String {
    let mut buffer: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 1024];
    let header_end = loop {
        if let Some(index) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            break index;
        }
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => return String::new(),
            Ok(read) => buffer.extend_from_slice(&chunk[..read]),
        }
    };
    let head = String::from_utf8_lossy(&buffer[..header_end]).to_string();
    let length: usize = head
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
    while buffer.len() < body_start + length {
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(read) => buffer.extend_from_slice(&chunk[..read]),
        }
    }
    let end = (body_start + length).min(buffer.len());
    String::from_utf8_lossy(&buffer[body_start..end]).to_string()
}

/// 被拉起的内核进程：写帧、读帧、退出时收尸。
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
            .expect("无法拉起内核进程");

        let stdin = child.stdin.take().expect("内核 stdin 可用");
        let stdout = child.stdout.take().expect("内核 stdout 可用");
        let (sender, frames) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if line.trim().is_empty() {
                    continue;
                }
                let Ok(value) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                if sender.send(value).is_err() {
                    break;
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
        let mut line = serde_json::to_string(&frame).expect("帧可序列化");
        line.push('\n');
        self.stdin
            .write_all(line.as_bytes())
            .and_then(|()| self.stdin.flush())
            .expect("写帧失败");
    }

    fn next_frame(&self) -> Value {
        match self.frames.recv_timeout(WAIT) {
            Ok(frame) => frame,
            Err(RecvTimeoutError::Timeout) => panic!("等待内核帧超时（{WAIT:?}）"),
            Err(RecvTimeoutError::Disconnected) => panic!("内核在应答前退出"),
        }
    }

    fn initialize(&mut self, model: Option<Value>) {
        self.initialize_with(model, false);
    }

    fn initialize_with(&mut self, model: Option<Value>, plugin_model_hooks: bool) {
        let mut params = json!({"protocol_version": "1.0", "client": {"name": "e2e"}});
        if let Some(model) = model {
            params["model"] = model;
        }
        params["plugin_model_hooks"] = json!(plugin_model_hooks);
        self.send(json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": params,
        }));
        let response = self.next_frame();
        assert_eq!(response["id"], 1, "initialize 应答：{response}");
        assert_eq!(response["result"]["protocol_version"], "1.0");
    }

    /// 提交回合并收帧，直到 `turn.finished`；返回途中所有帧。
    fn run_turn(&mut self, reply_to_model_reply: Option<Value>) -> Vec<Value> {
        self.send(json!({
            "jsonrpc": "2.0", "id": 2, "method": "turn.submit",
            "params": {"turn_id": "turn-1", "user_text": "你好"},
        }));

        let mut collected = Vec::new();
        loop {
            let frame = self.next_frame();
            let method = frame["method"].as_str().unwrap_or_default().to_string();
            if method == "model.reply" {
                let reply = reply_to_model_reply
                    .clone()
                    .expect("没给模型配置时应当走 model.reply，但用例没有准备应答");
                let id = frame["id"].clone();
                self.send(json!({"jsonrpc": "2.0", "id": id, "result": reply}));
                collected.push(frame);
                continue;
            }
            let finished = method == "turn.finished";
            collected.push(frame);
            if finished {
                return collected;
            }
        }
    }
}

impl Drop for Kernel {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn model_config(server: &StubServer) -> Value {
    json!({
        "model": "e2e-model",
        "base_url": server.addr,
        "api_key_env": "OMNICRAWL_TEST_KEY",
        "user_agent": "omnicrawl-e2e",
        "system_prompt": "你是助手。",
        "tools": [{
            "type": "function",
            "function": {"name": "read_file", "description": "读文件", "parameters": {"type": "object"}}
        }],
        "options": {"temperature": 0.2},
        "request_timeout_seconds": 5,
    })
}

fn methods(frames: &[Value]) -> Vec<String> {
    frames
        .iter()
        .map(|frame| frame["method"].as_str().unwrap_or_default().to_string())
        .collect()
}

#[test]
fn kernel_calls_the_model_itself_when_configured() {
    let server = StubServer::spawn(TEXT_STREAM);
    let mut kernel = Kernel::spawn();
    kernel.initialize(Some(model_config(&server)));
    let frames = kernel.run_turn(None);

    let seen = methods(&frames);
    assert!(
        !seen.iter().any(|method| method == "model.reply"),
        "内核自己发请求时不应再出现 model.reply：{seen:?}"
    );
    assert!(
        seen.iter().any(|method| method == "turn.delta"),
        "增量应经协议外发：{seen:?}"
    );
    let finished = frames.last().expect("至少有一个 turn.finished 帧");
    assert_eq!(finished["method"], "turn.finished");
    assert_eq!(finished["params"]["final_text"], "你好");
    assert_eq!(finished["params"]["turn_id"], "turn-1");

    // 内核实际发出去的请求体：模型、消息、流式开关都在，且没有 timeout 字段。
    let bodies = server.bodies();
    assert_eq!(bodies.len(), 1, "一次回合只应发一次请求");
    assert_eq!(bodies[0]["model"], "e2e-model");
    assert_eq!(bodies[0]["stream"], true);
    assert!(bodies[0].get("timeout").is_none(), "timeout 不进请求体");
    assert_eq!(bodies[0]["messages"][0]["role"], "system");
    assert_eq!(bodies[0]["messages"][0]["content"], "你是助手。");
    assert_eq!(bodies[0]["messages"][1]["content"], "你好");
    assert_eq!(bodies[0]["tools"][0]["function"]["name"], "read_file");
    assert_eq!(bodies[0]["temperature"], 0.2);

    // 应答帧：turn.submit 的响应在 turn.finished 之后到达，这里只确认它存在。
    let header = kernel.next_frame();
    assert_eq!(header["id"], 2);
}

#[test]
fn kernel_runs_model_request_hook_when_host_declares_capability() {
    let server = StubServer::spawn(TEXT_STREAM);
    let mut kernel = Kernel::spawn();
    kernel.initialize_with(Some(model_config(&server)), true);
    kernel.send(json!({
        "jsonrpc": "2.0", "id": 2, "method": "turn.submit",
        "params": {"turn_id": "turn-1", "user_text": "你好"},
    }));

    let mut hook_seen = false;
    loop {
        let frame = kernel.next_frame();
        let method = frame["method"].as_str().unwrap_or_default().to_string();
        if method == "model.hook" {
            hook_seen = true;
            assert_eq!(frame["params"]["model"], "e2e-model");
            // 改写第一条（唯一的用户）消息，验证改写真的进了模型请求。
            let mut messages = frame["params"]["messages"].clone();
            messages[0]["content"] = json!("你好（插件改写）");
            let id = frame["id"].clone();
            kernel.send(json!({"jsonrpc": "2.0", "id": id, "result": {"messages": messages}}));
            continue;
        }
        if method == "turn.finished" {
            break;
        }
    }
    assert!(hook_seen, "声明能力后应在模型请求前收到 model.hook");

    let bodies = server.bodies();
    assert_eq!(bodies.len(), 1, "一次回合只应发一次请求");
    assert_eq!(
        bodies[0]["messages"][1]["content"], "你好（插件改写）",
        "插件改写的消息应进模型请求"
    );
}

#[test]
fn kernel_skips_model_request_hook_without_capability() {
    let server = StubServer::spawn(TEXT_STREAM);
    let mut kernel = Kernel::spawn();
    // 不声明 plugin_model_hooks：内核不发 model.hook，回合照常完成。
    kernel.initialize(Some(model_config(&server)));
    let frames = kernel.run_turn(None);
    let seen = methods(&frames);
    assert!(
        !seen.iter().any(|method| method == "model.hook"),
        "未声明能力时不应发 model.hook：{seen:?}"
    );
    assert_eq!(server.bodies().len(), 1);
}

#[test]
fn host_without_model_config_still_uses_model_reply() {
    let mut kernel = Kernel::spawn();
    kernel.initialize(None);
    let frames = kernel.run_turn(Some(json!({
        "message": {"role": "assistant", "content": "宿主代答"},
        "content": "宿主代答",
        "tool_calls": [],
        "reasoning": "",
        "content_streamed": false,
    })));

    let seen = methods(&frames);
    assert!(
        seen.iter().any(|method| method == "model.reply"),
        "没有模型配置时应退回代答路径：{seen:?}"
    );
    let finished = frames.last().expect("至少有一个 turn.finished 帧");
    assert_eq!(finished["params"]["final_text"], "宿主代答");
}
