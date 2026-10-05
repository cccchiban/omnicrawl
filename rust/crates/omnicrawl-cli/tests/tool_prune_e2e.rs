//! 「老登淘汰」的端到端验收：内核把**刚变老**的那一批调用交宿主裁决，判为无用的整组移出上下文。
//!
//! 两条硬约束正是本文件要钉住的：
//! * **淘汰点贴近尾部**：最新一批（正在被使用的「小登」）从不送审，只有上一批进 `context.prune`；
//! * **按整组淘汰**：被判无用的调用，其 `tool_calls` 项与 `tool` 结果必须同时消失在下一轮请求里，
//!   协议不留半截。
//!
//! 判据全在协议帧、内核实际发出的模型请求体与会话转录上，不依赖真实工具实现。
//!
//! 淘汰运行期在宿主侧（决策渠道、脱敏、开关都在那里），因此这里用一个「脚本化宿主」扮演它：
//! 声明能力、对每个 `context.prune` 按脚本回一组调用 ID。

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

/// 固定 SSE：一段文本 + 结束原因，随后 [DONE]。
fn text_stream(text: &str) -> String {
    let delta = json!({"choices": [{"delta": {"content": text}}]}).to_string();
    let finish = json!({"choices": [{"delta": {}, "finish_reason": "stop"}]}).to_string();
    format!("data: {delta}\n\ndata: {finish}\n\ndata: [DONE]\n\n")
}

/// SSE：一次模型回复要求调用若干个工具（同一批，带各自独立的调用 ID）。
fn tool_batch_stream(calls: &[(&str, &str, &str)]) -> String {
    let tool_calls: Vec<Value> = calls
        .iter()
        .enumerate()
        .map(|(index, (call_id, name, arguments))| {
            json!({
                "index": index,
                "id": call_id,
                "type": "function",
                "function": {"name": name, "arguments": arguments},
            })
        })
        .collect();
    let delta = json!({"choices": [{"delta": {"tool_calls": tool_calls}}]}).to_string();
    let finish = json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]}).to_string();
    format!("data: {delta}\n\ndata: {finish}\n\ndata: [DONE]\n\n")
}

/// 本机回环服务端：按脚本依次应答，并记录收到的请求体。
struct StubServer {
    addr: String,
    requests: Arc<Mutex<Vec<Value>>>,
}

impl StubServer {
    fn spawn(script: Vec<String>) -> Self {
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
                let body = script
                    .get(index)
                    .cloned()
                    .unwrap_or_else(|| text_stream("（脚本用尽）"));
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
    let end = std::cmp::min(buffer.len(), body_start + length);
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

    fn initialize(&mut self, model: Value, session: Value, prune_capable: bool) {
        self.send(json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocol_version": "1.0",
                "client": {"name": "tool-prune-e2e"},
                "model": model,
                "session": session,
                "tool_call_prune": prune_capable,
            },
        }));
        let response = self.next_frame();
        assert_eq!(response["id"], 1, "initialize 应答：{response}");
        assert!(
            response.get("error").is_none(),
            "initialize 失败：{response}"
        );
    }
}

impl Drop for Kernel {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// 一轮跑完的结果：途中收到的帧、每个工具批次的参数、每次淘汰裁决的送审负载。
#[derive(Debug, Default)]
struct TurnTraffic {
    frames: Vec<Value>,
    batches: Vec<Value>,
    prunes: Vec<Value>,
}

/// 跑一轮：宿主侧按「整批代执行 + 按脚本裁决淘汰」应答，直到 `turn.finished`。
///
/// `evict` 按 `context.prune` 的送审负载给出要淘汰的调用 ID（`None` 表示不淘汰任何一组）。
fn run_turn_with_prune(
    kernel: &mut Kernel,
    user_text: &str,
    evict: impl Fn(&Value) -> Option<Vec<String>>,
) -> TurnTraffic {
    kernel.send(json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "turn.submit",
        "params": {"turn_id": "turn-1", "user_text": user_text},
    }));

    let mut traffic = TurnTraffic::default();
    loop {
        let frame = kernel.next_frame();
        let method = frame["method"].as_str().unwrap_or_default().to_string();
        if method == "tool.batch" {
            let params = frame["params"].clone();
            traffic.batches.push(params.clone());
            let observations: Vec<Value> = params["calls"]
                .as_array()
                .cloned()
                .unwrap_or_default()
                .iter()
                .map(|call| {
                    let output = format!(
                        "{} 的输出（{}）",
                        call["function"]["name"].as_str().unwrap_or("工具"),
                        call["id"].as_str().unwrap_or("call")
                    );
                    json!({
                        "tool_call": call,
                        "result": {"ok": true, "output": output, "full_output": output},
                        "message": {
                            "role": "tool",
                            "tool_call_id": call["id"],
                            "content": output,
                        },
                        "followup_messages": [],
                    })
                })
                .collect();
            kernel.send(json!({
                "jsonrpc": "2.0",
                "id": frame["id"],
                "result": {"observations": observations},
            }));
            continue;
        }
        if method == "context.prune" {
            let params = frame["params"].clone();
            traffic.prunes.push(params.clone());
            let evicted = evict(&params).unwrap_or_default();
            kernel.send(json!({
                "jsonrpc": "2.0",
                "id": frame["id"],
                "result": {"evicted_call_ids": evicted},
            }));
            continue;
        }
        assert_ne!(
            method, "model.reply",
            "已给模型配置，内核不应走代答路径：{frame}"
        );
        let finished = method == "turn.finished";
        traffic.frames.push(frame);
        if finished {
            return traffic;
        }
    }
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
        std::env::temp_dir().join(format!("omnicrawl-toolprune-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("无法建临时会话根");
    root
}

fn root_param(root: &Path) -> String {
    root.to_string_lossy().to_string()
}

/// 转录里指定类型的**第一条**事件（按 JSONL 逐行解析）。
fn transcript_event(root: &Path, event_type: &str) -> Value {
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
                continue;
            }
            if path.extension().map(|ext| ext == "jsonl").unwrap_or(false) {
                for line in std::fs::read_to_string(&path).unwrap_or_default().lines() {
                    let Ok(value) = serde_json::from_str::<Value>(line) else {
                        continue;
                    };
                    if value["type"].as_str() == Some(event_type) {
                        return value;
                    }
                }
            }
        }
    }
    panic!("转录里没有 {event_type} 事件");
}

/// 请求体里出现过的全部工具调用 ID。
fn call_ids_in(body: &Value) -> Vec<String> {
    body["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|message| message["tool_calls"].as_array())
        .flatten()
        .filter_map(|call| call["id"].as_str())
        .map(str::to_string)
        .collect()
}

/// 请求体里出现过的全部 `tool` 结果消息的调用 ID。
fn tool_result_ids_in(body: &Value) -> Vec<String> {
    body["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|message| message["role"] == "tool")
        .filter_map(|message| message["tool_call_id"].as_str())
        .map(str::to_string)
        .collect()
}

/// 核心语义：最新一批（小登）不被送审；上一批（老登）按裁决整组消失，且协议仍成对。
#[test]
fn only_the_previous_batch_is_pruned_and_its_pairs_leave_together() {
    // 三个模型回复：第一批调用（两个调用）→ 第二批调用 → 收尾文本。
    let server = StubServer::spawn(vec![
        tool_batch_stream(&[
            ("call-a", "read_file", "{\"path\":\"a.txt\"}"),
            ("call-b", "read_file", "{\"path\":\"b.txt\"}"),
        ]),
        tool_batch_stream(&[("call-c", "read_file", "{\"path\":\"c.txt\"}")]),
        text_stream("看完了。"),
    ]);
    let root = temp_root("previous-batch");
    let mut kernel = Kernel::spawn();
    kernel.initialize(
        model_config(&server),
        json!({"root": root_param(&root)}),
        true,
    );

    // 裁决脚本：把第一批里的 `call-b` 判为无用。
    let traffic = run_turn_with_prune(&mut kernel, "读三个文件", |params| {
        let ids: Vec<String> = params["groups"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|group| group["call_id"].as_str())
            .map(str::to_string)
            .collect();
        assert_eq!(
            ids,
            vec!["call-a".to_string(), "call-b".to_string()],
            "只有刚变老的那一批进送审：{params}"
        );
        Some(vec!["call-b".to_string()])
    });

    assert_eq!(traffic.prunes.len(), 1, "整回合只该裁决一次（本批是最新一批，从不送审）");
    assert_eq!(traffic.batches.len(), 2, "两次工具批次：{traffic:?}");
    // 送审负载带上了任务背景与两组调用（最新一批的 call-c 不在里面）。
    assert_eq!(traffic.prunes[0]["task"], "读三个文件");
    assert_eq!(traffic.prunes[0]["groups"][0]["tool"], "read_file");

    // 淘汰必须落成会话事件：重启（/resume）据此重建同一份上下文。
    let evicted = transcript_event(&root, "tool_call_evicted");
    assert_eq!(
        evicted["payload"]["evicted_call_ids"],
        json!(["call-b"]),
        "淘汰事件记下被淘汰的调用 ID：{evicted}"
    );

    // 第二次工具批次之后的请求体：call-b 的调用项与结果一起消失，call-a 仍成对保留。
    let bodies = server.bodies();
    assert!(bodies.len() >= 3, "三次模型请求：{bodies:?}");
    let after_eviction = &bodies[2];
    let calls = call_ids_in(after_eviction);
    let results = tool_result_ids_in(after_eviction);
    assert!(!calls.contains(&"call-b".to_string()), "调用项应被移除：{calls:?}");
    assert!(
        !results.contains(&"call-b".to_string()),
        "结果消息应被移除：{results:?}"
    );
    assert!(calls.contains(&"call-a".to_string()), "未被淘汰的调用保留：{calls:?}");
    assert!(
        results.contains(&"call-a".to_string()),
        "未被淘汰的结果保留：{results:?}"
    );
    // 最新一批（小登）在送审时还没执行；执行后它必须完整留在上下文里。
    assert!(
        calls.contains(&"call-c".to_string()) && results.contains(&"call-c".to_string()),
        "最新一批始终保留：{calls:?} / {results:?}"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// 宿主不声明能力时内核永不发起 `context.prune`，上下文原样保留。
#[test]
fn kernel_never_prunes_without_the_host_capability() {
    let server = StubServer::spawn(vec![
        tool_batch_stream(&[("call-a", "read_file", "{\"path\":\"a.txt\"}")]),
        tool_batch_stream(&[("call-b", "read_file", "{\"path\":\"b.txt\"}")]),
        text_stream("看完了。"),
    ]);
    let root = temp_root("no-capability");
    let mut kernel = Kernel::spawn();
    kernel.initialize(
        model_config(&server),
        json!({"root": root_param(&root)}),
        false,
    );

    let traffic = run_turn_with_prune(&mut kernel, "读两个文件", |_| {
        panic!("未声明能力时不该收到 context.prune")
    });
    assert_eq!(traffic.batches.len(), 2);
    assert!(traffic.prunes.is_empty());

    // 两次调用都还留在上下文里（建会话时没有淘汰事实，也没有淘汰事件）。
    let last = server.bodies().last().cloned().unwrap_or_default();
    let calls = call_ids_in(&last);
    assert!(calls.contains(&"call-a".to_string()) && calls.contains(&"call-b".to_string()));

    let _ = std::fs::remove_dir_all(&root);
}

/// 宿主裁决不可用时回空列表：内核什么都不删，也不留淘汰事件。
#[test]
fn empty_verdict_keeps_everything_and_writes_no_event() {
    let server = StubServer::spawn(vec![
        tool_batch_stream(&[("call-a", "read_file", "{\"path\":\"a.txt\"}")]),
        tool_batch_stream(&[("call-b", "read_file", "{\"path\":\"b.txt\"}")]),
        text_stream("看完了。"),
    ]);
    let root = temp_root("empty-verdict");
    let mut kernel = Kernel::spawn();
    kernel.initialize(
        model_config(&server),
        json!({"root": root_param(&root)}),
        true,
    );

    let traffic = run_turn_with_prune(&mut kernel, "读两个文件", |_| None);
    assert_eq!(traffic.prunes.len(), 1, "仍会问一次");
    assert_eq!(traffic.batches.len(), 2);

    let last = server.bodies().last().cloned().unwrap_or_default();
    let calls = call_ids_in(&last);
    assert!(
        calls.contains(&"call-a".to_string()) && calls.contains(&"call-b".to_string()),
        "空裁决不删任何东西：{calls:?}"
    );

    let _ = std::fs::remove_dir_all(&root);
}
