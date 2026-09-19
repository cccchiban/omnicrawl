//! 内核「恢复会话」的端到端验收：同一个会话根、同一个 `session_id` 两进两出，
//! 第二轮必须看见第一轮的历史。
//!
//! 结论此前只有实现（`KernelSession::open` 带 ID 时 `reload_history`），没有 e2e 证明。
//! 协议 v1 的 `initialize.session` 语义是：**不给 `session_id` 才新建**（新建后 ID 只经 stderr
//! 报出，帧里没有），**给了就是恢复且该会话必须已存在**。
//!
//! 判据落在内核发往宿主的 `model.reply` 请求上——它的 `params.messages` 就是内核持有的运行期历史，
//! 比外接假 provider 更直接，也不需要网络与凭据。

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::Duration;

use serde_json::{json, Value};

const WAIT: Duration = Duration::from_secs(15);
const FIRST_TEXT: &str = "第一轮的问题";
const FIRST_REPLY: &str = "第一轮的回答";
const SECOND_TEXT: &str = "第二轮的问题";
const SECOND_REPLY: &str = "第二轮的回答";

/// 被拉起的内核进程：写帧、读帧、读日志、退出时收尸。
struct Kernel {
    child: Child,
    stdin: ChildStdin,
    frames: Receiver<Value>,
    logs: Receiver<String>,
}

impl Kernel {
    fn spawn() -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_omnicrawl"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("无法拉起内核进程");

        let stdin = child.stdin.take().expect("内核 stdin 可用");
        let stdout = child.stdout.take().expect("内核 stdout 可用");
        let stderr = child.stderr.take().expect("内核 stderr 可用");

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

        let (log_sender, logs) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stderr).lines() {
                let Ok(line) = line else { break };
                if log_sender.send(line).is_err() {
                    break;
                }
            }
        });

        Self {
            child,
            stdin,
            frames,
            logs,
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

    /// 等内核在 stderr 上报出新建会话的 ID。
    fn session_id(&self) -> String {
        for _ in 0..20 {
            match self.logs.recv_timeout(WAIT) {
                Ok(line) => {
                    if let Some((_, id)) = line.split_once("会话已就绪：") {
                        return id.trim().to_string();
                    }
                }
                Err(_) => break,
            }
        }
        panic!("没等到内核报出会话 ID");
    }

    fn initialize(&mut self, session: Value) {
        self.send(json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocol_version": "1.0",
                "client": {"name": "resume-e2e"},
                "session": session,
            },
        }));
        let response = self.next_frame();
        assert_eq!(response["id"], 1, "initialize 应答：{response}");
        assert!(
            response.get("error").is_none(),
            "initialize 失败：{response}"
        );
        assert_eq!(response["result"]["protocol_version"], "1.0");
    }

    /// 优雅关停：内核在这里收尾落盘（强杀会丢当前进程尚未写出的会话数据）。
    fn shutdown(&mut self) {
        self.send(json!({
            "jsonrpc": "2.0",
            "id": 9,
            "method": "shutdown",
            "params": {},
        }));
        let _ = self.child.wait();
    }

    /// 提交回合并用给定文本代答；返回途中所有帧与该次 `model.reply` 请求里的 messages。
    fn run_turn(&mut self, user_text: &str, reply_text: &str) -> (Vec<Value>, Vec<Value>) {
        self.send(json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "turn.submit",
            "params": {"turn_id": "turn-1", "user_text": user_text},
        }));

        let mut collected = Vec::new();
        let mut seen: Vec<Value> = Vec::new();
        loop {
            let frame = self.next_frame();
            let method = frame["method"].as_str().unwrap_or_default().to_string();
            if method == "model.reply" {
                seen = frame["params"]["messages"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default();
                let id = frame["id"].clone();
                self.send(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "message": {"role": "assistant", "content": reply_text},
                        "content": reply_text,
                        "tool_calls": [],
                        "reasoning": "",
                        "content_streamed": false,
                    },
                }));
                collected.push(frame);
                continue;
            }
            let finished = method == "turn.finished";
            collected.push(frame);
            if finished {
                return (collected, seen);
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

/// 会话根：两个内核进程共用同一个目录，才有「恢复」可言。
///
/// 名字里必须带用例名——`cargo test` 默认并行跑，同名目录会互相 `remove_dir_all`。
fn temp_root(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("omnicrawl-resume-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("无法建临时会话根");
    root
}

fn root_param(root: &Path) -> String {
    root.to_string_lossy().to_string()
}

fn pairs(messages: &[Value]) -> Vec<(String, String)> {
    messages
        .iter()
        .map(|message| {
            (
                message["role"].as_str().unwrap_or_default().to_string(),
                message["content"].as_str().unwrap_or_default().to_string(),
            )
        })
        .collect()
}

fn final_text(frames: &[Value]) -> String {
    let finished = frames.last().expect("至少有一个 turn.finished 帧");
    assert_eq!(finished["method"], "turn.finished");
    finished["params"]["final_text"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

#[test]
fn kernel_reloads_session_history_after_restart() {
    let root = temp_root("resume");

    // 第一进：不给 session_id → 内核新建并报出 ID；跑一轮后进程退出（转录落盘）。
    let session_id = {
        let mut kernel = Kernel::spawn();
        kernel.initialize(json!({"root": root_param(&root)}));
        let session_id = kernel.session_id();
        assert!(!session_id.is_empty(), "内核应报出会话 ID");

        let (frames, messages) = kernel.run_turn(FIRST_TEXT, FIRST_REPLY);
        assert_eq!(final_text(&frames), FIRST_REPLY);
        assert_eq!(
            pairs(&messages),
            vec![("user".to_string(), FIRST_TEXT.to_string())],
            "首轮不该凭空多出历史"
        );

        // 阶段 A 必须留下转录，否则「恢复」无从谈起。
        kernel.shutdown();
        let transcript = root.join("sessions").join(format!("{session_id}.jsonl"));
        assert!(
            transcript.exists(),
            "阶段 A 之后应有转录文件 {}（会话根内容：{:?}）",
            transcript.display(),
            std::fs::read_dir(&root)
                .map(|entries| entries
                    .filter_map(|entry| entry.ok().map(|entry| entry.file_name()))
                    .collect::<Vec<_>>())
                .unwrap_or_default()
        );
        session_id
    };

    // 第二进：同一会话根 + 同一 session_id → 走恢复路径。
    let mut kernel = Kernel::spawn();
    kernel.initialize(json!({
        "root": root_param(&root),
        "session_id": session_id,
    }));
    let (frames, messages) = kernel.run_turn(SECOND_TEXT, SECOND_REPLY);
    assert_eq!(final_text(&frames), SECOND_REPLY);

    assert_eq!(
        pairs(&messages),
        vec![
            ("user".to_string(), FIRST_TEXT.to_string()),
            ("assistant".to_string(), FIRST_REPLY.to_string()),
            ("user".to_string(), SECOND_TEXT.to_string()),
        ],
        "恢复后内核看到的历史（第一轮问答 + 本轮输入）"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
#[ignore = "验收发现的缺口：强杀内核会丢回合事件（只有优雅关停才 flush），修复后去掉本标记"]
fn kernel_reloads_history_even_when_previous_process_was_killed() {
    let root = temp_root("resume-killed");

    let session_id = {
        let mut kernel = Kernel::spawn();
        kernel.initialize(json!({"root": root_param(&root)}));
        let session_id = kernel.session_id();
        let (frames, _) = kernel.run_turn(FIRST_TEXT, FIRST_REPLY);
        assert_eq!(final_text(&frames), FIRST_REPLY);
        // 不关停：Drop 里直接 kill，模拟宿主崩溃。
        session_id
    };

    let mut kernel = Kernel::spawn();
    kernel.initialize(json!({
        "root": root_param(&root),
        "session_id": session_id,
    }));
    let (_, messages) = kernel.run_turn(SECOND_TEXT, SECOND_REPLY);
    assert_eq!(
        pairs(&messages),
        vec![
            ("user".to_string(), FIRST_TEXT.to_string()),
            ("assistant".to_string(), FIRST_REPLY.to_string()),
            ("user".to_string(), SECOND_TEXT.to_string()),
        ],
        "强杀后也应当能恢复"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn resume_of_unknown_session_is_rejected() {
    let root = temp_root("unknown-resume");
    let mut kernel = Kernel::spawn();
    kernel.send(json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocol_version": "1.0",
            "client": {"name": "resume-e2e"},
            "session": {"root": root_param(&root), "session_id": "20260101-000000-abcdef"},
        },
    }));

    let response = kernel.next_frame();
    assert_eq!(response["id"], 1);
    let message = response["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("未找到会话"),
        "恢复不存在的会话应被拒绝：{response}"
    );

    let _ = std::fs::remove_dir_all(&root);
}
