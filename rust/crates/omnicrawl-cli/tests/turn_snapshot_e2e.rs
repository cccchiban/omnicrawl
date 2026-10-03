//! 回合快照（`/undo` 的副作用回滚依据）的端到端验收：真拉起内核、真 git 工作区，
//! 走完「模型要写工具 → 宿主执行 → 内核回合收尾拍快照」这条链。
//!
//! 判据在会话转录与 artifact 目录上：写工具轮次落一条 `turn_snapshot` 事件并写出
//! `undo/` 四个文件；纯读轮次两样都不落。

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::Duration;

use serde_json::{json, Value};

const WAIT: Duration = Duration::from_secs(20);
const TEST_KEY: &str = "test-key";
const FINAL_TEXT: &str = "回合收尾";

fn text_stream(text: &str) -> String {
    format!(
        "data: {{\"choices\":[{{\"delta\":{{\"content\":\"{text}\"}}}}]}}\n\n\
         data: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"stop\"}}]}}\n\n\
         data: [DONE]\n\n"
    )
}

fn tool_call_stream(name: &str, arguments: &str) -> String {
    let escaped = arguments.replace('"', "\\\"");
    format!(
        "data: {{\"choices\":[{{\"delta\":{{\"tool_calls\":[{{\"index\":0,\"id\":\"call-1\",\
         \"type\":\"function\",\"function\":{{\"name\":\"{name}\",\"arguments\":\"{escaped}\"}}}}]}}}}]}}\n\n\
         data: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\n\
         data: [DONE]\n\n"
    )
}

/// 回环服务端：第一次请求要工具，之后收尾。
struct StubServer {
    addr: String,
}

impl StubServer {
    fn spawn(tool: &'static str, arguments: &'static str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("无法监听回环端口");
        let addr = listener.local_addr().expect("无法取本地地址");
        let text = FINAL_TEXT.to_string();

        thread::spawn(move || {
            let mut count = 0usize;
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let _ = read_request(&mut stream);
                count += 1;
                let payload = if count == 1 {
                    tool_call_stream(tool, arguments)
                } else {
                    text_stream(&text)
                };
                let reply = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\n\
                     Connection: close\r\n\r\n{payload}",
                    payload.len()
                );
                let _ = stream.write_all(reply.as_bytes());
                let _ = stream.flush();
            }
        });

        Self {
            addr: format!("http://{addr}/v1"),
        }
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

/// 把配置放到临时 home 的 `~/.OmniCrawl/config.toml` 并返回该 home：
/// 配置路径不再有环境变量覆盖，隔离只能靠 home。
fn isolate_home(config_path: &Path) -> PathBuf {
    let home = config_path
        .parent()
        .expect("配置目录")
        .join("home");
    let target = home.join(".OmniCrawl");
    std::fs::create_dir_all(&target).expect("建隔离配置目录");
    std::fs::copy(config_path, target.join("config.toml")).expect("复制配置");
    home
}

struct Kernel {
    child: Child,
    stdin: ChildStdin,
    frames: Receiver<Value>,
}

impl Kernel {
    fn spawn(config_path: &Path) -> Self {
        let home = isolate_home(config_path);
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
        let line = format!("{frame}\n");
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

    fn initialize(&mut self, model: Value, session_root: &Path, workspace: &Path) {
        self.send(json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocol_version": "1.0",
                "client": {"name": "turn-snapshot-e2e"},
                "model": model,
                "session": {
                    "root": session_root.to_string_lossy(),
                    "workspace_root": workspace.to_string_lossy(),
                },
            },
        }));
        let response = self.next_frame();
        assert_eq!(response["id"], 1, "initialize 应答：{response}");
        assert!(
            response.get("error").is_none(),
            "initialize 失败：{response}"
        );
    }

    /// 提交回合：收到 `tool.batch` 就由宿主真执行写工具，直到 `turn.finished`。
    fn run_turn(&mut self, user_text: &str, workspace: &Path) -> Vec<Value> {
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
                let call = &frame["params"]["calls"][0];
                let call_id = call["id"].as_str().unwrap_or("call-1");
                let name = call["name"].as_str().unwrap_or_default().to_string();
                let output = execute_tool(&name, call, workspace);
                let id = frame["id"].clone();
                self.send(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {"observations": [{
                        "tool_call": call,
                        "result": {"ok": true, "output": output, "full_output": output},
                        "message": {"role": "tool", "tool_call_id": call_id, "content": output},
                        "followup_messages": [],
                    }]},
                }));
            }
            let finished = method == "turn.finished";
            collected.push(frame);
            if finished {
                return collected;
            }
        }
    }

    /// 发 `turn.undo` 并返回它的响应帧（跳过其间可能到达的通知）。
    fn undo(&mut self) -> Value {
        self.send(json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "turn.undo",
            "params": {},
        }));
        loop {
            let frame = self.next_frame();
            if frame["id"] == 3 {
                return frame;
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

/// 宿主侧工具执行：写工具真改工作区，读工具只回文本。
fn execute_tool(name: &str, call: &Value, workspace: &Path) -> String {
    if name == "write_file" {
        let relative = call["arguments"]["path"].as_str().unwrap_or("a.txt");
        let path = workspace.join(relative);
        std::fs::write(&path, "被工具改写\n").expect("写文件失败");
        return format!("已写入 {relative}");
    }
    "文件内容：初始内容".to_string()
}

fn run_git(workspace: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(workspace)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("无法执行 git");
    assert!(status.success(), "git {:?} 失败", args);
}

/// 临时 Git 工作区：一个已提交的 `a.txt`，满足快照所需的 HEAD。
fn prepare_workspace(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("omnicrawl-undo-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("建工作区");
    run_git(&root, &["init"]);
    std::fs::write(root.join("a.txt"), "初始内容\n").expect("写初始文件");
    run_git(&root, &["add", "."]);
    run_git(
        &root,
        &[
            "-c",
            "user.email=e2e@example.com",
            "-c",
            "user.name=e2e",
            "commit",
            "-m",
            "init",
        ],
    );
    root
}

/// 一份空配置：避免测试进程读用户真实配置（例如已启用的 [vision]）。
fn write_config(name: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("omnicrawl-undo-cfg-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("建临时配置目录");
    let path = dir.join("config.toml");
    std::fs::write(&path, "").expect("写配置失败");
    path
}

/// 会话转录全文（递归收集 `.jsonl`）。
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

/// 找到 artifact 里的 undo 目录（`artifacts/<会话 id>/undo`）。
fn undo_dir(root: &Path) -> Option<PathBuf> {
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if path.file_name().map(|name| name == "undo").unwrap_or(false) {
                    return Some(path);
                }
                pending.push(path);
            }
        }
    }
    None
}

fn model_config(server: &StubServer) -> Value {
    json!({
        "model": "main-model",
        "base_url": server.addr,
        "api_key_env": "OMNICRAWL_TEST_KEY",
        "system_prompt": "你是助手。",
        "tools": [{
            "type": "function",
            "function": {
                "name": "write_file",
                "description": "写文件",
                "parameters": {"type": "object", "properties": {"path": {"type": "string"}, "content": {"type": "string"}}}
            }
        }],
        "request_timeout_seconds": 5,
    })
}

#[test]
fn write_turn_captures_a_snapshot() {
    let server = StubServer::spawn("write_file", "{\"path\":\"a.txt\",\"content\":\"改了\"}");
    let config = write_config("write");
    let workspace = prepare_workspace("write");
    let session_root = config.parent().expect("配置目录").join("sessions");

    let mut kernel = Kernel::spawn(&config);
    kernel.initialize(model_config(&server), &session_root, &workspace);
    let frames = kernel.run_turn("改一下 a.txt", &workspace);
    assert!(
        frames
            .iter()
            .any(|frame| frame["method"] == "turn.finished"),
        "回合应正常收尾：{frames:?}"
    );

    // ① 会话里出现快照事件，且记下了执行过的工具。
    let transcript = session_transcript(&session_root);
    assert!(
        transcript.contains("turn_snapshot"),
        "写工具轮次应落 turn_snapshot 事件：{transcript}"
    );
    assert!(
        transcript.contains("write_file"),
        "事件应记下执行过的工具：{transcript}"
    );
    assert!(
        transcript.contains("undo/begin.patch") && transcript.contains("undo/end.patch"),
        "事件应指向四个快照文件：{transcript}"
    );

    // ② 四个快照文件真的落盘；终点补丁非空（工作区被改过）。
    let undo = undo_dir(&session_root).expect("应有 artifacts/<会话 id>/undo 目录");
    for name in [
        "begin.patch",
        "begin.untracked.txt",
        "end.patch",
        "end.untracked.txt",
    ] {
        assert!(undo.join(name).is_file(), "{name} 应存在");
    }
    let end_patch = std::fs::read_to_string(undo.join("end.patch")).expect("读 end.patch");
    assert!(!end_patch.is_empty(), "终点补丁应记录本轮改动");

    let _ = std::fs::remove_dir_all(&workspace);
    let _ = std::fs::remove_dir_all(config.parent().expect("配置目录"));
}

#[test]
fn read_only_turn_captures_nothing() {
    let server = StubServer::spawn("read", "{\"path\":\"a.txt\"}");
    let config = write_config("read");
    let workspace = prepare_workspace("read");
    let session_root = config.parent().expect("配置目录").join("sessions");

    let mut kernel = Kernel::spawn(&config);
    kernel.initialize(model_config(&server), &session_root, &workspace);
    let frames = kernel.run_turn("读一下 a.txt", &workspace);
    assert!(
        frames
            .iter()
            .any(|frame| frame["method"] == "turn.finished"),
        "回合应正常收尾：{frames:?}"
    );

    // 纯读轮次不拍快照、不落盘、不产生事件（与 Python 一致）。
    let transcript = session_transcript(&session_root);
    assert!(
        !transcript.contains("turn_snapshot"),
        "纯读轮次不应有快照事件：{transcript}"
    );
    assert!(
        undo_dir(&session_root).is_none(),
        "纯读轮次不应产生 undo 目录"
    );

    let _ = std::fs::remove_dir_all(&workspace);
    let _ = std::fs::remove_dir_all(config.parent().expect("配置目录"));
}

#[test]
fn undo_restores_workspace_and_session() {
    let server = StubServer::spawn("write_file", "{\"path\":\"a.txt\",\"content\":\"改了\"}");
    let config = write_config("undo");
    let workspace = prepare_workspace("undo");
    let session_root = config.parent().expect("配置目录").join("sessions");

    let mut kernel = Kernel::spawn(&config);
    kernel.initialize(model_config(&server), &session_root, &workspace);
    let frames = kernel.run_turn("改一下 a.txt", &workspace);
    assert!(
        frames
            .iter()
            .any(|frame| frame["method"] == "turn.finished"),
        "回合应正常收尾：{frames:?}"
    );
    // 先用一次写工具把工作区改掉（宿主真执行）。
    assert_eq!(
        std::fs::read_to_string(workspace.join("a.txt")).expect("读 a.txt"),
        "被工具改写\n"
    );

    // 撤销：恢复工作区副作用 + 回退会话逻辑。
    let response = kernel.undo();
    assert!(
        response.get("error").is_none(),
        "turn.undo 应成功：{response}"
    );
    assert_eq!(response["result"]["kind"], json!("complete"), "{response}");
    assert_eq!(
        response["result"]["side_effects_reverted"],
        json!(true),
        "本轮有写入，应回滚了副作用：{response}"
    );
    let restored = std::fs::read_to_string(workspace.join("a.txt")).expect("读 a.txt");
    assert_eq!(
        restored.replace("\r\n", "\n"),
        "初始内容\n",
        "工作区应回到本轮开始前"
    );

    let transcript = session_transcript(&session_root);
    assert!(
        transcript.contains("turn_undone"),
        "会话应落 turn_undone 事件：{transcript}"
    );

    let _ = std::fs::remove_dir_all(&workspace);
    let _ = std::fs::remove_dir_all(config.parent().expect("配置目录"));
}

#[test]
fn undo_without_history_is_rejected() {
    let server = StubServer::spawn("read", "{\"path\":\"a.txt\"}");
    let config = write_config("undo-empty");
    let workspace = prepare_workspace("undo-empty");
    let session_root = config.parent().expect("配置目录").join("sessions");

    let mut kernel = Kernel::spawn(&config);
    kernel.initialize(model_config(&server), &session_root, &workspace);

    // 还没有任何回合：撤销应被拒绝，而不是静默成功。
    let response = kernel.undo();
    let error = response.get("error").expect("空会话撤销应报错");
    assert!(
        error["message"]
            .as_str()
            .unwrap_or_default()
            .contains("回退"),
        "错误文案应说明无轮次可回退：{response}"
    );

    let _ = std::fs::remove_dir_all(&workspace);
    let _ = std::fs::remove_dir_all(config.parent().expect("配置目录"));
}
