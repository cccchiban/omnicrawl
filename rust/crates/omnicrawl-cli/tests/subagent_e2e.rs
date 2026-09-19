//! 内核自持 `subagent` 工具的端到端验收：主回合要求调用 subagent → 内核跑子回合（子回合里真的
//! 调用了一个工具）→ 折回主回合；以及 `fail_fast` 在前序任务失败后停止调度。
//!
//! 判据全在协议帧与内核实际发出的模型请求体上：
//! 1. `subagent` 本身不占宿主批次（宿主收到的每一批 `tool.batch` 里都没有 `subagent` 调用）；
//! 2. 子代理内部的工具调用**走宿主**，且 `turn_id` 是子任务号；
//! 3. 子回合的模型请求带角色定义的 system 提示与 `<subagent_task>` 指令，并带回工具观察；
//! 4. 子代理的 `subagent.event` 通知按 `batch.created` → `task.started` → `task.completed` 顺序发出；
//! 5. 主回合的下一次请求里带上了子任务的 tool 观察；
//! 6. `fail_fast` 生效时，前序任务失败后剩余任务不再被调度，而是落成 `cancelled`。

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
const FAIL_FAST_REASON: &str = "fail_fast 已在前序任务失败后停止调度该任务。";

/// 回环服务端的一次应答：正常 SSE，或一段完整原始响应（用来造失败）。
enum Reply {
    Stream(String),
    Raw(String),
    /// 先要求调工具、拿到结果后再收尾；脚本用尽时按请求内容兜底（并发下顺序不定）。
    ToolThenText {
        tool: String,
        tool_arguments: String,
        text: String,
    },
    /// 任何请求都回同一段文本（并发下顺序不定时用来验证调度本身）。
    AlwaysText(String),
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
                let reply = match script.get(index) {
                    Some(Reply::Stream(body)) => sse_response(body),
                    Some(Reply::Raw(raw)) => raw.clone(),
                    Some(Reply::AlwaysText(text)) => sse_response(&text_stream(text)),
                    Some(Reply::ToolThenText { .. }) | None => {
                        // 脚本用尽：有 AlwaysText 就继续用它，否则按请求内容兜底
                        // （已经回过工具结果就要文本，否则要求调工具）。
                        let always = script.iter().find_map(|item| match item {
                            Reply::AlwaysText(text) => Some(text),
                            _ => None,
                        });
                        let tool_rule = script.iter().find_map(|item| match item {
                            Reply::ToolThenText {
                                tool,
                                tool_arguments,
                                text,
                            } => Some((tool, tool_arguments, text)),
                            _ => None,
                        });
                        match (always, tool_rule) {
                            (Some(text), _) => sse_response(&text_stream(text)),
                            (None, Some((tool, arguments, _)))
                                if !raw.contains("\"role\":\"tool\"") =>
                            {
                                sse_response(&tool_call_stream(tool, arguments))
                            }
                            (None, Some((_, _, text))) => sse_response(&text_stream(text)),
                            (None, None) => sse_response(&text_stream("（脚本用尽）")),
                        }
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
    fn start(workspace: &PathBuf, agents: &PathBuf, config: &PathBuf) -> Self {
        let home = workspace
            .parent()
            .map(|parent| parent.join("home"))
            .unwrap_or_else(|| workspace.join("home"));
        let mut child = Command::new(env!("CARGO_BIN_EXE_omnicrawl"))
            .current_dir(workspace)
            .env("OPENAI_API_KEY", TEST_KEY)
            .env("USERPROFILE", &home)
            .env("HOME", &home)
            .env("AI_SUBAGENTS_FILE", config)
            .env("OMNICRAWL_SUBAGENTS_DIR", agents)
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

    /// 收到某条通知就返回；期间照常服务工具批次。
    fn wait_for_notification(&mut self, needle: &str, timeout: Duration) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        while std::time::Instant::now() < deadline {
            let Ok(line) = self.lines.recv_timeout(Duration::from_millis(200)) else {
                continue;
            };
            let frame: Value = serde_json::from_str(&line).expect("帧是合法 JSON");
            if frame.get("method").and_then(Value::as_str) == Some("subagent.event")
                && frame
                    .get("params")
                    .and_then(|params| params.get("name"))
                    .and_then(Value::as_str)
                    .is_some_and(|name| name.contains(needle))
            {
                self.events.push(frame);
                return true;
            }
            self.handle_side_frame(&frame);
        }
        false
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

/// 准备一套临时工作区：定义目录 + 子代理配置。
fn prepare_root(tag: &str, config_body: &str) -> (PathBuf, PathBuf, PathBuf, PathBuf) {
    let root =
        std::env::temp_dir().join(format!("omnicrawl-subagent-{tag}-{}", std::process::id()));
    let workspace = root.join("ws");
    let agents = root.join("agents");
    let config = root.join("subagents.toml");
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
        "tasks": [{
            "description": "看代码",
            "prompt": "读一下 a.py",
            "subagent_type": "review",
        }],
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
        "[subagents]\nenabled = true\nmax_concurrency = 1\nmax_tasks_per_batch = 1\n",
    );

    let mut host = Host::start(&workspace, &agents, &config);
    handshake(
        &mut host,
        &server,
        json!([
            {"type": "function", "function": {"name": "read"}},
            {"type": "function", "function": {"name": "grep"}},
            {"type": "function", "function": {"name": "subagent"}}
        ]),
        None,
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
fn fail_fast_stops_scheduling_after_first_failure() {
    let tool_arguments = json!({
        "action": "run",
        "fail_fast": true,
        "tasks": [
            {"description": "第一个", "prompt": "读一下 a.py", "subagent_type": "review"},
            {"description": "第二个", "prompt": "读一下 b.py", "subagent_type": "review"},
        ],
    })
    .to_string();
    // 子回合第一次请求直接失败；fail_fast 生效时第二个任务不会再被调度。
    let server = StubServer::spawn(vec![
        ok_stream(tool_call_stream("subagent", &tool_arguments)),
        server_error(),
        ok_stream(text_stream(PARENT_TEXT)),
    ]);
    let (root, workspace, agents, config) = prepare_root(
        "ff",
        "[subagents]\nenabled = true\nmax_concurrency = 1\nmax_tasks_per_batch = 2\n",
    );

    let mut host = Host::start(&workspace, &agents, &config);
    handshake(
        &mut host,
        &server,
        json!([
            {"type": "function", "function": {"name": "read"}},
            {"type": "function", "function": {"name": "subagent"}}
        ]),
        Some(0),
    );

    let finished = host.run_turn("帮我评审");
    host.shutdown();

    assert_eq!(finished.len(), 1, "应恰好收到一次 turn.finished");

    // 6. 第二个任务没有被调度：子回合请求只有一个，且它是第一个任务的。
    let requests = server.requests.lock().expect("请求锁").clone();
    let child_requests: Vec<String> = requests
        .iter()
        .map(|item| serde_json::to_string(item).unwrap_or_default())
        .filter(|text| text.contains("subagent_task"))
        .collect();
    assert_eq!(
        child_requests.len(),
        1,
        "fail_fast 生效后只应有一个子回合请求：{child_requests:?}"
    );
    assert!(
        child_requests[0].contains("读一下 a.py"),
        "唯一被调度的应是第一个任务"
    );
    assert!(
        !child_requests[0].contains("读一下 b.py"),
        "第二个任务不该进入子回合"
    );

    let started = host.notifications("subagent.event", Some("subagent.task.started"));
    let cancelled = host.notifications("subagent.event", Some("subagent.task.cancelled"));
    assert_eq!(started.len(), 1, "只应有一个任务真正开始");
    assert_eq!(cancelled.len(), 1, "未调度的任务应落成取消事件");
    let cancelled_text = serde_json::to_string(&cancelled[0]).unwrap_or_default();
    assert!(
        cancelled_text.contains(FAIL_FAST_REASON),
        "取消原因应为 fail_fast 文案：{cancelled_text}"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn worktree_isolation_creates_and_reports_isolated_workspace() {
    let tool_arguments = json!({
        "action": "run",
        "tasks": [{
            "description": "写点东西",
            "prompt": "在隔离区里读一下 a.txt",
            "subagent_type": "writer",
        }],
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
    let agents = root.join("agents");
    let config = root.join("subagents.toml");
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
max_concurrency = 1
max_tasks_per_batch = 1
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

#[test]
fn background_spawn_runs_after_turn_and_is_queryable() {
    let spawn_arguments = json!({
        "action": "spawn",
        "tasks": [{
            "description": "后台看代码",
            "prompt": "读一下 a.py",
            "subagent_type": "review",
        }],
    })
    .to_string();
    let list_arguments = json!({"action": "list"}).to_string();
    // 脚本顺序：父回合受理 spawn → 父回合收尾 → 后台子回合调工具 → 后台收尾 →
    // 第二个回合查 list → 收尾。宿主会等后台收尾事件，所以顺序是确定的。
    let server = StubServer::spawn(vec![
        ok_stream(tool_call_stream("subagent", &spawn_arguments)),
        ok_stream(text_stream(PARENT_TEXT)),
        ok_stream(tool_call_stream("read", "{\"path\":\"a.py\"}")),
        ok_stream(text_stream(CHILD_TEXT)),
        ok_stream(tool_call_stream("subagent", &list_arguments)),
        ok_stream(text_stream(PARENT_TEXT)),
    ]);
    let (root, workspace, agents, config) = prepare_root(
        "bg",
        "[subagents]\nenabled = true\nallow_background = true\nmax_concurrency = 1\nmax_tasks_per_batch = 1\n",
    );

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

    // 回合 1：父回合受理后台任务后立刻收尾，不等子任务。
    let first = host.run_turn("后台看代码");
    assert_eq!(first.len(), 1, "应恰好收到一次 turn.finished");
    assert_eq!(
        first[0]["params"]["final_text"].as_str().unwrap(),
        PARENT_TEXT,
        "父回合应直接收尾而不是等后台任务"
    );

    // 等后台任务收尾；期间它的工具批次仍要有人接。
    assert!(
        host.wait_for_notification("completed", WAIT),
        "后台任务应发出收尾事件；批次={:?}",
        host.batches
    );

    // 后台任务的工具批次用子任务号发出（不占父回合的 id）。
    assert!(
        host.batches.iter().any(|(turn_id, names, _)| {
            turn_id.starts_with("task-") && names.iter().any(|name| name == "read")
        }),
        "后台任务的工具批次应以子任务号发出：{:?}",
        host.batches
    );

    // 回合 2：模型查后台任务，观察里应能看到完成态与摘要。
    let second = host.run_turn("看看后台任务");
    host.shutdown();
    assert_eq!(second.len(), 1, "第二个回合也应收尾");

    let requests = server.requests.lock().expect("请求锁").clone();
    let last = requests
        .last()
        .map(|item| serde_json::to_string(item).unwrap_or_default())
        .unwrap_or_default();
    assert!(last.contains("completed"), "查询结果里应是完成态：{last}");
    assert!(
        last.contains(CHILD_TEXT),
        "后台任务的摘要应回给父回合：{last}"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn max_concurrency_runs_tasks_in_parallel() {
    let tool_arguments = json!({
        "action": "run",
        "max_concurrency": 2,
        "tasks": [
            {"description": "第一个", "prompt": "读一下 a.py", "subagent_type": "review"},
            {"description": "第二个", "prompt": "读一下 b.py", "subagent_type": "review"},
        ],
    })
    .to_string();
    // 两个子任务的请求会交错：只固定主回合的第一次请求，其余一律回同一段文本。
    let server = StubServer::spawn(vec![
        ok_stream(tool_call_stream("subagent", &tool_arguments)),
        Reply::AlwaysText(CHILD_TEXT.to_string()),
    ]);
    let (root, workspace, agents, config) = prepare_root(
        "concurrency",
        "[subagents]
enabled = true
max_concurrency = 2
max_tasks_per_batch = 2
",
    );

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

    let finished = host.run_turn("并发看两个文件");
    host.shutdown();

    assert_eq!(finished.len(), 1, "应恰好收到一次 turn.finished");
    assert_eq!(
        finished[0]["params"]["final_text"].as_str().unwrap(),
        CHILD_TEXT
    );

    // 两个子任务都在同一批里跑完，各自发出开始与完成事件。
    let started = host.notifications("subagent.event", Some("subagent.task.started"));
    let completed = host.notifications("subagent.event", Some("subagent.task.completed"));
    assert_eq!(started.len(), 2, "两个子任务都应发出开始事件");
    assert_eq!(completed.len(), 2, "两个子任务都应发出完成事件");

    // 父回合的观察里应带回两条完成结果，且任务号互不相同。
    let requests = server.requests.lock().expect("请求锁").clone();
    let parent_payload = serde_json::to_string(&requests[requests.len() - 1]).unwrap_or_default();
    assert!(
        parent_payload.matches("completed").count() >= 2,
        "父回合应看到两个完成结果：{parent_payload}"
    );
    assert!(
        parent_payload.matches("task-").count() >= 2,
        "两条结果应带各自的子任务号：{parent_payload}"
    );

    let _ = std::fs::remove_dir_all(&root);
}
