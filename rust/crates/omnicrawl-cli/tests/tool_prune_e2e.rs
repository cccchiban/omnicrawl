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
    /// 本实例的隔离配置根：`~/.omnicrawl` 指向它，`Drop` 时连同子进程一起清掉。
    home: PathBuf,
}

impl Kernel {
    fn spawn() -> Self {
        // 隔离配置环境：内核会按宿主 HOME 去读 `~/.omnicrawl/*`（工具输出压缩、上下文压缩、
        // 脱敏、决策通道……）。用例只钉「淘汰」这一条链路，必须与开发机的真实配置和凭据无关——
        // 否则回合末的整轮概括会真的发往压缩模型，断言也会随本机开关（如
        // `[tool_output_compression].enabled`）时绿时红。`process_home()` 在 Windows 认
        // `USERPROFILE`、其余平台认 `HOME`，两个都指向临时目录。
        let home =
            std::env::temp_dir().join(format!("omnicrawl-toolprune-home-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).expect("无法建隔离配置根");
        let mut child = Command::new(env!("CARGO_BIN_EXE_omnicrawl"))
            .env("OMNICRAWL_TEST_KEY", TEST_KEY)
            .env("USERPROFILE", &home)
            .env("HOME", &home)
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
            home,
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
        let _ = std::fs::remove_dir_all(&self.home);
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
    run_turn_frames(kernel, 2, "turn-1", user_text, evict)
}

/// 同上的显式版本：指定帧 ID 与 `turn_id`，供需要跑第二轮（验证标注在回合末生效）的用例调用。
fn run_turn_frames(
    kernel: &mut Kernel,
    request_id: i64,
    turn_id: &str,
    user_text: &str,
    evict: impl Fn(&Value) -> Option<Vec<String>>,
) -> TurnTraffic {
    kernel.send(json!({
        "jsonrpc": "2.0",
        "id": request_id,
        "method": "turn.submit",
        "params": {"turn_id": turn_id, "user_text": user_text},
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

/// 核心语义：最新一批（小登）不被送审；上一批（老登）先只落**标注**——当回合上下文原样保留，
/// 到回合收尾才整组移出（下一次请求就再也看不到它），协议始终成对。
#[test]
fn annotated_calls_stay_in_turn_and_leave_the_context_at_turn_end() {
    // 四个模型回复：第一批调用（两个调用）→ 第二批调用 → 收尾文本 → 第二轮收尾文本。
    let server = StubServer::spawn(vec![
        tool_batch_stream(&[
            ("call-a", "read_file", "{\"path\":\"a.txt\"}"),
            ("call-b", "read_file", "{\"path\":\"b.txt\"}"),
        ]),
        tool_batch_stream(&[("call-c", "read_file", "{\"path\":\"c.txt\"}")]),
        text_stream("看完了。"),
        text_stream("第二轮没有工具调用。"),
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

    // 回合内：标注只是标注，当回合的请求体里 call-b 的调用项与结果都还在。
    let bodies = server.bodies();
    assert!(bodies.len() >= 3, "三次模型请求：{bodies:?}");
    let during_turn = &bodies[2];
    let calls = call_ids_in(during_turn);
    let results = tool_result_ids_in(during_turn);
    assert!(
        calls.contains(&"call-b".to_string()) && results.contains(&"call-b".to_string()),
        "先标注-后压缩：当回合不剔除：{calls:?} / {results:?}"
    );
    for id in ["call-a", "call-c"] {
        assert!(
            calls.contains(&id.to_string()) && results.contains(&id.to_string()),
            "{id} 成对保留：{calls:?} / {results:?}"
        );
    }

    // 回合收尾统一处理：下一轮第一次请求里 call-b 整组消失，未淘汰的照旧成对。
    let next = run_turn_frames(&mut kernel, 3, "turn-2", "接着做", |_| {
        panic!("第二轮没有工具批次，不该收到裁决")
    });
    assert!(next.prunes.is_empty());
    let after_turn = server.bodies().last().cloned().unwrap_or_default();
    let calls = call_ids_in(&after_turn);
    let results = tool_result_ids_in(&after_turn);
    assert!(
        !calls.contains(&"call-b".to_string()) && !results.contains(&"call-b".to_string()),
        "回合收尾应当整组移出：{calls:?} / {results:?}"
    );
    for id in ["call-a", "call-c"] {
        assert!(
            calls.contains(&id.to_string()) && results.contains(&id.to_string()),
            "{id} 成对保留：{calls:?} / {results:?}"
        );
    }

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

/// 白名单：记忆 / 知识库 / 编辑替换 / `git` 不进送审；`read` / `grep` 与普通工具一起送审。
/// 回合收尾统一处理时，也只有进过送审的那几次会被移出上下文。
#[test]
fn excluded_tools_are_never_sent_for_review() {
    let server = StubServer::spawn(vec![
        tool_batch_stream(&[
            ("call-r", "read", "{\"path\":\"a.txt\"}"),
            ("call-g", "grep", "{\"pattern\":\"x\"}"),
            ("call-m", "memory_search", "{\"query\":\"偏好\"}"),
            ("call-k", "kb_search", "{\"query\":\"规范\"}"),
            ("call-w", "write_file", "{\"path\":\"b.txt\"}"),
            ("call-e", "Edit_file", "{\"path\":\"c.txt\"}"),
            ("call-t", "git", "{\"command\":\"status\"}"),
            ("call-b1", "bash", "{\"command\":\"ls\"}"),
        ]),
        tool_batch_stream(&[("call-b2", "bash", "{\"command\":\"pwd\"}")]),
        text_stream("看完了。"),
        text_stream("第二轮没有工具调用。"),
    ]);
    let root = temp_root("excluded-tools");
    let mut kernel = Kernel::spawn();
    kernel.initialize(
        model_config(&server),
        json!({"root": root_param(&root)}),
        true,
    );

    let traffic = run_turn_with_prune(&mut kernel, "整理仓库", |params| {
        let ids: Vec<String> = params["groups"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|group| group["call_id"].as_str())
            .map(str::to_string)
            .collect();
        assert_eq!(
            ids,
            vec![
                "call-r".to_string(),
                "call-g".to_string(),
                "call-b1".to_string()
            ],
            "read / grep 进送审，记忆 / 知识库 / 编辑替换 / git 不进：{params}"
        );
        assert_eq!(params["task"], "整理仓库", "送审要带用户本回合的请求");
        Some(vec![
            "call-r".to_string(),
            "call-g".to_string(),
            "call-b1".to_string(),
        ])
    });

    assert_eq!(traffic.prunes.len(), 1, "只裁决一次：{traffic:?}");

    // 回合收尾统一处理：进过送审的三次整组消失，被排除的与最新一批原样成对保留。
    run_turn_frames(&mut kernel, 3, "turn-2", "接着做", |_| {
        panic!("第二轮没有工具批次，不该收到裁决")
    });
    let after = server.bodies().last().cloned().unwrap_or_default();
    let calls = call_ids_in(&after);
    let results = tool_result_ids_in(&after);
    for id in ["call-r", "call-g", "call-b1"] {
        assert!(
            !calls.contains(&id.to_string()) && !results.contains(&id.to_string()),
            "{id} 被淘汰后应当整组消失：{calls:?} / {results:?}"
        );
    }
    for id in ["call-m", "call-k", "call-w", "call-e", "call-t", "call-b2"] {
        assert!(
            calls.contains(&id.to_string()) && results.contains(&id.to_string()),
            "{id} 必须成对留在上下文里：{calls:?} / {results:?}"
        );
    }

    let _ = std::fs::remove_dir_all(&root);
}

/// 整批都是被排除的工具时连裁决请求都不发：不必为一个裁决不了的批次多跑一次决策往返。
#[test]
fn a_batch_of_only_excluded_tools_skips_the_review_entirely() {
    let server = StubServer::spawn(vec![
        tool_batch_stream(&[
            ("call-m", "memory_search", "{\"query\":\"偏好\"}"),
            ("call-k", "kb_search", "{\"query\":\"规范\"}"),
            ("call-w", "write_file", "{\"path\":\"b.txt\"}"),
            ("call-e", "Edit_file", "{\"path\":\"c.txt\"}"),
            ("call-t", "git", "{\"command\":\"status\"}"),
        ]),
        tool_batch_stream(&[("call-b", "bash", "{\"command\":\"ls\"}")]),
        text_stream("看完了。"),
    ]);
    let root = temp_root("only-excluded");
    let mut kernel = Kernel::spawn();
    kernel.initialize(
        model_config(&server),
        json!({"root": root_param(&root)}),
        true,
    );

    let traffic = run_turn_with_prune(&mut kernel, "看看文件", |_| {
        panic!("整批都被排除时不该收到 context.prune")
    });
    assert_eq!(traffic.batches.len(), 2);
    assert!(traffic.prunes.is_empty());

    let _ = std::fs::remove_dir_all(&root);
}
