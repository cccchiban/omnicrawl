//! 线上工具调用 id 的端到端验收。
//!
//! 网关/中继常按「每次请求内的序号」发工具调用 id（`call_0`、`call_1`…），于是**同一个回合**
//! 里的几个批次会拿到相同的 id；这些批次最终落在**同一次请求**里，网关会把「同一请求重复提交
//! 相同的 call_id」直接判成 HTTP 400（`type: invalid_tool_state`），整段会话当场断掉。
//!
//! 这里真拉起内核进程 + 本机回环模型服务端，断言三件事：
//! ① 发出前就把同一请求里的重复 id 改写成唯一 id，且 assistant 调用与 `tool` 结果同步改写；
//! ② 改写只发生在线上的那份副本，会话转录里仍是网关原样的 id（淘汰标注、历史投影不被污染）；
//! ③ 网关明确判过「重复提交相同 call_id」时，重试前会轮换尾部一批的 id（不再原样重发）。

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::collections::BTreeSet;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use serde_json::{json, Value};

const WAIT: Duration = Duration::from_secs(15);
const TEST_KEY: &str = "test-key";
const TOOL_OUTPUT: &str = "文件内容：42";
const FINAL_TEXT: &str = "看完了";

/// SSE：一段正文 + 正常结束。
fn text_stream(text: &str) -> String {
    let delta = json!({"choices": [{"delta": {"content": text}}]}).to_string();
    let finish = json!({"choices": [{"delta": {}, "finish_reason": "stop"}]}).to_string();
    format!("data: {delta}\n\ndata: {finish}\n\ndata: [DONE]\n\n")
}

/// SSE：模型要求调用工具，call id 由脚本指定（复现网关按序号发 id）。
fn tool_call_stream(call_id: &str, name: &str, arguments: &str) -> String {
    let delta = json!({
        "choices": [{
            "delta": {"tool_calls": [{
                "index": 0,
                "id": call_id,
                "type": "function",
                "function": {"name": name, "arguments": arguments}
            }]}
        }]
    })
    .to_string();
    let finish = json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]}).to_string();
    format!("data: {delta}\n\ndata: {finish}\n\ndata: [DONE]\n\n")
}

enum Reply {
    Raw(String),
    Text(String),
    /// 指定状态码的错误体：网关的 400 就是从这里来的。
    Status(u16, String),
}

/// 本机回环服务端：按脚本依次应答，并记录收到的每个请求体。
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
                let (status, content_type, body) = match script.get(index) {
                    Some(Reply::Text(text)) => (200, "text/event-stream", text_stream(text)),
                    Some(Reply::Raw(raw)) => (200, "text/event-stream", raw.clone()),
                    Some(Reply::Status(code, message)) => (
                        *code,
                        "application/json",
                        json!({"error": {"message": message}}).to_string(),
                    ),
                    None => (200, "text/event-stream", text_stream("（脚本用尽）")),
                };
                let reply = format!(
                    "HTTP/1.1 {status} STATUS\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\
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

    fn initialize(&mut self, model: Value, root: &Path) {
        self.send(json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocol_version": "1.0",
                "client": {"name": "call-id-wire-e2e"},
                "model": model,
                "session": {"root": root.to_string_lossy()},
            },
        }));
        let response = self.next_frame();
        assert_eq!(response["id"], 1, "initialize 应答：{response}");
        assert!(
            response.get("error").is_none(),
            "initialize 失败：{response}"
        );
    }

    /// 提交回合：收到 `tool.batch` 就整批代执行，直到 `turn.finished`。
    fn run_turn(&mut self, user_text: &str) -> Vec<Value> {
        self.send(json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "turn.submit",
            "params": {"turn_id": "turn-1", "user_text": user_text},
        }));

        let mut collected = Vec::new();
        loop {
            let frame = self.next_frame();
            let method = frame["method"].as_str().unwrap_or_default().to_string();
            if method == "tool.batch" {
                let observation = observation_for(&frame["params"]);
                let id = frame["id"].clone();
                self.send(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {"observations": [observation]},
                }));
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

/// 宿主对一条工具调用的观察：原样回带调用 id，结果与要折进上下文的消息都用同一个 id。
fn observation_for(batch: &Value) -> Value {
    let call = &batch["calls"][0];
    let call_id = call["id"].as_str().unwrap_or("call-1");
    json!({
        "tool_call": call,
        "result": {"ok": true, "output": TOOL_OUTPUT, "full_output": TOOL_OUTPUT},
        "message": {"role": "tool", "tool_call_id": call_id, "content": TOOL_OUTPUT},
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

fn temp_root(name: &str) -> PathBuf {
    let root =
        std::env::temp_dir().join(format!("omnicrawl-callid-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("无法建临时会话根");
    root
}

/// 请求体里两侧的 id 出现序列：assistant 侧的 `tool_calls[].id` 与 `tool` 侧的 `tool_call_id`。
fn ids_of(body: &Value) -> (Vec<String>, Vec<String>) {
    let mut calls = Vec::new();
    let mut results = Vec::new();
    for message in body["messages"].as_array().expect("请求体里有消息数组") {
        match message["role"].as_str().unwrap_or_default() {
            "assistant" => {
                for call in message["tool_calls"].as_array().into_iter().flatten() {
                    calls.push(call["id"].as_str().unwrap_or_default().to_string());
                }
            }
            "tool" => results.push(
                message["tool_call_id"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
            ),
            _ => {}
        }
    }
    (calls, results)
}

/// 线上请求的协议体检：两侧都不许有空 id、不许重复，且必须逐一双向配对。
fn assert_protocol_valid(body: &Value, context: &str) {
    let (calls, results) = ids_of(body);
    let unique = |ids: &[String]| ids.iter().collect::<BTreeSet<_>>().len() == ids.len();
    assert!(unique(&calls), "{context}：assistant 侧 call_id 必须唯一：{calls:?}");
    assert!(
        unique(&results),
        "{context}：tool 侧 tool_call_id 必须唯一：{results:?}"
    );
    assert!(
        calls.iter().all(|id| !id.is_empty()) && results.iter().all(|id| !id.is_empty()),
        "{context}：两侧都不许有空 id：{calls:?} / {results:?}"
    );
    let call_set: BTreeSet<&String> = calls.iter().collect();
    let result_set: BTreeSet<&String> = results.iter().collect();
    assert_eq!(call_set, result_set, "{context}：调用与结果必须逐一双向配对");
}

/// 会话转录的全部文本（递归收集 jsonl）。
fn transcript(root: &Path) -> String {
    let mut sink = String::new();
    collect_jsonl(root, &mut sink);
    sink
}

fn collect_jsonl(directory: &Path, sink: &mut String) {
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

#[test]
fn repeated_call_id_within_one_request_is_repaired_before_sending() {
    // 两次工具调用都用同一个 id：网关按「请求内序号」发 id 时就是这个形状。
    let server = StubServer::spawn(vec![
        Reply::Raw(tool_call_stream("call-dup", "read_file", "{\"path\":\"a.txt\"}")),
        Reply::Raw(tool_call_stream("call-dup", "read_file", "{\"path\":\"b.txt\"}")),
        Reply::Text(FINAL_TEXT.to_string()),
    ]);
    let root = temp_root("repair");
    let mut kernel = Kernel::spawn();
    kernel.initialize(model_config(&server), &root);
    let frames = kernel.run_turn("读两个文件");

    let finished = frames
        .last()
        .expect("至少有一个 turn.finished 帧")
        .clone();
    assert_eq!(finished["method"], "turn.finished", "本回合应当正常完成：{frames:?}");
    assert_eq!(finished["params"]["final_text"], FINAL_TEXT);

    let bodies = server.bodies();
    assert_eq!(bodies.len(), 3, "两个工具批次各再问一次模型");
    // ① 第一次请求里只有一个批次，原样不动；第二次请求带上了两个批次，必须已被改写。
    // （请求 #1 是用户提问那一发，还没有任何工具调用。）
    assert_eq!(ids_of(&bodies[0]).0, Vec::<String>::new());
    assert_eq!(ids_of(&bodies[1]).0, vec!["call-dup".to_string()]);
    let (calls, results) = ids_of(&bodies[2]);
    assert_eq!(
        calls,
        vec!["call-dup".to_string(), "call_dedup1".to_string()],
        "重复的 id 必须被改写成唯一 id（首次出现保留原 id，前缀逐字不变）"
    );
    assert_eq!(
        results,
        vec!["call-dup".to_string(), "call_dedup1".to_string()],
        "工具结果必须与它的调用同步改写，配对不能走散"
    );
    for (index, body) in bodies.iter().enumerate() {
        assert_protocol_valid(body, &format!("第 {} 次请求", index + 1));
    }

    // ② 宿主看得到修复动作（否则这类改写是静默发生的）。
    assert!(
        frames.iter().any(|frame| {
            frame["method"] == json!("turn.notice")
                && frame["params"]["message"]
                    .as_str()
                    .is_some_and(|message| message.contains("工具调用 id 线上修复"))
        }),
        "修复必须留下宿主提示：{frames:?}"
    );

    // ③ 改写只在线上的副本：转录里仍是网关原样的 id。
    let transcript = transcript(&root);
    assert!(
        transcript.contains("call-dup"),
        "转录应当保留网关原样的 id：{transcript}"
    );
    assert!(
        !transcript.contains("call_dedup1"),
        "转录不该出现线上改写后的 id：{transcript}"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn tool_state_rejection_rotates_tail_ids_before_retrying() {
    // 序列：一个工具批次 → 网关以 400 判「重复提交相同 call_id」→ 重试（此时应轮换 id）→ 成功。
    let server = StubServer::spawn(vec![
        Reply::Raw(tool_call_stream("call-tail", "read_file", "{\"path\":\"a.txt\"}")),
        Reply::Status(400, "同一请求不能重复提交相同的 call_id".to_string()),
        Reply::Text(FINAL_TEXT.to_string()),
    ]);
    let root = temp_root("rotate");
    let mut kernel = Kernel::spawn();
    let mut config = model_config(&server);
    config["request_retry_count"] = json!(3);
    kernel.initialize(config, &root);
    let frames = kernel.run_turn("读文件");

    let finished = frames
        .iter()
        .find(|frame| frame["method"] == json!("turn.finished"))
        .unwrap_or_else(|| panic!("重试成功后应当收到 turn.finished：{frames:?}"));
    assert_eq!(finished["params"]["final_text"], FINAL_TEXT);

    let bodies = server.bodies();
    assert_eq!(bodies.len(), 3, "一次工具请求 + 一次被拒 + 一次重试");
    assert_eq!(
        ids_of(&bodies[1]).0,
        vec!["call-tail".to_string()],
        "被拒的那次仍是原样的 id"
    );
    let (calls, results) = ids_of(&bodies[2]);
    assert_ne!(
        calls,
        vec!["call-tail".to_string()],
        "重试前必须轮换尾部 id，否则等于把同一份提交再发一遍"
    );
    assert_eq!(
        calls.len(),
        1,
        "只轮换尾部批次，不引入新消息：{calls:?}"
    );
    assert_eq!(results, calls, "轮换后两侧仍然逐一双向配对");
    assert_protocol_valid(&bodies[2], "重试请求");
    assert!(
        calls[0].starts_with("call_r1"),
        "轮换后的 id 要能看出是重试产物：{calls:?}"
    );
    assert!(
        frames.iter().any(|frame| {
            frame["method"] == json!("turn.retry_status")
                && frame["params"]["message"]
                    .as_str()
                    .is_some_and(|message| message.contains("轮换"))
        }),
        "轮换必须留下宿主提示：{frames:?}"
    );

    let _ = std::fs::remove_dir_all(&root);
}
