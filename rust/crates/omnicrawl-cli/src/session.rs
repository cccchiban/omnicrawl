//! 会话：在一条 NDJSON 流上承载协议 v1。
//!
//! 内核自己拥有回合循环（`omnicrawl-core`），把两个宿主端口经协议外发：`model.reply` 代模型回复、
//! `tool.batch` 代工具批次。宿主端口是过渡形态——内核自带 provider runtime 后只需换掉
//! [宿主端口实现] 一个实现，协议与循环都不动。
//!
//! 一个回合只跑一个（第二个 `turn.submit` 回 `-32002`）；回合进行中到达的 `turn.cancel` / `shutdown`
//! 会立即中止当前端口调用，其余请求照常应答，不阻塞宿主。

use std::cell::RefCell;
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
    TextPayload, TokenUsagePayload, ToolBatch, ToolBatchResult, TurnCancelParams,
    TurnFinishedPayload, TurnSubmitParams,
};
use omnicrawl_ipc::frame::{error_code, ErrorObject, Frame, Id};
use omnicrawl_ipc::version::negotiate_version;
use omnicrawl_llm::{
    to_openai_messages, ChatEndpoint, ChatRequestInput, ModelRuntime, OpenAiChatRuntime,
    RuntimeErrorKind, SinkFlow, TurnSink, CONTEXT_LENGTH_EXCEEDED_MESSAGE,
};
use omnicrawl_protocol::{
    conversation_from_openai_messages, tool_spec_from_openai_item, GenerationOptions, ModelReply,
    ModelStreamEvent, ToolSpec,
};
use serde_json::{json, Value};

use omnicrawl_session::tool_result_message;

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
    /// 凭据只从环境读；端点缺省时用运行时默认值。
    ///
    /// 返回 trait 对象：Provider 实现与将来的装饰器（出网脱敏）都从这里换入。
    fn runtime(&self) -> Result<Box<dyn ModelRuntime>, LoopError> {
        let endpoint = ChatEndpoint {
            base_url: endpoint_base_url(&self.config),
            api_key: read_api_key(&self.config)?,
            user_agent: self.config.user_agent.clone(),
        };
        Ok(Box::new(OpenAiChatRuntime::new(endpoint)))
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

/// 端点地址：宿主没给就用运行时默认。
pub(crate) fn endpoint_base_url(config: &KernelModelConfig) -> String {
    if config.base_url.trim().is_empty() {
        ChatEndpoint::default().base_url
    } else {
        config.base_url.clone()
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
        Ok(ordered)
    }
}

impl RemoteTools {
    /// 内核侧工具：命中即本地作答；返回 None 表示交给宿主。
    fn local_tool(&self, call: &ToolCall) -> Option<AgentLoopObservation> {
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
    let mut tools = RemoteTools {
        conn: Rc::clone(conn),
        turn_id: turn_id.clone(),
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
            connection.notify(HostEvent::TurnFinished(TurnFinishedPayload {
                turn_id: turn_id.clone(),
                final_text: result.final_text.clone(),
                reasoning: result.reasoning,
                model_turns: result.model_turns,
                tool_calls: result.tool_calls,
                paused: result.paused,
            }));
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

    let conn = Rc::new(RefCell::new(Conn {
        inbound: receiver,
        out: Box::new(std::io::stdout()),
        cancel: Arc::new(AtomicBool::new(false)),
        handshaken: false,
        exit_requested: false,
        model: None,
        session: None,
        usage: Rc::new(RefCell::new(TurnUsage::default())),
        next_id: 0,
    }));

    eprintln!(
        "[kernel] omnicrawl {} 就绪（协议 v1，等待 initialize）。",
        env!("CARGO_PKG_VERSION")
    );

    loop {
        let inbound = conn.borrow().inbound.recv();
        let frame = match inbound {
            Ok(Inbound::Frame(frame)) => *frame,
            Ok(Inbound::Closed) | Err(_) => break,
        };
        if dispatch(&conn, frame) {
            break;
        }
    }
    Ok(())
}
