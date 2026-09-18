//! 飞书连接器服务：事件接入、命令分发、任务执行时间线与长连接主循环。
//!
//! 语义基准是 Python `omnicrawl/connectors/fsapp.py` 的 `FeishuBot`：`im.message.receive_v1`
//! 事件先白名单与去重，再回答挂起的提问、分发命令或排队执行任务；任务执行期间每个条目
//! （正文段、工具调用、思考、执行计划、子任务进度）独立成一条消息、按发生顺序出现。
//!
//! 与 Python 的差别有两处，都记在 crate README 的「尚未移植」里：入站队列是进程内的
//! （没有 `pending.jsonl` 的跨重启重放），文件上传与 `[FILE:]` 标记的发文件未搬。

use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use omnicrawl_session::{redact_sensitive_text, redact_sensitive_values};

use super::api::FeishuApi;
use super::dedupe::{inbox_dedupe_key, SeenMessages};
use super::files::{
    post_text_and_images, resolve_temp_destination, resource_file_key, resource_file_name,
    MESSAGE_RESOURCE_TYPES,
};
use super::render::{
    question_card_json, question_resolved_card_json, ASK_USER_TOOL_NAME, TODO_TOOL_NAME,
};
use super::text::{display_text, parse_json_object, resolve_final_text, text_value};
use super::timeline::{
    MessagePort, PlanMessage, ReasoningMessage, SubAgentMessage, TextMessage, ToolMessage,
    ToolRecord,
};
use crate::agent::{
    AgentDriver, AskUserHandler, ConfirmHandler, ToolCall, ToolResult, TurnError, TurnEvent,
};
use crate::json;

/// 流式 patch 节拍（秒）：飞书对消息更新有频率限制，不做逐字 patch。
pub const STREAM_PATCH_INTERVAL_SECONDS: f64 = 1.5;

/// 没有收到 `message_id` 时使用的兜底去重键前缀。
const UNKNOWN_SENDER: &str = "(unknown)";

/// 一次等待用户批准的工具调用。
struct PendingConfirmation {
    tool_name: String,
    receive_id: String,
    receive_id_type: String,
    sender_open_id: String,
    decision: Mutex<Option<bool>>,
    signal: Condvar,
}

impl PendingConfirmation {
    fn wait(&self, timeout: Duration) -> Option<bool> {
        let guard = self.decision.lock().expect("审批锁中毒");
        let (guard, _) = self
            .signal
            .wait_timeout_while(guard, timeout, |decision| decision.is_none())
            .expect("审批锁中毒");
        *guard
    }

    fn resolve(&self, decision: bool) {
        let mut guard = self.decision.lock().expect("审批锁中毒");
        if guard.is_none() {
            *guard = Some(decision);
        }
        self.signal.notify_all();
    }
}

/// 一次等待飞书用户回答的 `ask_user` 请求。
struct PendingQuestion {
    question_id: String,
    kind: String,
    question: String,
    options: Vec<String>,
    receive_id: String,
    receive_id_type: String,
    sender_open_id: String,
    message_id: Mutex<Option<String>>,
    answer: Mutex<Option<String>>,
    signal: Condvar,
}

impl PendingQuestion {
    fn resolve(&self, answer: Option<String>) {
        let mut guard = self.answer.lock().expect("提问锁中毒");
        if guard.is_none() {
            *guard = answer;
        }
        self.signal.notify_all();
    }

    fn wait(&self, timeout: Duration) -> Option<String> {
        let guard = self.answer.lock().expect("提问锁中毒");
        let (guard, _) = self
            .signal
            .wait_timeout_while(guard, timeout, |answer| answer.is_none())
            .expect("提问锁中毒");
        guard.clone()
    }
}

/// 当前唯一活动任务的最小状态。
pub struct ActiveTask {
    pub receive_id: String,
    pub receive_id_type: String,
    pub sender_open_id: String,
    pub text: String,
    pub dedupe_key: String,
    cancel: AtomicBool,
    running: AtomicBool,
    started_at: Instant,
}

impl ActiveTask {
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::SeqCst)
    }

    fn elapsed_seconds(&self) -> f64 {
        self.started_at.elapsed().as_secs_f64()
    }
}

/// 排队等待执行的任务（进程内队列）。
struct QueuedTask {
    receive_id: String,
    receive_id_type: String,
    sender_open_id: String,
    text: String,
    dedupe_key: String,
}

#[derive(Default)]
struct BotState {
    active: Option<Arc<ActiveTask>>,
    queue: VecDeque<QueuedTask>,
    confirmation: Option<Arc<PendingConfirmation>>,
    question: Option<Arc<PendingQuestion>>,
    seen: SeenMessages,
}

/// 通过飞书长连接远程驱动一个 Agent 的轮询服务。
pub struct FeishuBot<D: AgentDriver> {
    api: Arc<FeishuApi>,
    driver: Arc<D>,
    allowed_user_ids: std::collections::BTreeSet<String>,
    public_access: bool,
    confirmation_timeout: Duration,
    state: Mutex<BotState>,
    stopped: AtomicBool,
    show_thinking: AtomicBool,
    temp_root: PathBuf,
}

impl<D: AgentDriver> FeishuBot<D> {
    /// 构造连接器：临时目录与工作区根来自宿主（收到文件后按类型落盘）。
    pub fn new(
        api: Arc<FeishuApi>,
        driver: Arc<D>,
        allowed_user_ids: std::collections::BTreeSet<String>,
        confirmation_timeout_seconds: f64,
    ) -> FeishuBot<D> {
        FeishuBot {
            temp_root: driver.temp_root(),
            api,
            driver,
            public_access: allowed_user_ids.is_empty() || allowed_user_ids.contains("*"),
            allowed_user_ids,
            confirmation_timeout: Duration::from_secs_f64(confirmation_timeout_seconds.max(1.0)),
            state: Mutex::new(BotState::default()),
            stopped: AtomicBool::new(false),
            show_thinking: AtomicBool::new(false),
        }
    }

    /// 把自身注册成宿主的工具确认桥与提问桥。
    pub fn bind_handlers(self: &Arc<Self>) {
        self.driver.set_confirm_handler(self.clone());
        self.driver.set_ask_user_handler(self.clone());
    }

    /// 建立长连接并进入事件循环（断线按对端配置退避重连）。
    pub fn run(self: Arc<Self>) -> Result<(), String> {
        self.bind_handlers();
        self.run_forever()
    }

    /// 长连接主循环：端点 → 连接 → 收帧 → 处理 → 回执，失败后按策略重连。
    pub fn run_forever(self: &Arc<Self>) -> Result<(), String> {
        if self.public_access {
            eprintln!("[feishu] allowed_user_ids 为空或包含 *，当前为公开访问模式。");
        }
        let mut attempt = 0_u64;
        while !self.is_stopped() {
            let outcome = self.run_session();
            if self.is_stopped() {
                break;
            }
            match outcome {
                Ok(()) => break,
                Err(error) => {
                    attempt += 1;
                    let policy = self.reconnect_policy();
                    let wait = policy.backoff_seconds(attempt);
                    eprintln!("[feishu] 长连接中断（第 {attempt} 次）：{error}，{wait} 秒后重连");
                    if policy.max_attempts != 0 && attempt >= policy.max_attempts {
                        return Err(format!("长连接重连 {attempt} 次仍失败：{error}"));
                    }
                    self.sleep_until_stopped(wait as f64);
                }
            }
        }
        Ok(())
    }

    fn run_session(self: &Arc<Self>) -> Result<(), String> {
        let mut connection = super::ws::connect(&self.api).map_err(|error| error.to_string())?;
        eprintln!(
            "[feishu] 长连接已建立（service_id={}）",
            connection.config.service_id
        );
        let ping_interval = super::ws::ping_interval_seconds(&connection.config);
        let mut next_ping = Instant::now() + Duration::from_secs(ping_interval);
        loop {
            if self.is_stopped() {
                return Ok(());
            }
            if Instant::now() >= next_ping {
                let service_id: i64 = connection.config.service_id.parse().unwrap_or(0);
                connection.send_ping(service_id)?;
                next_ping = Instant::now() + Duration::from_secs(ping_interval);
            }
            let frame = match connection.next_data_frame() {
                Ok(Some(frame)) => frame,
                Ok(None) => continue,
                Err(error) => return Err(error),
            };
            let started = Instant::now();
            let (ok, ack) = self.handle_frame(&frame);
            connection.respond(&frame, ok, Some(started.elapsed().as_millis() as i64), ack)?;
        }
    }

    /// 处理一个数据帧：`event` 交给消息接入，`card` 交给卡片回调。
    fn handle_frame(self: &Arc<Self>, frame: &super::ws::pbbp2::Frame) -> (bool, Option<String>) {
        let payload = match serde_json::from_slice::<Value>(&frame.payload) {
            Ok(payload) => payload,
            Err(error) => {
                eprintln!("[feishu] 无法解析事件负载：{error}");
                return (false, None);
            }
        };
        match frame.header(super::ws::pbbp2::HEADER_TYPE).as_str() {
            super::ws::pbbp2::MESSAGE_TYPE_EVENT => {
                self.handle_message(&payload);
                (true, None)
            }
            super::ws::pbbp2::MESSAGE_TYPE_CARD => {
                let ack = self.answer_user_question_action(&payload);
                (true, Some(json::dumps(&ack)))
            }
            other => {
                eprintln!("[feishu] 忽略未知帧类型：{other}");
                (true, None)
            }
        }
    }

    /// 供测试与信号处理触发退出。
    pub fn request_stop(&self) {
        self.stopped.store(true, Ordering::SeqCst);
    }

    pub fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::SeqCst)
    }

    pub fn show_thinking(&self) -> bool {
        self.show_thinking.load(Ordering::SeqCst)
    }

    /// `im.message.receive_v1` 事件入口。
    pub fn handle_message(self: &Arc<Self>, data: &Value) {
        let event = data.get("event").cloned().unwrap_or(Value::Null);
        let message = event.get("message").cloned().unwrap_or(Value::Null);
        if message.is_null() {
            return;
        }
        let message_id = field_text(&message, "message_id");
        let now = unix_seconds_f64();
        {
            let mut state = self.state.lock().expect("状态锁中毒");
            if !state.seen.claim(&message_id, now) {
                eprintln!("[feishu] 忽略重复飞书消息：{message_id}");
                return;
            }
        }
        let open_id = sender_open_id(&event);
        if !self.allows(&open_id) {
            eprintln!(
                "[feishu] 忽略未授权飞书用户：{}",
                if open_id.is_empty() {
                    UNKNOWN_SENDER
                } else {
                    open_id.as_str()
                }
            );
            return;
        }
        let chat_id = field_text(&message, "chat_id");
        let receive_id = if chat_id.is_empty() {
            open_id.clone()
        } else {
            chat_id
        };
        let receive_id_type = if receive_id == open_id && !open_id.is_empty() {
            "open_id".to_string()
        } else {
            "chat_id".to_string()
        };
        let message_type = field_text(&message, "message_type");
        let (user_text, _local_files) = self.build_user_message(&message);
        if self.answer_pending_user_question(
            &receive_id,
            &receive_id_type,
            &open_id,
            &user_text,
            &message_type,
        ) {
            return;
        }
        if user_text.is_empty() {
            self.api.send_text(
                &receive_id,
                &format!("⚠️ 暂不支持处理此类飞书消息：{message_type}"),
                &receive_id_type,
            );
            return;
        }
        eprintln!(
            "[feishu] 收到飞书消息（user={}, type={}）：{}",
            if open_id.is_empty() {
                UNKNOWN_SENDER
            } else {
                open_id.as_str()
            },
            message_type,
            take_chars(&user_text, 200)
        );
        if message_type == "text" && user_text.starts_with('/') {
            let bot = self.clone();
            let receive = receive_id.clone();
            let kind = receive_id_type.clone();
            let sender = open_id.clone();
            let text = user_text.clone();
            let _ = thread::Builder::new()
                .name("omnicrawl-feishu-command".to_string())
                .spawn(move || bot.dispatch(&receive, &kind, &sender, &text));
            return;
        }
        let dedupe_key = inbox_dedupe_key(
            &message_type,
            &message_id,
            &field_text(&message, "create_time"),
            &field_text(&message, "chat_id"),
            &open_id,
            &user_text,
        );
        self.enqueue(QueuedTask {
            receive_id: receive_id.clone(),
            receive_id_type: receive_id_type.clone(),
            sender_open_id: open_id.clone(),
            text: user_text,
            dedupe_key,
        });
        self.api.send_text(
            &receive_id,
            "📥 已收到任务，正在排队执行（/cancel 可取消，发送 /status 查看队列）。",
            &receive_id_type,
        );
        self.pump();
    }

    /// 入队：同一 `dedupe_key` 已在队列或正在执行时丢弃。
    fn enqueue(&self, task: QueuedTask) {
        let mut state = self.state.lock().expect("状态锁中毒");
        if let Some(active) = &state.active {
            if active.dedupe_key == task.dedupe_key {
                eprintln!("[feishu] 忽略已处理的飞书消息：{}", task.dedupe_key);
                return;
            }
        }
        if state
            .queue
            .iter()
            .any(|item| item.dedupe_key == task.dedupe_key)
        {
            eprintln!("[feishu] 忽略已排队的飞书消息：{}", task.dedupe_key);
            return;
        }
        state.queue.push_back(task);
    }

    /// 空闲时取出队首任务执行；任务执行期间到达的消息排队等待。
    pub fn pump(self: &Arc<Self>) {
        let task = {
            let mut state = self.state.lock().expect("状态锁中毒");
            let busy = state
                .active
                .as_ref()
                .map(|active| active.is_running())
                .unwrap_or(false);
            if busy || self.is_stopped() {
                return;
            }
            match state.queue.pop_front() {
                Some(queued) => {
                    let task = Arc::new(ActiveTask {
                        receive_id: queued.receive_id,
                        receive_id_type: queued.receive_id_type,
                        sender_open_id: queued.sender_open_id,
                        text: queued.text,
                        dedupe_key: queued.dedupe_key,
                        cancel: AtomicBool::new(false),
                        running: AtomicBool::new(true),
                        started_at: Instant::now(),
                    });
                    state.active = Some(task.clone());
                    task
                }
                None => return,
            }
        };
        let bot = self.clone();
        let spawned = thread::Builder::new()
            .name("omnicrawl-feishu-task".to_string())
            .spawn(move || bot.execute_task(task));
        if let Err(error) = spawned {
            eprintln!("[feishu] 任务线程启动失败：{error}");
            self.state.lock().expect("状态锁中毒").active = None;
        }
    }

    /// 任务线程：驱动回合、把事件映射成时间线条目，并保留已展示的正文段。
    fn execute_task(self: &Arc<Self>, task: Arc<ActiveTask>) {
        let mut display = TaskTimeline::new(self.clone(), &task);
        let result = {
            let mut events = |event: TurnEvent| display.handle(event);
            self.driver.run_turn(&task.text, &mut events)
        };
        let cancelled = task.is_cancelled() || self.is_stopped();
        self.finish_task(&task, &mut display, result, cancelled);
        task.running.store(false, Ordering::SeqCst);
        {
            let mut state = self.state.lock().expect("状态锁中毒");
            let same = state
                .active
                .as_ref()
                .map(|active| Arc::ptr_eq(active, &task))
                .unwrap_or(false);
            if same {
                state.active = None;
            }
            if let Some(confirmation) = state.confirmation.clone() {
                if confirmation.receive_id == task.receive_id {
                    state.confirmation = None;
                }
            }
            if let Some(question) = state.question.clone() {
                if question.receive_id == task.receive_id {
                    state.question = None;
                }
            }
        }
        self.pump();
    }

    fn finish_task(
        self: &Arc<Self>,
        task: &ActiveTask,
        display: &mut TaskTimeline<D>,
        result: Result<String, TurnError>,
        cancelled: bool,
    ) {
        if cancelled {
            display.abort_running_tools();
            let partial = display.cleaned_deltas();
            if display.text_message_created() && !partial.is_empty() {
                display.seal_text(None, "\n\n⏹ 输出已中断，任务已取消。");
            } else {
                self.api
                    .send_text(&task.receive_id, "⏹ 任务已取消。", &task.receive_id_type);
            }
            return;
        }
        match result {
            Ok(reply) => {
                let raw_reply = if reply.is_empty() {
                    display.all_deltas()
                } else {
                    reply
                };
                if self.show_thinking() && display.has_reasoning_message() {
                    display.seal_reasoning();
                }
                if display.text_message_created() {
                    let streamed = display.deltas();
                    let final_text = resolve_final_text(&streamed, &display_text(&raw_reply));
                    display.seal_text(Some(final_text), "");
                } else if !raw_reply.is_empty() || !display.streamed_any() {
                    let content = display_text(&raw_reply);
                    let remaining = self.send_text_segment(task, &content);
                    if let Some(remaining) = remaining {
                        self.api
                            .send_text(&task.receive_id, &remaining, &task.receive_id_type);
                    }
                }
            }
            Err(TurnError::Cancelled) => {
                display.abort_running_tools();
                self.api
                    .send_text(&task.receive_id, "⏹ 任务已取消。", &task.receive_id_type);
            }
            Err(TurnError::Failed(message)) => {
                display.abort_running_tools();
                display.seal_text(None, "");
                eprintln!("[feishu] Agent 任务失败：{message}");
                let error = format!("❌ 任务执行失败：{}", redact_sensitive_text(&message));
                self.api
                    .send_text(&task.receive_id, &error, &task.receive_id_type);
            }
        }
    }

    /// 末段没有流式正文时补发一条文本消息；返回仍需补发的剩余内容。
    fn send_text_segment(&self, task: &ActiveTask, content: &str) -> Option<String> {
        let mut message =
            TextMessage::new(self.api.clone(), &task.receive_id, &task.receive_id_type);
        message.seal(content, "")
    }

    /// 处理飞书命令；未知命令回提示。
    pub fn dispatch(
        self: &Arc<Self>,
        receive_id: &str,
        receive_id_type: &str,
        sender_open_id: &str,
        text: &str,
    ) {
        let command = text
            .split_whitespace()
            .next()
            .unwrap_or("")
            .split('@')
            .next()
            .unwrap_or("")
            .to_lowercase();
        match command.as_str() {
            "/start" | "/help" => {
                self.api.send_text(receive_id, HELP_TEXT, receive_id_type);
            }
            "/status" => {
                let status = self.status_text();
                self.api.send_text(receive_id, &status, receive_id_type);
            }
            "/session" => {
                let session_id = self.driver.status().session_id;
                let shown = if session_id.is_empty() {
                    "（尚未创建）".to_string()
                } else {
                    session_id
                };
                self.api.send_text(
                    receive_id,
                    &format!("当前会话 ID：{shown}"),
                    receive_id_type,
                );
            }
            "/reset" | "/new" => self.reset_conversation(receive_id, receive_id_type),
            "/cancel" => self.request_cancel(receive_id, receive_id_type),
            "/approve" | "/reject" => {
                self.handle_approval(
                    receive_id,
                    receive_id_type,
                    sender_open_id,
                    command == "/approve",
                );
            }
            "/thinking" => self.handle_thinking(receive_id, receive_id_type, text),
            "/workspace" => self.handle_workspace(receive_id, receive_id_type, text),
            _ => {
                let reply = match self.driver.handle_command(text, "feishu") {
                    Ok(Some(reply)) => reply,
                    Ok(None) => "未知命令。发送 /start 查看可用命令。".to_string(),
                    Err(error) => {
                        eprintln!("[feishu] 执行飞书命令失败：{text}");
                        format!("❌ 命令执行失败：{}", redact_sensitive_text(&error))
                    }
                };
                self.api.send_text(receive_id, &reply, receive_id_type);
            }
        }
    }

    fn reset_conversation(self: &Arc<Self>, receive_id: &str, receive_id_type: &str) {
        let busy = {
            let state = self.state.lock().expect("状态锁中毒");
            state
                .active
                .as_ref()
                .map(|task| task.is_running())
                .unwrap_or(false)
        };
        if busy {
            self.api.send_text(
                receive_id,
                "当前有任务正在执行，请先 /cancel 后再开启新会话。",
                receive_id_type,
            );
            return;
        }
        match self.driver.reset_conversation() {
            Ok(()) => {
                self.api.send_text(
                    receive_id,
                    "已开启新会话（对话历史已清空）。",
                    receive_id_type,
                );
            }
            Err(error) => {
                self.api.send_text(
                    receive_id,
                    &format!("❌ 重置会话失败：{}", redact_sensitive_text(&error)),
                    receive_id_type,
                );
            }
        }
    }

    /// 请求取消当前任务；只有发起任务的会话可以取消。
    pub fn request_cancel(self: &Arc<Self>, receive_id: &str, receive_id_type: &str) {
        let (task, confirmation, question) = {
            let state = self.state.lock().expect("状态锁中毒");
            (
                state.active.clone(),
                state.confirmation.clone(),
                state.question.clone(),
            )
        };
        let Some(task) = task.filter(|task| task.is_running()) else {
            self.api
                .send_text(receive_id, "当前没有正在执行的任务。", receive_id_type);
            return;
        };
        if task.receive_id != receive_id {
            self.api.send_text(
                receive_id,
                "只有发起当前任务的会话可以取消它。",
                receive_id_type,
            );
            return;
        }
        task.cancel.store(true, Ordering::SeqCst);
        self.driver.request_cancel();
        if let Some(confirmation) = confirmation {
            if confirmation.receive_id == receive_id {
                confirmation.resolve(false);
            }
        }
        if let Some(question) = question {
            if question.receive_id == receive_id {
                question.resolve(None);
            }
        }
        self.api.send_text(
            receive_id,
            "⏹ 已请求取消当前任务，请稍候……",
            receive_id_type,
        );
    }

    /// `/approve` 与 `/reject`：仅发起任务的白名单用户本人可批准或拒绝。
    pub fn handle_approval(
        self: &Arc<Self>,
        receive_id: &str,
        receive_id_type: &str,
        sender_open_id: &str,
        approve: bool,
    ) {
        let message = {
            let state = self.state.lock().expect("状态锁中毒");
            match state.confirmation.clone() {
                None => "当前没有等待确认的工具调用。".to_string(),
                Some(pending) => {
                    if pending.receive_id != receive_id || pending.sender_open_id != sender_open_id
                    {
                        "只有发起当前任务的用户可以批准或拒绝该操作。".to_string()
                    } else {
                        pending.resolve(approve);
                        format!(
                            "✅ {}：{}",
                            if approve { "已批准" } else { "已拒绝" },
                            pending.tool_name
                        )
                    }
                }
            }
        };
        self.api.send_text(receive_id, &message, receive_id_type);
    }

    /// `/thinking`：查看或切换思考内容显示。
    pub fn handle_thinking(self: &Arc<Self>, receive_id: &str, receive_id_type: &str, text: &str) {
        let parts: Vec<&str> = text.split_whitespace().collect();
        if parts.len() == 1 {
            let state = if self.show_thinking() {
                "开启"
            } else {
                "关闭"
            };
            self.api.send_text(
                receive_id,
                &format!(
                    "思考内容显示：{state}（默认关闭）。\n/thinking on 开启，/thinking off 关闭。"
                ),
                receive_id_type,
            );
            return;
        }
        match parts[1].to_lowercase().as_str() {
            "on" | "1" | "true" | "yes" | "开" | "开启" => {
                self.show_thinking.store(true, Ordering::SeqCst);
                self.api.send_text(
                    receive_id,
                    "已开启思考内容显示（💭 折叠面板）。",
                    receive_id_type,
                );
            }
            "off" | "0" | "false" | "no" | "关" | "关闭" => {
                self.show_thinking.store(false, Ordering::SeqCst);
                self.api
                    .send_text(receive_id, "已关闭思考内容显示。", receive_id_type);
            }
            _ => {
                self.api.send_text(
                    receive_id,
                    "用法：/thinking on 或 /thinking off。",
                    receive_id_type,
                );
            }
        }
    }

    /// `/workspace`：查看或切换工作区（持久化由宿主负责）。
    pub fn handle_workspace(self: &Arc<Self>, receive_id: &str, receive_id_type: &str, text: &str) {
        let trimmed = text.trim_start();
        let argument = trimmed
            .split_once(char::is_whitespace)
            .map(|(_command, value)| value.trim());
        let Some(path) = argument.filter(|value| !value.is_empty()) else {
            let root = self.driver.status().workspace_root;
            self.api.send_text(
                receive_id,
                &format!("当前工作区：{root}\n用法：/workspace <路径>"),
                receive_id_type,
            );
            return;
        };
        match self.driver.switch_workspace(path) {
            Ok(switched) => {
                self.api.send_text(
                    receive_id,
                    &format!(
                        "✅ 已切换工作区：{}{}",
                        switched.workspace_root, switched.note
                    ),
                    receive_id_type,
                );
            }
            Err(error) => {
                self.api.send_text(
                    receive_id,
                    &format!("❌ 切换工作区失败：{}", redact_sensitive_text(&error)),
                    receive_id_type,
                );
            }
        }
    }

    fn status_text(&self) -> String {
        let status = self.driver.status();
        let state = self.state.lock().expect("状态锁中毒");
        let busy = state
            .active
            .as_ref()
            .map(|task| task.is_running())
            .unwrap_or(false);
        let mut lines = vec![
            "📊 OmniCrawl 状态".to_string(),
            format!("工作区：{}", status.workspace_root),
            format!("会话 ID：{}", status.session_id),
            format!(
                "状态：{}",
                if busy {
                    "🔄 正在执行任务"
                } else {
                    "✅ 空闲"
                }
            ),
            format!("队列：{} 条等待", state.queue.len()),
        ];
        if busy {
            if let Some(task) = &state.active {
                lines.push(format!(
                    "任务已运行 {} 秒，/cancel 可取消。",
                    task.elapsed_seconds() as i64
                ));
            }
        }
        lines.join("\n")
    }

    fn allows(&self, open_id: &str) -> bool {
        self.public_access || (!open_id.is_empty() && self.allowed_user_ids.contains(open_id))
    }

    /// 把飞书消息转成 Agent 可理解的文本；资源先落盘再给出相对路径。
    pub fn build_user_message(&self, message: &Value) -> (String, Vec<PathBuf>) {
        let message_type = field_text(message, "message_type");
        let message_id = field_text(message, "message_id");
        let content = parse_json_object(&message.get("content").cloned().unwrap_or(Value::Null));
        let mut parts: Vec<String> = Vec::new();
        let mut local_files: Vec<PathBuf> = Vec::new();
        match message_type.as_str() {
            "text" => {
                let text = text_value(content.get("text").unwrap_or(&Value::Null));
                if !text.is_empty() {
                    parts.push(text);
                }
            }
            "post" => {
                let (text, image_keys) = post_text_and_images(&content);
                if !text.is_empty() {
                    parts.push(text);
                }
                for image_key in image_keys {
                    match self.save_message_resource(
                        &message_id,
                        &json!({"image_key": image_key}),
                        "image",
                    ) {
                        Some(path) => local_files.push(path),
                        None => parts.push("[飞书图片下载失败]".to_string()),
                    }
                }
            }
            other if MESSAGE_RESOURCE_TYPES.contains(&other) => {
                match self.save_message_resource(&message_id, &content, other) {
                    Some(path) => local_files.push(path),
                    None => parts.push(format!("[飞书 {other} 下载失败]")),
                }
            }
            "share_chat"
            | "share_user"
            | "interactive"
            | "share_calendar_event"
            | "system"
            | "merge_forward" => parts.push(format!("[飞书消息类型：{message_type}]")),
            other => parts.push(format!(
                "[飞书消息类型：{}]",
                if other.is_empty() { "unknown" } else { other }
            )),
        }
        for path in &local_files {
            let display_path =
                relative_display(path, &PathBuf::from(self.driver.status().workspace_root));
            parts.push(format!(
                "已收到飞书文件：位于 {display_path}。如需分析图片或文件，请使用可用工具读取该路径。"
            ));
        }
        (
            parts
                .into_iter()
                .filter(|part| !part.is_empty())
                .collect::<Vec<String>>()
                .join("\n")
                .trim()
                .to_string(),
            local_files,
        )
    }

    /// 下载消息资源并落到 `.agent_tmp`。
    fn save_message_resource(
        &self,
        message_id: &str,
        content: &Value,
        message_type: &str,
    ) -> Option<PathBuf> {
        let file_key = resource_file_key(content)?;
        let (data, name) = self
            .api
            .download_resource(message_id, &file_key, message_type)
            .map_err(|error| eprintln!("[feishu] 下载消息资源失败：{error}"))
            .ok()?;
        let name = resource_file_name(&name, &file_key, message_type);
        let destination = resolve_temp_destination(&self.temp_root, &name)
            .map_err(|error| eprintln!("[feishu] 落盘失败：{error}"))
            .ok()?;
        std::fs::write(&destination, data)
            .map_err(|error| eprintln!("[feishu] 写入失败：{error}"))
            .ok()?;
        Some(destination)
    }

    /// 文本消息分派为当前 `ask_user` 的回答；返回是否已消费该消息。
    pub fn answer_pending_user_question(
        self: &Arc<Self>,
        receive_id: &str,
        receive_id_type: &str,
        sender_open_id: &str,
        text: &str,
        message_type: &str,
    ) -> bool {
        if message_type != "text" {
            return false;
        }
        let question = {
            let state = self.state.lock().expect("状态锁中毒");
            match state.question.clone() {
                None => return false,
                Some(question) => question,
            }
        };
        if question.receive_id != receive_id || question.sender_open_id != sender_open_id {
            return false;
        }
        let answer = text.trim().to_string();
        if answer.to_lowercase() == "/cancel" {
            question.resolve(None);
            if let Some(task) = self.state.lock().expect("状态锁中毒").active.clone() {
                task.cancel.store(true, Ordering::SeqCst);
            }
            self.driver.request_cancel();
            self.api.send_text(
                receive_id,
                "⏹ 已请求取消当前任务，请稍候……",
                receive_id_type,
            );
            return true;
        }
        if answer.is_empty() {
            self.api
                .send_text(receive_id, "请发送非空回答。", receive_id_type);
            return true;
        }
        if question.kind == "select" && !question.options.contains(&answer) {
            self.api.send_text(
                receive_id,
                "请点击问题卡片中的选项按钮作答。",
                receive_id_type,
            );
            return true;
        }
        question.resolve(Some(answer));
        self.api
            .send_text(receive_id, "✅ 已收到回答。", receive_id_type);
        true
    }

    /// 交互卡片回调：返回 ACK 字典（含 toast），形状与 SDK 期望一致。
    pub fn answer_user_question_action(&self, data: &Value) -> Value {
        let payload = data.get("event").unwrap_or(data);
        let value = payload
            .get("action")
            .and_then(|action| action.get("value"))
            .map(parse_json_object)
            .unwrap_or_else(|| parse_json_object(&Value::Null));
        if value.get("type").and_then(Value::as_str) != Some("ask_user") {
            return json!({"toast": {"type": "info", "content": "已忽略该卡片操作。"}});
        }
        let question_id = field_text(&value, "question_id");
        let answer = field_text(&value, "answer");
        let open_id = payload
            .get("operator")
            .or_else(|| payload.get("user"))
            .map(|operator| field_text(operator, "open_id"))
            .unwrap_or_default();
        let (message, reply_to, answered) = {
            let state = self.state.lock().expect("状态锁中毒");
            match state.question.clone() {
                None => ("该问题已处理或已失效。".to_string(), None, false),
                Some(question) if question.question_id != question_id => {
                    ("该问题已处理或已失效。".to_string(), None, false)
                }
                Some(question) if question.sender_open_id != open_id => (
                    "只有发起任务的用户可以回答该问题。".to_string(),
                    Some(question),
                    false,
                ),
                Some(question)
                    if answer.is_empty()
                        || (question.kind == "select" && !question.options.contains(&answer)) =>
                {
                    ("无效的提问选项。".to_string(), Some(question), false)
                }
                Some(question) => {
                    question.resolve(Some(answer));
                    ("✅ 已收到回答。".to_string(), Some(question), true)
                }
            }
        };
        if let Some(question) = &reply_to {
            self.api
                .send_text(&question.receive_id, &message, &question.receive_id_type);
        }
        json!({
            "toast": {"type": if answered { "success" } else { "info" }, "content": message},
        })
    }

    fn reconnect_policy(&self) -> super::ws::ReconnectPolicy {
        super::ws::ReconnectPolicy::from_config(&super::api::ClientConfig::default())
    }

    fn sleep_until_stopped(&self, seconds: f64) {
        let deadline = Instant::now() + Duration::from_secs_f64(seconds.max(0.0));
        while Instant::now() < deadline {
            if self.is_stopped() {
                return;
            }
            thread::sleep(Duration::from_millis(50));
        }
    }

    /// 关闭连接器：停止任务、释放挂起审批与提问，并通知宿主回收资源。
    pub fn close(&self) {
        self.stopped.store(true, Ordering::SeqCst);
        let (task, confirmation, question) = {
            let state = self.state.lock().expect("状态锁中毒");
            (
                state.active.clone(),
                state.confirmation.clone(),
                state.question.clone(),
            )
        };
        if let Some(task) = task {
            task.cancel.store(true, Ordering::SeqCst);
            self.driver.request_cancel();
        }
        if let Some(confirmation) = confirmation {
            confirmation.resolve(false);
        }
        if let Some(question) = question {
            question.resolve(None);
        }
        if let Err(error) = self.driver.shutdown() {
            eprintln!("[feishu] 关闭 Agent 失败：{error}");
        }
    }
}

impl<D: AgentDriver> ConfirmHandler for FeishuBot<D> {
    /// 把工具确认转发到飞书，等 `/approve` 或 `/reject`，超时按拒绝。
    fn confirm(&self, tool_name: &str, arguments: &Value) -> bool {
        let task = {
            let state = self.state.lock().expect("状态锁中毒");
            match state.active.clone() {
                Some(task) if task.is_running() && !task.is_cancelled() => task,
                _ => return false,
            }
        };
        if self.is_stopped() {
            return false;
        }
        let pending = Arc::new(PendingConfirmation {
            tool_name: tool_name.to_string(),
            receive_id: task.receive_id.clone(),
            receive_id_type: task.receive_id_type.clone(),
            sender_open_id: task.sender_open_id.clone(),
            decision: Mutex::new(None),
            signal: Condvar::new(),
        });
        {
            let mut state = self.state.lock().expect("状态锁中毒");
            state.confirmation = Some(pending.clone());
        }
        let safe_arguments = redact_sensitive_values(arguments);
        let detail = json::dumps_indent(&safe_arguments);
        let prompt = format!(
            "⚠️ 需要确认执行敏感操作\n工具：{tool_name}\n参数：\n{detail}\n回复 /approve 允许，/reject 拒绝。{} 秒内未回复将自动拒绝。",
            self.confirmation_timeout.as_secs()
        );
        self.api
            .send_text(&pending.receive_id, &prompt, &pending.receive_id_type);
        let decided = pending.wait(self.confirmation_timeout);
        {
            let mut state = self.state.lock().expect("状态锁中毒");
            let same = state
                .confirmation
                .as_ref()
                .map(|current| Arc::ptr_eq(current, &pending))
                .unwrap_or(false);
            if same {
                state.confirmation = None;
            }
        }
        match decided {
            Some(decision) => decision,
            None => {
                self.api.send_text(
                    &pending.receive_id,
                    "⏰ 确认超时，已自动拒绝该操作。",
                    &pending.receive_id_type,
                );
                false
            }
        }
    }
}

impl<D: AgentDriver> AskUserHandler for FeishuBot<D> {
    /// 发送飞书提问并阻塞当前任务，直到文本或卡片回答到达。
    fn ask(&self, request: &Value) -> Option<String> {
        let task = {
            let state = self.state.lock().expect("状态锁中毒");
            match state.active.clone() {
                Some(task) if !task.is_cancelled() => task,
                _ => return None,
            }
        };
        if self.is_stopped() {
            return None;
        }
        let question_id = {
            let provided = field_text(request, "request_id");
            if provided.is_empty() {
                format!("question-{}", unix_nanos())
            } else {
                provided
            }
        };
        let kind = {
            let provided = field_text(request, "kind");
            if provided.is_empty() {
                "question".to_string()
            } else {
                provided
            }
        };
        let question_text = field_text(request, "question").trim().to_string();
        let options: Vec<String> = request
            .get("options")
            .and_then(Value::as_array)
            .map(|items| items.iter().map(take_text).collect())
            .unwrap_or_default();
        let question = Arc::new(PendingQuestion {
            question_id: question_id.clone(),
            kind,
            question: question_text.clone(),
            options: options.clone(),
            receive_id: task.receive_id.clone(),
            receive_id_type: task.receive_id_type.clone(),
            sender_open_id: task.sender_open_id.clone(),
            message_id: Mutex::new(None),
            answer: Mutex::new(None),
            signal: Condvar::new(),
        });
        {
            let mut state = self.state.lock().expect("状态锁中毒");
            state.question = Some(question.clone());
        }
        let sent = if options.is_empty() {
            let mut prompt = format!("❓ {question_text}\n请直接回复答案。");
            if question.kind == "confirm" {
                prompt.push_str("（例如：是/否，或 yes/no）");
            }
            self.api
                .send_text(&question.receive_id, &prompt, &question.receive_id_type)
        } else {
            let card = question_card_json(&question_text, &options, &question_id);
            match self.api.send_message(
                &question.receive_id,
                &card,
                "interactive",
                &question.receive_id_type,
            ) {
                Some(message_id) => {
                    *question.message_id.lock().expect("提问锁中毒") = Some(message_id);
                    true
                }
                None => false,
            }
        };
        if !sent {
            let mut state = self.state.lock().expect("状态锁中毒");
            let same = state
                .question
                .as_ref()
                .map(|current| Arc::ptr_eq(current, &question))
                .unwrap_or(false);
            if same {
                state.question = None;
            }
            return None;
        }
        let answer = question.wait(self.confirmation_timeout);
        {
            let mut state = self.state.lock().expect("状态锁中毒");
            let same = state
                .question
                .as_ref()
                .map(|current| Arc::ptr_eq(current, &question))
                .unwrap_or(false);
            if same {
                state.question = None;
            }
        }
        match answer {
            None if question.is_answered() => None,
            None => {
                self.api.send_text(
                    &question.receive_id,
                    "⏰ 问题超时，已取消本次等待。",
                    &question.receive_id_type,
                );
                self.settle_question_card(&question, "⏰ 问题超时，等待已取消。");
                None
            }
            Some(value) => {
                self.settle_question_card(&question, &format!("✅ 已收到回答：{value}"));
                Some(value)
            }
        }
    }
}

impl PendingQuestion {
    fn is_answered(&self) -> bool {
        self.answer.lock().expect("提问锁中毒").is_some()
    }
}

impl<D: AgentDriver> FeishuBot<D> {
    /// 把提问卡片原地改写为只读终态，移除全部选项按钮。
    fn settle_question_card(&self, question: &PendingQuestion, status: &str) {
        if question.options.is_empty() {
            return;
        }
        let message_id = question.message_id.lock().expect("提问锁中毒").clone();
        let Some(message_id) = message_id else { return };
        let card = question_resolved_card_json(&question.question, status);
        self.api.patch_card(&message_id, &card);
    }
}

/// 一次回合的时间线装配：正文段、工具、思考、计划、子任务各自独立成消息。
struct TaskTimeline<D: AgentDriver> {
    bot: Arc<FeishuBot<D>>,
    receive_id: String,
    receive_id_type: String,
    deltas: String,
    all_deltas: String,
    reasoning: String,
    text_message: Option<TextMessage>,
    reasoning_message: Option<ReasoningMessage>,
    plan_message: Option<PlanMessage>,
    subagent_messages: BTreeMap<String, SubAgentMessage>,
    tool_messages: BTreeMap<String, ToolMessage>,
    streamed_any: bool,
    last_status: String,
    last_stream_patch: Option<Instant>,
    last_reasoning_patch: Option<Instant>,
}

impl<D: AgentDriver> TaskTimeline<D> {
    fn new(bot: Arc<FeishuBot<D>>, task: &ActiveTask) -> TaskTimeline<D> {
        TaskTimeline {
            bot,
            receive_id: task.receive_id.clone(),
            receive_id_type: task.receive_id_type.clone(),
            deltas: String::new(),
            all_deltas: String::new(),
            reasoning: String::new(),
            text_message: None,
            reasoning_message: None,
            plan_message: None,
            subagent_messages: BTreeMap::new(),
            tool_messages: BTreeMap::new(),
            streamed_any: false,
            last_status: String::new(),
            last_stream_patch: None,
            last_reasoning_patch: None,
        }
    }

    fn port(&self) -> Arc<dyn MessagePort> {
        self.bot.api.clone()
    }

    fn handle(&mut self, event: TurnEvent) {
        match event {
            TurnEvent::Delta(delta) => self.on_delta(&delta),
            TurnEvent::ReasoningDelta(delta) => self.on_reasoning(&delta),
            TurnEvent::Status(message) => self.on_status(&message),
            TurnEvent::ToolStarted { step, call } => self.on_tool_start(step, &call),
            TurnEvent::ToolFinished { call, result } => self.on_tool_result(&call, &result),
            TurnEvent::SubagentEvent { name, payload } => self.on_subagent(&name, &payload),
            TurnEvent::TodoUpdate { todos } => self.on_todos(&todos),
            TurnEvent::StreamRollback => {
                self.deltas.clear();
                self.all_deltas.clear();
                self.reasoning.clear();
                self.last_stream_patch = None;
                self.last_reasoning_patch = None;
            }
            _ => {}
        }
    }

    fn on_delta(&mut self, delta: &str) {
        if delta.is_empty() {
            return;
        }
        self.deltas.push_str(delta);
        self.all_deltas.push_str(delta);
        if self.text_message.is_none() {
            let joined = super::text::clean_text(&self.deltas);
            if joined.is_empty() {
                return;
            }
            self.seal_reasoning();
            let mut message =
                TextMessage::new(self.port(), &self.receive_id, &self.receive_id_type);
            message.stream(&joined);
            self.text_message = Some(message);
            self.streamed_any = true;
            self.last_stream_patch = Some(Instant::now());
            return;
        }
        if !self.patch_due(true) {
            return;
        }
        let content = self.deltas.clone();
        if let Some(message) = self.text_message.as_mut() {
            message.stream(&content);
        }
    }

    fn on_reasoning(&mut self, delta: &str) {
        if !self.bot.show_thinking() || delta.is_empty() {
            return;
        }
        self.reasoning.push_str(delta);
        if self.reasoning_message.is_none() {
            let mut message =
                ReasoningMessage::new(self.port(), &self.receive_id, &self.receive_id_type);
            message.stream(&self.reasoning.clone());
            self.reasoning_message = Some(message);
            self.last_reasoning_patch = Some(Instant::now());
            return;
        }
        if !self.patch_due(false) {
            return;
        }
        let content = self.reasoning.clone();
        if let Some(message) = self.reasoning_message.as_mut() {
            message.stream(&content);
        }
    }

    fn on_status(&mut self, message: &str) {
        let text = message.trim();
        if text.is_empty() || text == self.last_status {
            return;
        }
        self.last_status = text.to_string();
        self.bot.api.send_text(
            &self.receive_id,
            &format!("⏳ {text}"),
            &self.receive_id_type,
        );
    }

    fn on_tool_start(&mut self, step: u64, call: &ToolCall) {
        self.seal_reasoning();
        self.seal_text(None, "");
        let name = if call.name.is_empty() {
            "?"
        } else {
            call.name.as_str()
        };
        let operation = super::render::operation_of(name);
        if operation == ASK_USER_TOOL_NAME || operation == TODO_TOOL_NAME {
            return;
        }
        let key = match &call.id {
            Some(id) if !id.is_empty() => id.clone(),
            _ => format!("step-{step}"),
        };
        if self.tool_messages.contains_key(&key) {
            return;
        }
        let record = ToolRecord::new(&key, name, call.arguments.clone());
        let mut message =
            ToolMessage::new(self.port(), &self.receive_id, &self.receive_id_type, record);
        message.start();
        self.tool_messages.insert(key, message);
    }

    fn on_tool_result(&mut self, call: &ToolCall, result: &ToolResult) {
        let name = call.name.as_str();
        let operation = super::render::operation_of(name);
        if operation == ASK_USER_TOOL_NAME || operation == TODO_TOOL_NAME {
            return;
        }
        let key = call.id.clone().filter(|id| !id.is_empty());
        let target_key = match key {
            Some(key) if self.tool_messages.contains_key(&key) => Some(key),
            _ => self
                .tool_messages
                .iter()
                .find(|(_key, message)| message.running())
                .map(|(key, _message)| key.clone()),
        };
        let Some(target_key) = target_key else { return };
        if let Some(message) = self.tool_messages.get_mut(&target_key) {
            message.finish(result.ok, &result.output);
        }
    }

    fn on_subagent(&mut self, event_name: &str, payload: &Value) {
        if !payload.is_object() {
            return;
        }
        let batch_id = {
            let provided = field_text(payload, "batch_id");
            if !provided.is_empty() {
                provided
            } else {
                let task_id = field_text(payload, "task_id");
                format!(
                    "batch-{}",
                    if task_id.is_empty() {
                        "task"
                    } else {
                        task_id.as_str()
                    }
                )
            }
        };
        let port = self.port();
        let receive_id = self.receive_id.clone();
        let receive_id_type = self.receive_id_type.clone();
        let message = self
            .subagent_messages
            .entry(batch_id)
            .or_insert_with(|| SubAgentMessage::new(port, &receive_id, &receive_id_type));
        message.update(event_name, payload);
    }

    fn on_todos(&mut self, todos: &Value) {
        if !todos.is_object() {
            return;
        }
        let port = self.port();
        let receive_id = self.receive_id.clone();
        let receive_id_type = self.receive_id_type.clone();
        let message = self
            .plan_message
            .get_or_insert_with(|| PlanMessage::new(port, &receive_id, &receive_id_type));
        message.update(todos.get("todos"));
    }

    fn seal_reasoning(&mut self) {
        let Some(mut message) = self.reasoning_message.take() else {
            return;
        };
        message.seal(&self.reasoning.clone());
        self.reasoning.clear();
    }

    /// 封口当前正文段；`text` 为 None 时用流式累积文本，剩余部分补发为文本消息。
    fn seal_text(&mut self, text: Option<String>, suffix: &str) {
        let Some(mut message) = self.text_message.take() else {
            return;
        };
        let content = text.unwrap_or_else(|| self.deltas.clone());
        let remaining = message.seal(&content, suffix);
        self.deltas.clear();
        if let Some(remaining) = remaining {
            self.bot
                .api
                .send_text(&self.receive_id, &remaining, &self.receive_id_type);
        }
    }

    fn abort_running_tools(&mut self) {
        for message in self.tool_messages.values_mut() {
            message.abort();
        }
    }

    fn patch_due(&mut self, stream: bool) -> bool {
        let slot = if stream {
            &mut self.last_stream_patch
        } else {
            &mut self.last_reasoning_patch
        };
        let now = Instant::now();
        match slot {
            Some(previous)
                if now.duration_since(*previous)
                    < Duration::from_secs_f64(STREAM_PATCH_INTERVAL_SECONDS) =>
            {
                false
            }
            _ => {
                *slot = Some(now);
                true
            }
        }
    }

    fn deltas(&self) -> String {
        self.deltas.clone()
    }

    fn all_deltas(&self) -> String {
        self.all_deltas.clone()
    }

    fn cleaned_deltas(&self) -> String {
        super::text::clean_text(&self.deltas)
    }

    fn text_message_created(&self) -> bool {
        self.text_message.is_some()
    }

    fn has_reasoning_message(&self) -> bool {
        self.reasoning_message.is_some()
    }

    fn streamed_any(&self) -> bool {
        self.streamed_any
    }
}

fn sender_open_id(event: &Value) -> String {
    event
        .get("sender")
        .and_then(|sender| sender.get("sender_id"))
        .map(|sender_id| field_text(sender_id, "open_id"))
        .unwrap_or_default()
}

fn field_text(value: &Value, key: &str) -> String {
    match value.get(key) {
        Some(Value::String(text)) => text.trim().to_string(),
        Some(Value::Number(number)) => number.to_string(),
        _ => String::new(),
    }
}

fn take_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

fn take_chars(text: &str, limit: usize) -> String {
    text.chars().take(limit).collect()
}

fn relative_display(path: &std::path::Path, root: &std::path::Path) -> String {
    let resolved_path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let resolved_root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    match resolved_path.strip_prefix(&resolved_root) {
        Ok(relative) => relative.to_string_lossy().replace('\\', "/"),
        Err(_) => path.to_string_lossy().replace('\\', "/"),
    }
}

fn unix_seconds_f64() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_secs_f64())
        .unwrap_or(0.0)
}

fn unix_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_nanos())
        .unwrap_or(0)
}

const HELP_TEXT: &str = "🤖 OmniCrawl 飞书远程控制\n\n\
直接发送文本即可让 Agent 执行任务，例如：\n\
  「列出当前目录的文件」\n\
  「修复 README.md 中的错别字」\n\n\
任务控制：\n\
  /start  显示本帮助\n\
  /cancel  取消当前任务\n\
  /approve / /reject  批准/拒绝工具调用确认\n\
  /thinking on|off  思考内容折叠面板开关（默认关）\n\
  /status  查看状态与队列长度\n\
  /session 查看当前会话 ID\n\
  /reset   开启新会话\n\
  /workspace [路径]  查看/切换工作区\n\n\
文件：\n\
  直接发送图片/文件/语音，自动存入 .agent_tmp 的分类目录并交给 Agent 处理\n\n\
其余斜杠命令（/sessions、/resume、/undo、/compact、/plan、/tasks、/mcp 等）\n\
与会话管理命令共用 TUI 的同一套实现。";
