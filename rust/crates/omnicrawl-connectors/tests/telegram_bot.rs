//! Telegram 连接器的编排测试：时间线消息、命令分发与审批/提问桥。
//!
//! 只起进程内桩件：HTTP 传输记录请求并回放固定响应，宿主用一个可编程的 `AgentDriver`
//! 代替。重点是显示口径——正文段、工具调用、执行计划各自独立成一条消息，条目内的流式
//! 更新只编辑自己那条消息（这正是「所有 AI 消息堆在一条里」的回归测试）。

use std::collections::HashSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use omnicrawl_connectors::agent::{
    AgentDriver, AgentStatus, AskUserHandler, ConfirmHandler, ToolCall, ToolResult, TurnError,
    TurnEvent, WorkspaceSwitch,
};
use omnicrawl_connectors::http::{HttpReply, HttpTransport};
use omnicrawl_connectors::telegram::{load_telegram_config, TelegramBot};
use serde_json::{json, Value};

/// 记录请求并按 URL 回放响应的传输桩件；`sendMessage` 逐条分配 message_id。
#[derive(Default)]
struct MockHttp {
    requests: Mutex<Vec<Value>>,
    messages_created: AtomicUsize,
    /// 「内容未修改」响应的次数：验证它不会被当成失败。
    not_modified_edits: AtomicUsize,
}

impl MockHttp {
    fn requests(&self) -> Vec<Value> {
        self.requests.lock().expect("请求锁中毒").clone()
    }

    /// 所有 POST 请求体（已做百分号解码，便于断言中文正文）。
    fn bodies(&self) -> Vec<String> {
        self.requests()
            .iter()
            .filter(|request| request["method"] == "POST")
            .filter_map(|request| request["body"].as_str().map(percent_decode))
            .collect()
    }

    /// 某个方法被调用的次数（`sendMessage` / `editMessageText`）。
    fn calls_of(&self, method: &str) -> usize {
        self.requests()
            .iter()
            .filter(|request| request["url"].as_str().unwrap_or("").contains(method))
            .count()
    }
}

/// `application/x-www-form-urlencoded` 的解码：`+` 还原成空格，`%XX` 还原成字节。
fn percent_decode(body: &str) -> String {
    let bytes = body.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            b'%' if index + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(byte) => {
                        out.push(byte);
                        index += 3;
                    }
                    Err(_) => {
                        out.push(b'%');
                        index += 1;
                    }
                }
            }
            other => {
                out.push(other);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

impl HttpTransport for MockHttp {
    fn request(
        &self,
        method: &str,
        url: &str,
        _headers: &[(String, String)],
        body: Option<&[u8]>,
        _timeout: Duration,
    ) -> Result<HttpReply, String> {
        let body_text = body
            .map(|raw| String::from_utf8_lossy(raw).to_string())
            .unwrap_or_default();
        self.requests.lock().expect("请求锁中毒").push(json!({
            "method": method,
            "url": url,
            "body": body_text,
        }));
        let payload = if url.contains("getUpdates") {
            json!({"ok": true, "result": []})
        } else if url.contains("sendMessage") {
            let count = self.messages_created.fetch_add(1, Ordering::SeqCst) + 1;
            json!({"ok": true, "result": {"message_id": count}})
        } else if url.contains("editMessageText")
            && self.not_modified_edits.load(Ordering::SeqCst) > 0
        {
            // 按需模拟 Telegram 的「内容未修改」：它不是失败，不能被当成编辑失败。
            self.not_modified_edits.fetch_sub(1, Ordering::SeqCst);
            json!({
                "ok": false,
                "error_code": 400,
                "description": "Bad Request: message is not modified",
            })
        } else {
            json!({"ok": true, "result": {}})
        };
        Ok(HttpReply {
            status: 200,
            body: payload.to_string().into_bytes(),
            headers: Vec::new(),
        })
    }
}

/// 可编程宿主：按脚本外发事件。
#[derive(Default)]
struct MockDriver {
    confirm: Mutex<Option<Arc<dyn ConfirmHandler>>>,
    ask_user: Mutex<Option<Arc<dyn AskUserHandler>>>,
    turns: Mutex<Vec<String>>,
    script: Mutex<Value>,
    menu: Vec<(String, String)>,
}

impl MockDriver {
    fn with_menu(menu: Vec<(&str, &str)>) -> MockDriver {
        MockDriver {
            menu: menu
                .into_iter()
                .map(|(command, description)| (command.to_string(), description.to_string()))
                .collect(),
            ..MockDriver::default()
        }
    }

    fn set_script(&self, script: Value) {
        *self.script.lock().expect("脚本锁中毒") = script;
    }

    fn turns(&self) -> Vec<String> {
        self.turns.lock().expect("回合锁中毒").clone()
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

    fn handle_command(&self, text: &str, _channel: &str) -> Result<Option<String>, String> {
        Ok(Some(format!("宿主收到：{text}")))
    }

    fn command_menu(&self) -> Vec<(String, String)> {
        self.menu.clone()
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
                    "delta" => events(TurnEvent::Delta(
                        step["text"].as_str().unwrap_or("").to_string(),
                    )),
                    "tool_start" => events(TurnEvent::ToolStarted {
                        step: 1,
                        call: ToolCall {
                            name: step["name"].as_str().unwrap_or("bash").to_string(),
                            arguments: json!({"command": "echo hi"}),
                            id: Some(step["id"].as_str().unwrap_or("c1").to_string()),
                        },
                    }),
                    "tool_finish" => events(TurnEvent::ToolFinished {
                        call: ToolCall {
                            name: step["name"].as_str().unwrap_or("bash").to_string(),
                            arguments: Value::Null,
                            id: Some(step["id"].as_str().unwrap_or("c1").to_string()),
                        },
                        result: ToolResult {
                            ok: step["ok"].as_bool().unwrap_or(true),
                            output: step["output"].as_str().unwrap_or("done").to_string(),
                            error_code: None,
                            retryable: false,
                        },
                    }),
                    "todos" => events(TurnEvent::TodoUpdate {
                        todos: step["todos"].clone(),
                    }),
                    "subagent" => events(TurnEvent::SubagentEvent {
                        name: step["name"].as_str().unwrap_or("").to_string(),
                        payload: step["payload"].clone(),
                    }),
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

    fn request_cancel(&self) {}

    fn temp_root(&self) -> std::path::PathBuf {
        std::env::temp_dir().join("ocl-telegram-bot-tests")
    }

    fn set_confirm_handler(&self, handler: Arc<dyn ConfirmHandler>) {
        *self.confirm.lock().expect("确认锁中毒") = Some(handler);
    }

    fn set_ask_user_handler(&self, handler: Arc<dyn AskUserHandler>) {
        *self.ask_user.lock().expect("提问锁中毒") = Some(handler);
    }
}

fn bot_with(driver: Arc<MockDriver>, http: Arc<MockHttp>) -> Arc<TelegramBot<MockDriver>> {
    let section = json!({
        "bot_token": "123:token",
        "allowed_user_ids": [7],
        "confirmation_timeout_seconds": 5,
    });
    let config = load_telegram_config(Some(&section)).expect("配置可解析");
    let transport: Arc<dyn HttpTransport> = http;
    let bot = Arc::new(TelegramBot::new(&config, transport, driver).expect("构造 Bot"));
    bot.bind_handlers();
    bot
}

fn update(chat_id: i64, user_id: i64, text: &str) -> Value {
    json!({"message": {"chat": {"id": chat_id}, "from": {"id": user_id}, "text": text}})
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
fn text_before_and_after_tool_use_are_separate_messages() {
    let driver = Arc::new(MockDriver::default());
    let http = Arc::new(MockHttp::default());
    let bot = bot_with(driver.clone(), http.clone());
    driver.set_script(json!({
        "events": [
            {"kind": "delta", "text": "先看目录。"},
            {"kind": "tool_start", "name": "list"},
            {"kind": "tool_finish", "name": "list", "ok": true, "output": "a.py\nb.py\n"},
            {"kind": "delta", "text": "共两个文件。"},
        ],
        "reply": "先看目录。\n共两个文件。",
    }));
    bot.handle_update(&update(11, 7, "看看目录"));
    wait_for(5.0, || driver.turns().len() == 1);
    wait_for(5.0, || http.calls_of("sendMessage") >= 3);
    std::thread::sleep(Duration::from_millis(100));

    let bodies = http.bodies();
    // 工具前后的正文各自成条：不会黏在同一段里。
    assert!(
        bodies.iter().any(|body| body.contains("◇ 先看目录。")),
        "{bodies:?}"
    );
    assert!(
        bodies.iter().any(|body| body.contains("◇ 共两个文件。")),
        "工具之后的正文应另起一条：{bodies:?}"
    );
    assert!(
        bodies.iter().any(|body| body.contains("● list")),
        "工具调用应独立成条：{bodies:?}"
    );
    assert!(
        !bodies
            .iter()
            .any(|body| body.contains("先看目录。") && body.contains("共两个文件。")),
        "两段正文不应堆在同一条消息里：{bodies:?}"
    );
}

#[test]
fn tool_message_is_edited_in_place_on_finish() {
    let driver = Arc::new(MockDriver::default());
    let http = Arc::new(MockHttp::default());
    let bot = bot_with(driver.clone(), http.clone());
    driver.set_script(json!({
        "events": [
            {"kind": "tool_start", "name": "bash", "id": "c1"},
            {"kind": "tool_finish", "name": "bash", "id": "c1", "ok": false, "output": "boom"},
        ],
        "reply": "",
    }));
    bot.handle_update(&update(11, 7, "跑一下"));
    wait_for(5.0, || driver.turns().len() == 1);
    wait_for(5.0, || http.calls_of("editMessageText") >= 1);
    std::thread::sleep(Duration::from_millis(100));

    let bodies = http.bodies();
    assert!(
        bodies
            .iter()
            .any(|body| body.contains("● bash echo hi · … 调用中")),
        "{bodies:?}"
    );
    assert!(
        bodies.iter().any(|body| body.contains("✗ 失败")),
        "结果应原地收口：{bodies:?}"
    );
}

#[test]
fn todo_updates_own_a_single_message() {
    let driver = Arc::new(MockDriver::default());
    let http = Arc::new(MockHttp::default());
    let bot = bot_with(driver.clone(), http.clone());
    driver.set_script(json!({
        "events": [
            {"kind": "todos", "todos": {"todos": [
                {"step": "读代码", "completed": false},
                {"step": "改实现", "completed": false},
            ]}},
            {"kind": "todos", "todos": {"todos": [
                {"step": "读代码", "completed": true},
                {"step": "改实现", "completed": false},
            ]}},
        ],
        "reply": "完成",
    }));
    bot.handle_update(&update(11, 7, "做个计划"));
    wait_for(5.0, || driver.turns().len() == 1);
    wait_for(5.0, || http.calls_of("editMessageText") >= 1);
    std::thread::sleep(Duration::from_millis(100));

    let bodies = http.bodies();
    assert!(
        bodies
            .iter()
            .any(|body| body.contains("执行计划 · 0/2 完成")),
        "{bodies:?}"
    );
    assert!(
        bodies
            .iter()
            .any(|body| body.contains("执行计划 · 1/2 完成")),
        "计划更新应原地替换：{bodies:?}"
    );
}

/// 「内容未修改」（400 message is not modified）不是失败：条目不会被判死，
/// 后续交付仍能把内容推进到新版本。
#[test]
fn not_modified_edit_is_treated_as_success() {
    let driver = Arc::new(MockDriver::default());
    let http = Arc::new(MockHttp::default());
    http.not_modified_edits.store(1, Ordering::SeqCst);
    let bot = bot_with(driver.clone(), http.clone());
    driver.set_script(json!({
        "events": [{"kind": "delta", "text": "第一版。"}],
        "reply": "第一版。第二版续写。",
    }));
    bot.handle_update(&update(11, 7, "继续写"));
    wait_for(5.0, || driver.turns().len() == 1);
    wait_for(5.0, || http.calls_of("editMessageText") >= 1);
    std::thread::sleep(Duration::from_millis(400));
    let bodies = http.bodies();
    assert!(
        bodies.iter().any(|body| body.contains("第二版续写")),
        "未修改错误不应让正文停在首版：{bodies:?}"
    );
}

#[test]
fn unauthorized_user_gets_no_request() {
    let driver = Arc::new(MockDriver::default());
    let http = Arc::new(MockHttp::default());
    let bot = bot_with(driver.clone(), http.clone());
    bot.handle_update(&update(11, 9, "你好"));
    std::thread::sleep(Duration::from_millis(80));
    assert!(driver.turns().is_empty());
    assert!(http.requests().is_empty(), "未授权用户不应触发请求");
}

#[test]
fn commands_are_answered_locally() {
    let driver = Arc::new(MockDriver::default());
    let http = Arc::new(MockHttp::default());
    let bot = bot_with(driver.clone(), http.clone());
    bot.dispatch(11, 7, "/status");
    bot.dispatch(11, 7, "/thinking on");
    bot.dispatch(11, 7, "/resume latest");
    wait_for(5.0, || http.calls_of("sendMessage") >= 3);
    let bodies = http.bodies();
    assert!(bodies.iter().any(|body| body.contains("📊 OmniCrawl 状态")));
    assert!(bodies
        .iter()
        .any(|body| body.contains("已开启思考内容显示")));
    assert!(bodies
        .iter()
        .any(|body| body.contains("宿主收到：/resume latest")));
    assert!(bot.show_thinking());
}

#[test]
fn empty_reply_still_reports_completion() {
    let driver = Arc::new(MockDriver::default());
    let http = Arc::new(MockHttp::default());
    let bot = bot_with(driver.clone(), http.clone());
    driver.set_script(json!({"reply": ""}));
    bot.handle_update(&update(11, 7, "不说话"));
    wait_for(5.0, || driver.turns().len() == 1);
    // 回执「已收到任务」也会增加 sendMessage 计数，因此等的是兜底文案本身。
    wait_for(5.0, || {
        http.bodies()
            .iter()
            .any(|body| body.contains("任务完成，无文本输出"))
    });
    let bodies = http.bodies();
    assert!(
        bodies
            .iter()
            .any(|body| body.contains("任务完成，无文本输出")),
        "{bodies:?}"
    );
}

/// 白名单之外的会话不会拿到别人的任务（群聊里多用户共存）。
#[test]
fn allowed_set_is_enforced_from_config() {
    let section = json!({
        "bot_token": "123:token",
        "allowed_user_ids": "7，8",
    });
    let config = load_telegram_config(Some(&section)).expect("配置可解析");
    assert_eq!(
        config
            .allowed_user_ids
            .iter()
            .copied()
            .collect::<HashSet<i64>>(),
        HashSet::from([7_i64, 8])
    );
}

/// 启动时把宿主命令表发布成 bot 菜单：`setMyCommands` 收到的是裸命令名与说明。
#[test]
fn startup_publishes_host_command_menu() {
    let driver = Arc::new(MockDriver::with_menu(vec![
        ("/sessions", "查看当前工作区最近会话。"),
        ("memory:clean", "清理过期记忆。"),
        ("plan", "启用计划模式。"),
    ]));
    let http = Arc::new(MockHttp::default());
    let bot = bot_with(driver.clone(), http.clone());
    bot.publish_command_menu();

    let requests = http.requests();
    let request = requests
        .iter()
        .find(|request| {
            request["url"]
                .as_str()
                .unwrap_or("")
                .contains("setMyCommands")
        })
        .expect("应调用 setMyCommands");
    let body = percent_decode(request["body"].as_str().unwrap_or(""));
    assert!(body.contains("sessions"), "{body}");
    assert!(body.contains("plan"), "{body}");
    assert!(!body.contains("memory:clean"), "非法命令名应被丢弃：{body}");
    assert!(!body.contains("/sessions"), "命令名不带前导斜杠：{body}");
    assert!(http.calls_of("getUpdates") == 0, "发布菜单不应触发轮询");
}

/// 宿主没有菜单时不去改平台侧设置，避免把用户自己在 BotFather 里配的菜单清空。
#[test]
fn empty_menu_skips_set_my_commands() {
    let driver = Arc::new(MockDriver::default());
    let http = Arc::new(MockHttp::default());
    let bot = bot_with(driver.clone(), http.clone());
    bot.publish_command_menu();
    assert!(
        http.requests().is_empty(),
        "空菜单不应发出任何请求：{:?}",
        http.requests()
    );
}
