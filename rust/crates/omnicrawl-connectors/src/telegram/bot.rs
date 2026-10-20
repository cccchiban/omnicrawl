//! Telegram Bot 轮询服务：更新分发、单活动任务、时间线显示与工具确认桥。
//!
//! 语义基准是 Python `omnicrawl/connectors/telegram.py` 的 `TelegramAgentBot`：
//! 单活动执行、`/approve` `/reject` 审批与文件消息后台下载保持同一行为；Agent 回合与
//! 斜杠命令走 [`AgentDriver`]。
//!
//! 显示口径与飞书连接器对齐（见 `super::timeline`）：正文段、工具调用、思考、执行计划与
//! 子任务进度各自独立成一条消息、按发生顺序出现，条目自己的更新只编辑自己那条消息。
//! 此前所有正文增量都累进同一条流式消息、`TodoUpdate`/`SubagentEvent` 被整体忽略，
//! 长任务因此在聊天里堆成一大坨。

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;

use omnicrawl_session::{redact_sensitive_text, redact_sensitive_values};

use super::api::{MenuCommand, TelegramApi, TelegramApiError};
use super::config::{effective_confirm_timeout, validate_config, TelegramConfig};
use super::dispatch::{
    classify, parse_thinking_command, route_update, workspace_argument, Command, ThinkingCommand,
    UpdateRoute,
};
use super::files::{
    classify_file_name, fallback_download_name, relative_display_path, safe_download_name,
    temp_destination, unix_seconds, TelegramFile,
};
use super::format::plan_abort;
use super::timeline::{
    MessagePort, PlanMessage, ReasoningMessage, SubAgentMessage, TextMessage, ToolMessage,
    ToolRecord,
};
use crate::agent::{
    AgentDriver, AskUserHandler, ConfirmHandler, ToolCall, ToolResult, TurnError, TurnEvent,
};
use crate::http::HttpTransport;
use crate::render::{operation_of, ASK_USER_TOOL_NAME, TODO_TOOL_NAME};
use crate::text::{display_text, resolve_final_text};

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
    fn new(
        kind: &str,
        question: &str,
        options: Vec<String>,
        chat_id: i64,
        user_id: i64,
    ) -> PendingQuestion {
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
        format!("❓ {}\n{list}\n直接回复选项正文即可作答。", self.question)
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
    api: Arc<TelegramApi>,
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
            api: Arc::new(TelegramApi::new(&config.bot_token, transport)),
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
        self.publish_command_menu();
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

    /// 把宿主的斜杠命令表发布成 bot 的命令菜单（`setMyCommands`）。
    ///
    /// 菜单是客户端侧提示，宿主没给表就跳过；发布失败只记日志，不阻断轮询。
    pub fn publish_command_menu(&self) {
        let commands: Vec<MenuCommand> = self
            .driver
            .command_menu()
            .into_iter()
            .map(|(command, description)| MenuCommand::new(command, description))
            .collect();
        if commands.is_empty() {
            return;
        }
        match self.api.set_my_commands(&commands) {
            Ok(count) => eprintln!("[telegram] 已发布 {count} 条命令菜单。"),
            Err(error) => eprintln!("[telegram] 发布命令菜单失败（不影响命令分发）：{error}"),
        }
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
    pub fn answer_pending_question(
        self: &Arc<Self>,
        chat_id: i64,
        user_id: i64,
        text: &str,
    ) -> bool {
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

    /// 任务线程：驱动回合、把事件映射成时间线条目，并保留已展示的正文段。
    fn execute_task(self: &Arc<Self>, task: Arc<ActiveTask>) {
        let mut display = TaskTimeline::new(self, task.chat_id);
        let result = {
            let mut events = |event: TurnEvent| display.handle(event);
            self.driver.run_turn(&task.text, &mut events)
        };
        let cancelled = task.cancel.load(Ordering::SeqCst);
        self.finish_task(&task, &mut display, result, cancelled);
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
        display: &mut TaskTimeline<'_, D>,
        result: Result<String, TurnError>,
        cancelled: bool,
    ) {
        if cancelled {
            display.abort_running_tools();
            self.abort_task(task, display, "⏹ 任务已取消。");
            return;
        }
        match result {
            Ok(reply) => {
                let raw_reply = if reply.is_empty() {
                    display.all_deltas()
                } else {
                    reply
                };
                if self.show_thinking() {
                    display.seal_reasoning();
                }
                if display.text_message_created() {
                    let streamed = display.deltas();
                    let final_text = resolve_final_text(&streamed, &display_text(&raw_reply));
                    display.seal_text(Some(final_text), "");
                } else {
                    let cleaned_reply = crate::text::clean_text(&raw_reply);
                    if !cleaned_reply.is_empty() {
                        // 已经原样展示过的内容不再发一遍。
                        if display.cleaned_all_deltas() != cleaned_reply {
                            display.send_segment(&cleaned_reply);
                        }
                    } else if !display.streamed_any() {
                        self.send(task.chat_id, "（任务完成，无文本输出）");
                    }
                }
            }
            Err(TurnError::Cancelled) => {
                display.abort_running_tools();
                self.abort_task(task, display, "⏹ 任务已取消。");
            }
            Err(TurnError::Failed(message)) => {
                display.abort_running_tools();
                display.seal_text(None, "");
                eprintln!("[telegram] 任务执行失败：{message}");
                let error_text = format!("❌ 任务执行失败：{}", redact_sensitive_text(&message));
                self.send(task.chat_id, &error_text);
            }
        }
    }

    /// 保留已输出的正文段，错误或取消提示单独发送。
    ///
    /// 已流式展示的正文就地补中断标记（保留 `◇` 前缀，与正常封口同形），错误另发一条。
    fn abort_task(&self, task: &ActiveTask, display: &TaskTimeline<'_, D>, error_text: &str) {
        let streamed = crate::text::clean_text(&display.deltas());
        let plan = plan_abort(display.text_message_created(), &streamed, error_text);
        if let (Some(head), Some(message_id)) = (plan.edit_head, display.text_message_id()) {
            MessagePort::edit(&*self.api, task.chat_id, message_id, &format!("◇ {head}"));
        }
        self.send(task.chat_id, &plan.send_text);
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

/// 一次回合的时间线装配：正文段、工具、思考、计划、子任务各自独立成一条消息。
///
/// 与飞书连接器的 `TaskTimeline` 同构：模型一个 pass 的正文段在工具调用处封口，下一段
/// 正文另起一条新消息；每个条目只编辑自己那条消息，条目之间互不覆盖。
struct TaskTimeline<'a, D: AgentDriver> {
    bot: &'a TelegramBot<D>,
    chat_id: i64,
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
    last_stream_edit_at: Option<Instant>,
    last_reasoning_edit_at: Option<Instant>,
}

impl<'a, D: AgentDriver> TaskTimeline<'a, D> {
    fn new(bot: &'a TelegramBot<D>, chat_id: i64) -> TaskTimeline<'a, D> {
        TaskTimeline {
            bot,
            chat_id,
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
            last_stream_edit_at: None,
            last_reasoning_edit_at: None,
        }
    }

    fn port(&self) -> Arc<dyn MessagePort> {
        let port: Arc<dyn MessagePort> = self.bot.api.clone();
        port
    }

    fn handle(&mut self, event: TurnEvent) {
        match event {
            TurnEvent::Delta(delta) => self.on_delta(&delta),
            TurnEvent::ReasoningDelta(delta) => self.on_reasoning(&delta),
            TurnEvent::Status(message) => self.on_status(&message),
            TurnEvent::RetryStatus(message) => self.on_status(&message),
            TurnEvent::ToolStarted { step, call } => self.on_tool_start(step, &call),
            TurnEvent::ToolFinished { call, result } => self.on_tool_result(&call, &result),
            TurnEvent::SubagentEvent { name, payload } => self.on_subagent(&name, &payload),
            TurnEvent::TodoUpdate { todos } => self.on_todos(&todos),
            TurnEvent::StreamRollback => {
                // 模型流中断重试：作废的半截输出丢弃，下一段正文另起一条消息。
                // `streamed_any` 一并复位，避免整轮没有可见正文时连兜底提示都不发。
                self.deltas.clear();
                self.all_deltas.clear();
                self.reasoning.clear();
                self.text_message = None;
                self.reasoning_message = None;
                self.streamed_any = false;
                self.last_stream_edit_at = None;
                self.last_reasoning_edit_at = None;
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
            if crate::text::clean_text(&self.deltas).is_empty() {
                return;
            }
            self.seal_reasoning();
            let mut message = TextMessage::new(self.port(), self.chat_id);
            message.stream(&self.deltas.clone());
            self.text_message = Some(message);
            self.streamed_any = true;
            self.last_stream_edit_at = Some(Instant::now());
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
            let mut message = ReasoningMessage::new(self.port(), self.chat_id);
            message.stream(&self.reasoning.clone());
            self.reasoning_message = Some(message);
            self.last_reasoning_edit_at = Some(Instant::now());
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
        self.bot.send(self.chat_id, &format!("⏳ {text}"));
    }

    /// 工具调用：封口当前正文段与思考面板，随后让这次调用独立成一条消息。
    fn on_tool_start(&mut self, step: u64, call: &ToolCall) {
        self.seal_reasoning();
        self.seal_text(None, "");
        let name = if call.name.is_empty() {
            "?"
        } else {
            call.name.as_str()
        };
        let operation = operation_of(name);
        // 提问与执行计划各有独立条目，不产生工具消息（与飞书一致）。
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
        let mut message = ToolMessage::new(self.port(), self.chat_id, record);
        message.start();
        self.tool_messages.insert(key, message);
    }

    fn on_tool_result(&mut self, call: &ToolCall, result: &ToolResult) {
        let operation = operation_of(call.name.as_str());
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
        let chat_id = self.chat_id;
        let message = self
            .subagent_messages
            .entry(batch_id)
            .or_insert_with(|| SubAgentMessage::new(port, chat_id));
        message.update(event_name, payload);
    }

    fn on_todos(&mut self, todos: &Value) {
        if !todos.is_object() {
            return;
        }
        let port = self.port();
        let chat_id = self.chat_id;
        let message = self
            .plan_message
            .get_or_insert_with(|| PlanMessage::new(port, chat_id));
        message.update(todos.get("todos"));
    }

    fn seal_reasoning(&mut self) {
        let Some(mut message) = self.reasoning_message.take() else {
            return;
        };
        message.seal(&self.reasoning.clone());
        self.reasoning.clear();
    }

    /// 封口当前正文段；`text` 为 None 时用流式累积文本，剩余部分补发为普通文本。
    fn seal_text(&mut self, text: Option<String>, suffix: &str) {
        let Some(mut message) = self.text_message.take() else {
            return;
        };
        let content = text.unwrap_or_else(|| self.deltas.clone());
        let remaining = message.seal(&content, suffix);
        self.deltas.clear();
        if let Some(remaining) = remaining {
            self.bot.send(self.chat_id, &remaining);
        }
    }

    /// 整轮没有流式正文时补发一条正文消息（超长按上限分段）。
    fn send_segment(&self, cleaned: &str) {
        self.bot.send(self.chat_id, &format!("◇ {cleaned}"));
    }

    fn abort_running_tools(&mut self) {
        for message in self.tool_messages.values_mut() {
            message.abort();
        }
    }

    fn patch_due(&mut self, stream: bool) -> bool {
        let slot = if stream {
            &mut self.last_stream_edit_at
        } else {
            &mut self.last_reasoning_edit_at
        };
        let now = Instant::now();
        match slot {
            Some(previous)
                if now.duration_since(*previous)
                    < Duration::from_secs_f64(super::format::STREAM_EDIT_INTERVAL_SECONDS) =>
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

    fn cleaned_all_deltas(&self) -> String {
        crate::text::clean_text(&self.all_deltas)
    }

    fn streamed_any(&self) -> bool {
        self.streamed_any
    }

    fn text_message_created(&self) -> bool {
        self.text_message.is_some()
    }

    fn text_message_id(&self) -> Option<i64> {
        self.text_message.as_ref().and_then(TextMessage::message_id)
    }
}

fn field_text(value: &Value, key: &str) -> String {
    match value.get(key) {
        Some(Value::String(text)) => text.trim().to_string(),
        Some(Value::Number(number)) => number.to_string(),
        _ => String::new(),
    }
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
