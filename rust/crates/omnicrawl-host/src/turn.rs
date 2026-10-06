//! 无头回合运行器：把一个回合从提交跑到 `turn.finished`，不依赖任何界面。
//!
//! 界面无关的三件事在这里定死：握手时声明哪些工具、`tool.batch` 怎么定调
//! （自持工具就地执行、敏感工具问审批、提问工具等作答）、整批观察按模型顺序回填。
//! 宿主只从外部拿两个输入：审批与提问的决断（[`Interactor`]）和事件出口（事件回调）。
//!
//! 工具生命周期与执行清单（`tool.started` / `tool.finished` / `todo.update`）按协议由宿主发出，
//! 因此也在这里产生——本地 API 直接把它们翻成 SSE 事件。

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use omnicrawl_core::diagnostics;
use omnicrawl_core::{AgentLoopObservation, ToolCall, ToolResult};
use omnicrawl_ipc::bridge::{
    Command, ContextPruneGroup, ContextPruneRequest, ContextPruneResult, HostEvent,
    InitializeParams, KernelModelConfig, KernelSessionConfig, MessagePayload, ModelHookRequest,
    ModelHookResult, SessionSettingsParams, SubagentQueryParams, TodoUpdatePayload, ToolBatch,
    ToolEventPayload, ToolStartedPayload,
};
use omnicrawl_ipc::{
    error_code, Frame, Id, ToolBatchResult, TurnCancelParams, TurnSubmitParams, PROTOCOL_VERSION,
};
use serde_json::{json, Map, Value};

use crate::approval::ApprovalMode;
use crate::host::{self, BatchContext, BatchStep, PendingBatch, TodoItem, VisionPayload, Waiting};
use crate::kernel::KernelClient;
use crate::plugins::PluginHost;
use crate::prompt::PromptRuntime;
use crate::prompt_cache::build_prompt_cache_identity;
use crate::review::{needs_review, review_tool_call, ReviewContext, ReviewOptions, ReviewRequest};
use crate::tools::decision_choice::{chosen_option, CustodyContext, CustodyOptions};
use crate::tools::decision_prune::{evicted_call_ids_for, PruneOptions};
use crate::tools::{RegistryOptions, ToolRegistry};

/// 握手响应最多等这么久；内核启动即刻回帧，卡住说明进程有问题。
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);

/// 帧与批次截止时间的轮询粒度：既要及时收口超时与取消，也不要空转。
const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// 一次运行的固定输入。
#[derive(Clone)]
pub struct RunnerOptions {
    pub workspace_root: PathBuf,
    /// 模型端点；`tools` 字段由运行器用工具表覆盖，调用方不必填。
    pub model: KernelModelConfig,
    /// 会话块：`None` 表示不落会话（内核按无会话行为跑）。
    pub session: Option<KernelSessionConfig>,
    pub approval: ApprovalMode,
    pub command_timeout_seconds: i64,
    pub tool_timeout_seconds: i64,
    /// 是否把图片交给内核：原生视觉或 `[vision]` 代理任一可用即为真（与 Python
    /// `route_image_result` 同义）。为假时图片不会进入请求——主模型看不懂图，
    /// 留在请求里只会被 Provider 拒掉。
    pub attach_vision_images: bool,
    /// 握手时报出的宿主名，供内核诊断。
    pub client_name: String,
    /// 插件运行期；`None` 表示无插件模式，所有 Hook 节点都退化为原样放行。
    pub plugins: Option<Arc<PluginHost>>,
    /// 审查模型（`approval.mode = review` 时用）；`None` 表示没有审查运行期，
    /// 此模式下需审查的调用会 fail-closed 拒绝。
    pub review: Option<ReviewOptions>,
    /// 提问托管（`decision_models.toml` 的 `ask_user_custody` 开关）：开启后**有选项**的提问
    /// 交给决策模型自动作答；`None` 或不可用时一律退回人工提问（fail-open）。
    pub custody: Option<CustodyOptions>,
    /// 工具调用淘汰（`decision_models.toml` 的 `tool_call_prune` 开关）：开启后内核会把「刚变老
    /// 的那一批」调用交回来裁决，判为无用的整组移出上下文；`None` 或不可用时一律保留原文
    /// （fail-open）。
    pub prune: Option<PruneOptions>,
    /// 提示词装配结果（模板 + AGENTS.md + Skill 索引 + 运行环境）。
    ///
    /// 有它时握手用装配出来的 system prompt 与 `context_messages`，并按**真实**的项目规范
    /// 与 Skill 索引算稳定前缀身份；`None` 时沿用 `model` 里的文本（嵌入与测试）。
    pub prompt: Option<Arc<PromptRuntime>>,
}

/// 需要用户决定的交互：本地 API 接审批/提问的 HTTP，TUI 接键盘。
pub trait Interactor: Send {
    /// 是否批准这次工具调用；`None` 按拒绝处理（超时、未决定、触达不到用户都走这里）。
    fn decide(&mut self, tool: &str, arguments: &Map<String, Value>) -> Option<bool>;

    /// 回答一次提问；`None` 或空串按「未作答」处理。
    fn answer(
        &mut self,
        prompt: &str,
        options: &[String],
        arguments: &Map<String, Value>,
    ) -> Option<String>;
}

/// 回合失败的原因。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnError {
    /// 外部要求取消（协议里取消是建议性的，内核可能已在收尾）。
    Cancelled,
    /// 内核进程结束或连接断开。
    KernelExited,
    /// 内核返回了错误响应。
    Rejected(String),
    /// 插件 Hook 拒绝本轮：守卫类 Hook 的显式 deny，或 fail-closed 策略下的分发故障。
    /// 文案已由插件控制器生成（区分「插件拒绝」与「插件超时/通信错误」），原样展示。
    PluginDenied(String),
}

impl std::fmt::Display for TurnError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cancelled => write!(formatter, "已取消当前回合。"),
            Self::KernelExited => write!(formatter, "内核进程已退出。"),
            Self::Rejected(detail) => write!(formatter, "内核拒绝该操作：{detail}"),
            Self::PluginDenied(detail) => write!(formatter, "{detail}"),
        }
    }
}

impl std::error::Error for TurnError {}

/// 一轮的收尾信息（对应 `turn.finished`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnOutcome {
    pub turn_id: String,
    pub final_text: String,
    pub reasoning: String,
    pub model_turns: usize,
    pub tool_calls: usize,
    pub paused: bool,
}

/// 跨线程的取消开关：回合跑在一条线程里，取消请求从别处进来。
#[derive(Clone, Default)]
pub struct TurnControl {
    cancelled: Arc<AtomicBool>,
}

impl TurnControl {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn request_cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }
}

/// 一个调用执行完的通知。
struct JobDone {
    index: usize,
    /// 工具名：提问托管要从顾问答复里取上下文。
    tool: String,
    result: ToolResult,
    vision: Option<VisionPayload>,
}

/// 回合一跑一建：持有一个内核连接与一张工具表。
pub struct TurnRunner {
    kernel: KernelClient,
    registry: Arc<ToolRegistry>,
    options: RunnerOptions,
    next_turn: u64,
    /// 审查载荷要的两条会话事实（最近用户消息 + 最近一次 ask_user 问答）。
    review_context: ReviewContext,
    /// 提问托管的上下文：用户本回合的请求 + 本回合已有的顾问答复（每次提交回合时刷新）。
    custody_context: CustodyContext,
}

impl TurnRunner {
    /// 按运行选项建工具表；工作区根就是工具的执行根。
    pub fn new(
        kernel: KernelClient,
        options: RunnerOptions,
        registry_options: &RegistryOptions,
    ) -> Result<Self, String> {
        let registry = ToolRegistry::new(
            options.workspace_root.clone(),
            registry_options,
            options.command_timeout_seconds,
        )
        .map_err(|error| format!("工具表构建失败：{error}"))?;
        Ok(Self {
            kernel,
            registry: Arc::new(registry),
            options,
            next_turn: 1,
            review_context: ReviewContext::default(),
            custody_context: CustodyContext::default(),
        })
    }

    pub fn registry(&self) -> &ToolRegistry {
        &self.registry
    }

    /// 运行期重建工具表（MCP / 顾问 / 工具开关等宿主侧能力变化后用）。
    ///
    /// 与首次构建走同一入口：调用方传进来的选项必须已经带上新的运行期句柄；后台任务
    /// 管理器与取消令牌一律沿用旧表，重建不会丢掉正在跑的后台命令，也不改变本回合的
    /// 取消语义。重建之后调用方还要把新声明下发给内核
    /// （`session.settings.model.tools`），否则内核仍按旧工具表向模型声明能力。
    pub fn rebuild_registry(&mut self, registry_options: &RegistryOptions) -> Result<(), String> {
        let mut options = registry_options.clone();
        options.monitors = Some(self.registry.monitors().clone());
        options.cancel = Some(self.registry.cancel_token());
        let registry = ToolRegistry::new(
            self.options.workspace_root.clone(),
            &options,
            self.options.command_timeout_seconds,
        )
        .map_err(|error| format!("工具表构建失败：{}", error.message))?;
        self.registry = Arc::new(registry);
        Ok(())
    }

    pub fn kernel_closed(&self) -> bool {
        self.kernel.is_closed()
    }

    /// 握手：`initialize` 带上模型配置（内核自己发请求）、工具声明与可选的会话块。
    ///
    /// 有 `prompt` 装配结果时，system prompt 与 `context_messages` 以它为准（对映 TUI
    /// 的同一套装配），`options.model` 里的文本只作为无装配运行期（嵌入/测试）的输入。
    pub fn handshake(&mut self, on_event: &mut dyn FnMut(HostEvent)) -> Result<(), String> {
        let mut model = self.options.model.clone();
        model.tools = self.registry.declarations();
        if let Some(prompt) = self.options.prompt.clone() {
            let tool_count = model.tools.len();
            model.system_prompt = prompt.system_prompt();
            model.context_messages = prompt
                .context_messages_with_plugins(
                    tool_count > 0,
                    self.options.plugins.as_deref(),
                    None,
                    None,
                )
                .map_err(|error| format!("上下文装配失败：{error}"))?;
        }
        // 稳定前缀身份必须在工具表就位后组装：`tool_schema_hash` 参与哈希，而
        // `options.model` 里从 `options_from_process` 拿到的工具表还是空的。
        model.prompt_cache_identity = self.prompt_cache_identity(&model);
        let params = InitializeParams {
            protocol_version: PROTOCOL_VERSION.to_string(),
            client: json!({
                "name": self.options.client_name,
                "version": env!("CARGO_PKG_VERSION"),
            }),
            model: Some(Box::new(model)),
            session: self.options.session.clone().map(Box::new),
            // 有插件运行期就声明能力：内核才会在模型请求前发 `model.hook`。
            plugin_model_hooks: self.options.plugins.is_some(),
            // 淘汰运行期只有在装配出配置时才声明：未声明时内核永不发 `context.prune`，
            // 也就不会为一次无效裁决阻住回合。
            tool_call_prune: self
                .options
                .prune
                .as_ref()
                .map(|prune| prune.enabled)
                .unwrap_or(false),
        };
        let id = self.kernel.next_id();
        self.kernel
            .send_frame(&Command::Initialize(params).to_frame(id.clone()))
            .map_err(|error| format!("发送握手帧失败：{error}"))?;

        let deadline = Instant::now() + HANDSHAKE_TIMEOUT;
        while Instant::now() < deadline {
            let wait = deadline
                .saturating_duration_since(Instant::now())
                .min(POLL_INTERVAL);
            let Some(frame) = self.kernel.recv_until_closed(wait) else {
                if self.kernel.is_closed() {
                    return Err("内核进程在握手期退出。".to_string());
                }
                continue;
            };
            if frame.is_response() && frame.id() == Some(&id) {
                return match frame.error.as_ref() {
                    Some(error) => Err(format!("内核拒绝握手：{}", error.message)),
                    None => Ok(()),
                };
            }
            self.dispatch(frame, on_event);
        }
        Err("等待内核握手响应超时。".to_string())
    }

    /// 运行期切换审批模式：下一次工具批次立即生效（审批由宿主定调）。
    pub fn set_approval_mode(&mut self, mode: ApprovalMode) {
        self.options.approval = mode;
    }

    /// 组装稳定 prompt 前缀的身份指纹（`initialize.model.prompt_cache_identity`）。
    ///
    /// 有 `prompt` 装配结果时用**真实的**项目规范与 Skill 索引参与哈希——它们真的会进
    /// `context_messages`，身份必须跟着它们走；无装配运行期（嵌入/测试）才按空集。
    /// 哈希算法复用 [`build_prompt_cache_identity`]，与 TUI 及 Python 逐字节对齐。
    fn prompt_cache_identity(
        &self,
        model: &KernelModelConfig,
    ) -> std::collections::BTreeMap<String, String> {
        let (project_instructions, skills) = match self.options.prompt.as_ref() {
            Some(prompt) => (
                prompt.project_instructions().unwrap_or_default(),
                prompt.skill_metas(),
            ),
            None => (String::new(), Vec::new()),
        };
        build_prompt_cache_identity(
            &model.system_prompt,
            &self.options.workspace_root,
            &project_instructions,
            &skills,
            &[],
            &model.tools,
        )
        .to_identity_map()
    }

    /// 下发一次 `session.settings`（模型 / 生成选项 / 推理强度 / 压缩等），返回内核回执。
    ///
    /// 与 `handshake` 同一套请求-回应配对：发送后轮询响应帧，期间到达的宿主事件照常交给
    /// 回调。内核主循环在跑回合时被占用，因此这条通道只在没有回合在途时可用（调用方负责
    /// 判断，见 `omnicrawl-api` 的 `apply_session_settings`）。
    pub fn apply_session_settings(
        &mut self,
        params: SessionSettingsParams,
        on_event: &mut dyn FnMut(HostEvent),
    ) -> Result<Value, String> {
        let id = self.kernel.next_id();
        self.kernel
            .send_frame(&Command::SessionSettings(Box::new(params)).to_frame(id.clone()))
            .map_err(|error| format!("发送设置帧失败：{error}"))?;

        let deadline = Instant::now() + HANDSHAKE_TIMEOUT;
        while Instant::now() < deadline {
            let wait = deadline
                .saturating_duration_since(Instant::now())
                .min(POLL_INTERVAL);
            let Some(frame) = self.kernel.recv_until_closed(wait) else {
                if self.kernel.is_closed() {
                    return Err("内核进程在设置下发期退出。".to_string());
                }
                continue;
            };
            if frame.is_response() && frame.id() == Some(&id) {
                return match frame.error.as_ref() {
                    Some(error) => Err(format!("内核拒绝设置：{}", error.message)),
                    None => Ok(frame.result.clone().unwrap_or(Value::Null)),
                };
            }
            self.dispatch(frame, on_event);
        }
        Err("等待内核设置响应超时。".to_string())
    }

    /// 显式压缩当前会话；内核回 `{summary, compacted}`。
    pub fn compact_session(&mut self) -> Result<Value, String> {
        let id = self.kernel.next_id();
        self.kernel
            .send_frame(&Command::SessionCompact.to_frame(id.clone()))
            .map_err(|error| format!("发送压缩帧失败：{error}"))?;

        let deadline = Instant::now() + HANDSHAKE_TIMEOUT;
        while Instant::now() < deadline {
            let wait = deadline
                .saturating_duration_since(Instant::now())
                .min(POLL_INTERVAL);
            let Some(frame) = self.kernel.recv_until_closed(wait) else {
                if self.kernel.is_closed() {
                    return Err("内核进程在压缩期退出。".to_string());
                }
                continue;
            };
            if frame.is_response() && frame.id() == Some(&id) {
                return match frame.error.as_ref() {
                    Some(error) => Err(format!("内核拒绝压缩：{}", error.message)),
                    None => Ok(frame.result.clone().unwrap_or(Value::Null)),
                };
            }
            self.dispatch(frame, &mut |_| {});
        }
        Err("等待内核压缩响应超时。".to_string())
    }

    /// 查询／取消内核持有的后台 SubAgent 任务；返回 `{unavailable, tasks, task, result}`。
    pub fn manage_subagents(&mut self, action: &str, task_id: &str) -> Result<Value, String> {
        let params = SubagentQueryParams {
            action: action.to_string(),
            task_id: task_id.to_string(),
        };
        let id = self.kernel.next_id();
        self.kernel
            .send_frame(&Command::SubagentQuery(params).to_frame(id.clone()))
            .map_err(|error| format!("发送后台任务查询帧失败：{error}"))?;

        let deadline = Instant::now() + HANDSHAKE_TIMEOUT;
        while Instant::now() < deadline {
            let wait = deadline
                .saturating_duration_since(Instant::now())
                .min(POLL_INTERVAL);
            let Some(frame) = self.kernel.recv_until_closed(wait) else {
                if self.kernel.is_closed() {
                    return Err("内核进程在后台任务查询期退出。".to_string());
                }
                continue;
            };
            if frame.is_response() && frame.id() == Some(&id) {
                return match frame.error.as_ref() {
                    Some(error) => Err(format!("内核拒绝后台任务查询：{}", error.message)),
                    None => Ok(frame.result.clone().unwrap_or(Value::Null)),
                };
            }
            self.dispatch(frame, &mut |_| {});
        }
        Err("等待内核后台任务查询响应超时。".to_string())
    }

    /// 取出已排队的宿主通知（不阻塞）。
    ///
    /// 回合之外内核仍会推后台任务事件；宿主没有常驻泵时，这些帧会积在客户端通道里。
    /// 调用方按需排空并把它们投影到会话级事件流（见 `omnicrawl-api` 的事件泵）。
    pub fn drain_notifications(&mut self) -> Vec<HostEvent> {
        let mut collected: Vec<HostEvent> = Vec::new();
        while let Some(frame) = self.kernel.try_recv() {
            self.dispatch(frame, &mut |event| collected.push(event));
        }
        collected
    }

    /// 提交一个回合并跑到 `turn.finished`。
    ///
    /// `control` 在回合内被轮询：置位后先发 `turn.cancel` 并回收本回合资源，再按取消收尾。
    pub fn submit(
        &mut self,
        text: &str,
        control: &TurnControl,
        interactor: &mut dyn Interactor,
        on_event: &mut dyn FnMut(HostEvent),
    ) -> Result<TurnOutcome, TurnError> {
        let turn_id = format!("turn-{}", self.next_turn);
        self.next_turn += 1;
        let session_id = self.session_id();
        // `turn.start` 在提交给内核之前分发：transform 类 Handler 可以改写 `userText`，
        // 守卫类 Handler 拒绝时本轮根本不进内核（与 Python 的 `turn_payload` 同上位）。
        let user_text = match self.options.plugins.as_ref() {
            Some(plugins) => plugins
                .turn_start(text, session_id.as_deref(), Some(&turn_id))
                .map_err(|error| TurnError::PluginDenied(error.to_string()))?,
            None => text.to_string(),
        };
        // 审查闸的意图摘要取本轮提交的原文（与 Python 从消息快照取的最近一条用户消息同义）。
        self.review_context.record_user_text(&user_text);
        // 提问托管的上下文按回合刷新：用户请求取本轮原文，顾问答复只算本回合的。
        self.custody_context.question.clear();
        self.custody_context.user_prompt = user_text.clone();
        self.custody_context.advisor_replies.clear();
        let result = self.run_turn(&turn_id, &user_text, control, interactor, on_event);
        if let Some(plugins) = self.options.plugins.as_ref() {
            match &result {
                Ok(_) => plugins.turn_end(session_id.as_deref(), Some(&turn_id)),
                Err(TurnError::Cancelled) => {
                    plugins.turn_cancelled(session_id.as_deref(), Some(&turn_id))
                }
                Err(error) => {
                    plugins.turn_error(&error.to_string(), session_id.as_deref(), Some(&turn_id))
                }
            }
        }
        result
    }

    /// 会话标识：`session` 块里的会话 id；空串表示由内核新建，此时不作为 Hook 的会话键。
    fn session_id(&self) -> Option<String> {
        self.options
            .session
            .as_ref()
            .filter(|session| !session.session_id.trim().is_empty())
            .map(|session| session.session_id.clone())
    }

    /// 一个回合的主体：提交 `turn.submit` 并跑到 `turn.finished`。
    fn run_turn(
        &mut self,
        turn_id: &str,
        text: &str,
        control: &TurnControl,
        interactor: &mut dyn Interactor,
        on_event: &mut dyn FnMut(HostEvent),
    ) -> Result<TurnOutcome, TurnError> {
        self.registry.set_monitor_scope(Some(turn_id));
        // 新回合开始：清掉上一回合取消（`TurnControl::request_cancel`）留下的取消标记。
        // 运行器与工具表跨回合复用（API 服务持有同一份 `TurnRunner`），不复位会让取消后的
        // 下一个回合里所有 `bash` / `powershell` 一启动就被判定成已取消。
        self.registry.cancel_token().reset();
        let id = self.kernel.next_id();
        let frame = Command::TurnSubmit(TurnSubmitParams {
            turn_id: turn_id.to_string(),
            user_text: text.to_string(),
            images: Vec::new(),
        })
        .to_frame(id.clone());
        self.kernel
            .send_frame(&frame)
            .map_err(|_| TurnError::KernelExited)?;

        let outcome = loop {
            if control.is_cancelled() {
                self.cancel_turn(turn_id);
                return Err(TurnError::Cancelled);
            }
            let Some(frame) = self.kernel.recv_until_closed(POLL_INTERVAL) else {
                if self.kernel.is_closed() {
                    return Err(TurnError::KernelExited);
                }
                continue;
            };
            if frame.is_notification() {
                match HostEvent::from_frame(&frame) {
                    Ok(HostEvent::TurnFinished(payload)) => {
                        let outcome = TurnOutcome {
                            turn_id: payload.turn_id.clone(),
                            final_text: payload.final_text.clone(),
                            reasoning: payload.reasoning.clone(),
                            model_turns: payload.model_turns,
                            tool_calls: payload.tool_calls,
                            paused: payload.paused,
                        };
                        on_event(HostEvent::TurnFinished(payload));
                        break outcome;
                    }
                    Ok(event) => on_event(event),
                    Err(error) => diagnostics::warn(format!("[host] 未识别的内核通知：{error}")),
                }
                continue;
            }
            let Some(frame_id) = frame.id().cloned() else {
                diagnostics::warn("[host] 内核发来没有 id 的帧，已忽略。");
                continue;
            };
            if frame.is_response() && frame.id() == Some(&id) {
                if let Some(error) = frame.error.as_ref() {
                    return Err(TurnError::Rejected(error.message.clone()));
                }
                continue;
            }
            self.serve_request(frame_id, frame, interactor, on_event);
        };
        self.registry.set_monitor_scope(None);
        Ok(outcome)
    }

    /// 请内核退出并回收后台资源。
    pub fn shutdown(&mut self) {
        self.registry.close_monitors();
        self.registry.close_mcp();
        if self.kernel.is_closed() {
            return;
        }
        let id = self.kernel.next_id();
        let _ = self.kernel.send_frame(&Command::Shutdown.to_frame(id));
    }

    /// 请内核退出后等它自己收尾（宿主退出收尾用）。
    ///
    /// `shutdown` 只负责把 `shutdown` 帧发出去；内核需要在这之后才把
    /// `session_closed` 落盘，因此要发 `session.close.after` 的宿主必须等它退出。
    /// 超时后强杀，避免留下孤儿进程。
    pub fn wait_for_exit(&mut self, timeout: std::time::Duration) {
        self.kernel.wait_or_kill(timeout);
    }

    /// 取消：请内核停止，并回收本回合的进程树与后台任务。
    fn cancel_turn(&mut self, turn_id: &str) {
        let id = self.kernel.next_id();
        let _ = self.kernel.send_frame(
            &Command::TurnCancel(TurnCancelParams {
                turn_id: turn_id.to_string(),
            })
            .to_frame(id),
        );
        self.registry.cancel_token().cancel();
        self.registry
            .stop_monitors_in_scope(turn_id, crate::tools::monitor::CANCEL_REASON);
        self.registry.set_monitor_scope(None);
    }

    /// 收其他帧：通知转给事件出口，请求就地服务（握手期用无人值守交互）。
    fn dispatch(&mut self, frame: Frame, on_event: &mut dyn FnMut(HostEvent)) {
        if frame.is_notification() {
            match HostEvent::from_frame(&frame) {
                Ok(event) => on_event(event),
                Err(error) => diagnostics::warn(format!("[host] 未识别的内核通知：{error}")),
            }
            return;
        }
        let Some(id) = frame.id().cloned() else {
            return;
        };
        if frame.is_response() {
            return;
        }
        let mut interactor = Unattended;
        self.serve_request(id, frame, &mut interactor, on_event);
    }

    /// 服务一个内核请求。
    fn serve_request(
        &mut self,
        id: Id,
        frame: Frame,
        interactor: &mut dyn Interactor,
        on_event: &mut dyn FnMut(HostEvent),
    ) {
        match frame.method() {
            Some(method) if method == omnicrawl_ipc::bridge::method::TOOL_BATCH => {
                match ToolBatch::from_frame(&frame) {
                    Ok(batch) => {
                        let observations = self.run_batch(batch, interactor, on_event);
                        let result = ToolBatchResult { observations }.to_result();
                        if let Err(error) = self.kernel.respond(&id, result) {
                            diagnostics::warn(format!("[host] 回工具批次失败：{error}"));
                        }
                    }
                    Err(error) => {
                        let _ = self.kernel.respond_error(
                            &id,
                            error_code::INVALID_PARAMS,
                            &format!("tool.batch 负载不符：{error}"),
                        );
                    }
                }
            }
            Some(method) if method == omnicrawl_ipc::bridge::method::MODEL_HOOK => {
                match ModelHookRequest::from_frame(&frame) {
                    Ok(request) => match self.run_model_hook(request) {
                        Ok(result) => {
                            if let Err(error) = self.kernel.respond(&id, result.to_result()) {
                                diagnostics::warn(format!("[host] 回 model.hook 失败：{error}"));
                            }
                        }
                        Err(message) => {
                            let _ = self.kernel.respond_error(
                                &id,
                                error_code::INVALID_REQUEST,
                                &message,
                            );
                        }
                    },
                    Err(error) => {
                        let _ = self.kernel.respond_error(
                            &id,
                            error_code::INVALID_PARAMS,
                            &format!("model.hook 负载不符：{error}"),
                        );
                    }
                }
            }
            Some(method) if method == omnicrawl_ipc::bridge::method::CONTEXT_PRUNE => {
                match ContextPruneRequest::from_frame(&frame) {
                    Ok(request) => {
                        let result = self.run_context_prune(request);
                        if let Err(error) = self.kernel.respond(&id, result.to_result()) {
                            diagnostics::warn(format!("[host] 回 context.prune 失败：{error}"));
                        }
                    }
                    Err(error) => {
                        let _ = self.kernel.respond_error(
                            &id,
                            error_code::INVALID_PARAMS,
                            &format!("context.prune 负载不符：{error}"),
                        );
                    }
                }
            }
            // 内核自带 provider runtime，`model.reply` 代答路径不再需要。
            Some(method) => {
                let _ = self.kernel.respond_unsupported(&id, method);
            }
            None => {}
        }
    }

    /// `model.request.before`：插件可改写消息；拒绝时以错误响应回填拒绝文案。
    ///
    /// 无插件运行期时原样放行（不会走到这里：未声明能力时内核不发该请求，
    /// 这里只是把两条路径都收在一个地方，避免以后能力判定漂移）。
    fn run_model_hook(&mut self, request: ModelHookRequest) -> Result<ModelHookResult, String> {
        let ModelHookRequest {
            mut messages,
            model,
        } = request;
        let Some(plugins) = self.options.plugins.clone() else {
            return Ok(ModelHookResult { messages });
        };
        let session_id = self
            .options
            .session
            .as_ref()
            .map(|session| session.session_id.clone())
            .filter(|id| !id.is_empty());
        plugins
            .model_request_before(&mut messages, &model, session_id.as_deref())
            .map_err(|error| error.message().to_string())?;
        Ok(ModelHookResult { messages })
    }

    /// `context.prune`：把内核送来的「刚变老」那批调用交决策模型裁决，回可以移除的调用 ID。
    ///
    /// 可用性判据在 [`evicted_call_ids_for`]：开关关闭、没有可用渠道、缺凭据、请求失败、
    /// 响应不可解析都返回 `None`，这里回**空列表**（等于「都留着」）——淘汰是 fail-open 的
    /// 省上下文手段，不值得让回合失败，也绝不能在裁决不可用时误删内容。
    fn run_context_prune(&mut self, request: ContextPruneRequest) -> ContextPruneResult {
        let groups = request
            .groups
            .iter()
            .map(|group| {
                omnicrawl_controllers::turn::tool_prune::PruneCandidate::bounded(
                    group.call_id.clone(),
                    group.tool.clone(),
                    group.arguments.clone(),
                    group.ok,
                    group.output.clone(),
                )
            })
            .collect::<Vec<_>>();
        // 判定背景取**用户本回合提交的原文**：内核随请求带来（每回合刷新），比宿主自己维护的
        // 意图摘要更全，也与 TUI 宿主读同一份事实（`handle_context_prune`）。
        let evicted = self
            .options
            .prune
            .as_ref()
            .and_then(|prune| evicted_call_ids_for(prune, &request.task, &groups))
            .unwrap_or_default();
        ContextPruneResult {
            evicted_call_ids: evicted,
        }
    }

    /// 跑完一整批工具：先把整批定调（自持工具就地办、敏感工具逐个问），再并发执行，最后回观察。
    fn run_batch(
        &mut self,
        batch: ToolBatch,
        interactor: &mut dyn Interactor,
        on_event: &mut dyn FnMut(HostEvent),
    ) -> Vec<AgentLoopObservation> {
        let mut pending = PendingBatch::new(Id::Number(0), batch.calls.clone());
        let mut todos: Vec<TodoItem> = Vec::new();
        let mut paused = false;
        let (sender, completions) = mpsc::channel::<JobDone>();
        // 每个调用只报一次「已开始」与一次「已完成」：定调就地办的、被拒的、已作答的
        // 与真跑过的调用都要有卡片，且不能重复。
        let mut started = vec![false; batch.calls.len()];
        let mut finished = vec![false; batch.calls.len()];
        // 每个调用只过一次插件守卫：人工审批路径与直接执行路径共享这份标记。
        let mut guarded = vec![false; batch.calls.len()];
        // 审批结论只通知一次插件（人工决定已发过的，不再在执行前重复发）。
        let mut decided = vec![false; batch.calls.len()];
        // 审批判定要读工具说明与参数 schema：先把工具表借出来（用局部 Arc 避开 `&mut self`）。
        let registry = Arc::clone(&self.registry);
        let facts: Option<&dyn host::ToolFacts> = Some(&*registry as &dyn host::ToolFacts);
        let mut step = advance(
            &mut pending,
            self.options.approval,
            &mut todos,
            &mut paused,
            facts,
        );
        flush_settled(&pending, batch.step, &mut started, &mut finished, on_event);

        loop {
            match step {
                BatchStep::Complete => break,
                BatchStep::Awaiting => {
                    let Some((index, call)) = pending.current() else {
                        break;
                    };
                    let call = call.clone();
                    let arguments = call.arguments.clone();
                    // 需要人工决定的调用先报「已开始」，与 Python 的 `on_tool_start` 同时机。
                    if call.name != host::TODO_TOOL && !started[index] {
                        started[index] = true;
                        on_event(HostEvent::ToolStarted(ToolStartedPayload {
                            step: batch.step,
                            call: call.clone(),
                        }));
                    }
                    let Some(waiting) = pending.waiting().cloned() else {
                        break;
                    };
                    let next = match waiting {
                        Waiting::Approval(panel) => {
                            // 与 Python 同一顺序：`tool.call.before` → `tool.approval.before` →
                            // 人工决定 → `tool.approval.after`；被插件挡下时不再打扰用户。
                            let mut denial: Option<String> = None;
                            if !guarded[index] {
                                guarded[index] = true;
                                match self.plugin_tool_call_before(&call) {
                                    Ok(Some(rewritten)) => {
                                        pending.rewrite_arguments(index, rewritten);
                                    }
                                    Ok(None) => {}
                                    Err(reason) => denial = Some(reason),
                                }
                                if denial.is_none() {
                                    let current_arguments = pending
                                        .calls()
                                        .get(index)
                                        .map(|item| item.arguments.clone())
                                        .unwrap_or_else(|| call.arguments.clone());
                                    if let Err(reason) = self.plugin_approval_before(
                                        &call,
                                        &current_arguments,
                                        true,
                                        self.options.approval.as_str(),
                                    ) {
                                        denial = Some(reason);
                                    }
                                }
                            }
                            // 参数可能已被插件改写：审批面板与人工决定看改写后的版本。
                            let arguments = pending
                                .calls()
                                .get(index)
                                .map(|item| item.arguments.clone())
                                .unwrap_or(arguments);
                            if let Some(reason) = denial {
                                pending.record_result(
                                    index,
                                    host::denied_with_reason(&reason),
                                    None,
                                );
                                self.plugin_approval_after(&panel.tool, false, &reason);
                                decided[index] = true;
                                pending.decide(
                                    true,
                                    &mut context(
                                        self.options.approval,
                                        &mut todos,
                                        &mut paused,
                                        facts,
                                    ),
                                )
                            } else {
                                let approved =
                                    interactor.decide(&panel.tool, &arguments).unwrap_or(false);
                                let reason = if approved {
                                    String::new()
                                } else {
                                    omnicrawl_controllers::approval::user_cancelled_reason(
                                        &panel.tool,
                                    )
                                };
                                if !approved {
                                    // 人工拒绝的 MCP 调用落审计（与 Python 同粒度）。
                                    host::record_mcp_denial(
                                        &self.registry,
                                        &panel.tool,
                                        &arguments,
                                        &reason,
                                    );
                                }
                                self.plugin_approval_after(&panel.tool, approved, &reason);
                                decided[index] = true;
                                pending.decide(
                                    approved,
                                    &mut context(
                                        self.options.approval,
                                        &mut todos,
                                        &mut paused,
                                        facts,
                                    ),
                                )
                            }
                        }
                        Waiting::Question(panel) => {
                            // 提问托管：有选项且决策服务可用时自动作答，不再打扰用户；
                            // 其余情形（无选项、不可用、失败）一律 fail-open 退回人工提问。
                            let custodied = self.custody_answer(&panel);
                            let answer = match custodied.as_ref() {
                                Some(answer) => answer.clone(),
                                None => interactor
                                    .answer(&panel.prompt, &panel.options, &arguments)
                                    .unwrap_or_default(),
                            };
                            // 问答是审查模型判断授权边界的最新事实，先记下来再继续推进。
                            self.review_context.record_ask_user(&panel.prompt, &answer);
                            if let Some(answer) = custodied.as_ref() {
                                // 自动作答要留痕：用户看不到面板，但要知道发生了什么。
                                on_event(HostEvent::Notice(MessagePayload {
                                    message: custody_notice(&panel.prompt, answer),
                                }));
                            }
                            pending.answer(
                                answer,
                                &mut context(self.options.approval, &mut todos, &mut paused, facts),
                            )
                        }
                    };
                    let Some(next) = next else {
                        break;
                    };
                    flush_settled(&pending, batch.step, &mut started, &mut finished, on_event);
                    step = next;
                }
                BatchStep::Execute(mut jobs) => {
                    // 逐调用按 Python 的顺序过闸：`tool.call.before` → `tool.approval.before`
                    // →〔审查模型〕→ `tool.approval.after` → `tool.execute.before`；
                    // 任一惊拒绝就不进执行层，直接按拒绝结果回填。
                    let mut blocked: Vec<usize> = Vec::new();
                    for (index, call) in jobs.iter_mut() {
                        if !guarded[*index] {
                            guarded[*index] = true;
                            match self.plugin_tool_call_before(call) {
                                Ok(Some(rewritten)) => {
                                    pending.rewrite_arguments(*index, rewritten);
                                }
                                Ok(None) => {}
                                Err(reason) => {
                                    pending.record_result(
                                        *index,
                                        host::denied_with_reason(&reason),
                                        None,
                                    );
                                    blocked.push(*index);
                                    continue;
                                }
                            }
                            if let Err(reason) = self.plugin_approval_before(
                                call,
                                &call.arguments,
                                false,
                                self.options.approval.as_str(),
                            ) {
                                pending.record_result(
                                    *index,
                                    host::denied_with_reason(&reason),
                                    None,
                                );
                                blocked.push(*index);
                                continue;
                            }
                        }
                        // 审查只在 `review` 模式且判定为 Review 的调用上发生（删除类、
                        // 下载并执行类、高风险 Git）；其余模式下它是空操作。
                        if let Err(reason) = self.review_gate(call, facts) {
                            // 审查拒绝与人工拒绝同属「未批准」：同样落 MCP 审计。
                            host::record_mcp_denial(
                                &self.registry,
                                &call.name,
                                &call.arguments,
                                &reason,
                            );
                            pending.record_result(*index, host::denied_with_reason(&reason), None);
                            self.plugin_approval_after(&call.name, false, &reason);
                            decided[*index] = true;
                            blocked.push(*index);
                            continue;
                        }
                        if !decided[*index] {
                            decided[*index] = true;
                            self.plugin_approval_after(&call.name, true, "");
                        }
                        if let Err(reason) = self.plugin_execute_before(call) {
                            pending.record_result(*index, host::denied_with_reason(&reason), None);
                            blocked.push(*index);
                        }
                    }
                    if !blocked.is_empty() {
                        jobs.retain(|(index, _)| !blocked.contains(index));
                    }
                    for (index, call) in &jobs {
                        if call.name != host::TODO_TOOL && !started[*index] {
                            started[*index] = true;
                            on_event(HostEvent::ToolStarted(ToolStartedPayload {
                                step: batch.step,
                                call: call.clone(),
                            }));
                        }
                    }
                    if !jobs.is_empty() {
                        self.run_jobs(jobs, &completions, &sender, &mut pending);
                    }
                    flush_settled(&pending, batch.step, &mut started, &mut finished, on_event);
                    step = advance(
                        &mut pending,
                        self.options.approval,
                        &mut todos,
                        &mut paused,
                        facts,
                    );
                    flush_settled(&pending, batch.step, &mut started, &mut finished, on_event);
                }
            }
        }

        if !todos.is_empty() {
            on_event(HostEvent::TodoUpdate(TodoUpdatePayload {
                todos: todos_to_value(&todos),
            }));
        }
        let _ = paused;
        pending.observations(self.options.attach_vision_images)
    }

    /// 本批调用一个一线程并发执行，按批次绝对截止时间收口未完成的调用。
    fn run_jobs(
        &mut self,
        jobs: Vec<(usize, ToolCall)>,
        completions: &Receiver<JobDone>,
        sender: &Sender<JobDone>,
        pending: &mut PendingBatch,
    ) {
        if jobs.is_empty() {
            return;
        }
        let deadline =
            Instant::now() + Duration::from_secs(self.options.tool_timeout_seconds.max(1) as u64);
        for (index, call) in jobs {
            run_job(
                Arc::clone(&self.registry),
                self.options.plugins.clone(),
                sender.clone(),
                index,
                call,
            );
        }
        loop {
            match completions.recv_timeout(POLL_INTERVAL) {
                Ok(done) => {
                    // 顾问答复是提问托管判断「该选哪一项」的上下文，整批回填前先记下来。
                    if done.tool == crate::tools::advisor::ADVISOR_TOOL_NAME && done.result.ok {
                        self.custody_context
                            .advisor_replies
                            .push(done.result.output.clone());
                    }
                    pending.record_result(done.index, done.result, done.vision);
                    if pending.is_ready() {
                        return;
                    }
                }
                Err(RecvTimeoutError::Timeout) => {
                    if Instant::now() < deadline {
                        continue;
                    }
                    // 截止时间到了：未回填的调用写成超时结果，后台线程继续跑但结果被丢弃
                    // （与 Python 的批次绝对截止时间语义一致）。
                    pending.fill_timeout(self.options.tool_timeout_seconds);
                    diagnostics::warn(format!(
                        "[host] 工具执行超过 {} 秒仍未完成，已按超时回收等待。",
                        self.options.tool_timeout_seconds
                    ));
                    return;
                }
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
    }

    /// 提问托管：有选项的提问交给决策模型选一项，返回选中的答案。
    ///
    /// `None` 表示本次不托管（开关关、没有选项、不可用，或决策服务这一趟失败），
    /// 调用方要退回人工提问——托管的目的是省一次人工往返，不值当替用户瞎猜。
    fn custody_answer(&mut self, panel: &crate::host::QuestionPanel) -> Option<String> {
        if panel.options.is_empty() {
            return None;
        }
        // 待决提问的正文就是这个面板的问题：托管请求的 `state.question` 取它。
        self.custody_context.question = panel.prompt.clone();
        let custody = self.options.custody.as_ref()?;
        let index = chosen_option(custody, &self.custody_context, &panel.options)?;
        panel.options.get(index).cloned()
    }

    /// `tool.call.before`：可改写调用参数，也可拒绝整个调用。
    ///
    /// 返回 `Ok(Some(arguments))` 表示插件改写了参数，调用方要回写到批次里；
    /// `Err(reason)` 的文案与 Python 的 `插件拒绝工具调用：{tool}。` 逐字一致。
    fn plugin_tool_call_before(
        &self,
        call: &ToolCall,
    ) -> Result<Option<Map<String, Value>>, String> {
        let Some(plugins) = self.options.plugins.as_ref() else {
            return Ok(None);
        };
        let mut arguments = call.arguments.clone();
        if let Err(error) = plugins.tool_call_before(&call.name, &mut arguments) {
            diagnostics::warn(format!(
                "[host] {0} 被插件挡下（tool.call.before）：{error}",
                call.name
            ));
            return Err(omnicrawl_controllers::approval::plugin_call_denied_reason(
                &call.name,
            ));
        }
        if arguments == call.arguments {
            Ok(None)
        } else {
            Ok(Some(arguments))
        }
    }

    /// `tool.approval.before`：只能拒绝，不能代表用户批准。
    ///
    /// `arguments` 必须是当时生效的参数（`tool.call.before` 改写后的版本）。
    fn plugin_approval_before(
        &self,
        call: &ToolCall,
        arguments: &Map<String, Value>,
        requires_confirmation: bool,
        mode: &str,
    ) -> Result<(), String> {
        let Some(plugins) = self.options.plugins.as_ref() else {
            return Ok(());
        };
        if let Err(error) =
            plugins.tool_approval_before(&call.name, arguments, requires_confirmation, mode)
        {
            diagnostics::warn(format!(
                "[host] {0} 被插件挡下（tool.approval.before）：{error}",
                call.name
            ));
            return Err(omnicrawl_controllers::approval::plugin_approval_denied_reason(&call.name));
        }
        Ok(())
    }

    /// `tool.execute.before`：执行前守卫（在审查与审批之后）。
    fn plugin_execute_before(&self, call: &ToolCall) -> Result<(), String> {
        let Some(plugins) = self.options.plugins.as_ref() else {
            return Ok(());
        };
        if let Err(error) = plugins.tool_execute_before(&call.name, &call.arguments) {
            diagnostics::warn(format!(
                "[host] {0} 被插件挡下（tool.execute.before）：{error}",
                call.name
            ));
            return Err(omnicrawl_controllers::approval::plugin_execute_denied_reason(&call.name));
        }
        Ok(())
    }

    /// 审查闸：`review` 模式下判定为 Review 的调用交给审查模型（[`crate::review`]）。
    ///
    /// 返回 `Err(reason)` 表示拒绝，`reason` 是可直接展示的文案（审查模型不可用、请求
    /// 失败、响应无法解析都算拒绝，fail-closed）；其余情形是空操作。
    fn review_gate(
        &self,
        call: &ToolCall,
        facts: Option<&dyn host::ToolFacts>,
    ) -> Result<(), String> {
        if self.options.approval != ApprovalMode::Review {
            return Ok(());
        }
        let (description, schema) = facts
            .and_then(|facts| facts.facts(&call.name))
            .unwrap_or_default();
        if !needs_review(
            self.options.approval,
            &call.name,
            &description,
            &schema,
            &call.arguments,
        ) {
            return Ok(());
        }
        let Some(review) = self.options.review.as_ref() else {
            return Err(
                omnicrawl_controllers::approval::review_request_failed_reason(
                    "审查模型不可用：宿主未装配审查运行期。",
                ),
            );
        };
        let workspace_root = self.options.workspace_root.to_string_lossy().to_string();
        review_tool_call(
            review,
            &ReviewRequest {
                tool_name: &call.name,
                description: &description,
                arguments: &call.arguments,
                workspace_root: &workspace_root,
                context: &self.review_context,
            },
        )
    }

    /// `tool.approval.after`：审批结论通知（无插件时为空操作）。
    fn plugin_approval_after(&self, tool: &str, approved: bool, reason: &str) {
        if let Some(plugins) = self.options.plugins.as_ref() {
            plugins.tool_approval_after(tool, approved, reason, self.options.approval.as_str());
        }
    }

    /// 会话建立/恢复完成后的 `session.*` 生命周期 Hook。
    ///
    /// 由宿主在握手拿到会话标识后调用：新建会话发 `session.start.after`，恢复路径发
    /// `session.resume.before`（守卫）+ `session.resume.after`。
    pub fn notify_session_lifecycle(&self, session_id: &str) -> Result<(), TurnError> {
        let Some(plugins) = self.options.plugins.as_ref() else {
            return Ok(());
        };
        let resume_session_id = self.session_id().unwrap_or_default();
        plugins
            .session_lifecycle(&resume_session_id, session_id)
            .map_err(|error| TurnError::PluginDenied(error.to_string()))
    }
}

/// 把「刚定调」的调用补成工具生命周期事件：清单工具、被拒的调用与已作答的提问
/// 都在这里出现，时机与 Python 侧的 `on_tool_start` / `on_tool_result` 一致。
fn flush_settled(
    pending: &PendingBatch,
    batch_step: usize,
    started: &mut [bool],
    finished: &mut [bool],
    on_event: &mut dyn FnMut(HostEvent),
) {
    for (index, call) in pending.calls().iter().enumerate() {
        let Some(result) = pending.results().get(index).and_then(Option::as_ref) else {
            continue;
        };
        if !started.get(index).copied().unwrap_or(true) && call.name != host::TODO_TOOL {
            started[index] = true;
            on_event(HostEvent::ToolStarted(ToolStartedPayload {
                step: batch_step,
                call: call.clone(),
            }));
        }
        if finished.get(index).copied().unwrap_or(true) {
            continue;
        }
        finished[index] = true;
        // 清单工具成功时清单已由 `todo.update` 承载，不产生工具卡；失败仍要可见。
        if call.name == host::TODO_TOOL && result.ok {
            continue;
        }
        on_event(HostEvent::ToolFinished(ToolEventPayload {
            call: call.clone(),
            result: result.clone(),
        }));
    }
}

/// 定调一步：自持工具就地办、敏感工具返回等待、其余攒成执行批。
fn advance(
    pending: &mut PendingBatch,
    approval: ApprovalMode,
    todos: &mut Vec<TodoItem>,
    paused: &mut bool,
    tools: Option<&dyn host::ToolFacts>,
) -> BatchStep {
    pending.advance(&mut context(approval, todos, paused, tools))
}

fn context<'a>(
    approval: ApprovalMode,
    todos: &'a mut Vec<TodoItem>,
    paused: &'a mut bool,
    tools: Option<&'a dyn host::ToolFacts>,
) -> BatchContext<'a> {
    BatchContext {
        approval,
        todos,
        paused,
        tools,
    }
}

/// 一个调用一条线程：执行体的 panic 必须转成失败观察，否则整批永远凑不齐。
///
/// 插件在两头参与：panic 时发 `tool.execute.error`（通知类），有结果时发
/// `tool.execute.after` 并接受它对 `displayText` 的改写。模型看到的 `output` 不变。
fn run_job(
    registry: Arc<ToolRegistry>,
    plugins: Option<Arc<PluginHost>>,
    sender: Sender<JobDone>,
    index: usize,
    call: ToolCall,
) {
    thread::spawn(move || {
        let tool_name = call.name.clone();
        let executed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            registry.execute_with_vision(&call)
        }));
        let (mut result, vision) = match executed {
            Ok(Some(execution)) => {
                let vision = if execution.images.is_empty() {
                    None
                } else {
                    Some(VisionPayload {
                        prompt: execution.vision_prompt,
                        images: execution.images,
                    })
                };
                (execution.result, vision)
            }
            Ok(None) => (host::unavailable_result(&tool_name), None),
            Err(_) => {
                if let Some(plugins) = plugins.as_ref() {
                    plugins.tool_execute_error(&tool_name, "工具执行线程 panic");
                }
                (host::panicked_result(&tool_name), None)
            }
        };
        if let Some(plugins) = plugins.as_ref() {
            let base = if result.full_output.is_empty() {
                result.output.clone()
            } else {
                result.full_output.clone()
            };
            result.full_output = plugins.tool_execute_after(&tool_name, result.ok, &base);
        }
        let _ = sender.send(JobDone {
            index,
            tool: tool_name,
            result,
            vision,
        });
    });
}

/// 自动作答的可见提示：用户看不到提问面板，这条提示要交代「问了什么、选了什么」。
pub fn custody_notice(question: &str, answer: &str) -> String {
    format!(
        "提问已由决策模型自动作答：{question} → {answer}",
        question = question.trim(),
        answer = answer.trim()
    )
}

fn todos_to_value(todos: &[TodoItem]) -> Value {
    json!(todos
        .iter()
        .map(|item| json!({
            "id": item.id,
            "step": item.step,
            "completed": item.completed,
        }))
        .collect::<Vec<Value>>())
}

/// 无人值守的交互：握手期（还没进回合）用不到人工决策，一律拒绝。
struct Unattended;

impl Interactor for Unattended {
    fn decide(&mut self, _tool: &str, _arguments: &Map<String, Value>) -> Option<bool> {
        None
    }

    fn answer(
        &mut self,
        _prompt: &str,
        _options: &[String],
        _arguments: &Map<String, Value>,
    ) -> Option<String> {
        None
    }
}
