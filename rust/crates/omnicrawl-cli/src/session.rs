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

use omnicrawl_core::{
    AgentLoopLimits, AgentLoopObservation, AgentLoopRunner, AgentModelReply, LoopError, LoopGuards,
    ReplySource, SystemClock, ToolBatchHost, ToolCall,
};
use omnicrawl_ipc::bridge::{
    initialize_result, method, unsupported_version_error, BridgeError, Command, HostEvent,
    InitializeParams, KernelModelConfig, MessagePayload, ModelRequest, TextPayload,
    TokenUsagePayload, ToolBatch, ToolBatchResult, TurnCancelParams, TurnFinishedPayload,
    TurnSubmitParams,
};
use omnicrawl_ipc::frame::{error_code, ErrorObject, Frame, Id};
use omnicrawl_ipc::version::negotiate_version;
use omnicrawl_llm::{
    to_openai_messages, ChatEndpoint, ChatRequestInput, ModelRuntime, OpenAiChatRuntime,
    RuntimeErrorKind, SinkFlow, TurnSink,
};
use omnicrawl_protocol::{
    conversation_from_openai_messages, tool_spec_from_openai_item, GenerationOptions, ModelReply,
    ModelStreamEvent, ToolSpec,
};
use serde_json::{json, Value};

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
}

impl ReplySource for KernelModelPort {
    fn request_reply(&mut self, messages: &mut Vec<Value>) -> Result<AgentModelReply, LoopError> {
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
fn endpoint_base_url(config: &KernelModelConfig) -> String {
    if config.base_url.trim().is_empty() {
        ChatEndpoint::default().base_url
    } else {
        config.base_url.clone()
    }
}

/// 凭据只从环境读：帧里出现的只是环境变量名。
fn read_api_key(config: &KernelModelConfig) -> Result<String, LoopError> {
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

fn parse_options(config: &KernelModelConfig) -> Result<GenerationOptions, LoopError> {
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
        let batch = ToolBatch {
            turn_id: self.turn_id.clone(),
            step: first_step,
            calls: calls.to_vec(),
        };
        let params = serde_json::to_value(&batch)
            .expect("tool.batch 负载是serde_json::Value字段，必须可序列化");
        let value = self
            .conn
            .borrow_mut()
            .request(method::TOOL_BATCH, params)
            .map_err(PortFailure::into_tool_error)?;
        ToolBatchResult::from_result(&value)
            .map(|parsed| parsed.observations)
            .map_err(|error| LoopError::ToolBatch(format!("宿主返回的观察无法解析：{error}")))
    }
}

/// 跑一个回合：用户输入进上下文 → 模型与工具交替 → `turn.finished`。
fn run_turn(conn: &Rc<RefCell<Conn>>, request_id: Id, params: TurnSubmitParams) {
    let turn_id = params.turn_id.clone();
    conn.borrow().cancel.store(false, Ordering::SeqCst);

    let mut messages = vec![json!({"role": "user", "content": params.user_text})];
    // 宿主给了模型配置就由内核自己发请求；没给则维持 model.reply 代答的兼容路径。
    let mut model: Box<dyn ReplySource> = match conn.borrow().model.clone() {
        Some(config) => Box::new(KernelModelPort {
            conn: Rc::clone(conn),
            config,
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
    let cancel_source = Rc::clone(conn);
    let mut cancel_check = move || {
        if cancel_source.borrow().cancel.load(Ordering::SeqCst) {
            Err(LoopError::Cancelled("回合已取消。".to_string()))
        } else {
            Ok(())
        }
    };
    let guards = LoopGuards {
        cancel_check: Some(&mut cancel_check),
        stop_check: None,
    };
    let runner = AgentLoopRunner::new(Box::new(SystemClock::new()));

    // 主 Agent 的预算是无限的：与 Python 侧一致，靠取消与停止检查来收敛。
    let outcome = runner.run(
        &mut messages,
        &mut *model,
        &mut tools,
        AgentLoopLimits::default(),
        guards,
    );

    let mut connection = conn.borrow_mut();
    match outcome {
        Ok(result) => {
            connection.notify(HostEvent::TurnFinished(TurnFinishedPayload {
                turn_id: turn_id.clone(),
                final_text: result.final_text,
                reasoning: result.reasoning,
                model_turns: result.model_turns,
                tool_calls: result.tool_calls,
                paused: result.paused,
            }));
            connection.respond(request_id, json!({}));
        }
        Err(error) => connection.respond_error(request_id, turn_error(&error)),
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
            ..
        }) => {
            if let Some(config) = model {
                conn.borrow_mut().model = Some(*config);
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
