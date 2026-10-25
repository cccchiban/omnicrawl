//! 内核自持 `subagent` 工具的端到端验收：主回合要求调用 subagent → 内核跑子回合（子回合里真的
//! 调用了一个工具）→ 折回主回合；以及同一条消息里多次调用时各开一个独立批次。
//!
//! 判据全在协议帧与内核实际发出的模型请求体上：
//! 1. `subagent` 本身不占宿主批次（宿主收到的每一批 `tool.batch` 里都没有 `subagent` 调用）；
//! 2. 子代理内部的工具调用**走宿主**，且 `turn_id` 是子任务号；
//! 3. 子回合的模型请求带角色定义的 system 提示与 `<subagent_task>` 指令，并带回工具观察；
//! 4. 子代理的 `subagent.event` 通知按 `batch.created` → `task.started` → `task.completed` 顺序发出；
//! 5. 主回合的下一次请求里带上了子任务的 tool 观察；
//! 6. 同一批里的多个 `subagent` 调用各自成批（=各开一个窗口），互不影响。

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use serde_json::{json, Value};

const WAIT: Duration = Duration::from_secs(20);
const TEST_KEY: &str = "test-key";
const TOOL_OUTPUT: &str = "文件内容：42";
const CHILD_TEXT: &str = "子任务完成";
const PARENT_TEXT: &str = "主回合完成";

/// 回环服务端的一次应答：正常 SSE，或一段完整原始响应（用来造失败）。
#[derive(Clone)]
enum Reply {
    Stream(String),
    Raw(String),
    /// 先要求调工具、拿到结果后再收尾；脚本用尽时按请求内容兜底（并发下顺序不定）。
    ///
    /// 当前用例都走 `ToolCalls`/`AlwaysText`，这条分支留给脚本化调试；标 `allow`
    /// 是为了让 `clippy -D warnings` 的门槛不被测试里的备用分支挡住。
    #[allow(dead_code)]
    ToolThenText {
        tool: String,
        tool_arguments: String,
        text: String,
    },
    /// 任何请求都回同一段文本（并发下顺序不定时用来验证调度本身）。
    AlwaysText(String),
}

/// 本机回环测试脚本：按连接到达序号推进。
#[derive(Clone)]
enum Script {
    Sequential(Vec<Reply>),
}

impl Script {
    fn sequential(steps: Vec<Reply>) -> Self {
        Script::Sequential(steps)
    }
}

/// SSE：一段文本 + 结束原因。
fn text_stream(text: &str) -> String {
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

/// SSE：模型在同一批里要求调用多个工具（并行子代理的调用形状）。
fn multi_tool_call_stream(calls: &[(&str, &str)]) -> String {
    let items: Vec<serde_json::Value> = calls
        .iter()
        .enumerate()
        .map(|(index, (name, arguments))| {
            json!({
                "index": index,
                "id": format!("call-{}", index + 1),
                "type": "function",
                "function": {"name": name, "arguments": arguments}
            })
        })
        .collect();
    let delta = json!({"choices": [{"delta": {"tool_calls": items}}]}).to_string();
    let finish = json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]}).to_string();
    format!("data: {delta}\n\ndata: {finish}\n\ndata: [DONE]\n\n")
}

/// 一段完整的 500 响应（触发子回合失败）。
fn server_error() -> Reply {
    Reply::Raw(
        "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            .to_string(),
    )
}

fn ok_stream(body: String) -> Reply {
    Reply::Stream(body)
}

/// 本机回环服务端：按请求来源分流应答，并记录收到的请求体。
struct StubServer {
    addr: String,
    requests: Arc<Mutex<Vec<Value>>>,
}

impl StubServer {
    /// 顺序脚本：等价旧 API，按连接顺序全局派发（供既有 4 个测试沿用）。
    fn spawn(script: Vec<Reply>) -> Self {
        Self::spawn_script(Script::sequential(script))
    }

    /// 双泳道分流脚本：按请求来源派发，供并发用例使用。
    fn spawn_script(script: Script) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("无法监听回环端口");
        let addr = listener.local_addr().expect("无法取本地地址");
        let requests: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&requests);

        thread::spawn(move || {
            let global_cursor = Arc::new(Mutex::new(0usize));
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let raw = read_request(&mut stream);
                if let Ok(value) = serde_json::from_str::<Value>(&raw) {
                    recorded.lock().expect("记录锁").push(value);
                }
                let reply = match &script {
                    Script::Sequential(steps) => {
                        let mut guard = global_cursor.lock().expect("全局游标锁");
                        let index = *guard;
                        *guard += 1;
                        build_reply(steps.get(index), steps, &raw)
                    }
                };
                let _ = stream.write_all(reply.as_bytes());
                let _ = stream.flush();
            }
        });

        Self {
            addr: format!("http://{addr}/v1"),
            requests,
        }
    }
}

/// 取脚本里第 `index` 条应答；用尽时按请求内容兜底。
///
/// 兜底规则沿用旧实现：先找 `AlwaysText`（并发用例常用），再按 `ToolThenText` 判断
/// 是否已经回过工具结果——回过就要文本，没回过就要求调工具。
fn build_reply(step: Option<&Reply>, steps: &[Reply], raw: &str) -> String {
    match step {
        Some(Reply::Stream(body)) => sse_response(body),
        Some(Reply::Raw(raw)) => raw.clone(),
        Some(Reply::AlwaysText(text)) => sse_response(&text_stream(text)),
        Some(Reply::ToolThenText { .. }) | None => {
            let always = steps.iter().find_map(|item| match item {
                Reply::AlwaysText(text) => Some(text),
                _ => None,
            });
            let tool_rule = steps.iter().find_map(|item| match item {
                Reply::ToolThenText {
                    tool,
                    tool_arguments,
                    text,
                } => Some((tool, tool_arguments, text)),
                _ => None,
            });
            match (always, tool_rule) {
                (Some(text), _) => sse_response(&text_stream(text)),
                (None, Some((tool, arguments, _))) if !raw.contains("\"role\":\"tool\"") => {
                    sse_response(&tool_call_stream(tool, arguments))
                }
                (None, Some((_, _, text))) => sse_response(&text_stream(text)),
                (None, None) => sse_response(&text_stream("（脚本用尽）")),
            }
        }
    }
}

fn sse_response(body: &str) -> String {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    )
}

fn read_request(stream: &mut TcpStream) -> String {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(size) => {
                buffer.extend_from_slice(&chunk[..size]);
                if let Some(position) = find_body_start(&buffer) {
                    let head = String::from_utf8_lossy(&buffer[..position]).to_string();
                    let length = head
                        .lines()
                        .find_map(|line| {
                            let lower = line.to_lowercase();
                            lower
                                .strip_prefix("content-length:")
                                .map(|value| value.trim().parse::<usize>().unwrap_or(0))
                        })
                        .unwrap_or(0);
                    if buffer.len() >= position + 4 + length {
                        break;
                    }
                }
            }
            Err(_) => break,
        }
    }
    match find_body_start(&buffer) {
        Some(position) => String::from_utf8_lossy(&buffer[position + 4..]).to_string(),
        None => String::new(),
    }
}

fn find_body_start(buffer: &[u8]) -> Option<usize> {
    buffer.windows(4).position(|window| window == b"\r\n\r\n")
}

/// 宿主侧驱动：写帧、按 id 配对响应、响应工具批次、收集通知。
struct Host {
    child: Child,
    stdin: std::process::ChildStdin,
    lines: Receiver<String>,
    events: Vec<Value>,
    /// 收到的工具批次：`(turn_id, 调用名, 隔离根)`。
    batches: Vec<(String, Vec<String>, Option<String>)>,
    next_id: i64,
    turn_counter: i64,
}

impl Host {
    /// `agents` / `config` 由 `prepare_root` 建在 `~/.OmniCrawl` 下，配置路径不再有
    /// 环境变量覆盖，这里只按 workspace 推出隔离 home。
    fn start(workspace: &PathBuf, _agents: &PathBuf, _config: &PathBuf) -> Self {
        let home = workspace
            .parent()
            .map(|parent| parent.join("home"))
            .unwrap_or_else(|| workspace.join("home"));
        let mut child = Command::new(env!("CARGO_BIN_EXE_omnicrawl"))
            .current_dir(workspace)
            .env("OPENAI_API_KEY", TEST_KEY)
            .env("USERPROFILE", &home)
            .env("HOME", &home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("无法启动内核");
        let stdin = child.stdin.take().expect("stdin");
        let stdout = child.stdout.take().expect("stdout");
        let (sender, lines) = mpsc::channel();
        thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines().map_while(Result::ok) {
                if sender.send(line).is_err() {
                    break;
                }
            }
        });
        Self {
            child,
            stdin,
            lines,
            events: Vec::new(),
            batches: Vec::new(),
            next_id: 1,
            turn_counter: 1,
        }
    }

    fn send(&mut self, frame: Value) {
        let line = serde_json::to_string(&frame).expect("帧可序列化");
        writeln!(self.stdin, "{line}").expect("写帧");
        self.stdin.flush().expect("刷新帧");
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        loop {
            let line = self
                .lines
                .recv_timeout(WAIT)
                .unwrap_or_else(|error| panic!("等待 {method} 响应失败：{error}"));
            let frame: Value = serde_json::from_str(&line).expect("响应是合法 JSON");
            if frame.get("id").and_then(Value::as_i64) == Some(id) {
                return frame;
            }
            self.handle_side_frame(&frame);
        }
    }

    /// 处理非响应帧：工具批次就地执行并回观察，其余作为通知记录。
    fn handle_side_frame(&mut self, frame: &Value) {
        if frame.get("method").and_then(Value::as_str) == Some("tool.batch") {
            let id = frame.get("id").cloned().unwrap_or(Value::Null);
            let params = frame.get("params").cloned().unwrap_or(Value::Null);
            let turn_id = params
                .get("turn_id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let calls = params
                .get("calls")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let names: Vec<String> = calls
                .iter()
                .filter_map(|call| call.get("name").and_then(Value::as_str))
                .map(str::to_string)
                .collect();
            let root = params
                .get("workspace_root")
                .and_then(Value::as_str)
                .map(str::to_string);
            self.batches.push((turn_id, names, root));
            let observations: Vec<Value> = calls
                .iter()
                .map(|call| {
                    json!({
                        "tool_call": call,
                        "result": {
                            "ok": true,
                            "output": TOOL_OUTPUT,
                            "full_output": "",
                            "error_code": null,
                            "retryable": false,
                        },
                        "message": {"role": "tool", "content": TOOL_OUTPUT},
                        "followup_messages": [],
                    })
                })
                .collect();
            self.send(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {"observations": observations},
            }));
            return;
        }
        if frame.get("method").is_some() {
            self.events.push(frame.clone());
        }
    }

    /// 提交一回合并收集通知，直到 `turn.finished` 与响应都到齐。
    fn run_turn(&mut self, text: &str) -> Vec<Value> {
        let id = self.next_id;
        self.next_id += 1;
        let turn_id = format!("turn-{}", self.turn_counter);
        self.turn_counter += 1;
        self.send(json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "turn.submit",
            "params": {"turn_id": turn_id, "user_text": text},
        }));
        let mut finished = Vec::new();
        let mut response_seen = false;
        let deadline = std::time::Instant::now() + WAIT;
        while std::time::Instant::now() < deadline {
            let Ok(line) = self.lines.recv_timeout(Duration::from_millis(500)) else {
                continue;
            };
            let frame: Value = serde_json::from_str(&line).expect("帧是合法 JSON");
            if frame.get("id").and_then(Value::as_i64) == Some(id) {
                response_seen = true;
                if !finished.is_empty() {
                    break;
                }
                continue;
            }
            if frame.get("method").and_then(Value::as_str) == Some("turn.finished") {
                finished.push(frame.clone());
                self.events.push(frame);
                if response_seen {
                    break;
                }
                continue;
            }
            self.handle_side_frame(&frame);
        }
        finished
    }

    fn shutdown(&mut self) {
        let _ = self.request("shutdown", json!({}));
        let _ = self.child.wait();
    }

    /// 某类通知（`method` 与可选 `name` 都匹配）。
    fn notifications(&self, method: &str, name: Option<&str>) -> Vec<Value> {
        self.events
            .iter()
            .filter(|frame| {
                if frame.get("method").and_then(Value::as_str) != Some(method) {
                    return false;
                }
                match name {
                    None => true,
                    Some(expected) => {
                        frame
                            .get("params")
                            .and_then(|params| params.get("name"))
                            .and_then(Value::as_str)
                            == Some(expected)
                    }
                }
            })
            .cloned()
            .collect()
    }
}

fn agent_definition() -> &'static str {
    "---\nname: review\ndescription: 评审\ndisallowedTools:\n  - subagent\nmodel: inherit\npermissionMode: delegated-read-only\nisolation: shared\n---\n你是评审子代理，只读。\n"
}

/// 准备一套临时工作区：隔离 home 下的定义目录 + 子代理配置。
///
/// 定义目录与配置路径不再有环境变量覆盖，因此都放进 `~/.OmniCrawl`。
fn prepare_root(tag: &str, config_body: &str) -> (PathBuf, PathBuf, PathBuf, PathBuf) {
    let root =
        std::env::temp_dir().join(format!("omnicrawl-subagent-{tag}-{}", std::process::id()));
    let workspace = root.join("ws");
    let agents = root.join("home").join(".OmniCrawl").join("agents");
    let config = root.join("home").join(".OmniCrawl").join("subagents.toml");
    std::fs::create_dir_all(&workspace).expect("建工作区");
    std::fs::create_dir_all(&agents).expect("建定义目录");
    std::fs::write(agents.join("review.md"), agent_definition()).expect("写定义");
    std::fs::write(&config, config_body).expect("写配置");
    (root, workspace, agents, config)
}

fn model_block(server: &StubServer, tools: Value, retry: Option<i64>) -> Value {
    let mut block = json!({
        "model": "stub-model",
        "base_url": server.addr,
        "api_key_env": "OPENAI_API_KEY",
        "system_prompt": "你是主助手。",
        "tools": tools,
    });
    if let Some(count) = retry {
        block["request_retry_count"] = json!(count);
    }
    block
}

/// 带会话的握手：父回合的调用会落进该会话的转录。
fn handshake_with_session(
    host: &mut Host,
    server: &StubServer,
    tools: Value,
    session_root: &PathBuf,
) {
    let response = host.request(
        "initialize",
        json!({
            "protocol_version": "1.0",
            "client": {"name": "subagent-e2e", "version": "0.1.0"},
            "model": model_block(server, tools, None),
            "session": {"root": session_root.to_string_lossy()},
        }),
    );
    assert!(response.get("result").is_some(), "握手失败：{response}");
}

/// 转录里工具事件的 `payload.tool`，按落盘顺序。
///
/// 用来验证「子代理内部的工具调用不落父会话」：父子共用一个会话根，落盘的应该只有父回合
/// 那一次 `subagent` 调用（Python 在子代理循环里传 `persist_session_events=False`）。
fn transcript_tools(session_root: &PathBuf) -> Vec<String> {
    let sessions = session_root.join("sessions");
    let mut paths: Vec<PathBuf> = std::fs::read_dir(&sessions)
        .expect("会话目录应当存在")
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().map(|ext| ext == "jsonl").unwrap_or(false))
        .collect();
    paths.sort();
    let mut tools: Vec<String> = Vec::new();
    for path in paths {
        for line in std::fs::read_to_string(&path).unwrap_or_default().lines() {
            let Ok(value) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            let kind = value["type"].as_str().unwrap_or_default();
            if kind == "tool_call_requested" || kind == "tool_result" {
                tools.push(
                    value["payload"]["tool"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string(),
                );
            }
        }
    }
    tools
}

fn handshake(host: &mut Host, server: &StubServer, tools: Value, retry: Option<i64>) {
    let response = host.request(
        "initialize",
        json!({
            "protocol_version": "1.0",
            "client": {"name": "subagent-e2e", "version": "0.1.0"},
            "model": model_block(server, tools, retry),
        }),
    );
    assert!(response.get("result").is_some(), "握手失败：{response}");
}

#[test]
fn kernel_runs_subagent_with_host_tool_batch() {
    let tool_arguments = json!({
        "action": "run",
        "task": {
            "description": "看代码",
            "prompt": "读一下 a.py",
            "subagent_type": "review",
        },
    })
    .to_string();
    let server = StubServer::spawn(vec![
        ok_stream(tool_call_stream("subagent", &tool_arguments)),
        ok_stream(tool_call_stream("read", "{\"path\":\"a.py\"}")),
        ok_stream(text_stream(CHILD_TEXT)),
        ok_stream(text_stream(PARENT_TEXT)),
    ]);
    let (root, workspace, agents, config) = prepare_root(
        "e2e",
        "[subagents]\nenabled = true\n",
    );

    let session_root = root.join("session");
    std::fs::create_dir_all(&session_root).expect("建会话根");
    let mut host = Host::start(&workspace, &agents, &config);
    handshake_with_session(
        &mut host,
        &server,
        json!([
            {"type": "function", "function": {"name": "read"}},
            {"type": "function", "function": {"name": "grep"}},
            {"type": "function", "function": {"name": "subagent"}}
        ]),
        &session_root,
    );

    let finished = host.run_turn("帮我评审");
    host.shutdown();

    assert_eq!(finished.len(), 1, "应恰好收到一次 turn.finished");
    assert_eq!(
        finished[0]["params"]["final_text"].as_str().unwrap(),
        PARENT_TEXT
    );

    // 1. subagent 本身不占宿主批次：每一批 tool.batch 里都没有 subagent 调用。
    assert!(!host.batches.is_empty(), "子代理内部的工具调用应落到宿主");
    for (turn_id, names, _root) in &host.batches {
        assert!(
            !names.iter().any(|name| name == "subagent"),
            "subagent 不该出现在宿主批次里：{turn_id} {names:?}"
        );
    }

    // 2. 子代理内部的工具调用用子任务号作为 turn_id。
    assert!(
        host.batches
            .iter()
            .all(|(turn_id, _, _)| turn_id.starts_with("task-")),
        "子代理批次应以子任务号分组：{:?}",
        host.batches
    );

    // 4. 子代理事件按顺序发出，状态是完成。
    let created = host.notifications("subagent.event", Some("subagent.batch.created"));
    let started = host.notifications("subagent.event", Some("subagent.task.started"));
    let completed = host.notifications("subagent.event", Some("subagent.task.completed"));
    assert_eq!(created.len(), 1, "应有批次创建事件");
    assert_eq!(started.len(), 1, "应有任务开始事件");
    assert_eq!(completed.len(), 1, "应有任务完成事件");
    // 收尾事件恰好一次：宿主的子代理窗口靠它自动关闭。
    let finished = host.notifications("subagent.event", Some("subagent.batch.finished"));
    assert_eq!(finished.len(), 1, "批次收尾事件应恰好发一次");
    assert_eq!(
        finished[0]["params"]["payload"]["batch_id"],
        created[0]["params"]["payload"]["batch_id"],
        "收尾事件要带同一个批次号"
    );

    // 4a. 子代理的正文以流式增量的形式进独立窗口（而不是等整段回复落地）。
    let child_text = host.notifications("subagent.event", Some("subagent.turn.text"));
    let streamed_child = child_text
        .iter()
        .map(|frame| {
            frame["params"]["payload"]["text"]
                .as_str()
                .unwrap_or_default()
        })
        .collect::<String>();
    assert!(
        streamed_child.contains(CHILD_TEXT),
        "子代理正文应以流式增量进窗口：{streamed_child}"
    );

    // 4b. 子回合的正文只走 `subagent.turn.text`，绝不能以 `turn.delta` 外发：宿主会把
    //     `turn.delta` 直接追加到主代理的回复里，那样子代理的话就混进主代理的消息区。
    let deltas = host.notifications("turn.delta", None);
    let streamed = deltas
        .iter()
        .map(|frame| {
            frame
                .get("params")
                .and_then(|params| params.get("text"))
                .and_then(Value::as_str)
                .unwrap_or_default()
        })
        .collect::<String>();
    assert!(
        !streamed.contains(CHILD_TEXT),
        "子代理正文不该出现在主会话的流式增量里：{streamed}"
    );
    assert!(
        streamed.contains(PARENT_TEXT),
        "主代理正文照旧要走 turn.delta：{streamed}"
    );

    // 5. `subagent` 工具批次的子回合也开过程流：宿主的子代理窗口靠它渲染，
    //    因此工具调用必须与子任务号/批次号一起报出来（不再只有 `/review` 才有）。
    let tool_started = host.notifications("subagent.event", Some("subagent.tool.started"));
    let tool_completed = host.notifications("subagent.event", Some("subagent.tool.completed"));
    assert_eq!(tool_started.len(), 1, "应有子代理工具开始事件");
    assert_eq!(tool_completed.len(), 1, "应有子代理工具完成事件");
    let payload = &tool_started[0]["params"]["payload"];
    assert_eq!(payload["tool"], "read");
    assert!(
        payload["batch_id"]
            .as_str()
            .is_some_and(|id| !id.is_empty()),
        "工具事件要带批次号：{payload}"
    );
    assert!(
        payload["task_id"]
            .as_str()
            .is_some_and(|id| id.starts_with("task-")),
        "工具事件要带子任务号：{payload}"
    );
    assert_eq!(
        tool_completed[0]["params"]["payload"]["ok"],
        Value::Bool(true)
    );

    // 6. 工具事件只记父回合那一次 `subagent` 调用：子代理内部的 `read` 不落父会话
    //    （父子共用一个会话根，落盘的若出现 `read` 就说明开关没生效）。
    assert_eq!(
        transcript_tools(&session_root),
        vec!["subagent".to_string(), "subagent".to_string()],
        "父会话应只有 subagent 的请求与结果事件"
    );

    // 3/5. 模型请求体：子回合带角色定义，带回工具观察；主回合带回子任务观察。
    let requests = server.requests.lock().expect("请求锁").clone();
    assert!(
        requests.len() >= 4,
        "至少四次模型请求，实际 {}",
        requests.len()
    );
    let child_system = requests[1]["messages"][0]["content"]
        .as_str()
        .unwrap_or_default();
    assert!(
        child_system.contains("你是评审子代理"),
        "子回合 system 应是角色定义：{child_system}"
    );
    let child_payload = serde_json::to_string(&requests[1]).unwrap_or_default();
    assert!(
        child_payload.contains("subagent_task"),
        "子回合应带任务指令：{child_payload}"
    );
    let child_second = serde_json::to_string(&requests[2]).unwrap_or_default();
    assert!(
        child_second.contains(TOOL_OUTPUT),
        "子回合第二次请求应带上工具观察：{child_second}"
    );

    let parent_second = serde_json::to_string(&requests[3]).unwrap_or_default();
    assert!(
        parent_second.contains(CHILD_TEXT),
        "主回合应带回子任务观察：{parent_second}"
    );
    assert!(
        parent_second.contains("task-"),
        "主回合观察应含任务号：{parent_second}"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn two_subagent_calls_in_one_batch_run_concurrently() {
    // 并行靠「同一条消息里多次调用」而不是「一次调用塞多个任务」：
    // 两个调用在同一批里，各自的窗口与任务号互不相同。
    let first = json!({
        "action": "run",
        "task": {
            "description": "第一个",
            "prompt": "读一下 a.py",
            "subagent_type": "review",
        },
    })
    .to_string();
    let second = json!({
        "action": "run",
        "task": {
            "description": "第二个",
            "prompt": "读一下 b.py",
            "subagent_type": "review",
        },
    })
    .to_string();
    let server = StubServer::spawn(vec![
        ok_stream(multi_tool_call_stream(&[
            ("subagent", first.as_str()),
            ("subagent", second.as_str()),
        ])),
        Reply::AlwaysText(CHILD_TEXT.to_string()),
    ]);
    let (root, workspace, agents, config) =
        prepare_root("parallel", "[subagents]
enabled = true
");

    let mut host = Host::start(&workspace, &agents, &config);
    handshake(
        &mut host,
        &server,
        json!([
            {"type": "function", "function": {"name": "read"}},
            {"type": "function", "function": {"name": "subagent"}}
        ]),
        None,
    );

    let finished = host.run_turn("并行看两个文件");
    host.shutdown();

    assert_eq!(finished.len(), 1, "应恰好收到一次 turn.finished");

    // 两次调用 = 两个独立的批次窗口，各有一份开始与完成事件。
    let started = host.notifications("subagent.event", Some("subagent.task.started"));
    let completed = host.notifications("subagent.event", Some("subagent.task.completed"));
    assert_eq!(started.len(), 2, "两个子代理都应发出开始事件");
    assert_eq!(completed.len(), 2, "两个子代理都应发出完成事件");

    let batches = host.notifications("subagent.event", Some("subagent.batch.created"));
    assert_eq!(batches.len(), 2, "每次调用各开一个批次（=一个窗口）");
    let first_batch = batches[0]["params"]["payload"]["batch_id"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let second_batch = batches[1]["params"]["payload"]["batch_id"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert_ne!(first_batch, second_batch, "两次调用必须是不同批次");

    // 两个子回合的请求都真的发过，且各带自己的任务文本。
    let requests = server.requests.lock().expect("请求锁").clone();
    let child_requests: Vec<String> = requests
        .iter()
        .map(|item| serde_json::to_string(item).unwrap_or_default())
        .filter(|text| text.contains("subagent_task"))
        .collect();
    assert_eq!(child_requests.len(), 2, "两个子回合各发一次请求");
    assert!(
        child_requests.iter().any(|text| text.contains("读一下 a.py")),
        "第一个子代理的请求应该发出去"
    );
    assert!(
        child_requests.iter().any(|text| text.contains("读一下 b.py")),
        "第二个子代理的请求应该发出去"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn worktree_isolation_creates_and_reports_isolated_workspace() {
    let tool_arguments = json!({
        "action": "run",
        "task": {
            "description": "写点东西",
            "prompt": "在隔离区里读一下 a.txt",
            "subagent_type": "writer",
        },
    })
    .to_string();
    let server = StubServer::spawn(vec![
        ok_stream(tool_call_stream("subagent", &tool_arguments)),
        ok_stream(tool_call_stream("read", "{\"path\":\"a.txt\"}")),
        ok_stream(text_stream(CHILD_TEXT)),
        ok_stream(text_stream(PARENT_TEXT)),
    ]);
    // 工作区本身必须是 git 仓库：worktree 隔离要基于它建。
    let root = std::env::temp_dir().join(format!("omnicrawl-subagent-wt-{}", std::process::id()));
    let workspace = root.join("ws");
    let agents = root.join("home").join(".OmniCrawl").join("agents");
    let config = root.join("home").join(".OmniCrawl").join("subagents.toml");
    std::fs::create_dir_all(&workspace).expect("建工作区");
    std::fs::create_dir_all(&agents).expect("建定义目录");
    for args in [
        vec!["init", "-q"],
        vec!["config", "user.email", "test@example.com"],
        vec!["config", "user.name", "test"],
    ] {
        let output = std::process::Command::new("git")
            .args(&args)
            .current_dir(&workspace)
            .output()
            .expect("git 可执行");
        assert!(output.status.success(), "git {args:?} 失败");
    }
    std::fs::write(
        workspace.join("a.txt"),
        "one
",
    )
    .expect("写文件");
    let output = std::process::Command::new("git")
        .args(["add", "-A"])
        .current_dir(&workspace)
        .output()
        .expect("git add");
    assert!(output.status.success());
    let output = std::process::Command::new("git")
        .args(["commit", "-q", "-m", "init"])
        .current_dir(&workspace)
        .output()
        .expect("git commit");
    assert!(output.status.success());
    std::fs::write(
        agents.join("writer.md"),
        "---
name: writer
description: 写手
model: inherit
permissionMode: standard
isolation: worktree
tools:
  - read
  - write_file
---
你是写手子代理。
",
    )
    .expect("写定义");
    std::fs::write(
        &config,
        "[subagents]
enabled = true
allow_worktree = true
allow_standard_agent = true
",
    )
    .expect("写配置");

    let mut host = Host::start(&workspace, &agents, &config);
    handshake(
        &mut host,
        &server,
        json!([
            {"type": "function", "function": {"name": "read"}},
            {"type": "function", "function": {"name": "write_file"}},
            {"type": "function", "function": {"name": "subagent"}}
        ]),
        None,
    );

    let finished = host.run_turn("写点东西");
    host.shutdown();

    assert_eq!(finished.len(), 1, "应恰好收到一次 turn.finished");

    // 1. 子代理的工具批次带着隔离根，且根落在托管根下。
    let isolated: Vec<&(String, Vec<String>, Option<String>)> = host
        .batches
        .iter()
        .filter(|(_, _, root)| root.is_some())
        .collect();
    let observed = server.requests.lock().expect("请求锁").clone();
    let tool_notes: Vec<String> = observed
        .iter()
        .flat_map(|request| {
            request["messages"]
                .as_array()
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .filter(|message| message["role"] == "tool")
                .filter_map(|message| message["content"].as_str().map(str::to_string))
                .collect::<Vec<_>>()
        })
        .collect();
    assert!(
        !isolated.is_empty(),
        "worktree 子任务的批次应带隔离根；事件={:?}；批次={:?}；结局={}；工具观察={:?}",
        host.events
            .iter()
            .filter(|frame| frame.get("method").and_then(Value::as_str) == Some("subagent.event"))
            .filter_map(|frame| serde_json::to_string(&frame["params"]).ok())
            .collect::<Vec<_>>(),
        host.batches,
        finished[0]["params"]["final_text"],
        tool_notes,
    );
    let isolated_root = isolated[0].2.clone().unwrap_or_default();
    assert!(
        isolated_root.contains("agent-worktrees") && isolated_root.contains("sw-"),
        "隔离根应在托管根下：{isolated_root}"
    );

    // 2. 隔离区在磁盘上真的建起来了，元数据按 agent_isolation 的格式落盘。
    let host_root = workspace
        .parent()
        .map(|parent| parent.join("home"))
        .unwrap_or_else(|| workspace.join("home"))
        .join(".omnicrawl")
        .join("agent-worktrees");
    let entries: Vec<String> = std::fs::read_dir(&host_root)
        .map(|items| {
            items
                .flatten()
                .map(|item| item.file_name().to_string_lossy().to_string())
                .collect()
        })
        .unwrap_or_default();
    assert!(
        entries
            .iter()
            .any(|name| name.starts_with("sw-") && name.ends_with(".json")),
        "应有隔离元数据：{entries:?}"
    );

    // 3. 主回合的观察里带上了 worktree 产物摘要（分支名）。
    let requests = server.requests.lock().expect("请求锁").clone();
    let parent_payload = serde_json::to_string(&requests[requests.len() - 1]).unwrap_or_default();
    assert!(
        parent_payload.contains("omnicrawl/subagent/"),
        "父回合应看到隔离分支：{parent_payload}"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// `computer_use` 的端到端验收：主回合调用它 → 内核跑专职子代理 → 折回主回合。
///
/// 判据：
/// 1. `computer_use` 不占宿主批次（宿主收到的每批都没有它）；
/// 2. 子代理内部的桌面工具调用走宿主，`turn_id` 带 `computer-use-` 前缀（免审批据）；
/// 3. 子代理的 system 提示是专职定义那份，工具面被收窄到桌面工具；
/// 4. 子代理收尾 JSON 里的 `blocked` 被投影成 `status=blocked` 而非错误；
/// 5. 它不受 `[subagents] enabled` 管辖：这里 `subagents` 是关闭的。
#[test]
fn computer_use_runs_dedicated_subagent_without_subagents_switch() {
    // 子代理总开关关闭，但 computer_use 打开：桌面能力应当照常可用。
    let root =
        std::env::temp_dir().join(format!("omnicrawl-computer-use-e2e-{}", std::process::id()));
    let workspace = root.join("ws");
    let home = root.join("home");
    let agents = home.join(".OmniCrawl").join("agents");
    std::fs::create_dir_all(&workspace).expect("建工作区");
    std::fs::create_dir_all(&agents).expect("建定义目录");
    // 专职定义：这里自己写一份，避免依赖二进制旁是否有模板目录。
    std::fs::write(
        agents.join("computer-use.md"),
        "---\nname: computer-use\ndescription: 桌面操作\nmodel: stub-child\npermissionMode: standard\nisolation: shared\n---\n你是桌面操作子代理。\n",
    )
    .expect("写专职定义");
    std::fs::write(
        home.join(".OmniCrawl").join("subagents.toml"),
        "[subagents]\nenabled = false\n",
    )
    .expect("写子代理配置（关闭）");
    std::fs::write(
        home.join(".OmniCrawl").join("config.toml"),
        "[computer_use]\nenabled = true\ntimeout_seconds = 120\n",
    )
    .expect("写 computer_use 配置");

    let child_report = "{\"status\": \"blocked\", \"summary\": \"找不到保存按钮\", \
                        \"obstacle\": \"窗口高度不足\", \"screenshots\": [\"shot.png\"]}";
    let server = StubServer::spawn(vec![
        ok_stream(tool_call_stream(
            "computer_use",
            "{\"prompt\":\"在记事本里保存文件\",\"timeout_seconds\":60}",
        )),
        ok_stream(tool_call_stream(
            "windows_input",
            "{\"action\":\"click\",\"x\":10,\"y\":20}",
        )),
        ok_stream(text_stream(child_report)),
        ok_stream(text_stream(PARENT_TEXT)),
    ]);

    let mut host = Host::start(&workspace, &agents, &home.join(".OmniCrawl"));
    handshake(
        &mut host,
        &server,
        json!([
            {"type": "function", "function": {"name": "windows_input"}},
            {"type": "function", "function": {"name": "windows_screenshot"}},
            {"type": "function", "function": {"name": "computer_use"}}
        ]),
        None,
    );

    let finished = host.run_turn("帮我点一下保存");
    host.shutdown();

    assert_eq!(finished.len(), 1, "应恰好收到一次 turn.finished");
    assert_eq!(
        finished[0]["params"]["final_text"].as_str().unwrap(),
        PARENT_TEXT
    );

    // 1. computer_use 由内核自持，绝不进宿主批次。
    for (turn_id, names, _root) in &host.batches {
        assert!(
            !names.iter().any(|name| name == "computer_use"),
            "computer_use 不该出现在宿主批次里：{turn_id} {names:?}"
        );
    }

    // 2. 子代理的桌面调用走宿主，turn_id 带专用前缀（免审批的依据）。
    assert!(
        host.batches
            .iter()
            .any(|(turn_id, names, _)| turn_id.starts_with("computer-use-")
                && names.iter().any(|name| name == "windows_input")),
        "子代理的 windows_input 应以 computer-use- 前缀落到宿主：{:?}",
        host.batches
    );

    let requests = server.requests.lock().expect("请求锁").clone();
    // 3. 子回合的请求带专职提示词，且工具面收窄。
    let child_request = requests
        .iter()
        .find(|value| {
            serde_json::to_string(value)
                .unwrap_or_default()
                .contains("你是桌面操作子代理")
        })
        .expect("应当存在一次带专职系统提示的子回合请求")
        .clone();
    let child_text = serde_json::to_string(&child_request).unwrap_or_default();
    assert!(
        !child_text.contains("\"computer_use\""),
        "子代理工具面不该包含 computer_use 本身：{child_text}"
    );
    assert!(
        child_text.contains("windows_input"),
        "子代理工具面应包含桌面工具：{child_text}"
    );
    assert!(
        child_text.contains("stub-child"),
        "定义里的模型应当生效：{child_text}"
    );

    // 4. blocked 是业务结果而非错误：父回合的观察里必须看得到 summary 与截图路径。
    let parent_text = serde_json::to_string(&requests[requests.len() - 1]).unwrap_or_default();
    assert!(
        parent_text.contains("blocked"),
        "父回合应看到 blocked 状态：{parent_text}"
    );
    assert!(
        parent_text.contains("找不到保存按钮"),
        "父回合应看到 summary：{parent_text}"
    );
    assert!(
        parent_text.contains("窗口高度不足"),
        "父回合应看到阻塞点：{parent_text}"
    );
    assert!(
        parent_text.contains("shot.png"),
        "父回合应看到截图路径：{parent_text}"
    );

    let _ = std::fs::remove_dir_all(&root);
}
