//! 会话：在一条 NDJSON 流上承载协议 v1。
//!
//! 内核自己拥有回合循环（`omnicrawl-core`），把两个宿主端口经协议外发：`model.reply` 代模型回复、
//! `tool.batch` 代工具批次。宿主端口是过渡形态——内核自带 provider runtime 后只需换掉
//! [宿主端口实现] 一个实现，协议与循环都不动。
//!
//! 一个回合只跑一个（第二个 `turn.submit` 回 `-32002`）；回合进行中到达的 `turn.cancel` / `shutdown`
//! 会立即中止当前端口调用，其余请求照常应答，不阻塞宿主。

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread;

use omnicrawl_config::core::runtime::ConfigEnvironment;
use omnicrawl_config::features::desensitization::{
    load_desensitization_config, DesensitizationConfig,
};
use omnicrawl_controllers::context_compaction::TokenUsageSample;
use omnicrawl_controllers::context_compaction::RECALL_SESSION_EVIDENCE_TOOL_NAME;
use omnicrawl_controllers::shared::CONTEXT_OVERFLOW_ERROR_MARKERS;
use omnicrawl_controllers::tool_args::{public_tool_arguments, TODO_TOOL_NAME};
use omnicrawl_controllers::tool_impl::{project_todos, TodoItem};
use omnicrawl_controllers::turn::continuation::{
    self, ContinueConfig, ContinueStep, LastReplyFacts,
};
use omnicrawl_controllers::turn::{is_continue_last_task_request, resolve_continue_request};
use omnicrawl_core::{
    AgentLoopLimits, AgentLoopObservation, AgentLoopResult, AgentLoopRunner, AgentModelReply,
    LoopError, LoopGuards, ReplySource, SystemClock, ToolBatchHost, ToolCall, ToolResult,
};
use omnicrawl_ipc::bridge::{
    initialize_result, method, unsupported_version_error, BridgeError, Command,
    ContextCompactionPayload, HostEvent, InitializeParams, KernelModelConfig, KernelSessionConfig,
    MessagePayload, ModelHookRequest, ModelHookResult, ModelRequest, ModelRequestErrorPayload,
    ModelResponseAfterPayload, SessionAppendParams, SessionHistoryParams, SessionListParams,
    SessionRenameParams, SessionResumeParams, SessionSettingsParams, SubagentEventPayload,
    SubagentQueryParams, SubagentRunParams, TextPayload, TokenUsagePayload, ToolBatch,
    ToolBatchResult, ToolCallArgumentsPayload, ToolCallStartedPayload, ToolOutputCompressionPayload,
    TurnCancelParams, TurnFinishedPayload, TurnSubmitParams, WorkspaceSwitchParams,
};
use omnicrawl_ipc::frame::{error_code, ErrorObject, Frame, Id};
use omnicrawl_ipc::version::negotiate_version;
use omnicrawl_llm::desensitization::ner::NerLayerOptions;
use omnicrawl_llm::desensitization::rules::{
    CATEGORY_BANK_CARD, CATEGORY_DB_CONNECTION_STRING, CATEGORY_EMAIL, CATEGORY_EXTERNAL_IP,
    CATEGORY_INTERNAL_IP, CATEGORY_LICENSE_PLATE, CATEGORY_MAC_ADDRESS, CATEGORY_PEM_PRIVATE_KEY,
    CATEGORY_URL,
};
use omnicrawl_llm::desensitization::{DesensitizationOptions, DesensitizationRuntime};
use omnicrawl_llm::{
    build_runtime, to_openai_messages, ChatRequestInput, ModelDescriptor, ModelRuntime,
    ProviderProfile, RuntimeErrorKind, SinkFlow, TurnSink, CONTEXT_LENGTH_EXCEEDED_MESSAGE,
};
use omnicrawl_protocol::{
    conform_tool_names, conversation_from_openai_messages, tool_spec_from_openai_item,
    GenerationOptions, ModelReply, ModelStreamEvent, ToolSpec,
};
use serde_json::{json, Map, Value};

use omnicrawl_controllers::subagents::batch::{
    background_rejection, batch_summary, BACKGROUND_DISABLED_MESSAGE,
};
use omnicrawl_controllers::subagents::coordinator::{
    cancelled_payload, failure_payload, json_result_text, task_event_payload,
    terminal_event_payload, top_level_error,
};
use omnicrawl_controllers::subagents::coordinator::{query_request, QueryRequest};
use omnicrawl_controllers::subagents::coordinator::{
    worktree_control_request, WorktreeControlRequest,
};
use omnicrawl_controllers::subagents::orchestration::require_completed_result;
use omnicrawl_controllers::subagents::tasks::{
    CancelToken, SubAgentTaskManager, SubAgentTaskSpec, TaskObserver, TaskRunner,
};
use omnicrawl_controllers::subagents::worktrees::{
    artifact_lines, discard_guard, WorktreeArtifacts,
};
use omnicrawl_session::{tool_result_message, utc_now, SessionListQuery, SessionStore};

use crate::compression::{CompressionPhase, KernelCompressor};
use crate::subagent::{PreparedTask, SubAgentExecution, SubAgentRuntime};
use crate::vision_proxy::KernelVisionProxy;

use crate::compaction::{
    compact_after_turn, compact_now, recall_session_evidence, recover_after_overflow, KernelSession,
};

/// 入站消息：解析好的帧，或读取端已关闭。
///
/// 帧走箱装指针：`Frame` 装着 JSON 载荷，直接内联会让每次通道投递都搬几百字节，
/// 也会让枚举尺寸被最大的变体拖大。
enum Inbound {
    Frame(Box<Frame>),
    Closed,
}

/// 端口调用失败；区分宿主主动取消与远端错误，才能映射成不同的 `LoopError`。
enum PortFailure {
    Cancelled(String),
    Shutdown,
    Disconnected,
    Remote(ErrorObject),
    Io(String),
}

impl PortFailure {
    fn into_reply_error(self) -> LoopError {
        match self {
            Self::Cancelled(message) => LoopError::Cancelled(message),
            Self::Shutdown => LoopError::Cancelled("收到 shutdown，回合中止。".to_string()),
            Self::Disconnected => LoopError::ReplySource("宿主连接已关闭。".to_string()),
            Self::Remote(error) => {
                LoopError::ReplySource(format!("宿主代答模型回复失败：{}", error.message))
            }
            Self::Io(detail) => LoopError::ReplySource(format!("写请求失败：{detail}")),
        }
    }

    fn into_tool_error(self) -> LoopError {
        match self {
            Self::Cancelled(message) => LoopError::Cancelled(message),
            Self::Shutdown => LoopError::Cancelled("收到 shutdown，回合中止。".to_string()),
            Self::Disconnected => LoopError::ToolBatch("宿主连接已关闭。".to_string()),
            Self::Remote(error) => {
                LoopError::ToolBatch(format!("宿主执行工具批次失败：{}", error.message))
            }
            Self::Io(detail) => LoopError::ToolBatch(format!("写请求失败：{detail}")),
        }
    }
}

/// 内核侧的对话连接：帧的收发、请求应答配对与回合状态。
/// 后台任务借用连接的请求：连接由主循环独占，任务线程只能经这条通道提交。
enum BackgroundRequest {
    /// 代跑一批工具（回合外批次）：子任务的工具调用仍由主循环转给宿主。
    ToolBatch {
        turn_id: String,
        calls: Vec<ToolCall>,
        /// 隔离根：worktree 子任务的工具批次要在自己的工作树里执行。
        workspace_root: Option<String>,
        reply: mpsc::SyncSender<Result<Vec<AgentLoopObservation>, String>>,
    },
    /// 事件外发：后台任务的流式增量与状态提示也要让宿主看见。
    Notify(Box<HostEvent>),
    /// 子代理的模型请求前让主循环代跑 `model.request.before`（插件运行期在宿主侧）。
    ModelHook {
        messages: Vec<Value>,
        model: String,
        reply: mpsc::SyncSender<Result<Vec<Value>, String>>,
    },
}

struct Conn {
    inbound: Receiver<Inbound>,
    out: Box<dyn Write>,
    cancel: Arc<AtomicBool>,
    handshaken: bool,
    exit_requested: bool,
    next_id: i64,
    /// 宿主交来的模型配置：有它内核自己发模型请求，没有则退回 `model.reply` 代答。
    model: Option<KernelModelConfig>,
    /// 宿主交来的会话配置：有它内核自己持有会话（多轮历史、转录落盘与回合结束后的压缩）。
    session: Option<KernelSession>,
    /// 宿主是否支持插件模型 Hook：声明后才发 `model.hook` 请求（否则不阻住回合）。
    plugin_model_hooks: bool,
    /// 本回合模型请求的用量累计与最近一次请求消息（压缩判定与前缀复用都要用）。
    usage: Rc<RefCell<TurnUsage>>,
    /// 后台任务借用连接的端口；接收端也在这里，服务方始终是当前正在跑的那条线程。
    background: mpsc::Sender<BackgroundRequest>,
    background_receiver: mpsc::Receiver<BackgroundRequest>,
    /// 后台任务管理器：首次 `action=spawn` 时创建。
    tasks: Option<SubAgentTaskManager>,
}

/// 一个回合内的模型用量累计与最近一次请求的逐字消息。
#[derive(Debug, Clone, Default)]
struct TurnUsage {
    input_tokens: i64,
    output_tokens: i64,
    cached_input_tokens: i64,
    last_request_input_tokens: i64,
    last_request_messages: Vec<Value>,
    /// 本回合是否被上游判定为上下文超限：命中即走压缩恢复再重试一次。
    context_overflow: bool,
}

impl TurnUsage {
    fn snapshot(&self) -> TokenUsageSample {
        TokenUsageSample::new(
            self.input_tokens,
            self.output_tokens,
            self.cached_input_tokens,
        )
        .unwrap_or_default()
    }

    fn reset(&mut self) {
        self.input_tokens = 0;
        self.output_tokens = 0;
        self.cached_input_tokens = 0;
        self.last_request_input_tokens = 0;
        self.last_request_messages.clear();
        self.context_overflow = false;
    }
}

impl Conn {
    fn send(&mut self, frame: &Frame) -> Result<(), String> {
        let mut line = frame.to_line();
        line.push('\n');
        self.out
            .write_all(line.as_bytes())
            .and_then(|()| self.out.flush())
            .map_err(|error| error.to_string())
    }

    fn respond(&mut self, id: Id, result: Value) {
        if let Err(detail) = self.send(&Frame::response(id, result)) {
            eprintln!("[kernel] 写响应失败：{detail}");
        }
    }

    fn respond_error(&mut self, id: Id, error: ErrorObject) {
        if let Err(detail) = self.send(&Frame::error_response(id, error)) {
            eprintln!("[kernel] 写错误响应失败：{detail}");
        }
    }

    fn notify(&mut self, event: HostEvent) {
        if let Err(detail) = self.send(&event.to_frame()) {
            eprintln!("[kernel] 写事件失败：{detail}");
        }
    }

    /// 应用一次 `session.settings`：模型配置与会话压缩配置，对后续回合生效。
    fn apply_settings(
        &mut self,
        params: &SessionSettingsParams,
    ) -> Result<Vec<String>, crate::settings::SettingsRejection> {
        let config = self.session.as_mut().map(|session| &mut session.config);
        crate::settings::apply(&mut self.model, config, params)
    }

    /// 退出前的会话收尾：补写 `session_closed` 并丢弃空占位。
    ///
    /// 宿主在发 `shutdown` 前已发 `session.close.before`、收到进程退出后发
    /// `session.close.after`，事件补写正落在这两个钩子之间。判定与幂等性见
    /// [`KernelSession::close`]。没有自持会话时什么都不做。
    fn close_session(&self) {
        if let Some(session) = self.session.as_ref() {
            session.close();
        }
    }

    /// 运行中切换工作区：把新根记到自持会话并转录 `workspace_switched`。
    ///
    /// 宿主侧已经解析并校验过路径，内核不再重复解析。`Ok(None)` 表示当前会话
    /// 不受内核持有（与 `session.append` 回同一个错误）；`Err` 是转录失败。
    fn switch_workspace(&mut self, path: &str) -> Result<Option<(String, String)>, String> {
        match self.session.as_mut() {
            Some(session) => session.switch_workspace(path).map(Some),
            None => Ok(None),
        }
    }

    /// 完成握手：版本不匹配回 `-32001`。
    ///
    /// 回包里带上当前会话 id（`initialize.session` 没给 id 而新建时，这是宿主唯一
    /// 能知道「现在在哪个会话」的途径；`/sessions`、`/rename` 都依赖它）。
    fn handshake(&mut self, id: Id, host_version: &str) {
        match negotiate_version(host_version) {
            Ok(_) => {
                self.handshaken = true;
                let session_id = self
                    .session
                    .as_ref()
                    .map(|session| session.session_id.clone())
                    .unwrap_or_default();
                let mut result = initialize_result();
                if let Some(object) = result.as_object_mut() {
                    object.insert("session_id".to_string(), Value::String(session_id));
                }
                self.respond(id, result);
            }
            Err(_) => self.respond_error(id, unsupported_version_error(host_version)),
        }
    }

    /// 发请求并等它的响应；等待期间照常处理入站帧。
    fn request(&mut self, method_name: &str, params: Value) -> Result<Value, PortFailure> {
        self.next_id += 1;
        let id = Id::Number(self.next_id);
        self.send(&Frame::request(id.clone(), method_name, params))
            .map_err(PortFailure::Io)?;
        self.await_response(&id)
    }

    fn await_response(&mut self, id: &Id) -> Result<Value, PortFailure> {
        loop {
            let frame = match self.inbound.recv() {
                Ok(Inbound::Frame(frame)) => *frame,
                Ok(Inbound::Closed) | Err(_) => return Err(PortFailure::Disconnected),
            };
            let is_response = frame.method().is_none() && frame.id().is_some();
            if is_response {
                if frame.id() == Some(id) {
                    return match &frame.error {
                        Some(error) => Err(PortFailure::Remote(error.clone())),
                        None => Ok(frame.result.clone().unwrap_or(Value::Null)),
                    };
                }
                eprintln!("[kernel] 忽略不在等待的响应：{:?}", frame.id());
                continue;
            }
            if let Some(failure) = self.handle_inbound(&frame) {
                return Err(failure);
            }
        }
    }

    /// 处理回合进行中的入站帧；返回 `Some` 表示当前回合立即中止。
    fn handle_inbound(&mut self, frame: &Frame) -> Option<PortFailure> {
        let id = frame.id().cloned();
        let method_name = frame.method().unwrap_or_default().to_string();
        match (id, method_name.as_str()) {
            (Some(id), method::TURN_CANCEL) => {
                self.cancel.store(true, Ordering::SeqCst);
                self.respond(id, json!({}));
                Some(PortFailure::Cancelled("回合已取消。".to_string()))
            }
            (Some(id), method::WORKSPACE_SWITCH) => {
                // 回合进行中切换：当前回合的工作区快照已拍，更新只对后续回合生效。
                match serde_json::from_value::<WorkspaceSwitchParams>(
                    frame.params.clone().unwrap_or_else(|| json!({})),
                ) {
                    Ok(params) => handle_workspace_switch(self, id, &params),
                    Err(error) => self.respond_error(
                        id,
                        ErrorObject::new(
                            error_code::INVALID_PARAMS,
                            format!("workspace.switch 负载不符：{error}"),
                        ),
                    ),
                }
                None
            }
            (Some(id), method::SHUTDOWN) => {
                self.cancel.store(true, Ordering::SeqCst);
                self.exit_requested = true;
                // 回合进行中也能收到 shutdown：先收尾会话再回包，保证 `session_closed`
                // 落在宿主的 before / after 钩子之间。
                self.close_session();
                self.respond(id, json!({}));
                Some(PortFailure::Shutdown)
            }
            (Some(id), method::TURN_SUBMIT) => {
                self.respond_error(
                    id,
                    ErrorObject::new(error_code::TURN_BUSY, "已有回合在执行。"),
                );
                None
            }
            (Some(id), method::INITIALIZE) => {
                let host_version = protocol_version_of(frame.params.as_ref());
                if let Some(config) = model_config_of(frame.params.as_ref()) {
                    self.model = Some(*config);
                }
                self.plugin_model_hooks = plugin_model_hooks_of(frame.params.as_ref());
                if let Some(settings) = session_config_of(frame.params.as_ref()) {
                    match KernelSession::open(*settings) {
                        Ok(opened) => self.session = Some(opened),
                        Err(detail) => {
                            self.respond_error(
                                id,
                                ErrorObject::new(
                                    error_code::INVALID_PARAMS,
                                    format!("会话初始化失败：{detail}"),
                                ),
                            );
                            return None;
                        }
                    }
                }
                self.handshake(id, &host_version);
                None
            }
            (Some(id), method::SESSION_LIST)
            | (Some(id), method::SESSION_HISTORY)
            | (Some(id), method::SESSION_EVENTS) => {
                // 只读会话查询在回合进行中也照常应答（见 [`SessionQuery`]）。
                match SessionQuery::from_frame(frame) {
                    Ok(query) => match query.respond(self) {
                        Ok(value) => self.respond(id, value),
                        Err(error) => self.respond_error(id, error),
                    },
                    Err(error) => self.respond_error(id, error),
                }
                None
            }
            (Some(id), method::SESSION_SETTINGS) => {
                // 回合进行中也照常应答：本次更新对后续回合生效，正在跑的回合
                // 在开始时已快照模型配置，不会被中途换掉。
                let params = frame.params.clone().unwrap_or_else(|| json!({}));
                match serde_json::from_value::<SessionSettingsParams>(params) {
                    Ok(params) => match self.apply_settings(&params) {
                        Ok(applied) => self.respond(id, crate::settings::applied_result(&applied)),
                        Err(rejection) => self.respond_error(id, rejection.to_error()),
                    },
                    Err(error) => self.respond_error(
                        id,
                        ErrorObject::new(
                            error_code::INVALID_PARAMS,
                            format!("session.settings 负载不符：{error}"),
                        ),
                    ),
                }
                None
            }
            (Some(id), other) => {
                self.respond_error(
                    id,
                    ErrorObject::new(error_code::METHOD_NOT_FOUND, format!("未知方法 {other}。")),
                );
                None
            }
            (None, other) => {
                eprintln!("[kernel] 忽略宿主的通知：{other}");
                None
            }
        }
    }
}

/// 模型端口（兼容路径）：把模型请求交给宿主代答，供未提供模型配置的宿主使用。
struct RemoteModelPort {
    conn: Rc<RefCell<Conn>>,
    turn_id: String,
    /// 本批 assistant 原文：代答路径下宿主回的回复同样带工具调用，事件里也要有协议字段。
    active_assistant: ActiveAssistantSlot,
}

impl ReplySource for RemoteModelPort {
    fn request_reply(&mut self, messages: &mut Vec<Value>) -> Result<AgentModelReply, LoopError> {
        let request = ModelRequest {
            turn_id: self.turn_id.clone(),
            messages: messages.clone(),
        };
        let params = serde_json::to_value(&request)
            .expect("model.reply 负载是serde_json::Value字段，必须可序列化");
        let value = self
            .conn
            .borrow_mut()
            .request(method::MODEL_REPLY, params)
            .map_err(PortFailure::into_reply_error)?;
        let reply = serde_json::from_value(value)
            .map_err(|error| LoopError::ReplySource(format!("宿主返回的模型回复无法解析：{error}")))?;
        remember_active_assistant(&self.active_assistant, &reply);
        Ok(reply)
    }
}

/// 模型端口（内核自带）：由 `omnicrawl-llm` 自己发请求，增量与用量经协议外发。
///
/// 重试位置在这里而不是循环里：Python 侧同样把 `request_retry_count` 施加在主循环，
/// 有界重试空响应与可重试错误；文案与重试节奏保持与 Python 一致。
struct KernelModelPort {
    conn: Rc<RefCell<Conn>>,
    config: KernelModelConfig,
    usage: Rc<RefCell<TurnUsage>>,
    /// 本批 assistant 原文：工具批次落 `tool_call_requested` 时取协议字段。
    active_assistant: ActiveAssistantSlot,
}

impl ReplySource for KernelModelPort {
    fn request_reply(&mut self, messages: &mut Vec<Value>) -> Result<AgentModelReply, LoopError> {
        // model.request.before：宿主声明支持时才发请求，允许插件改写消息或拒绝本轮。
        // 必须在记录「最近一次请求消息」之前：压缩前缀复用取的是改写后的消息（与 Python 同序）。
        self.run_request_hook(messages)?;
        {
            // 摘要请求要逐字复用最近一次主请求：这里记下它真正发出的消息。
            let mut usage = self.usage.borrow_mut();
            usage.last_request_messages = messages.clone();
            usage.last_request_input_tokens = 0;
        }
        let runtime = self.runtime()?;
        let options = parse_options(&self.config)?;
        // 线上名收敛：MCP 的 `server.tool`（以及资源名的 `:`/`/`）会被上游的
        // `^[a-zA-Z0-9_-]+$` 拒掉，这里换成合法名并把模型回传的调用名还原。
        let (tools, name_map) = conform_tool_names(&parse_tools(&self.config));
        let conversation = conversation_from_openai_messages(messages);
        let identity = self.config.prompt_cache_identity.clone();
        let input = ChatRequestInput {
            model: &self.config.model,
            system_prompt: &self.config.system_prompt,
            messages: &conversation,
            tools: &tools,
            options: &options,
            profile_request_timeout_seconds: self
                .config
                .request_timeout_seconds
                .filter(|seconds| *seconds > 0.0)
                .unwrap_or_default(),
            prompt_cache_capable: self.config.prompt_cache_capable,
            prompt_cache_identity: &identity,
        };
        let limit = self.config.request_retry_count.max(1);
        let mut attempt = 0;

        loop {
            let mut sink = ProtocolSink {
                conn: Rc::clone(&self.conn),
                usage: Rc::clone(&self.usage),
            };
            match runtime.run_turn(&input, &mut sink) {
                Ok(reply) => {
                    if is_empty_reply(&reply) {
                        attempt += 1;
                        if attempt < limit {
                            self.notify_retry(format!("正在重试(第{attempt}次)"));
                            continue;
                        }
                        return Err(self.model_error(format!(
                            "Agent 连续 {limit} 次返回空响应，已停止本轮请求。"
                        )));
                    }
                    let mut reply = reply;
                    name_map.restore_reply(&mut reply);
                    let reply = to_agent_reply(reply)?;
                    self.notify_model_response(&reply);
                    remember_active_assistant(&self.active_assistant, &reply);
                    return Ok(reply);
                }
                Err(error) => {
                    if error.kind == RuntimeErrorKind::Cancelled {
                        return Err(LoopError::Cancelled(error.message));
                    }
                    if error.message.contains(CONTEXT_LENGTH_EXCEEDED_MESSAGE) {
                        self.usage.borrow_mut().context_overflow = true;
                    }
                    attempt += 1;
                    // 内核策略（刻意偏离 Python 的 `is_retryable_model_request_error`）：
                    // 任何**非取消**的请求失败都先退避重试——网络抖动、网关 5xx、限流，甚至
                    // 瞬时被拒，都不该让整段会话直接断掉；确定性错误重试几次后仍会如实上报。
                    // 上下文超限例外：它要走压缩恢复路径，不能在这里耗重试。
                    let overflow = error.message.contains(CONTEXT_LENGTH_EXCEEDED_MESSAGE)
                        || self.usage.borrow().context_overflow;
                    let retryable = !overflow;
                    if retryable && attempt < limit {
                        // 已经推给界面的半截流要先撤销，否则重试会叠在旧文本上。
                        if error.kind == RuntimeErrorKind::StreamInterrupted {
                            self.conn.borrow_mut().notify(HostEvent::StreamRollback);
                        }
                        // 退避前先把入站帧抽干：这段时间里用户按 ESC 也能立刻止痛。
                        if drain_inbound_during_turn(&self.conn) {
                            return Err(LoopError::Cancelled("回合已取消。".to_string()));
                        }
                        self.notify_retry(format!("请求失败，正在自动重试（第{attempt}次）"));
                        let backoff_ms = (400u64 << (attempt - 1).min(3)).min(2_000);
                        std::thread::sleep(std::time::Duration::from_millis(backoff_ms));
                        continue;
                    }
                    if retryable {
                        return Err(self.model_error(format!(
                            "上游错误已达到本回合自动重试上限，已停止本次请求。{}",
                            error.message
                        )));
                    }
                    return Err(self.model_error(error.message));
                }
            }
        }
    }
}

impl KernelModelPort {
    /// 凭据只从环境读；端点与协议缺省时用内核工厂的默认值。
    ///
    /// 返回 trait 对象：Provider 实现与将来的装饰器（出网脱敏）都从这里换入。
    fn runtime(&self) -> Result<Box<dyn ModelRuntime>, LoopError> {
        build_model_runtime(&self.config)
    }

    fn notify_retry(&self, message: String) {
        self.conn
            .borrow_mut()
            .notify(HostEvent::RetryStatus(MessagePayload { message }));
    }

    /// `model.request.before`：宿主声明支持时才发请求；插件拒绝时中止本轮。
    ///
    /// 拒绝文案由宿主的 `HookDecision` 生成并放在错误响应里，这里原样上抛（与 Python
    /// `_plugin_denial_error("model.request.before")` 同口径）。
    fn run_request_hook(&self, messages: &mut Vec<Value>) -> Result<(), LoopError> {
        if !self.conn.borrow().plugin_model_hooks {
            return Ok(());
        }
        let request = ModelHookRequest {
            messages: messages.clone(),
            model: self.config.model.clone(),
        };
        let params =
            serde_json::to_value(&request).expect("model.hook 负载是 Value 字段，必须可序列化");
        let value = self
            .conn
            .borrow_mut()
            .request(method::MODEL_HOOK, params)
            .map_err(|failure| match failure {
                PortFailure::Cancelled(message) => LoopError::Cancelled(message),
                PortFailure::Shutdown => {
                    LoopError::Cancelled("收到 shutdown，回合中止。".to_string())
                }
                PortFailure::Remote(error) => LoopError::ReplySource(error.message),
                PortFailure::Disconnected => LoopError::ReplySource("宿主连接已关闭。".to_string()),
                PortFailure::Io(detail) => LoopError::ReplySource(format!("写请求失败：{detail}")),
            })?;
        match ModelHookResult::from_result(&value) {
            Ok(result) => {
                *messages = result.messages;
                Ok(())
            }
            Err(error) => Err(LoopError::ReplySource(format!(
                "宿主返回的 model.hook 结果无法解析：{error}"
            ))),
        }
    }

    /// `model.request.error`：一次模型请求以协议错误终结时的通知（取消不算）。
    fn notify_model_error(&self, message: &str) {
        self.conn
            .borrow_mut()
            .notify(HostEvent::ModelRequestError(ModelRequestErrorPayload {
                error: message.to_string(),
                model: self.config.model.clone(),
            }));
    }

    /// 记一条错误通知并把它原样折成循环错误。
    fn model_error(&self, message: String) -> LoopError {
        self.notify_model_error(&message);
        LoopError::ReplySource(message)
    }

    /// `model.response.after`：一次模型请求成功返回后的观察通知。
    fn notify_model_response(&self, reply: &AgentModelReply) {
        self.conn
            .borrow_mut()
            .notify(HostEvent::ModelResponseAfter(ModelResponseAfterPayload {
                model: self.config.model.clone(),
                content: reply.content.clone(),
                tool_call_count: reply.tool_calls.len(),
            }));
    }
}

/// 内核流事件 → 协议通知。
///
/// 工具生命周期事件由宿主在执行 `tool.batch` 时自行产生，这里只发模型侧的信息，
/// 避免同一件事在协议上出现两份。
struct ProtocolSink {
    conn: Rc<RefCell<Conn>>,
    usage: Rc<RefCell<TurnUsage>>,
}

impl TurnSink for ProtocolSink {
    fn on_event(&mut self, event: ModelStreamEvent) -> SinkFlow {
        // 内核自己发请求时主循环正阻塞在这一层：宿主的 `turn.cancel` / `shutdown` 只能靠
        // 每个流事件里顺手把入站帧抽干才会被看见（否则 ESC 要等整段流跑完）。
        if drain_inbound_during_turn(&self.conn) {
            return SinkFlow::Cancel;
        }
        let mut connection = self.conn.borrow_mut();
        match event {
            ModelStreamEvent::TextDelta(delta) => {
                connection.notify(HostEvent::Delta(TextPayload { text: delta.text }));
            }
            ModelStreamEvent::ReasoningDelta(delta) => {
                connection.notify(HostEvent::ReasoningDelta(TextPayload { text: delta.text }));
            }
            ModelStreamEvent::UsageReported(usage) => {
                // 用量字段是有符号的：负值原样累加与透传，与 Python 同口径。
                {
                    let mut totals = self.usage.borrow_mut();
                    totals.last_request_input_tokens = usage.input_tokens;
                    totals.input_tokens += usage.input_tokens;
                    totals.output_tokens += usage.output_tokens;
                    totals.cached_input_tokens += usage.cached_input_tokens;
                }
                connection.notify(HostEvent::TokenUsage(TokenUsagePayload {
                    input_tokens: usage.input_tokens,
                    output_tokens: usage.output_tokens,
                    cached_input_tokens: usage.cached_input_tokens,
                }));
            }
            ModelStreamEvent::ProviderWarning(warning) => {
                connection.notify(HostEvent::Status(MessagePayload {
                    message: warning.message,
                }));
            }
            // 工具调用随模型流一起给宿主：宿主先把卡片立起来、逐段补参数
            // （Python 只在批次执行时才画卡片，这是 Rust 侧刻意的增量渲染）。
            ModelStreamEvent::ToolCallStarted(started) => {
                connection.notify(HostEvent::ToolCallStarted(ToolCallStartedPayload {
                    call_id: started.call_id,
                    tool: started.name,
                }));
            }
            ModelStreamEvent::ToolCallArgumentsDelta(delta) => {
                connection.notify(HostEvent::ToolCallArguments(ToolCallArgumentsPayload {
                    call_id: delta.call_id,
                    delta: delta.delta,
                }));
            }
            _ => {}
        }
        SinkFlow::Continue
    }

    fn cancelled(&self) -> bool {
        self.conn.borrow().cancel.load(Ordering::SeqCst)
    }
}

/// 按协议组装模型运行时：Provider 的默认 API 根、能力合并与运行时选择都在内核工厂里做。
///
/// 内核没有 Profile id（宿主才有），因此这里的 `profile_id` 留空。
pub(crate) fn build_model_runtime(
    config: &KernelModelConfig,
) -> Result<Box<dyn ModelRuntime>, LoopError> {
    let api_key = read_api_key(config)?;
    build_model_runtime_with_key(config, api_key)
}

/// 同上，但复用调用方已经解析好的凭据（压缩摘要那条链自己读一次环境变量）。
pub(crate) fn build_model_runtime_with_key(
    config: &KernelModelConfig,
    api_key: String,
) -> Result<Box<dyn ModelRuntime>, LoopError> {
    let provider = if config.provider.trim().is_empty() {
        "openai"
    } else {
        config.provider.trim()
    };
    let profile = ProviderProfile {
        id: String::new(),
        provider: provider.to_string(),
        base_url: config.base_url.clone(),
        api_key,
        user_agent: config.user_agent.clone(),
        default_protocol: config.protocol.clone(),
    };
    let descriptor = ModelDescriptor {
        model_id: config.model.clone(),
        protocol: config.protocol.clone(),
        capabilities: None,
        context_window_tokens: config.context_window_tokens,
        max_output_tokens: None,
    };
    let bundle = build_runtime(&profile, &descriptor)
        .map_err(|error| LoopError::ReplySource(error.message))?;
    Ok(maybe_wrap_desensitization(bundle.runtime))
}

/// 出网脱敏装饰器（Python `maybe_wrap_runtime`）：按进程配置的 `[desensitization]` 决定是否包装。
///
/// 语义基准是 Python `registry.py::build_runtime` 在返回处调用 `maybe_wrap_runtime`——
/// 配置读取失败按「未启用」处理（零成本原样返回），不影响模型运行时构建。
/// 内核自带 provider 运行时后，脱敏也必须跟到内核侧，否则内核独立运行时会丢失出网脱敏。
fn maybe_wrap_desensitization(runtime: Box<dyn ModelRuntime>) -> Box<dyn ModelRuntime> {
    let environment = ConfigEnvironment::from_process();
    let Ok(config) = load_desensitization_config(&environment, None) else {
        return runtime;
    };
    let options = desensitization_options(&config);
    DesensitizationRuntime::maybe_wrap(runtime, &options)
}

/// `[desensitization]` 配置 → 内核装饰器选项（值类型规则层按 `detect_*` 开关裁剪）。
fn desensitization_options(config: &DesensitizationConfig) -> DesensitizationOptions {
    let rule_categories: Vec<String> = [
        (CATEGORY_PEM_PRIVATE_KEY, config.detect_pem_private_key),
        (
            CATEGORY_DB_CONNECTION_STRING,
            config.detect_db_connection_string,
        ),
        (CATEGORY_EMAIL, config.detect_email),
        (CATEGORY_BANK_CARD, config.detect_bank_card),
        (CATEGORY_INTERNAL_IP, config.detect_internal_ip),
        (CATEGORY_EXTERNAL_IP, config.detect_external_ip),
        (CATEGORY_URL, config.detect_url),
        (CATEGORY_MAC_ADDRESS, config.detect_mac_address),
        (CATEGORY_LICENSE_PLATE, config.detect_license_plate),
    ]
    .into_iter()
    .filter(|(_, enabled)| *enabled)
    .map(|(category, _)| category.to_string())
    .collect();
    let gitleaks_config_path = config.gitleaks_config_path.trim();
    DesensitizationOptions {
        enabled: config.enabled,
        strict_restore: config.strict_restore,
        fail_closed: config.fail_closed,
        entropy_enabled: config.entropy_enabled,
        entropy_min_length: config.entropy_min_length.max(0) as usize,
        entropy_min_bits: config.entropy_min_bits,
        entropy_pure_letters: config.entropy_pure_letters,
        entropy_pure_digits: config.entropy_pure_digits,
        rule_categories,
        gitleaks_enabled: config.gitleaks_enabled,
        gitleaks_config_path: if gitleaks_config_path.is_empty() {
            None
        } else {
            Some(gitleaks_config_path.to_string())
        },
        extra_sensitive_keys: config.extra_sensitive_keys.clone(),
        exempt_keys: config.exempt_keys.clone(),
        // NER 语义兜底层：配置层已就绪（`[desensitization].ner_*`），这里逐字段搬运。
        // 权重路径空串 ⇒ 交给内核的路径解析（环境变量 → 随包二进制）。
        ner: NerLayerOptions {
            enabled: config.ner_enabled,
            model_path: config.ner_model_path.clone(),
            device: config.ner_device.clone(),
            entity_types: config.ner_entity_types.clone(),
            min_entity_chars: config.ner_min_entity_chars,
            cache_size: config.ner_cache_size,
        },
    }
}

/// 凭据只从环境读：帧里出现的只是环境变量名。
pub(crate) fn read_api_key(config: &KernelModelConfig) -> Result<String, LoopError> {
    let name = config.api_key_env.trim();
    if name.is_empty() {
        return Ok(String::new());
    }
    std::env::var(name).map_err(|error| {
        LoopError::ReplySource(format!(
            "读取环境变量 {name} 失败：{error}；模型请求无法鉴权。"
        ))
    })
}

pub(crate) fn parse_options(config: &KernelModelConfig) -> Result<GenerationOptions, LoopError> {
    if config.options.is_null() {
        return Ok(GenerationOptions::default());
    }
    serde_json::from_value(config.options.clone())
        .map_err(|error| LoopError::ReplySource(format!("模型配置里的 options 无法解析：{error}")))
}

fn parse_tools(config: &KernelModelConfig) -> Vec<ToolSpec> {
    config
        .tools
        .iter()
        .filter_map(tool_spec_from_openai_item)
        .collect()
}

fn is_empty_reply(reply: &ModelReply) -> bool {
    reply.content.trim().is_empty() && reply.tool_calls.is_empty()
}

/// 归并后的回复 → 循环要的形状；assistant 消息按 OpenAI 形状回填进上下文。
fn to_agent_reply(reply: ModelReply) -> Result<AgentModelReply, LoopError> {
    let mut assistant = to_openai_messages("", std::slice::from_ref(&reply.assistant_message));
    let message = assistant
        .pop()
        .unwrap_or_else(|| json!({"role": "assistant", "content": reply.content}));
    let tool_calls = reply
        .tool_calls
        .iter()
        .map(|call| ToolCall {
            name: call.name.clone(),
            arguments: call.arguments.clone(),
            id: call.call_id.clone(),
            function_name: call.name.clone(),
        })
        .collect();
    Ok(AgentModelReply {
        message,
        content: reply.content,
        tool_calls,
        reasoning: reply.reasoning,
        content_streamed: reply.content_streamed,
    })
}

/// 本批「含工具调用的 assistant 原文」暂存格（Python 的 `_active_assistant_tool_message`）。
///
/// 模型回复带工具调用时写进来，同一批的工具批次读走——`tool_call_requested` 事件要带回
/// `assistant_content` / 思考回传字段 / `function_name`，否则恢复后同一段历史换一种写法，
/// 前缀缓存必然失效（Python 侧的原话）。**取走即清空**：批与批之间不残留，免得下个模型
/// 回合的事件带上错位的 `assistant_content`。
type ActiveAssistantSlot = Rc<RefCell<Option<Value>>>;

/// 记下这一批的 assistant 协议原文；只有「带工具调用」的回复才需要暂存。
fn remember_active_assistant(slot: &ActiveAssistantSlot, reply: &AgentModelReply) {
    if reply.tool_calls.is_empty() || !reply.message.is_object() {
        return;
    }
    *slot.borrow_mut() = Some(reply.message.clone());
}

/// 从 `initialize` 参数里取会话配置；给了就给，缺字段或形状不对按「没给」处理。
fn session_config_of(params: Option<&Value>) -> Option<Box<KernelSessionConfig>> {
    let value = params?.get("session")?;
    if value.is_null() {
        return None;
    }
    serde_json::from_value::<KernelSessionConfig>(value.clone())
        .ok()
        .map(Box::new)
}

/// 从 `initialize` 参数里取「宿主支持插件模型 Hook」的能力声明；缺省 false。
fn plugin_model_hooks_of(params: Option<&Value>) -> bool {
    params
        .and_then(|value| value.get("plugin_model_hooks"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// 从 `initialize` 参数里取模型配置；给了就给，缺字段或形状不对按「没给」处理。
fn model_config_of(params: Option<&Value>) -> Option<Box<KernelModelConfig>> {
    let value = params?.get("model")?;
    if value.is_null() {
        return None;
    }
    serde_json::from_value::<KernelModelConfig>(value.clone())
        .ok()
        .map(Box::new)
}

/// 工具端口：整批交给宿主执行，按调用顺序取回观察。
struct RemoteTools {
    conn: Rc<RefCell<Conn>>,
    turn_id: String,
    /// 隔离根：子任务把工具批次指到 worktree 里执行；主回合与共享子任务为 `None`。
    workspace_root: Option<String>,
    /// 工具输出压缩旁路：配置未启用时为 `None`，此时整批观察原样返回。
    compressor: Option<Rc<KernelCompressor>>,
    /// 独立视觉模型代理的连接配置：真有带图观察时才装配（`[vision]` 未启用时 `load` 返回 `None`）。
    vision_model: Option<KernelModelConfig>,
    /// 本轮 undo 账本：记录工具副作用，回合收尾时落 `turn_snapshot`。后台批次不记账。
    undo: Option<Rc<RefCell<crate::undo::TurnUndo>>>,
    /// 本轮任务文本，作为压缩请求的任务背景。
    task_hint: String,
    /// 本轮执行清单：`update_todos` 调用按内核投影写进来，供 run_guard 续跑判定读。
    todos: Option<Rc<RefCell<Vec<TodoItem>>>>,
    /// 工具事件落盘开关与来源：`Some` 时本批写 `tool_call_requested` / `tool_result`，
    /// 格子里是本批的 assistant 原文。**`None` 表示这批不落事件**——后台子任务批次走的就是
    /// 这条路，对应 Python 子代理循环里的 `persist_session_events=False`。
    active_assistant: Option<ActiveAssistantSlot>,
}

impl ToolBatchHost for RemoteTools {
    fn execute_tool_batch(
        &mut self,
        calls: &[ToolCall],
        first_step: usize,
    ) -> Result<Vec<AgentLoopObservation>, LoopError> {
        // 本轮 undo 账本：工作区是有 HEAD 的 Git 仓库时才记账（探测只做一次）。
        if let Some(undo) = &self.undo {
            crate::undo::record_calls(&mut undo.borrow_mut(), calls);
        }

        // 执行清单的最新一版：投影规则与 `update_todos` 的工具实现同一份
        // （`project_todos`），续跑判定与 `run_guard_*` 事件载荷都读它。
        if let Some(todos) = &self.todos {
            for call in calls {
                if call.name != TODO_TOOL_NAME {
                    continue;
                }
                let raw = call
                    .arguments
                    .get("todos")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                let projected = project_todos(&raw);
                let mut tracked = todos.borrow_mut();
                tracked.clear();
                tracked.extend(projected);
            }
        }

        // 工具事件落盘（Python `_normalize_tool_calls` 的落盘点）：模型每次请求工具都先
        // 落 `tool_call_requested`，参数走公开投影、带上本批 assistant 协议字段。
        // 后台子任务批次不落（`active_assistant` 为 None ≡ `persist_session_events=False`）；
        // 协议原文只在本批有效，读走即清空，免得残留到下一个模型回合。
        let persist_events = self.active_assistant.is_some();
        if let Some(slot) = self.active_assistant.as_ref() {
            let assistant_message = slot.borrow_mut().take();
            let connection = self.conn.borrow();
            if let Some(session) = connection.session.as_ref() {
                let _ = crate::tool_events::record_requested_calls(
                    calls,
                    assistant_message.as_ref(),
                    session.store.as_ref(),
                    &session.session_id,
                );
            }
        }

        // 内核自己持有会话时，「只读当前会话」的工具由内核直接答，不占宿主的批次；
        // 其余工具照旧整批交给宿主执行。
        let mut answers: Vec<Option<AgentLoopObservation>> =
            (0..calls.len()).map(|_| None).collect();
        let mut remote_calls: Vec<ToolCall> = Vec::new();
        let mut remote_slots: Vec<usize> = Vec::new();
        for (index, call) in calls.iter().enumerate() {
            if let Some(observation) = self.local_tool(call) {
                answers[index] = Some(observation);
                continue;
            }
            remote_calls.push(call.clone());
            remote_slots.push(index);
        }

        if !remote_calls.is_empty() {
            let batch = ToolBatch {
                turn_id: self.turn_id.clone(),
                step: first_step,
                calls: remote_calls,
                workspace_root: self.workspace_root.clone(),
            };
            let params = serde_json::to_value(&batch)
                .expect("tool.batch 负载是serde_json::Value字段，必须可序列化");
            let value = self
                .conn
                .borrow_mut()
                .request(method::TOOL_BATCH, params)
                .map_err(PortFailure::into_tool_error)?;
            let observations = ToolBatchResult::from_result(&value)
                .map(|parsed| parsed.observations)
                .map_err(|error| {
                    LoopError::ToolBatch(format!("宿主返回的观察无法解析：{error}"))
                })?;
            for (slot, observation) in remote_slots.into_iter().zip(observations) {
                answers[slot] = Some(observation);
            }
        }

        let mut ordered: Vec<AgentLoopObservation> = Vec::with_capacity(answers.len());
        for answer in answers {
            match answer {
                Some(observation) => ordered.push(observation),
                None => {
                    return Err(LoopError::ToolBatch(format!(
                        "工具批次缺少第 {} 个调用的观察。",
                        ordered.len() + 1
                    )))
                }
            }
        }

        // 宿主拒绝的调用先落进会话转录：投影与历史据此还原「用户拒绝执行」。
        // 与请求事件同一个开关：子代理的工具调用不落父会话（Python 同规则）。
        if persist_events {
            let conn = self.conn.borrow();
            if let Some(session) = conn.session.as_ref() {
                crate::approval_audit::record_denied_calls(
                    &ordered,
                    session.store.as_ref(),
                    &session.session_id,
                );
            }
        }

        // 超长输出先落盘：模型上下文只留头尾预览与落盘路径（与 Python 的批次预算同口径）。
        {
            let conn = self.conn.borrow();
            let session = conn
                .session
                .as_ref()
                .map(|session| (session.store.as_ref(), session.session_id.as_str()));
            crate::output_budget::apply_batch_output_budget(&mut ordered, session);
        }

        // 图片观察的去向与 Python `route_image_result` 同优先级：原生视觉直送主模型，
        // 否则交给独立视觉模型代理。两条路都走不通时必须把图片摄掉——主模型看不懂图，
        // 图片留在请求里只会被 Provider 拒掉；宿主侧已在未配置代理时不交图，这里是兵底。
        if crate::vision_proxy::has_vision_observation(&ordered) {
            let native_vision = self
                .vision_model
                .as_ref()
                .map(|model| model.native_vision)
                .unwrap_or(false);
            if !native_vision {
                match self.vision_model.as_ref().and_then(KernelVisionProxy::load) {
                    Some(proxy) => {
                        let cancel_source = Rc::clone(&self.conn);
                        let cancelled = || cancel_source.borrow().cancel.load(Ordering::SeqCst);
                        if !cancelled() {
                            proxy.apply_observations(&mut ordered, &cancelled);
                        }
                    }
                    None => {
                        omnicrawl_controllers::vision_proxy::strip_vision_followups(&mut ordered);
                    }
                }
            }
        }

        // 超长工具观察交给压缩旁路：未启用、失败或没压小都保留原文。
        if let Some(compressor) = &self.compressor {
            let cancel_source = Rc::clone(&self.conn);
            let cancelled = || cancel_source.borrow().cancel.load(Ordering::SeqCst);
            if !cancelled() {
                compressor.apply_observations(
                    &mut ordered,
                    &self.task_hint,
                    &cancelled,
                    &|phase| {
                        let event = match phase {
                            CompressionPhase::Started {
                                call_id,
                                tool,
                                before_chars,
                            } => HostEvent::ToolOutputCompression(ToolOutputCompressionPayload {
                                call_id: call_id.to_string(),
                                tool: tool.to_string(),
                                phase: "started".to_string(),
                                before_chars,
                                after_chars: 0,
                                output: String::new(),
                                error: String::new(),
                            }),
                            CompressionPhase::Finished {
                                call_id,
                                tool,
                                before_chars,
                                after_chars,
                                text,
                            } => HostEvent::ToolOutputCompression(ToolOutputCompressionPayload {
                                call_id: call_id.to_string(),
                                tool: tool.to_string(),
                                phase: "finished".to_string(),
                                before_chars,
                                after_chars,
                                output: text.to_string(),
                                error: String::new(),
                            }),
                            // 失败/超时也报给宿主：卡片上显示「压缩超时…」/「压缩失败…」。
                            CompressionPhase::Failed {
                                call_id,
                                tool,
                                before_chars,
                                message,
                            } => HostEvent::ToolOutputCompression(ToolOutputCompressionPayload {
                                call_id: call_id.to_string(),
                                tool: tool.to_string(),
                                phase: "failed".to_string(),
                                before_chars,
                                after_chars: 0,
                                output: String::new(),
                                error: message.to_string(),
                            }),
                        };
                        self.conn.borrow_mut().notify(event);
                    },
                );
            }
        }
        // `tool_result` 落盘（Python `_execute_tool_batch` 的落盘点在同一个位置）：写的是
        // **处理过**的观察，事件里的 output / model_output 与回填模型的文本同源；超长输出的
        // artifact 化由会话存储负责。被拒绝的调用也有结果事件（观察本身就是拒绝结果）。
        if persist_events {
            let conn = self.conn.borrow();
            if let Some(session) = conn.session.as_ref() {
                let _ = crate::tool_events::record_results(
                    &ordered,
                    session.store.as_ref(),
                    &session.session_id,
                );
            }
        }
        Ok(ordered)
    }
}

impl RemoteTools {
    /// 内核侧工具：命中即本地作答；返回 None 表示交给宿主。
    fn local_tool(&self, call: &ToolCall) -> Option<AgentLoopObservation> {
        if call.name == crate::subagent::SUBAGENT_TOOL_NAME {
            let (ok, output) = run_subagent_batch(&self.conn, &call.arguments);
            return Some(AgentLoopObservation {
                tool_call: call.clone(),
                result: ToolResult {
                    ok,
                    output: output.clone(),
                    full_output: output.clone(),
                    error_code: None,
                    retryable: false,
                },
                message: tool_result_message(&call.name, ok, &output, &call.id),
                followup_messages: Vec::new(),
            });
        }
        if call.name != RECALL_SESSION_EVIDENCE_TOOL_NAME {
            return None;
        }
        let (ok, output) = {
            let conn = self.conn.borrow();
            conn.session.as_ref()?;
            let arguments = Value::Object(call.arguments.clone());
            recall_session_evidence(conn.session.as_ref(), &arguments)
        };
        Some(AgentLoopObservation {
            tool_call: call.clone(),
            result: ToolResult {
                ok,
                output: output.clone(),
                full_output: output.clone(),
                error_code: None,
                retryable: false,
            },
            message: tool_result_message(&call.name, ok, &output, &call.id),
            followup_messages: Vec::new(),
        })
    }
}

/// 内核自持的 `subagent`：校验 → 准备 → 逐个跑子回合（子模型请求由内核发，子工具批次交宿主）。
fn run_subagent_batch(conn: &Rc<RefCell<Conn>>, arguments: &Map<String, Value>) -> (bool, String) {
    let runtime = match SubAgentRuntime::load() {
        Ok(runtime) => runtime,
        Err(detail) => {
            return (
                false,
                json_result_text(&top_level_error("SUBAGENT_DISABLED", &detail)),
            )
        }
    };
    if !runtime.enabled() {
        return (false, runtime.disabled_text());
    }
    // 后台批次：交给线程池，父回合立刻拿到受理凭据。
    if arguments.get("action").and_then(Value::as_str) == Some("spawn") {
        return spawn_subagent_batch(conn, arguments);
    }
    // 后台任务查询（list / get / cancel）由内核处理：任务表在内核里。
    if matches!(
        arguments.get("action").and_then(Value::as_str),
        Some("list") | Some("get") | Some("cancel")
    ) {
        return run_subagent_query(conn, arguments);
    }
    // worktree 控制动作（列 / 应用 / 丢弃）由内核处理：会话信息落在托管根的元数据里。
    if matches!(
        arguments.get("action").and_then(Value::as_str),
        Some("list_worktrees") | Some("apply_worktree") | Some("discard_worktree")
    ) {
        return run_worktree_control(arguments);
    }
    // 后台批次还没搬进内核：明确拒绝，避免把它当同步批次悄悄跑掉。
    if arguments.get("action").and_then(Value::as_str) == Some("spawn") {
        return (
            false,
            json_result_text(&top_level_error(
                "SUBAGENT_BACKGROUND_DISABLED",
                BACKGROUND_DISABLED_MESSAGE,
            )),
        );
    }
    let (model_config, fork_messages) = {
        let borrowed = conn.borrow();
        let Some(model) = borrowed.model.clone() else {
            return (
                false,
                json_result_text(&top_level_error(
                    "SUBAGENT_MODEL_ERROR",
                    "内核未持有模型配置，无法执行子任务。",
                )),
            );
        };
        let history = borrowed
            .session
            .as_ref()
            .map(|session| session.history.clone())
            .unwrap_or_default();
        (model, history)
    };
    let parent_tools = SubAgentRuntime::parent_tool_names(&model_config);
    let batch_id = next_subagent_batch_id();
    let tasks = match runtime.prepare(arguments, &parent_tools, &batch_id) {
        Ok(tasks) => tasks,
        Err((code, message)) => {
            return (false, json_result_text(&top_level_error(&code, &message)))
        }
    };

    emit_subagent_event(
        conn,
        "subagent.batch.created",
        json!({
            "batch_id": batch_id,
            "status": "queued",
            "task_count": tasks.len(),
        }),
    );
    let fail_fast = arguments.get("fail_fast").and_then(Value::as_bool) == Some(true);
    let max_concurrency = arguments
        .get("max_concurrency")
        .and_then(Value::as_i64)
        .unwrap_or(runtime.config.max_concurrency);
    let results = run_scheduled_tasks(
        conn,
        &Arc::new(runtime),
        &model_config,
        &tasks,
        &fork_messages,
        max_concurrency,
        fail_fast,
    );
    let (ok, summary) = batch_summary(&batch_id, results);
    (ok, json_result_text(&summary))
}

/// 内核侧的 SubAgent 所有者标识（协议一条连接只服务一个宿主）。
const SUBAGENT_OWNER_ID: &str = "kernel";

/// `action=spawn`：把任务交给后台线程池；工具批次仍由主循环转给宿主。
fn spawn_subagent_batch(
    conn: &Rc<RefCell<Conn>>,
    arguments: &Map<String, Value>,
) -> (bool, String) {
    let runtime = match SubAgentRuntime::load() {
        Ok(runtime) => runtime,
        Err(detail) => {
            return (
                false,
                json_result_text(&top_level_error("SUBAGENT_DISABLED", &detail)),
            )
        }
    };
    if !runtime.enabled() {
        return (false, runtime.disabled_text());
    }
    let fail_fast = arguments.get("fail_fast").and_then(Value::as_bool) == Some(true);
    if let Some(rejection) = background_rejection(&runtime.config, fail_fast) {
        return (false, json_result_text(&rejection));
    }
    let (model_config, fork_messages, session_id) = {
        let borrowed = conn.borrow();
        let Some(model) = borrowed.model.clone() else {
            return (
                false,
                json_result_text(&top_level_error(
                    "SUBAGENT_MODEL_ERROR",
                    "内核未持有模型配置，无法执行子任务。",
                )),
            );
        };
        let history = borrowed
            .session
            .as_ref()
            .map(|session| session.history.clone())
            .unwrap_or_default();
        let session_id = borrowed
            .session
            .as_ref()
            .map(|session| session.session_id.clone())
            .unwrap_or_default();
        (model, history, session_id)
    };
    let parent_tools = SubAgentRuntime::parent_tool_names(&model_config);
    let batch_id = next_subagent_batch_id();
    let tasks = match runtime.prepare(arguments, &parent_tools, &batch_id) {
        Ok(tasks) => tasks,
        Err((code, message)) => {
            return (false, json_result_text(&top_level_error(&code, &message)))
        }
    };
    let specs: Vec<SubAgentTaskSpec> = tasks
        .iter()
        .map(|task| {
            SubAgentTaskSpec::new(
                task.task_id.clone(),
                task.description.clone(),
                task.agent_type.clone(),
                task.batch_id.clone(),
            )
        })
        .collect();

    let runtime = Arc::new(runtime);
    let prepared: Arc<BTreeMap<String, PreparedTask>> = Arc::new(
        tasks
            .iter()
            .map(|task| (task.task_id.clone(), task.clone()))
            .collect(),
    );
    let fork_messages = Arc::new(fork_messages);
    let model_config = Arc::new(model_config);
    let sender = conn.borrow().background.clone();
    let runner: TaskRunner = {
        let runtime = Arc::clone(&runtime);
        let prepared = Arc::clone(&prepared);
        let fork_messages = Arc::clone(&fork_messages);
        let model_config = Arc::clone(&model_config);
        let sender = sender.clone();
        Arc::new(move |spec, cancel| {
            let Some(task) = prepared.get(&spec.task_id) else {
                let mut failure = Map::new();
                failure.insert("status".to_string(), Value::String("failed".to_string()));
                return failure;
            };
            match run_background_task(
                &runtime,
                &model_config,
                task,
                &fork_messages,
                &sender,
                cancel,
                None,
                false,
            ) {
                Ok(execution) => runtime
                    .completed_result(task, &execution)
                    .as_object()
                    .cloned()
                    .unwrap_or_default(),
                Err((code, message)) => failure_payload(&task.view(), &code, &message, None)
                    .as_object()
                    .cloned()
                    .unwrap_or_default(),
            }
        })
    };
    let observer: TaskObserver = {
        let sender = sender.clone();
        Arc::new(move |name, payload| {
            let _ = sender.send(BackgroundRequest::Notify(Box::new(
                HostEvent::SubagentEvent(SubagentEventPayload {
                    name: name.to_string(),
                    payload,
                }),
            )));
        })
    };

    let mut borrowed = conn.borrow_mut();
    if borrowed.tasks.is_none() {
        let retention_seconds = (runtime.config.task_retention_minutes.max(1) as f64) * 60.0;
        let workers = runtime.config.max_concurrency.max(1) as usize;
        borrowed.tasks = Some(SubAgentTaskManager::new(retention_seconds, workers));
    }
    let Some(manager) = borrowed.tasks.as_ref() else {
        return (
            false,
            json_result_text(&top_level_error(
                "SUBAGENT_MODEL_ERROR",
                "后台任务管理器不可用。",
            )),
        );
    };
    match manager.spawn(
        SUBAGENT_OWNER_ID,
        &session_id,
        &specs,
        runner,
        Some(observer),
    ) {
        Ok(value) => (true, json_result_text(&value)),
        Err(error) => (
            false,
            json_result_text(&top_level_error("SUBAGENT_LIMIT_EXCEEDED", error.message())),
        ),
    }
}

/// 后台任务的子回合：模型请求直连，工具批次经通道交给主循环。
#[allow(clippy::too_many_arguments)]
fn run_background_task(
    runtime: &SubAgentRuntime,
    model_config: &KernelModelConfig,
    task: &PreparedTask,
    fork_messages: &[Value],
    sender: &mpsc::Sender<BackgroundRequest>,
    cancel: &CancelToken,
    workspace_root: Option<&str>,
    stream_conversation: bool,
) -> Result<SubAgentExecution, (String, String)> {
    let mut messages = runtime.task_messages(task, fork_messages);
    let mut model = BackgroundModelPort {
        config: runtime.child_model_config(model_config, task),
        sender: sender.clone(),
        cancel: cancel.clone(),
        task_id: task.task_id.clone(),
        batch_id: task.batch_id.clone(),
        agent_type: task.agent_type.clone(),
        stream: stream_conversation,
    };
    let mut tools = BackgroundTools {
        sender: sender.clone(),
        turn_id: task.task_id.clone(),
        workspace_root: workspace_root.map(str::to_string),
        batch_id: task.batch_id.clone(),
        agent_type: task.agent_type.clone(),
        stream: stream_conversation,
    };
    let runner = AgentLoopRunner::new(Box::new(SystemClock::new()));
    let cancel_for_check = cancel.clone();
    let mut cancel_check = move || {
        if cancel_for_check.is_set() {
            Err(LoopError::Cancelled("子任务已取消。".to_string()))
        } else {
            Ok(())
        }
    };
    let guards = LoopGuards {
        cancel_check: Some(&mut cancel_check),
        stop_check: None,
    };
    let limits = AgentLoopLimits {
        max_model_turns: None,
        max_tool_calls: None,
        timeout_seconds: Some(runtime.config.default_timeout_seconds),
    };
    match runner.run(&mut messages, &mut model, &mut tools, limits, guards) {
        Ok(result) => Ok(SubAgentExecution {
            final_text: result.final_text,
            model_turns: result.model_turns,
            tool_calls: result.tool_calls,
            input_tokens: 0,
            output_tokens: 0,
            cached_input_tokens: 0,
        }),
        Err(LoopError::Cancelled(message)) => Err(("SUBAGENT_CANCELLED".to_string(), message)),
        Err(error) => Err((
            "SUBAGENT_MODEL_ERROR".to_string(),
            format!("子任务模型请求失败：{}", error.tag()),
        )),
    }
}

/// `action=list|get|cancel`：查询与取消后台任务。
fn run_subagent_query(conn: &Rc<RefCell<Conn>>, arguments: &Map<String, Value>) -> (bool, String) {
    let request = match query_request(&Value::Object(arguments.clone())) {
        Ok(request) => request,
        Err((code, message)) => {
            return (false, json_result_text(&top_level_error(&code, &message)))
        }
    };
    let borrowed = conn.borrow();
    let Some(manager) = borrowed.tasks.as_ref() else {
        return (
            false,
            json_result_text(&top_level_error(
                "SUBAGENT_NOT_FOUND",
                "当前还没有后台 SubAgent 任务。",
            )),
        );
    };
    let session_id = borrowed
        .session
        .as_ref()
        .map(|session| session.session_id.clone())
        .unwrap_or_default();
    match request {
        QueryRequest::List => {
            let tasks = manager.list(SUBAGENT_OWNER_ID, Some(&session_id));
            let notifications = manager.drain_notifications(SUBAGENT_OWNER_ID, Some(&session_id));
            (
                true,
                json_result_text(&json!({
                    "tasks": tasks,
                    "notifications": notifications,
                })),
            )
        }
        QueryRequest::Get { task_id } => {
            match manager.get(&task_id, SUBAGENT_OWNER_ID, Some(&session_id)) {
                Some(task) => (true, json_result_text(&json!({"task": task}))),
                None => (
                    false,
                    json_result_text(&top_level_error(
                        "SUBAGENT_NOT_FOUND",
                        "未找到当前 Agent 的任务。",
                    )),
                ),
            }
        }
        QueryRequest::Cancel { task_id, batch_id } => {
            let value = manager.cancel(
                SUBAGENT_OWNER_ID,
                Some(&session_id),
                (!task_id.is_empty()).then_some(task_id.as_str()),
                (!batch_id.is_empty()).then_some(batch_id.as_str()),
            );
            (true, json_result_text(&value))
        }
    }
}

/// 同步批次的调度：`max_concurrency` 决定同时在跑几个任务。
///
/// 子任务的模型请求在各自线程里直连，工具批次经端口回到本线程转发给宿主——本线程在等结果时
/// 就调用 `drain_background`，所以并发再高也只有一个 `tool.batch` 在途，宿主无需改造。
fn run_scheduled_tasks(
    conn: &Rc<RefCell<Conn>>,
    runtime: &Arc<SubAgentRuntime>,
    model_config: &KernelModelConfig,
    tasks: &[PreparedTask],
    fork_messages: &[Value],
    max_concurrency: i64,
    fail_fast: bool,
) -> Vec<Value> {
    let total = tasks.len();
    let mut results: Vec<Option<Value>> = (0..total).map(|_| None).collect();
    let concurrency = max_concurrency.max(1) as usize;
    let sender = conn.borrow().background.clone();
    let (tx, rx) = mpsc::channel::<(usize, Value)>();
    let cancels: Vec<CancelToken> = (0..total).map(|_| CancelToken::new()).collect();
    let mut next = 0usize;
    let mut running = 0usize;
    let mut stopped = false;

    loop {
        if conn.borrow().cancel.load(Ordering::SeqCst) {
            // 父回合被取消：停止调度，并让在跑的任务尽快收尾。
            stopped = true;
            for cancel in &cancels {
                cancel.set();
            }
        }
        while !stopped && running < concurrency && next < total {
            let index = next;
            next += 1;
            running += 1;
            let task = tasks[index].clone();
            let runtime = Arc::clone(runtime);
            let model_config = model_config.clone();
            let fork_messages = fork_messages.to_vec();
            let sender = sender.clone();
            let cancel = cancels[index].clone();
            let tx = tx.clone();
            let workspace =
                std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
            thread::spawn(move || {
                let result = run_task_in_isolation(
                    &runtime,
                    &model_config,
                    &task,
                    &fork_messages,
                    &sender,
                    &cancel,
                    &workspace,
                    false,
                );
                let _ = tx.send((index, result));
            });
        }
        if running == 0 {
            break;
        }
        match rx.recv_timeout(BACKGROUND_TICK) {
            Ok((index, result)) => {
                running -= 1;
                if fail_fast && result.get("status").and_then(Value::as_str) != Some("completed") {
                    stopped = true;
                }
                results[index] = Some(result);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
        drain_background(conn);
    }

    results
        .into_iter()
        .enumerate()
        .map(|(index, value)| {
            value.unwrap_or_else(|| {
                let view = tasks[index].view();
                let payload = cancelled_payload(
                    &view,
                    if fail_fast {
                        FAIL_FAST_STOPPED_MESSAGE
                    } else {
                        "父任务已取消尚未调度的子任务。"
                    },
                );
                emit_subagent_event(
                    conn,
                    &terminal_event_name(&payload),
                    terminal_event_payload(&view, &payload),
                );
                payload
            })
        })
        .collect()
}

/// 一个子任务从隔离区到公开结果的全过程：建工作树 → 跑子回合 → 收集产物 → 发事件。
///
/// `keep_full_text` 为真时把子 Agent 的收尾文本原样挂进结果的 `full_text`：
/// `/review` 这类宿主入口要自己解析结构化 JSON 报告，而公开的 `summary` 会按
/// `result_summary_chars` 截断，截断会直接把 JSON 打碎（与 Python `keep_full_text=True` 同义）。
#[allow(clippy::too_many_arguments)]
fn run_task_in_isolation(
    runtime: &SubAgentRuntime,
    model_config: &KernelModelConfig,
    task: &PreparedTask,
    fork_messages: &[Value],
    sender: &mpsc::Sender<BackgroundRequest>,
    cancel: &CancelToken,
    workspace: &std::path::Path,
    keep_full_text: bool,
) -> Value {
    let view = task.view();
    emit_background_event(
        sender,
        "subagent.task.started",
        task_event_payload(&view, "started"),
    );
    let session = if task.definition.isolation == "worktree" {
        match crate::worktree::create_session(workspace, &task.task_id, "HEAD", None) {
            Ok(session) => Some(session),
            Err(error) => {
                let result =
                    failure_payload(&view, "SUBAGENT_WORKTREE_ERROR", error.message(), None);
                emit_background_event(
                    sender,
                    &terminal_event_name(&result),
                    terminal_event_payload(&view, &result),
                );
                return result;
            }
        }
    } else {
        None
    };
    let root = session
        .as_ref()
        .map(|session| session.worktree_path.to_string_lossy().to_string());
    let result = match run_background_task(
        runtime,
        model_config,
        task,
        fork_messages,
        sender,
        cancel,
        root.as_deref(),
        keep_full_text,
    ) {
        Ok(execution) => {
            let mut result = runtime.completed_result(task, &execution);
            if keep_full_text {
                if let Some(object) = result.as_object_mut() {
                    object.insert("full_text".to_string(), Value::String(execution.final_text));
                }
            }
            result
        }
        Err((code, message)) => failure_payload(&view, &code, &message, None),
    };
    let result = match session.as_ref().map(crate::worktree::collect_artifacts) {
        Some(Ok(artifacts)) => attach_worktree_artifacts(result, &artifacts),
        Some(Err(error)) => attach_worktree_note(
            result,
            &format!("worktree 产物收集失败：{}", error.message()),
        ),
        None => result,
    };
    emit_background_event(
        sender,
        &terminal_event_name(&result),
        terminal_event_payload(&view, &result),
    );
    result
}

fn terminal_event_name(result: &Value) -> String {
    let status = result
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("failed");
    format!("subagent.task.{status}")
}

fn emit_background_event(sender: &mpsc::Sender<BackgroundRequest>, name: &str, payload: Value) {
    let _ = sender.send(BackgroundRequest::Notify(Box::new(
        HostEvent::SubagentEvent(SubagentEventPayload {
            name: name.to_string(),
            payload,
        }),
    )));
}

/// 主循环在等宿主帧时的轮询间隔：间隙用来服务后台任务借用的连接。
const BACKGROUND_TICK: std::time::Duration = std::time::Duration::from_millis(20);

/// `fail_fast` 在前序任务失败后写进未调度任务的原因（与 Python 同文案）。
const FAIL_FAST_STOPPED_MESSAGE: &str = "fail_fast 已在前序任务失败后停止调度该任务。";

/// 把 worktree 产物摘要挂到子任务结果的 `artifacts` 上（与 Python 的 `[worktree]` 标记同义）。
fn attach_worktree_artifacts(mut result: Value, artifacts: &WorktreeArtifacts) -> Value {
    let lines = artifact_lines(Some(artifacts));
    if lines.is_empty() {
        return result;
    }
    if let Some(object) = result.as_object_mut() {
        let entry = json!({"type": "worktree", "content": lines.join("\n")});
        match object.get_mut("artifacts").and_then(Value::as_array_mut) {
            Some(items) => items.push(entry),
            None => {
                object.insert("artifacts".to_string(), Value::Array(vec![entry]));
            }
        }
    }
    result
}

/// 产物收集失败时留下可诊断的说明，不改动任务状态。
fn attach_worktree_note(mut result: Value, note: &str) -> Value {
    if let Some(object) = result.as_object_mut() {
        object.insert("worktree_note".to_string(), Value::String(note.to_string()));
    }
    result
}

/// 处理 `list_worktrees` / `apply_worktree` / `discard_worktree`。
fn run_worktree_control(arguments: &Map<String, Value>) -> (bool, String) {
    let request = match worktree_control_request(&Value::Object(arguments.clone())) {
        Ok(request) => request,
        Err((code, message)) => {
            return (false, json_result_text(&top_level_error(&code, &message)))
        }
    };
    match request {
        WorktreeControlRequest::List => {
            let sessions: Vec<Value> = crate::worktree::list_sessions(None)
                .iter()
                .map(crate::worktree::session_value)
                .collect();
            (true, json_result_text(&json!({"worktrees": sessions})))
        }
        WorktreeControlRequest::Apply {
            key,
            strategy,
            cleanup,
        } => {
            let Some(session) = crate::worktree::find_session(&key, None) else {
                return (
                    false,
                    json_result_text(&top_level_error(
                        "SUBAGENT_NOT_FOUND",
                        &format!("未找到 SubAgent worktree 会话：{key}"),
                    )),
                );
            };
            match crate::worktree::apply_to_main(&session, &strategy) {
                Ok(message) => {
                    if cleanup {
                        let _ = crate::worktree::cleanup_session(&session, true);
                    }
                    (
                        true,
                        json_result_text(&json!({
                            "action": "apply_worktree",
                            "key": key,
                            "strategy": strategy,
                            "cleanup": cleanup,
                            "message": message,
                        })),
                    )
                }
                Err(error) => (
                    false,
                    json_result_text(&top_level_error(
                        "SUBAGENT_MODEL_ERROR",
                        &format!("应用 worktree 失败：{}", error.message()),
                    )),
                ),
            }
        }
        WorktreeControlRequest::Discard {
            key,
            remove_branch,
            force,
        } => {
            let Some(session) = crate::worktree::find_session(&key, None) else {
                return (
                    false,
                    json_result_text(&top_level_error(
                        "SUBAGENT_NOT_FOUND",
                        &format!("未找到 SubAgent worktree 会话：{key}"),
                    )),
                );
            };
            if !force {
                match crate::worktree::summarize_changes(&session) {
                    Ok(Some(changes)) => {
                        if let Some(reason) = discard_guard(changes) {
                            return (
                                false,
                                json_result_text(&top_level_error(
                                    "SUBAGENT_PERMISSION_DENIED",
                                    &reason,
                                )),
                            );
                        }
                    }
                    Ok(None) => {}
                    Err(error) => {
                        return (
                            false,
                            json_result_text(&top_level_error(
                                "SUBAGENT_MODEL_ERROR",
                                &format!(
                                    "worktree 变更检查失败（可传 force=true 强制丢弃）：{}",
                                    error.message()
                                ),
                            )),
                        )
                    }
                }
            }
            match crate::worktree::cleanup_session(&session, remove_branch) {
                Ok(()) => (
                    true,
                    json_result_text(&json!({
                        "action": "discard_worktree",
                        "key": key,
                        "remove_branch": remove_branch,
                        "force": force,
                        "message": format!("已清理 worktree 会话：{}", session.branch_name),
                    })),
                ),
                Err(error) => (
                    false,
                    json_result_text(&top_level_error(
                        "SUBAGENT_MODEL_ERROR",
                        &format!("清理 worktree 失败：{}", error.message()),
                    )),
                ),
            }
        }
    }
}

/// 同步路径的子代理事件：直接经连接外发。
fn emit_subagent_event(conn: &Rc<RefCell<Conn>>, name: &str, payload: Value) {
    conn.borrow_mut()
        .notify(HostEvent::SubagentEvent(SubagentEventPayload {
            name: name.to_string(),
            payload,
        }));
}

/// 读取 `[run_guard].continuation`：与其它内核旁路配置同一套解析
/// （显式路径 > `AI_CONFIG_FILE` > 用户目录）。
///
/// 总开关或续跑开关关闭、配置读不出来都返回 `None`：调用点因此不必区分「没配置」与「关掉了」。
fn load_continue_config() -> Option<ContinueConfig> {
    let environment = omnicrawl_config::core::runtime::ConfigEnvironment::from_process();
    let config =
        omnicrawl_config::features::run_guard::load_run_guard_config(&environment, None).ok()?;
    ContinueConfig::resolve(
        config.enabled,
        config.continuation.enabled,
        config.continuation.max_auto_followups,
    )
}

/// 会话投影里的清单条目 → 判定用的清单项（形状不符的条目直接跳过）。
fn todo_item_from_value(value: &Value) -> Option<TodoItem> {
    let object = value.as_object()?;
    let step = object
        .get("step")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    if step.is_empty() {
        return None;
    }
    Some(TodoItem {
        id: object
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        step,
        completed: object
            .get("completed")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    })
}

/// 回合内的续跑痕迹：待续文本、逐次续跑事件与终态。
///
/// 对应 Python `run_stream` 内联在编排壳里的局部变量。收尾时按同一顺序写进会话转录：
/// `user_message` → `run_guard_continue`×N → `run_guard_paused` / `assistant_message`
/// → `run_guard_continue_exhausted`。
struct TurnRunGuard {
    /// 本轮发给模型的用户文本（短「继续」已还原成上一轮的任务原文）。
    user_text: String,
    /// 待续任务文本：用户发「继续」时是上一轮的任务原文，否则是本轮输入。
    pending_user_text: String,
    /// 本轮用户就是发「继续」进来的。
    continue_requested: bool,
    /// 收尾时用的执行清单（Stop 分支同步一次）。
    todos: Vec<TodoItem>,
    /// 逐段循环的回复与推理（最终按 Python 的 join 规则合并）。
    reply_parts: Vec<String>,
    reasoning_parts: Vec<String>,
    /// 整回合是否有文本流式发过。
    content_streamed: bool,
    /// 已自动续跑的次数。
    followups: usize,
    /// 最近一次续跑理由。
    reason: String,
    /// 逐次续跑事件载荷，按发生顺序。
    continue_events: Vec<Value>,
    /// 本回合被 `pause_work` 主动暂停。
    paused: bool,
    /// 续跑上限已用尽且仍未完成。
    exhausted: bool,
}

impl TurnRunGuard {
    fn begin(user_text: &str, continue_requested: bool, pending_user_text: &str) -> Self {
        let pending = if continue_requested && !pending_user_text.trim().is_empty() {
            pending_user_text.trim().to_string()
        } else {
            user_text.to_string()
        };
        Self {
            user_text: user_text.to_string(),
            pending_user_text: pending,
            continue_requested,
            todos: Vec::new(),
            reply_parts: Vec::new(),
            reasoning_parts: Vec::new(),
            content_streamed: false,
            followups: 0,
            reason: String::new(),
            continue_events: Vec::new(),
            paused: false,
            exhausted: false,
        }
    }

    /// 并进一段循环的收尾事实。
    fn record_episode(&mut self, result: &AgentLoopResult) {
        continuation::record_episode(
            &mut self.reply_parts,
            &mut self.reasoning_parts,
            &mut self.content_streamed,
            &result.final_text,
            &result.reasoning,
            result.content_streamed,
        );
    }

    /// 记一次续跑：次数、理由与事件载荷。
    fn record_continue(&mut self, followup: usize, reason: &'static str) {
        self.followups = followup;
        self.reason = reason.to_string();
        self.continue_events
            .push(continuation::continue_event_payload(
                followup,
                reason,
                &self.pending_user_text,
            ));
    }

    /// 本回合发给模型的最终回复。
    fn final_reply(&self) -> String {
        continuation::joined_reply(&self.reply_parts)
    }

    /// 合并后的推理文本。
    fn final_reasoning(&self) -> String {
        continuation::joined_reasoning(&self.reasoning_parts)
    }

    /// `user_message` 事件载荷：待续文本总是写；清单只在「继续」回合写。
    fn user_message_payload(&self) -> Value {
        let mut payload = json!({
            "content": self.user_text,
            "pending_user_text": self.pending_user_text,
        });
        if self.continue_requested && !self.todos.is_empty() {
            payload["todo_items"] = continuation::todo_items_value(&self.todos);
        }
        payload
    }

    /// 助手消息载荷：有推理文本时带 `reasoning_content`。
    fn assistant_payload(&self, final_text: &str) -> Value {
        let mut payload = json!({"content": final_text.trim()});
        let reasoning = self.final_reasoning();
        if !reasoning.is_empty() {
            payload["reasoning_content"] = Value::from(reasoning);
        }
        payload
    }

    /// `run_guard_paused` 事件载荷。
    fn paused_payload(&self) -> Value {
        continuation::paused_event_payload(&self.user_text, &self.pending_user_text, &self.todos)
    }

    /// `run_guard_continue_exhausted` 事件载荷。
    fn exhausted_payload(&self) -> Value {
        continuation::exhausted_event_payload(
            self.followups,
            &self.pending_user_text,
            &self.reason,
            &self.todos,
        )
    }
}

/// 批次号：12 位十六进制，时间戳与自增计数混合，避免同进程内重复。
fn next_subagent_batch_id() -> String {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos() as u64)
        .unwrap_or(0);
    let tick = COUNTER.fetch_add(1, Ordering::SeqCst);
    let mixed = nanos ^ tick.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    format!("batch-{:012x}", mixed & 0xffff_ffff_ffff)
}

/// 跑一个回合：用户输入进上下文 → 模型与工具交替 → `turn.finished`。
fn run_turn(conn: &Rc<RefCell<Conn>>, request_id: Id, params: TurnSubmitParams) {
    let turn_id = params.turn_id.clone();
    conn.borrow().cancel.store(false, Ordering::SeqCst);

    let user_text = params.user_text.clone();
    let usage = Rc::clone(&conn.borrow().usage);
    usage.borrow_mut().reset();
    // 短「继续/重试」要还原成上一轮未完成的真实任务：待续文本与清单从会话推测（只有真是
    // 「继续」才读一次转录），之后的持久化与模型上下文都用还原后的文本——与 Python 的
    // `_resolve_continue_request` 同一时机（早于 user_message 落盘与请求组装）。
    let continue_requested = is_continue_last_task_request(&user_text);
    let (recovered_pending, recovered_todos) = if continue_requested {
        conn.borrow()
            .session
            .as_ref()
            .map(|session| session.run_guard_state())
            .unwrap_or_default()
    } else {
        (String::new(), Vec::new())
    };
    let model_text = resolve_continue_request(&user_text, Some(&recovered_pending));
    let model_config = conn.borrow().model.clone();
    // system 之外的上下文消息（项目规范、Skill、工具能力说明、运行环境）由宿主整段给来，
    // 每轮插在历史之前：这些是「本轮读到的真实环境」，不能随会话落盘（Python 侧同语义）。
    let context_messages = model_config
        .as_ref()
        .map(|config| config.context_messages.clone())
        .unwrap_or_default();
    // 有会话就从转录恢复的运行期历史接着走；没有会话维持「一回合一条 user 消息」的旧行为。
    let mut messages = conn
        .borrow()
        .session
        .as_ref()
        .map(|session| session.history.clone())
        .unwrap_or_default();
    messages.push(json!({"role": "user", "content": model_text.clone()}));
    // 宿主给了模型配置就由内核自己发请求；没给则维持 model.reply 代答的兼容路径。
    if !context_messages.is_empty() {
        let mut combined: Vec<Value> = Vec::with_capacity(context_messages.len() + messages.len());
        combined.extend(context_messages.iter().cloned());
        combined.extend(messages);
        messages = combined;
    }
    // 本批 assistant 原文：模型端口写、工具端口读，两个端口共用同一个格子。
    let active_assistant: ActiveAssistantSlot = Rc::new(RefCell::new(None));
    let mut model: Box<dyn ReplySource> = match model_config.clone() {
        Some(config) => Box::new(KernelModelPort {
            conn: Rc::clone(conn),
            config,
            usage: Rc::clone(&usage),
            active_assistant: Rc::clone(&active_assistant),
        }),
        None => Box::new(RemoteModelPort {
            conn: Rc::clone(conn),
            turn_id: turn_id.clone(),
            active_assistant: Rc::clone(&active_assistant),
        }),
    };
    let compressor = model_config
        .clone()
        .and_then(|config| KernelCompressor::load(&config))
        .map(Rc::new);
    // 本轮 undo 账本：工作区来自 initialize 的 session 配置，非 Git 仓库时自动不记账。
    let undo_workspace = {
        let connection = conn.borrow();
        connection
            .session
            .as_ref()
            .and_then(|session| session.workspace.clone())
    };
    let undo = Rc::new(RefCell::new(crate::undo::TurnUndo::begin(
        undo_workspace.as_deref(),
    )));
    // 本轮执行清单：`update_todos` 的投影由工具批次线程写进来，续跑判定与 `run_guard_*`
    // 事件载荷读同一份——对应 Python 的 `_active_todo_items`。用户发「继续」时，先把
    // 会话投影里的清单当起点（Python 的 `source_todos = session_todos or previous_todo_items`）。
    let active_todos: Rc<RefCell<Vec<TodoItem>>> = Rc::new(RefCell::new(Vec::new()));
    if continue_requested {
        active_todos
            .borrow_mut()
            .extend(recovered_todos.iter().filter_map(todo_item_from_value));
    }
    let mut tools = RemoteTools {
        conn: Rc::clone(conn),
        turn_id: turn_id.clone(),
        workspace_root: None,
        compressor,
        vision_model: model_config.clone(),
        undo: Some(Rc::clone(&undo)),
        task_hint: user_text.clone(),
        todos: Some(Rc::clone(&active_todos)),
        active_assistant: Some(active_assistant),
    };
    let build_cancel_check = || {
        let cancel_source = Rc::clone(conn);
        move || {
            if cancel_source.borrow().cancel.load(Ordering::SeqCst) {
                Err(LoopError::Cancelled("回合已取消。".to_string()))
            } else {
                Ok(())
            }
        }
    };
    let runner = AgentLoopRunner::new(Box::new(SystemClock::new()));

    // run_guard 续跑：一段循环跑完后，模型只给 reasoning（没有文本与工具调用）或执行清单
    // 仍有未完成项时自动补发一轮「继续」，上限是 `[run_guard].continuation.max_auto_followups`；
    // 判定面在 `omnicrawl_controllers::turn::continuation`，这里只做模型调用与事件累积。
    let continuation = load_continue_config();
    let mut run_guard = TurnRunGuard::begin(&model_text, continue_requested, &recovered_pending);
    // 用户消息**先落盘**再发请求（与 Python `loop.py` 同序）：回合中途失败或被取消时，
    // 下一轮与恢复投影仍然看得到这次提问，而不是把整轮丢在转录之外。
    run_guard.todos = active_todos.borrow().clone();
    if let Some(session) = conn.borrow_mut().session.as_mut() {
        if let Err(detail) = session.append("user_message", run_guard.user_message_payload()) {
            eprintln!("[kernel] 会话写入用户消息失败：{detail}");
        }
    }

    // 主 Agent 的预算是无限的：与 Python 侧一致，靠取消与停止检查来收敛。
    // 上下文超限时压缩当前未完成回合，用返回的投影重试一次（与 Python 的恢复路径同口径）。
    let outcome = 'episodes: loop {
        let mut recovered = false;
        let attempt = loop {
            let mut cancel_check = build_cancel_check();
            let guards = LoopGuards {
                cancel_check: Some(&mut cancel_check),
                stop_check: None,
            };
            let attempt = runner.run(
                &mut messages,
                &mut *model,
                &mut tools,
                AgentLoopLimits::default(),
                guards,
            );
            match attempt {
                Err(LoopError::ReplySource(message))
                    if !recovered
                        && (usage.borrow().context_overflow || is_context_overflow(&message)) =>
                {
                    recovered = true;
                    let recovered_history = model_config
                        .clone()
                        .and_then(|config| recover_context_overflow(conn, &messages, &config));
                    match recovered_history {
                        Some(projection) => {
                            // 压缩投影只覆盖历史；上下文消息每轮照旧插在前面。
                            let mut combined: Vec<Value> =
                                Vec::with_capacity(context_messages.len() + projection.len());
                            combined.extend(context_messages.iter().cloned());
                            combined.extend(projection);
                            messages = combined;
                            continue;
                        }
                        None => break Err(LoopError::ReplySource(message)),
                    }
                }
                other => break other,
            }
        };
        let result = match attempt {
            Ok(result) => result,
            Err(error) => break 'episodes Err(error),
        };
        let last_reply = result.last_reply.as_ref().map(|reply| {
            LastReplyFacts::new(&reply.reasoning, &reply.content, reply.tool_calls.len())
        });
        run_guard.record_episode(&result);
        run_guard.paused = result.paused;
        let todos = active_todos.borrow().clone();
        match continuation::next_step(
            continuation.as_ref(),
            run_guard.followups,
            result.paused,
            last_reply.as_ref(),
            &todos,
        ) {
            ContinueStep::Stop => {
                // 上限用尽仍未完成：落可恢复终态，事件保留待续文本（用户可显式发「继续」）。
                run_guard.exhausted = continuation::continuation_exhausted(
                    continuation.as_ref(),
                    run_guard.followups,
                    result.paused,
                    last_reply.as_ref(),
                    &todos,
                );
                run_guard.todos = todos;
                break 'episodes Ok(result);
            }
            ContinueStep::Continue { followup, reason } => {
                run_guard.record_continue(followup, reason);
                conn.borrow_mut().notify(HostEvent::Status(MessagePayload {
                    message: continuation::continue_status_text(followup),
                }));
                // 上一段循环的最终回复要进上下文（它不在 `messages` 里），随后才是补发指令。
                if let Some(reply) = result.last_reply.as_ref() {
                    messages.push(reply.message.clone());
                }
                messages.push(json!({
                    "role": "user",
                    "content": continuation::CONTINUE_PROMPT,
                }));
            }
        }
    };

    let mut connection = conn.borrow_mut();
    match outcome {
        Ok(result) => {
            // 续跑各段的回复按 `"\n\n"` 合并后去掉首尾空白（与 Python 的 `final_reply` 同一算法）。
            let final_text = run_guard.final_reply();
            let final_reasoning = run_guard.final_reasoning();
            // 先落盘再通知：宿主收到 turn.finished 时转录必须已经持久化，
            // 否则宿主此刻退出（或被强杀）就会丢掉这一轮的问答。
            let session = connection.session.take();
            drop(connection);
            if let Some(mut session) = session {
                run_session_tail(
                    conn,
                    &mut session,
                    &run_guard,
                    &messages,
                    &final_text,
                    &turn_id,
                );
                conn.borrow_mut().session = Some(session);
                // 快照事件跟在会话事件之后：`/undo` 的副作用回滚依赖它还原工作区。
                if let Some(session) = conn.borrow().session.as_ref() {
                    let store = Arc::clone(&session.store);
                    let session_id = session.session_id.clone();
                    if let Err(detail) = undo.borrow_mut().complete(&store, &session_id) {
                        eprintln!("[kernel] 本轮快照落盘失败：{detail}");
                    }
                }
            }
            conn.borrow_mut()
                .notify(HostEvent::TurnFinished(TurnFinishedPayload {
                    turn_id: turn_id.clone(),
                    final_text,
                    reasoning: final_reasoning,
                    model_turns: result.model_turns,
                    tool_calls: result.tool_calls,
                    paused: result.paused,
                }));
            conn.borrow_mut().respond(request_id, json!({}));
        }
        Err(error) => {
            eprintln!("[kernel] 回合失败：{}", error.message());
            // 失败/取消同样要留下可恢复的终态（Python `loop.py` 的两个 except 分支）：
            // 用户消息此前已落盘，这里补一条终态事件并把投影写回运行期历史，否则紧接着的
            // 下一次提问看不到被中断的任务与已执行的工具。
            let cancelled = matches!(error, LoopError::Cancelled(_));
            let executed = undo.borrow().executed_tools().to_vec();
            let reason = error.message();
            let response = turn_error(&error);
            let session = connection.session.take();
            drop(connection);
            if let Some(mut session) = session {
                let (event_type, payload) = if cancelled {
                    (
                        "turn_cancelled",
                        json!({
                            "user_text": user_text.clone(),
                            "pending_user_text": run_guard.pending_user_text.clone(),
                            "reason": reason,
                            "summary": cancelled_turn_summary(&executed),
                        }),
                    )
                } else {
                    (
                        "session_interrupted",
                        json!({
                            "user_text": user_text.clone(),
                            "pending_user_text": run_guard.pending_user_text.clone(),
                            "reason": reason,
                        }),
                    )
                };
                if let Err(detail) = session.append(event_type, payload) {
                    eprintln!("[kernel] 会话写入回合终态失败：{detail}");
                }
                // 与 Python `_commit_turn_history()` 同义：把本轮已落盘的事件投影回运行期
                // 历史，下一轮接着走时上下文完整。
                if let Err(detail) = session.reload_history() {
                    eprintln!("[kernel] 重建运行期历史失败：{detail}");
                }
                conn.borrow_mut().session = Some(session);
            }
            conn.borrow_mut().respond_error(request_id, response);
        }
    }
}

/// 被取消回合的历史摘要（语义基准 Python `TurnLoopMixin._cancelled_turn_summary`）。
///
/// 只写纯文本助手消息（不写未配对的 `tool_calls`），带上本轮已执行工具的次数统计；
/// 这是紧接着的下一轮能延续上下文的依据。
fn cancelled_turn_summary(executed_tools: &[String]) -> String {
    let mut counts: Vec<(String, usize)> = Vec::new();
    for name in executed_tools {
        let name = name.trim();
        if name.is_empty() {
            continue;
        }
        match counts.iter_mut().find(|(existing, _)| existing == name) {
            Some((_, count)) => *count += 1,
            None => counts.push((name.to_string(), 1)),
        }
    }
    if counts.is_empty() {
        return "（上一回合被取消，未生成最终回复，未执行任何工具）".to_string();
    }
    let summary = counts
        .iter()
        .map(|(name, count)| {
            if *count > 1 {
                format!("{name}×{count}")
            } else {
                name.clone()
            }
        })
        .collect::<Vec<_>>()
        .join("，");
    format!("（上一回合被取消，未生成最终回复）已执行工具：{summary}")
}

/// 上下文超限的错误判定：与 Python 侧 `_CONTEXT_OVERFLOW_ERROR_MARKERS` 同源。
fn is_context_overflow(message: &str) -> bool {
    CONTEXT_OVERFLOW_ERROR_MARKERS
        .iter()
        .any(|marker| message.contains(marker))
}

/// 上下文超限后的恢复：压缩当前未完成回合，返回可继续的历史投影。
fn recover_context_overflow(
    conn: &Rc<RefCell<Conn>>,
    previous_history: &[Value],
    model_config: &KernelModelConfig,
) -> Option<Vec<Value>> {
    let mut session = conn.borrow_mut().session.take()?;
    let api_key = read_api_key(model_config).unwrap_or_default();
    let last_request_messages = conn.borrow().usage.borrow().last_request_messages.clone();
    let projection = match recover_after_overflow(
        &session,
        model_config,
        &api_key,
        &last_request_messages,
        previous_history,
    ) {
        Ok(report) => {
            if let Some(notice) = report.notice.as_deref() {
                conn.borrow_mut().notify(HostEvent::Status(MessagePayload {
                    message: format!(
                        "{notice}（检测到上下文超限，已压缩当前任务上下文并自动继续。）"
                    ),
                }));
            }
            if !report.compacted {
                eprintln!("[kernel] 上下文超限后的模型压缩失败：{}", report.diagnostic);
            }
            report.history
        }
        Err(detail) => {
            eprintln!("[kernel] 上下文超限后的模型压缩失败：{detail}");
            None
        }
    };
    match projection.as_ref() {
        Some(history) => session.history = history.clone(),
        None => {
            if let Err(detail) = session.reload_history() {
                eprintln!("[kernel] 重建运行期历史失败：{detail}");
            }
        }
    }
    conn.borrow_mut().session = Some(session);
    projection
}

/// 回合收尾：落会话事件（用户消息与最终回复）→ 跑一次压缩 → 通知提示并更新运行期历史。
fn run_session_tail(
    conn: &Rc<RefCell<Conn>>,
    session: &mut KernelSession,
    run_guard: &TurnRunGuard,
    working_messages: &[Value],
    final_text: &str,
    turn_id: &str,
) {
    // `user_message` 已在发请求之前落盘（见 `run_turn`），这里只写本轮终态与续跑痕迹。
    // 续跑痕迹的落盘顺序与 Python 完全一致：先逐次 `run_guard_continue`，再写本轮终态。
    for payload in &run_guard.continue_events {
        if let Err(detail) = session.append(continuation::CONTINUE_EVENT, payload.clone()) {
            eprintln!("[kernel] 会话写入续跑事件失败：{detail}");
        }
    }
    if run_guard.paused {
        // 暂停回合同样写入完整协议轨迹：恢复投影会把 `run_guard_paused` 还原成说明消息。
        if let Err(detail) = session.append(continuation::PAUSED_EVENT, run_guard.paused_payload())
        {
            eprintln!("[kernel] 会话写入暂停事件失败：{detail}");
        }
    } else if !final_text.trim().is_empty() {
        if let Err(detail) =
            session.append("assistant_message", run_guard.assistant_payload(final_text))
        {
            eprintln!("[kernel] 会话写入助手回复失败：{detail}");
        }
    }
    if run_guard.exhausted {
        if let Err(detail) = session.append(
            continuation::CONTINUE_EXHAUSTED_EVENT,
            run_guard.exhausted_payload(),
        ) {
            eprintln!("[kernel] 会话写入续跑终态失败：{detail}");
        }
    }

    let model = conn.borrow().model.clone();
    let Some(model) = model else {
        // 没有模型配置就没有可发的摘要请求：保留转录，历史按转录重建。
        if let Err(detail) = session.reload_history() {
            eprintln!("[kernel] 重建运行期历史失败：{detail}");
        }
        return;
    };
    let api_key = read_api_key(&model).unwrap_or_default();
    let (usage, last_request_input_tokens, last_request_messages) = {
        let connection = conn.borrow();
        let totals = connection.usage.borrow();
        (
            totals.snapshot(),
            totals.last_request_input_tokens,
            totals.last_request_messages.clone(),
        )
    };
    match compact_after_turn(
        session,
        &model,
        &api_key,
        usage,
        last_request_input_tokens,
        &last_request_messages,
        working_messages,
    ) {
        Ok(report) => {
            if !report.compacted && !report.diagnostic.is_empty() {
                eprintln!(
                    "[kernel] 模型摘要未通过校验，已跳过本回合压缩：{}",
                    report.diagnostic
                );
            }
            // 触发压缩的回合把计量发给宿主：宿主据此分发 `context.compaction.after_turn`
            // 插件 Hook（Python 在同一位置由 agent runtime 内部派发）。未触发则不发。
            if report
                .measurement_payload
                .get("trigger_reached")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                let post_turn_context_tokens = report
                    .measurement_payload
                    .get("post_turn_context_tokens")
                    .and_then(Value::as_i64)
                    .unwrap_or(0);
                let trigger_context_tokens = report
                    .measurement_payload
                    .get("trigger_context_tokens")
                    .and_then(Value::as_i64)
                    .unwrap_or(0);
                conn.borrow_mut()
                    .notify(HostEvent::ContextCompaction(ContextCompactionPayload {
                        post_turn_context_tokens,
                        trigger_context_tokens,
                        turn_id: turn_id.to_string(),
                    }));
            }
            if let Some(notice) = report.notice {
                conn.borrow_mut()
                    .notify(HostEvent::Status(MessagePayload { message: notice }));
            }
            match report.history {
                Some(history) => session.history = history,
                None => {
                    if let Err(detail) = session.reload_history() {
                        eprintln!("[kernel] 重建运行期历史失败：{detail}");
                    }
                }
            }
        }
        Err(detail) => {
            eprintln!("[kernel] 上下文压缩失败，已跳过本回合：{detail}");
            if let Err(detail) = session.reload_history() {
                eprintln!("[kernel] 重建运行期历史失败：{detail}");
            }
        }
    }
}

/// 显式压缩当前会话：摘要与压缩结果回到 `{summary, compacted}`。
fn respond_compact(conn: &Rc<RefCell<Conn>>, id: Id) {
    let target = {
        let connection = conn.borrow();
        connection
            .session
            .as_ref()
            .map(|session| (session.session_id.clone(), connection.model.clone()))
    };
    let Some((_session_id, model)) = target else {
        conn.borrow_mut().respond_error(
            id,
            ErrorObject::new(
                error_code::INVALID_REQUEST,
                "当前没有可压缩的活动 Session。",
            ),
        );
        return;
    };
    let Some(model) = model else {
        conn.borrow_mut().respond_error(
            id,
            ErrorObject::new(
                error_code::INVALID_REQUEST,
                "内核缺少模型配置，无法生成摘要。",
            ),
        );
        return;
    };
    let api_key = read_api_key(&model).unwrap_or_default();
    let last_request_messages = {
        let connection = conn.borrow();
        let usage = connection.usage.borrow();
        usage.last_request_messages.clone()
    };
    let outcome = {
        let connection = conn.borrow();
        let Some(session) = connection.session.as_ref() else {
            return;
        };
        compact_now(session, &model, &api_key, &last_request_messages)
    };

    match outcome {
        Ok(report) => {
            // 压缩后运行期历史要换成摘要 + 保留窗口，否则下一轮还会带上被压缩的原文。
            match report.history.clone() {
                Some(history) => {
                    if let Some(session) = conn.borrow_mut().session.as_mut() {
                        session.history = history;
                    }
                }
                None => {
                    // `reload_history` 就地在会话上重建运行期历史，需要可变借用。
                    if let Some(session) = conn.borrow_mut().session.as_mut() {
                        if let Err(detail) = session.reload_history() {
                            eprintln!("[kernel] 压缩后重建运行期历史失败：{detail}");
                        }
                    }
                }
            }
            if let Some(notice) = report.notice.clone() {
                conn.borrow_mut()
                    .notify(HostEvent::Status(MessagePayload { message: notice }));
            }
            conn.borrow_mut().respond(
                id,
                json!({"summary": report.summary, "compacted": report.compacted}),
            );
        }
        Err(detail) => {
            conn.borrow_mut()
                .respond_error(id, ErrorObject::new(error_code::INVALID_REQUEST, detail));
        }
    }
}

/// `subagent.query`：后台任务的查询与取消，返回结构化 JSON（不再走工具结果信封）。
///
/// 没有任务管理器时回 `unavailable: true`，让宿主区分「功能未启用」（503）与
/// 「任务不存在」（404）；动作非法按协议错误回。
fn subagent_query_value(
    conn: &Rc<RefCell<Conn>>,
    params: &SubagentQueryParams,
) -> Result<Value, String> {
    let action = params.action.trim().to_lowercase();
    // worktree 清单放在任务表检查之前：托管根里的残留 worktree 与后台任务管理器是否启用无关，
    // 切换工作区前必须能看到它们（对映 Python `list_subagent_worktrees` 的拦阻项）。
    if action == "list_worktrees" {
        let sessions: Vec<Value> = crate::worktree::list_sessions(None)
            .iter()
            .map(crate::worktree::session_value)
            .collect();
        return Ok(json!({
            "unavailable": false,
            "action": action,
            "tasks": [],
            "task": Value::Null,
            "result": {},
            "worktrees": sessions,
        }));
    }
    if !matches!(action.as_str(), "list" | "get" | "cancel") {
        return Err(format!(
            "action 仅支持 list、get、cancel 或 list_worktrees，收到：{}",
            params.action
        ));
    }
    let borrowed = conn.borrow();
    let Some(manager) = borrowed.tasks.as_ref() else {
        return Ok(json!({
            "unavailable": true,
            "action": action,
            "tasks": [],
            "task": Value::Null,
            "result": {},
        }));
    };
    let session_id = borrowed
        .session
        .as_ref()
        .map(|session| session.session_id.clone())
        .unwrap_or_default();
    let task_id = params.task_id.trim();

    Ok(match action.as_str() {
        "list" => json!({
            "unavailable": false,
            "action": action,
            "tasks": manager.list(SUBAGENT_OWNER_ID, Some(&session_id)),
            "task": Value::Null,
            "result": {},
        }),
        "get" => json!({
            "unavailable": false,
            "action": action,
            "tasks": [],
            "task": manager
                .get(task_id, SUBAGENT_OWNER_ID, Some(&session_id))
                .unwrap_or(Value::Null),
            "result": {},
        }),
        _ => json!({
            "unavailable": false,
            "action": action,
            "tasks": [],
            "task": Value::Null,
            "result": manager.cancel(
                SUBAGENT_OWNER_ID,
                Some(&session_id),
                (!task_id.is_empty()).then_some(task_id),
                None,
            ),
        }),
    })
}

/// 循环错误 → 协议错误对象；`data.kind` 用 `LoopError::tag()`，宿主可据此分支。
fn turn_error(error: &LoopError) -> ErrorObject {
    let code = match error {
        LoopError::Cancelled(_) => error_code::TURN_CANCELLED,
        LoopError::InvalidBudget(_) => error_code::INVALID_PARAMS,
        _ => error_code::TURN_FAILED,
    };
    ErrorObject::with_data(code, error.message(), json!({"kind": error.tag()}))
}

/// 处理一条空闲状态下的入站帧；返回 `true` 表示会话应当结束。
fn dispatch(conn: &Rc<RefCell<Conn>>, frame: Frame) -> bool {
    let Some(id) = frame.id().cloned() else {
        eprintln!(
            "[kernel] 忽略宿主的通知：{}",
            frame.method().unwrap_or("<无方法>")
        );
        return false;
    };
    if frame.method().is_none() {
        eprintln!("[kernel] 忽略没有 method 的响应帧。");
        return false;
    }
    if !conn.borrow().handshaken && frame.method() != Some(method::INITIALIZE) {
        conn.borrow_mut().respond_error(
            id,
            ErrorObject::new(error_code::INVALID_REQUEST, "未完成 initialize 握手。"),
        );
        return false;
    }

    let pending = match Command::from_frame(&frame) {
        Ok(pending) => pending,
        Err(failure) => {
            let error = match failure {
                BridgeError::UnknownMethod(name) => {
                    ErrorObject::new(error_code::METHOD_NOT_FOUND, format!("未知方法 {name}。"))
                }
                other => ErrorObject::new(error_code::INVALID_PARAMS, other.to_string()),
            };
            conn.borrow_mut().respond_error(id, error);
            return false;
        }
    };

    let (id, command) = (pending.id, pending.command);
    match command {
        Command::Initialize(InitializeParams {
            protocol_version,
            model,
            session,
            plugin_model_hooks,
            ..
        }) => {
            if let Some(config) = model {
                conn.borrow_mut().model = Some(*config);
            }
            conn.borrow_mut().plugin_model_hooks = plugin_model_hooks;
            if let Some(settings) = session {
                match KernelSession::open(*settings) {
                    Ok(opened) => {
                        eprintln!("[kernel] 会话已就绪：{}", opened.session_id);
                        conn.borrow_mut().session = Some(opened);
                    }
                    Err(detail) => {
                        conn.borrow_mut().respond_error(
                            id,
                            ErrorObject::new(
                                error_code::INVALID_PARAMS,
                                format!("会话初始化失败：{detail}"),
                            ),
                        );
                        return false;
                    }
                }
            }
            conn.borrow_mut().handshake(id, &protocol_version);
            false
        }
        Command::TurnSubmit(params) => {
            run_turn(conn, id, params);
            conn.borrow().exit_requested
        }
        Command::TurnCancel(TurnCancelParams { .. }) => {
            conn.borrow_mut().respond(id, json!({}));
            false
        }
        Command::TurnUndo => {
            respond_undo(conn, id);
            false
        }
        Command::SessionSettings(params) => {
            // 借用必须在同一条语句里结束：respond 还要可变借用同一条连接。
            let outcome = conn.borrow_mut().apply_settings(&params);
            match outcome {
                Ok(applied) => {
                    let result = crate::settings::applied_result(&applied);
                    conn.borrow_mut().respond(id, result);
                }
                Err(rejection) => conn.borrow_mut().respond_error(id, rejection.to_error()),
            }
            false
        }
        Command::SessionCompact => {
            respond_compact(conn, id);
            false
        }
        Command::SubagentQuery(params) => {
            match subagent_query_value(conn, &params) {
                Ok(value) => conn.borrow_mut().respond(id, value),
                Err(message) => conn
                    .borrow_mut()
                    .respond_error(id, ErrorObject::new(error_code::INVALID_PARAMS, message)),
            }
            false
        }
        Command::SessionList(params) => {
            respond_session_query(conn, id, &SessionQuery::List(params));
            false
        }
        Command::SessionRename(params) => {
            respond_session_rename(conn, id, &params);
            false
        }
        Command::SessionArchive => {
            respond_session_archive(conn, id);
            false
        }
        Command::SessionHistory(params) => {
            respond_session_query(conn, id, &SessionQuery::History(params));
            false
        }
        Command::SessionEvents => {
            respond_session_query(conn, id, &SessionQuery::Events);
            false
        }
        Command::SessionNew => {
            respond_session_new(conn, id);
            false
        }
        Command::SessionResume(params) => {
            respond_session_resume(conn, id, &params);
            false
        }
        Command::SubagentRun(params) => {
            respond_subagent_run(conn, id, &params);
            false
        }
        Command::SessionAppend(params) => {
            respond_session_append(conn, id, &params);
            false
        }
        Command::WorkspaceSwitch(params) => {
            handle_workspace_switch(&mut conn.borrow_mut(), id, &params);
            false
        }
        Command::Shutdown => {
            // 正常退出：先收尾会话（写 `session_closed` 并丢弃空占位），再回包退出。
            conn.borrow().close_session();
            conn.borrow_mut().respond(id, json!({}));
            true
        }
    }
}

/// 撤销最近一轮：恢复工作区副作用并回退会话逻辑，结果作为命令响应返回。
fn respond_undo(conn: &Rc<RefCell<Conn>>, id: Id) {
    let target = {
        let connection = conn.borrow();
        connection.session.as_ref().map(|session| {
            (
                Arc::clone(&session.store),
                session.session_id.clone(),
                session.workspace.clone(),
            )
        })
    };
    let Some((store, session_id, workspace)) = target else {
        conn.borrow_mut().respond_error(
            id,
            ErrorObject::new(
                error_code::INVALID_REQUEST,
                "当前会话不受内核持有，无法撤销。",
            ),
        );
        return;
    };
    let Some(workspace) = workspace else {
        conn.borrow_mut().respond_error(
            id,
            ErrorObject::new(
                error_code::INVALID_REQUEST,
                "会话未绑定工作区，无法回滚副作用。",
            ),
        );
        return;
    };
    match crate::undo::undo_last_turn(&store, &session_id, std::path::Path::new(&workspace)) {
        Ok(outcome) => {
            // 回退后运行期历史要跟着会话重建，否则下一轮请求还会带上被撤回的消息。
            if let Some(session) = conn.borrow_mut().session.as_mut() {
                if let Err(detail) = session.reload_history() {
                    eprintln!("[kernel] 撤销后重建运行期历史失败：{detail}");
                }
            }
            // 回退后的历史一并回给宿主：它据此重放对话视图（撤回的消息与工具卡不能留着）。
            let history = conn
                .borrow()
                .session
                .as_ref()
                .map(|session| Value::Array(session.history.clone()))
                .unwrap_or(Value::Null);
            conn.borrow_mut().respond(
                id,
                json!({
                    "kind": outcome.kind,
                    "message_count": outcome.message_count,
                    "side_effects_reverted": outcome.side_effects_reverted,
                    "unrestorable": outcome.unrestorable,
                    "history": history,
                }),
            );
        }
        Err(detail) => {
            conn.borrow_mut()
                .respond_error(id, ErrorObject::new(error_code::INVALID_REQUEST, detail));
        }
    }
}

/// 当前内核持有的会话存储与 id；没有自持会话时给出同一条明确拒绝。
fn session_handle(conn: &Conn) -> Option<(Arc<SessionStore>, String)> {
    conn.session
        .as_ref()
        .map(|session| (Arc::clone(&session.store), session.session_id.clone()))
}

/// 会话的**只读**查询：`session.list`、`session.history` 与 `session.events`。
///
/// 三条都只看会话目录（索引、提示历史与转录文件），与正在跑的回合不共享可变状态，所以回合内
/// 也可应答：宿主侧 `/sessions`、`/archives`、`/history` 是「立即命令」，生成期间照常
/// 执行，若内核只在空闲时应答，它们就只能撞上「未知方法」。
#[derive(Debug, Clone)]
enum SessionQuery {
    List(SessionListParams),
    History(SessionHistoryParams),
    /// 回放用的有效事件流：不带参数，读内核当前持有的会话。
    Events,
}

impl SessionQuery {
    /// 解帧；`method` 从帧里取，非法负载按协议报错。
    fn from_frame(frame: &Frame) -> Result<Self, ErrorObject> {
        let params = frame.params.clone().unwrap_or_else(|| json!({}));
        let invalid = |error: serde_json::Error| {
            ErrorObject::new(
                error_code::INVALID_PARAMS,
                format!("{} 负载不符：{error}", frame.method().unwrap_or_default()),
            )
        };
        match frame.method() {
            Some(method::SESSION_LIST) => {
                Ok(Self::List(serde_json::from_value(params).map_err(invalid)?))
            }
            Some(method::SESSION_HISTORY) => Ok(Self::History(
                serde_json::from_value(params).map_err(invalid)?,
            )),
            Some(method::SESSION_EVENTS) => Ok(Self::Events),
            other => Err(ErrorObject::new(
                error_code::METHOD_NOT_FOUND,
                format!("未知方法 {}。", other.unwrap_or_default()),
            )),
        }
    }

    fn respond(&self, conn: &Conn) -> Result<Value, ErrorObject> {
        let Some((store, current)) = session_handle(conn) else {
            return Err(no_session_error());
        };
        match self {
            Self::List(params) => {
                // 不按工作区过滤：与 Python「列出全部会话，不再绑当前工作区」一致。
                let query = SessionListQuery {
                    workspace_root: None,
                    project_path: None,
                    limit: params.limit as usize,
                    include_archived: false,
                    archived_only: params.archived,
                };
                let entries = store.list_sessions_filtered(&query).map_err(|error| {
                    ErrorObject::new(
                        error_code::INTERNAL_ERROR,
                        format!("会话列表读取失败：{}", error.message()),
                    )
                })?;
                let items: Vec<Value> = entries.iter().map(|entry| entry.to_dict()).collect();
                Ok(json!({ "sessions": items, "current_session_id": current }))
            }
            Self::History(params) => {
                let entries = store
                    .prompt_history()
                    .search(None, None, &params.query, i64::from(params.limit))
                    .map_err(|error| {
                        ErrorObject::new(
                            error_code::INTERNAL_ERROR,
                            format!("提示历史读取失败：{}", error.message()),
                        )
                    })?;
                let items: Vec<Value> = entries.iter().map(|entry| entry.to_dict()).collect();
                Ok(json!({ "entries": items }))
            }
            Self::Events => {
                // 回退投影后的有效事件流（`turn_undone` 撤掉的轮次不出现），与
                // Python `current_session_events()` 读的是同一条视图，UI 回放据此
                // 重建消息、工具卡与 SubAgent 进度树。
                let events = store
                    .read_active_events(&current)
                    .map_err(|error| {
                        ErrorObject::new(
                            error_code::INTERNAL_ERROR,
                            format!("会话事件读取失败：{}", error.message()),
                        )
                    })?
                    .iter()
                    .map(|event| event.to_dict())
                    .collect::<Vec<Value>>();
                Ok(json!({ "session_id": current, "events": events }))
            }
        }
    }
}

/// 内核没有自持会话时的统一拒绝（会话读写都要求 `initialize.session`）。
fn no_session_error() -> ErrorObject {
    ErrorObject::new(
        error_code::INVALID_REQUEST,
        "当前会话不受内核持有，无法执行该会话操作。",
    )
}

/// 把一次只读会话查询的结果写回（空闲状态下的分发入口）。
fn respond_session_query(conn: &Rc<RefCell<Conn>>, id: Id, query: &SessionQuery) {
    let outcome = query.respond(&conn.borrow());
    match outcome {
        Ok(value) => conn.borrow_mut().respond(id, value),
        Err(error) => conn.borrow_mut().respond_error(id, error),
    }
}

fn respond_no_session(conn: &Rc<RefCell<Conn>>, id: Id) {
    conn.borrow_mut().respond_error(id, no_session_error());
}

/// `session.rename`：重命名当前会话。
fn respond_session_rename(conn: &Rc<RefCell<Conn>>, id: Id, params: &SessionRenameParams) {
    let Some((store, current)) = session_handle(&conn.borrow()) else {
        respond_no_session(conn, id);
        return;
    };
    match store.rename_session(&current, &params.title, utc_now()) {
        Ok(entry) => conn
            .borrow_mut()
            .respond(id, json!({ "session": entry.to_dict() })),
        Err(error) => conn.borrow_mut().respond_error(
            id,
            ErrorObject::new(
                error_code::INVALID_REQUEST,
                format!("重命名会话失败：{}", error.message()),
            ),
        ),
    }
}

/// `session.archive`：归档当前会话并立即开启一个新的空会话。
fn respond_session_archive(conn: &Rc<RefCell<Conn>>, id: Id) {
    let Some((store, current)) = session_handle(&conn.borrow()) else {
        respond_no_session(conn, id);
        return;
    };
    let archived = match store.archive_session(&current, utc_now()) {
        Ok(entry) => entry,
        Err(error) => {
            conn.borrow_mut().respond_error(
                id,
                ErrorObject::new(
                    error_code::INVALID_REQUEST,
                    format!("归档会话失败：{}", error.message()),
                ),
            );
            return;
        }
    };
    // 归档完成后再开新会话：新开的会话成为当前会话，归档的那条从默认列表隐藏。
    let started = conn
        .borrow_mut()
        .session
        .as_mut()
        .map(|session| session.start_new());
    let mut result = json!({ "session": archived.to_dict() });
    match started {
        Some(Ok(new_session_id)) => {
            result["new_session_id"] = json!(new_session_id);
        }
        Some(Err(detail)) => {
            eprintln!("[kernel] 归档后新建会话失败：{detail}");
        }
        None => {}
    }
    conn.borrow_mut().respond(id, result);
}

/// `session.new`：清空当前对话并开启新会话。
fn respond_session_new(conn: &Rc<RefCell<Conn>>, id: Id) {
    let started = conn
        .borrow_mut()
        .session
        .as_mut()
        .map(|session| session.start_new());
    match started {
        None => respond_no_session(conn, id),
        Some(Ok(session_id)) => conn
            .borrow_mut()
            .respond(id, json!({ "session_id": session_id })),
        Some(Err(detail)) => conn.borrow_mut().respond_error(
            id,
            ErrorObject::new(
                error_code::INTERNAL_ERROR,
                format!("新建会话失败：{detail}"),
            ),
        ),
    }
}

/// `session.resume`：切到指定会话，用转录重建历史；归档会话先解除归档。
fn respond_session_resume(conn: &Rc<RefCell<Conn>>, id: Id, params: &SessionResumeParams) {
    let Some((store, current)) = session_handle(&conn.borrow()) else {
        respond_no_session(conn, id);
        return;
    };
    let target = params.session_id.trim();
    if target.is_empty() {
        conn.borrow_mut().respond_error(
            id,
            ErrorObject::new(
                error_code::INVALID_PARAMS,
                "session.resume 需要 session_id。",
            ),
        );
        return;
    }
    // 先证明目标会话有效，再做任何切换动作。
    let entry = match store.list_sessions() {
        Ok(entries) => entries.into_iter().find(|entry| entry.session_id == target),
        Err(error) => {
            conn.borrow_mut().respond_error(
                id,
                ErrorObject::new(
                    error_code::INTERNAL_ERROR,
                    format!("会话列表读取失败：{}", error.message()),
                ),
            );
            return;
        }
    };
    let Some(entry) = entry else {
        conn.borrow_mut().respond_error(
            id,
            ErrorObject::new(error_code::INVALID_REQUEST, format!("未找到会话：{target}")),
        );
        return;
    };
    // 归档会话恢复后还要继续写入，先解除归档（失败不阻断切换）。
    if entry.archived_at.is_some() {
        if let Err(error) = store.unarchive_session(target, utc_now()) {
            conn.borrow_mut().respond_error(
                id,
                ErrorObject::new(
                    error_code::INVALID_REQUEST,
                    format!("恢复归档会话失败：{}", error.message()),
                ),
            );
            return;
        }
    }
    // 切换走之前丢掉当前的空占位会话（切回同一会话时不动）。
    if current != target {
        let _ = store.discard_empty_session(&current);
    }
    let reopened = conn
        .borrow_mut()
        .session
        .as_mut()
        .map(|session| session.reopen(target));
    match reopened {
        Some(Ok(())) => {
            let history = conn
                .borrow()
                .session
                .as_ref()
                .map(|session| session.history.clone())
                .unwrap_or_default();
            // 回包带上索引条目与历史：宿主据此展示标题/消息数并重放对话视图。
            conn.borrow_mut().respond(
                id,
                json!({
                    "session_id": target,
                    "session": entry.to_dict(),
                    "history": history,
                }),
            );
        }
        Some(Err(detail)) => conn.borrow_mut().respond_error(
            id,
            ErrorObject::new(
                error_code::INTERNAL_ERROR,
                format!("恢复会话失败：{detail}"),
            ),
        ),
        None => respond_no_session(conn, id),
    }
}

/// `subagent.run`：派生单个子 Agent 并等它跑完，回子 Agent 的**原始输出文本**。
///
/// 与 `subagent` 工具的 `action=run` 同一条执行路径（定义发现、工具白名单、worktree
/// 隔离都一致），区别只在于：本方法是宿主显式请求的单个任务，结果不回公开摘要，
/// 而是把子 Agent 的收尾文本原样交给宿主（`/review` 要自己解析 JSON 报告）。
fn respond_subagent_run(conn: &Rc<RefCell<Conn>>, id: Id, params: &SubagentRunParams) {
    let agent_type = params.agent_type.trim();
    if agent_type.is_empty() {
        conn.borrow_mut().respond_error(
            id,
            ErrorObject::new(error_code::INVALID_PARAMS, "subagent.run 需要 agent_type。"),
        );
        return;
    }
    if params.prompt.trim().is_empty() {
        conn.borrow_mut().respond_error(
            id,
            ErrorObject::new(error_code::INVALID_PARAMS, "subagent.run 需要 prompt。"),
        );
        return;
    }
    match run_subagent_request(conn, agent_type, params.description.trim(), &params.prompt) {
        Ok((output, task)) => {
            conn.borrow_mut().respond(
                id,
                json!({
                    "agent_type": agent_type,
                    "output": output,
                    "task": task,
                }),
            );
        }
        Err((code, message)) => {
            conn.borrow_mut().respond_error(
                id,
                ErrorObject::with_data(
                    error_code::INVALID_REQUEST,
                    message,
                    json!({ "kind": code }),
                ),
            );
        }
    }
}

/// `session.append`：把宿主产生的文本作为 assistant 消息追加到内核会话历史。
///
/// 对映 Python 的 `remember_review_report`：`/review` 在模型循环之外跑，报告默认只作界面
/// 消息；注入后下一轮请求才会带上它。只允许 assistant 角色——不借这个入口伪造用户输入
/// 或工具结果。空内容不注入（与 Python 的 `remember_review_report` 同口径）。
/// `workspace.switch` 的统一应答：校验 path → 会话侧切换 → 回包。
///
/// 内核只做会话一致性（把自持会话的工作区指向新根 + 转录 `workspace_switched`）；
/// 工具表、MCP、临时目录等宿主侧资源由宿主自己重建，协议不回传。空闲路径与
/// 回合内路径（`handle_inbound`）共用这一份，避免两条路径的文案与校验分叉。
fn handle_workspace_switch(conn: &mut Conn, id: Id, params: &WorkspaceSwitchParams) {
    let path = params.path.trim().to_string();
    if path.is_empty() {
        conn.respond_error(
            id,
            ErrorObject::new(error_code::INVALID_PARAMS, "workspace.switch 需要非空 path。"),
        );
        return;
    }
    match conn.switch_workspace(&path) {
        // 没有自持会话时与 `session.append` 同一口径。
        Ok(None) => conn.respond_error(id, no_session_error()),
        Ok(Some((from, to))) => conn.respond(
            id,
            json!({ "switched": from != to, "from": from, "to": to }),
        ),
        Err(detail) => {
            eprintln!("[kernel] 转录 workspace_switched 失败：{detail}");
            conn.respond_error(
                id,
                ErrorObject::new(
                    error_code::INVALID_REQUEST,
                    format!("工作区切换记录失败：{detail}"),
                ),
            );
        }
    }
}

fn respond_session_append(conn: &Rc<RefCell<Conn>>, id: Id, params: &SessionAppendParams) {
    let role = params.role.trim().to_lowercase();
    if role != "assistant" {
        conn.borrow_mut().respond_error(
            id,
            ErrorObject::with_data(
                error_code::INVALID_PARAMS,
                "session.append 只支持 role=assistant。",
                json!({ "kind": "invalid_settings" }),
            ),
        );
        return;
    }
    let content = params.content.trim();
    if content.is_empty() {
        conn.borrow_mut().respond(id, json!({ "appended": false }));
        return;
    }
    let mut borrowed = conn.borrow_mut();
    let Some(session) = borrowed.session.as_mut() else {
        drop(borrowed);
        respond_no_session(conn, id);
        return;
    };
    let message = json!({"role": "assistant", "content": content});
    // 先落盘再进运行期历史：读取端（本次响应）看到的与下一轮请求带上的一致。
    let appended = match session.append("assistant_message", json!({ "content": content })) {
        Ok(()) => true,
        Err(detail) => {
            eprintln!("[kernel] 追加 assistant 消息失败：{detail}");
            false
        }
    };
    if appended {
        session.history.push(message);
    }
    drop(borrowed);
    conn.borrow_mut()
        .respond(id, json!({ "appended": appended }));
}

/// 派生单个子 Agent 并等它结束：返回（子 Agent 收尾文本，公开结果）。
///
/// 工具批次仍要回到宿主执行，所以本函数在等待期间必须继续服务后台请求
/// （与 `run_scheduled_tasks` 同一条 `drain_background` 泵）。宿主侧因此不能同步
/// 等待本方法的响应，必须保持事件循环可响应。
fn run_subagent_request(
    conn: &Rc<RefCell<Conn>>,
    agent_type: &str,
    description: &str,
    prompt: &str,
) -> Result<(String, Value), (String, String)> {
    let runtime =
        SubAgentRuntime::load().map_err(|detail| ("SUBAGENT_DISABLED".to_string(), detail))?;
    if !runtime.enabled() {
        return Err((
            "SUBAGENT_DISABLED".to_string(),
            "SubAgent 功能未启用。请在配置中显式设置 subagents.enabled=true。".to_string(),
        ));
    }
    let (model_config, fork_messages) = {
        let borrowed = conn.borrow();
        let Some(model) = borrowed.model.clone() else {
            return Err((
                "SUBAGENT_MODEL_ERROR".to_string(),
                "内核未持有模型配置，无法执行子任务。".to_string(),
            ));
        };
        let history = borrowed
            .session
            .as_ref()
            .map(|session| session.history.clone())
            .unwrap_or_default();
        (model, history)
    };
    let parent_tools = SubAgentRuntime::parent_tool_names(&model_config);
    let batch_id = next_subagent_batch_id();
    // 复用 `subagent` 工具的入参形状：校验、工具白名单与错误文案因此完全同源。
    let arguments: Map<String, Value> = json!({
        "action": "run",
        "tasks": [{
            "subagent_type": agent_type,
            "description": description,
            "prompt": prompt,
        }],
    })
    .as_object()
    .cloned()
    .unwrap_or_default();
    let mut tasks = runtime.prepare(&arguments, &parent_tools, &batch_id)?;
    let task = tasks.pop().ok_or_else(|| {
        (
            "SUBAGENT_LIMIT_EXCEEDED".to_string(),
            "subagent.run 没有可执行的任务。".to_string(),
        )
    })?;

    let runtime = Arc::new(runtime);
    let sender = conn.borrow().background.clone();
    let cancel = CancelToken::new();
    let workspace = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let (tx, rx) = mpsc::channel::<Value>();
    {
        let tx = tx.clone();
        thread::spawn(move || {
            let result = run_task_in_isolation(
                &runtime,
                &model_config,
                &task,
                &fork_messages,
                &sender,
                &cancel,
                &workspace,
                // 宿主入口要完整报告：截断摘要会把评审 JSON 打碎。
                true,
            );
            let _ = tx.send(result);
        });
    }
    let result = loop {
        match rx.recv_timeout(BACKGROUND_TICK) {
            Ok(value) => break value,
            Err(mpsc::RecvTimeoutError::Timeout) => drain_background(conn),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err((
                    "SUBAGENT_MODEL_ERROR".to_string(),
                    "子任务线程提前结束，没有可用结果。".to_string(),
                ))
            }
        }
    };
    // 结果投影复用协调器的同一条路径：失败描述、全文优先与脱敏在这里一次做对。
    let (ok, summary) = batch_summary(&batch_id, vec![result.clone()]);
    let encoded = json_result_text(&summary);
    let output = require_completed_result(ok, &encoded)
        .map_err(|error| (subagent_failure_code(&summary), error.message().to_string()))?;
    Ok((output, result))
}

/// 失败结果里的可判定代码（`data.kind` 用它分支）；缺失时给一个通用值。
fn subagent_failure_code(summary: &Value) -> String {
    summary
        .pointer("/results/0/error/code")
        .or_else(|| summary.pointer("/error/code"))
        .and_then(Value::as_str)
        .unwrap_or("SUBAGENT_TASK_FAILED")
        .to_string()
}

fn protocol_version_of(params: Option<&Value>) -> String {
    params
        .and_then(|params| params.get("protocol_version"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn spawn_reader(sender: Sender<Inbound>) {
    thread::spawn(move || {
        let stdin = std::io::stdin();
        let mut reader = BufReader::new(stdin.lock());
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) => {
                    if line.trim().is_empty() {
                        continue;
                    }
                    match Frame::parse(&line) {
                        Ok(frame) => {
                            if sender.send(Inbound::Frame(Box::new(frame))).is_err() {
                                break;
                            }
                        }
                        // 无法解析的行不回 -32700：id 不可信时响应也无处可去，丢弃并记日志。
                        Err(error) => eprintln!("[kernel] 丢弃无法解析的行：{error}"),
                    }
                }
                Err(error) => {
                    eprintln!("[kernel] 读取 stdin 失败：{error}");
                    break;
                }
            }
        }
        let _ = sender.send(Inbound::Closed);
    });
}

/// 在 stdio 上跑完一个会话，直到 EOF、`shutdown` 或读取失败。
pub fn run_stdio() -> Result<(), String> {
    let (sender, receiver) = mpsc::channel::<Inbound>();
    spawn_reader(sender);
    let (background_sender, background_receiver) = mpsc::channel::<BackgroundRequest>();

    let conn = Rc::new(RefCell::new(Conn {
        inbound: receiver,
        out: Box::new(std::io::stdout()),
        cancel: Arc::new(AtomicBool::new(false)),
        handshaken: false,
        exit_requested: false,
        model: None,
        session: None,
        plugin_model_hooks: false,
        usage: Rc::new(RefCell::new(TurnUsage::default())),
        background: background_sender,
        background_receiver,
        tasks: None,
        next_id: 0,
    }));

    eprintln!(
        "[kernel] omnicrawl {} 就绪（协议 v1，等待 initialize）。",
        env!("CARGO_PKG_VERSION")
    );

    loop {
        // 后台任务在回合外借用连接：先把积压的请求处理掉，再等宿主的下一条帧。
        drain_background(&conn);
        // 借用必须在本条语句内结束：dispatch 里还要可变借用同一条连接。
        let inbound = conn.borrow().inbound.recv_timeout(BACKGROUND_TICK);
        match inbound {
            Ok(Inbound::Frame(frame)) => {
                if dispatch(&conn, *frame) {
                    break;
                }
            }
            Ok(Inbound::Closed) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    Ok(())
}

/// 把积压的后台借用请求处理掉：工具批次转给宿主，事件直接外发。
///
/// 跑子任务的线程在等结果时也会调用它——否则子任务的工具批次无人可发，会僵住。
fn drain_background(conn: &Rc<RefCell<Conn>>) {
    loop {
        let request = match conn.borrow().background_receiver.try_recv() {
            Ok(request) => request,
            Err(_) => return,
        };
        serve_background(conn, request);
    }
}

/// 回合进行中（内核自己发模型请求）抽干入站帧：让 `turn.cancel` / `shutdown` 立刻生效。
///
/// 返回 `true` 表示应当中止当前请求（已取消、已请求退出或宿主已断开）。其余帧交给
/// [`Conn::handle_inbound`] 正常应答（重复 `turn.submit` 仍会收到 `-32002`）。
fn drain_inbound_during_turn(conn: &Rc<RefCell<Conn>>) -> bool {
    loop {
        let frame = match conn.borrow().inbound.try_recv() {
            Ok(Inbound::Frame(frame)) => frame,
            Ok(Inbound::Closed) => return true,
            Err(mpsc::TryRecvError::Empty) => break,
            Err(mpsc::TryRecvError::Disconnected) => return true,
        };
        let failure = conn.borrow_mut().handle_inbound(&frame);
        if matches!(
            failure,
            Some(PortFailure::Cancelled(_)) | Some(PortFailure::Shutdown)
        ) {
            return true;
        }
        let connection = conn.borrow();
        if connection.cancel.load(Ordering::SeqCst) || connection.exit_requested {
            return true;
        }
    }
    let connection = conn.borrow();
    connection.cancel.load(Ordering::SeqCst) || connection.exit_requested
}

/// 主循环代子代理跑 `model.request.before`：宿主声明支持时才发请求。
///
/// 与主端口同口径：宿主未声明能力（或插件未启用）时原样放行；拒绝时把宿主给的文案带回去，
/// 由子代理端口折算成中止本轮的错误。
fn serve_model_hook(
    conn: &Rc<RefCell<Conn>>,
    messages: Vec<Value>,
    model: String,
) -> Result<Vec<Value>, String> {
    if !conn.borrow().plugin_model_hooks {
        return Ok(messages);
    }
    let request = ModelHookRequest { messages, model };
    let params =
        serde_json::to_value(&request).expect("model.hook 负载是 Value 字段，必须可序列化");
    let value = conn
        .borrow_mut()
        .request(method::MODEL_HOOK, params)
        .map_err(|failure| match failure {
            PortFailure::Cancelled(message) => message,
            PortFailure::Shutdown => "收到 shutdown，回合中止。".to_string(),
            PortFailure::Remote(error) => error.message,
            PortFailure::Disconnected => "宿主连接已关闭。".to_string(),
            PortFailure::Io(detail) => format!("写请求失败：{detail}"),
        })?;
    ModelHookResult::from_result(&value)
        .map(|result| result.messages)
        .map_err(|error| format!("宿主返回的 model.hook 结果无法解析：{error}"))
}

/// 主循环代后台任务借用一次连接：工具批次转给宿主，事件直接外发。
fn serve_background(conn: &Rc<RefCell<Conn>>, request: BackgroundRequest) {
    match request {
        BackgroundRequest::Notify(event) => conn.borrow_mut().notify(*event),
        BackgroundRequest::ModelHook {
            messages,
            model,
            reply,
        } => {
            let _ = reply.send(serve_model_hook(conn, messages, model));
        }
        BackgroundRequest::ToolBatch {
            turn_id,
            calls,
            workspace_root,
            reply,
        } => {
            // 后台子任务批次也走压缩旁路：模型连接取当前进程的协议配置。
            let compressor = conn
                .borrow()
                .model
                .clone()
                .and_then(|config| KernelCompressor::load(&config))
                .map(Rc::new);
            let mut tools = RemoteTools {
                conn: Rc::clone(conn),
                turn_id,
                workspace_root,
                compressor,
                vision_model: conn.borrow().model.clone(),
                undo: None,
                // 后台子任务批次没有自己的执行清单（run_guard 只看主回合）。
                todos: None,
                task_hint: String::new(),
                // 子代理的工具调用不落父会话：Python 在子代理循环里传
                // `persist_session_events=False`。
                active_assistant: None,
            };
            let outcome = tools
                .execute_tool_batch(&calls, 0)
                .map_err(|error| error.tag().to_string());
            let _ = reply.send(outcome);
        }
    }
}

/// 后台任务的工具端口：只把批次递给主循环，自己不碰连接。
struct BackgroundTools {
    sender: mpsc::Sender<BackgroundRequest>,
    turn_id: String,
    /// 隔离根：worktree 子任务的工具批次要在自己的工作树里执行。
    workspace_root: Option<String>,
    /// 子代理对话流：批次号、角色名（与 `subagent.tool.*` 事件同一身份）。
    batch_id: String,
    agent_type: String,
    /// 是否流式上报子代理对话（只有 `/review` 这类宿主入口开启）。
    stream: bool,
}

impl BackgroundTools {
    /// 子代理工具事件的身份字段（与 Python 的 `subagent.tool.*` 载荷同形）。
    fn conversation_payload(&self, tool: &str) -> Value {
        json!({
            "task_id": self.turn_id,
            "batch_id": self.batch_id,
            "agent_type": self.agent_type,
            "tool": tool,
        })
    }
}

impl ToolBatchHost for BackgroundTools {
    fn execute_tool_batch(
        &mut self,
        calls: &[ToolCall],
        _first_step: usize,
    ) -> Result<Vec<AgentLoopObservation>, LoopError> {
        // 开始事件先发：宿主据此把工具卡挂进子代理会话面板。
        if self.stream {
            for call in calls {
                let mut payload = self.conversation_payload(&call.name);
                if let Some(object) = payload.as_object_mut() {
                    object.insert(
                        "arguments".to_string(),
                        public_tool_arguments(&call.name, &call.arguments),
                    );
                }
                emit_background_event(&self.sender, "subagent.tool.started", payload);
            }
        }
        let started = std::time::Instant::now();
        let (reply, receiver) = mpsc::sync_channel(1);
        self.sender
            .send(BackgroundRequest::ToolBatch {
                turn_id: self.turn_id.clone(),
                calls: calls.to_vec(),
                workspace_root: self.workspace_root.clone(),
                reply,
            })
            .map_err(|_| {
                LoopError::ToolBatch("内核主循环已停止，后台任务无法继续。".to_string())
            })?;
        match receiver.recv() {
            Ok(Ok(observations)) => {
                let elapsed = started.elapsed().as_secs_f64();
                if self.stream {
                    for observation in &observations {
                        let mut payload = self.conversation_payload(&observation.tool_call.name);
                        if let Some(object) = payload.as_object_mut() {
                            let output = if observation.result.full_output.is_empty() {
                                observation.result.output.clone()
                            } else {
                                observation.result.full_output.clone()
                            };
                            object.insert("ok".to_string(), Value::Bool(observation.result.ok));
                            object.insert("output".to_string(), Value::String(output));
                            object.insert("duration_seconds".to_string(), json!(elapsed));
                        }
                        emit_background_event(&self.sender, "subagent.tool.completed", payload);
                    }
                }
                Ok(observations)
            }
            Ok(Err(detail)) => Err(LoopError::ToolBatch(detail)),
            Err(_) => Err(LoopError::ToolBatch(
                "后台工具批次没有拿到结果。".to_string(),
            )),
        }
    }
}

/// 后台任务的模型端口：直接发请求，流事件经通道转给主循环。
struct BackgroundModelPort {
    config: KernelModelConfig,
    sender: mpsc::Sender<BackgroundRequest>,
    cancel: CancelToken,
    /// 子代理对话流：任务号、批次号与角色名。
    task_id: String,
    batch_id: String,
    agent_type: String,
    /// 是否流式上报子代理对话（与工具端口同一开关）。
    stream: bool,
}

impl ReplySource for BackgroundModelPort {
    fn request_reply(&mut self, messages: &mut Vec<Value>) -> Result<AgentModelReply, LoopError> {
        // 子代理的模型请求同样先过 `model.request.before`（由主循环代跑）。
        self.run_request_hook(messages)?;
        let runtime = build_model_runtime(&self.config)?;
        let options = parse_options(&self.config)?;
        // 与主回合同一套线上名收敛（子 Agent 也能拿到 MCP 工具）。
        let (tools, name_map) = conform_tool_names(&parse_tools(&self.config));
        let conversation = conversation_from_openai_messages(messages);
        let identity = self.config.prompt_cache_identity.clone();
        let input = ChatRequestInput {
            model: &self.config.model,
            system_prompt: &self.config.system_prompt,
            messages: &conversation,
            tools: &tools,
            options: &options,
            profile_request_timeout_seconds: self
                .config
                .request_timeout_seconds
                .filter(|seconds| *seconds > 0.0)
                .unwrap_or_default(),
            prompt_cache_capable: self.config.prompt_cache_capable,
            prompt_cache_identity: &identity,
        };
        let mut sink = BackgroundSink {
            sender: self.sender.clone(),
            cancel: self.cancel.clone(),
        };
        match runtime.run_turn(&input, &mut sink) {
            Ok(reply) => {
                let mut reply = reply;
                name_map.restore_reply(&mut reply);
                let reply = to_agent_reply(reply)?;
                // 与 Python 同规则：只有带工具调用的过程性文本进子代理会话面板。
                if self.stream && !reply.content.is_empty() && !reply.tool_calls.is_empty() {
                    emit_background_event(
                        &self.sender,
                        "subagent.turn.text",
                        json!({
                            "task_id": self.task_id,
                            "batch_id": self.batch_id,
                            "agent_type": self.agent_type,
                            "text": reply.content,
                        }),
                    );
                }
                self.notify_model_response(&reply);
                Ok(reply)
            }
            Err(error) => {
                if error.kind == RuntimeErrorKind::Cancelled {
                    return Err(LoopError::Cancelled(error.message));
                }
                self.notify_model_error(&error.message);
                Err(LoopError::ReplySource(error.message))
            }
        }
    }
}

impl BackgroundModelPort {
    /// 请主循环代跑 `model.request.before`，并把（可能被改写的）消息取回。
    fn run_request_hook(&self, messages: &mut Vec<Value>) -> Result<(), LoopError> {
        let (reply, receiver) = mpsc::sync_channel(1);
        self.sender
            .send(BackgroundRequest::ModelHook {
                messages: messages.clone(),
                model: self.config.model.clone(),
                reply,
            })
            .map_err(|_| {
                LoopError::ReplySource("内核主循环已停止，子代理无法继续。".to_string())
            })?;
        match receiver.recv() {
            Ok(Ok(rewritten)) => {
                *messages = rewritten;
                Ok(())
            }
            Ok(Err(detail)) => {
                if self.cancel.is_set() {
                    Err(LoopError::Cancelled(detail))
                } else {
                    Err(LoopError::ReplySource(detail))
                }
            }
            Err(_) => Err(LoopError::ReplySource(
                "子代理的模型 Hook 没有拿到结果。".to_string(),
            )),
        }
    }

    /// `model.request.error`：一次子代理模型请求以错误终结时的通知。
    fn notify_model_error(&self, message: &str) {
        let _ = self.sender.send(BackgroundRequest::Notify(Box::new(
            HostEvent::ModelRequestError(ModelRequestErrorPayload {
                error: message.to_string(),
                model: self.config.model.clone(),
            }),
        )));
    }

    /// `model.response.after`：一次子代理模型请求成功返回后的观察通知。
    fn notify_model_response(&self, reply: &AgentModelReply) {
        let _ = self.sender.send(BackgroundRequest::Notify(Box::new(
            HostEvent::ModelResponseAfter(ModelResponseAfterPayload {
                model: self.config.model.clone(),
                content: reply.content.clone(),
                tool_call_count: reply.tool_calls.len(),
            }),
        )));
    }
}

/// 后台任务的流事件出口：文本增量与状态提示转给主循环，其余丢弃。
struct BackgroundSink {
    sender: mpsc::Sender<BackgroundRequest>,
    cancel: CancelToken,
}

impl TurnSink for BackgroundSink {
    fn on_event(&mut self, event: ModelStreamEvent) -> SinkFlow {
        let forwarded = match event {
            ModelStreamEvent::TextDelta(delta) => {
                Some(HostEvent::Delta(TextPayload { text: delta.text }))
            }
            ModelStreamEvent::ReasoningDelta(delta) => {
                Some(HostEvent::ReasoningDelta(TextPayload { text: delta.text }))
            }
            ModelStreamEvent::ProviderWarning(warning) => Some(HostEvent::Status(MessagePayload {
                message: warning.message,
            })),
            _ => None,
        };
        if let Some(event) = forwarded {
            let _ = self.sender.send(BackgroundRequest::Notify(Box::new(event)));
        }
        if self.cancel.is_set() {
            SinkFlow::Cancel
        } else {
            SinkFlow::Continue
        }
    }

    fn cancelled(&self) -> bool {
        self.cancel.is_set()
    }
}

#[cfg(test)]
mod runtime_selection_tests {
    use super::*;

    fn config(provider: &str, protocol: &str) -> KernelModelConfig {
        KernelModelConfig {
            model: "m".to_string(),
            provider: provider.to_string(),
            protocol: protocol.to_string(),
            base_url: String::new(),
            api_key_env: String::new(),
            user_agent: String::new(),
            system_prompt: String::new(),
            context_messages: Vec::new(),
            tools: Vec::new(),
            options: Value::Null,
            request_timeout_seconds: None,
            context_window_tokens: 0,
            prompt_cache_capable: false,
            prompt_cache_identity: std::collections::BTreeMap::new(),
            native_vision: false,
            request_retry_count: 1,
        }
    }

    /// 缺凭据在**构造运行期**时就失败（与 Python 的 `create_*_client` 同），所以回退验证
    /// 改用显式凭据的入口；同时钉住「空 id 不会造出 `Profile  缺少…` 双空格文案」。
    #[test]
    fn empty_provider_falls_back_to_openai() {
        assert!(build_model_runtime_with_key(&config("", ""), "k".to_string()).is_ok());
        assert!(build_model_runtime_with_key(&config("gemini", ""), "k".to_string()).is_ok());
        let error = build_model_runtime(&config("", ""))
            .err()
            .expect("缺凭据必须失败");
        match error {
            LoopError::ReplySource(message) => assert_eq!(
                message,
                "模型 Profile 缺少 API Key。请配置 api_key_env 环境变量或 profile.api_key。",
            ),
            other => panic!("意外的错误类型：{other:?}"),
        }
    }

    #[test]
    fn protocol_must_match_provider() {
        let error = build_model_runtime(&config("gemini", "anthropic_messages"))
            .err()
            .expect("协议与 Provider 不匹配必须失败");
        match error {
            LoopError::ReplySource(message) => {
                assert!(
                    message.contains("anthropic_messages"),
                    "报错文案：{message}"
                );
            }
            other => panic!("应为 ReplySource，实际 {other:?}"),
        }
    }
}

#[cfg(test)]
mod session_tests {
    use super::cancelled_turn_summary;

    #[test]
    fn cancelled_summary_counts_executed_tools() {
        assert_eq!(
            cancelled_turn_summary(&[]),
            "（上一回合被取消，未生成最终回复，未执行任何工具）"
        );
        assert_eq!(
            cancelled_turn_summary(&["read".to_string()]),
            "（上一回合被取消，未生成最终回复）已执行工具：read"
        );
        // 同一工具多次：Python 用 `name×次数`；不同工具之间用「，」连接。
        assert_eq!(
            cancelled_turn_summary(&[
                "read".to_string(),
                "bash".to_string(),
                "read".to_string(),
            ]),
            "（上一回合被取消，未生成最终回复）已执行工具：read×2，bash"
        );
        assert_eq!(
            cancelled_turn_summary(&["  ".to_string()]),
            "（上一回合被取消，未生成最终回复，未执行任何工具）"
        );
    }
}
