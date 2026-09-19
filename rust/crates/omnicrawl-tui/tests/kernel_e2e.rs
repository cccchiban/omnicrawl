//! 端到端：真拉起 `omnicrawl` 内核 + 本机回环模型服务端，把一轮对话跑成界面上的记录。
//!
//! 这个用例需要内核二进制：先执行 `cargo build -p omnicrawl-cli`（debug 或 release 都行），
//! 找不到二进制时用例会跳过并在输出里说明原因。

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use serde_json::{json, Value};

use omnicrawl_tui::app::App;
use omnicrawl_tui::args::{ApprovalMode, Options};
use omnicrawl_tui::kernel::KernelClient;
use omnicrawl_tui::state::{Record, ToolStatus};

const WAIT: Duration = Duration::from_secs(20);
const TEST_KEY: &str = "test-key";

/// 固定 SSE 响应：一段文本 + 结束原因，随后 [DONE]。
fn text_stream() -> String {
    let text = json!({"choices": [{"delta": {"content": "你好"}}]});
    let finish = json!({"choices": [{"delta": {}, "finish_reason": "stop"}]});
    format!("data: {text}\n\ndata: {finish}\n\ndata: [DONE]\n\n")
}

/// 本机回环模型服务端：只回一段固定 SSE，并记录收到的请求体。
struct StubServer {
    base_url: String,
    requests: Arc<Mutex<Vec<Value>>>,
}

impl StubServer {
    /// `bodies` 按请求顺序消费；用完后重复最后一段。
    fn spawn(bodies: Vec<String>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("无法监听回环端口");
        let addr = listener.local_addr().expect("无法取本地地址");
        let requests: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&requests);
        thread::spawn(move || {
            for (served, stream) in listener.incoming().enumerate() {
                let Ok(mut stream) = stream else { break };
                let raw = read_request(&mut stream);
                if let Ok(value) = serde_json::from_str::<Value>(&raw) {
                    recorded.lock().expect("记录锁").push(value);
                }
                let body = bodies
                    .get(served)
                    .or_else(|| bodies.last())
                    .cloned()
                    .unwrap_or_default();
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
            base_url: format!("http://{addr}/v1"),
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

/// 内核二进制：优先 release，其次 debug；都没有就返回 `None`（用例跳过）。
fn kernel_binary() -> Option<PathBuf> {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let target = manifest.parent()?.parent()?.join("target");
    let name = format!("omnicrawl{}", std::env::consts::EXE_SUFFIX);
    [
        target.join("release").join(&name),
        target.join("debug").join(&name),
    ]
    .into_iter()
    .find(|path| path.is_file())
}

/// 每个用例一个干净的真实工作区。
fn workspace_dir(name: &str) -> PathBuf {
    let root =
        std::env::temp_dir().join(format!("omnicrawl-tui-e2e-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("创建临时工作区");
    root
}

#[test]
fn real_kernel_streams_a_turn_into_the_ui() {
    let Some(binary) = kernel_binary() else {
        eprintln!("跳过：找不到内核二进制，先执行 `cargo build -p omnicrawl-cli`");
        return;
    };
    let workspace = workspace_dir("turn");
    let server = StubServer::spawn(vec![text_stream()]);
    // 凭据只经环境变量名传递，帧里不带 Key。
    std::env::set_var("OMNICRAWL_TUI_E2E_KEY", TEST_KEY);

    let options = Options {
        kernel: binary,
        model: "stub-model".to_string(),
        base_url: server.base_url.clone(),
        api_key_env: "OMNICRAWL_TUI_E2E_KEY".to_string(),
        system_prompt: "你是端到端测试助手。".to_string(),
        session_root: None,
        context_window_tokens: Some(128_000),
        approval: ApprovalMode::Manual,
        command_timeout_seconds: 360,
        tool_timeout_seconds: 120,
        native_vision: false,
        image_gen: Default::default(),
        advisor: Default::default(),
    };
    let kernel = KernelClient::spawn(&options.kernel).expect("无法启动内核进程");
    let mut app = App::new(options, kernel, &workspace).expect("工具表应当构建成功");
    app.handshake().expect("握手应当成功");

    app.state.composer.insert("你好");
    app.handle_event(Event::Key(KeyEvent::new(
        KeyCode::Enter,
        KeyModifiers::NONE,
    )));

    let deadline = Instant::now() + WAIT;
    let answered = loop {
        app.drain_frames();
        if app
            .state
            .records
            .iter()
            .any(|record| matches!(record, Record::Assistant(text) if text.contains("你好")))
        {
            break true;
        }
        if Instant::now() >= deadline {
            break false;
        }
        thread::sleep(Duration::from_millis(20));
    };

    app.request_shutdown();
    app.kernel.wait_or_kill(Duration::from_secs(5));

    assert!(
        answered,
        "模型流没有进界面；界面记录：{:?}",
        app.state.records
    );
    assert!(!app.quit, "正常结束时不该被判成内核异常退出");
    assert!(
        !app.state.turn.is_running(),
        "回合结束通知应把界面复位：{:?}",
        app.state.turn
    );
    assert!(
        app.state
            .records
            .iter()
            .any(|record| matches!(record, Record::User(text) if text == "你好")),
        "用户消息应留在消息流里：{:?}",
        app.state.records
    );

    let bodies = server.bodies();
    let request = bodies.first().expect("模型服务端应收到一次请求");
    assert_eq!(request["model"], "stub-model");
    let serialized = request.to_string();
    assert!(
        serialized.contains("端到端测试助手"),
        "请求体应带上宿主给内核的系统提示词"
    );
    assert!(!serialized.contains(TEST_KEY), "凭据不能出现在请求体里");
}

/// 模型流里的一次工具调用（OpenAI Chat 分片形状，用 serde_json 生成避免手写转义）。
fn tool_call_stream(name: &str, arguments: &str) -> String {
    let open = json!({"choices": [{"delta": {"tool_calls": [{
        "index": 0, "id": "call_1", "type": "function",
        "function": {"name": name, "arguments": ""}
    }]}}]});
    let args = json!({"choices": [{"delta": {"tool_calls": [{
        "index": 0, "function": {"arguments": arguments}
    }]}}]});
    let finish = json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]});
    format!(
        "data: {open}

data: {args}

data: {finish}

data: [DONE]

"
    )
}

/// 第二段：模型看到工具结果后的收尾回复。
fn after_tool_stream(text: &str) -> String {
    let text = json!({"choices": [{"delta": {"content": text}}]});
    let finish = json!({"choices": [{"delta": {}, "finish_reason": "stop"}]});
    format!(
        "data: {text}

data: {finish}

data: [DONE]

"
    )
}

#[test]
fn real_kernel_executes_an_approved_tool_call() {
    let Some(binary) = kernel_binary() else {
        eprintln!("跳过：找不到内核二进制，先执行 `cargo build -p omnicrawl-cli`");
        return;
    };
    let workspace = workspace_dir("tools");
    std::fs::write(
        workspace.join("a.txt"),
        "工具读到的内容
",
    )
    .expect("写测试文件");
    let server = StubServer::spawn(vec![
        tool_call_stream("read", "{\"path\":\"a.txt\"}"),
        after_tool_stream("已读到文件"),
    ]);
    std::env::set_var("OMNICRAWL_TUI_E2E_TOOLS_KEY", "test-key");

    let options = Options {
        kernel: binary,
        model: "stub-model".to_string(),
        base_url: server.base_url.clone(),
        api_key_env: "OMNICRAWL_TUI_E2E_TOOLS_KEY".to_string(),
        system_prompt: "你是端到端测试助手。".to_string(),
        session_root: None,
        context_window_tokens: Some(128_000),
        approval: ApprovalMode::Manual,
        command_timeout_seconds: 360,
        tool_timeout_seconds: 120,
        native_vision: false,
        image_gen: Default::default(),
        advisor: Default::default(),
    };
    let kernel = KernelClient::spawn(&options.kernel).expect("无法启动内核进程");
    let mut app = App::new(options, kernel, &workspace).expect("工具表应当构建成功");
    app.handshake().expect("握手应当成功");

    app.state.composer.insert("读一下 a.txt");
    app.handle_event(Event::Key(KeyEvent::new(
        KeyCode::Enter,
        KeyModifiers::NONE,
    )));

    // read 在 manual 模式下无需审批：直接等模型收尾。
    let deadline = Instant::now() + WAIT;
    let mut answered = false;
    while Instant::now() < deadline {
        app.drain_frames();
        assert!(
            app.state.waiting().is_none(),
            "文件类工具不该弹审批：{:?}",
            app.state.waiting()
        );
        if app
            .state
            .records
            .iter()
            .any(|record| matches!(record, Record::Assistant(text) if text.contains("已读到文件")))
        {
            answered = true;
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }

    app.request_shutdown();
    app.kernel.wait_or_kill(Duration::from_secs(5));

    assert!(answered, "模型收尾回复没有进界面：{:?}", app.state.records);
    assert!(
        app.state.records.iter().any(
            |record| matches!(record, Record::Tool(card) if card.name == "read" && card.status == ToolStatus::Ok)
        ),
        "应当留下成功的 read 工具卡：{:?}",
        app.state.records
    );

    // 第二次请求体里必须带上工具结果：证明执行真的发生并回填给了模型。
    let bodies = server.bodies();
    assert!(
        bodies.len() >= 2,
        "模型服务端应收到两轮请求：{}",
        bodies.len()
    );
    let second = bodies[1].to_string();
    assert!(
        second.contains("工具读到的内容"),
        "工具结果没有进第二轮请求：{second}"
    );
    let declared = bodies[0]["tools"]
        .as_array()
        .map(Vec::len)
        .unwrap_or_default();
    assert!(declared >= 8, "握手应声明已实现的工具：{declared}");
    let names: Vec<String> = bodies[0]["tools"]
        .as_array()
        .map(|tools| {
            tools
                .iter()
                .filter_map(|tool| tool["function"]["name"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    for expected in ["read", "write_file", "Edit_file", "bash", "powershell"] {
        assert!(
            names.contains(&expected.to_string()),
            "缺少声明 {expected}：{names:?}"
        );
    }
}

/// 用真内核跑一轮「模型请求某个工具 → 界面批准 → 工具真执行 → 结果回填给模型」。
///
/// 返回启动好的 App 与模型桩服务端；调用方负责断言副作用与请求体。
fn drive_tool_turn(
    workspace_name: &str,
    seed_files: &[(&str, &str)],
    tool_name: &str,
    arguments: &str,
    final_text: &str,
) -> (App, StubServer, PathBuf, bool) {
    let Some(binary) = kernel_binary() else {
        panic!("跳过：找不到内核二进制，先执行 `cargo build -p omnicrawl-cli`");
    };
    let workspace = workspace_dir(workspace_name);
    for (name, content) in seed_files {
        let path = workspace.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("建种子目录");
        }
        std::fs::write(&path, content).expect("写初始文件");
    }
    let server = StubServer::spawn(vec![
        tool_call_stream(tool_name, arguments),
        after_tool_stream(final_text),
    ]);
    std::env::set_var("OMNICRAWL_TUI_E2E_TOOL_KEY", TEST_KEY);

    let options = Options {
        kernel: binary,
        model: "stub-model".to_string(),
        base_url: server.base_url.clone(),
        api_key_env: "OMNICRAWL_TUI_E2E_TOOL_KEY".to_string(),
        system_prompt: "你是端到端测试助手。".to_string(),
        session_root: None,
        context_window_tokens: Some(128_000),
        approval: ApprovalMode::Manual,
        command_timeout_seconds: 360,
        tool_timeout_seconds: 120,
        native_vision: false,
        image_gen: Default::default(),
        advisor: Default::default(),
    };
    let kernel = KernelClient::spawn(&options.kernel).expect("无法启动内核进程");
    let mut app = App::new(options, kernel, &workspace).expect("工具表应当构建成功");
    app.handshake().expect("握手应当成功");

    app.state.composer.insert("执行一次工具");
    app.handle_event(Event::Key(KeyEvent::new(
        KeyCode::Enter,
        KeyModifiers::NONE,
    )));

    // 一边收帧一边等模型收尾：manual 模式下只有 shell 命令会弹审批，出现就批准。
    let deadline = Instant::now() + WAIT;
    let mut approved = false;
    let mut answered = false;
    while Instant::now() < deadline {
        app.drain_frames();
        if app.state.waiting().is_some() {
            app.handle_event(Event::Key(KeyEvent::new(
                KeyCode::Char('y'),
                KeyModifiers::NONE,
            )));
            approved = true;
            continue;
        }
        if app
            .state
            .records
            .iter()
            .any(|record| matches!(record, Record::Assistant(text) if text.contains(final_text)))
        {
            answered = true;
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
    app.request_shutdown();
    app.kernel.wait_or_kill(Duration::from_secs(5));
    assert!(answered, "模型收尾回复没有进界面：{:?}", app.state.records);

    (app, server, workspace, approved)
}

#[test]
fn real_kernel_edits_a_file_after_approval() {
    let (app, server, workspace, approved) = drive_tool_turn(
        "edit",
        &[(
            "a.txt",
            "旧内容
",
        )],
        "Edit_file",
        "{\"path\":\"a.txt\",\"old_text\":\"旧内容\",\"new_text\":\"新内容\"}",
        "已改好文件",
    );

    assert!(!approved, "文件类工具在 manual 模式下不该弹审批");
    // 1) 磁盘上真的改了。
    let content = std::fs::read_to_string(workspace.join("a.txt")).expect("读回被改的文件");
    assert_eq!(content, "新内容\n", "Edit_file 应当真的改写了文件");

    // 2) 工具卡是成功的，并且结果回填给了模型（第二轮请求体里能看到替换摘要）。
    assert!(
        app.state.records.iter().any(
            |record| matches!(record, Record::Tool(card) if card.name == "Edit_file" && card.status == ToolStatus::Ok)
        ),
        "应当留下成功的 Edit_file 工具卡：{:?}",
        app.state.records
    );
    let bodies = server.bodies();
    assert!(bodies.len() >= 2, "模型服务端应收到两轮请求");
    let second = bodies[1].to_string();
    assert!(
        second.contains("已修改"),
        "替换结果没有进第二轮请求：{second}"
    );
    assert!(
        second.contains("新内容") || second.contains("替换 1 处"),
        "{second}"
    );
}

#[test]
fn real_kernel_runs_bash_after_approval() {
    let (app, server, workspace, approved) = drive_tool_turn(
        "bash",
        &[],
        "bash",
        "{\"command\":\"echo 命令真的跑了 > out.txt && echo done\"}",
        "命令执行完毕",
    );

    assert!(approved, "shell 命令在 manual 模式下应先弹审批");
    // 1) 命令的副作用真的落到磁盘上（bash 工具在 Windows 上走 Git Bash）。
    let produced = workspace.join("out.txt");
    if produced.is_file() {
        let content = std::fs::read_to_string(&produced).expect("读回命令输出文件");
        assert!(content.contains("命令真的跑了"), "命令输出不符：{content}");
    } else {
        // 没有 Git Bash 的机器上，工具会明确报错；那也必须是一条失败观察而不是空等。
        let failed = app.state.records.iter().any(|record| {
            matches!(record, Record::Tool(card) if card.name == "bash" && card.status != ToolStatus::Ok)
        });
        assert!(
            failed,
            "没有 bash 时应当留下失败工具卡：{:?}",
            app.state.records
        );
        eprintln!("跳过副作用断言：本机没有可用的 Git Bash");
    }

    let bodies = server.bodies();
    assert!(bodies.len() >= 2, "模型服务端应收到两轮请求");
    let second = bodies[1].to_string();
    assert!(
        second.contains("退出码：0"),
        "命令结果没有进第二轮请求：{second}"
    );
}

/// 与 `drive_tool_turn` 同流程，但复用调用方准备好的工作区（例如已 `git init` 的仓库）。
fn drive_tool_turn_in(
    workspace: &std::path::Path,
    tool_name: &str,
    arguments: &str,
    final_text: &str,
) -> (App, StubServer, bool) {
    let Some(binary) = kernel_binary() else {
        panic!("跳过：找不到内核二进制，先执行 `cargo build -p omnicrawl-cli`");
    };
    let server = StubServer::spawn(vec![
        tool_call_stream(tool_name, arguments),
        after_tool_stream(final_text),
    ]);
    std::env::set_var("OMNICRAWL_TUI_E2E_TOOL_KEY", TEST_KEY);
    let options = Options {
        kernel: binary,
        model: "stub-model".to_string(),
        base_url: server.base_url.clone(),
        api_key_env: "OMNICRAWL_TUI_E2E_TOOL_KEY".to_string(),
        system_prompt: "你是端到端测试助手。".to_string(),
        session_root: None,
        context_window_tokens: Some(128_000),
        approval: ApprovalMode::Manual,
        command_timeout_seconds: 360,
        tool_timeout_seconds: 120,
        native_vision: false,
        image_gen: Default::default(),
        advisor: Default::default(),
    };
    let kernel = KernelClient::spawn(&options.kernel).expect("无法启动内核进程");
    let mut app = App::new(options, kernel, workspace).expect("工具表应当构建成功");
    app.handshake().expect("握手应当成功");

    app.state.composer.insert("执行一次工具");
    app.handle_event(Event::Key(KeyEvent::new(
        KeyCode::Enter,
        KeyModifiers::NONE,
    )));

    let deadline = Instant::now() + WAIT;
    let mut approved = false;
    let mut answered = false;
    while Instant::now() < deadline {
        app.drain_frames();
        if app.state.waiting().is_some() {
            app.handle_event(Event::Key(KeyEvent::new(
                KeyCode::Char('y'),
                KeyModifiers::NONE,
            )));
            approved = true;
            continue;
        }
        if app
            .state
            .records
            .iter()
            .any(|record| matches!(record, Record::Assistant(text) if text.contains(final_text)))
        {
            answered = true;
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
    app.request_shutdown();
    app.kernel.wait_or_kill(Duration::from_secs(5));
    assert!(answered, "模型收尾回复没有进界面：{:?}", app.state.records);
    (app, server, approved)
}

#[test]
fn real_kernel_searches_with_grep_without_approval() {
    let (app, server, workspace, approved) = drive_tool_turn(
        "grep",
        &[("src/agent.py", "class Agent:\n    pass\n")],
        "grep",
        "{\"pattern\":\"class Agent\",\"path\":\"src\"}",
        "已经找到定义",
    );

    assert!(!approved, "grep 在 manual 模式下不该弹审批");
    assert!(
        app.state.records.iter().any(
            |record| matches!(record, Record::Tool(card) if card.name == "grep" && card.status == ToolStatus::Ok)
        ),
        "应当留下成功的 grep 工具卡：{:?}",
        app.state.records
    );
    let bodies = server.bodies();
    assert!(bodies.len() >= 2, "模型服务端应收到两轮请求");
    let second = bodies[1].to_string();
    assert!(
        second.contains("agent.py:1: class Agent:"),
        "搜索结果没有进第二轮请求：{second}"
    );
    assert!(workspace.join("src").join("agent.py").is_file());
}

#[test]
fn real_kernel_verifies_changes_with_git_status() {
    let probe = std::process::Command::new("git").arg("--version").output();
    if probe.is_err() {
        eprintln!("跳过：本机没有 git");
        return;
    }
    let workspace = workspace_dir("git");
    let init = std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(&workspace)
        .output();
    if init.map(|output| !output.status.success()).unwrap_or(true) {
        eprintln!("跳过：git init 失败");
        return;
    }

    let (app, _server, approved) =
        drive_tool_turn_in(&workspace, "git", "{\"action\":\"status\"}", "看完了状态");

    assert!(!approved, "只读 git 操作不该要人工确认");
    assert!(
        app.state.records.iter().any(
            |record| matches!(record, Record::Tool(card) if card.name == "git" && card.status == ToolStatus::Ok)
        ),
        "应当留下成功的 git 工具卡：{:?}",
        app.state.records
    );
}
