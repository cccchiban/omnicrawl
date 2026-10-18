//! Telegram Bot 轮询服务：更新分发、单活动任务、流式显示与工具确认桥。
//!
//! 语义基准是 Python `omnicrawl/connectors/telegram.py` 的 `TelegramAgentBot`：
//! 任务前后同步的配置、单活动执行、打字机式流式输出、`/approve` `/reject` 审批与
//! 文件消息后台下载都保持同一行为；Agent 回合与斜杠命令走 [`AgentDriver`]。

use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;

use omnicrawl_session::{redact_sensitive_text, redact_sensitive_values};

use super::api::{TelegramApi, TelegramApiError};
use super::config::{effective_confirm_timeout, validate_config, TelegramConfig};
use super::dispatch::{
    classify, parse_thinking_command, route_update, workspace_argument, Command, ThinkingCommand,
    UpdateRoute,
};
use super::files::{
    classify_file_name, fallback_download_name, relative_display_path, safe_download_name,
    temp_destination, unix_seconds, TelegramFile,
};
use super::format::{plan_abort, plan_finalize, truncate_for_stream, FinalizePlan};
use crate::agent::{AgentDriver, AskUserHandler, ConfirmHandler, TurnError, TurnEvent};
use crate::http::HttpTransport;

/// `close()` 等待任务线程退出的最大时长。
pub const CLOSE_TASK_JOIN_TIMEOUT: Duration = Duration::from_secs(10);

/// 确认提示里参数 JSON 的最大长度。
const CONFIRM_ARGUMENTS_LIMIT: usize = 1500;

const MAX_BACKOFF_SECONDS: f64 = 30.0;

/// 当前正在执行的 Agent 任务。
pub struct ActiveTask {
    pub chat_id: i64,
    pub user_id: i64,
    pub text: String,
    pub cancel: Arc<AtomicBool>,
    pub started_at: Instant,
    running: Arc<AtomicBool>,
}

/// 一次等待用户审批的工具调用请求。
struct PendingConfirm {
    tool_name: String,
    chat_id: i64,
    user_id: i64,
    decision: Mutex<Option<bool>>,
    signal: Condvar,
}

impl PendingConfirm {
    fn new(tool_name: &str, chat_id: i64, user_id: i64) -> PendingConfirm {
        PendingConfirm {
            tool_name: tool_name.to_string(),
            chat_id,
            user_id,
            decision: Mutex::new(None),
            signal: Condvar::new(),
        }
    }

    /// 返回 `Some(decision)` 表示已答复；`None` 表示等待超时。
    fn wait(&self, timeout: Duration) -> Option<bool> {
        let guard = self.decision.lock().expect("确认状态锁中毒");
        let (guard, _) = self
            .signal
            .wait_timeout_while(guard, timeout, |decision| decision.is_none())
            .expect("确认状态锁中毒");
        *guard
    }

    fn resolve(&self, decision: bool) {
        let mut guard = self.decision.lock().expect("确认状态锁中毒");
        if guard.is_none() {
            *guard = Some(decision);
        }
        self.signal.notify_all();
    }
}

/// 一次等待用户回答的 `ask_user` 请求。
struct PendingQuestion {
    kind: String,
    question: String,
    options: Vec<String>,
    chat_id: i64,
    user_id: i64,
    answer: Mutex<Option<String>>,
    signal: Condvar,
}

impl PendingQuestion {
    fn new(kind: &str, question: &str, options: Vec<String>, chat_id: i64, user_id: i64) -> PendingQuestion {
        PendingQuestion {
            kind: kind.to_string(),
            question: question.to_string(),
            options,
            chat_id,
            user_id,
            answer: Mutex::new(None),
            signal: Condvar::new(),
        }
    }

    /// 发给用户的提问提示：没有选项时只提示直接回复，有选项时列出编号。
    fn prompt(&self) -> String {
        if self.options.is_empty() {
            let mut text = format!("❓ {}\n请直接回复答案。", self.question);
            if self.kind == "confirm" {
                text.push_str("\n（例如：是 / 否，或 yes / no）");
            }
            return text;
        }
        let list = self
            .options
            .iter()
            .enumerate()
            .map(|(index, option)| format!("{}. {option}", index + 1))
            .collect::<Vec<String>>()
            .join("\n");
        format!(
            "❓ {}\n{list}\n直接回复选项正文即可作答。",
            self.question
        )
    }

    /// 返回 `Some(answer)` 表示已作答；`None` 表示等待超时或未作答。
    fn wait(&self, timeout: Duration) -> Option<String> {
        let guard = self.answer.lock().expect("提问状态锁中毒");
        let (guard, _) = self
            .signal
            .wait_timeout_while(guard, timeout, |answer| answer.is_none())
            .expect("提问状态锁中毒");
        guard.clone()
    }

    fn resolve(&self, answer: Option<String>) {
        let mut guard = self.answer.lock().expect("提问状态锁中毒");
        if guard.is_none() {
            *guard = answer;
        }
        self.signal.notify_all();
    }
}

#[derive(Default)]
struct BotState {
    active: Option<Arc<ActiveTask>>,
    pending: Option<Arc<PendingConfirm>>,
    question: Option<Arc<PendingQuestion>>,
}

/// 通过 Telegram 远程驱动单个 OmniCrawl Agent 的轮询服务。
pub struct TelegramBot<D: AgentDriver> {
    api: TelegramApi,
    driver: Arc<D>,
    allowed: HashSet<i64>,
    confirm_timeout: Duration,
    state: Mutex<BotState>,
    stopped: AtomicBool,
    show_thinking: AtomicBool,
}

impl<D: AgentDriver> TelegramBot<D> {
    /// 构造 Bot：Token 与白名单必填，超时下限 1 秒。
    pub fn new(
        config: &TelegramConfig,
        transport: Arc<dyn HttpTransport>,
        driver: Arc<D>,
    ) -> Result<TelegramBot<D>, String> {
        validate_config(config)?;
        Ok(TelegramBot {
            api: TelegramApi::new(&config.bot_token, transport),
            driver,
            allowed: config.allowed_user_ids.iter().copied().collect(),
            confirm_timeout: Duration::from_secs_f64(effective_confirm_timeout(
                config.confirm_timeout_seconds,
            )),
            state: Mutex::new(BotState::default()),
            stopped: AtomicBool::new(false),
            show_thinking: AtomicBool::new(false),
        })
    }

    /// 把自身注册成宿主的工具确认桥与提问桥。
    pub fn bind_handlers(self: &Arc<Self>) {
        self.driver.set_confirm_handler(self.clone());
        self.driver.set_ask_user_handler(self.clone());
    }

    /// 绑定确认桥并进入轮询主循环，直到 [`TelegramBot::request_stop`] 或认证失败。
    pub fn run(self: Arc<Self>) -> Result<(), TelegramApiError> {
        self.bind_handlers();
        self.run_forever()
    }

    /// 启动 `getUpdates` 长轮询；可恢复错误退避重试，不可恢复错误直接返回。
    pub fn run_forever(self: &Arc<Self>) -> Result<(), TelegramApiError> {
        let mut offset = 0_i64;
        let mut backoff = 1.0_f64;
        let mut allowed: Vec<i64> = self.allowed.iter().copied().collect();
        allowed.sort_unstable();
        eprintln!("[telegram] Telegram Bot 已启动（polling），允许用户：{allowed:?}");
        while !self.is_stopped() {
            match self.api.get_updates(offset) {
                Ok((updates, next_offset)) => {
                    offset = next_offset;
                    backoff = 1.0;
                    for update in &updates {
                        self.handle_update(update);
                    }
                }
                Err(error) if error.retryable() => {
                    eprintln!("[telegram] Telegram API 错误，{backoff:.0} 秒后重试：{error}");
                    if self.sleep_until_stopped(backoff) {
                        break;
                    }
                    backoff = (backoff * 2.0).min(MAX_BACKOFF_SECONDS);
                }
                Err(error) => {
                    eprintln!("[telegram] Telegram API 不可恢复错误，停止轮询：{error}");
                    return Err(error);
                }
            }
        }
        eprintln!("[telegram] Telegram Bot 已停止。");
        Ok(())
    }

    /// 停止轮询并回收 Agent 资源。
    ///
    /// 先置停止/取消事件并释放挂起的工具确认与提问（否则两条桥都会阻塞到超时），
    /// 再等待任务线程自行退出（上限 [`CLOSE_TASK_JOIN_TIMEOUT`]），最后通知宿主关闭。
    pub fn close(&self) {
        self.stopped.store(true, Ordering::SeqCst);
        let (task, pending, question) = {
            let state = self.state.lock().expect("状态锁中毒");
            (
                state.active.clone(),
                state.pending.clone(),
                state.question.clone(),
            )
        };
        if let Some(task) = &task {
            task.cancel.store(true, Ordering::SeqCst);
            self.driver.request_cancel();
        }
        if let Some(pending) = &pending {
            pending.resolve(false);
        }
        if let Some(question) = &question {
            question.resolve(None);
        }
        let deadline = Instant::now() + CLOSE_TASK_JOIN_TIMEOUT;
        while Instant::now() < deadline && self.is_busy() {
            thread::sleep(Duration::from_millis(20));
        }
        if self.is_busy() {
            eprintln!(
                "[telegram] 任务线程在 {} 秒内未退出，继续关闭。",
                CLOSE_TASK_JOIN_TIMEOUT.as_secs()
            );
        }
        if let Err(error) = self.driver.shutdown() {
            eprintln!("[telegram] 关闭 Agent 失败：{error}");
        }
    }

    /// 供信号处理或测试从其他线程触发退出。
    pub fn request_stop(&self) {
        self.stopped.store(true, Ordering::SeqCst);
    }

    /// 处理一条更新：白名单 → 提问回答 → 命令 / 任务 / 文件。
    pub fn handle_update(self: &Arc<Self>, update: &Value) {
        match route_update(update, &self.allowed) {
            UpdateRoute::MissingIdentifiers | UpdateRoute::Unsupported => {}
            UpdateRoute::Unauthorized { chat_id, user_id } => {
                eprintln!("[telegram] 拒绝未授权用户 {user_id} 的操作（chat={chat_id}）");
            }
            UpdateRoute::Text {
                chat_id,
                user_id,
                text,
            } => {
                // 挂起提问优先于命令与任务：回答只交给当前提问，不启动新任务。
                if self.answer_pending_question(chat_id, user_id, &text) {
                    return;
                }
                self.dispatch(chat_id, user_id, &text)
            }
            UpdateRoute::File {
                chat_id,
                user_id,
                file,
                caption,
            } => self.spawn_file_task(chat_id, user_id, file, caption),
        }
    }

    /// 文本是否被当前挂起的 `ask_user` 提问消费。
    ///
    /// 仅发起该任务的白名单用户本人可作答；`select` 提问只接受已声明的选项之一，
    /// `/cancel` 等同取消当前任务。返回 `true` 表示该文本不再作为任务或命令处理。
    pub fn answer_pending_question(self: &Arc<Self>, chat_id: i64, user_id: i64, text: &str) -> bool {
        let question = {
            let state = self.state.lock().expect("状态锁中毒");
            match state.question.clone() {
                Some(question) => question,
                None => return false,
            }
        };
        if question.chat_id != chat_id || question.user_id != user_id {
            return false;
        }
        let answer = text.trim().to_string();
        if answer == "/cancel" {
            question.resolve(None);
            self.request_cancel(chat_id);
            return true;
        }
        if answer.is_empty() {
            self.send(chat_id, "请发送非空回答。");
            return true;
        }
        if question.kind == "select" && !question.options.contains(&answer) {
            self.send(chat_id, "请回复问题列出的选项之一。");
            return true;
        }
        question.resolve(Some(answer));
        self.send(chat_id, "✅ 已收到回答。");
        true
    }

    /// 分发基础命令、宿主命令与任务文本。
    pub fn dispatch(self: &Arc<Self>, chat_id: i64, user_id: i64, text: &str) {
        match classify(text) {
            Command::Start => self.send(chat_id, &self.help_text()),
            Command::Status => {
                let status = self.status_text();
                self.send(chat_id, &status);
            }
            Command::Session => {
                let session_id = self.driver.status().session_id;
                self.send(chat_id, &format!("当前会话 ID：`{session_id}`"));
            }
            Command::Reset => match self.driver.reset_conversation() {
                Ok(()) => self.send(chat_id, "已开启新会话（对话历史已清空）。"),
                Err(error) => {
                    eprintln!("[telegram] 重置会话失败：{error}");
                    self.send(
                        chat_id,
                        &format!("❌ 重置会话失败：{}", redact_sensitive_text(&error)),
                    );
                }
            },
            Command::Cancel => self.request_cancel(chat_id),
            Command::Approve => self.handle_approval(chat_id, user_id, true),
            Command::Reject => self.handle_approval(chat_id, user_id, false),
            Command::Thinking => self.handle_thinking(chat_id, text),
            Command::Workspace => self.handle_workspace(chat_id, text),
            Command::Harness => match self.driver.handle_command(text, "telegram") {
                Ok(Some(reply)) => self.send(chat_id, &reply),
                Ok(None) => self.send(chat_id, "未知命令。发送 /start 查看可用命令。"),
                Err(error) => {
                    eprintln!("[telegram] 宿主命令执行失败：{text}");
                    self.send(
                        chat_id,
                        &format!("❌ 命令执行失败：{}", redact_sensitive_text(&error)),
                    );
                }
            },
            Command::Task => self.start_task(chat_id, user_id, text.to_string()),
        }
    }

    /// 单活动校验后，在后台线程执行 Agent 任务。
    pub fn start_task(self: &Arc<Self>, chat_id: i64, user_id: i64, text: String) {
        {
            let mut state = self.state.lock().expect("状态锁中毒");
            if let Some(active) = state.active.clone() {
                if active.running.load(Ordering::SeqCst) {
                    drop(state);
                    self.send(
                        chat_id,
                        "🔄 当前已有任务正在执行，请等待完成或发送 /cancel 取消。",
                    );
                    return;
                }
            }
            let task = Arc::new(ActiveTask {
                chat_id,
                user_id,
                text,
                cancel: Arc::new(AtomicBool::new(false)),
                started_at: Instant::now(),
                running: Arc::new(AtomicBool::new(true)),
            });
            state.active = Some(task.clone());
            let bot = self.clone();
            let spawned = thread::Builder::new()
                .name("omnicrawl-telegram-task".to_string())
                .spawn(move || bot.execute_task(task));
            if let Err(error) = spawned {
                eprintln!("[telegram] 任务线程启动失败：{error}");
                self.state.lock().expect("状态锁中毒").active = None;
                self.send(chat_id, "❌ 任务线程启动失败。");
                return;
            }
        }
        self.send(chat_id, "✅ 已收到任务，开始执行（/cancel 可取消）。");
    }

    /// 任务线程：驱动回合、把事件映射成消息，并保留已流式输出的部分。
    fn execute_task(&self, task: Arc<ActiveTask>) {
        let mut display = TaskDisplay::new(self, task.chat_id);
        let result = self
            .driver
            .run_turn(&task.text, &mut |event| display.handle(event));
        let cancelled = task.cancel.load(Ordering::SeqCst);
        self.finish_task(&task, display, result, cancelled);
        task.running.store(false, Ordering::SeqCst);
        let mut state = self.state.lock().expect("状态锁中毒");
        let same = state
            .active
            .as_ref()
            .map(|active| Arc::ptr_eq(active, &task))
            .unwrap_or(false);
        if same {
            state.active = None;
        }
    }

    fn finish_task(
        &self,
        task: &ActiveTask,
        display: TaskDisplay<'_, D>,
        result: Result<String, TurnError>,
        cancelled: bool,
    ) {
        if cancelled {
            self.abort_task(task, &display, "⏹ 任务已取消。");
            return;
        }
        match result {
            Ok(reply) => {
                let reply = if reply.is_empty() {
                    if display.deltas.is_empty() {
                        "（任务完成，无文本输出）".to_string()
                    } else {
                        display.deltas.clone()
                    }
                } else {
                    reply
                };
                self.finalize_stream(
                    task.chat_id,
                    display.stream_message_id,
                    &format!("✅ {reply}"),
                );
            }
            Err(TurnError::Cancelled) => self.abort_task(task, &display, "⏹ 任务已取消。"),
            Err(TurnError::Failed(message)) => {
                eprintln!("[telegram] 任务执行失败：{message}");
                let error_text = format!("❌ 任务执行失败：{}", redact_sensitive_text(&message));
                self.abort_task(task, &display, &error_text);
            }
        }
    }

    /// 保留已流式输出的内容，错误或取消提示单独发送。
    fn abort_task(&self, task: &ActiveTask, display: &TaskDisplay<'_, D>, error_text: &str) {
        let plan = plan_abort(
            display.stream_message_id.is_some(),
            &display.deltas,
            error_text,
        );
        if let (Some(head), Some(message_id)) = (plan.edit_head, display.stream_message_id) {
            self.api
                .edit_stream_message(task.chat_id, message_id, &head);
        }
        self.send(task.chat_id, &plan.send_text);
    }

    /// 定型流式消息：短文本就地编辑，超长先显示截断头再补发完整内容。
    fn finalize_stream(&self, chat_id: i64, stream_message_id: Option<i64>, text: &str) {
        match plan_finalize(stream_message_id.is_some(), text) {
            FinalizePlan::EditFull(content) => {
                if let Some(message_id) = stream_message_id {
                    self.api.edit_stream_message(chat_id, message_id, &content);
                }
            }
            FinalizePlan::EditHeadThenSend { head, full } => {
                if let Some(message_id) = stream_message_id {
                    self.api.edit_stream_message(chat_id, message_id, &head);
                }
                self.send(chat_id, &full);
            }
            FinalizePlan::SendFull(full) => self.send(chat_id, &full),
        }
    }

    /// `/approve` 与 `/reject`：仅发起任务的白名单用户本人可批准或拒绝。
    pub fn handle_approval(&self, chat_id: i64, user_id: i64, approve: bool) {
        let pending = {
            let state = self.state.lock().expect("状态锁中毒");
            state.pending.clone()
        };
        let Some(pending) = pending else { return };
        if pending.chat_id != chat_id || pending.user_id != user_id {
            return;
        }
        pending.resolve(approve);
        let action = if approve { "已批准" } else { "已拒绝" };
        self.send(chat_id, &format!("✅ {action}：{}", pending.tool_name));
    }

    /// 请求取消当前任务；无活动任务时仅提示。
    pub fn request_cancel(&self, chat_id: i64) {
        let (task, pending, question) = {
            let state = self.state.lock().expect("状态锁中毒");
            (
                state.active.clone(),
                state.pending.clone(),
                state.question.clone(),
            )
        };
        let Some(task) = task else {
            self.send(chat_id, "当前没有正在执行的任务。");
            return;
        };
        if !task.running.load(Ordering::SeqCst) {
            self.send(chat_id, "当前没有正在执行的任务。");
            return;
        }
        task.cancel.store(true, Ordering::SeqCst);
        self.driver.request_cancel();
        if let Some(pending) = pending {
            if pending.chat_id == chat_id {
                pending.resolve(false);
            }
        }
        // 挂起提问同样要释放，否则提问桥会一直等到超时。
        if let Some(question) = question {
            if question.chat_id == chat_id {
                question.resolve(None);
            }
        }
        self.send(chat_id, "⏹ 已请求取消当前任务，请稍候……");
    }

    /// `/thinking`：查看或切换思考内容显示（默认关闭）。
    pub fn handle_thinking(&self, chat_id: i64, text: &str) {
        match parse_thinking_command(text) {
            ThinkingCommand::Query => {
                let state = if self.show_thinking() {
                    "开启"
                } else {
                    "关闭"
                };
                self.send(
                    chat_id,
                    &format!("思考内容显示：{state}（默认关闭）。\n/thinking on 开启，/thinking off 关闭。"),
                );
            }
            ThinkingCommand::Enable => {
                self.show_thinking.store(true, Ordering::SeqCst);
                self.send(chat_id, "已开启思考内容显示（🧠 独立消息）。");
            }
            ThinkingCommand::Disable => {
                self.show_thinking.store(false, Ordering::SeqCst);
                self.send(chat_id, "已关闭思考内容显示。");
            }
            ThinkingCommand::Usage => self.send(chat_id, "用法：/thinking on 或 /thinking off。"),
        }
    }

    /// `/workspace`：查看或切换工作区（切换由宿主负责持久化）。
    pub fn handle_workspace(&self, chat_id: i64, text: &str) {
        let Some(path) = workspace_argument(text) else {
            let root = self.driver.status().workspace_root;
            self.send(
                chat_id,
                &format!("当前工作区：{root}\n用法：/workspace <路径>"),
            );
            return;
        };
        match self.driver.switch_workspace(&path) {
            Ok(switched) => self.send(
                chat_id,
                &format!(
                    "✅ 已切换工作区：{}{}",
                    switched.workspace_root, switched.note
                ),
            ),
            Err(error) => self.send(
                chat_id,
                &format!("❌ 切换工作区失败：{}", redact_sensitive_text(&error)),
            ),
        }
    }

    /// 后台线程：下载文件、分类落盘并通知 Agent。
    fn spawn_file_task(
        self: &Arc<Self>,
        chat_id: i64,
        user_id: i64,
        file: TelegramFile,
        caption: String,
    ) {
        let bot = self.clone();
        let spawned = thread::Builder::new()
            .name("omnicrawl-telegram-file".to_string())
            .spawn(move || bot.process_file_message(chat_id, user_id, file, caption));
        if let Err(error) = spawned {
            eprintln!("[telegram] 文件处理线程启动失败：{error}");
        }
    }

    /// 下载 → 分类存放 → 通知用户 → 启动任务；各步失败都回传脱敏错误。
    pub fn process_file_message(
        self: &Arc<Self>,
        chat_id: i64,
        user_id: i64,
        file: TelegramFile,
        caption: String,
    ) {
        let remote_path = match self.api.get_file_path(&file.file_id) {
            Ok(path) => path,
            Err(error) => {
                self.send_download_failure(chat_id, &error.to_string());
                return;
            }
        };
        let content = match self.api.download_file(&remote_path) {
            Ok(content) => content,
            Err(error) => {
                self.send_download_failure(chat_id, &error.to_string());
                return;
            }
        };
        let stamp = unix_seconds();
        let reference = if file.file_name.is_empty() {
            remote_path.as_str()
        } else {
            file.file_name.as_str()
        };
        let subdir = classify_file_name(reference);
        let name = if file.file_name.is_empty() {
            fallback_download_name(&remote_path, stamp)
        } else {
            safe_download_name(&file.file_name, stamp)
        };
        let destination = match temp_destination(&self.driver.temp_root(), subdir, &name) {
            Ok(path) => path,
            Err(error) => {
                eprintln!("[telegram] 文件保存失败：{error}");
                self.send(
                    chat_id,
                    &format!(
                        "❌ 文件保存失败：{}",
                        redact_sensitive_text(&error.to_string())
                    ),
                );
                return;
            }
        };
        if let Err(error) = fs::write(&destination, &content) {
            eprintln!("[telegram] 文件保存失败：{error}");
            self.send(
                chat_id,
                &format!(
                    "❌ 文件保存失败：{}",
                    redact_sensitive_text(&error.to_string())
                ),
            );
            return;
        }
        let relative = relative_display_path(&destination, &self.workspace_root());
        let file_label = destination
            .file_name()
            .map(|value| value.to_string_lossy().to_string())
            .unwrap_or_default();
        self.send(
            chat_id,
            &format!("✅ 已收到文件：{file_label} → {relative}"),
        );
        if self.is_stopped() {
            eprintln!("[telegram] 服务已停止，跳过文件任务启动：{relative}");
            return;
        }
        let mut task_text = format!("已收到文件：位于 {relative}");
        if !caption.is_empty() {
            task_text.push_str(&format!("\n\n用户补充说明：{caption}"));
        }
        self.start_task(chat_id, user_id, task_text);
    }

    fn send_download_failure(&self, chat_id: i64, detail: &str) {
        eprintln!("[telegram] 文件下载失败：{detail}");
        self.send(
            chat_id,
            &format!("❌ 文件下载失败：{}", redact_sensitive_text(detail)),
        );
    }

    pub fn send(&self, chat_id: i64, text: &str) {
        self.api.send_message(chat_id, text);
    }

    pub fn show_thinking(&self) -> bool {
        self.show_thinking.load(Ordering::SeqCst)
    }

    pub fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::SeqCst)
    }

    /// 是否有任务线程在跑（`/status` 与单活动校验共用）。
    pub fn is_busy(&self) -> bool {
        let state = self.state.lock().expect("状态锁中毒");
        state
            .active
            .as_ref()
            .map(|task| task.running.load(Ordering::SeqCst))
            .unwrap_or(false)
    }

    fn workspace_root(&self) -> PathBuf {
        PathBuf::from(self.driver.status().workspace_root)
    }

    fn sleep_until_stopped(&self, seconds: f64) -> bool {
        let deadline = Instant::now() + Duration::from_secs_f64(seconds.max(0.0));
        while Instant::now() < deadline {
            if self.is_stopped() {
                return true;
            }
            thread::sleep(Duration::from_millis(50));
        }
        self.is_stopped()
    }

    fn status_text(&self) -> String {
        let status = self.driver.status();
        let mut lines = vec![
            "📊 OmniCrawl 状态".to_string(),
            format!("工作区：{}", status.workspace_root),
            format!("会话 ID：{}", status.session_id),
            format!(
                "状态：{}",
                if self.is_busy() {
                    "🔄 正在执行任务"
                } else {
                    "✅ 空闲"
                }
            ),
        ];
        if self.is_busy() {
            let state = self.state.lock().expect("状态锁中毒");
            if let Some(task) = &state.active {
                let elapsed = task.started_at.elapsed().as_secs();
                lines.push(format!("任务已运行 {elapsed} 秒，/cancel 可取消。"));
            }
        }
        lines.join("\n")
    }

    fn help_text(&self) -> String {
        HELP_TEXT.to_string()
    }
}

impl<D: AgentDriver> ConfirmHandler for TelegramBot<D> {
    /// 把工具确认转发到 Telegram，等 `/approve` 或 `/reject`，超时按拒绝。
    fn confirm(&self, tool_name: &str, arguments: &Value) -> bool {
        let task = {
            let state = self.state.lock().expect("状态锁中毒");
            state.active.clone()
        };
        let Some(task) = task else { return false };
        let pending = Arc::new(PendingConfirm::new(tool_name, task.chat_id, task.user_id));
        {
            let mut state = self.state.lock().expect("状态锁中毒");
            state.pending = Some(pending.clone());
        }
        let safe_arguments = redact_sensitive_values(arguments);
        let rendered = serde_json::to_string_pretty(&safe_arguments).unwrap_or_default();
        let prompt = format!(
            "⚠️ 需要确认执行敏感操作：\n工具：{tool_name}\n参数：\n{}\n回复 /approve 允许，/reject 拒绝。{} 秒内未回复将自动拒绝。",
            take_chars(&rendered, CONFIRM_ARGUMENTS_LIMIT),
            self.confirm_timeout.as_secs(),
        );
        self.send(task.chat_id, &prompt);
        let decided = pending.wait(self.confirm_timeout);
        {
            let mut state = self.state.lock().expect("状态锁中毒");
            let same = state
                .pending
                .as_ref()
                .map(|current| Arc::ptr_eq(current, &pending))
                .unwrap_or(false);
            if same {
                state.pending = None;
            }
        }
        match decided {
            Some(decision) => decision,
            None => {
                self.send(task.chat_id, "⏰ 确认超时，已自动拒绝该操作。");
                false
            }
        }
    }
}

impl<D: AgentDriver> AskUserHandler for TelegramBot<D> {
    /// 把 `ask_user` 转发到 Telegram，等用户回复文本，超时按「未作答」。
    ///
    /// 没有选项时只提示直接回复；有选项时列出编号，同时接受直接回复选项正文。
    fn ask(&self, request: &Value) -> Option<String> {
        if self.is_stopped() {
            return None;
        }
        let task = {
            let state = self.state.lock().expect("状态锁中毒");
            match state.active.clone() {
                Some(task) if task.running.load(Ordering::SeqCst) => task,
                _ => return None,
            }
        };
        let question_text = request
            .get("question")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        let kind = request
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or("question")
            .trim()
            .to_string();
        let options: Vec<String> = request
            .get("options")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();

        let question = Arc::new(PendingQuestion::new(
            &kind,
            &question_text,
            options,
            task.chat_id,
            task.user_id,
        ));
        {
            let mut state = self.state.lock().expect("状态锁中毒");
            state.question = Some(question.clone());
        }

        self.send(task.chat_id, &question.prompt());

        let timeout = self.confirm_timeout;
        let answered = question.wait(timeout);
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
        match answered {
            Some(answer) => Some(answer),
            None => {
                if !self.is_stopped() {
                    self.send(task.chat_id, "⏰ 提问超时，本轮不再等待作答。");
                }
                None
            }
        }
    }
}

/// 一次回合的显示状态：流式回答、思考内容、状态提示各自独立成消息。
struct TaskDisplay<'a, D: AgentDriver> {
    bot: &'a TelegramBot<D>,
    chat_id: i64,
    deltas: String,
    reasoning: String,
    last_status: String,
    stream_message_id: Option<i64>,
    thinking_message_id: Option<i64>,
    stream_edit_at: Option<Instant>,
    thinking_edit_at: Option<Instant>,
}

impl<'a, D: AgentDriver> TaskDisplay<'a, D> {
    fn new(bot: &'a TelegramBot<D>, chat_id: i64) -> TaskDisplay<'a, D> {
        TaskDisplay {
            bot,
            chat_id,
            deltas: String::new(),
            reasoning: String::new(),
            last_status: String::new(),
            stream_message_id: None,
            thinking_message_id: None,
            stream_edit_at: None,
            thinking_edit_at: None,
        }
    }

    fn handle(&mut self, event: TurnEvent) {
        match event {
            TurnEvent::Delta(delta) => {
                self.deltas.push_str(&delta);
                if self.throttled(&mut StreamSlot::Stream) {
                    return;
                }
                let display = truncate_for_stream(&self.deltas);
                self.stream_message_id = self.push_stream(self.stream_message_id, &display);
            }
            TurnEvent::ReasoningDelta(delta) => {
                if !self.bot.show_thinking() {
                    return;
                }
                self.reasoning.push_str(&delta);
                if self.throttled(&mut StreamSlot::Thinking) {
                    return;
                }
                let display = format!("🧠 {}", truncate_for_stream(&self.reasoning));
                self.thinking_message_id = self.push_stream(self.thinking_message_id, &display);
            }
            TurnEvent::Status(message) => {
                if message == self.last_status {
                    return;
                }
                self.last_status = message.clone();
                self.bot.send(self.chat_id, &format!("⏳ {message}"));
            }
            TurnEvent::ToolStarted { call, .. } => {
                let safe = redact_sensitive_values(call.parameters());
                let summary = take_chars(&serde_json::to_string(&safe).unwrap_or_default(), 300);
                self.bot.send(
                    self.chat_id,
                    &format!("🛠 正在执行：{}({summary})", call.name),
                );
            }
            TurnEvent::ToolFinished { call, result } => {
                if result.ok {
                    return;
                }
                let output = take_chars(&redact_sensitive_text(&result.output), 300);
                self.bot.send(
                    self.chat_id,
                    &format!("⚠️ {} 执行失败：{output}", call.name),
                );
            }
            _ => {}
        }
    }

    fn push_stream(&self, message_id: Option<i64>, display: &str) -> Option<i64> {
        match message_id {
            Some(message_id) => {
                self.bot
                    .api
                    .edit_stream_message(self.chat_id, message_id, display);
                Some(message_id)
            }
            None => self.bot.api.create_stream_message(self.chat_id, display),
        }
    }

    /// 节流：Telegram 对同一条消息的编辑频率约 1 次/秒，间隔内只累积不发送。
    fn throttled(&mut self, slot: &mut StreamSlot) -> bool {
        let last = match slot {
            StreamSlot::Stream => &mut self.stream_edit_at,
            StreamSlot::Thinking => &mut self.thinking_edit_at,
        };
        let now = Instant::now();
        match last {
            Some(previous)
                if now.duration_since(*previous)
                    < Duration::from_secs_f64(super::format::STREAM_EDIT_INTERVAL_SECONDS) =>
            {
                true
            }
            _ => {
                *last = Some(now);
                false
            }
        }
    }
}

enum StreamSlot {
    Stream,
    Thinking,
}

fn take_chars(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    text.chars().take(limit).collect()
}

const HELP_TEXT: &str = "🤖 OmniCrawl 远程控制\n\n\
直接发送文本即可让 Agent 执行任务，例如：\n\
  「列出当前目录的文件」\n\
  「修复 README.md 中的错别字」\n\n\
任务控制：\n\
  /start  显示本帮助\n\
  /cancel  取消当前任务\n\
  /approve / /reject  批准/拒绝工具调用确认\n\
  /thinking on|off  思考内容显示开关（默认关）\n\
  /status  查看 harness 状态\n\
  /session 查看当前会话 ID\n\
  /reset   开启新会话\n\
  /workspace [路径]  查看/切换工作区（切换会同步到 TUI）\n\
  /plan  启用计划模式，后续任务先制定 Markdown 计划\n\n\
文件：\n\
  直接发送图片/文档/视频/语音/音频，自动存入 .agent_tmp\n\
  的 images/videos/audio/files/code/scripts 分类目录，\n\
  并交给 Agent 处理（可附 caption 说明任务）\n\n\
会话管理：\n\
  /sessions  最近会话\n\
  /archives  归档会话\n\
  /archive   归档当前会话\n\
  /resume <id> / /resume latest  恢复会话（latest 恢复最近活动）\n\
  /rename <标题>  重命名当前会话\n\
  /undo      回退最近一轮\n\
  /compact  压缩上下文\n\
  /history [关键词]  提示历史\n\n\
子系统状态：\n\
  /tasks / /task <id> [cancel]  后台子任务\n\
  /mcp   /plugins   /skills\n\
  /memory:clean  清理过期记忆\n\
  /reasoning [级别]  推理强度\n\
  /approval  查看审批模式\n\
  /approval:manual|review（远程不支持 auto）  切换审批模式\n\n\
敏感操作（bash/powershell 等）在手动/审查模式下会请求确认，超时自动拒绝；\n\
完全自动（auto）仅限本地 TUI，远程默认自动审查（review）。";
