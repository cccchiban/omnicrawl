//! 工具输出压缩旁路的端到端验收：真拉起内核，走完「模型要工具 → 宿主回超长观察 →
//! 内核调压缩模型 → 用精简文本继续对话」这条链。
//!
//! 判据全在协议帧与内核实际发出的模型请求体上：压缩请求带内置系统提示与输出包裹标记，
//! 下一轮主请求里出现的是精简文本而不是原始长输出。

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use serde_json::{json, Value};

const WAIT: Duration = Duration::from_secs(15);
const TEST_KEY: &str = "test-key";
const COMPRESSED_TEXT: &str = "精简观察：一行结论";
const FINAL_TEXT: &str = "压缩后用精简文本继续";

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

/// 回环服务端：按请求内容决定回应——压缩请求回精简文本，主模型第一次回工具调用、之后回终稿。
struct StubServer {
    addr: String,
    requests: Arc<Mutex<Vec<Value>>>,
}

impl StubServer {
    fn spawn() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("无法监听回环端口");
        let addr = listener.local_addr().expect("无法取本地地址");
        let requests: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&requests);

        thread::spawn(move || {
            let mut main_requests = 0usize;
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let raw = read_request(&mut stream);
                let body: Value = serde_json::from_str(&raw).unwrap_or(Value::Null);
                recorded.lock().expect("记录锁").push(body.clone());
                let is_compression = body.to_string().contains("<<<TOOL_OUTPUT_START>>>");
                let payload = if is_compression {
                    text_stream(COMPRESSED_TEXT)
                } else {
                    main_requests += 1;
                    if main_requests == 1 {
                        tool_call_stream("bash", "{\"command\":\"cat big.log\"}")
                    } else {
                        text_stream(FINAL_TEXT)
                    }
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

struct Kernel {
    child: Child,
    stdin: ChildStdin,
    frames: Receiver<Value>,
}

impl Kernel {
    fn spawn(config_path: &std::path::Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_omnicrawl"))
            .env("OMNICRAWL_TEST_KEY", TEST_KEY)
            .env("AI_CONFIG_FILE", config_path)
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

    fn initialize(&mut self, model: Value) {
        self.send(json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {"protocol_version": "1.0", "client": {"name": "compression-e2e"}, "model": model},
        }));
        let response = self.next_frame();
        assert!(
            response.get("error").is_none(),
            "initialize 失败：{response}"
        );
    }

    /// 提交回合：收到 `tool.batch` 就回一条超长观察，直到 `turn.finished`。
    fn run_turn(&mut self, user_text: &str, tool_output: &str) -> Vec<Value> {
        self.send(json!({
            "jsonrpc": "2.0", "id": 2, "method": "turn.submit",
            "params": {"turn_id": "turn-1", "user_text": user_text},
        }));

        let mut collected = Vec::new();
        loop {
            let frame = self.next_frame();
            let method = frame["method"].as_str().unwrap_or_default().to_string();
            if method == "tool.batch" {
                let params = frame["params"].clone();
                let id = frame["id"].clone();
                self.send(json!({
                    "jsonrpc": "2.0", "id": id,
                    "result": {"observations": [observation_for(&params, tool_output)]},
                }));
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

fn observation_for(batch: &Value, output: &str) -> Value {
    let call = &batch["calls"][0];
    let id = call["id"].as_str().unwrap_or("call-1");
    json!({
        "tool_call": call,
        "result": {"ok": true, "output": output, "full_output": output},
        "message": {"role": "tool", "tool_call_id": id, "content": output},
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
            "function": {"name": "bash", "description": "跑命令", "parameters": {"type": "object"}}
        }],
        "request_timeout_seconds": 5,
    })
}

/// 写一份启用压缩的 config.toml，返回路径。
fn write_config(name: &str, min_chars: usize) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("omnicrawl-compress-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("建临时配置目录");
    let path = dir.join("config.toml");
    std::fs::write(
        &path,
        format!(
            "[tool_output_compression]\nenabled = true\nmodel_key = \"compress-model\"\n\
             thinking_enabled = false\nreasoning_effort = \"high\"\nmin_chars = {min_chars}\n\
             max_input_chars = 4000\nmax_output_chars = 400\ntimeout_seconds = 5\n"
        ),
    )
    .expect("写配置失败");
    path
}

#[test]
fn long_tool_output_is_compressed_before_next_request() {
    let server = StubServer::spawn();
    let config = write_config("enabled", 100);
    let mut kernel = Kernel::spawn(&config);
    kernel.initialize(model_config(&server));

    let long_output = "日志行内容".repeat(200);
    let frames = kernel.run_turn("看一下日志", &long_output);
    assert!(
        frames
            .iter()
            .any(|frame| frame["method"] == "turn.finished"),
        "回合应正常收尾：{frames:?}"
    );

    let bodies = server.bodies();
    let compression = bodies
        .iter()
        .find(|body| body.to_string().contains("<<<TOOL_OUTPUT_START>>>"))
        .expect("应当发出一次压缩请求");
    let compression_text = compression.to_string();
    assert!(
        compression_text.contains("请压缩下面这次工具调用的原始输出。"),
        "压缩请求要带任务与调用背景：{compression_text}"
    );
    assert!(
        compression_text.contains("工具输出压缩"),
        "压缩请求的系统提示来自内置模板：{compression_text}"
    );

    let follow_up = bodies
        .iter()
        .rfind(|body| !body.to_string().contains("<<<TOOL_OUTPUT_START>>>"))
        .expect("应当有后续主请求");
    let follow_up_text = follow_up.to_string();
    assert!(
        follow_up_text.contains(COMPRESSED_TEXT),
        "后续主请求应带精简文本：{follow_up_text}"
    );
    assert!(
        !follow_up_text.contains(&long_output[..long_output.len() / 2]),
        "原始长输出不应再进上下文"
    );

    std::fs::remove_dir_all(config.parent().expect("配置目录")).ok();
}

#[test]
fn compression_model_key_is_resolved_through_the_profile_connection() {
    // 回归：`model_key` 形如 `profile/model_id` 时，要按模型切换同一套口径解析出
    // 真正的 model 名与连接（以前把整个 key 当模型名发给上游，上游会回
    // 「模型不存在或当前账号无权使用该模型」）。
    let server = StubServer::spawn();
    let config = write_config_with_profile("profile-key", &server.addr, 100);
    let mut kernel = Kernel::spawn(&config);
    kernel.initialize(model_config(&server));

    let long_output = "日志行内容".repeat(200);
    kernel.run_turn("看一下日志", &long_output);

    let bodies = server.bodies();
    let compression = bodies
        .iter()
        .find(|body| body.to_string().contains("<<<TOOL_OUTPUT_START>>>"))
        .expect("应当发出一次压缩请求");
    assert_eq!(
        compression["model"].as_str(),
        Some("sub-model-x"),
        "压缩请求应带解析后的模型名，而不是 `stub/sub-model-x` 这个 key：{compression}"
    );
    // 连接也跟着被选中的 Profile 走（请求真到了本回环服务端才可能被记下来）。
    assert!(
        compression["messages"].is_array(),
        "压缩请求应是一份正常对话：{compression}"
    );
    std::fs::remove_dir_all(config.parent().expect("配置目录")).ok();
}

/// 写一份「有 [llm] Profile，压缩选择用 `profile/model_id` 形式」的 config.toml。
fn write_config_with_profile(name: &str, base_url: &str, min_chars: usize) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("omnicrawl-compress-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("建临时配置目录");
    let path = dir.join("config.toml");
    std::fs::write(
        &path,
        format!(
            "version = 2\n\
             [llm]\n\
             [llm.active_model]\n\
             source = \"detected\"\n\
             profile = \"stub\"\n\
             model_id = \"main-model\"\n\
             protocol = \"openai_chat_completions\"\n\
             [llm.profiles.stub]\n\
             provider = \"openai\"\n\
             base_url = \"{base_url}\"\n\
             api_key = \"{TEST_KEY}\"\n\
             [tool_output_compression]\nenabled = true\nmodel_key = \"stub/sub-model-x\"\n\
             thinking_enabled = false\nreasoning_effort = \"high\"\nmin_chars = {min_chars}\n\
             max_input_chars = 4000\nmax_output_chars = 400\ntimeout_seconds = 5\n"
        ),
    )
    .expect("写配置失败");
    path
}

#[test]
fn short_tool_output_is_left_alone() {
    let server = StubServer::spawn();
    // 门槛高于观察长度：不该发出压缩请求。
    let config = write_config("threshold", 100_000);
    let mut kernel = Kernel::spawn(&config);
    kernel.initialize(model_config(&server));

    let frames = kernel.run_turn("看一下日志", "短的输出");
    assert!(frames
        .iter()
        .any(|frame| frame["method"] == "turn.finished"));

    let bodies = server.bodies();
    assert!(
        !bodies
            .iter()
            .any(|body| body.to_string().contains("<<<TOOL_OUTPUT_START>>>")),
        "未达门槛不应压缩：{bodies:?}"
    );

    std::fs::remove_dir_all(config.parent().expect("配置目录")).ok();
}
