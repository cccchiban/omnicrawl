//! 内核会话生命周期的端到端验收：握手拿到会话 id、恢复旧会话、列举/重命名/归档/新建、
//! 注入 assistant 文本，以及单任务派生（`subagent.run`）的失败面。
//!
//! 恢复这条结论此前只有实现（`KernelSession::open` 带 ID 时 `reload_history`），没有 e2e 证明。
//! 协议 v1 的 `initialize.session` 语义是：**不给 `session_id` 才新建**（新建后 ID 随握手回包
//! 的 `result.session_id` 交给宿主），**给了就是恢复且该会话必须已存在**。
//!
//! 判据落在内核发出的帧上：`model.reply` 请求的 `params.messages` 就是内核持有的运行期历史，
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
        // 配置根也隔离：内核读不到开发者本机的 `~/.OmniCrawl`（`subagents.toml`、`AGENTS.md`、
        // `config.toml`），于是子代理开关这类配置在测试里就是默认值。
        let home = std::env::temp_dir().join(format!("omnicrawl-e2e-home-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&home);
        let mut child = Command::new(env!("CARGO_BIN_EXE_omnicrawl"))
            .env("HOME", &home)
            .env("USERPROFILE", &home)
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

    /// 握手；返回响应帧（`result.session_id` 就是内核当前的会话 id）。
    fn initialize(&mut self, session: Value) -> Value {
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
        response
    }

    /// 发一条请求并等它的响应帧（id 由调用方给，便于断言）。
    fn request(&mut self, id: u64, method_name: &str, params: Value) -> Value {
        self.send(json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method_name,
            "params": params,
        }));
        loop {
            let frame = self.next_frame();
            match frame.get("id").and_then(Value::as_u64) {
                // 通知（无 id）与旧请求的迟到响应（`turn.submit` 的应答可能在
                // `turn.finished` 之后才到）都跳过，继续等目标 id。
                Some(value) if value == id => return frame,
                _ => continue,
            }
        }
    }

    /// 发一条请求并断言它成功，返回 `result`。
    fn request_ok(&mut self, id: u64, method_name: &str, params: Value) -> Value {
        let response = self.request(id, method_name, params);
        assert!(
            response.get("error").is_none(),
            "{method_name} 应当成功：{response}"
        );
        response["result"].clone()
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

    // 第一进：不给 session_id → 内核新建并通过握手回包交出 ID；跑一轮后进程退出（转录落盘）。
    let session_id = {
        let mut kernel = Kernel::spawn();
        let handshake = kernel.initialize(json!({"root": root_param(&root)}));
        let from_frame = handshake["result"]["session_id"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        assert!(!from_frame.is_empty(), "握手回包应当带上新建的会话 id");
        let session_id = kernel.session_id();
        assert_eq!(from_frame, session_id, "握手回包与内核日志应当是同一个会话");
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

/// 会话生命周期命令一条链走完：list → rename → new → archive → list(archived) → resume。
///
/// 这些命令以前在 TUI 里只能报「协议没有入口」，现在由内核持有会话、宿主只投影结果，
/// 因此验收点落在每一步回包里的会话 id 与索引条目上。
#[test]
fn session_lifecycle_commands_round_trip() {
    let root = temp_root("lifecycle");
    let mut kernel = Kernel::spawn();
    let first = kernel.initialize(json!({"root": root_param(&root)}))["result"]["session_id"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(!first.is_empty(), "握手应当交出会话 id");

    // list：默认只看未归档，当前会话标记就是握手交出的那一个。
    let listed = kernel.request_ok(2, "session.list", json!({"limit": 10}));
    assert_eq!(listed["current_session_id"], json!(first));
    let sessions = listed["sessions"].as_array().cloned().unwrap_or_default();
    assert_eq!(sessions.len(), 1, "新建会话应当已进索引：{listed}");
    assert_eq!(sessions[0]["session_id"], json!(first));

    // rename：标题进索引条目，回包直接给宿主展示用。
    let renamed = kernel.request_ok(3, "session.rename", json!({"title": "重命名后的会话"}));
    assert_eq!(renamed["session"]["title"], "重命名后的会话");
    assert_eq!(renamed["session"]["session_id"], json!(first));

    // new：切到一条新会话，id 必须换，且老会话仍在索引里。
    let started = kernel.request_ok(4, "session.new", json!({}));
    let second = started["session_id"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(
        !second.is_empty() && second != first,
        "新会话应当换 id：{started}"
    );
    let listed = kernel.request_ok(5, "session.list", json!({"limit": 10}));
    assert_eq!(listed["current_session_id"], json!(second));
    assert_eq!(
        listed["sessions"].as_array().map(Vec::len),
        Some(2),
        "新会话不该顶掉旧会话：{listed}"
    );

    // archive：归档当前会话并自动新开一条。
    let archived = kernel.request_ok(6, "session.archive", json!({}));
    assert_eq!(archived["session"]["session_id"], json!(second));
    let third = archived["new_session_id"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(
        !third.is_empty() && third != second,
        "归档后应当自动开新会话：{archived}"
    );

    // 归档的只在 archived=true 里出现，默认列表看不到它。
    let archived_only = kernel.request_ok(7, "session.list", json!({"archived": true}));
    let items = archived_only["sessions"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert_eq!(items.len(), 1, "只有一条被归档：{archived_only}");
    assert_eq!(items[0]["session_id"], json!(second));
    let visible = kernel.request_ok(8, "session.list", json!({"limit": 10}));
    let ids: Vec<&str> = visible["sessions"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item["session_id"].as_str())
                .collect()
        })
        .unwrap_or_default();
    assert!(
        !ids.contains(&second.as_str()),
        "归档会话不应再出现在默认列表：{visible}"
    );

    // resume：切回归档会话（内核自动解除归档）并把重建后的历史回给宿主。
    let resumed = kernel.request_ok(9, "session.resume", json!({"session_id": first}));
    assert_eq!(resumed["session_id"], json!(first));
    assert_eq!(resumed["session"]["title"], "重命名后的会话");
    assert!(
        resumed["history"].is_array(),
        "恢复要回给宿主可重放的历史：{resumed}"
    );

    // history：提示历史是独立存储，没有提示时给空数组（不报错）。
    let history = kernel.request_ok(10, "session.history", json!({"query": ""}));
    assert!(
        history["entries"].is_array(),
        "提示历史应当是数组：{history}"
    );

    // 恢复不存在的会话：明确拒绝，不动当前会话。
    let missing = kernel.request(
        11,
        "session.resume",
        json!({"session_id": "20260101-000000-abcdef"}),
    );
    let message = missing["error"]["message"].as_str().unwrap_or_default();
    assert!(message.contains("未找到会话"), "应当明确报错：{missing}");

    kernel.shutdown();
    let _ = std::fs::remove_dir_all(&root);
}

/// `session.append`：只收 assistant，注入的文本必须出现在下一轮的 `model.reply` 消息里。
#[test]
fn session_append_injects_assistant_message_into_next_request() {
    let root = temp_root("append");
    let mut kernel = Kernel::spawn();
    kernel.initialize(json!({"root": root_param(&root)}));

    // 空内容不注入（与 Python `remember_review_report` 同口径）。
    let empty = kernel.request_ok(2, "session.append", json!({"content": "   "}));
    assert_eq!(empty["appended"], json!(false));

    // 只有 assistant 能注入：不借这个入口伪造用户输入。
    let rejected = kernel.request(
        3,
        "session.append",
        json!({"role": "user", "content": "伪造"}),
    );
    let message = rejected["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("assistant"),
        "非 assistant 角色应被拒绝：{rejected}"
    );

    let report = "[评审报告]\n发现 1 项问题。";
    let appended = kernel.request_ok(
        4,
        "session.append",
        json!({"role": "assistant", "content": report}),
    );
    assert_eq!(appended["appended"], json!(true));

    // 下一轮请求必须带上注入的消息，否则「注入」只写在盘上没进上下文。
    let (frames, messages) = kernel.run_turn("继续", "好的");
    assert_eq!(final_text(&frames), "好的");
    assert_eq!(
        pairs(&messages),
        vec![
            ("assistant".to_string(), report.to_string()),
            ("user".to_string(), "继续".to_string()),
        ],
        "注入的 assistant 消息应当排在下一轮用户输入之前"
    );

    kernel.shutdown();
    let _ = std::fs::remove_dir_all(&root);
}

/// `subagent.run` 未启用子代理时必须明确拒绝（带可判定的 `data.kind`），
/// 不能悄悄拿别的 agent 定义或空结果顶替。
#[test]
fn subagent_run_reports_disabled_subagents() {
    let root = temp_root("subagent-run-disabled");
    let mut kernel = Kernel::spawn();
    kernel.initialize(json!({"root": root_param(&root)}));

    let response = kernel.request(
        2,
        "subagent.run",
        json!({
            "agent_type": "review",
            "description": "评审当前代码变更",
            "prompt": "请评审当前工作区的代码变更。",
        }),
    );
    let error = response["error"].clone();
    assert_eq!(
        error["data"]["kind"],
        json!("SUBAGENT_DISABLED"),
        "未启用时要给可判定的原因：{response}"
    );
    assert!(
        error["message"]
            .as_str()
            .unwrap_or_default()
            .contains("SubAgent"),
        "文案要说明是子代理未启用：{response}"
    );

    // 参数校验在配置检查之前也要先给出明确错误（缺 prompt）。
    let missing = kernel.request(
        3,
        "subagent.run",
        json!({"agent_type": "review", "prompt": "  "}),
    );
    assert!(
        missing["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("prompt"),
        "缺少 prompt 应直接报参数错误：{missing}"
    );

    kernel.shutdown();
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

/// 转录里的全部事件（按写入顺序）。
fn events(path: &Path) -> Vec<Value> {
    let text = std::fs::read_to_string(path).expect("转录应当存在");
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .collect()
}

/// 转录里的事件类型序列（按写入顺序）。
fn event_types(path: &Path) -> Vec<String> {
    events(path)
        .iter()
        .filter_map(|event| event["type"].as_str().map(str::to_string))
        .collect()
}

/// 正常退出必须补写 `session_closed`，且已有业务事件的会话不能被当成空占位丢掉。
///
/// 这条覆盖「宿主发 shutdown → 内核收尾」这一段：Python 侧在 `Agent.close()` 里
/// 把补写事件夹在 `session.close.before` / `after` 两个钩子之间，内核侧就是对映。
#[test]
fn shutdown_writes_session_closed_event() {
    let root = temp_root("close-event");

    let mut kernel = Kernel::spawn();
    kernel.initialize(json!({"root": root_param(&root)}));
    let session_id = kernel.session_id();
    let (frames, _) = kernel.run_turn(FIRST_TEXT, FIRST_REPLY);
    assert_eq!(final_text(&frames), FIRST_REPLY);

    kernel.shutdown();

    let transcript = root.join("sessions").join(format!("{session_id}.jsonl"));
    assert!(
        transcript.exists(),
        "有业务事件的会话退出后必须保留：{}",
        transcript.display()
    );
    let types = event_types(&transcript);
    assert_eq!(
        types.last().map(String::as_str),
        Some("session_closed"),
        "转录最后一条应当是 session_closed：{types:?}"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// 运行中切换工作区：内核把自持会话的工作区指到新根，并转录 `workspace_switched`。
///
/// 这覆盖宿主侧 `/workspace` 编排里唯一动内核会话的一步（对映 Python
/// `_append_session_event("workspace_switched", {"from": ..., "to": ...})`）：
/// 会话不重建、历史不丢，只多一条切换事件；指向同一目录时不重复写事件。
#[test]
fn workspace_switch_appends_event_and_keeps_history() {
    let root = temp_root("workspace-switch");
    let base = std::env::temp_dir().join(format!(
        "omnicrawl-switch-base-{}",
        std::process::id()
    ));
    let target = std::env::temp_dir().join(format!(
        "omnicrawl-switch-target-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&base);
    let _ = std::fs::remove_dir_all(&target);
    std::fs::create_dir_all(&base).expect("无法建初始工作区");
    std::fs::create_dir_all(&target).expect("无法建切换目标工作区");

    let mut kernel = Kernel::spawn();
    kernel.initialize(json!({
        "root": root_param(&root),
        "workspace_root": root_param(&base),
    }));
    let session_id = kernel.session_id();
    let (frames, _) = kernel.run_turn(FIRST_TEXT, FIRST_REPLY);
    assert_eq!(final_text(&frames), FIRST_REPLY);

    // 切换：应答给出 from/to，且 `switched` 为真。
    let result = kernel.request_ok(
        3,
        "workspace.switch",
        json!({"path": root_param(&target)}),
    );
    assert_eq!(result["switched"], true, "切换应生效：{result}");
    assert_eq!(result["to"], root_param(&target));

    let transcript = root.join("sessions").join(format!("{session_id}.jsonl"));
    let all = events(&transcript);
    let last = all.last().expect("转录至少有 session_started");
    assert_eq!(last["type"], "workspace_switched", "切换后应多一条事件：{last}");
    assert_eq!(last["payload"]["from"], result["from"]);
    assert_eq!(last["payload"]["to"], root_param(&target));

    // 会话不重建：再跑一轮时，运行期历史里仍有第一轮的用户消息。
    let (_, messages) = kernel.run_turn(SECOND_TEXT, SECOND_REPLY);
    let pairs = pairs(&messages);
    assert!(
        pairs.contains(&("user".to_string(), FIRST_TEXT.to_string())),
        "切换工作区不应丢历史：{pairs:?}"
    );

    // 指向同一目录：不再重复写事件。
    let before = event_types(&transcript);
    let again = kernel.request_ok(
        4,
        "workspace.switch",
        json!({"path": root_param(&target)}),
    );
    assert_eq!(again["switched"], false, "同目录切换不算切换：{again}");
    assert_eq!(
        event_types(&transcript),
        before,
        "同目录切换不应再写事件"
    );

    // 空 path 是无效请求，且不落任何事件。
    let empty = kernel.request(5, "workspace.switch", json!({"path": "  "}));
    assert!(
        empty.get("error").is_some(),
        "空 path 应当被拒绝：{empty}"
    );
    assert_eq!(event_types(&transcript), before, "被拒绝的切换不应写事件");

    kernel.shutdown();
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&base);
    let _ = std::fs::remove_dir_all(&target);
}

/// 只有启动占位的会话在正常退出时应当被丢弃（补写 `session_closed` 后删除转录）。
///
/// 对映 Python `discard_current_empty_session()`：没有真实聊天内容的会话不该留在
/// 历史与索引里。
#[test]
fn shutdown_discards_empty_placeholder_session() {
    let root = temp_root("close-empty");

    let mut kernel = Kernel::spawn();
    kernel.initialize(json!({"root": root_param(&root)}));
    let session_id = kernel.session_id();
    kernel.shutdown();

    let transcript = root.join("sessions").join(format!("{session_id}.jsonl"));
    assert!(
        !transcript.exists(),
        "空占位会话应当被丢弃：{}",
        transcript.display()
    );
    let index = std::fs::read_to_string(root.join("index.json")).unwrap_or_default();
    assert!(
        !index.contains(&session_id),
        "索引里不该留下空会话：{index}"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// `session.events`：宿主回放历史页的数据源，回的是回退投影后的有效事件流。
#[test]
fn session_events_returns_the_active_transcript() {
    let root = temp_root("session-events");
    // 绑定工作区：`turn.undo` 要按工作区快照回滚副作用，未绑定会被拒绝。
    let workspace = std::env::temp_dir().join(format!(
        "omnicrawl-events-ws-{}",
        std::process::id()
    ));
    let _ = std::fs::create_dir_all(&workspace);
    let mut kernel = Kernel::spawn();
    kernel.initialize(json!({
        "root": root_param(&root),
        "workspace_root": root_param(&workspace),
    }));
    let session_id = kernel.session_id();
    kernel.run_turn(FIRST_TEXT, FIRST_REPLY);

    let result = kernel.request_ok(3, "session.events", json!({}));
    assert_eq!(result["session_id"], session_id);
    let events = result["events"].as_array().expect("events 是数组");
    let types: Vec<&str> = events
        .iter()
        .map(|event| event["type"].as_str().unwrap_or_default())
        .collect();
    assert!(types.contains(&"session_started"), "事件流：{types:?}");
    assert!(types.contains(&"user_message"), "事件流：{types:?}");
    assert!(types.contains(&"assistant_message"), "事件流：{types:?}");
    // 事件字段与转录行同形（宿主按 `type` / `payload` 消费）。
    let user = events
        .iter()
        .find(|event| event["type"] == "user_message")
        .expect("有用户消息事件");
    assert_eq!(user["payload"]["content"], FIRST_TEXT);

    // 撤回一轮后，被撤掉的轮次不再出现在有效事件流里（与 Python `read_active_events` 同义）。
    kernel.request_ok(4, "turn.undo", json!({}));
    let replayed = kernel.request_ok(5, "session.events", json!({}));
    let remaining = replayed["events"].as_array().expect("events 是数组");
    assert!(
        !remaining
            .iter()
            .any(|event| event["type"] == "user_message"),
        "撤回后不应再回放被撤掉的用户消息：{remaining:?}"
    );

    kernel.shutdown();
}

/// `subagent.query` 的 `list_worktrees`：宿主在 `/workspace` 切换前用它做 worktree 拦阻。
#[test]
fn subagent_query_lists_worktrees_from_the_managed_root() {
    let root = temp_root("worktree-list");
    let mut kernel = Kernel::spawn();
    kernel.initialize(json!({"root": root_param(&root)}));

    let result = kernel.request_ok(3, "subagent.query", json!({"action": "list_worktrees"}));
    assert_eq!(result["unavailable"], false, "worktree 清单不依赖任务表：{result}");
    let worktrees = result["worktrees"].as_array().expect("worktrees 是数组");
    // 隔离的 HOME 下托管根是空的：清单为空，但字段必须在。
    assert!(worktrees.is_empty(), "隔离 HOME 下不该有 worktree：{worktrees:?}");

    // 认不出的动作要报错并点出可选值（宿主拼错 action 时不该静默成功）。
    let response = kernel.request(4, "subagent.query", json!({"action": "nope"}));
    let message = response["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("list_worktrees"),
        "错误文案应点出可选动作：{response}"
    );

    kernel.shutdown();
}
