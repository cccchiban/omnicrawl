//! QQ 机器人服务：网关事件接入、命令分发、消息下发与工具确认桥。
//!
//! 与另外两个连接器最大的差别是**平台形态**：同一个发送接口既能做被动回复又能发主动
//! 消息，区别只在带不带 `msg_id`。本连接器默认走被动回复，因为被动回复带上下文、
//! 不占主动消息额度：
//!
//! - 被动回复带触发消息的 `msg_id`，同一条入站消息的额度是硬限制（群聊 5 条 / 5 分钟，
//!   单聊 5 条 / 60 分钟），超出会被平台拒绝（`40034128`）；
//! - 消息**不能编辑**，也没有流式预览：正文段在工具边界处封口成一条完整消息；
//! - 工具调用不各自成条，而是攒成一段「进度」消息，避免几次调用就用光额度；
//! - 进度消息只报「调用了什么、结果如何」，**不下发工具输出**——输出属于内部过程，
//!   端上只留一行 `● 摘要 · ✓ 成功`；
//! - 额度按「每条入站消息最多 5 条」硬性计数，其中**中间过程最多 4 条**，第 5 条固定
//!   留给最终回答（[`ReplyHandle::send_intermediate`] 与 [`ReplyHandle::send_final`]）。
//!   中间过程用满 4 条之后不再发送任何内容，直到最终文本出现；
//! - 被动不可用时**最终回答降级成主动消息**（不带 `msg_id`）：本地额度用尽或平台报
//!   `40034005`（`msg_id` 过期）/ `40034128`（次数超限）都会触发，受
//!   [`ActiveBudget`] 的每分钟条数约束。过程消息不降级，免得进度把限流额度耗光。
//!
//! 除这些平台限制外，显示口径与 Telegram/飞书一致：摘要与正文的生成规则仍共用
//! `crate::render`，正文段与进度段各自独立、按发生顺序出现。含 Markdown 语法的段走
//! `msg_type=2`（代码块、表格、深标题等平台不认的写法在 [`super::markdown`] 里就地规整），
//! 回复里的 `[FILE:路径]` 标记转成 `msg_type=7` 富媒体。

use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use serde_json::Value;

use omnicrawl_session::{redact_sensitive_text, redact_sensitive_values};

use super::api::{QqApi, Target, MAX_REPLIES_PER_GROUP_MESSAGE};
use super::config::{effective_confirm_timeout, validate_config, AccessPolicy, QqBotConfig};
use super::dispatch::{
    classify, parse_thinking_command, route_event_with_bot, workspace_argument, BotIdentity,
    Command, EventRoute, Inbound, ThinkingCommand,
};
use super::gateway::{GatewayConnection, GatewaySignal};
use super::history::GroupHistory;
use super::summary::{summary_prompt, GroupSummarizer, SummaryJob};
use crate::agent::{AgentDriver, AskUserHandler, ConfirmHandler};
use crate::platform::{BotSwitches, PendingSlot};

mod attachments;
mod groups;
mod query;
mod queue;
mod reply;
mod task;
mod timeline;

pub use reply::{split_content, ActiveBudget, QqPort, ReplyHandle, MAX_ACTIVE_PER_MINUTE};
use query::{match_answer, quoted_attachments, suggest_command};
use task::{BotState, GatewaySession, PendingConfirm, PendingQuestion};

/// 单条文本消息的安全上限（协议没有下发上限，社区实现普遍按 2000 字符保守处理）。
pub const MAX_CONTENT_CHARS: usize = 1800;

/// 断线重连的退避上限（秒）。
const MAX_BACKOFF_SECONDS: f64 = 30.0;

/// 确认提示里参数 JSON 的最大长度。
const CONFIRM_ARGUMENTS_LIMIT: usize = 1200;

/// 简洁回复模式的模式标识（与主 Agent 模板 `templates/speaking-style.md` 同名）。
const SPEAKING_STYLE_MODE: &str = "speaking-style";

/// 中间过程占用的额度：留 1 条给最终回答（总额度按场景取，见 [`Target::max_replies`]）。
pub const MAX_INTERMEDIATE_REPLIES: usize = MAX_REPLIES_PER_GROUP_MESSAGE - 1;


/// 一次等待用户批准的工具调用。

/// 一个 QQ 机器人：驱动单个 OmniCrawl Agent。
/// 一个 QQ 机器人：驱动单个 OmniCrawl Agent。
pub struct QqBot<D: AgentDriver> {
    api: Arc<QqApi>,
    port: Arc<dyn QqPort>,
    driver: Arc<D>,
    access: AccessPolicy,
    confirm_timeout: Duration,
    intents: i64,
    /// 注入 system prompt 末尾的预设提示词（`[qqbot] persona`）：`READY` 取到昵称后才下发。
    persona: String,
    state: Mutex<BotState>,
    switches: BotSwitches,
    session: Mutex<GatewaySession>,
    /// 机器人自身 openid（`READY.user.id`）：兼容 `<@openid>` 占位符写法。
    bot_openid: Mutex<String>,
    /// 机器人昵称（`READY.user.username`）：全量群消息把 @ 渲染成 `@昵称` 文本。
    bot_name: Mutex<String>,
    /// 主动消息额度：被动回复不可用时所有会话共用这一份。
    active_budget: Arc<ActiveBudget>,
    /// 群聊消息记录：给「纯 @ 机器人」提供最近上下文，按群隔离。
    pub(super) history: Arc<GroupHistory>,
    /// 纯 @ 时带上的群消息条数（`[qqbot] history_context_size`）。
    pub(super) context_size: usize,
    /// 定时总结是否启用（`[qqbot] summary_interval_hours > 0`）。
    summary_enabled: bool,
    /// 群聊总结的调度器：间隔与每群上次成功时间。
    summarizer: Arc<GroupSummarizer>,
}

impl<D: AgentDriver> QqBot<D> {
    /// 构造 Bot：AppID/AppSecret 必填，超时下限 1 秒。
    pub fn new(config: &QqBotConfig, api: Arc<QqApi>, driver: Arc<D>) -> Result<QqBot<D>, String> {
        validate_config(config)?;
        let port: Arc<dyn QqPort> = api.clone();
        // 记录目录取在 `driver` 被移进结构体之前。
        let records_root = driver.temp_root();
        Ok(QqBot {
            api,
            port,
            driver,
            access: config.access(),
            confirm_timeout: Duration::from_secs_f64(effective_confirm_timeout(
                config.confirm_timeout_seconds,
            )),
            intents: config.intents,
            persona: config.persona.clone(),
            state: Mutex::new(BotState::default()),
            switches: BotSwitches::new(false),
            session: Mutex::new(GatewaySession::default()),
            bot_openid: Mutex::new(String::new()),
            bot_name: Mutex::new(String::new()),
            active_budget: Arc::new(ActiveBudget::per_minute()),
            history: Arc::new(GroupHistory::open(records_root)),
            summarizer: Arc::new(GroupSummarizer::new(Duration::from_secs_f64(
                config.summary_interval_hours * 3600.0,
            ))),
            context_size: config.history_context_size,
            summary_enabled: config.summary_interval_hours > 0.0,
        })
    }

    /// 群聊记录窗口：纯 @ 时注入最近多少条群消息。
    pub fn history(&self) -> Arc<GroupHistory> {
        Arc::clone(&self.history)
    }

    /// 替换总结器（配置化间隔与测试用）。
    pub fn with_summarizer(mut self, summarizer: Arc<GroupSummarizer>) -> QqBot<D> {
        self.summarizer = summarizer;
        self
    }

    /// 起后台总结线程：每 `summarizer.interval()` 把到期群的记录交独立回合总结并落记忆。
    ///
    /// 间隔配成 0（`summary_interval_hours = 0`）时用 `with_summarizer` 传一个关闭态进来，
    /// 这里只按 `enabled` 判断是否真起线程。
    pub fn start_summarizer(self: &Arc<Self>) {
        if !self.summary_enabled {
            eprintln!("[qqbot] 已关闭群聊消息定时总结（summary_interval_hours = 0）。");
            return;
        }
        let history = Arc::clone(&self.history);
        let summarizer = Arc::clone(&self.summarizer);
        let bot = Arc::clone(self);
        // 线程随连接器进程存在：不保留句柄，进程退出时自然结束。
        let _handle =
            super::summary::spawn_scheduler(history, summarizer, move |job| {
                bot.summarize_group(job)
            });
        eprintln!(
            "[qqbot] 群聊消息总结已启动：每 {} 小时一次（独立回合，不影响主循环）。",
            self.summarizer.interval().as_secs() / 3600
        );
    }

    /// 跑一个群的总结：独立回合 → Agent 自己 `memory_write` 落项目级记忆。
    fn summarize_group(&self, job: &SummaryJob) -> Result<(), String> {
        if self.is_stopped() {
            return Err("服务已停止。".to_string());
        }
        let prompt = summary_prompt(&job.group, &job.records);
        self.driver
            .run_isolated_turn(&prompt)
            .map(|_reply| ())
            .map_err(|error| redact_sensitive_text(&error))
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
        // 独立回合的定时总结：与主循环并行，不占对话名额。
        self.start_summarizer();
        self.run_forever()
    }

    /// 把机器人身份注入 system prompt 末尾：模型因此知道自己在 QQ 上、叫什么名字。
    ///
    /// 昵称要等网关 `READY` 才拿到，所以这件事不能在构造时做。宿主不支持提示词装配
    /// （内核自持的 `omnicrawl --connector qqbot` 等）时只记日志，不影响收发消息。
    pub fn apply_persona(&self, bot_name: &str) {
        let append = super::config::prompt_append(&self.persona, bot_name);
        match self.driver.set_system_prompt_append(&append) {
            Ok(()) => eprintln!("[qqbot] 已注入预设提示词：{append}"),
            Err(error) => eprintln!("[qqbot] 注入预设提示词失败：{error}"),
        }
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
                        self.handle_ready(&data);
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

    /// 处理网关 `READY`：记住会话、机器人身份，并把平台身份注入 system prompt。
    pub fn handle_ready(self: &Arc<Self>, data: &Value) {
        let session_id = data
            .get("session_id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        // 全量模式靠机器人自身身份判断哪条群消息 @ 了它：平台把 @ 渲染成 `@昵称` 文本，
        // 所以昵称是关键；openid 用于兼容占位符写法。优先用 READY，缺字段时退回 /users/@me。
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
        if let Ok(mut slot) = self.bot_openid.lock() {
            *slot = id.unwrap_or_default();
        }
        if let Ok(mut slot) = self.bot_name.lock() {
            *slot = name.unwrap_or_default();
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
            eprintln!("[qqbot] 未取到机器人昵称：全量群消息可能无法识别 @（openid={id_text}）");
        } else {
            eprintln!("[qqbot] 机器人身份：{name_text}（openid={id_text}），用于识别群内 @");
        }
        self.apply_persona(&name_text);
        if let Ok(mut session) = self.session.lock() {
            session.session_id = session_id;
        }
        eprintln!("[qqbot] 登录成功，开始接收事件。");
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
            EventRoute::NotAMessage {
                event,
                conversation,
            } => {
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
                // 群里没 @ 机器人的消息同样要记：它们是「纯 @ 时带上的前十条」的来源。
                self.record_group_message(data);
                eprintln!("[qqbot] 群消息未 @ 机器人，忽略：{conversation}（{event}）；{detail}");
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
    ///
    /// 用户引用了某条消息时，被引用的正文/附件先落成本地附件再一起交给 Agent，
    /// 这样「你引用的这张图」才有东西可看。
    fn handle_message(self: &Arc<Self>, inbound: &Inbound) {
        let reply = Arc::new(self.reply_for(inbound));
        // 群聊里 @ 机器人的消息也是一条群消息：先入记录，再当指令处理。
        self.record_inbound(inbound);
        if self.resolve_pending_question(inbound) {
            return;
        }
        if inbound.content.is_empty() && inbound.attachments.is_empty() && inbound.quoted.is_empty()
        {
            // 群里的纯 @（只 @ 机器人、没有正文）不是空消息：它靠该群最近的消息提供
            // 上下文，历史本身就是这次提问的全部内容。历史也空才算真的没事可做。
            if !matches!(inbound.target, Target::Group(_)) || !self.has_group_context(inbound) {
                return;
            }
        }
        if !inbound.attachments.is_empty() || !quoted_attachments(inbound).is_empty() {
            self.spawn_file_task(inbound.clone(), reply);
            return;
        }
        self.dispatch(inbound, reply);
    }

    /// 没有 @ 机器人的群消息：从原始事件里取群与作者，入本群记录。
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
        .with_reference(&inbound.ref_idx)
        .with_active_budget(Arc::clone(&self.active_budget))
        .with_workspace_root(PathBuf::from(self.driver.status().workspace_root))
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
        // 队列里还有任务时要说清楚：取消只停当前这条，剩下的会接着跑。
        let waiting = {
            let state = self.state.lock().expect("状态锁中毒");
            state.queue.len()
        };
        if waiting > 0 {
            reply.send_notice(&format!("队列里还有 {waiting} 条会继续执行。"));
        }
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
                self.switches.set_show_thinking(true);
                let _ = reply.send_final("已开启思考内容显示（🧠 独立消息）。");
            }
            ThinkingCommand::Disable => {
                self.switches.set_show_thinking(false);
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


    pub fn show_thinking(&self) -> bool {
        self.switches.show_thinking()
    }

    pub fn is_stopped(&self) -> bool {
        self.switches.is_stopped()
    }

    /// 停止网关循环；挂起的确认与提问一并释放，避免任务线程一直等到超时。
    pub fn request_stop(&self) {
        self.switches.mark_stopped();
        let (pending, question) = {
            let state = self.state.lock().expect("状态锁中毒");
            (state.pending.clone(), state.question.clone())
        };
        if let Some(pending) = pending {
            pending.release();
        }
        if let Some(question) = question {
            question.release();
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
        self.switches.sleep_until_stopped(seconds)
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
            if !state.queue.is_empty() {
                lines.push(format!("排队等待：{} 条", state.queue.len()));
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

/// 单个回合的显示装配：正文段在工具边界封口，工具/计划/子任务攒成一段进度。


impl<D: AgentDriver> ConfirmHandler for QqBot<D> {
    /// 把工具确认转发到 QQ，等 `/approve` 或 `/reject`，超时按拒绝。
    fn confirm(&self, tool_name: &str, arguments: &Value) -> bool {
        let task = {
            let state = self.state.lock().expect("状态锁中毒");
            state.active.clone()
        };
        let Some(task) = task else { return false };
        let pending = Arc::new(PendingConfirm::new(tool_name, &task.sender));
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
            slot: PendingSlot::new(),
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


fn take_chars(text: &str, limit: usize) -> String {
    crate::text::take_chars_with_ellipsis(text, limit)
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
    use super::query::{edit_distance, suggest_command};
    use super::timeline::remaining_after;

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
