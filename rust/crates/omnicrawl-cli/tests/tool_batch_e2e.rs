//! 内核与宿主之间「工具批次」的端到端验收：内核发起 `tool.batch`，宿主整批执行后回观察，
//! 内核把观察折回上下文并继续问模型。
//!
//! 这是协议里最容易含糊的一条边界——工具实现目前不在内核里（`omnicrawl-tui/src/tools/`），
//! 内核只做请求方。既有测试只断言过「本地证据工具不该占宿主批次」，**没有任何 e2e 证明**
//! 远端工具那条主链路（请求形状 → 宿主结果解析 → 观察回填 → 下一轮请求带工具结果）。
//!
//! 判据全在协议帧与内核实际发出的模型请求体上，不依赖真实工具实现。

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use serde_json::{json, Value};

const WAIT: Duration = Duration::from_secs(15);
const TEST_KEY: &str = "test-key";
const TOOL_OUTPUT: &str = "文件内容：42";
const FINAL_TEXT: &str = "文件里写的是 42";

/// 固定 SSE：一段文本 + 结束原因，随后 [DONE]。
fn make_stream(text: &str) -> String {
    let delta = json!({"choices": [{"delta": {"content": text}}]}).to_string();
    let finish = json!({"choices": [{"delta": {}, "finish_reason": "stop"}]}).to_string();
    format!("data: {delta}\n\ndata: {finish}\n\ndata: [DONE]\n\n")
}

/// SSE：模型要求调用某个工具。
fn tool_call_stream(name: &str, arguments: &str) -> String {
    let delta = json!({
        "choices": [{
            "delta": {"tool_calls": [{
                "index": 0, "id": "call-1", "type": "function",
                "function": {"name": name, "arguments": arguments}
            }]}
        }]
    })
    .to_string();
    let finish = json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]}).to_string();
    format!("data: {delta}\n\ndata: {finish}\n\ndata: [DONE]\n\n")
}

enum Reply {
    Text(String),
    Raw(String),
}

/// 本机回环服务端：按脚本依次应答，并记录收到的请求体。
struct StubServer {
    addr: String,
    requests: Arc<Mutex<Vec<Value>>>,
}

impl StubServer {
    fn spawn(script: Vec<Reply>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("无法监听回环端口");
        let addr = listener.local_addr().expect("无法取本地地址");
        let requests: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&requests);

        thread::spawn(move || {
            for (index, stream) in listener.incoming().enumerate() {
                let Ok(mut stream) = stream else { break };
                let raw = read_request(&mut stream);
                if let Ok(value) = serde_json::from_str::<Value>(&raw) {
                    recorded.lock().expect("记录锁").push(value);
                }
                let body = match script.get(index) {
                    Some(Reply::Text(text)) => make_stream(text),
                    Some(Reply::Raw(raw)) => raw.clone(),
                    None => make_stream("（脚本用尽）"),
                };
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

    fn initialize(&mut self, model: Value, session: Value) {
        self.send(json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocol_version": "1.0",
                "client": {"name": "tool-batch-e2e"},
                "model": model,
                "session": session,
            },
        }));
        let response = self.next_frame();
        assert_eq!(response["id"], 1, "initialize 应答：{response}");
        assert!(
            response.get("error").is_none(),
            "initialize 失败：{response}"
        );
    }

    /// 提交回合：收到 `tool.batch` 就整批代执行（返回固定输出），直到 `turn.finished`。
    ///
    /// 返回途中所有帧与每个工具批次的请求参数。
    fn run_turn(&mut self, user_text: &str, tool_output: &str) -> (Vec<Value>, Vec<Value>) {
        self.run_turn_with(user_text, |params| observation_for(params, tool_output))
    }

    /// 与 [`Kernel::run_turn`] 同一流程，观察由调用方给出（用于回填拒绝这类非成功结果）。
    fn run_turn_with(
        &mut self,
        user_text: &str,
        reply: impl Fn(&Value) -> Value,
    ) -> (Vec<Value>, Vec<Value>) {
        self.send(json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "turn.submit",
            "params": {"turn_id": "turn-1", "user_text": user_text},
        }));

        let mut collected = Vec::new();
        let mut batches = Vec::new();
        loop {
            let frame = self.next_frame();
            let method = frame["method"].as_str().unwrap_or_default().to_string();
            if method == "tool.batch" {
                let params = frame["params"].clone();
                batches.push(params.clone());
                let id = frame["id"].clone();
                let observation = reply(&params);
                self.send(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {"observations": [observation]},
                }));
            } else {
                assert_ne!(
                    method, "model.reply",
                    "已给模型配置，内核不应走代答路径：{frame}"
                );
            }
            let finished = method == "turn.finished";
            collected.push(frame);
            if finished {
                return (collected, batches);
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

/// 宿主对一条工具调用的观察：原样回带调用、结果与要折进上下文的消息。
fn observation_for(batch: &Value, output: &str) -> Value {
    let call = &batch["calls"][0];
    let call_id = call["id"].as_str().unwrap_or("call-1");
    json!({
        "tool_call": call,
        "result": {"ok": true, "output": output, "full_output": output},
        "message": {"role": "tool", "tool_call_id": call_id, "content": output},
        "followup_messages": [],
    })
}

fn model_config(server: &StubServer) -> Value {
    json!({
        "model": "e2e-model",
        "base_url": server.addr,
        "api_key_env": "OMNICRAWL_TEST_KEY",
        "system_prompt": "你是助手。",
        "tools": [{
            "type": "function",
            "function": {
                "name": "read_file",
                "description": "读文件",
                "parameters": {"type": "object", "properties": {"path": {"type": "string"}}}
            }
        }],
        "request_timeout_seconds": 5,
    })
}

/// 会话根：名字带用例名，避免并行用例互相清理。
fn temp_root(name: &str) -> PathBuf {
    let root =
        std::env::temp_dir().join(format!("omnicrawl-toolbatch-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("无法建临时会话根");
    root
}

fn root_param(root: &Path) -> String {
    root.to_string_lossy().to_string()
}

fn pairs(body: &Value) -> Vec<(String, String)> {
    body["messages"]
        .as_array()
        .expect("请求体里有消息数组")
        .iter()
        .map(|message| {
            (
                message["role"].as_str().unwrap_or_default().to_string(),
                message["content"].as_str().unwrap_or_default().to_string(),
            )
        })
        .collect()
}

#[test]
fn kernel_hands_the_batch_to_the_host_and_folds_results_back() {
    let server = StubServer::spawn(vec![
        Reply::Raw(tool_call_stream("read_file", "{\"path\":\"a.txt\"}")),
        Reply::Text(FINAL_TEXT.to_string()),
    ]);
    let root = temp_root("handoff");

    let mut kernel = Kernel::spawn();
    kernel.initialize(model_config(&server), json!({"root": root_param(&root)}));
    let (frames, batches) = kernel.run_turn("读一下 a.txt", TOOL_OUTPUT);

    // ① 内核把整批交给宿主，自己不动手：请求形状必须是宿主能直接执行的那一份。
    assert_eq!(batches.len(), 1, "应恰好发一次工具批次，实收帧：{frames:?}");
    let batch = &batches[0];
    assert_eq!(batch["turn_id"], "turn-1");
    assert_eq!(batch["step"], 1, "第一步从 1 开始");
    assert_eq!(batch["calls"][0]["name"], "read_file");
    assert_eq!(batch["calls"][0]["arguments"]["path"], "a.txt");
    assert_eq!(batch["calls"][0]["id"], "call-1");

    // ② 宿主回的观察被折回上下文：第二次模型请求里带上了工具结果。
    let bodies = server.bodies();
    assert_eq!(
        bodies.len(),
        2,
        "工具执行后应再问模型一次，实收请求数：{}",
        bodies.len()
    );
    let second = pairs(&bodies[1]);
    assert!(
        second
            .iter()
            .any(|(role, content)| role == "tool" && content == TOOL_OUTPUT),
        "第二次请求应带宿主返回的工具结果：{second:?}"
    );
    assert!(
        second
            .iter()
            .any(|(role, content)| role == "user" && content == "读一下 a.txt"),
        "用户输入仍应在上下文里：{second:?}"
    );

    // ③ 回合结果统计了这次调用，最终文本来自第二次模型响应。
    let finished = frames.last().expect("至少有一个 turn.finished 帧");
    assert_eq!(finished["method"], "turn.finished");
    assert_eq!(finished["params"]["tool_calls"], 1);
    assert_eq!(finished["params"]["final_text"], FINAL_TEXT);

    let _ = std::fs::remove_dir_all(&root);
}

/// 宿主拒绝的观察：拒绝原因走 `result.output`，错误码标记为拒绝。
fn observation_for_denied(batch: &Value, reason: &str) -> Value {
    let call = &batch["calls"][0];
    let call_id = call["id"].as_str().unwrap_or("call-1");
    json!({
        "tool_call": call,
        "result": {
            "ok": false,
            "output": reason,
            "full_output": reason,
            "error_code": omnicrawl_ipc::DENIED_ERROR_CODE,
        },
        "message": {"role": "tool", "tool_call_id": call_id, "content": reason},
        "followup_messages": [],
    })
}

/// 会话转录全文：会话布局由内核决定，这里递归收集所有 `.jsonl`。
fn session_transcript(root: &Path) -> String {
    let mut text = String::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
            } else if path.extension().map(|ext| ext == "jsonl").unwrap_or(false) {
                text.push_str(&std::fs::read_to_string(&path).unwrap_or_default());
            }
        }
    }
    text
}

#[test]
fn kernel_records_denied_call_in_the_session() {
    let server = StubServer::spawn(vec![
        Reply::Raw(tool_call_stream("bash", "{\"command\":\"rm -rf build\"}")),
        Reply::Text(FINAL_TEXT.to_string()),
    ]);
    let root = temp_root("denied");

    let mut kernel = Kernel::spawn();
    kernel.initialize(model_config(&server), json!({"root": root_param(&root)}));
    let (frames, batches) = kernel.run_turn_with("清一下 build", |params| {
        observation_for_denied(params, "用户取消执行：bash。")
    });

    // ① 拒绝照旧走同一个批次通道，宿主不必额外通知内核。
    assert_eq!(batches.len(), 1, "应恰好发一次工具批次，实收帧：{frames:?}");
    assert_eq!(batches[0]["calls"][0]["name"], "bash");

    // ② 拒绝原因被折回上下文：第二次模型请求里带着它。
    let bodies = server.bodies();
    assert_eq!(bodies.len(), 2, "实收请求数：{}", bodies.len());
    let second = pairs(&bodies[1]);
    assert!(
        second
            .iter()
            .any(|(role, content)| role == "tool" && content == "用户取消执行：bash。"),
        "第二次请求应带拒绝原因：{second:?}"
    );

    // ③ 内核把拒绝写进会话转录，投影与历史据此还原「用户拒绝执行」。
    let transcript = session_transcript(&root);
    assert!(
        transcript.contains("tool_call_denied"),
        "会话转录应有 tool_call_denied 事件：{transcript}"
    );
    assert!(
        transcript.contains("用户取消执行：bash。"),
        "事件里应带拒绝原因：{transcript}"
    );

    let _ = std::fs::remove_dir_all(&root);
}
