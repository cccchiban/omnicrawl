//! 飞书连接器的编排测试：事件接入、命令分发、时间线消息与审批/提问桥。
//!
//! 只起进程内桩件：HTTP 传输记录请求并回放固定响应，宿主用一个可编程的
//! `AgentDriver` 代替，验证连接器自己那部分（去重、白名单、消息序列、等待语义）。

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use omnicrawl_connectors::agent::{
    AgentDriver, AgentStatus, AskUserHandler, ConfirmHandler, ToolCall, ToolResult, TurnError,
    TurnEvent, WorkspaceSwitch,
};
use omnicrawl_connectors::feishu::{FeishuApi, FeishuBot};
use omnicrawl_connectors::http::{HttpReply, HttpTransport};
use serde_json::{json, Value};

/// 记录请求并按 URL 回放响应的传输桩件。
#[derive(Default)]
struct MockHttp {
    requests: Mutex<Vec<Value>>,
    messages_created: AtomicUsize,
}

impl MockHttp {
    fn requests(&self) -> Vec<Value> {
        self.requests.lock().expect("请求锁中毒").clone()
    }

    fn created_messages(&self) -> usize {
        self.messages_created.load(Ordering::SeqCst)
    }
}

impl HttpTransport for MockHttp {
    fn request(
        &self,
        method: &str,
        url: &str,
        headers: &[(String, String)],
        body: Option<&[u8]>,
        _timeout: Duration,
    ) -> Result<HttpReply, String> {
        let body_text = body
            .map(|raw| String::from_utf8_lossy(raw).to_string())
            .unwrap_or_default();
        self.requests.lock().expect("请求锁中毒").push(json!({
            "method": method,
            "url": url,
            "authorization": headers
                .iter()
                .find(|(key, _value)| key == "Authorization")
                .map(|(_key, value)| value.clone()),
            "body": body_text,
        }));
        let payload = if url.contains("tenant_access_token") {
            json!({"code": 0, "tenant_access_token": "t-mock", "expire": 3600})
        } else if url.contains("/im/v1/images") {
            json!({"code": 0, "data": {"image_key": "img_mock"}})
        } else if url.contains("/im/v1/files") {
            json!({"code": 0, "data": {"file_key": "file_mock"}})
        } else if url.contains("/im/v1/messages") && method == "POST" {
            self.messages_created.fetch_add(1, Ordering::SeqCst);
            json!({"code": 0, "data": {"message_id": format!("om_{}", self.messages_created.load(Ordering::SeqCst))}})
        } else {
            json!({"code": 0})
        };
        Ok(HttpReply {
            status: 200,
            body: payload.to_string().into_bytes(),
            headers: Vec::new(),
        })
    }
}

/// 可编程宿主：按脚本外发事件，并记录确认/提问回调的结果。
#[derive(Default)]
struct MockDriver {
    confirm: Mutex<Option<Arc<dyn ConfirmHandler>>>,
    ask_user: Mutex<Option<Arc<dyn AskUserHandler>>>,
    turns: Mutex<Vec<String>>,
    confirm_results: Mutex<Vec<bool>>,
    cancel_requested: AtomicUsize,
    script: Mutex<Value>,
}

impl MockDriver {
    fn set_script(&self, script: Value) {
        *self.script.lock().expect("脚本锁中毒") = script;
    }

    fn confirm_results(&self) -> Vec<bool> {
        self.confirm_results.lock().expect("结果锁中毒").clone()
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
            note: "（已持久化）".to_string(),
        })
    }

    fn handle_command(&self, text: &str, _channel: &str) -> Result<Option<String>, String> {
        Ok(Some(format!("宿主收到：{text}")))
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
                            id: Some("c1".to_string()),
                        },
                    }),
                    "tool_finish" => events(TurnEvent::ToolFinished {
                        call: ToolCall {
                            name: step["name"].as_str().unwrap_or("bash").to_string(),
                            arguments: Value::Null,
                            id: Some("c1".to_string()),
                        },
                        result: ToolResult {
                            ok: step["ok"].as_bool().unwrap_or(true),
                            output: step["output"].as_str().unwrap_or("done").to_string(),
                            error_code: None,
                            retryable: false,
                        },
                    }),
                    other => panic!("未知脚本步骤：{other}"),
                }
            }
        }
        if let Some(seconds) = script.get("hold_seconds").and_then(Value::as_f64) {
            std::thread::sleep(Duration::from_secs_f64(seconds));
        }
        if script
            .get("confirm")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            if let Some(handler) = self.confirm.lock().expect("确认锁中毒").clone() {
                let decision = handler.confirm("bash", &json!({"command": "rm -rf"}));
                self.confirm_results
                    .lock()
                    .expect("结果锁中毒")
                    .push(decision);
            }
        }
        Ok(script
            .get("reply")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string())
    }

    fn request_cancel(&self) {
        self.cancel_requested.fetch_add(1, Ordering::SeqCst);
    }

    fn temp_root(&self) -> std::path::PathBuf {
        std::env::temp_dir().join("ocl-feishu-bot-tests")
    }

    fn set_confirm_handler(&self, handler: Arc<dyn ConfirmHandler>) {
        *self.confirm.lock().expect("确认锁中毒") = Some(handler);
    }

    fn set_ask_user_handler(&self, handler: Arc<dyn AskUserHandler>) {
        *self.ask_user.lock().expect("提问锁中毒") = Some(handler);
    }
}

fn bot_with(
    driver: Arc<MockDriver>,
    http: Arc<MockHttp>,
    allowed: &[&str],
) -> Arc<FeishuBot<MockDriver>> {
    let api = Arc::new(FeishuApi::new("cli_x", "secret", http));
    let allowed: BTreeSet<String> = allowed.iter().map(|value| value.to_string()).collect();
    let bot = Arc::new(FeishuBot::new(api, driver, allowed, 5.0));
    bot.bind_handlers();
    bot
}

fn text_event(message_id: &str, open_id: &str, text: &str) -> Value {
    json!({
        "event": {
            "sender": {"sender_id": {"open_id": open_id}},
            "message": {
                "message_id": message_id,
                "chat_id": "oc_1",
                "message_type": "text",
                "create_time": "1700000000",
                "content": json!({"text": text}).to_string(),
            },
        }
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
fn unauthorized_sender_is_ignored() {
    let driver = Arc::new(MockDriver::default());
    let http = Arc::new(MockHttp::default());
    let bot = bot_with(driver.clone(), http.clone(), &["ou_owner"]);
    bot.handle_message(&text_event("m1", "ou_stranger", "你好"));
    assert!(driver.turns().is_empty());
    assert!(http.requests().is_empty(), "未授权用户不应触发任何请求");
}

#[test]
fn duplicate_message_is_claimed_once() {
    let driver = Arc::new(MockDriver::default());
    let http = Arc::new(MockHttp::default());
    let bot = bot_with(driver.clone(), http.clone(), &["ou_owner"]);
    driver.set_script(json!({"reply": "回答"}));
    bot.handle_message(&text_event("m1", "ou_owner", "任务"));
    bot.handle_message(&text_event("m1", "ou_owner", "任务"));
    wait_for(5.0, || driver.turns().len() == 1);
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(driver.turns(), vec!["任务".to_string()]);
}

#[test]
fn task_timeline_creates_text_and_tool_messages() {
    let driver = Arc::new(MockDriver::default());
    let http = Arc::new(MockHttp::default());
    let bot = bot_with(driver.clone(), http.clone(), &["ou_owner"]);
    driver.set_script(json!({
        "events": [
            {"kind": "delta", "text": "正文一"},
            {"kind": "tool_start", "name": "bash"},
            {"kind": "tool_finish", "name": "bash", "ok": true, "output": "done"},
        ],
        "reply": "正文一",
    }));
    bot.handle_message(&text_event("m1", "ou_owner", "任务"));
    wait_for(5.0, || driver.turns().len() == 1);
    wait_for(5.0, || http.created_messages() >= 3);
    let bodies: Vec<String> = http
        .requests()
        .iter()
        .filter(|request| request["method"] == "POST")
        .filter_map(|request| request["body"].as_str().map(|value| value.to_string()))
        .collect();
    assert!(
        bodies.iter().any(|body| body.contains("◇ 正文一")),
        "正文段应独立成卡片：{bodies:?}"
    );
    assert!(
        bodies.iter().any(|body| body.contains("● bash")),
        "工具调用应独立成卡片：{bodies:?}"
    );
}

#[test]
fn commands_are_answered_locally() {
    let driver = Arc::new(MockDriver::default());
    let http = Arc::new(MockHttp::default());
    let bot = bot_with(driver.clone(), http.clone(), &["ou_owner"]);
    bot.dispatch("oc_1", "chat_id", "ou_owner", "/status");
    bot.dispatch("oc_1", "chat_id", "ou_owner", "/thinking on");
    bot.dispatch("oc_1", "chat_id", "ou_owner", "/resume latest");
    wait_for(5.0, || http.created_messages() >= 3);
    let bodies: Vec<String> = http
        .requests()
        .iter()
        .filter_map(|request| request["body"].as_str().map(|value| value.to_string()))
        .collect();
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
fn approval_flow_resolves_through_commands() {
    let driver = Arc::new(MockDriver::default());
    let http = Arc::new(MockHttp::default());
    let bot = bot_with(driver.clone(), http.clone(), &["ou_owner"]);
    driver.set_script(json!({"confirm": true, "reply": "完成"}));
    bot.handle_message(&text_event("m1", "ou_owner", "危险任务"));
    wait_for(5.0, || {
        http.requests().iter().any(|request| {
            request["body"]
                .as_str()
                .unwrap_or("")
                .contains("需要确认执行敏感操作")
        })
    });
    bot.handle_approval("oc_1", "chat_id", "ou_other", true);
    assert!(driver.confirm_results().is_empty(), "非发起人不能批准");
    bot.handle_approval("oc_1", "chat_id", "ou_owner", true);
    wait_for(5.0, || !driver.confirm_results().is_empty());
    assert_eq!(driver.confirm_results(), vec![true]);
}

#[test]
fn ask_user_waits_for_text_answer() {
    let driver = Arc::new(MockDriver::default());
    let http = Arc::new(MockHttp::default());
    let bot = bot_with(driver.clone(), http.clone(), &["ou_owner"]);
    let answer: Arc<Mutex<Option<Option<String>>>> = Arc::new(Mutex::new(None));
    let answer_slot = answer.clone();
    // 题目卡片需要一条正在执行的任务：让宿主回合停住，提问期间任务保持活动。
    driver.set_script(json!({"reply": "完成", "hold_seconds": 2.0}));
    bot.handle_message(&text_event("m1", "ou_owner", "任务"));
    wait_for(5.0, || !driver.turns().is_empty());
    let handler = driver
        .ask_user
        .lock()
        .expect("提问锁中毒")
        .clone()
        .expect("已绑定提问桥");
    let waiter = std::thread::spawn(move || {
        let result = handler.ask(&json!({
            "request_id": "q-1",
            "kind": "select",
            "question": "选哪个？",
            "options": ["A", "B"],
        }));
        *answer_slot.lock().expect("答案锁中毒") = Some(result);
    });
    wait_for(5.0, || {
        http.requests()
            .iter()
            .any(|request| request["body"].as_str().unwrap_or("").contains("选哪个？"))
    });
    let answered = bot.answer_pending_user_question("oc_1", "chat_id", "ou_owner", "B", "text");
    assert!(answered, "文本回答应被挂起提问消费");
    waiter.join().expect("等待线程");
    assert_eq!(
        answer.lock().expect("答案锁中毒").clone(),
        Some(Some("B".to_string()))
    );
}

#[test]
fn card_callback_acknowledges_answer() {
    let driver = Arc::new(MockDriver::default());
    let http = Arc::new(MockHttp::default());
    let bot = bot_with(driver.clone(), http.clone(), &["ou_owner"]);
    let ack = bot.answer_user_question_action(&json!({
        "event": {"action": {"value": {"type": "ask_user", "question_id": "q-1", "answer": "A"}},
                  "operator": {"open_id": "ou_owner"}},
    }));
    assert_eq!(ack["toast"]["content"], "该问题已处理或已失效。");
    let ignored = bot
        .answer_user_question_action(&json!({"event": {"action": {"value": {"type": "other"}}}}));
    assert_eq!(ignored["toast"]["content"], "已忽略该卡片操作。");
}

#[test]
fn cancel_releases_pending_approval() {
    let driver = Arc::new(MockDriver::default());
    let http = Arc::new(MockHttp::default());
    let bot = bot_with(driver.clone(), http.clone(), &["ou_owner"]);
    driver.set_script(json!({"confirm": true, "reply": "完成"}));
    bot.handle_message(&text_event("m1", "ou_owner", "危险任务"));
    wait_for(5.0, || {
        http.requests().iter().any(|request| {
            request["body"]
                .as_str()
                .unwrap_or("")
                .contains("需要确认执行敏感操作")
        })
    });
    // 取消任务同时按拒绝释放挂起的确认，任务线程不会等到超时。
    bot.request_cancel("oc_1", "chat_id");
    wait_for(5.0, || !driver.confirm_results().is_empty());
    assert_eq!(driver.confirm_results(), vec![false]);
    assert!(driver.cancel_requested.load(Ordering::SeqCst) >= 1);
}

#[test]
fn generated_file_marker_uploads_and_sends_image_message() {
    let driver = Arc::new(MockDriver::default());
    let http = Arc::new(MockHttp::default());
    let bot = bot_with(driver.clone(), http.clone(), &["ou_owner"]);

    let dir = std::env::temp_dir().join(format!("omnicrawl-bot-file-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("创建临时目录");
    let path = dir.join("out.png");
    std::fs::write(&path, b"PNG").expect("写临时图片");
    driver.set_script(json!({"reply": format!("结果见文件 [FILE:{}]", path.display())}));

    bot.handle_message(&text_event("m1", "ou_owner", "生成图片"));
    // 负载是 Python 风格的 `json.dumps`（分隔符带空格），断言要照着这个形状写。
    wait_for(8.0, || {
        http.requests().iter().any(|request| {
            request["body"]
                .as_str()
                .unwrap_or("")
                .contains("\"msg_type\": \"image\"")
        })
    });

    let requests = http.requests();
    assert!(
        requests
            .iter()
            .any(|request| request["body"].as_str().unwrap_or("").contains("PNG")),
        "上传正文应带上文件内容"
    );
    assert!(
        requests.iter().any(|request| request["url"]
            .as_str()
            .unwrap_or("")
            .contains("/im/v1/images")),
        "图片应走上传接口"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
