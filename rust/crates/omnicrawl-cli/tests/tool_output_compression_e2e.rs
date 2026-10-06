//! 工具输出压缩旁路的端到端验收：真拉起内核，走完「模型要工具 → 宿主回超长观察 →
//! 内核调压缩模型 → 用精简文本继续对话」这条链。
//!
//! 判据全在协议帧与内核实际发出的模型请求体上：压缩请求带内置系统提示与输出包裹标记，
//! 下一轮主请求里出现的是精简文本而不是原始长输出。

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
/// 压缩渠道写在 config.toml 里的明文 key：刻意与 `OMNICRAWL_TEST_KEY` 的值不同，
/// 才能验出「压缩用的是渠道自己的 key，不是宿主为主渠道注入的那个环境变量」。
const LITERAL_KEY: &str = "literal-compression-key";
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

/// 一条 assistant 消息里的两个并发工具调用（`call-1` / `call-2`）。
fn two_tool_call_stream() -> String {
    let first = json!({"index": 0, "id": "call-1", "type": "function",
        "function": {"name": "bash", "arguments": "{\"command\":\"cat a.log\"}"}});
    let second = json!({"index": 1, "id": "call-2", "type": "function",
        "function": {"name": "bash", "arguments": "{\"command\":\"cat b.log\"}"}});
    format!(
        "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        json!({"choices": [{"delta": {"tool_calls": [first, second]}}]}),
        json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]})
    )
}

/// 回环服务端：按请求内容决定回应——压缩请求回精简文本，主模型第一次回工具调用、之后回终稿。
struct StubServer {
    addr: String,
    requests: Arc<Mutex<Vec<Value>>>,
    /// 每次请求的 `Authorization` 头，用来验证凭据确实换了渠道。
    auths: Arc<Mutex<Vec<String>>>,
    /// 压缩请求的**到达**时刻（按到达顺序）。
    compression_arrivals: Arc<Mutex<Vec<std::time::Instant>>>,
    /// 压缩响应的**写出**时刻（按写出顺序）。
    compression_responses: Arc<Mutex<Vec<std::time::Instant>>>,
}

impl StubServer {
    fn spawn() -> Self {
        Self::spawn_with(Duration::ZERO, false)
    }

    /// 两个并发工具调用 + 把**第一个**压缩响应延后 `delay`：用来验证并发压缩
    /// （第二个压缩请求必须在第一个响应之前到达，否则就是排队跑）。
    fn spawn_parallel_compressions(delay: Duration) -> Self {
        Self::spawn_with(delay, true)
    }

    fn spawn_with(delay: Duration, two_calls: bool) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("无法监听回环端口");
        let addr = listener.local_addr().expect("无法取本地地址");
        let requests: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
        let auths: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let arrivals: Arc<Mutex<Vec<std::time::Instant>>> = Arc::new(Mutex::new(Vec::new()));
        let responses: Arc<Mutex<Vec<std::time::Instant>>> = Arc::new(Mutex::new(Vec::new()));
        let main_requests: Arc<Mutex<usize>> = Arc::new(Mutex::new(0));

        // 一个连接一个线程：并发压缩要求服务端能同时接住多条连接。
        let recorded = Arc::clone(&requests);
        let recorded_auths = Arc::clone(&auths);
        let recorded_arrivals = Arc::clone(&arrivals);
        let recorded_responses = Arc::clone(&responses);
        let shared_main = Arc::clone(&main_requests);
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { break };
                let recorded = Arc::clone(&recorded);
                let recorded_auths = Arc::clone(&recorded_auths);
                let recorded_arrivals = Arc::clone(&recorded_arrivals);
                let recorded_responses = Arc::clone(&recorded_responses);
                let shared_main = Arc::clone(&shared_main);
                thread::spawn(move || {
                    let mut stream = stream;
                    let (raw, auth) = read_request(&mut stream);
                    recorded_auths.lock().expect("记录锁").push(auth);
                    let body: Value = serde_json::from_str(&raw).unwrap_or(Value::Null);
                    recorded.lock().expect("记录锁").push(body.clone());
                    // 压缩旁路的两段请求：工具调用概括与模型回复压缩（两者都走主渠道）。
                    let text_body = body.to_string();
                    let is_compression = text_body.contains("<<<TOOL_CALLS_START>>>")
                        || text_body.contains("<<<MODEL_REPLIES_START>>>");
                    let payload = if is_compression {
                        let index = {
                            let mut list = recorded_arrivals.lock().expect("记录锁");
                            list.push(std::time::Instant::now());
                            list.len()
                        };
                        // 只拖住第一条：如果内核是排队压缩，第二条请求只会在它之后才到。
                        if index == 1 && !delay.is_zero() {
                            thread::sleep(delay);
                        }
                        text_stream(COMPRESSED_TEXT)
                    } else {
                        let mut count = shared_main.lock().expect("记录锁");
                        *count += 1;
                        if *count == 1 {
                            if two_calls {
                                two_tool_call_stream()
                            } else {
                                tool_call_stream("bash", "{\"command\":\"cat big.log\"}")
                            }
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
                    if is_compression {
                        recorded_responses
                            .lock()
                            .expect("记录锁")
                            .push(std::time::Instant::now());
                    }
                });
            }
        });

        Self {
            addr: format!("http://{addr}/v1"),
            requests,
            auths,
            compression_arrivals: arrivals,
            compression_responses: responses,
        }
    }

    fn bodies(&self) -> Vec<Value> {
        self.requests.lock().expect("记录锁").clone()
    }

    fn auths(&self) -> Vec<String> {
        self.auths.lock().expect("记录锁").clone()
    }

    fn compression_arrivals(&self) -> Vec<std::time::Instant> {
        self.compression_arrivals.lock().expect("记录锁").clone()
    }

    fn compression_responses(&self) -> Vec<std::time::Instant> {
        self.compression_responses.lock().expect("记录锁").clone()
    }
}

/// 读一次请求：返回（请求体, Authorization 头）。
fn read_request(stream: &mut TcpStream) -> (String, String) {
    let mut buffer: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 1024];
    let header_end = loop {
        if let Some(index) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            break index;
        }
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => return (String::new(), String::new()),
            Ok(read) => buffer.extend_from_slice(&chunk[..read]),
        }
    };
    let head = String::from_utf8_lossy(&buffer[..header_end]).to_string();
    let auth = head
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            if name.eq_ignore_ascii_case("authorization") {
                Some(value.trim().to_string())
            } else {
                None
            }
        })
        .unwrap_or_default();
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
    (
        String::from_utf8_lossy(&buffer[body_start..end]).to_string(),
        auth,
    )
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
    fn spawn(config_path: &std::path::Path) -> Self {
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

    fn initialize(&mut self, model: Value, session: Option<Value>) {
        self.send(json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {
                "protocol_version": "1.0",
                "client": {"name": "compression-e2e"},
                "model": model,
                "session": session,
            },
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

/// 带会话根目录的模型配置：回合末的整轮概括只在**内核自持会话**时执行。
fn session_config(dir: &std::path::Path) -> Value {
    session_config_with_trigger(dir, None)
}

/// 带压缩触发阈值的会话配置：给了阈值就一定会触发（用于阈值路径的两段并发压缩）。
fn session_config_with_trigger(dir: &std::path::Path, trigger: Option<i64>) -> Value {
    let root = dir.join(".omnicrawl");
    std::fs::create_dir_all(root.join("sessions")).expect("建会话目录");
    let mut config = json!({"root": root.to_string_lossy().replace(char::from(92), "/")});
    if let Some(trigger) = trigger {
        config["compaction"] = json!({"trigger_context_tokens": trigger});
    }
    config
}

/// 写一份启用压缩的 config.toml，返回路径。
///
/// 必须带 `[llm]` 段：压缩渠道的凭据与基地址都只从配置读（配置只读 TOML 后，环境变量
/// 不再参与凭据解析）。没有可解析的 `[llm]` 时压缩器拿不到凭据，会静默跳过压缩，
/// 于是这些用例收不到任何压缩请求。
///
/// `base_url` 传回环桩地址：裸 `model_key` 只换模型名、沿用 Profile 的连接，
/// 压缩请求因此会打到桩服务上。
fn write_config(name: &str, base_url: &str, min_chars: usize) -> PathBuf {
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
             api_key_env = \"OMNICRAWL_TEST_KEY\"\n\
             api_key = \"{LITERAL_KEY}\"\n\
             [tool_output_compression]\nenabled = true\nmodel_key = \"compress-model\"\n\
             thinking_enabled = false\nreasoning_effort = \"high\"\nmin_chars = {min_chars}\n\
             max_input_chars = 4000\nmax_output_chars = 400\ntimeout_seconds = 5\n"
        ),
    )
    .expect("写配置失败");
    path
}

/// 用户要求：多个工具调用**同时**发多个压缩请求，而不是一个压完才开始下一个。
#[test]
fn both_compactions_run_at_the_same_time() {
    // 回合末的两段压缩必须并发：工具调用压缩的响应被拖住时，
    // 模型回复压缩的请求仍应先到达（否则就是排队执行）。
    // 阈值必然触发：两段压缩（工具调用 + 模型回复）在回合末并发发起。
    let server = StubServer::spawn_parallel_compressions(Duration::from_millis(900));
    let config = write_config("parallel", &server.addr, 100);
    let mut kernel = Kernel::spawn(&config);
    kernel.initialize(
        model_config(&server),
        Some(session_config_with_trigger(
            config.parent().expect("配置目录"),
            Some(1),
        )),
    );

    let long_output = "日志行内容".repeat(200);
    kernel.send(json!({
        "jsonrpc": "2.0", "id": 2, "method": "turn.submit",
        "params": {"turn_id": "turn-1", "user_text": "看两个日志"},
    }));
    loop {
        let frame = kernel.next_frame();
        if frame["method"] == "tool.batch" {
            let params = frame["params"].clone();
            let id = frame["id"].clone();
            let observations: Vec<Value> = params["calls"]
                .as_array()
                .cloned()
                .unwrap_or_default()
                .iter()
                .map(|call| {
                    json!({
                        "tool_call": call,
                        "result": {"ok": true, "output": long_output, "full_output": long_output},
                        "message": {"role": "tool", "tool_call_id": call["id"], "content": long_output},
                        "followup_messages": [],
                    })
                })
                .collect();
            assert_eq!(observations.len(), 2, "一条消息里的两个工具调用打一个批次");
            kernel.send(json!({"jsonrpc": "2.0", "id": id, "result": {"observations": observations}}));
            continue;
        }
        if frame["method"] == "turn.finished" {
            break;
        }
    }

    let arrivals = server.compression_arrivals();
    let responses = server.compression_responses();
    assert!(
        arrivals.len() >= 2 && responses.len() >= 2,
        "两段压缩各发一次请求：到达 {arrivals:?} 写出 {responses:?}"
    );
    assert!(
        arrivals[1] < responses[0],
        "两次压缩必须并发：到达 {arrivals:?} 写出 {responses:?}"
    );
}

#[test]
fn tool_calls_are_summarized_once_after_the_turn() {
    // 新粒度：批内不再逐条压缩，整轮结束把全部工具调用压成一段注入上下文。
    let server = StubServer::spawn();
    let config = write_config("enabled", &server.addr, 100);
    let mut kernel = Kernel::spawn(&config);
    kernel.initialize(model_config(&server), Some(session_config(config.parent().expect("配置目录"))));

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
        .find(|body| body.to_string().contains("<<<TOOL_CALLS_START>>>"))
        .expect("应当发出一次整轮概括请求");
    let text = compression.to_string();
    assert!(
        text.contains("请把下面这一轮的全部工具调用概括成一段内容。"),
        "概括请求要带任务背景：{text}"
    );

    // 会话转录里落下概括事件，投影据此剔除逐条工具事件。
    let events = read_session_events(&config);
    let summary = events
        .iter()
        .find(|event| event["type"] == "tool_call_summary")
        .unwrap_or_else(|| panic!("应当写入 tool_call_summary 事件：{events:?}"));
    // 计量随事件落盘：宿主回放（/resume、/undo）据此重建同一行，不必重算概括正文。
    let raw_chars = summary["payload"]["raw_chars"]
        .as_u64()
        .expect("事件要带原始字符数");
    let summary_chars = summary["payload"]["summary_chars"]
        .as_u64()
        .expect("事件要带概括字符数");
    assert!(
        summary_chars > 0 && raw_chars > summary_chars,
        "概括应当确实压小：{raw_chars} → {summary_chars}"
    );

    // 会话区提示与上下文压缩同款：一行「已压缩 a → b 字符」，不是状态行。
    let notice = frames
        .iter()
        .find(|frame| frame["method"] == "turn.notice")
        .unwrap_or_else(|| panic!("应当发一条会话区提示：{frames:?}"));
    assert_eq!(
        notice["params"]["message"],
        json!(format!(
            "已压缩 {} → {} 字符",
            group_digits(raw_chars),
            group_digits(summary_chars)
        )),
        "提示文案要与事件载荷的计量一致"
    );

    std::fs::remove_dir_all(config.parent().expect("配置目录")).ok();
}

#[test]
fn tool_compaction_rebuilds_history_and_reports_the_new_context_size() {
    // 概括不只是「转录里看起来被压缩了」：它必须重建运行期历史，让下一轮请求真的带概括文本；
    // 同时把压缩后的上下文大小随 `turn.finished` 报给宿主刷新遥测。
    let server = StubServer::spawn();
    let config = write_config("history", &server.addr, 100);
    let mut kernel = Kernel::spawn(&config);
    kernel.initialize(
        model_config(&server),
        Some(session_config(config.parent().expect("配置目录"))),
    );

    let long_output = "日志行内容".repeat(200);
    let frames = kernel.run_turn("看一下日志", &long_output);
    let finished = frames
        .iter()
        .find(|frame| frame["method"] == "turn.finished")
        .unwrap_or_else(|| panic!("回合应正常收尾：{frames:?}"));

    let reported = finished["params"]["post_compaction_context_tokens"]
        .as_i64()
        .unwrap_or_else(|| panic!("回合末工具压缩要报压缩后大小：{finished}"));
    assert!(reported > 0, "压缩后上下文大小应为正数：{reported}");

    // 覆盖事件已落盘：被概括的逐条工具事件不再进入下一轮请求。
    let events = read_session_events(&config);
    assert!(
        events
            .iter()
            .any(|event| event["type"] == "tool_call_summary"),
        "应当写入概括事件：{events:?}"
    );

    // 历史确实被重建：再发一轮，请求体里带的是概括文本，不是 200 份原始长输出。
    let bodies_before = server.bodies().len();
    kernel.run_turn("接着看", &long_output);
    let bodies = server.bodies();
    let followup = bodies[bodies_before..]
        .iter()
        .find(|body| !body.to_string().contains("<<<TOOL_CALLS_START>>>"))
        .unwrap_or_else(|| panic!("应当发出下一轮主请求"));
    let text = followup.to_string();
    assert!(
        text.contains(COMPRESSED_TEXT),
        "下一轮请求应带整轮概括文本：{text}"
    );
    assert!(
        !text.contains(&long_output),
        "被概括的原始长输出不得再进入请求：{} 字符的原文仍在请求体里",
        long_output.len()
    );

    std::fs::remove_dir_all(config.parent().expect("配置目录")).ok();
}

/// 千分位分组：契约是「用户读得到的字符数」，因此断言侧独立算一遍，不复用实现的函数。
fn group_digits(value: u64) -> String {
    let digits = value.to_string();
    let mut out = String::new();
    for (index, ch) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// 读最近一条会话转录的事件类型与载荷。
fn read_session_events(config: &std::path::Path) -> Vec<Value> {
    let root = config.parent().expect("配置目录").join(".omnicrawl").join("sessions");
    let mut found: Vec<Value> = Vec::new();
    let Ok(entries) = std::fs::read_dir(&root) else {
        return found;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("jsonl") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        for line in text.lines() {
            if let Ok(value) = serde_json::from_str::<Value>(line) {
                found.push(value);
            }
        }
    }
    found
}

#[test]
fn compression_model_key_is_resolved_through_the_profile_connection() {
    // 回归：`model_key` 形如 `profile/model_id` 时，要按模型切换同一套口径解析出
    // 真正的 model 名与连接（以前把整个 key 当模型名发给上游，上游会回
    // 「模型不存在或当前账号无权使用该模型」）。
    let server = StubServer::spawn();
    let config = write_config_with_profile("profile-key", &server.addr, 100);
    let mut kernel = Kernel::spawn(&config);
    kernel.initialize(model_config(&server), Some(session_config(config.parent().expect("配置目录"))));

    let long_output = "日志行内容".repeat(200);
    kernel.run_turn("看一下日志", &long_output);

    let bodies = server.bodies();
    let compression = bodies
        .iter()
        .find(|body| body.to_string().contains("<<<TOOL_CALLS_START>>>"))
        .expect("应当发出一次整轮概括请求");
    assert_eq!(
        compression["model"].as_str(),
        Some("sub-model-x"),
        "压缩请求应带解析后的模型名，而不是 `stub/sub-model-x` 这个 key：{compression}"
    );
    let auths = server.auths();
    let index = bodies
        .iter()
        .position(|body| body.to_string().contains("<<<TOOL_CALLS_START>>>"))
        .expect("概括请求下标");
    assert_eq!(
        auths[index],
        format!("Bearer {LITERAL_KEY}"),
        "压缩请求必须用渠道明文 key：{auths:?}"
    );
    assert_eq!(
        auths[0],
        format!("Bearer {TEST_KEY}"),
        "主渠道请求仍走帧里声明的环境变量：{auths:?}"
    );
    std::fs::remove_dir_all(config.parent().expect("配置目录")).ok();
}

/// 写一份「有 [llm] Profile，压缩选择用 `profile/model_id` 形式」的 config.toml。
fn write_config_with_profile(name: &str, base_url: &str, min_chars: usize) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("omnicrawl-compress-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("建临时配置目录");
    let path = dir.join("config.toml");
    let literal = format!("api_key = \"{LITERAL_KEY}\"");
    std::fs::write(
        &path,
        format!(
            "version = 2
             [llm]
             [llm.active_model]
             source = \"detected\"
             profile = \"stub\"
             model_id = \"main-model\"
             protocol = \"openai_chat_completions\"
             [llm.profiles.stub]
             provider = \"openai\"
             base_url = \"{base_url}\"
             api_key_env = \"OMNICRAWL_TEST_KEY\"
             {literal}
             [tool_output_compression]
enabled = true
model_key = \"stub/sub-model-x\"
             thinking_enabled = false
reasoning_effort = \"high\"
min_chars = {min_chars}
             max_input_chars = 4000
max_output_chars = 400
timeout_seconds = 5
"
        ),
    )
    .expect("写配置失败");
    path
}

#[test]
fn every_tool_call_participates_regardless_of_length() {
    // 新粒度不再按工具名与长度筛选：短输出同样进整轮概括，
    // 概括粒度由模型决定，长度由 `max_output_chars` 兜底。
    let server = StubServer::spawn();
    let config = write_config("short", &server.addr, 100_000);
    let mut kernel = Kernel::spawn(&config);
    kernel.initialize(
        model_config(&server),
        Some(session_config(config.parent().expect("配置目录"))),
    );

    let frames = kernel.run_turn("看一下日志", "短的输出");
    assert!(frames
        .iter()
        .any(|frame| frame["method"] == "turn.finished"));

    let bodies = server.bodies();
    assert!(
        bodies
            .iter()
            .any(|body| body.to_string().contains("<<<TOOL_CALLS_START>>>")),
        "短输出同样参与整轮概括：{bodies:?}"
    );

    std::fs::remove_dir_all(config.parent().expect("配置目录")).ok();
}
