//! 内核侧会话与压缩接线的端到端测试：真拉起 `omnicrawl` 进程。
//!
//! 三件事必须成立：
//! 1. 宿主给了会话配置后，内核自己持有转录，回合结束把用户消息与最终回复落盘；
//! 2. 回合结束后内核按阈值跑一次压缩：摘要请求复用主请求前缀、带工具面、`tool_choice=none`；
//! 3. 下一轮请求带上从转录恢复的历史，而不是「只有当前一句」。

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use omnicrawl_session::{utc_now, SessionStore};
use serde_json::{json, Value};

const WAIT: Duration = Duration::from_secs(15);
const TEST_KEY: &str = "test-key";

fn make_stream(text: &str) -> String {
    // 用 serde_json 拼 SSE：摘要正文里带引号，手工拼字符串会产出非法 JSON。
    let delta = json!({"choices": [{"delta": {"content": text}}]}).to_string();
    let finish = json!({"choices": [{"delta": {}, "finish_reason": "stop"}]}).to_string();
    format!(
        "data: {delta}

data: {finish}

data: [DONE]

"
    )
}

/// 桩服务端的第 n 次响应：固定文本，或按请求里的 `events_index` 合成一份合法摘要。
#[derive(Clone)]
enum Reply {
    Text(String),
    Summary,
    /// 上游错误响应：正文里带上下文超限标记，内核据此走恢复路径。
    UpstreamError(String),
    /// 直接给一段原始 SSE（工具调用等）。
    Raw(String),
}

/// 本机回环服务端：按第几次请求回不同的 SSE，并记录收到的请求体。
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
                let value = serde_json::from_str::<Value>(&raw).unwrap_or(Value::Null);
                if !value.is_null() {
                    recorded.lock().expect("记录锁").push(value.clone());
                }
                let reply = script
                    .get(index.min(script.len().saturating_sub(1)))
                    .cloned()
                    .unwrap_or(Reply::Text(String::new()));
                let value = serde_json::from_str::<Value>(&raw).unwrap_or(Value::Null);
                if let Reply::UpstreamError(body) = reply {
                    let head = format!(
                        "HTTP/1.1 400 Bad Request
Content-Type: application/json
Content-Length: {}
Connection: close

",
                        body.len()
                    );
                    let _ = stream.write_all(head.as_bytes());
                    let _ = stream.write_all(body.as_bytes());
                    let _ = stream.flush();
                    continue;
                }
                let body = match reply {
                    Reply::Text(text) => text,
                    Reply::Raw(text) => text,
                    Reply::Summary => {
                        let summary = summary_from_request(&value).unwrap_or_default();
                        make_stream(&summary)
                    }
                    Reply::UpstreamError(_) => String::new(),
                };
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

/// 摘要请求的提示词里带着 `events_index`（event_id / type / preview）；
/// 这里按它合成一份通过校验的结构化摘要：用户消息逐字覆盖，其余字段引用真实事件。
fn summary_from_request(body: &Value) -> Option<String> {
    let prompt = body["messages"]
        .as_array()?
        .last()?
        .get("content")?
        .as_str()?;
    let payload_text = prompt.split("输入：").nth(1)?.trim();
    let payload: Value = serde_json::from_str(payload_text).ok()?;
    let index = payload["events_index"].as_array()?;

    let mut user_items: Vec<Value> = Vec::new();
    let mut other_items: Vec<Value> = Vec::new();
    for entry in index {
        let id = entry["event_id"].as_str()?.to_string();
        let kind = entry["type"].as_str()?.to_string();
        let preview = entry["preview"].as_str()?;
        let text = serde_json::from_str::<Value>(preview)
            .ok()
            .and_then(|value| {
                value
                    .get("content")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .unwrap_or_else(|| "已完成".to_string());
        let item = json!({"text": text, "source_event_ids": [id]});
        if kind == "user_message" {
            user_items.push(item);
        } else {
            other_items.push(item);
        }
    }
    if user_items.is_empty() {
        return None;
    }
    let summary = json!({
        "objective": ["推进当前任务"],
        "current_state": ["正在压缩上下文"],
        "constraints": other_items.clone(),
        "decisions": other_items.clone(),
        "completed": other_items.clone(),
        "open_issues": other_items.clone(),
        "artifacts": other_items.clone(),
        "exact_evidence": other_items.clone(),
        "failed_attempts": other_items.clone(),
        "excluded_approaches": other_items.clone(),
        "key_concepts": other_items.clone(),
        "problem_solving_process": other_items.clone(),
        "user_messages": user_items,
        "next_steps": other_items,
    });
    serde_json::to_string(&summary).ok()
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
    next_id: i64,
}

impl Kernel {
    fn spawn() -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_omnicrawl"))
            .env("OMNICRAWL_TEST_KEY", TEST_KEY)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
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
            next_id: 0,
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

    fn initialize(&mut self, params: Value) {
        self.next_id += 1;
        self.send(json!({
            "jsonrpc": "2.0", "id": self.next_id, "method": "initialize", "params": params,
        }));
        let response = self.next_frame();
        assert_eq!(response["id"], self.next_id, "initialize 应答：{response}");
        assert_eq!(response["result"]["protocol_version"], "1.0");
    }

    /// 提交一个回合：返回起始到 `turn.finished` 之间的所有帧。
    fn submit_turn(&mut self, user_text: &str) -> Vec<Value> {
        self.next_id += 1;
        let request_id = self.next_id;
        self.send(json!({
            "jsonrpc": "2.0", "id": request_id, "method": "turn.submit",
            "params": {"turn_id": format!("turn-{request_id}"), "user_text": user_text},
        }));

        // 回合收尾（会话落盘 + 压缩）在 turn.finished 之后跑，因此要等 turn.submit 的应答：
        // 它一定在收尾之后发出，拿到它就说明转录已经写完。
        let mut collected = Vec::new();
        let mut finished = false;
        loop {
            let frame = self.next_frame();
            let method = frame["method"].as_str().unwrap_or_default().to_string();
            finished |= method == "turn.finished";
            let is_response = method.is_empty() && frame["id"].as_i64() == Some(request_id);
            collected.push(frame);
            if is_response {
                return collected;
            }
            let _ = finished;
        }
    }
}

impl Drop for Kernel {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn temp_root(tag: &str) -> PathBuf {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("时钟可用")
        .as_nanos();
    let path = std::env::temp_dir().join(format!("omnicrawl-cli-compaction-{tag}-{unique}"));
    std::fs::create_dir_all(&path).expect("建临时目录");
    path
}

fn transcript(root: &Path, session_id: &str) -> Vec<Value> {
    let path = root.join("sessions").join(format!("{session_id}.jsonl"));
    let text = std::fs::read_to_string(&path).expect("转录可读");
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str::<Value>(line).expect("转录行是 JSON"))
        .collect()
}

/// 从一回合的帧里取 `turn.finished` 的最终回复。
fn finished_text(frames: &[Value]) -> &str {
    frames
        .iter()
        .find(|frame| frame["method"] == "turn.finished")
        .and_then(|frame| frame["params"]["final_text"].as_str())
        .expect("有 turn.finished 帧")
}

fn methods(frames: &[Value]) -> Vec<String> {
    frames
        .iter()
        .map(|frame| frame["method"].as_str().unwrap_or_default().to_string())
        .collect()
}

#[test]
fn kernel_owns_the_session_and_compacts_after_turn() {
    let server = StubServer::spawn(vec![
        Reply::Text(make_stream("第一轮回复")),
        // 摘要响应按请求里的 events_index 合成：内核必须能通过校验并真的压缩。
        Reply::Summary,
        Reply::Text(make_stream("第二轮回复")),
        Reply::Summary,
    ]);

    let root = temp_root("session");
    let store = SessionStore::open(&root);
    store.ensure().expect("建目录");
    let created = store
        .start_session("/workspace", "历史会话", utc_now())
        .expect("建会话");
    let session_id = created.session_id.clone();
    store
        .append_event(
            &session_id,
            "user_message",
            payload(json!({"content": "历史用户"})),
            None,
            utc_now(),
        )
        .expect("追加历史用户消息");
    store
        .append_event(
            &session_id,
            "assistant_message",
            payload(json!({"content": "历史回复"})),
            None,
            utc_now(),
        )
        .expect("追加历史回复");

    let mut kernel = Kernel::spawn();
    kernel.initialize(json!({
        "protocol_version": "1.0",
        "client": {"name": "e2e"},
        "model": {
            "model": "e2e-model",
            "base_url": server.addr,
            "api_key_env": "OMNICRAWL_TEST_KEY",
            "user_agent": "omnicrawl-e2e",
            "system_prompt": "你是助手。",
            "tools": [{
                "type": "function",
                "function": {"name": "read_file", "description": "读文件", "parameters": {"type": "object"}}
            }],
            "request_timeout_seconds": 10,
        },
        "session": {
            "root": root.to_string_lossy().replace(char::from(92), "/"),
            "session_id": session_id,
            "compaction": {
                "trigger_context_tokens": 1,
                "preserve_exact_evidence": false,
            },
        },
    }));

    let frames = kernel.submit_turn("你好");
    assert_eq!(finished_text(&frames), "第一轮回复");
    assert!(
        !methods(&frames)
            .iter()
            .any(|method| method == "model.reply"),
        "内核自己发请求时不应出现 model.reply"
    );

    // 转录：本回合的用户消息与最终回复已落盘，压缩尝试也留了痕。
    let events = transcript(&root, &session_id);
    let types: Vec<&str> = events
        .iter()
        .map(|event| event["type"].as_str().unwrap_or_default())
        .collect();
    assert!(types.contains(&"user_message"), "用户消息落盘：{types:?}");
    assert!(
        types.contains(&"assistant_message"),
        "最终回复落盘：{types:?}"
    );
    assert!(
        types.contains(&"context_compaction_measurement"),
        "回合结束的测量事件落盘：{types:?}"
    );
    assert!(
        types.contains(&"compact_summary"),
        "摘要通过校验后写入压缩摘要：{types:?}"
    );
    assert!(
        !types.contains(&"context_compaction_failed"),
        "不该出现失败事件：{types:?}"
    );
    // 压缩边界提示经协议外发，供 TUI 单独成行渲染。
    let notices: Vec<String> = frames
        .iter()
        .filter_map(|frame| frame["params"]["message"].as_str())
        .map(str::to_string)
        .collect();
    assert!(
        notices.iter().any(|notice| notice.contains("已压缩")),
        "回合内应出现压缩提示：{notices:?}"
    );
    // 触发压缩的回合把计量发给宿主（宿主据此分发 `context.compaction.after_turn`）。
    let compaction = frames
        .iter()
        .find(|frame| frame["method"] == "turn.context_compaction")
        .expect("触发压缩的回合应有压缩计量通知");
    assert!(
        compaction["params"]["post_turn_context_tokens"]
            .as_i64()
            .unwrap_or(0)
            > 0,
        "计量应带上压缩前的实际上下文 Token：{compaction}"
    );
    assert_eq!(
        compaction["params"]["trigger_context_tokens"].as_i64(),
        Some(1),
        "计量应带上触发阈值：{compaction}"
    );
    assert!(
        !compaction["params"]["turn_id"]
            .as_str()
            .unwrap_or_default()
            .is_empty(),
        "计量应带上回合 id：{compaction}"
    );

    // 摘要请求：复用主请求前缀（历史 + 本轮用户消息），带工具面，且禁止调用工具。
    let bodies = server.bodies();
    assert!(
        bodies.len() >= 2,
        "本轮对话加一次摘要请求：{}",
        bodies.len()
    );
    let summary = &bodies[1];
    assert_eq!(summary["tools"][0]["function"]["name"], "read_file");
    assert_eq!(summary["tool_choice"], "none", "摘要请求禁止调用工具");
    let summary_messages = summary["messages"].as_array().expect("消息数组");
    let summary_texts: Vec<&str> = summary_messages
        .iter()
        .filter_map(|message| message["content"].as_str())
        .collect();
    assert!(
        summary_texts.contains(&"历史用户")
            && summary_texts.contains(&"历史回复")
            && summary_texts.contains(&"你好"),
        "摘要请求复用主请求前缀：{summary_texts:?}"
    );
    assert!(
        summary_texts
            .last()
            .map(|text| text.contains("会话状态压缩器"))
            .unwrap_or(false),
        "最后一条是摘要提示词：{summary_texts:?}"
    );

    // 第二轮：请求用的是压缩后的历史（摘要 + 最终回复锚点 + 当前输入），
    // 已被摘要取代的旧消息不再进上下文——这正是「压缩即丢弃」的可见证据。
    let second = kernel.submit_turn("第二问");
    assert_eq!(finished_text(&second), "第二轮回复");
    let bodies = server.bodies();
    let turn_request = bodies
        .iter()
        .find(|body| {
            body["messages"]
                .as_array()
                .and_then(|messages| messages.last())
                .and_then(|message| message["content"].as_str())
                == Some("第二问")
        })
        .expect("第二轮的主请求体");
    let messages = turn_request["messages"].as_array().expect("消息数组");
    let texts: Vec<&str> = messages
        .iter()
        .filter_map(|message| message["content"].as_str())
        .collect();
    assert!(
        texts.iter().any(|text| text.starts_with("会话压缩摘要")),
        "压缩后的历史里带着摘要：{texts:?}"
    );
    assert!(
        texts.contains(&"第一轮回复") && texts.contains(&"第二问"),
        "摘要锚点与当前输入都在：{texts:?}"
    );
    assert!(
        !texts.contains(&"历史用户"),
        "已被摘要取代的旧消息不再进上下文：{texts:?}"
    );

    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn kernel_recovers_from_context_overflow() {
    let overflow = json!({
        "error": {
            "message": "This model's maximum context length is 8192 tokens",
            "type": "invalid_request_error"
        }
    })
    .to_string();
    let server = StubServer::spawn(vec![
        Reply::UpstreamError(overflow),
        // 恢复用的摘要请求：按 events_index 合成合法摘要。
        Reply::Summary,
        Reply::Text(make_stream("恢复后的回复")),
    ]);

    let root = temp_root("overflow");
    let store = SessionStore::open(&root);
    store.ensure().expect("建目录");
    let created = store
        .start_session("/workspace", "未完成回合", utc_now())
        .expect("建会话");
    let session_id = created.session_id.clone();
    // 转录以一条用户消息结尾：这正是「被上下文超限中断的未完成回合」。
    store
        .append_event(
            &session_id,
            "user_message",
            payload(json!({"content": "超限前的问题"})),
            None,
            utc_now(),
        )
        .expect("追加未完成回合");

    let mut kernel = Kernel::spawn();
    kernel.initialize(json!({
        "protocol_version": "1.0",
        "client": {"name": "e2e"},
        "model": {
            "model": "e2e-model",
            "base_url": server.addr,
            "api_key_env": "OMNICRAWL_TEST_KEY",
            "system_prompt": "你是助手。",
            "tools": [],
            "request_timeout_seconds": 10,
        },
        "session": {
            "root": root.to_string_lossy().replace(char::from(92), "/"),
            "session_id": session_id,
            "compaction": {
                "target_summary_tokens": 2000,
                "preserve_exact_evidence": false,
            },
        },
    }));

    let frames = kernel.submit_turn("超限后的输入");
    assert_eq!(finished_text(&frames), "恢复后的回复", "超限后自动续接");

    let notices: Vec<String> = frames
        .iter()
        .filter_map(|frame| frame["params"]["message"].as_str())
        .map(str::to_string)
        .collect();
    assert!(
        notices.iter().any(|notice| notice.contains("上下文超限")),
        "给出超限恢复提示：{notices:?}"
    );

    let events = transcript(&root, &session_id);
    let types: Vec<&str> = events
        .iter()
        .map(|event| event["type"].as_str().unwrap_or_default())
        .collect();
    assert!(
        types.contains(&"compact_summary"),
        "恢复摘要落盘：{types:?}"
    );
    assert!(
        types.contains(&"context_overflow_recovery"),
        "恢复事件落盘：{types:?}"
    );
    assert!(
        events.iter().any(|event| {
            event["type"] == "user_message"
                && event["payload"]["content"] == "请依据上方的结构化工作摘要继续完成当前任务。"
        }),
        "续接指令写进会话：{types:?}"
    );

    let bodies = server.bodies();
    assert!(bodies.len() >= 3, "超限请求 + 摘要请求 + 重试请求");
    let retry = bodies.last().expect("重试请求体");
    let texts: Vec<&str> = retry["messages"]
        .as_array()
        .expect("消息数组")
        .iter()
        .filter_map(|message| message["content"].as_str())
        .collect();
    assert!(
        texts
            .iter()
            .any(|text| text.contains("请依据上方的结构化工作摘要继续完成当前任务。")),
        "重试请求带续接指令：{texts:?}"
    );

    std::fs::remove_dir_all(&root).ok();
}

/// 一段请求工具调用的 SSE：参数一次性给全，随后以 `tool_calls` 收尾。
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
    format!(
        "data: {delta}

data: {finish}

data: [DONE]

"
    )
}

#[test]
fn kernel_answers_evidence_tool_without_the_host() {
    let root = temp_root("evidence-tool");
    let store = SessionStore::open(&root);
    store.ensure().expect("建目录");
    let created = store
        .start_session("/workspace", "证据恢复", utc_now())
        .expect("建会话");
    let session_id = created.session_id.clone();
    // 被摘要覆盖的原始事件 + 授权它的摘要：证据恢复只认最后一个有效摘要。
    let original = store
        .append_event(
            &session_id,
            "user_message",
            payload(json!({"content": "被压缩的原始问题"})),
            None,
            utc_now(),
        )
        .expect("追加原始事件");
    store
        .append_event(
            &session_id,
            "compact_summary",
            payload(json!({
                "schema_version": 2,
                "content": "会话压缩摘要",
                "structured": {"objective": ["目标"]},
                "covered_event_ids": [original.event_id.clone()],
                "compacted_event_ids": [original.event_id.clone()],
                "remaining_event_ids": [],
            })),
            None,
            utc_now(),
        )
        .expect("追加压缩摘要");

    let arguments = json!({"event_ids": [original.event_id.clone()]}).to_string();
    let server = StubServer::spawn(vec![
        Reply::Raw(tool_call_stream("recall_session_evidence", &arguments)),
        Reply::Text(make_stream("工具已答复")),
    ]);

    let mut kernel = Kernel::spawn();
    kernel.initialize(json!({
        "protocol_version": "1.0",
        "client": {"name": "e2e"},
        "model": {
            "model": "e2e-model",
            "base_url": server.addr,
            "api_key_env": "OMNICRAWL_TEST_KEY",
            "system_prompt": "你是助手。",
            "tools": [{
                "type": "function",
                "function": {"name": "recall_session_evidence", "description": "恢复证据", "parameters": {"type": "object"}}
            }],
            "request_timeout_seconds": 10,
        },
        "session": {
            "root": root.to_string_lossy().replace(char::from(92), "/"),
            "session_id": session_id,
            "compaction": {"trigger_context_tokens": 1000000},
        },
    }));

    let frames = kernel.submit_turn("请恢复证据");
    assert_eq!(finished_text(&frames), "工具已答复");
    assert!(
        !methods(&frames).iter().any(|method| method == "tool.batch"),
        "只读会话的工具应由内核本地作答，不该占用宿主的批次：{:?}",
        methods(&frames)
    );
    // 未触发压缩的回合不发泄量：宿主也就不会分发压缩 Hook。
    assert!(
        !methods(&frames)
            .iter()
            .any(|method| method == "turn.context_compaction"),
        "未触发压缩的回合不该发压缩计量：{:?}",
        methods(&frames)
    );

    // 第二轮模型请求里带着本地生成的工具结果。
    let bodies = server.bodies();
    let followup = bodies.last().expect("工具结果后的模型请求");
    let tool_content = followup["messages"]
        .as_array()
        .expect("消息数组")
        .iter()
        .find(|message| message["role"] == "tool")
        .and_then(|message| message["content"].as_str())
        .unwrap_or_default()
        .to_string();
    assert!(
        tool_content.contains("被压缩的原始问题"),
        "工具结果带上了被压缩事件的原文：{tool_content}"
    );
    assert!(
        tool_content.contains("\"ok\":true") || tool_content.contains("ok: true"),
        "工具结果信封标了成功：{tool_content}"
    );

    std::fs::remove_dir_all(&root).ok();
}

fn payload(value: Value) -> serde_json::Map<String, Value> {
    value.as_object().cloned().expect("载荷是 JSON 对象")
}
