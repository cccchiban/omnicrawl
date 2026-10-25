//! QQ 机器人服务：网关事件接入、命令分发、被动回复与工具确认桥。
//!
//! 与另外两个连接器最大的差别是**平台形态**：2025-04 起 QQ 关闭了主动消息，所有下发都
//! 必须是对某条入站消息的被动回复（携带 `msg_id`），而且同一条入站消息只有 5 次回复
//! 额度（单聊 60 分钟、群聊 5 分钟）。因此：
//!
//! - 消息**不能编辑**，也没有流式预览：正文段在工具边界处封口成一条完整消息；
//! - 工具调用不各自成条，而是攒成一段「进度」消息，避免几次调用就用光额度；
//! - 进度消息只报「调用了什么、结果如何」，**不下发工具输出**——输出属于内部过程，
//!   端上只留一行 `● 摘要 · ✓ 成功`；
//! - 额度按「每条入站消息最多 5 条」硬性计数，其中**中间过程最多 4 条**，第 5 条固定
//!   留给最终回答（[`ReplyHandle::send_intermediate`] 与 [`ReplyHandle::send_final`]）。
//!   中间过程用满 4 条之后不再发送任何内容，直到最终文本出现。
//!
//! 除这些平台限制外，显示口径与 Telegram/飞书一致：摘要与正文的生成规则仍共用
//! `crate::render`，正文段与进度段各自独立、按发生顺序出现。含 Markdown 语法的段走
//! `msg_type=2`（代码块、表格、深标题等平台不认的写法在 [`super::markdown`] 里就地规整），
//! 回复里的 `[FILE:路径]` 标记转成 `msg_type=7` 富媒体。

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;

use omnicrawl_session::{redact_sensitive_text, redact_sensitive_values};

use super::api::{QqApi, Target, MAX_REPLIES_PER_MESSAGE};
use super::config::{effective_confirm_timeout, validate_config, AccessPolicy, QqBotConfig};
use super::dispatch::{
    classify, parse_thinking_command, route_event_with_bot, workspace_argument, BotIdentity,
    Command, EventRoute, Inbound, ThinkingCommand,
};
use super::files::{classify_file_name, relative_display_path, safe_name};
use super::gateway::{GatewayConnection, GatewaySignal};
use super::markdown;
use super::media::{self, MediaUpload};
use crate::agent::{
    AgentDriver, AskUserHandler, ConfirmHandler, ToolCall, ToolResult, TurnError, TurnEvent,
};
use crate::render::{
    normalize_todos, operation_of, reasoning_body, subagents_text, todos_text, tool_status_icon,
    tool_summary, SubagentNode, ASK_USER_TOOL_NAME, SUBAGENT_EVENT_STATUS,
    SUBAGENT_TERMINAL_STATUSES, TODO_TOOL_NAME,
};
use crate::text::{clean_text, resolve_final_text};

/// 单条文本消息的安全上限（协议没有下发上限，社区实现普遍按 2000 字符保守处理）。
pub const MAX_CONTENT_CHARS: usize = 1800;

/// 中间过程最多占用的额度：留 1 条给最终回答（平台总上限 5 条）。
pub const MAX_INTERMEDIATE_REPLIES: usize = MAX_REPLIES_PER_MESSAGE - 1;

/// 断线重连的退避上限（秒）。
const MAX_BACKOFF_SECONDS: f64 = 30.0;

/// 确认提示里参数 JSON 的最大长度。
const CONFIRM_ARGUMENTS_LIMIT: usize = 1200;

/// 进度段最多列出的工具条目数。
const MAX_PROGRESS_TOOLS: usize = 12;

/// 简洁回复模式的模式标识（与主 Agent 模板 `templates/speaking-style.md` 同名）。
const SPEAKING_STYLE_MODE: &str = "speaking-style";

/// 平台下发入口：发消息、上传并发送富媒体、下载附件。
///
/// 真实实现是 [`QqApi`]，测试用记录型桩件替换。
pub trait QqPort: Send + Sync {
    fn send(
        &self,
        target: &Target,
        content: &str,
        msg_id: &str,
        msg_seq: i64,
    ) -> Result<String, String>;

    /// 发一条 Markdown（`msg_type=2`）。
    fn send_markdown(
        &self,
        target: &Target,
        content: &str,
        msg_id: &str,
        msg_seq: i64,
    ) -> Result<String, String>;

    /// 上传一个文件并作为富媒体（`msg_type=7`）发出去。
    fn send_media(
        &self,
        target: &Target,
        upload: &MediaUpload,
        msg_id: &str,
        msg_seq: i64,
    ) -> Result<String, String>;

    fn download(&self, url: &str) -> Result<Vec<u8>, String>;
}

impl QqPort for QqApi {
    fn send(
        &self,
        target: &Target,
        content: &str,
        msg_id: &str,
        msg_seq: i64,
    ) -> Result<String, String> {
        self.send_text(target, content, msg_id, msg_seq)
            .map_err(|error| error.to_string())
    }

    fn send_markdown(
        &self,
        target: &Target,
        content: &str,
        msg_id: &str,
        msg_seq: i64,
    ) -> Result<String, String> {
        QqApi::send_markdown(self, target, content, msg_id, msg_seq)
            .map_err(|error| error.to_string())
    }

    fn send_media(
        &self,
        target: &Target,
        upload: &MediaUpload,
        msg_id: &str,
        msg_seq: i64,
    ) -> Result<String, String> {
        let file_info = self
            .upload_media(target, upload.file_type(), &upload.file_name, &upload.bytes)
            .map_err(|error| error.to_string())?;
        QqApi::send_media(self, target, &file_info, msg_id, msg_seq)
            .map_err(|error| error.to_string())
    }

    fn download(&self, url: &str) -> Result<Vec<u8>, String> {
        QqApi::download(self, url).map_err(|error| error.to_string())
    }
}

/// 一条入站消息的被动回复句柄：`msg_seq` 递增、额度硬性递减。
///
/// 额度口径：平台对同一条入站消息最多接受 5 条回复，其中**中间过程最多 4 条**
/// （[`ReplyHandle::send_intermediate`]），第 5 条留给最终回答
/// （[`ReplyHandle::send_final`]）。中间额度用尽后 [`ReplyHandle::can_send_intermediate`]
/// 返回 false，调用方必须继续攒内容、直到最终文本才下发。
pub struct ReplyHandle {
    port: Arc<dyn QqPort>,
    pub target: Target,
    /// 触发本次回复的入站消息 ID。
    pub msg_id: String,
    seq: AtomicI64,
    slots_left: AtomicUsize,
    intermediate_left: AtomicUsize,
}

impl ReplyHandle {
    pub fn new(port: Arc<dyn QqPort>, target: Target, msg_id: &str) -> ReplyHandle {
        ReplyHandle {
            port,
            target,
            msg_id: msg_id.to_string(),
            seq: AtomicI64::new(1),
            slots_left: AtomicUsize::new(MAX_REPLIES_PER_MESSAGE),
            intermediate_left: AtomicUsize::new(MAX_INTERMEDIATE_REPLIES),
        }
    }

    /// 剩余总额度。
    pub fn slots_left(&self) -> usize {
        self.slots_left.load(Ordering::SeqCst)
    }

    /// 剩余中间额度（已含为最终回答预留的一条之外）。
    pub fn intermediate_left(&self) -> usize {
        self.intermediate_left.load(Ordering::SeqCst)
    }

    /// 还能否发中间消息：中间额度与总额度都还有余量。
    pub fn can_send_intermediate(&self) -> bool {
        self.intermediate_left() > 0 && self.slots_left() > 1
    }

    /// 发一段中间内容（正文段、进度、思考）：占中间额度。
    ///
    /// 返回实际发出的条数；`0` 表示额度已用尽或平台报错。内容里的 Markdown 语法走
    /// `msg_type=2`，`[FILE:…]` 标记转成富媒体。
    pub fn send_intermediate(&self, text: &str) -> usize {
        self.send_parts(text, true)
    }

    /// 发最终回答：只受总额度约束（最多两段，末段写满即止）。
    pub fn send_final(&self, text: &str) -> usize {
        self.send_parts(text, false)
    }

    /// 按上限切段并逐段下发（富媒体标记先摘出来单独发）。
    ///
    /// 每发一条都在下发前预扣额度（[`ReplyHandle::reserve`]），这样一次调用切成多段
    /// 也不会越过中间过程的上限。
    fn send_parts(&self, text: &str, intermediate: bool) -> usize {
        let mut sent = 0_usize;
        let stripped: String;
        let mut text = text;
        if media::supports(&self.target) {
            let mut any_sent = false;
            let mut all_sent = true;
            for path in media::marker_paths(text) {
                match media::load(&path) {
                    Ok(upload) => {
                        if self.deliver_media(&upload, intermediate) {
                            any_sent = true;
                            sent += 1;
                        } else {
                            all_sent = false;
                        }
                    }
                    Err(error) => {
                        // 发不出去时保留文本路径，用户至少知道文件在哪。
                        eprintln!("[qqbot] 富媒体未发送：{error}");
                        all_sent = false;
                    }
                }
            }
            // 全部发成功才从正文里摘掉标记，否则保留路径给用户看。
            if any_sent && all_sent {
                stripped = media::strip_markers(text);
                text = &stripped;
            }
        }
        let parts = split_content(text);
        for (index, part) in parts.iter().enumerate() {
            match self.deliver_text(part, intermediate) {
                Some(true) => sent += 1,
                Some(false) => break,
                None => {
                    eprintln!(
                        "[qqbot] 回复额度已用尽，跳过后续 {} 段内容。",
                        parts.len() - index
                    );
                    break;
                }
            }
        }
        sent
    }

    /// 预扣一次下发额度：中间过程同时扣中间额度与总额度，最终回答只扣总额度。
    fn reserve(&self, intermediate: bool) -> bool {
        if intermediate {
            loop {
                let left = self.intermediate_left.load(Ordering::SeqCst);
                if left == 0 {
                    return false;
                }
                if self
                    .intermediate_left
                    .compare_exchange(left, left - 1, Ordering::SeqCst, Ordering::SeqCst)
                    .is_ok()
                {
                    break;
                }
            }
        }
        loop {
            let left = self.slots_left.load(Ordering::SeqCst);
            if left == 0 {
                if intermediate {
                    // 总额度先耗尽：把刚扣的中间额度还回去。
                    self.intermediate_left.fetch_add(1, Ordering::SeqCst);
                }
                return false;
            }
            if self
                .slots_left
                .compare_exchange(left, left - 1, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                return true;
            }
        }
    }

    /// 纯文本还是 Markdown：单聊/群聊且正文含平台支持的语法时走 `markdown.content`。
    ///
    /// 返回 `None` 表示额度已用尽，`Some(false)` 表示平台报错或额度在重试时耗尽。
    fn deliver_text(&self, text: &str, intermediate: bool) -> Option<bool> {
        if !self.reserve(intermediate) {
            return None;
        }
        let markdown = if markdown::supports(&self.target) {
            markdown::to_markdown(text)
        } else {
            None
        };
        let seq = self.next_seq();
        match markdown {
            Some(body) => match self
                .port
                .send_markdown(&self.target, &body, &self.msg_id, seq)
            {
                Ok(_id) => Some(true),
                Err(error) => {
                    // Markdown 被拒（无权限、内容违规等）时按纯文本再试一次，正文不能丢。
                    // 重试同样要占额度：先预扣再发，扣不到就当作本条没发出去。
                    eprintln!("[qqbot] Markdown 消息发送失败：{error}");
                    if !self.reserve(intermediate) {
                        return Some(false);
                    }
                    Some(self.send_plain(text))
                }
            },
            None => Some(self.send_plain(text)),
        }
    }

    fn send_plain(&self, text: &str) -> bool {
        let seq = self.next_seq();
        match self.port.send(&self.target, text, &self.msg_id, seq) {
            Ok(_id) => true,
            Err(error) => {
                eprintln!("[qqbot] 发送消息失败：{error}");
                false
            }
        }
    }

    fn deliver_media(&self, upload: &MediaUpload, intermediate: bool) -> bool {
        if !self.reserve(intermediate) {
            return false;
        }
        let seq = self.next_seq();
        match self
            .port
            .send_media(&self.target, upload, &self.msg_id, seq)
        {
            Ok(_id) => true,
            Err(error) => {
                eprintln!("[qqbot] 富媒体发送失败（{}）：{error}", upload.file_name);
                false
            }
        }
    }

    fn next_seq(&self) -> i64 {
        self.seq.fetch_add(1, Ordering::SeqCst)
    }
}

/// 按上限切段：优先按行边界切，单行超长硬切。
pub fn split_content(text: &str) -> Vec<String> {
    let trimmed = text.trim_end();
    let text = if trimmed.is_empty() { text } else { trimmed };
    if text.is_empty() {
        return Vec::new();
    }
    let count = |value: &str| value.chars().count();
    if count(text) <= MAX_CONTENT_CHARS {
        return vec![text.to_string()];
    }
    let mut parts: Vec<String> = Vec::new();
    let mut current = String::new();
    for line in text.split('\n') {
        let separator = usize::from(!current.is_empty());
        if count(&current) + separator + count(line) > MAX_CONTENT_CHARS && !current.is_empty() {
            parts.push(current.clone());
            current.clear();
        }
        if current.is_empty() {
            current.push_str(line);
        } else {
            current.push('\n');
            current.push_str(line);
        }
    }
    if !current.is_empty() {
        parts.push(current);
    }
    let mut final_parts: Vec<String> = Vec::new();
    for part in parts {
        let mut rest: Vec<char> = part.chars().collect();
        while rest.len() > MAX_CONTENT_CHARS {
            final_parts.push(rest[..MAX_CONTENT_CHARS].iter().collect());
            rest = rest[MAX_CONTENT_CHARS..].to_vec();
        }
        if !rest.is_empty() {
            final_parts.push(rest.into_iter().collect());
        }
    }
    final_parts
}

/// 一次等待用户批准的工具调用。
struct PendingConfirm {
    tool_name: String,
    sender: String,
    decision: Mutex<Option<bool>>,
    signal: Condvar,
}

impl PendingConfirm {
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
    sender: String,
    answer: Mutex<Option<String>>,
    signal: Condvar,
}

impl PendingQuestion {
    /// 提问提示：没有选项时只提示直接回复，有选项时列出编号。
    fn prompt(&self) -> String {
        if self.options.is_empty() {
            let mut text = format!("❓ {}\n请直接回复答案。", self.question);
            if self.kind == "confirm" {
                text.push_str("\n（例如：是 / 否）");
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

/// 当前正在执行的任务。
pub struct ActiveTask {
    pub sender: String,
    pub text: String,
    pub reply: Arc<ReplyHandle>,
    pub cancel: Arc<AtomicBool>,
    pub started_at: Instant,
    running: Arc<AtomicBool>,
}

#[derive(Default)]
struct BotState {
    active: Option<Arc<ActiveTask>>,
    pending: Option<Arc<PendingConfirm>>,
    question: Option<Arc<PendingQuestion>>,
}

/// 网关会话（Resume 要用）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct GatewaySession {
    session_id: String,
    seq: i64,
}

/// 一个 QQ 机器人：驱动单个 OmniCrawl Agent。
pub struct QqBot<D: AgentDriver> {
    api: Arc<QqApi>,
    port: Arc<dyn QqPort>,
    driver: Arc<D>,
    access: AccessPolicy,
    confirm_timeout: Duration,
    intents: i64,
    state: Mutex<BotState>,
    stopped: AtomicBool,
    show_thinking: AtomicBool,
    session: Mutex<GatewaySession>,
    /// 机器人自身 openid（`READY.user.id`）：兼容 `<@openid>` 占位符写法。
    bot_openid: Mutex<String>,
    /// 机器人昵称（`READY.user.username`）：全量群消息把 @ 渲染成 `@昵称` 文本。
    bot_name: Mutex<String>,
}

impl<D: AgentDriver> QqBot<D> {
    /// 构造 Bot：AppID/AppSecret 必填，超时下限 1 秒。
    pub fn new(config: &QqBotConfig, api: Arc<QqApi>, driver: Arc<D>) -> Result<QqBot<D>, String> {
        validate_config(config)?;
        let port: Arc<dyn QqPort> = api.clone();
        Ok(QqBot {
            api,
            port,
            driver,
            access: config.access(),
            confirm_timeout: Duration::from_secs_f64(effective_confirm_timeout(
                config.confirm_timeout_seconds,
            )),
            intents: config.intents,
            state: Mutex::new(BotState::default()),
            stopped: AtomicBool::new(false),
            show_thinking: AtomicBool::new(false),
            session: Mutex::new(GatewaySession::default()),
            bot_openid: Mutex::new(String::new()),
            bot_name: Mutex::new(String::new()),
        })
    }

    /// 测试与自定义传输用：替换平台端口。
    pub fn with_port(mut self, port: Arc<dyn QqPort>) -> QqBot<D> {
        self.port = port;
        self
    }

    /// 把自身注册成宿主的工具确认桥与提问桥。
    pub fn bind_handlers(self: &Arc<Self>) {
        self.driver.set_confirm_handler(self.clone());
        self.driver.set_ask_user_handler(self.clone());
    }

    /// 绑定确认桥并进入网关主循环，直到 [`QqBot::request_stop`]。
    pub fn run(self: Arc<Self>) -> Result<(), String> {
        self.bind_handlers();
        // 远程端默认要短回复：平台单条消息有上限、回复条数也有限，长文体验很差。
        self.enable_default_speaking_style();
        self.run_forever()
    }

    /// 启动时默认开启简洁回复模式；已有其他模式时不动，失败只记日志不阻断启动。
    pub fn enable_default_speaking_style(&self) {
        let active = self.driver.active_mode();
        if !active.is_empty() {
            if active != SPEAKING_STYLE_MODE {
                eprintln!("[qqbot] 当前已启用模式 {active}，跳过默认简洁回复模式。");
            }
            return;
        }
        match self.driver.activate_mode(SPEAKING_STYLE_MODE) {
            Ok(_) => eprintln!("[qqbot] 已默认开启简洁回复模式（/speaking-style off 可关闭）。"),
            Err(error) => eprintln!("[qqbot] 默认开启简洁回复模式失败：{error}"),
        }
    }

    /// 网关主循环：取 token → 取网关地址 → 连接 → 收事件，断线按退避重连。
    pub fn run_forever(self: &Arc<Self>) -> Result<(), String> {
        if let Some(warning) = self.access.startup_warning() {
            eprintln!("[qqbot] {warning}");
        }
        let mut backoff = 1.0_f64;
        while !self.is_stopped() {
            let outcome = self.run_session();
            if self.is_stopped() {
                break;
            }
            match outcome {
                Ok(()) => break,
                Err(error) => {
                    eprintln!("[qqbot] 网关连接中断：{error}，{backoff:.0} 秒后重连");
                    if self.sleep_until_stopped(backoff) {
                        break;
                    }
                    backoff = (backoff * 2.0).min(MAX_BACKOFF_SECONDS);
                }
            }
        }
        eprintln!("[qqbot] QQ 机器人已停止。");
        Ok(())
    }

    fn run_session(self: &Arc<Self>) -> Result<(), String> {
        let token = self.api.access_token().map_err(|error| error.to_string())?;
        let url = self.api.gateway_url().map_err(|error| error.to_string())?;
        let stream =
            crate::ws::connect_wss(&url, super::gateway::READ_TIMEOUT, super::api::USER_AGENT)?;
        let resume = {
            let session = self.session.lock().expect("会话锁中毒");
            if session.session_id.is_empty() {
                None
            } else {
                Some((session.session_id.clone(), Some(session.seq)))
            }
        };
        let resume_arg = resume
            .as_ref()
            .map(|(session_id, seq)| (session_id.as_str(), *seq));
        let mut connection = GatewayConnection::connect(stream, &token, self.intents, resume_arg)?;
        eprintln!(
            "[qqbot] 网关已连接（心跳 {:?}{}）",
            connection.heartbeat_interval,
            if resume.is_some() {
                "，恢复会话"
            } else {
                ""
            }
        );
        loop {
            if self.is_stopped() {
                return Ok(());
            }
            let signal = match connection.poll() {
                Ok(Some(signal)) => signal,
                Ok(None) => continue,
                Err(error) => return Err(error),
            };
            self.remember_seq(connection.last_seq);
            match signal {
                GatewaySignal::Dispatch { event, data, .. } => {
                    if event == "READY" {
                        let session_id = data
                            .get("session_id")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string();
                        // 全量模式靠机器人自身身份判断哪条群消息 @ 了它：平台把 @ 渲染成
                        // `@昵称` 文本，所以昵称是关键；openid 用于兼容占位符写法。
                        // 优先用 READY，缺字段时退回 /users/@me。
                        let mut id = data
                            .get("user")
                            .and_then(|user| user.get("id"))
                            .and_then(Value::as_str)
                            .filter(|value| !value.is_empty())
                            .map(|value| value.to_string());
                        let mut name = data
                            .get("user")
                            .and_then(|user| user.get("username"))
                            .and_then(Value::as_str)
                            .filter(|value| !value.is_empty())
                            .map(|value| value.to_string());
                        if id.is_none() || name.is_none() {
                            if let Ok(payload) = self.api.me() {
                                if id.is_none() {
                                    id = payload
                                        .get("id")
                                        .and_then(Value::as_str)
                                        .filter(|value| !value.is_empty())
                                        .map(|value| value.to_string());
                                }
                                if name.is_none() {
                                    name = payload
                                        .get("username")
                                        .and_then(Value::as_str)
                                        .filter(|value| !value.is_empty())
                                        .map(|value| value.to_string());
                                }
                            }
                        }
                        {
                            if let Ok(mut slot) = self.bot_openid.lock() {
                                *slot = id.unwrap_or_default();
                            }
                            if let Ok(mut slot) = self.bot_name.lock() {
                                *slot = name.unwrap_or_default();
                            }
                        }
                        let id_text = self
                            .bot_openid
                            .lock()
                            .map(|value| value.clone())
                            .unwrap_or_default();
                        let name_text = self
                            .bot_name
                            .lock()
                            .map(|value| value.clone())
                            .unwrap_or_default();
                        if name_text.is_empty() {
                            eprintln!(
                                "[qqbot] 未取到机器人昵称：全量群消息可能无法识别 @（openid={id_text}）"
                            );
                        } else {
                            eprintln!(
                                "[qqbot] 机器人身份：{name_text}（openid={id_text}），用于识别群内 @"
                            );
                        }
                        if let Ok(mut session) = self.session.lock() {
                            session.session_id = session_id;
                        }
                        eprintln!("[qqbot] 登录成功，开始接收事件。");
                        continue;
                    }
                    if event == "RESUMED" {
                        eprintln!("[qqbot] 会话已恢复，补发事件接收完成。");
                        continue;
                    }
                    self.handle_event(&event, &data);
                }
                GatewaySignal::Reconnect => return Err("服务端要求重连".to_string()),
                GatewaySignal::InvalidSession { resumable } => {
                    if !resumable {
                        if let Ok(mut session) = self.session.lock() {
                            *session = GatewaySession::default();
                        }
                    }
                    return Err(if resumable {
                        "会话失效（可恢复）".to_string()
                    } else {
                        "会话失效（需重新鉴权）".to_string()
                    });
                }
                GatewaySignal::HeartbeatAck | GatewaySignal::Ignored => {}
            }
        }
    }

    fn remember_seq(&self, seq: Option<i64>) {
        let Some(seq) = seq else { return };
        if let Ok(mut session) = self.session.lock() {
            session.seq = seq;
        }
    }

    /// 处理一帧事件（`READY` / `RESUMED` 已在主循环里消化）。
    pub fn handle_event(self: &Arc<Self>, event: &str, data: &Value) {
        let access = self.access.clone();
        let bot_openid = self
            .bot_openid
            .lock()
            .map(|id| id.clone())
            .unwrap_or_default();
        let bot_name = self
            .bot_name
            .lock()
            .map(|name| name.clone())
            .unwrap_or_default();
        let bot = BotIdentity {
            openid: &bot_openid,
            name: &bot_name,
        };
        match route_event_with_bot(event, data, &access, bot) {
            EventRoute::NotAMessage { event, conversation } => {
                if conversation.is_empty() {
                    eprintln!("[qqbot] 忽略非消息事件：{event}");
                } else {
                    eprintln!("[qqbot] 忽略非消息事件：{event}（在 {conversation}）");
                }
            }
            EventRoute::NotAddressed {
                event,
                conversation,
                detail,
            } => {
                eprintln!(
                    "[qqbot] 群消息未 @ 机器人，忽略：{conversation}（{event}）；{detail}"
                );
            }
            EventRoute::Malformed { event } => {
                eprintln!("[qqbot] 事件缺少必需字段：{event}");
            }
            EventRoute::Unauthorized {
                sender,
                sender_name,
                conversation,
                hint,
                event,
            } => {
                let who = if sender_name.is_empty() {
                    sender.clone()
                } else {
                    format!("{sender_name}（{sender}）")
                };
                eprintln!(
                    "[qqbot] 未授权用户 {who} 在 {conversation} 的 {event} 被忽略（{hint}）。"
                );
            }
            EventRoute::Message(inbound) => self.handle_message(&inbound),
        }
    }

    /// 一条入站消息：先回答挂起的提问，再分发命令或启动任务。
    fn handle_message(self: &Arc<Self>, inbound: &Inbound) {
        let reply = Arc::new(ReplyHandle::new(
            Arc::clone(&self.port),
            inbound.target.clone(),
            &inbound.message_id,
        ));
        if self.resolve_pending_question(inbound) {
            return;
        }
        if inbound.content.is_empty() && inbound.attachments.is_empty() {
            return;
        }
        if !inbound.attachments.is_empty() {
            self.spawn_file_task(inbound.clone(), reply);
            return;
        }
        self.dispatch(inbound, reply);
    }

    /// 挂起提问在等答案时，这条消息就是答案。
    fn resolve_pending_question(&self, inbound: &Inbound) -> bool {
        let question = {
            let state = self.state.lock().expect("状态锁中毒");
            state.question.clone()
        };
        let Some(question) = question else {
            return false;
        };
        if question.sender != inbound.sender || inbound.content.trim().is_empty() {
            return false;
        }
        question.resolve(Some(match_answer(&question.options, &inbound.content)));
        true
    }

    /// 命令与任务分发。
    pub fn dispatch(self: &Arc<Self>, inbound: &Inbound, reply: Arc<ReplyHandle>) {
        match classify(&inbound.content) {
            Command::Start => {
                reply.send_final(&self.help_text());
            }
            Command::Status => {
                reply.send_final(&self.status_text());
            }
            Command::Session => {
                let status = self.driver.status();
                reply.send_final(&format!(
                    "🧵 当前会话：{}\n工作区：{}",
                    status.session_id, status.workspace_root
                ));
            }
            Command::Reset => match self.driver.reset_conversation() {
                Ok(()) => {
                    reply.send_final("✅ 已开启新会话。");
                }
                Err(error) => {
                    reply.send_final(&format!(
                        "❌ 重置会话失败：{}",
                        redact_sensitive_text(&error)
                    ));
                }
            },
            Command::Cancel => self.request_cancel(inbound),
            Command::Approve => self.handle_approval(inbound, true),
            Command::Reject => self.handle_approval(inbound, false),
            Command::Thinking => self.handle_thinking(&inbound.content, &reply),
            Command::Workspace => self.handle_workspace(&inbound.content, &reply),
            Command::Harness => match self.driver.handle_command(&inbound.content, "qqbot") {
                Ok(Some(text)) => {
                    reply.send_final(&text);
                }
                Ok(None) => {
                    let suggestion = suggest_command(&inbound.content, &self.driver.command_menu());
                    eprintln!(
                        "[qqbot] 宿主不认识该命令：{}；建议={}",
                        inbound.content,
                        suggestion.as_deref().unwrap_or("无")
                    );
                    reply.send_final(&match suggestion {
                        Some(name) => {
                            format!("未知命令。是不是想用 /{name}？发送 /start 查看可用命令。")
                        }
                        None => "未知命令。发送 /start 查看可用命令。".to_string(),
                    });
                }
                Err(error) => {
                    eprintln!("[qqbot] 宿主命令执行失败：{}", inbound.content);
                    reply.send_final(&format!(
                        "❌ 命令执行失败：{}",
                        redact_sensitive_text(&error)
                    ));
                }
            },
            Command::Task => self.start_task(inbound.clone(), reply),
        }
    }

    fn reply_for(&self, inbound: &Inbound) -> ReplyHandle {
        ReplyHandle::new(
            Arc::clone(&self.port),
            inbound.target.clone(),
            &inbound.message_id,
        )
    }

    /// 单活动校验后，在后台线程执行 Agent 任务。
    pub fn start_task(self: &Arc<Self>, inbound: Inbound, reply: Arc<ReplyHandle>) {
        {
            let mut state = self.state.lock().expect("状态锁中毒");
            if let Some(active) = state.active.clone() {
                if active.running.load(Ordering::SeqCst) {
                    drop(state);
                    reply.send_final("🔄 当前已有任务正在执行，请等待完成或发送 /cancel 取消。");
                    return;
                }
            }
            let task = Arc::new(ActiveTask {
                sender: inbound.sender.clone(),
                text: inbound.content.clone(),
                reply: Arc::clone(&reply),
                cancel: Arc::new(AtomicBool::new(false)),
                started_at: Instant::now(),
                running: Arc::new(AtomicBool::new(true)),
            });
            state.active = Some(Arc::clone(&task));
            let bot = self.clone();
            let spawned = thread::Builder::new()
                .name("omnicrawl-qqbot-task".to_string())
                .spawn(move || bot.execute_task(task));
            if let Err(error) = spawned {
                eprintln!("[qqbot] 任务线程启动失败：{error}");
                self.state.lock().expect("状态锁中毒").active = None;
                reply.send_final("❌ 任务线程启动失败。");
                return;
            }
        }
        reply.send_intermediate("✅ 已收到任务，开始执行（/cancel 可取消）。");
    }

    /// 任务线程：驱动回合，并把事件映射成按发生顺序发出的消息。
    fn execute_task(self: &Arc<Self>, task: Arc<ActiveTask>) {
        let mut timeline = TaskTimeline::new(self, Arc::clone(&task.reply));
        let result = {
            let mut events = |event: TurnEvent| timeline.handle(event);
            self.driver.run_turn(&task.text, &mut events)
        };
        let cancelled = task.cancel.load(Ordering::SeqCst);
        self.finish_task(task.as_ref(), &mut timeline, result, cancelled);
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
        timeline: &mut TaskTimeline<D>,
        result: Result<String, TurnError>,
        cancelled: bool,
    ) {
        if cancelled {
            timeline.flush_intermediate();
            self.abort_task(task, "⏹ 任务已取消。");
            return;
        }
        match result {
            Ok(reply) => {
                let raw = if reply.is_empty() {
                    timeline.all_deltas()
                } else {
                    reply
                };
                timeline.flush_intermediate();
                timeline.finish(&raw);
                if !timeline.sent_any {
                    task.reply.send_final("（任务完成，无文本输出）");
                }
            }
            Err(TurnError::Cancelled) => {
                timeline.flush_intermediate();
                self.abort_task(task, "⏹ 任务已取消。");
            }
            Err(TurnError::Failed(message)) => {
                timeline.flush_intermediate();
                eprintln!("[qqbot] 任务执行失败：{message}");
                task.reply.send_final(&format!(
                    "❌ 任务执行失败：{}",
                    redact_sensitive_text(&message)
                ));
            }
        }
    }

    /// 保留已输出的正文段，错误或取消提示单独发送。
    fn abort_task(&self, task: &ActiveTask, error_text: &str) {
        task.reply.send_final(error_text);
    }

    /// `/approve` 与 `/reject`：只有发起任务的白名单用户本人可以批准或拒绝。
    pub fn handle_approval(self: &Arc<Self>, inbound: &Inbound, approve: bool) {
        let pending = {
            let state = self.state.lock().expect("状态锁中毒");
            state.pending.clone()
        };
        let Some(pending) = pending else {
            return;
        };
        if pending.sender != inbound.sender {
            return;
        }
        pending.resolve(approve);
        let action = if approve { "已批准" } else { "已拒绝" };
        self.reply_for(inbound)
            .send_final(&format!("✅ {action}：{}", pending.tool_name));
    }

    /// 请求取消当前任务；无活动任务时仅提示。
    pub fn request_cancel(self: &Arc<Self>, inbound: &Inbound) {
        let (task, pending, question) = {
            let state = self.state.lock().expect("状态锁中毒");
            (
                state.active.clone(),
                state.pending.clone(),
                state.question.clone(),
            )
        };
        let reply = self.reply_for(inbound);
        let Some(task) = task else {
            reply.send_final("当前没有正在执行的任务。");
            return;
        };
        if !task.running.load(Ordering::SeqCst) {
            reply.send_final("当前没有正在执行的任务。");
            return;
        }
        task.cancel.store(true, Ordering::SeqCst);
        self.driver.request_cancel();
        if let Some(pending) = pending {
            if pending.sender == inbound.sender {
                pending.resolve(false);
            }
        }
        if let Some(question) = question {
            if question.sender == inbound.sender {
                question.resolve(None);
            }
        }
        reply.send_final("⏹ 已请求取消当前任务，请稍候……");
    }

    fn handle_thinking(self: &Arc<Self>, text: &str, reply: &ReplyHandle) {
        match parse_thinking_command(text) {
            ThinkingCommand::Query => {
                let state = if self.show_thinking() {
                    "开启"
                } else {
                    "关闭"
                };
                reply.send_final(&format!(
                    "思考内容显示：{state}（默认关闭）。\n/thinking on 开启，/thinking off 关闭。"
                ));
            }
            ThinkingCommand::Enable => {
                self.show_thinking.store(true, Ordering::SeqCst);
                let _ = reply.send_final("已开启思考内容显示（🧠 独立消息）。");
            }
            ThinkingCommand::Disable => {
                self.show_thinking.store(false, Ordering::SeqCst);
                let _ = reply.send_final("已关闭思考内容显示。");
            }
            ThinkingCommand::Usage => {
                let _ = reply.send_final("用法：/thinking on 或 /thinking off。");
            }
        }
    }

    fn handle_workspace(self: &Arc<Self>, text: &str, reply: &ReplyHandle) {
        let Some(path) = workspace_argument(text) else {
            let root = self.driver.status().workspace_root;
            reply.send_final(&format!("当前工作区：{root}\n用法：/workspace <路径>"));
            return;
        };
        match self.driver.switch_workspace(&path) {
            Ok(switched) => {
                reply.send_final(&format!(
                    "✅ 已切换工作区：{}{}",
                    switched.workspace_root, switched.note
                ));
            }
            Err(error) => {
                reply.send_final(&format!(
                    "❌ 切换工作区失败：{}",
                    redact_sensitive_text(&error)
                ));
            }
        }
    }

    /// 附件消息：后台线程下载 → 分类落盘 → 交给 Agent。
    fn spawn_file_task(self: &Arc<Self>, inbound: Inbound, reply: Arc<ReplyHandle>) {
        let bot = self.clone();
        let spawned = thread::Builder::new()
            .name("omnicrawl-qqbot-file".to_string())
            .spawn(move || bot.process_attachments(inbound, reply));
        if let Err(error) = spawned {
            eprintln!("[qqbot] 附件处理线程启动失败：{error}");
        }
    }

    /// 下载 → 分类存放 → 通知用户 → 启动任务；各步失败都回传脱敏错误。
    pub fn process_attachments(self: &Arc<Self>, inbound: Inbound, reply: Arc<ReplyHandle>) {
        let temp_root = self.driver.temp_root();
        let workspace_root = PathBuf::from(self.driver.status().workspace_root);
        let stamp = super::files::unix_seconds();
        let mut saved: Vec<String> = Vec::new();
        for (index, (url, name)) in inbound.attachments.iter().enumerate() {
            let content = match self.port.download(url) {
                Ok(content) => content,
                Err(error) => {
                    eprintln!("[qqbot] 附件下载失败：{error}");
                    reply.send_final(&format!(
                        "❌ 附件下载失败：{}",
                        redact_sensitive_text(&error)
                    ));
                    return;
                }
            };
            let file_name = safe_name(name, url, stamp + index as i64, "");
            let subdir = classify_file_name(&file_name);
            let destination = match super::files::destination(&temp_root, subdir, &file_name) {
                Ok(path) => path,
                Err(error) => {
                    eprintln!("[qqbot] 附件保存失败：{error}");
                    reply.send_final(&format!(
                        "❌ 附件保存失败：{}",
                        redact_sensitive_text(&error.to_string())
                    ));
                    return;
                }
            };
            if let Err(error) = fs::write(&destination, &content) {
                eprintln!("[qqbot] 附件保存失败：{error}");
                reply.send_final(&format!(
                    "❌ 附件保存失败：{}",
                    redact_sensitive_text(&error.to_string())
                ));
                return;
            }
            saved.push(relative_display_path(&destination, &workspace_root));
        }
        if saved.is_empty() {
            return;
        }
        reply.send_final(&format!(
            "✅ 已收到 {} 个附件：\n{}",
            saved.len(),
            saved.join("\n")
        ));
        if self.is_stopped() {
            eprintln!("[qqbot] 服务已停止，跳过附件任务启动。");
            return;
        }
        let mut task_text = format!("已收到附件：位于 {}", saved.join("、"));
        if !inbound.content.is_empty() {
            task_text.push_str(&format!("\n\n用户补充说明：{}", inbound.content));
        }
        self.start_task(
            Inbound {
                target: inbound.target.clone(),
                sender: inbound.sender.clone(),
                sender_name: inbound.sender_name.clone(),
                message_id: inbound.message_id.clone(),
                content: task_text,
                attachments: Vec::new(),
                event: inbound.event.clone(),
            },
            reply,
        );
    }

    pub fn show_thinking(&self) -> bool {
        self.show_thinking.load(Ordering::SeqCst)
    }

    pub fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::SeqCst)
    }

    /// 停止网关循环；挂起的确认与提问一并释放，避免任务线程一直等到超时。
    pub fn request_stop(&self) {
        self.stopped.store(true, Ordering::SeqCst);
        let (pending, question) = {
            let state = self.state.lock().expect("状态锁中毒");
            (state.pending.clone(), state.question.clone())
        };
        if let Some(pending) = pending {
            pending.resolve(false);
        }
        if let Some(question) = question {
            question.resolve(None);
        }
    }

    /// 是否有任务在跑（`/status` 与单活动校验共用）。
    pub fn is_busy(&self) -> bool {
        let state = self.state.lock().expect("状态锁中毒");
        state
            .active
            .as_ref()
            .map(|task| task.running.load(Ordering::SeqCst))
            .unwrap_or(false)
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
                lines.push(format!(
                    "任务已运行 {} 秒，/cancel 可取消。",
                    task.started_at.elapsed().as_secs()
                ));
            }
        }
        lines.join("\n")
    }

    fn help_text(&self) -> String {
        HELP_TEXT.to_string()
    }
}

const HELP_TEXT: &str = "\
🤖 OmniCrawl QQ 机器人

直接发消息即可下达任务；常用命令：
/status 查看工作区与会话
/new 开启新会话
/cancel 取消当前任务
/workspace <路径> 切换工作区
/thinking on|off 显示或隐藏思考内容
/speaking-style on|off 开关简洁回复模式（默认开启）
/approve 批准敏感工具调用，/reject 拒绝
/start 显示本帮助

平台限制：QQ 只允许被动回复，同一条消息最多回 5 条，因此长任务会合并成少数几条消息。";

/// 未知命令时猜最接近的可用命令（取命令词，忽略 `@机器人` 后缀与参数）。
///
/// 只做提示、不自动执行：把 `/reset` 之类的拼写近似直接当命令跑，代价远高于少点一下。
fn suggest_command(text: &str, menu: &[(String, String)]) -> Option<String> {
    let token = text
        .trim()
        .strip_prefix('/')?
        .split_whitespace()
        .next()
        .unwrap_or("")
        .split('@')
        .next()
        .unwrap_or("");
    if token.is_empty() {
        return None;
    }
    let typed: Vec<char> = token.to_lowercase().chars().collect();
    // 短命令一个字符之差就可能撞上另一条命令（`/new` 与 `/rew`），因此按长度放宽阈值。
    let limit = match typed.len() {
        0..=4 => return None,
        5..=8 => 1,
        _ => 2,
    };
    menu.iter()
        .map(|(name, _)| (edit_distance(&typed, &name.to_lowercase()), name))
        .filter(|(distance, _)| *distance > 0 && *distance <= limit)
        .min_by_key(|(distance, _)| *distance)
        .map(|(_, name)| name.to_string())
}

/// 标准莱文斯坦距离（按字符计）。
fn edit_distance(left: &[char], right: &str) -> usize {
    let target: Vec<char> = right.chars().collect();
    let mut previous: Vec<usize> = (0..=target.len()).collect();
    for (i, left_char) in left.iter().enumerate() {
        let mut current = vec![i + 1];
        for (j, right_char) in target.iter().enumerate() {
            let cost = if left_char == right_char { 0 } else { 1 };
            current.push(
                (previous[j] + cost)
                    .min(previous[j + 1] + 1)
                    .min(current[j] + 1),
            );
        }
        previous = current;
    }
    previous[target.len()]
}

/// 从提问选项里解析用户答案：序号、选项正文与自由文本都接受。
fn match_answer(options: &[String], text: &str) -> String {
    let trimmed = text.trim();
    if let Ok(index) = trimmed.parse::<usize>() {
        if index >= 1 && index <= options.len() {
            return options[index - 1].clone();
        }
    }
    trimmed.to_string()
}

/// 单个回合的显示装配：正文段在工具边界封口，工具/计划/子任务攒成一段进度。
struct TaskTimeline<'a, D: AgentDriver> {
    bot: &'a QqBot<D>,
    reply: Arc<ReplyHandle>,
    deltas: String,
    all_deltas: String,
    reasoning: String,
    pending_tools: Vec<String>,
    todos: Vec<(String, bool)>,
    subagent_order: Vec<String>,
    subagent_nodes: BTreeMap<String, SubagentNode>,
    /// 已经作为正文段发出去的内容（收尾时据此只补剩余部分）。
    sent_segments: Vec<String>,
    sent_any: bool,
}

impl<'a, D: AgentDriver> TaskTimeline<'a, D> {
    fn new(bot: &'a QqBot<D>, reply: Arc<ReplyHandle>) -> TaskTimeline<'a, D> {
        TaskTimeline {
            bot,
            reply,
            deltas: String::new(),
            all_deltas: String::new(),
            reasoning: String::new(),
            pending_tools: Vec::new(),
            todos: Vec::new(),
            subagent_order: Vec::new(),
            subagent_nodes: BTreeMap::new(),
            sent_segments: Vec::new(),
            sent_any: false,
        }
    }

    fn handle(&mut self, event: TurnEvent) {
        match event {
            TurnEvent::Delta(delta) => {
                self.deltas.push_str(&delta);
                self.all_deltas.push_str(&delta);
            }
            TurnEvent::ReasoningDelta(delta) => {
                if self.bot.show_thinking() {
                    self.reasoning.push_str(&delta);
                }
            }
            TurnEvent::ToolStarted { call, .. } => self.on_tool_start(&call),
            TurnEvent::ToolFinished { call, result } => self.on_tool_result(&call, &result),
            TurnEvent::TodoUpdate { todos } => {
                self.todos = normalize_todos(Some(&todos));
            }
            TurnEvent::SubagentEvent { name, payload } => self.on_subagent(&name, &payload),
            TurnEvent::StreamRollback => {
                // 模型流重试：作废的半截输出丢弃，避免把残句发给用户。
                self.deltas.clear();
                self.all_deltas.clear();
            }
            _ => {}
        }
    }

    fn on_tool_start(&mut self, call: &ToolCall) {
        let operation = operation_of(&call.name);
        if operation == ASK_USER_TOOL_NAME || operation == TODO_TOOL_NAME {
            return;
        }
        // 工具调用是正文段边界：先把已经说出口的话封口。
        self.flush_text();
    }

    fn on_tool_result(&mut self, call: &ToolCall, result: &ToolResult) {
        let operation = operation_of(&call.name);
        if operation == ASK_USER_TOOL_NAME || operation == TODO_TOOL_NAME {
            return;
        }
        // 进度消息只报「调用了什么、结果如何」，工具输出本身不下发。
        let status = if result.ok { "成功" } else { "失败" };
        let summary = tool_summary(&call.name, &call.arguments, &result.output);
        self.pending_tools.push(format!(
            "● {} · {} {status}",
            redact_sensitive_text(&summary),
            tool_status_icon(status)
        ));
    }

    fn on_subagent(&mut self, name: &str, payload: &Value) {
        let status = SUBAGENT_EVENT_STATUS
            .iter()
            .find(|(event, _status)| *event == name)
            .map(|(_event, status)| (*status).to_string());
        let Some(status) = status else { return };
        let task_id = payload
            .get("task_id")
            .map(text_of)
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| "task".to_string());
        let agent_type = payload.get("agent_type").map(text_of).unwrap_or_default();
        let description = payload.get("description").map(text_of).unwrap_or_default();
        let terminal = SUBAGENT_TERMINAL_STATUSES.contains(&status.as_str());
        if !self.subagent_nodes.contains_key(&task_id) {
            self.subagent_order.push(task_id.clone());
            self.subagent_nodes.insert(
                task_id.clone(),
                SubagentNode {
                    agent_type: if agent_type.is_empty() {
                        "subagent".to_string()
                    } else {
                        agent_type.clone()
                    },
                    description: description.clone(),
                    status: status.clone(),
                    started_at: None,
                    finished_at: None,
                },
            );
        }
        let node = self
            .subagent_nodes
            .get_mut(&task_id)
            .expect("刚插入的节点必须存在");
        if SUBAGENT_TERMINAL_STATUSES.contains(&node.status.as_str()) {
            return;
        }
        if !agent_type.is_empty() {
            node.agent_type = agent_type;
        }
        if !description.is_empty() {
            node.description = description;
        }
        node.status = status;
        node.started_at = Some(Instant::now());
        if terminal {
            node.finished_at = Some(Instant::now());
        }
    }

    /// 封口当前正文段：发一条 `◇ …` 消息（中间额度用尽时留到最终回答里）。
    fn flush_text(&mut self) {
        let text = clean_text(&self.deltas);
        self.deltas.clear();
        if text.is_empty() {
            return;
        }
        if !self.reply.can_send_intermediate() {
            // 中间额度用满后不再下发任何过程内容，正文只随最终回答一起给出。
            eprintln!("[qqbot] 中间回复额度已用尽，本段正文不再单独下发。");
            return;
        }
        // 首行是块级语法（标题/列表/引用…）时 `◇` 单独占一行，否则会把块语法吃掉。
        let body = if markdown::starts_block(&text) && markdown::supports(&self.reply.target) {
            format!("◇\n{text}")
        } else {
            format!("◇ {text}")
        };
        if self.reply.send_intermediate(&body) > 0 {
            self.sent_segments.push(text);
            self.sent_any = true;
        }
    }

    /// 发一段合并后的进度：工具记录、执行计划与子任务树各一条。
    ///
    /// 中间额度用尽后这里什么都不发（攒着的内容直接丢弃），把最后一条留给最终文本。
    fn flush_progress(&mut self) {
        if !self.pending_tools.is_empty() {
            let total = self.pending_tools.len();
            let mut lines: Vec<String> = self
                .pending_tools
                .iter()
                .take(MAX_PROGRESS_TOOLS)
                .cloned()
                .collect();
            if total > MAX_PROGRESS_TOOLS {
                lines.push(format!("…另有 {} 次工具调用", total - MAX_PROGRESS_TOOLS));
            }
            self.pending_tools.clear();
            if self.reply.can_send_intermediate()
                && self.reply.send_intermediate(&lines.join("\n")) > 0
            {
                self.sent_any = true;
            }
        }
        if !self.todos.is_empty() {
            let text = todos_text(&self.todos);
            if self.reply.can_send_intermediate() && self.reply.send_intermediate(&text) > 0 {
                self.sent_any = true;
            }
        }
        if !self.subagent_order.is_empty() {
            let nodes: Vec<(String, SubagentNode)> = self
                .subagent_order
                .iter()
                .filter_map(|key| {
                    self.subagent_nodes
                        .get(key)
                        .map(|node| (key.clone(), node.clone()))
                })
                .collect();
            let text = subagents_text(&self.subagent_order, &nodes);
            if self.reply.can_send_intermediate() && self.reply.send_intermediate(&text) > 0 {
                self.sent_any = true;
            }
        }
    }

    /// 回合中段的中间输出：进度 + 已说出口的正文。
    fn flush_intermediate(&mut self) {
        self.flush_progress();
        self.flush_text();
    }

    /// 回合收尾：思考、剩余正文与最终回答。
    fn finish(&mut self, final_text: &str) {
        if !self.reasoning.trim().is_empty() {
            // QQ 是纯文本通道：用与 Telegram 同一份纯文本渲染，而不是飞书的卡片 JSON。
            let reasoning = format!("🧠 思考\n{}", reasoning_body(self.reasoning.trim(), false));
            if self.reply.can_send_intermediate() && self.reply.send_intermediate(&reasoning) > 0 {
                self.sent_any = true;
            }
        }
        let streamed = clean_text(&self.deltas);
        self.deltas.clear();
        let text = resolve_final_text(&streamed, &clean_text(final_text));
        // 消息不可编辑：已经发过的正文段不再重发，只补剩下的部分。
        let tail = remaining_after(&self.sent_segments, &text);
        if tail.trim().is_empty() {
            return;
        }
        if self.reply.send_final(&tail) > 0 {
            self.sent_any = true;
        }
    }

    fn all_deltas(&self) -> String {
        self.all_deltas.clone()
    }
}

impl<D: AgentDriver> ConfirmHandler for QqBot<D> {
    /// 把工具确认转发到 QQ，等 `/approve` 或 `/reject`，超时按拒绝。
    fn confirm(&self, tool_name: &str, arguments: &Value) -> bool {
        let task = {
            let state = self.state.lock().expect("状态锁中毒");
            state.active.clone()
        };
        let Some(task) = task else { return false };
        let pending = Arc::new(PendingConfirm {
            tool_name: tool_name.to_string(),
            sender: task.sender.clone(),
            decision: Mutex::new(None),
            signal: Condvar::new(),
        });
        {
            let mut state = self.state.lock().expect("状态锁中毒");
            state.pending = Some(Arc::clone(&pending));
        }
        let safe = redact_sensitive_values(arguments);
        let rendered = serde_json::to_string_pretty(&safe).unwrap_or_default();
        let prompt = format!(
            "⚠️ 需要确认执行敏感操作：\n工具：{tool_name}\n参数：\n{}\n回复 /approve 允许，/reject 拒绝。{} 秒内未回复将自动拒绝。",
            take_chars(&rendered, CONFIRM_ARGUMENTS_LIMIT),
            self.confirm_timeout.as_secs(),
        );
        if task.reply.send_intermediate(&prompt) == 0 {
            // 中间额度已满：提示发不出去，超时会按拒绝处理，至少留下诊断。
            eprintln!("[qqbot] 回复额度不足，确认提示未能发送（工具 {tool_name}）。");
        }
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
                task.reply
                    .send_intermediate("⏰ 确认超时，已自动拒绝该操作。");
                false
            }
        }
    }
}

impl<D: AgentDriver> AskUserHandler for QqBot<D> {
    /// 把 `ask_user` 转发到 QQ，等用户回复文本，超时按「未作答」。
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
        let question = PendingQuestion {
            kind: request
                .get("kind")
                .and_then(Value::as_str)
                .unwrap_or("question")
                .to_string(),
            question: request
                .get("question")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_string(),
            options: request
                .get("options")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default(),
            sender: task.sender.clone(),
            answer: Mutex::new(None),
            signal: Condvar::new(),
        };
        if question.question.is_empty() {
            return None;
        }
        let prompt = question.prompt();
        let pending = Arc::new(question);
        {
            let mut state = self.state.lock().expect("状态锁中毒");
            state.question = Some(Arc::clone(&pending));
        }
        if task.reply.send_intermediate(&prompt) == 0 {
            eprintln!("[qqbot] 回复额度不足，提问提示未能发送，本轮按未作答继续。");
        }
        let answer = pending.wait(self.confirm_timeout);
        {
            let mut state = self.state.lock().expect("状态锁中毒");
            let same = state
                .question
                .as_ref()
                .map(|current| Arc::ptr_eq(current, &pending))
                .unwrap_or(false);
            if same {
                state.question = None;
            }
        }
        answer
    }
}

/// 已经发过的正文段之后还剩多少：`text` 以若干已发正文段开头时返回尾部，否则原样返回。
///
/// 宿主的最终回答通常只是最后一个模型 pass 的文本，**不含**工具调用之前的正文段，因此
/// 逐段比对「已发段的尾部拼接」，而不是要求整段累计文本都出现在 `text` 里——那样一旦
/// 前几段是别的 pass 说的，前缀就匹配不上，最后一段会被整段重发一遍。
///
/// 比较时折叠空白（模型可能把两段用不同换行拼回来），但返回值取自原文，不重排内容。
fn remaining_after(sent: &[String], text: &str) -> String {
    let squeezed = |value: &str| value.split_whitespace().collect::<String>();
    let pieces: Vec<String> = sent
        .iter()
        .map(|segment| squeezed(segment))
        .filter(|segment| !segment.is_empty())
        .collect();
    if pieces.is_empty() {
        return text.to_string();
    }
    let target = squeezed(text);
    if target.is_empty() {
        return String::new();
    }
    // 从最长到最短尝试已发段的尾部拼接：整段说过就整段跳过，部分说过只补剩余部分。
    let mut consumed_chars = 0_usize;
    for start in 0..pieces.len() {
        let joined: String = pieces[start..].concat();
        if target.starts_with(&joined) {
            consumed_chars = joined.chars().count();
            break;
        }
    }
    if consumed_chars == 0 {
        return text.to_string();
    }
    if consumed_chars == target.chars().count() {
        return String::new();
    }
    // 在原文里逐字符推进，直到折叠空白后的长度达到已发部分。
    let mut consumed = 0_usize;
    let mut seen = 0_usize;
    for (index, character) in text.char_indices() {
        if !character.is_whitespace() {
            seen += 1;
            if seen == consumed_chars {
                consumed = index + character.len_utf8();
                break;
            }
        }
    }
    text[consumed..].trim_start().to_string()
}

fn take_chars(text: &str, limit: usize) -> String {
    let total = text.chars().count();
    let clipped: String = text.chars().take(limit).collect();
    if total > limit {
        format!("{clipped}…")
    } else {
        clipped
    }
}

fn text_of(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// 供启动日志用的诊断：远端会看到的机器人身份。
pub fn describe(api: &QqApi) -> String {
    match api.me() {
        Ok(payload) => {
            let name = payload
                .get("username")
                .and_then(Value::as_str)
                .unwrap_or("QQ 机器人");
            let id = payload.get("id").and_then(Value::as_str).unwrap_or("");
            format!("{name}（{id}）")
        }
        Err(error) => format!("身份查询失败：{error}"),
    }
}

#[cfg(test)]
mod tests {
    use super::{edit_distance, remaining_after, suggest_command};

    fn segments(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| (*item).to_string()).collect()
    }

    fn menu(items: &[&str]) -> Vec<(String, String)> {
        items
            .iter()
            .map(|name| ((*name).to_string(), String::new()))
            .collect()
    }

    #[test]
    fn a_misspelled_command_points_at_the_closest_one() {
        let menu = menu(&["status", "speaking-style", "skills", "reset"]);
        assert_eq!(
            suggest_command("/speaking-styl", &menu),
            Some("speaking-style".to_string())
        );
        assert_eq!(
            suggest_command("/speaking-styl@Bot on", &menu),
            Some("speaking-style".to_string())
        );
    }

    #[test]
    fn unrelated_input_gets_no_suggestion() {
        let menu = menu(&["status", "speaking-style", "reset"]);
        assert_eq!(suggest_command("/foobar", &menu), None);
        // 短命令不猜：一个字符之差就可能撞上另一条真命令。
        assert_eq!(suggest_command("/re", &menu), None);
        assert_eq!(suggest_command("/reset", &menu), None);
        // 没有斜杠、或命令表为空时也不猜。
        assert_eq!(suggest_command("speaking-styl", &menu), None);
        assert_eq!(suggest_command("/speaking-styl", &[]), None);
    }

    #[test]
    fn edit_distance_counts_character_edits() {
        let chars: Vec<char> = "speaking-styl".chars().collect();
        assert_eq!(edit_distance(&chars, "speaking-style"), 1);
        assert_eq!(edit_distance(&chars, "speaking-styl"), 0);
        assert_eq!(edit_distance(&[], "abc"), 3);
    }

    #[test]
    fn the_last_segment_is_not_repeated_as_the_final_answer() {
        // 宿主的最终回答只是最后一个 pass 的文本，已在正文段里说过。
        let sent = segments(&["先看目录。", "共一个文件。"]);
        assert_eq!(remaining_after(&sent, "共一个文件。"), "");
    }

    #[test]
    fn nothing_sent_means_the_whole_answer_goes_out() {
        assert_eq!(remaining_after(&[], "结论。"), "结论。");
    }

    #[test]
    fn an_unseen_answer_still_goes_out() {
        let sent = segments(&["先看目录。"]);
        assert_eq!(remaining_after(&sent, "结论不一样。"), "结论不一样。");
    }

    #[test]
    fn a_completion_of_the_last_segment_only_sends_the_tail() {
        let sent = segments(&["先看目录。"]);
        assert_eq!(
            remaining_after(&sent, "先看目录。结论是两行。"),
            "结论是两行。"
        );
    }
}
