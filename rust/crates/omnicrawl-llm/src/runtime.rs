//! 内核自带 Provider runtime：一次回合的请求组装、传输、流解析、工具调用收尾与归并。
//!
//! 语义基准是 Python 侧 `OpenAIChatCompletionsRuntime._stream_turn_events`：
//! 建连与读取放在可放弃的后台线程里（取消按 `CANCEL_POLL` 生效，流静默时也能停），
//! 流里的每条 SSE 负载映射成内核事件，收尾时校验工具调用是否完整，最后归并成 `ModelReply`。
//!
//! 通用重试不在本模块（Python 侧位于 agent 层）：这里只在网关不认 `prompt_cache_key`
//! 时摘掉该字段重发一次，其余失败以 `RuntimeError.retryable` 交给调用方决定。

use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, BufReader, Read};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use omnicrawl_protocol::{
    aggregate_stream_events, parse_arguments_object, ModelReply, ModelStreamEvent, ProviderWarning,
    ReasoningDelta, TextDelta, ToolCallCompleted, ToolCallStarted, UsageReported,
};
use serde_json::Value;

use crate::anthropic::{
    build_anthropic_request, format_anthropic_error, AnthropicStreamState, ANTHROPIC_VERSION,
};
use crate::errors::{
    http_status_error_with_body, map_exception, ExceptionView, ModelError, ModelErrorCode,
    RuntimeError,
};
use crate::openai_chat::{
    arguments_json_complete, emit_tool_call_deltas, first_choice, ToolCallBuffer,
};
use crate::request::{build_chat_request, ChatRequestInput};
use crate::sse::{payload_of_line, step_payload, SseStep};
use crate::transport::{self, HttpRequest, HttpResponse, TransportFailure};
use crate::usage::usage_from_openai_payload;

/// 取消检查与流消费共用的轮询节奏：决定取消后的最坏感知延迟。
const CANCEL_POLL: Duration = Duration::from_millis(50);

/// Provider 连接信息。凭据由调用方从环境变量或配置读入，内核不落盘。
pub struct ChatEndpoint {
    pub base_url: String,
    pub api_key: String,
    pub user_agent: String,
}

impl Default for ChatEndpoint {
    fn default() -> Self {
        Self {
            base_url: "https://api.openai.com/v1".to_string(),
            api_key: String::new(),
            user_agent: String::new(),
        }
    }
}

/// 流事件接收端：把增量交给进程主去写 NDJSON，或交给界面。
pub trait TurnSink {
    fn on_event(&mut self, event: ModelStreamEvent) -> SinkFlow;

    /// 事件之外的取消检查：流静默时也要能停下（对应 Python 的 `cancel_check`）。
    fn cancelled(&self) -> bool {
        false
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SinkFlow {
    Continue,
    Cancel,
}

/// 不关心增量时的空接收端。
pub struct DiscardSink;

impl TurnSink for DiscardSink {
    fn on_event(&mut self, _event: ModelStreamEvent) -> SinkFlow {
        SinkFlow::Continue
    }
}

pub struct OpenAiChatRuntime {
    endpoint: ChatEndpoint,
    agent: ureq::Agent,
}

/// 内核侧的模型运行时契约：一次回合的执行入口。
///
/// 语义基准是 Python `omnicrawl/llm/protocol.py` 的 `ModelRuntime` 协议（`stream_turn` + `close`）：
/// 内核用「sink + 取消」表达同一件事——增量经 [`TurnSink`] 外发，取消由 sink 回答，
/// 归并后的回复作为返回值。多一层 trait 是为了让 Provider 实现与装饰器（脱敏）能互换。
pub trait ModelRuntime {
    /// 跑完一次模型请求：请求体 → HTTP → 流事件 → 归并回复。
    fn run_turn(
        &self,
        input: &ChatRequestInput<'_>,
        sink: &mut dyn TurnSink,
    ) -> Result<ModelReply, RuntimeError>;
}

impl ModelRuntime for OpenAiChatRuntime {
    fn run_turn(
        &self,
        input: &ChatRequestInput<'_>,
        sink: &mut dyn TurnSink,
    ) -> Result<ModelReply, RuntimeError> {
        OpenAiChatRuntime::run_turn(self, input, sink)
    }
}

impl OpenAiChatRuntime {
    pub fn new(endpoint: ChatEndpoint) -> Self {
        Self {
            endpoint,
            agent: transport::build_agent(),
        }
    }

    pub fn endpoint(&self) -> &ChatEndpoint {
        &self.endpoint
    }

    /// 跑完一次模型请求：请求体 → HTTP → SSE → 内核事件 → 归并回复。
    pub fn run_turn(
        &self,
        input: &ChatRequestInput<'_>,
        sink: &mut dyn TurnSink,
    ) -> Result<ModelReply, RuntimeError> {
        let api_key = self.endpoint.api_key.trim();
        if api_key.is_empty() {
            return Err(RuntimeError::configuration(
                "模型 Profile 缺少 API Key。请配置 api_key_env 环境变量或 profile.api_key。",
            ));
        }

        let plan = build_chat_request(input)
            .map_err(|error| RuntimeError::configuration(error.message))?;
        let url = format!(
            "{}/chat/completions",
            self.endpoint.base_url.trim_end_matches('/')
        );

        let mut events: Vec<ModelStreamEvent> = Vec::new();
        let mut body = plan.body;
        let mut response = self.post(&url, api_key, &body, plan.timeout_seconds)?;

        let mut error_text = String::new();
        if response.status >= 400 {
            error_text = response.read_text();
            let text = error_text.clone();
            // prompt_cache_key 只是缓存优化：网关不认就摘掉重发一次，并把这件事报给调用方。
            if body.get("prompt_cache_key").is_some()
                && RuntimeError::is_unsupported_prompt_cache_error(&text)
            {
                if let Value::Object(map) = &mut body {
                    map.remove("prompt_cache_key");
                }
                let warning = ModelStreamEvent::ProviderWarning(ProviderWarning::new(
                    "prompt_cache_unsupported",
                    "当前网关不支持 prompt_cache_key，已自动移除后重试。",
                ));
                if emit(&mut events, sink, warning) == SinkFlow::Cancel {
                    return Err(RuntimeError::cancelled());
                }
                response = self.post(&url, api_key, &body, plan.timeout_seconds)?;
            }
        }
        if response.status >= 400 {
            return Err(RuntimeError::from_model_error(http_status_error_with_body(
                response.status,
                &error_text,
            )));
        }

        self.consume_stream(response, sink, &mut events)
    }

    fn post(
        &self,
        url: &str,
        api_key: &str,
        body: &Value,
        timeout_seconds: f64,
    ) -> Result<HttpResponse, RuntimeError> {
        let body = serde_json::to_string(body)
            .map_err(|error| RuntimeError::configuration(format!("请求体无法序列化：{error}")))?;
        let authorization = format!("Bearer {api_key}");
        let headers = [("Authorization", authorization.as_str())];
        transport::send(
            &self.agent,
            &HttpRequest {
                url,
                user_agent: &self.endpoint.user_agent,
                body: &body,
                timeout_seconds,
                headers: &headers,
            },
        )
        .map_err(transport_error)
    }

    fn consume_stream(
        &self,
        response: HttpResponse,
        sink: &mut dyn TurnSink,
        events: &mut Vec<ModelStreamEvent>,
    ) -> Result<ModelReply, RuntimeError> {
        let mut stream = StreamReader::spawn(response.body);
        let mut buffers: BTreeMap<u64, ToolCallBuffer> = BTreeMap::new();
        let mut started: BTreeSet<u64> = BTreeSet::new();
        let mut finish_reason = "stop".to_string();

        loop {
            if sink.cancelled() {
                return Err(RuntimeError::cancelled());
            }
            match stream.next(CANCEL_POLL) {
                StreamOutcome::Idle => continue,
                StreamOutcome::End => break,
                StreamOutcome::ProviderError => {
                    return Err(RuntimeError::provider_error_stream());
                }
                StreamOutcome::Io(message) => {
                    return Err(RuntimeError::stream_interrupted(format!(
                        "Agent 流式回复中断：{message}"
                    )));
                }
                StreamOutcome::Step(SseStep::Terminated) => break,
                StreamOutcome::Step(SseStep::Skip) => continue,
                StreamOutcome::Step(SseStep::Payload(value)) => {
                    let flow = handle_payload(
                        &value,
                        &mut buffers,
                        &mut started,
                        &mut finish_reason,
                        sink,
                        events,
                    );
                    if flow == SinkFlow::Cancel {
                        return Err(RuntimeError::cancelled());
                    }
                }
            }
        }

        // 流正常耗尽不代表工具调用完整：网关可能把断流包装成「正常结束」，
        // 此时名称缺失或参数是半截 JSON 都是截断信号，绝不能静默丢弃。
        for buffer in buffers.values() {
            if buffer.name.trim().is_empty() {
                return Err(RuntimeError::stream_interrupted(
                    "Chat Completions 流在工具调用名称完整到达前结束，疑似连接被网关截断。",
                ));
            }
            if arguments_truncated(&buffer.arguments) {
                return Err(RuntimeError::stream_interrupted(
                    "Chat Completions 流在工具调用参数完整到达前结束，疑似连接被网关截断。",
                ));
            }
        }
        for index in buffers.keys().copied().collect::<Vec<u64>>() {
            let Some(buffer) = buffers.get(&index) else {
                continue;
            };
            let call_id = if buffer.id.is_empty() {
                format!("call_{index}")
            } else {
                buffer.id.clone()
            };
            if !started.contains(&index) {
                let event = ModelStreamEvent::ToolCallStarted(ToolCallStarted::new(
                    call_id.clone(),
                    buffer.name.clone(),
                ));
                if emit(events, sink, event) == SinkFlow::Cancel {
                    return Err(RuntimeError::cancelled());
                }
            }
            let event = ModelStreamEvent::ToolCallCompleted(ToolCallCompleted::new(
                call_id,
                buffer.name.clone(),
                parse_arguments_object(&buffer.arguments),
            ));
            if emit(events, sink, event) == SinkFlow::Cancel {
                return Err(RuntimeError::cancelled());
            }
        }

        let event = ModelStreamEvent::Finished {
            finish_reason: finish_reason.clone(),
        };
        if emit(events, sink, event) == SinkFlow::Cancel {
            return Err(RuntimeError::cancelled());
        }
        Ok(aggregate_stream_events(events.iter().cloned()))
    }
}

/// Anthropic Claude Messages 运行时：与 OpenAI Chat 共用「传输 + 可放弃读取」骨架，
/// 差异只在请求体、鉴权头与流映射（见 [`crate::anthropic`]）。
///
/// 与 `OpenAiChatRuntime` 一样，能力门禁与通用重试不在这一层，由调用方负责。
pub struct AnthropicRuntime {
    endpoint: ChatEndpoint,
    /// 模型描述里的输出上限；仅在生成选项未声明 `max_output_tokens` 时参与兜底。
    descriptor_max_output_tokens: Option<u32>,
    agent: ureq::Agent,
}

impl ModelRuntime for AnthropicRuntime {
    fn run_turn(
        &self,
        input: &ChatRequestInput<'_>,
        sink: &mut dyn TurnSink,
    ) -> Result<ModelReply, RuntimeError> {
        AnthropicRuntime::run_turn(self, input, sink)
    }
}

impl AnthropicRuntime {
    pub fn new(endpoint: ChatEndpoint, descriptor_max_output_tokens: Option<u32>) -> Self {
        Self {
            endpoint,
            descriptor_max_output_tokens,
            agent: transport::build_agent(),
        }
    }

    pub fn endpoint(&self) -> &ChatEndpoint {
        &self.endpoint
    }

    /// 跑完一次模型请求：请求体 → HTTP → SSE → 内核事件 → 归并回复。
    pub fn run_turn(
        &self,
        input: &ChatRequestInput<'_>,
        sink: &mut dyn TurnSink,
    ) -> Result<ModelReply, RuntimeError> {
        let api_key = self.endpoint.api_key.trim();
        if api_key.is_empty() {
            return Err(RuntimeError::configuration(
                "模型 Profile 缺少 API Key。请配置 api_key_env 环境变量或 profile.api_key。",
            ));
        }

        let plan = build_anthropic_request(input, self.descriptor_max_output_tokens)
            .map_err(|error| RuntimeError::configuration(error.message))?;
        let body = serde_json::to_string(&plan.body)
            .map_err(|error| RuntimeError::configuration(format!("请求体无法序列化：{error}")))?;
        let url = format!(
            "{}/v1/messages",
            self.endpoint.base_url.trim_end_matches('/')
        );
        let headers = [
            ("x-api-key", api_key),
            ("anthropic-version", ANTHROPIC_VERSION),
        ];

        let mut response = transport::send(
            &self.agent,
            &HttpRequest {
                url: &url,
                user_agent: self.endpoint.user_agent.as_str(),
                body: &body,
                timeout_seconds: plan.timeout_seconds,
                headers: &headers,
            },
        )
        .map_err(|failure| anthropic_create_error(failure.sdk_view()))?;

        if response.status >= 400 {
            let text = response.read_text();
            return Err(anthropic_create_error((text.as_str(), "APIStatusError")));
        }

        let mut events: Vec<ModelStreamEvent> = Vec::new();
        self.consume_stream(response, sink, &mut events)
    }

    fn consume_stream(
        &self,
        response: HttpResponse,
        sink: &mut dyn TurnSink,
        events: &mut Vec<ModelStreamEvent>,
    ) -> Result<ModelReply, RuntimeError> {
        let mut stream = StreamReader::spawn(response.body);
        let mut state = AnthropicStreamState::new();

        loop {
            if sink.cancelled() {
                return Err(RuntimeError::cancelled());
            }
            match stream.next(CANCEL_POLL) {
                StreamOutcome::Idle => continue,
                StreamOutcome::End => break,
                StreamOutcome::ProviderError => {
                    return Err(RuntimeError::provider_error_stream());
                }
                StreamOutcome::Io(message) => {
                    return Err(RuntimeError::stream_interrupted(format!(
                        "Claude 流式回复中断：{}",
                        format_anthropic_error(&message, "APIConnectionError")
                    )));
                }
                StreamOutcome::Step(SseStep::Terminated) => break,
                StreamOutcome::Step(SseStep::Skip) => continue,
                StreamOutcome::Step(SseStep::Payload(value)) => {
                    let mut produced: Vec<ModelStreamEvent> = Vec::new();
                    state.handle_event(&value, &mut produced);
                    for event in produced {
                        if emit(events, sink, event) == SinkFlow::Cancel {
                            return Err(RuntimeError::cancelled());
                        }
                    }
                }
            }
        }

        let mut tail: Vec<ModelStreamEvent> = Vec::new();
        state.finish(&mut tail);
        for event in tail {
            if emit(events, sink, event) == SinkFlow::Cancel {
                return Err(RuntimeError::cancelled());
            }
        }
        Ok(aggregate_stream_events(events.iter().cloned()))
    }
}

/// 建连或状态码失败：Python 侧 `messages.create` 的异常被包成 `INVALID_REQUEST`，
/// 文案走 `_format_anthropic_error`。
fn anthropic_create_error((message, type_name): (&str, &str)) -> RuntimeError {
    RuntimeError::from_model_error(ModelError {
        code: ModelErrorCode::InvalidRequest,
        message: format!(
            "Claude 请求失败：{}",
            format_anthropic_error(message, type_name)
        ),
        retryable: false,
        status_code: None,
    })
}

/// 传输失败 → 内核错误面：先用 SDK 等价文案走分类阶梯（Python 侧是 SDK 异常 →
/// `map_openai_exception`），再套上 Python 的失败前缀。
fn transport_error(failure: TransportFailure) -> RuntimeError {
    let (message, type_name) = failure.sdk_view();
    let view = ExceptionView {
        message,
        type_name,
        ..ExceptionView::default()
    };
    RuntimeError::from_model_error(map_exception(&view, &[]))
}

fn emit(
    events: &mut Vec<ModelStreamEvent>,
    sink: &mut dyn TurnSink,
    event: ModelStreamEvent,
) -> SinkFlow {
    let flow = sink.on_event(event.clone());
    events.push(event);
    flow
}

/// 单条负载 → 内核事件。顺序与 Python 侧一致：用量 → 结束原因 → 增量内容。
fn handle_payload(
    value: &Value,
    buffers: &mut BTreeMap<u64, ToolCallBuffer>,
    started: &mut BTreeSet<u64>,
    finish_reason: &mut String,
    sink: &mut dyn TurnSink,
    events: &mut Vec<ModelStreamEvent>,
) -> SinkFlow {
    if let Some(usage) = usage_from_openai_payload(value) {
        let event = ModelStreamEvent::UsageReported(UsageReported {
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            cached_input_tokens: usage.cached_input_tokens,
            reasoning_tokens: usage.reasoning_tokens,
        });
        if emit(events, sink, event) == SinkFlow::Cancel {
            return SinkFlow::Cancel;
        }
    }

    let choice = first_choice(value);
    if let Some(reason) = choice
        .and_then(|choice| choice.get("finish_reason"))
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
    {
        *finish_reason = reason.to_string();
    }

    // 跨进程边界上只有 JSON：Python 里 SDK 对象的 delta 分支（getattr）不在这里。
    let Some(delta) = choice.and_then(|choice| choice.get("delta")) else {
        return SinkFlow::Continue;
    };

    if let Some(text) = non_empty_str(delta.get("content")) {
        if emit(
            events,
            sink,
            ModelStreamEvent::TextDelta(TextDelta::new(text)),
        ) == SinkFlow::Cancel
        {
            return SinkFlow::Cancel;
        }
    }
    if let Some(text) = non_empty_str(delta.get("reasoning_content")) {
        if emit(
            events,
            sink,
            ModelStreamEvent::ReasoningDelta(ReasoningDelta::new(text)),
        ) == SinkFlow::Cancel
        {
            return SinkFlow::Cancel;
        }
    }
    if let Some(tool_deltas) = delta.get("tool_calls").and_then(Value::as_array) {
        for event in emit_tool_call_deltas(tool_deltas, buffers, started) {
            if emit(events, sink, event) == SinkFlow::Cancel {
                return SinkFlow::Cancel;
            }
        }
    }
    SinkFlow::Continue
}

fn non_empty_str(value: Option<&Value>) -> Option<&str> {
    value
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
}

fn arguments_truncated(raw: &str) -> bool {
    !arguments_json_complete(&Value::String(raw.to_string()))
}

/// 后台读取线程转发的条目。
enum StreamItem {
    Step(SseStep),
    /// 流内 error 负载。
    ProviderError,
    /// 读取失败（IO）。
    Io(String),
}

enum StreamOutcome {
    Step(SseStep),
    Idle,
    End,
    ProviderError,
    Io(String),
}

/// 可放弃的 SSE 读取：建连与迭代都在后台线程里，消费方按 `CANCEL_POLL` 轮询，
/// 取消后立即返回；被放弃的线程在读到下一条数据或连接关闭时自行退出。
struct StreamReader {
    receiver: mpsc::Receiver<StreamItem>,
    abandoned: Arc<AtomicBool>,
    ended: bool,
}

impl StreamReader {
    fn spawn(body: Box<dyn Read + Send>) -> Self {
        let (sender, receiver) = mpsc::channel();
        let abandoned = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&abandoned);

        thread::spawn(move || {
            let mut reader = BufReader::new(body);
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line) {
                    Ok(0) => return,
                    Ok(_) => {}
                    Err(error) => {
                        let _ = sender.send(StreamItem::Io(error.to_string()));
                        return;
                    }
                }
                if flag.load(Ordering::Relaxed) {
                    return;
                }
                let Some(raw) = payload_of_line(&line) else {
                    continue;
                };
                let step = match step_payload(raw) {
                    Ok(SseStep::Terminated) => {
                        let _ = sender.send(StreamItem::Step(SseStep::Terminated));
                        return;
                    }
                    Ok(step) => step,
                    Err(_) => {
                        let _ = sender.send(StreamItem::ProviderError);
                        return;
                    }
                };
                if sender.send(StreamItem::Step(step)).is_err() {
                    return;
                }
            }
        });

        Self {
            receiver,
            abandoned,
            ended: false,
        }
    }

    fn next(&mut self, poll: Duration) -> StreamOutcome {
        if self.ended {
            return StreamOutcome::End;
        }
        match self.receiver.recv_timeout(poll) {
            Ok(StreamItem::Step(step)) => StreamOutcome::Step(step),
            Ok(StreamItem::ProviderError) => {
                self.ended = true;
                StreamOutcome::ProviderError
            }
            Ok(StreamItem::Io(message)) => {
                self.ended = true;
                StreamOutcome::Io(message)
            }
            Err(RecvTimeoutError::Timeout) => StreamOutcome::Idle,
            Err(RecvTimeoutError::Disconnected) => {
                self.ended = true;
                StreamOutcome::End
            }
        }
    }
}

impl Drop for StreamReader {
    fn drop(&mut self) {
        self.abandoned.store(true, Ordering::Relaxed);
    }
}
