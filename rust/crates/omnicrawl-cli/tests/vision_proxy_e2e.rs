//! 独立视觉模型代理的端到端验收：真拉起内核，走完「模型要 read_image → 宿主回带图观察 →
//! 内核调视觉模型 → 用文本分析继续对话」这条链。
//!
//! 判据全在协议帧与内核实际发出的模型请求体上：视觉请求带 data URL 图片、用候选模型名，
//! 下一轮主请求里换成 `<vision_observation>` 文本，图片不再进上下文。
//!
//! 另外两个分支也被钉住：宿主声明的 `initialize.model.native_vision` 为真时原生视觉优先，
//! 图片直送主模型且**不**调视觉模型；代理未启用时图片被掉（非视觉主模型只收到图片元数据）。

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
const VISION_MODEL: &str = "vision-model";
const VISION_TEXT: &str = "图片里是一张终端截图。";
const FINAL_TEXT: &str = "截图里是终端窗口";
const IMAGE_DATA: &str = "QUJD";
const TOOL_OUTPUT: &str = "已读取图片：shot.png";

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

/// 回环服务端：按请求里的 `model` 分流——视觉候选回分析文本，主模型先要工具再收尾。
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
                let is_vision = body.get("model").and_then(Value::as_str) == Some(VISION_MODEL);
                let payload = if is_vision {
                    text_stream(VISION_TEXT)
                } else {
                    main_requests += 1;
                    if main_requests == 1 {
                        tool_call_stream(
                            "read_image",
                            "{\"path\":\"shot.png\",\"prompt\":\"看看这张图\"}",
                        )
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
    fn spawn(config_path: &Path) -> Self {
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

    fn initialize(&mut self, model: Value, session_root: &Path) {
        self.send(json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocol_version": "1.0",
                "client": {"name": "vision-proxy-e2e"},
                "model": model,
                "session": {"root": session_root.to_string_lossy()},
            },
        }));
        let response = self.next_frame();
        assert_eq!(response["id"], 1, "initialize 应答：{response}");
        assert!(
            response.get("error").is_none(),
            "initialize 失败：{response}"
        );
    }

    /// 提交回合：收到 `tool.batch` 就回「带图观察」，直到 `turn.finished`。
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
                let call = &frame["params"]["calls"][0];
                let call_id = call["id"].as_str().unwrap_or("call-1");
                let id = frame["id"].clone();
                self.send(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {"observations": [{
                        "tool_call": call,
                        "result": {"ok": true, "output": TOOL_OUTPUT, "full_output": TOOL_OUTPUT},
                        "message": {"role": "tool", "tool_call_id": call_id, "content": TOOL_OUTPUT},
                        "followup_messages": [{
                            "role": "user",
                            "content": [
                                {"type": "text", "text": "看看这张图"},
                                {"type": "image_url", "image_url": {
                                    "url": format!("data:image/png;base64,{IMAGE_DATA}"),
                                    "detail": "auto",
                                }},
                            ],
                        }],
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
}

impl Drop for Kernel {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
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
                "name": "read_image",
                "description": "读图",
                "parameters": {"type": "object", "properties": {"path": {"type": "string"}, "prompt": {"type": "string"}}}
            }
        }],
        "request_timeout_seconds": 5,
    })
}

/// 写一份启用视觉代理的 config.toml，返回路径。
fn write_config(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("omnicrawl-vision-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("建临时配置目录");
    let path = dir.join("config.toml");
    std::fs::write(
        &path,
        format!(
            "[vision]\nenabled = true\n\n[[vision.models]]\nsource = \"custom\"\nkey = \"{VISION_MODEL}\"\n"
        ),
    )
    .expect("写配置失败");
    path
}

#[test]
fn image_observation_is_routed_to_the_vision_model() {
    let server = StubServer::spawn();
    let config = write_config("enabled");
    let session_root = config.parent().expect("配置目录").join("sessions");
    let mut kernel = Kernel::spawn(&config);
    kernel.initialize(model_config(&server), &session_root);

    let frames = kernel.run_turn("看看截图");
    assert!(
        frames
            .iter()
            .any(|frame| frame["method"] == "turn.finished"),
        "回合应正常收尾：{frames:?}"
    );

    let bodies = server.bodies();
    // ① 图片交给独立视觉模型：请求带 data URL、用候选模型名，且不带工具与系统提示。
    let vision = bodies
        .iter()
        .find(|body| body["model"] == json!(VISION_MODEL))
        .expect("应当发出一次视觉模型请求");
    let vision_text = vision.to_string();
    assert!(
        vision_text.contains(&format!("data:image/png;base64,{IMAGE_DATA}")),
        "视觉请求应带图片 data URL：{vision_text}"
    );
    assert!(
        vision_text.contains("看看这张图"),
        "视觉请求应带 read_image 的 prompt：{vision_text}"
    );
    assert!(
        !vision_text.contains("你是助手。"),
        "视觉请求不应带主模型系统提示：{vision_text}"
    );

    // ② 下一轮主请求换成不可信文本观察，图片不再进上下文。
    let follow_up = bodies
        .iter()
        .rfind(|body| body["model"] == json!("main-model"))
        .expect("应当有收尾主请求");
    let follow_up_text = follow_up.to_string();
    assert!(
        follow_up_text.contains("<vision_observation>") && follow_up_text.contains(VISION_TEXT),
        "主请求应带视觉结论：{follow_up_text}"
    );
    assert!(
        !follow_up_text.contains(&format!("base64,{IMAGE_DATA}")),
        "图片不应再进主请求：{follow_up_text}"
    );

    std::fs::remove_dir_all(config.parent().expect("配置目录")).ok();
}

#[test]
fn vision_disabled_drops_the_image_for_a_non_vision_model() {
    let server = StubServer::spawn();
    // 代理关着、宿主也没声明原生视觉：图片必须被掉。
    let config = write_config("disabled");
    std::fs::write(
        &config,
        "[vision]\nenabled = false\n\n[[vision.models]]\nsource = \"custom\"\nkey = \"vision-model\"\n",
    )
    .expect("改写配置失败");
    let session_root = config.parent().expect("配置目录").join("sessions");
    let mut kernel = Kernel::spawn(&config);
    kernel.initialize(model_config(&server), &session_root);

    let frames = kernel.run_turn("看看截图");
    assert!(
        frames
            .iter()
            .any(|frame| frame["method"] == "turn.finished"),
        "回合应正常收尾：{frames:?}"
    );

    let bodies = server.bodies();
    assert!(
        !bodies
            .iter()
            .any(|body| body["model"] == json!(VISION_MODEL)),
        "未启用代理时不应调用视觉模型"
    );
    let follow_up = bodies
        .iter()
        .rfind(|body| body["model"] == json!("main-model"))
        .expect("应当有收尾主请求");
    let text = follow_up.to_string();
    // 与 Python `route_image_result` 同义：非原生视觉 + 代理未启用 → 空 followup，
    // 主模型只收到图片元数据；data URL 送进去只会被 Provider 拒掉。
    assert!(
        !text.contains(&format!("base64,{IMAGE_DATA}")),
        "未启用代理时图片不应进主请求：{follow_up}"
    );
    assert!(
        text.contains(TOOL_OUTPUT),
        "图片元数据（工具文本）仍应保留：{follow_up}"
    );

    std::fs::remove_dir_all(config.parent().expect("配置目录")).ok();
}

#[test]
fn native_vision_keeps_the_image_and_skips_the_proxy() {
    let server = StubServer::spawn();
    // 代理同样开着，但宿主声明了原生视觉：Python 的优先级是原生视觉优先于代理。
    let config = write_config("native");
    let session_root = config.parent().expect("配置目录").join("sessions");
    let mut kernel = Kernel::spawn(&config);
    let mut model = model_config(&server);
    model["native_vision"] = json!(true);
    kernel.initialize(model, &session_root);

    let frames = kernel.run_turn("看看截图");
    assert!(
        frames
            .iter()
            .any(|frame| frame["method"] == "turn.finished"),
        "回合应正常收尾：{frames:?}"
    );

    let bodies = server.bodies();
    assert!(
        !bodies
            .iter()
            .any(|body| body["model"] == json!(VISION_MODEL)),
        "原生视觉优先，不应调用视觉模型：{bodies:?}"
    );
    let follow_up = bodies
        .iter()
        .rfind(|body| body["model"] == json!("main-model"))
        .expect("应当有主请求");
    assert!(
        follow_up
            .to_string()
            .contains(&format!("base64,{IMAGE_DATA}")),
        "原生视觉时图片应直送主模型：{follow_up}"
    );

    std::fs::remove_dir_all(config.parent().expect("配置目录")).ok();
}
