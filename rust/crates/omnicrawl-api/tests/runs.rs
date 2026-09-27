//! runs 端点的端到端测试：脚本化假内核 + 真实回环 HTTP。
//!
//! 覆盖 `POST /runs`、状态轮询、SSE 全量与游标重放、取消、人工审批与提问，
//! 以及隐藏推理不进事件流、未知任务与服务未就绪的错误码。

use std::collections::VecDeque;
use std::io::{self, BufReader, Read, Write};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use omnicrawl_api::{
    build_router, build_router_with_state, AgentService, ApiConfig, ApiState, ServiceOptions,
};
use omnicrawl_host::approval::ApprovalMode;
use omnicrawl_host::kernel::KernelClient;
use omnicrawl_host::tools::RegistryOptions;
use omnicrawl_host::turn::{RunnerOptions, TurnRunner};
use omnicrawl_ipc::bridge::KernelModelConfig;
use serde_json::{json, Value};

const TOKEN: &str = "parity-token";
/// 等待状态翻转的上限。
const WAIT: Duration = Duration::from_secs(10);

/// 假内核的输出：脚本喂完就阻塞等更多输入（避免“脚本读完 = 内核退出”的假信号）。
#[derive(Clone, Default)]
struct Script {
    inner: Arc<(Mutex<ScriptInner>, Condvar)>,
}

#[derive(Default)]
struct ScriptInner {
    buffer: VecDeque<u8>,
    closed: bool,
}

impl Script {
    fn push(&self, line: &str) {
        let (lock, wake) = &*self.inner;
        let mut inner = lock.lock().expect("脚本未被毒化");
        inner.buffer.extend(line.as_bytes());
        inner.buffer.push_back(b'\n');
        wake.notify_all();
    }
}

impl Read for Script {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        let (lock, wake) = &*self.inner;
        let mut inner = lock.lock().expect("脚本未被毒化");
        loop {
            if !inner.buffer.is_empty() {
                let take = out.len().min(inner.buffer.len());
                for slot in out.iter_mut().take(take) {
                    *slot = inner.buffer.pop_front().unwrap_or_default();
                }
                return Ok(take);
            }
            if inner.closed {
                return Ok(0);
            }
            inner = wake.wait(inner).expect("等待脚本");
        }
    }
}

/// 宿主写出的帧记录器。
#[derive(Clone, Default)]
struct Recorder(Arc<Mutex<Vec<u8>>>);

impl Recorder {
    fn methods(&self) -> Vec<String> {
        let bytes = self.0.lock().expect("记录缓冲未被毒化").clone();
        String::from_utf8(bytes)
            .expect("帧是 UTF-8")
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter_map(|frame| {
                frame
                    .get("method")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .collect()
    }
}

impl Write for Recorder {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.0
            .lock()
            .expect("记录缓冲未被毒化")
            .extend_from_slice(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// 一套「脚本化内核 + Agent 服务」。
struct Harness {
    service: Arc<AgentService>,
    recorder: Recorder,
    script: Script,
}

impl Harness {
    /// 起假内核、握手、建服务。服务发出的 `initialize` 占帧 id 1，`turn.submit` 占 2。
    fn new(tag: &str) -> Self {
        let root = workspace(tag);
        let script = Script::default();
        let recorder = Recorder::default();
        let client = KernelClient::from_streams(
            Box::new(BufReader::new(script.clone())),
            Box::new(recorder.clone()),
        );
        script.push(r#"{"jsonrpc":"2.0","id":1,"result":{"protocol_version":"1.0"}}"#);
        let mut runner =
            TurnRunner::new(client, runner_options(&root), &RegistryOptions::default())
                .expect("工具表应当建成");
        runner.handshake(&mut |_| {}).expect("握手应当成功");

        let mut options = ServiceOptions::new(root, model());
        options.confirmation_timeout_seconds = 5.0;
        Self {
            service: Arc::new(AgentService::with_runner(options, runner)),
            recorder,
            script,
        }
    }

    fn address(&self) -> SocketAddr {
        spawn(build_router_with_state(state_with(Arc::clone(
            &self.service,
        ))))
    }

    /// 只有 submit 响应的回合（取消测试用：回合永远不结束）。
    fn submit_only(&self) {
        self.script.push(r#"{"jsonrpc":"2.0","id":2,"result":{}}"#);
    }

    /// 无工具调用的简单回合：增量 + 隐藏推理 + 收尾。
    fn simple_turn(&self) {
        self.submit_only();
        self.script
            .push(r#"{"jsonrpc":"2.0","method":"turn.delta","params":{"text":"你好"}}"#);
        self.script.push(
            r#"{"jsonrpc":"2.0","method":"turn.reasoning_delta","params":{"text":"秘密推理"}}"#,
        );
        self.script.push(concat!(
            r#"{"jsonrpc":"2.0","method":"turn.finished","params":{"turn_id":"turn-1","#,
            r#""final_text":"你好，世界","reasoning":"","model_turns":1,"tool_calls":0,"paused":false}}"#,
        ));
    }

    /// 带工具批次的回合：需要审批的 bash、就地执行的清单、需要作答的提问。
    fn tool_turn(&self) {
        self.submit_only();
        self.script.push(concat!(
            r#"{"jsonrpc":"2.0","id":3,"method":"tool.batch","params":{"turn_id":"turn-1","step":1,"calls":["#,
            r#"{"name":"bash","arguments":{"command":"echo hi","api_key":"TOP_SECRET"},"id":"c1","function_name":"bash"},"#,
            r#"{"name":"update_todos","arguments":{"todos":[{"id":"1","step":"写测试","completed":false}]},"id":"c2","function_name":"update_todos"},"#,
            r#"{"name":"ask_user","arguments":{"question":"选哪个？","options":["选项A","选项B"],"kind":"select"},"id":"c3","function_name":"ask_user"}]}}"#,
        ));
        self.script.push(concat!(
            r#"{"jsonrpc":"2.0","method":"turn.finished","params":{"turn_id":"turn-1","#,
            r#""final_text":"好","reasoning":"","model_turns":1,"tool_calls":3,"paused":false}}"#,
        ));
    }
}

fn workspace(tag: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("oc-api-runs-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).expect("建立临时工作区");
    path
}

fn model() -> KernelModelConfig {
    KernelModelConfig {
        model: "test-model".to_string(),
        provider: String::new(),
        protocol: String::new(),
        base_url: String::new(),
        api_key_env: String::new(),
        user_agent: "omnicrawl-test".to_string(),
        system_prompt: "系统提示".to_string(),
        context_messages: Vec::new(),
        tools: Vec::new(),
        options: json!({}),
        request_timeout_seconds: None,
        context_window_tokens: 0,
        prompt_cache_capable: false,
        prompt_cache_identity: Default::default(),
        native_vision: false,
        request_retry_count: 1,
    }
}

fn runner_options(root: &Path) -> RunnerOptions {
    RunnerOptions {
        workspace_root: root.to_path_buf(),
        model: model(),
        session: None,
        approval: ApprovalMode::Manual,
        command_timeout_seconds: 5,
        tool_timeout_seconds: 5,
        attach_vision_images: false,
        client_name: "omnicrawl-api-test".to_string(),
        // 测试进程不装插件运行期：所有 Hook 节点都退化为原样放行。
        plugins: None,
        // 无审查模型：review 模式下的需审查调用按 fail-closed 拒绝。
        review: None,
        prompt: None,
    }
}

fn config() -> ApiConfig {
    ApiConfig::new(TOKEN, "127.0.0.1", 8765, Vec::new(), 300.0, 1).expect("合法配置")
}

fn state_with(service: Arc<AgentService>) -> ApiState {
    ApiState::new(config()).with_service(service)
}

/// 把应用挂到真实回环端口上。
fn spawn(app: axum::Router) -> SocketAddr {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("绑定回环端口");
    listener.set_nonblocking(true).expect("设为非阻塞");
    let address = listener.local_addr().expect("读取监听地址");
    thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("构建 tokio 运行时");
        runtime.block_on(async move {
            let listener = tokio::net::TcpListener::from_std(listener).expect("接管监听");
            let _ = axum::serve(listener, app).await;
        });
    });
    address
}

struct Reply {
    status: u16,
    body: String,
}

impl Reply {
    fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or(Value::Null)
    }
}

fn request(
    method: &str,
    address: SocketAddr,
    path: &str,
    body: Option<Value>,
    last_event_id: Option<&str>,
) -> Reply {
    let url = format!("http://{address}{path}");
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .build()
        .into();
    let mut common = Vec::new();
    if let Some(value) = last_event_id {
        common.push(("Last-Event-ID", value.to_string()));
    }
    let response = if method == "POST" {
        let mut builder = agent
            .post(&url)
            .header("Authorization", &format!("Bearer {TOKEN}"));
        for (name, value) in &common {
            builder = builder.header(*name, value.as_str());
        }
        match body {
            Some(payload) => builder
                .header("Content-Type", "application/json")
                .send(payload.to_string())
                .expect("请求应当到达服务端"),
            None => builder.send_empty().expect("请求应当到达服务端"),
        }
    } else {
        let mut builder = agent
            .get(&url)
            .header("Authorization", &format!("Bearer {TOKEN}"));
        for (name, value) in &common {
            builder = builder.header(*name, value.as_str());
        }
        builder.call().expect("请求应当到达服务端")
    };
    let status = response.status().as_u16();
    let mut reader = response.into_body().into_reader();
    let mut text = String::new();
    let _ = reader.read_to_string(&mut text);
    Reply { status, body: text }
}

fn post_run(address: SocketAddr, message: &str) -> Reply {
    request(
        "POST",
        address,
        "/api/v1/runs",
        Some(json!({"message": message})),
        None,
    )
}

fn run_id_of(reply: &Reply) -> String {
    reply.json()["data"]["run_id"]
        .as_str()
        .expect("响应应当带 run_id")
        .to_string()
}

/// 轮询任务状态直到进入期望集合。
fn wait_for_status(address: SocketAddr, run_id: &str, expected: &[&str]) -> Value {
    let deadline = Instant::now() + WAIT;
    loop {
        let reply = request(
            "GET",
            address,
            &format!("/api/v1/runs/{run_id}"),
            None,
            None,
        );
        let payload = reply.json();
        let status = payload["data"]["status"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        if expected.contains(&status.as_str()) {
            return payload;
        }
        assert!(
            Instant::now() < deadline,
            "等待状态 {expected:?} 超时，当前：{status}"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

/// 跟随流（默认 `follow=true`）：任务到终态时流自己结束。
fn events_of(address: SocketAddr, run_id: &str) -> Reply {
    request(
        "GET",
        address,
        &format!("/api/v1/runs/{run_id}/events"),
        None,
        None,
    )
}

/// 一次性快照（`follow=false`）：回合还在等人工决定时必须用这个，否则会一直挂到终态。
fn events_snapshot(address: SocketAddr, run_id: &str) -> Reply {
    request(
        "GET",
        address,
        &format!("/api/v1/runs/{run_id}/events?follow=false"),
        None,
        None,
    )
}

/// 从事件流里取出某个事件的数据对象。
fn event_json(body: &str, event: &str) -> Option<Value> {
    for block in body.split(
        "

",
    ) {
        if !block.contains(&format!("event: {event}")) {
            continue;
        }
        let data = block
            .lines()
            .find_map(|line| line.strip_prefix("data: "))
            .unwrap_or_default();
        return serde_json::from_str::<Value>(data).ok();
    }
    None
}

/// 从事件流里取出某个事件的数据字段。
fn event_field(body: &str, event: &str, field: &str) -> Option<String> {
    for block in body.split("\n\n") {
        if !block.contains(&format!("event: {event}")) {
            continue;
        }
        let data = block
            .lines()
            .find_map(|line| line.strip_prefix("data: "))
            .unwrap_or_default();
        let value: Value = serde_json::from_str(data).ok()?;
        if let Some(text) = value.get(field).and_then(Value::as_str) {
            return Some(text.to_string());
        }
    }
    None
}

#[test]
fn run_completes_and_events_replay_from_cursor() {
    let harness = Harness::new("complete");
    harness.simple_turn();
    let address = harness.address();

    let created = post_run(address, "你好");
    assert_eq!(created.status, 202);
    assert_eq!(created.json()["data"]["status"], "pending");
    let run_id = run_id_of(&created);

    let finished = wait_for_status(address, &run_id, &["completed"]);
    assert_eq!(finished["data"]["result"], "你好，世界");

    let full = events_of(address, &run_id);
    assert_eq!(full.status, 200);
    assert!(full.body.contains("event: run.started"), "{}", full.body);
    assert!(
        full.body.contains("event: assistant.delta"),
        "{}",
        full.body
    );
    assert!(full.body.contains("event: run.completed"), "{}", full.body);
    assert!(
        full.body.starts_with("id: 1\nevent: run.started\n"),
        "{}",
        full.body
    );
    assert!(
        !full.body.contains("秘密推理"),
        "隐藏推理不得进事件流：{}",
        full.body
    );

    let replay = request(
        "GET",
        address,
        &format!("/api/v1/runs/{run_id}/events"),
        None,
        Some("1"),
    );
    assert!(!replay.body.contains("id: 1\n"), "{}", replay.body);
    assert!(replay.body.contains("id: 2\n"), "{}", replay.body);
}

#[test]
fn manual_approval_and_question_close_the_loop() {
    let harness = Harness::new("decisions");
    harness.tool_turn();
    let address = harness.address();

    let created = post_run(address, "跑个命令");
    let run_id = run_id_of(&created);

    wait_for_status(address, &run_id, &["waiting_confirmation"]);
    let events = events_snapshot(address, &run_id);
    let required =
        event_json(&events.body, "confirmation.required").expect("应当发出 confirmation.required");
    assert_eq!(required["tool"], "bash");
    assert_eq!(required["timeout_seconds"], 5.0);
    // bash 这类工具的投影就是原参数（与 Python 的 `public_tool_arguments` 一致）。
    assert_eq!(required["arguments"]["command"], "echo hi");
    let confirmation_id = required["confirmation_id"]
        .as_str()
        .expect("confirmation_id")
        .to_string();

    let decided = request(
        "POST",
        address,
        &format!("/api/v1/runs/{run_id}/confirmations/{confirmation_id}"),
        Some(json!({"approved": false})),
        None,
    );
    assert_eq!(decided.status, 200);
    assert_eq!(decided.json()["data"]["approved"], false);

    let again = request(
        "POST",
        address,
        &format!("/api/v1/runs/{run_id}/confirmations/{confirmation_id}"),
        Some(json!({"approved": true})),
        None,
    );
    assert_eq!(again.status, 409, "重复决议必须 409");
    assert_eq!(again.json()["error"]["code"], "CONFIRMATION_RESOLVED");

    wait_for_status(address, &run_id, &["waiting_user"]);
    let events = events_snapshot(address, &run_id);
    let question_id = event_field(&events.body, "ask_user.required", "question_id")
        .expect("应当发出 ask_user.required");

    let invalid = request(
        "POST",
        address,
        &format!("/api/v1/runs/{run_id}/questions/{question_id}"),
        Some(json!({"answer": "不在选项里"})),
        None,
    );
    assert_eq!(invalid.status, 400, "响应：{}", invalid.body);
    assert_eq!(invalid.json()["error"]["code"], "INVALID_ANSWER");

    let answered = request(
        "POST",
        address,
        &format!("/api/v1/runs/{run_id}/questions/{question_id}"),
        Some(json!({"answer": "选项A"})),
        None,
    );
    assert_eq!(answered.status, 200, "响应：{}", answered.body);
    assert_eq!(answered.json()["data"]["answer"], "选项A");

    wait_for_status(address, &run_id, &["completed"]);
    let full = events_snapshot(address, &run_id);
    assert!(full.body.contains("event: todo.updated"), "{}", full.body);
    assert!(full.body.contains("event: tool.completed"), "{}", full.body);
    assert!(full.body.contains("event: tool.started"), "{}", full.body);
}

#[test]
fn cancel_marks_run_cancelled_and_asks_kernel_to_stop() {
    let harness = Harness::new("cancel");
    harness.submit_only();
    let address = harness.address();

    let created = post_run(address, "长任务");
    let run_id = run_id_of(&created);
    wait_for_status(address, &run_id, &["running"]);

    let cancelled = request(
        "POST",
        address,
        &format!("/api/v1/runs/{run_id}/cancel"),
        Some(json!({})),
        None,
    );
    assert_eq!(cancelled.status, 200);
    wait_for_status(address, &run_id, &["cancelled"]);

    assert!(
        harness
            .recorder
            .methods()
            .iter()
            .any(|method| method == "turn.cancel"),
        "取消必须请内核停止：{:?}",
        harness.recorder.methods()
    );
    let events = events_of(address, &run_id);
    assert!(
        events.body.contains("event: run.cancelled"),
        "{}",
        events.body
    );
}

#[test]
fn unknown_run_and_bad_cursor_report_contract_errors() {
    let harness = Harness::new("errors");
    harness.simple_turn();
    let address = harness.address();

    let missing = request("GET", address, "/api/v1/runs/missing", None, None);
    assert_eq!(missing.status, 404);
    assert_eq!(missing.json()["error"]["code"], "RUN_NOT_FOUND");

    let created = post_run(address, "x");
    let run_id = run_id_of(&created);
    let bad = request(
        "GET",
        address,
        &format!("/api/v1/runs/{run_id}/events"),
        None,
        Some("abc"),
    );
    assert_eq!(bad.status, 400);
    assert_eq!(bad.json()["error"]["code"], "INVALID_EVENT_ID");

    let empty = request(
        "POST",
        address,
        "/api/v1/runs",
        Some(json!({"message": ""})),
        None,
    );
    assert_eq!(empty.status, 422);
    assert_eq!(empty.json()["error"]["code"], "VALIDATION_ERROR");
}

#[test]
fn without_agent_service_routes_report_unavailable() {
    let address = spawn(build_router(config()));
    let reply = request("GET", address, "/api/v1/runs/whatever", None, None);
    assert_eq!(reply.status, 503);
    assert_eq!(reply.json()["error"]["code"], "SERVICE_UNAVAILABLE");
}
