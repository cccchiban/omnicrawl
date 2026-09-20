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

use omnicrawl_controllers::context_compaction::TokenUsageSample;
use omnicrawl_controllers::context_compaction::RECALL_SESSION_EVIDENCE_TOOL_NAME;
use omnicrawl_controllers::shared::CONTEXT_OVERFLOW_ERROR_MARKERS;
use omnicrawl_core::{
    AgentLoopLimits, AgentLoopObservation, AgentLoopRunner, AgentModelReply, LoopError, LoopGuards,
    ReplySource, SystemClock, ToolBatchHost, ToolCall, ToolResult,
};
use omnicrawl_ipc::bridge::{
    initialize_result, method, unsupported_version_error, BridgeError, Command, HostEvent,
    InitializeParams, KernelModelConfig, KernelSessionConfig, MessagePayload, ModelRequest,
    SubagentEventPayload, TextPayload, TokenUsagePayload, ToolBatch, ToolBatchResult,
    TurnCancelParams, TurnFinishedPayload, TurnSubmitParams,
};
use omnicrawl_ipc::frame::{error_code, ErrorObject, Frame, Id};
use omnicrawl_ipc::version::negotiate_version;
use omnicrawl_llm::{
    build_runtime, to_openai_messages, ChatRequestInput, ModelDescriptor, ModelRuntime,
    ProviderProfile, RuntimeErrorKind, SinkFlow, TurnSink, CONTEXT_LENGTH_EXCEEDED_MESSAGE,
};
use omnicrawl_protocol::{
    conversation_from_openai_messages, tool_spec_from_openai_item, GenerationOptions, ModelReply,
    ModelStreamEvent, ToolSpec,
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
use omnicrawl_controllers::subagents::tasks::{
    CancelToken, SubAgentTaskManager, SubAgentTaskSpec, TaskObserver, TaskRunner,
};
use omnicrawl_controllers::subagents::worktrees::{
    artifact_lines, discard_guard, WorktreeArtifacts,
};
use omnicrawl_session::tool_result_message;

use crate::compression::KernelCompressor;
use crate::subagent::{PreparedTask, SubAgentExecution, SubAgentRuntime};

use crate::compaction::{
    compact_after_turn, recall_session_evidence, recover_after_overflow, KernelSession,
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

    /// 完成握手：版本不匹配回 `-32001`。
    fn handshake(&mut self, id: Id, host_version: &str) {
        match negotiate_version(host_version) {
            Ok(_) => {
                self.handshaken = true;
                self.respond(id, initialize_result());
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
            (Some(id), method::SHUTDOWN) => {
                self.cancel.store(true, Ordering::SeqCst);
                self.exit_requested = true;
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
        serde_json::from_value(value)
            .map_err(|error| LoopError::ReplySource(format!("宿主返回的模型回复无法解析：{error}")))
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
}

impl ReplySource for KernelModelPort {
    fn request_reply(&mut self, messages: &mut Vec<Value>) -> Result<AgentModelReply, LoopError> {
        {
            // 摘要请求要逐字复用最近一次主请求：这里记下它真正发出的消息。
            let mut usage = self.usage.borrow_mut();
            usage.last_request_messages = messages.clone();
            usage.last_request_input_tokens = 0;
        }
        let runtime = self.runtime()?;
        let options = parse_options(&self.config)?;
        let tools = parse_tools(&self.config);
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
                        return Err(LoopError::ReplySource(format!(
                            "Agent 连续 {limit} 次返回空响应，已停止本轮请求。"
                        )));
                    }
                    return to_agent_reply(reply);
                }
                Err(error) => {
                    if error.kind == RuntimeErrorKind::Cancelled {
                        return Err(LoopError::Cancelled(error.message));
                    }
                    if error.message.contains(CONTEXT_LENGTH_EXCEEDED_MESSAGE) {
                        self.usage.borrow_mut().context_overflow = true;
                    }
                    attempt += 1;
                    if error.retryable && attempt < limit {
                        // 已经推给界面的半截流要先撤销，否则重试会叠在旧文本上。
                        if error.kind == RuntimeErrorKind::StreamInterrupted {
                            self.conn.borrow_mut().notify(HostEvent::StreamRollback);
                        }
                        self.notify_retry(format!("请求失败，正在自动重试（第{attempt}次）"));
                        continue;
                    }
                    if error.retryable {
                        return Err(LoopError::ReplySource(format!(
                            "上游错误已达到本回合自动重试上限，已停止本次请求。{}",
                            error.message
                        )));
                    }
                    return Err(LoopError::ReplySource(error.message));
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
        let mut connection = self.conn.borrow_mut();
        match event {
            ModelStreamEvent::TextDelta(delta) => {
                connection.notify(HostEvent::Delta(TextPayload { text: delta.text }));
            }
            ModelStreamEvent::ReasoningDelta(delta) => {
                connection.notify(HostEvent::ReasoningDelta(TextPayload { text: delta.text }));
            }
            ModelStreamEvent::UsageReported(usage) => {
                if let Ok(input) = i64::try_from(usage.input_tokens) {
                    let mut totals = self.usage.borrow_mut();
                    totals.last_request_input_tokens = input;
                }
                if let (Ok(input), Ok(output), Ok(cached)) = (
                    i64::try_from(usage.input_tokens),
                    i64::try_from(usage.output_tokens),
                    i64::try_from(usage.cached_input_tokens),
                ) {
                    let mut totals = self.usage.borrow_mut();
                    totals.input_tokens += input;
                    totals.output_tokens += output;
                    totals.cached_input_tokens += cached;
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
    Ok(bundle.runtime)
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
    /// 本轮任务文本，作为压缩请求的任务背景。
    task_hint: String,
}

impl ToolBatchHost for RemoteTools {
    fn execute_tool_batch(
        &mut self,
        calls: &[ToolCall],
        first_step: usize,
    ) -> Result<Vec<AgentLoopObservation>, LoopError> {
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

        // 超长工具观察交给压缩旁路：未启用、失败或没压小都保留原文。
        if let Some(compressor) = &self.compressor {
            let cancel_source = Rc::clone(&self.conn);
            let cancelled = || cancel_source.borrow().cancel.load(Ordering::SeqCst);
            if !cancelled() {
                compressor.apply_observations(&mut ordered, &self.task_hint, &cancelled);
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
fn run_background_task(
    runtime: &SubAgentRuntime,
    model_config: &KernelModelConfig,
    task: &PreparedTask,
    fork_messages: &[Value],
    sender: &mpsc::Sender<BackgroundRequest>,
    cancel: &CancelToken,
    workspace_root: Option<&str>,
) -> Result<SubAgentExecution, (String, String)> {
    let mut messages = runtime.task_messages(task, fork_messages);
    let mut model = BackgroundModelPort {
        config: runtime.child_model_config(model_config, task),
        sender: sender.clone(),
        cancel: cancel.clone(),
    };
    let mut tools = BackgroundTools {
        sender: sender.clone(),
        turn_id: task.task_id.clone(),
        workspace_root: workspace_root.map(str::to_string),
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
fn run_task_in_isolation(
    runtime: &SubAgentRuntime,
    model_config: &KernelModelConfig,
    task: &PreparedTask,
    fork_messages: &[Value],
    sender: &mpsc::Sender<BackgroundRequest>,
    cancel: &CancelToken,
    workspace: &std::path::Path,
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
    ) {
        Ok(execution) => runtime.completed_result(task, &execution),
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
    // 有会话就从转录恢复的运行期历史接着走；没有会话维持「一回合一条 user 消息」的旧行为。
    let mut messages = conn
        .borrow()
        .session
        .as_ref()
        .map(|session| session.history.clone())
        .unwrap_or_default();
    messages.push(json!({"role": "user", "content": user_text.clone()}));
    // 宿主给了模型配置就由内核自己发请求；没给则维持 model.reply 代答的兼容路径。
    let model_config = conn.borrow().model.clone();
    let mut model: Box<dyn ReplySource> = match model_config.clone() {
        Some(config) => Box::new(KernelModelPort {
            conn: Rc::clone(conn),
            config,
            usage: Rc::clone(&usage),
        }),
        None => Box::new(RemoteModelPort {
            conn: Rc::clone(conn),
            turn_id: turn_id.clone(),
        }),
    };
    let compressor = model_config
        .clone()
        .and_then(|config| KernelCompressor::load(&config))
        .map(Rc::new);
    let mut tools = RemoteTools {
        conn: Rc::clone(conn),
        turn_id: turn_id.clone(),
        workspace_root: None,
        compressor,
        task_hint: user_text.clone(),
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

    // 主 Agent 的预算是无限的：与 Python 侧一致，靠取消与停止检查来收敛。
    // 上下文超限时压缩当前未完成回合，用返回的投影重试一次（与 Python 的恢复路径同口径）。
    let mut recovered = false;
    let outcome = loop {
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
                        messages = projection;
                        continue;
                    }
                    None => break Err(LoopError::ReplySource(message)),
                }
            }
            other => break other,
        }
    };

    let mut connection = conn.borrow_mut();
    match outcome {
        Ok(result) => {
            // 先落盘再通知：宿主收到 turn.finished 时转录必须已经持久化，
            // 否则宿主此刻退出（或被强杀）就会丢掉这一轮的问答。
            let session = connection.session.take();
            drop(connection);
            if let Some(mut session) = session {
                run_session_tail(
                    conn,
                    &mut session,
                    &user_text,
                    &messages,
                    &result.final_text,
                );
                conn.borrow_mut().session = Some(session);
            }
            conn.borrow_mut()
                .notify(HostEvent::TurnFinished(TurnFinishedPayload {
                    turn_id: turn_id.clone(),
                    final_text: result.final_text.clone(),
                    reasoning: result.reasoning,
                    model_turns: result.model_turns,
                    tool_calls: result.tool_calls,
                    paused: result.paused,
                }));
            conn.borrow_mut().respond(request_id, json!({}));
        }
        Err(error) => {
            eprintln!("[kernel] 回合失败：{}", error.message());
            connection.respond_error(request_id, turn_error(&error))
        }
    }
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
    user_text: &str,
    working_messages: &[Value],
    final_text: &str,
) {
    if let Err(detail) = session.append("user_message", json!({"content": user_text})) {
        eprintln!("[kernel] 会话写入用户消息失败：{detail}");
    }
    let trimmed = final_text.trim();
    if !trimmed.is_empty() {
        if let Err(detail) = session.append("assistant_message", json!({"content": trimmed})) {
            eprintln!("[kernel] 会话写入助手回复失败：{detail}");
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
            ..
        }) => {
            if let Some(config) = model {
                conn.borrow_mut().model = Some(*config);
            }
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
        Command::Shutdown => {
            conn.borrow_mut().respond(id, json!({}));
            true
        }
    }
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

/// 主循环代后台任务借用一次连接：工具批次转给宿主，事件直接外发。
fn serve_background(conn: &Rc<RefCell<Conn>>, request: BackgroundRequest) {
    match request {
        BackgroundRequest::Notify(event) => conn.borrow_mut().notify(*event),
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
                task_hint: String::new(),
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
}

impl ToolBatchHost for BackgroundTools {
    fn execute_tool_batch(
        &mut self,
        calls: &[ToolCall],
        _first_step: usize,
    ) -> Result<Vec<AgentLoopObservation>, LoopError> {
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
            Ok(Ok(observations)) => Ok(observations),
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
}

impl ReplySource for BackgroundModelPort {
    fn request_reply(&mut self, messages: &mut Vec<Value>) -> Result<AgentModelReply, LoopError> {
        let runtime = build_model_runtime(&self.config)?;
        let options = parse_options(&self.config)?;
        let tools = parse_tools(&self.config);
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
            Ok(reply) => to_agent_reply(reply),
            Err(error) => {
                if error.kind == RuntimeErrorKind::Cancelled {
                    return Err(LoopError::Cancelled(error.message));
                }
                Err(LoopError::ReplySource(error.message))
            }
        }
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
            tools: Vec::new(),
            options: Value::Null,
            request_timeout_seconds: None,
            context_window_tokens: 0,
            prompt_cache_capable: false,
            prompt_cache_identity: std::collections::BTreeMap::new(),
            request_retry_count: 1,
        }
    }

    #[test]
    fn empty_provider_falls_back_to_openai() {
        assert!(build_model_runtime(&config("", "")).is_ok());
        assert!(build_model_runtime(&config("gemini", "")).is_ok());
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
