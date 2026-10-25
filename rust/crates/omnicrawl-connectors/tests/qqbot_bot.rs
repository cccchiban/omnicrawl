//! QQ 机器人连接器的编排测试：被动回复额度、正文/进度分段、命令与审批/提问桥。
//!
//! 只起进程内桩件：平台端口记录下发并把附件回放成固定字节，宿主用可编程的
//! `AgentDriver` 代替。重点是 QQ 的平台约束——不能编辑、同一条入站消息只有 5 次回复，
//! 因此这里钉住「正文段在工具边界封口」「工具合并成一段进度」「最终回答一定发得出去」。

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use omnicrawl_connectors::agent::{
    AgentDriver, AgentStatus, AskUserHandler, ConfirmHandler, ToolCall, ToolResult, TurnError,
    TurnEvent, WorkspaceSwitch,
};
use omnicrawl_connectors::qqbot::{
    load_qqbot_config, MediaUpload, QqApi, QqBot, QqPort, ReplyHandle, Target,
};
use serde_json::{json, Value};

static TEST_SEQUENCE: AtomicUsize = AtomicUsize::new(0);

/// 一次下发：文本走 `Msg::Text`（含 Markdown 标记）、富媒体走 `Msg::Media`。
#[derive(Debug, Clone, PartialEq, Eq)]
enum Msg {
    Text {
        content: String,
        markdown: bool,
    },
    Media {
        name: String,
        file_type: i64,
        bytes: usize,
    },
}

/// 记录下发的平台端口。
#[derive(Default)]
struct MockPort {
    sent: Mutex<Vec<(Target, Msg, String, i64)>>,
    downloads: Mutex<Vec<String>>,
    fail_after: Mutex<Option<usize>>,
}

impl MockPort {
    fn contents(&self) -> Vec<String> {
        self.sent
            .lock()
            .expect("发送锁中毒")
            .iter()
            .map(|(_target, message, _msg, _seq)| match message {
                Msg::Text { content, .. } => content.clone(),
                Msg::Media { name, .. } => format!("[media]{name}"),
            })
            .collect()
    }

    fn calls(&self) -> Vec<(Target, Msg, String, i64)> {
        self.sent.lock().expect("发送锁中毒").clone()
    }

    fn pushes(
        &self,
        target: &Target,
        message: Msg,
        msg_id: &str,
        msg_seq: i64,
    ) -> Result<String, String> {
        let mut sent = self.sent.lock().expect("发送锁中毒");
        if let Some(limit) = *self.fail_after.lock().expect("阈值锁中毒") {
            if sent.len() >= limit {
                return Err("模拟失败".to_string());
            }
        }
        sent.push((target.clone(), message, msg_id.to_string(), msg_seq));
        Ok(format!("m-{}", sent.len()))
    }
}

impl QqPort for MockPort {
    fn send(
        &self,
        target: &Target,
        content: &str,
        msg_id: &str,
        msg_seq: i64,
    ) -> Result<String, String> {
        self.pushes(
            target,
            Msg::Text {
                content: content.to_string(),
                markdown: false,
            },
            msg_id,
            msg_seq,
        )
    }

    fn send_markdown(
        &self,
        target: &Target,
        content: &str,
        msg_id: &str,
        msg_seq: i64,
    ) -> Result<String, String> {
        self.pushes(
            target,
            Msg::Text {
                content: content.to_string(),
                markdown: true,
            },
            msg_id,
            msg_seq,
        )
    }

    fn send_media(
        &self,
        target: &Target,
        upload: &MediaUpload,
        msg_id: &str,
        msg_seq: i64,
    ) -> Result<String, String> {
        self.pushes(
            target,
            Msg::Media {
                name: upload.file_name.clone(),
                file_type: upload.file_type(),
                bytes: upload.bytes.len(),
            },
            msg_id,
            msg_seq,
        )
    }

    fn download(&self, url: &str) -> Result<Vec<u8>, String> {
        self.downloads
            .lock()
            .expect("下载锁中毒")
            .push(url.to_string());
        Ok(b"file-bytes".to_vec())
    }
}

/// 可编程宿主。
#[derive(Default)]
struct MockDriver {
    confirm: Mutex<Option<Arc<dyn ConfirmHandler>>>,
    ask_user: Mutex<Option<Arc<dyn AskUserHandler>>>,
    turns: Mutex<Vec<String>>,
    script: Mutex<Value>,
    cancelled: AtomicBool,
    /// 当前模式与「被激活过几次」，用来验证 QQBot 的默认开启行为。
    mode: Mutex<String>,
    activations: Mutex<Vec<String>>,
    /// 让 `handle_command` 按命令表判定：未登记的命令回 `None`。
    strict: AtomicBool,
}

impl MockDriver {
    fn set_script(&self, script: Value) {
        *self.script.lock().expect("脚本锁中毒") = script;
    }

    fn turns(&self) -> Vec<String> {
        self.turns.lock().expect("回合锁中毒").clone()
    }

    fn activations(&self) -> Vec<String> {
        self.activations.lock().expect("激活记录锁中毒").clone()
    }

    fn set_mode(&self, mode: &str) {
        *self.mode.lock().expect("模式锁中毒") = mode.to_string();
    }
}

impl AgentDriver for MockDriver {
    fn status(&self) -> AgentStatus {
        AgentStatus {
            workspace_root: "D:/demo".to_string(),
            session_id: "s-1".to_string(),
        }
    }

    fn reset_conversation(&self) -> Result<(), String> {
        Ok(())
    }

    fn switch_workspace(&self, path: &str) -> Result<WorkspaceSwitch, String> {
        Ok(WorkspaceSwitch {
            workspace_root: path.to_string(),
            note: String::new(),
        })
    }

    fn handle_command(&self, text: &str, channel: &str) -> Result<Option<String>, String> {
        // 严格模式按命令表判定，用来验证「宿主不认识这条命令」的分支。
        if self.strict.load(Ordering::SeqCst) {
            let token = text
                .trim_start()
                .trim_start_matches('/')
                .split_whitespace()
                .next()
                .unwrap_or("")
                .split('@')
                .next()
                .unwrap_or("");
            if !self.command_menu().iter().any(|(name, _)| name == token) {
                return Ok(None);
            }
        }
        Ok(Some(format!("宿主收到（{channel}）：{text}")))
    }

    fn command_menu(&self) -> Vec<(String, String)> {
        vec![
            ("status".to_string(), String::new()),
            ("speaking-style".to_string(), String::new()),
            ("skills".to_string(), String::new()),
        ]
    }

    fn active_mode(&self) -> String {
        self.mode.lock().expect("模式锁中毒").clone()
    }

    fn activate_mode(&self, mode: &str) -> Result<String, String> {
        self.activations
            .lock()
            .expect("激活记录锁中毒")
            .push(mode.to_string());
        self.set_mode(mode);
        Ok(mode.to_string())
    }

    fn deactivate_mode(&self) -> Result<String, String> {
        let previous = std::mem::take(&mut *self.mode.lock().expect("模式锁中毒"));
        Ok(previous)
    }

    fn run_turn(&self, text: &str, events: &mut dyn FnMut(TurnEvent)) -> Result<String, TurnError> {
        self.turns
            .lock()
            .expect("回合锁中毒")
            .push(text.to_string());
        let script = self.script.lock().expect("脚本锁中毒").clone();
        if let Some(steps) = script.get("events").and_then(Value::as_array) {
            for step in steps {
                match step["kind"].as_str().unwrap_or("") {
                    "delta" => {
                        events(TurnEvent::Delta(
                            step["text"].as_str().unwrap_or("").to_string(),
                        ));
                    }
                    "reasoning" => {
                        events(TurnEvent::ReasoningDelta(
                            step["text"].as_str().unwrap_or("").to_string(),
                        ));
                    }
                    "tool_start" => {
                        events(TurnEvent::ToolStarted {
                            step: 1,
                            call: ToolCall {
                                name: step["name"].as_str().unwrap_or("bash").to_string(),
                                arguments: json!({"command": "echo hi"}),
                                id: Some(step["id"].as_str().unwrap_or("c1").to_string()),
                            },
                        });
                    }
                    "tool_finish" => {
                        events(TurnEvent::ToolFinished {
                            call: ToolCall {
                                name: step["name"].as_str().unwrap_or("bash").to_string(),
                                arguments: json!({"command": "echo hi"}),
                                id: Some(step["id"].as_str().unwrap_or("c1").to_string()),
                            },
                            result: ToolResult {
                                ok: step["ok"].as_bool().unwrap_or(true),
                                output: step["output"].as_str().unwrap_or("done").to_string(),
                                error_code: None,
                                retryable: false,
                            },
                        });
                    }
                    "confirm" => {
                        // 审批桥必须在任务线程里同步等待用户，这里就地从回合内触发。
                        let handler = self
                            .confirm
                            .lock()
                            .expect("确认锁中毒")
                            .clone()
                            .expect("应已注册确认桥");
                        let approved = handler.confirm(
                            step["tool"].as_str().unwrap_or("bash"),
                            &json!({"command": "rm -rf build"}),
                        );
                        events(TurnEvent::Delta(format!("确认结果：{approved}")));
                    }
                    "ask" => {
                        let handler = self
                            .ask_user
                            .lock()
                            .expect("提问锁中毒")
                            .clone()
                            .expect("应已注册提问桥");
                        let answer = handler.ask(&step["request"].clone());
                        events(TurnEvent::Delta(format!(
                            "回答：{}",
                            answer.unwrap_or_default()
                        )));
                    }
                    "todos" => {
                        events(TurnEvent::TodoUpdate {
                            todos: step["todos"].clone(),
                        });
                    }
                    other => panic!("未知脚本步骤：{other}"),
                }
            }
        }
        Ok(script
            .get("reply")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string())
    }

    fn request_cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }

    /// 每次测试用独立临时目录：同名附件在共享目录里会被重名规则改名。
    fn temp_root(&self) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!(
            "ocl-qqbot-tests-{}",
            TEST_SEQUENCE.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&root);
        root
    }

    fn set_confirm_handler(&self, handler: Arc<dyn ConfirmHandler>) {
        *self.confirm.lock().expect("确认锁中毒") = Some(handler);
    }

    fn set_ask_user_handler(&self, handler: Arc<dyn AskUserHandler>) {
        *self.ask_user.lock().expect("提问锁中毒") = Some(handler);
    }
}

fn bot_with(driver: Arc<MockDriver>, port: Arc<MockPort>) -> Arc<QqBot<MockDriver>> {
    let section = json!({
        "app_id": "102012345",
        "app_secret": "secret",
        "allowed_user_ids": ["F6A8"],
        "confirmation_timeout_seconds": 5,
    });
    let config = load_qqbot_config(Some(&section)).expect("配置可解析");
    let api = Arc::new(QqApi::new(
        &config.app_id,
        &config.app_secret,
        config.base_url(),
        Arc::new(omnicrawl_connectors::http::UreqTransport::new()),
    ));
    let bot = QqBot::new(&config, api, driver)
        .expect("构造 Bot")
        .with_port(port);
    let bot = Arc::new(bot);
    bot.bind_handlers();
    bot
}

fn c2c(openid: &str, content: &str, message_id: &str) -> Value {
    json!({
        "id": message_id,
        "content": content,
        "author": {"user_openid": openid},
    })
}

fn wait_for(deadline_seconds: f64, condition: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs_f64(deadline_seconds);
    while Instant::now() < deadline {
        if condition() {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("等待条件超时");
}

#[test]
fn task_reply_acknowledges_then_sends_the_answer() {
    let driver = Arc::new(MockDriver::default());
    let port = Arc::new(MockPort::default());
    let bot = bot_with(driver.clone(), port.clone());
    driver.set_script(json!({"reply": "目录里有 a.py 与 b.py。"}));
    bot.handle_event(
        omnicrawl_connectors::qqbot::EVENT_C2C_MESSAGE,
        &c2c("F6A8", "看看目录", "m-1"),
    );
    wait_for(5.0, || driver.turns().len() == 1);
    wait_for(5.0, || {
        port.contents().iter().any(|text| text.contains("a.py"))
    });

    let calls = port.calls();
    // 每条被动回复都带触发它的 msg_id，且序号递增。
    assert!(calls
        .iter()
        .all(|(_target, _content, msg, _seq)| msg == "m-1"));
    let seqs: Vec<i64> = calls.iter().map(|(_t, _c, _m, seq)| *seq).collect();
    let mut sorted = seqs.clone();
    sorted.sort_unstable();
    assert_eq!(seqs, sorted, "msg_seq 必须递增：{seqs:?}");
    assert!(
        port.contents()
            .iter()
            .any(|text| text.contains("已收到任务")),
        "{calls:?}"
    );
    assert_eq!(calls[0].0, Target::User("F6A8".to_string()));
}

#[test]
fn text_before_and_after_a_tool_are_separate_messages() {
    let driver = Arc::new(MockDriver::default());
    let port = Arc::new(MockPort::default());
    let bot = bot_with(driver.clone(), port.clone());
    driver.set_script(json!({
        "events": [
            {"kind": "delta", "text": "先看目录。"},
            {"kind": "tool_start", "name": "list", "id": "c1"},
            {"kind": "tool_finish", "name": "list", "id": "c1", "output": "a.py\nb.py"},
            {"kind": "delta", "text": "共两个文件。"},
        ],
        "reply": "先看目录。\n共两个文件。",
    }));
    bot.handle_event(
        omnicrawl_connectors::qqbot::EVENT_C2C_MESSAGE,
        &c2c("F6A8", "看看目录", "m-1"),
    );
    wait_for(5.0, || driver.turns().len() == 1);
    wait_for(5.0, || {
        port.contents()
            .iter()
            .any(|text| text.contains("共两个文件"))
    });

    let contents = port.contents();
    assert!(
        contents.iter().any(|text| text.contains("◇ 先看目录。")),
        "{contents:?}"
    );
    assert!(
        contents.iter().any(|text| text.contains("● list")),
        "工具调用应出现在进度消息里：{contents:?}"
    );
    assert!(
        !contents
            .iter()
            .any(|text| text.contains("先看目录。") && text.contains("共两个文件。")),
        "工具前后的正文不能黏在一起：{contents:?}"
    );
}

#[test]
fn the_last_answer_segment_is_not_sent_twice() {
    let driver = Arc::new(MockDriver::default());
    let port = Arc::new(MockPort::default());
    let bot = bot_with(driver.clone(), port.clone());
    // 宿主的最终回答只是最后一个模型 pass 的文本，不含工具前的正文段。
    driver.set_script(json!({
        "events": [
            {"kind": "delta", "text": "先看目录。"},
            {"kind": "tool_start", "name": "list", "id": "c1"},
            {"kind": "tool_finish", "name": "list", "id": "c1", "output": "a.py"},
            {"kind": "delta", "text": "共一个文件。"},
        ],
        "reply": "共一个文件。",
    }));
    bot.handle_event(
        omnicrawl_connectors::qqbot::EVENT_C2C_MESSAGE,
        &c2c("F6A8", "看看目录", "m-1"),
    );
    wait_for(5.0, || driver.turns().len() == 1);
    wait_for(5.0, || {
        port.contents()
            .iter()
            .any(|text| text.contains("共一个文件"))
    });
    wait_for(5.0, || !bot.is_busy());

    let contents = port.contents();
    assert_eq!(
        contents
            .iter()
            .filter(|text| text.contains("共一个文件"))
            .count(),
        1,
        "最后一段正文只能出现一次：{contents:?}"
    );
}

#[test]
fn tools_are_merged_into_a_single_progress_message() {
    let driver = Arc::new(MockDriver::default());
    let port = Arc::new(MockPort::default());
    let bot = bot_with(driver.clone(), port.clone());
    driver.set_script(json!({
        "events": [
            {"kind": "tool_start", "name": "bash", "id": "c1"},
            {"kind": "tool_finish", "name": "bash", "id": "c1", "output": "ok1"},
            {"kind": "tool_start", "name": "bash", "id": "c2"},
            {"kind": "tool_finish", "name": "bash", "id": "c2", "output": "ok2"},
            {"kind": "tool_start", "name": "bash", "id": "c3"},
            {"kind": "tool_finish", "name": "bash", "id": "c3", "output": "ok3"},
        ],
        "reply": "三次调用都完成了。",
    }));
    bot.handle_event(
        omnicrawl_connectors::qqbot::EVENT_C2C_MESSAGE,
        &c2c("F6A8", "跑三次", "m-1"),
    );
    wait_for(5.0, || driver.turns().len() == 1);
    wait_for(5.0, || {
        port.contents()
            .iter()
            .any(|text| text.contains("三次调用都完成了"))
    });

    let contents = port.contents();
    let progress = contents
        .iter()
        .find(|text| text.contains("● bash") && text.matches("● bash").count() > 1);
    assert!(
        progress.is_some(),
        "三次工具调用应合并成一条进度消息：{contents:?}"
    );
    // 不能编辑，因此每次下发都是一条新消息：合并才让额度够用。
    assert!(
        port.calls().len() <= 5,
        "总下发条数不能超出额度：{contents:?}"
    );
}

#[test]
fn final_answer_is_still_sent_when_the_budget_runs_short() {
    let driver = Arc::new(MockDriver::default());
    let port = Arc::new(MockPort::default());
    let bot = bot_with(driver.clone(), port.clone());
    // 多个 pass，每个 pass 都产生正文与工具调用：中间消息必须给最终回答让路。
    let mut events = Vec::new();
    for index in 0..6 {
        events.push(json!({"kind": "delta", "text": format!("第 {index} 段。")}));
        events.push(json!({"kind": "tool_start", "name": "bash", "id": format!("c{index}")}));
        events.push(json!({"kind": "tool_finish", "name": "bash", "id": format!("c{index}"), "output": "done"}));
    }
    driver.set_script(json!({"events": events, "reply": "全部完成，结论在最后。"}));
    bot.handle_event(
        omnicrawl_connectors::qqbot::EVENT_C2C_MESSAGE,
        &c2c("F6A8", "多步任务", "m-1"),
    );
    wait_for(5.0, || driver.turns().len() == 1);
    wait_for(5.0, || {
        port.contents()
            .iter()
            .any(|text| text.contains("全部完成，结论在最后。"))
    });
    assert!(
        port.calls().len() <= 5,
        "被动回复额度是 5 条：{:?}",
        port.contents()
    );
}

#[test]
fn unauthorized_sender_gets_no_reply_at_all() {
    let driver = Arc::new(MockDriver::default());
    let port = Arc::new(MockPort::default());
    let bot = bot_with(driver.clone(), port.clone());
    bot.handle_event(
        omnicrawl_connectors::qqbot::EVENT_C2C_MESSAGE,
        &c2c("STRANGER", "看看目录", "m-1"),
    );
    std::thread::sleep(Duration::from_millis(100));
    assert!(port.calls().is_empty());
    assert!(driver.turns().is_empty());
}

#[test]
fn commands_go_through_the_host_channel_name() {
    let driver = Arc::new(MockDriver::default());
    let port = Arc::new(MockPort::default());
    let bot = bot_with(driver.clone(), port.clone());
    bot.handle_event(
        omnicrawl_connectors::qqbot::EVENT_C2C_MESSAGE,
        &c2c("F6A8", "/status", "m-1"),
    );
    bot.handle_event(
        omnicrawl_connectors::qqbot::EVENT_C2C_MESSAGE,
        &c2c("F6A8", "/resume latest", "m-2"),
    );
    bot.handle_event(
        omnicrawl_connectors::qqbot::EVENT_C2C_MESSAGE,
        &c2c("F6A8", "/thinking on", "m-3"),
    );
    let contents = port.contents();
    assert!(
        contents
            .iter()
            .any(|text| text.contains("📊 OmniCrawl 状态")),
        "{contents:?}"
    );
    assert!(
        contents
            .iter()
            .any(|text| text.contains("宿主收到（qqbot）：/resume latest")),
        "{contents:?}"
    );
    assert!(contents
        .iter()
        .any(|text| text.contains("已开启思考内容显示")));
    assert!(bot.show_thinking());
}

#[test]
fn reasoning_is_hidden_until_thinking_is_enabled() {
    let driver = Arc::new(MockDriver::default());
    let port = Arc::new(MockPort::default());
    let bot = bot_with(driver.clone(), port.clone());
    driver.set_script(json!({
        "events": [{"kind": "reasoning", "text": "内部推理不应出现"}],
        "reply": "回答。",
    }));
    bot.handle_event(
        omnicrawl_connectors::qqbot::EVENT_C2C_MESSAGE,
        &c2c("F6A8", "问题", "m-1"),
    );
    wait_for(5.0, || driver.turns().len() == 1);
    wait_for(5.0, || {
        port.contents().iter().any(|text| text.contains("回答。"))
    });
    assert!(
        !port
            .contents()
            .iter()
            .any(|text| text.contains("内部推理不应出现")),
        "{:?}",
        port.contents()
    );
}

#[test]
fn reasoning_is_plain_text_not_a_card_payload() {
    let driver = Arc::new(MockDriver::default());
    let port = Arc::new(MockPort::default());
    let bot = bot_with(driver.clone(), port.clone());
    bot.handle_event(
        omnicrawl_connectors::qqbot::EVENT_C2C_MESSAGE,
        &c2c("F6A8", "/thinking on", "m-0"),
    );
    driver.set_script(json!({
        "events": [{"kind": "reasoning", "text": "先看目录再决定。"}],
        "reply": "结论。",
    }));
    bot.handle_event(
        omnicrawl_connectors::qqbot::EVENT_C2C_MESSAGE,
        &c2c("F6A8", "问题", "m-1"),
    );
    wait_for(5.0, || {
        port.contents().iter().any(|text| text.contains("结论。"))
    });

    let contents = port.contents();
    let reasoning = contents
        .iter()
        .find(|text| text.contains("🧠 思考"))
        .expect("应有一条思考消息");
    assert!(reasoning.contains("先看目录再决定。"), "{reasoning}");
    // 不能把飞书的卡片 JSON 当正文发出去。
    assert!(!reasoning.contains("collapsible_panel"), "{reasoning}");
    assert!(!reasoning.contains("\"tag\""), "{reasoning}");
}

#[test]
fn confirm_waits_for_approve_before_the_tool_runs() {
    let driver = Arc::new(MockDriver::default());
    let port = Arc::new(MockPort::default());
    let bot = bot_with(driver.clone(), port.clone());
    driver.set_script(json!({
        "events": [{"kind": "confirm", "tool": "bash"}],
        "reply": "",
    }));
    bot.handle_event(
        omnicrawl_connectors::qqbot::EVENT_C2C_MESSAGE,
        &c2c("F6A8", "删掉 build 目录", "m-1"),
    );
    wait_for(5.0, || {
        port.contents()
            .iter()
            .any(|text| text.contains("需要确认执行敏感操作"))
    });
    bot.handle_event(
        omnicrawl_connectors::qqbot::EVENT_C2C_MESSAGE,
        &c2c("F6A8", "/approve", "m-2"),
    );
    wait_for(5.0, || {
        port.contents()
            .iter()
            .any(|text| text.contains("确认结果：true"))
    });
    // 审批来自发起任务的白名单用户本人。
    assert!(port
        .contents()
        .iter()
        .any(|text| text.contains("已批准：bash")));
}

#[test]
fn ask_user_accepts_the_next_message_as_the_answer() {
    let driver = Arc::new(MockDriver::default());
    let port = Arc::new(MockPort::default());
    let bot = bot_with(driver.clone(), port.clone());
    driver.set_script(json!({
        "events": [{
            "kind": "ask",
            "request": {"question": "选哪个", "options": ["A", "B"], "kind": "select"},
        }],
        "reply": "",
    }));
    bot.handle_event(
        omnicrawl_connectors::qqbot::EVENT_C2C_MESSAGE,
        &c2c("F6A8", "帮我选", "m-1"),
    );
    wait_for(5.0, || {
        port.contents().iter().any(|text| text.contains("选哪个"))
    });
    bot.handle_event(
        omnicrawl_connectors::qqbot::EVENT_C2C_MESSAGE,
        &c2c("F6A8", "2", "m-2"),
    );
    wait_for(5.0, || {
        port.contents().iter().any(|text| text.contains("回答：B"))
    });
}

#[test]
fn attachments_are_downloaded_and_saved_into_agent_tmp() {
    let driver = Arc::new(MockDriver::default());
    let port = Arc::new(MockPort::default());
    let bot = bot_with(driver.clone(), port.clone());
    driver.set_script(json!({"reply": "收到图片。"}));
    let inbound = json!({
        "id": "m-20",
        "content": "",
        "author": {"user_openid": "F6A8"},
        "attachments": [{"url": "https://multimedia.nt.qq.com.cn/download/photo.png"}],
    });
    bot.handle_event(omnicrawl_connectors::qqbot::EVENT_C2C_MESSAGE, &inbound);
    wait_for(5.0, || driver.turns().len() == 1);

    assert_eq!(
        port.downloads.lock().expect("下载锁中毒").as_slice(),
        &["https://multimedia.nt.qq.com.cn/download/photo.png".to_string()]
    );
    let contents = port.contents();
    assert!(
        contents.iter().any(|text| text.contains("已收到 1 个附件")),
        "{contents:?}"
    );
    assert!(
        contents.iter().any(|text| text.contains("images/photo")),
        "应按扩展名落到 images 子目录：{contents:?}"
    );
    // 落盘内容也真的被交给 Agent 了。
    let turns = driver.turns();
    assert!(turns[0].contains("已收到附件"), "{turns:?}");
}

#[test]
fn intermediate_process_stops_after_four_replies_and_keeps_the_last_slot() {
    let driver = Arc::new(MockDriver::default());
    let port = Arc::new(MockPort::default());
    let bot = bot_with(driver.clone(), port.clone());
    // 每个 pass 都有正文与工具调用：中间消息必须严格停在 4 条，第 5 条留给结论。
    let mut events = Vec::new();
    for index in 0..8 {
        events.push(json!({"kind": "delta", "text": format!("第 {index} 段。")}));
        events.push(json!({"kind": "tool_start", "name": "bash", "id": format!("c{index}")}));
        events.push(json!({"kind": "tool_finish", "name": "bash", "id": format!("c{index}"), "output": "done"}));
    }
    events.push(json!({"kind": "todos", "todos": [
        {"id": "a", "step": "第一步", "completed": true},
        {"id": "b", "step": "第二步", "completed": false},
    ]}));
    driver.set_script(json!({"events": events, "reply": "全部完成，结论在最后。"}));
    bot.handle_event(
        omnicrawl_connectors::qqbot::EVENT_C2C_MESSAGE,
        &c2c("F6A8", "多步任务", "m-1"),
    );
    wait_for(5.0, || driver.turns().len() == 1);
    wait_for(5.0, || {
        port.contents()
            .iter()
            .any(|text| text.contains("全部完成，结论在最后"))
    });

    let contents = port.contents();
    assert_eq!(contents.len(), 5, "总下发条数必须是 5：{contents:?}");
    assert_eq!(
        contents.last().map(String::as_str),
        Some("全部完成，结论在最后。"),
        "第 5 条必须是最终文本：{contents:?}"
    );
    let intermediate = &contents[..4];
    assert_eq!(
        intermediate[0], "✅ 已收到任务，开始执行（/cancel 可取消）。",
        "{contents:?}"
    );
    assert_eq!(
        intermediate
            .iter()
            .filter(|text| text.starts_with("◇"))
            .count(),
        3,
        "回执占 1 条，正文段最多再占 3 条：{contents:?}"
    );
    assert!(
        !intermediate
            .iter()
            .any(|text| text.contains("第 4 段") || text.contains("第 5 段")),
        "中间额度用尽后不能再发正文或进度：{contents:?}"
    );
}

#[test]
fn markdown_content_goes_out_as_msg_type_2() {
    let driver = Arc::new(MockDriver::default());
    let port = Arc::new(MockPort::default());
    let bot = bot_with(driver.clone(), port.clone());
    driver.set_script(json!({"reply": "# 结论\n\n- 第一步完成\n- **第二步**完成"}));
    bot.handle_event(
        omnicrawl_connectors::qqbot::EVENT_C2C_MESSAGE,
        &c2c("F6A8", "总结一下", "m-1"),
    );
    wait_for(5.0, || {
        port.contents().iter().any(|text| text.contains("结论"))
    });

    let calls = port.calls();
    let markdown = calls
        .iter()
        .find_map(|(_target, message, _msg, _seq)| match message {
            Msg::Text {
                content,
                markdown: true,
            } if content.contains("# 结论") => Some(content.clone()),
            _ => None,
        })
        .expect("应有一条 Markdown 消息");
    assert!(markdown.contains("- **第二步**完成"), "{markdown}");
    // 回执这种纯文本不能被当成 Markdown 发。
    assert!(calls.iter().any(|(_t, message, _m, _s)| matches!(
        message,
        Msg::Text {
            content,
            markdown: false
        } if content.contains("已收到任务")
    )));
}

#[test]
fn plain_prose_is_not_sent_as_markdown() {
    let driver = Arc::new(MockDriver::default());
    let port = Arc::new(MockPort::default());
    let bot = bot_with(driver.clone(), port.clone());
    driver.set_script(json!({"reply": "目录里有 a.py 与 b.py。"}));
    bot.handle_event(
        omnicrawl_connectors::qqbot::EVENT_C2C_MESSAGE,
        &c2c("F6A8", "看看目录", "m-1"),
    );
    wait_for(5.0, || {
        port.contents().iter().any(|text| text.contains("a.py"))
    });
    assert!(port
        .calls()
        .iter()
        .all(|(_t, message, _m, _s)| !matches!(message, Msg::Text { markdown: true, .. })));
}

#[test]
fn image_markers_are_uploaded_and_sent_as_rich_media() {
    let driver = Arc::new(MockDriver::default());
    let port = Arc::new(MockPort::default());
    let bot = bot_with(driver.clone(), port.clone());
    let root = driver.temp_root();
    std::fs::create_dir_all(&root).expect("建目录");
    let image = root.join("shot.png");
    std::fs::write(&image, b"\x89PNG\r\n\x1a\n").expect("写图片");
    driver.set_script(json!({
        "reply": format!("截图在这里\n[FILE:{}]\n请查收。", image.display()),
    }));
    bot.handle_event(
        omnicrawl_connectors::qqbot::EVENT_C2C_MESSAGE,
        &c2c("F6A8", "截个图", "m-1"),
    );
    wait_for(5.0, || {
        port.calls()
            .iter()
            .any(|(_t, message, _m, _s)| matches!(message, Msg::Media { .. }))
    });

    let calls = port.calls();
    let media = calls
        .iter()
        .find_map(|(_target, message, _msg, _seq)| match message {
            Msg::Media {
                name,
                file_type,
                bytes,
            } => Some((name.clone(), *file_type, *bytes)),
            _ => None,
        })
        .expect("应有一条富媒体消息");
    assert_eq!(media.0, "shot.png");
    assert_eq!(media.1, 1, "png 应作为图片上传（file_type=1）");
    assert_eq!(media.2, 8);
    // 图片已单独发出，正文里不再重复 `[FILE:...]` 标记。
    assert!(
        port.contents()
            .iter()
            .any(|text| text.contains("截图在这里") && !text.contains("[FILE:")),
        "{:?}",
        port.contents()
    );
    assert!(calls.len() <= 5, "{:?}", port.contents());
}

#[test]
fn missing_media_file_keeps_the_path_in_text() {
    let driver = Arc::new(MockDriver::default());
    let port = Arc::new(MockPort::default());
    let bot = bot_with(driver.clone(), port.clone());
    driver.set_script(json!({"reply": "产物：[FILE:missing-file.png]"}));
    bot.handle_event(
        omnicrawl_connectors::qqbot::EVENT_C2C_MESSAGE,
        &c2c("F6A8", "生成图片", "m-1"),
    );
    wait_for(5.0, || {
        port.contents().iter().any(|text| text.contains("产物"))
    });
    assert!(
        port.contents()
            .iter()
            .any(|text| text.contains("[FILE:missing-file.png]")),
        "文件读不到时应保留路径文本：{:?}",
        port.contents()
    );
    assert!(port
        .calls()
        .iter()
        .all(|(_t, message, _m, _s)| !matches!(message, Msg::Media { .. })));
}

#[test]
fn channel_targets_never_send_markdown_or_rich_media() {
    let port = Arc::new(MockPort::default());
    let handle = ReplyHandle::new(port.clone(), Target::Channel("c-1".to_string()), "m-1");
    handle.send_final("# 标题\n\n[FILE:whatever.png]");
    let calls = port.calls();
    assert!(!calls.is_empty(), "频道消息仍应发出");
    assert!(calls.iter().all(|(_t, message, _m, _s)| matches!(
        message,
        Msg::Text {
            markdown: false,
            ..
        }
    )));
}

#[test]
fn reply_handle_hard_caps_intermediate_at_four_and_total_at_five() {
    let port = Arc::new(MockPort::default());
    let handle = ReplyHandle::new(port.clone(), Target::User("F6A8".to_string()), "m-1");
    assert_eq!(handle.intermediate_left(), 4);
    assert_eq!(handle.slots_left(), 5);

    for index in 0..6 {
        handle.send_intermediate(&format!("中间第 {index} 条"));
    }
    let contents = port.contents();
    assert_eq!(contents.len(), 4, "中间过程最多 4 条：{contents:?}");
    assert!(!handle.can_send_intermediate());

    assert_eq!(handle.send_final("最终回答"), 1);
    let contents = port.contents();
    assert_eq!(contents.len(), 5, "总下发最多 5 条：{contents:?}");
    assert_eq!(contents.last().map(String::as_str), Some("最终回答"));
    assert_eq!(handle.slots_left(), 0);
    assert_eq!(handle.send_final("第六条"), 0, "第六条必须被拒");
}

#[test]
fn reply_handle_splits_one_long_intermediate_message_within_budget() {
    let port = Arc::new(MockPort::default());
    let handle = ReplyHandle::new(port.clone(), Target::Group("A1B2".to_string()), "m-1");
    // 一条超长正文会切成多段，但中间额度只有 4 条。
    let long = "第一行。\n".repeat(1200);
    let sent = handle.send_intermediate(&long);
    assert_eq!(sent, 4, "{:?}", port.contents().len());
    assert!(!handle.can_send_intermediate());
    assert_eq!(handle.send_final("结论"), 1);
    assert_eq!(port.contents().len(), 5);
}

#[test]
fn tool_output_is_not_sent_to_the_user() {
    let driver = Arc::new(MockDriver::default());
    let port = Arc::new(MockPort::default());
    let bot = bot_with(driver.clone(), port.clone());
    driver.set_script(json!({
        "events": [
            {"kind": "tool_start", "name": "bash", "id": "c1"},
            {"kind": "tool_finish", "name": "bash", "id": "c1", "output": "OUTPUT-MARKER-12345"},
        ],
        "reply": "跑完了。",
    }));
    bot.handle_event(
        omnicrawl_connectors::qqbot::EVENT_C2C_MESSAGE,
        &c2c("F6A8", "跑一下", "m-1"),
    );
    wait_for(5.0, || driver.turns().len() == 1);
    wait_for(5.0, || {
        port.contents().iter().any(|text| text.contains("跑完了"))
    });

    let contents = port.contents();
    assert!(
        contents.iter().any(|text| text.contains("● bash")),
        "进度消息里仍应有这次调用：{contents:?}"
    );
    assert!(
        !contents
            .iter()
            .any(|text| text.contains("OUTPUT-MARKER-12345")),
        "工具输出不应下发：{contents:?}"
    );
}

#[test]
fn progress_lines_stay_single_line_per_tool() {
    let driver = Arc::new(MockDriver::default());
    let port = Arc::new(MockPort::default());
    let bot = bot_with(driver.clone(), port.clone());
    driver.set_script(json!({
        "events": [
            {"kind": "tool_start", "name": "grep", "id": "c1"},
            {"kind": "tool_finish", "name": "grep", "id": "c1", "output": "a.rs:1: hit"},
        ],
        "reply": "找完了。",
    }));
    bot.handle_event(
        omnicrawl_connectors::qqbot::EVENT_C2C_MESSAGE,
        &c2c("F6A8", "找一下", "m-1"),
    );
    wait_for(5.0, || driver.turns().len() == 1);
    wait_for(5.0, || {
        port.contents().iter().any(|text| text.contains("找完了"))
    });
    let progress = port
        .contents()
        .into_iter()
        .find(|text| text.contains("● grep"))
        .expect("应有工具进度行");
    assert_eq!(
        progress.lines().count(),
        1,
        "每次工具调用只占一行：{progress:?}"
    );
}

#[test]
fn code_blocks_and_tables_stay_markdown_instead_of_going_plain() {
    let driver = Arc::new(MockDriver::default());
    let port = Arc::new(MockPort::default());
    let bot = bot_with(driver.clone(), port.clone());
    driver.set_script(json!({
        "reply": "## 结果\n\n- **第一步**完成\n\n```rust\nfn main() {}\n```",
    }));
    bot.handle_event(
        omnicrawl_connectors::qqbot::EVENT_C2C_MESSAGE,
        &c2c("F6A8", "写段代码", "m-1"),
    );
    wait_for(5.0, || {
        port.contents().iter().any(|text| text.contains("结果"))
    });

    let markdown = port
        .calls()
        .into_iter()
        .find_map(|(_target, message, _msg, _seq)| match message {
            Msg::Text {
                content,
                markdown: true,
            } if content.contains("第一步") => Some(content),
            _ => None,
        })
        .expect("含代码块的正文仍应按 Markdown 下发");
    assert!(!markdown.contains("```"), "围栏应已换成引用：{markdown}");
    assert!(markdown.contains("**第一步**"), "同段的粗体不能被丢掉：{markdown}");
    assert!(markdown.contains("> fn main() {}"), "{markdown}");
}

#[test]
fn block_syntax_gets_its_own_line_after_the_prefix() {
    let driver = Arc::new(MockDriver::default());
    let port = Arc::new(MockPort::default());
    let bot = bot_with(driver.clone(), port.clone());
    driver.set_script(json!({
        "events": [
            {"kind": "delta", "text": "# 结论"},
            {"kind": "tool_start", "name": "bash", "id": "c1"},
            {"kind": "tool_finish", "name": "bash", "id": "c1", "output": "done"},
        ],
        "reply": "# 结论\n\n做完了。",
    }));
    bot.handle_event(
        omnicrawl_connectors::qqbot::EVENT_C2C_MESSAGE,
        &c2c("F6A8", "总结", "m-1"),
    );
    wait_for(5.0, || driver.turns().len() == 1);
    wait_for(5.0, || {
        port.contents().iter().any(|text| text.contains("做完了"))
    });

    let markdown = port
        .calls()
        .into_iter()
        .find_map(|(_target, message, _msg, _seq)| match message {
            Msg::Text {
                content,
                markdown: true,
            } if content.contains("结论") => Some(content),
            _ => None,
        })
        .expect("正文段应按 Markdown 下发");
    assert!(
        markdown.starts_with("◇\n# 结论"),
        "标题不能被 `◇ ` 前缀吃掉：{markdown:?}"
    );
}


#[test]
fn default_speaking_style_is_enabled_once_at_startup() {
    let driver = Arc::new(MockDriver::default());
    let port = Arc::new(MockPort::default());
    let bot = bot_with(driver.clone(), port);
    bot.enable_default_speaking_style();
    assert_eq!(driver.activations(), vec!["speaking-style".to_string()]);
    assert_eq!(driver.active_mode(), "speaking-style");
}

#[test]
fn an_explicit_mode_is_not_overridden_by_the_default() {
    let driver = Arc::new(MockDriver::default());
    let port = Arc::new(MockPort::default());
    driver.set_mode("plan");
    let bot = bot_with(driver.clone(), port);
    bot.enable_default_speaking_style();
    assert!(driver.activations().is_empty(), "已有模式时不应再激活");
    assert_eq!(driver.active_mode(), "plan");
}

#[test]
fn a_misspelled_command_is_answered_with_the_closest_one() {
    let driver = Arc::new(MockDriver::default());
    driver.strict.store(true, Ordering::SeqCst);
    let port = Arc::new(MockPort::default());
    let bot = bot_with(driver.clone(), port.clone());
    bot.handle_event(
        omnicrawl_connectors::qqbot::EVENT_C2C_MESSAGE,
        &c2c("F6A8", "/speaking-styl", "m-1"),
    );
    let contents = port.contents();
    assert!(
        contents
            .iter()
            .any(|text| text.contains("是不是想用 /speaking-style")),
        "{contents:?}"
    );
    // 只提示、不执行：拼错的命令不会真的切模式。
    assert!(driver.activations().is_empty(), "不应替用户执行命令");
}
