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
    http_status_error_with_body, is_retryable_model_request_error, map_exception, ExceptionView,
    ModelError, ModelErrorCode, RuntimeError, RuntimeErrorKind,
};
use crate::gemini::{
    build_generate_content_request, format_gemini_error, gemini_model_path, generate_content_body,
    GeminiStreamState,
};
use crate::openai_chat::{
    arguments_json_complete, emit_tool_call_deltas, first_choice, ToolCallBuffer,
};
use crate::request::{build_chat_request, ChatRequestInput};
use crate::responses::{
    build_responses_request_with_items, flatten_tool_history_to_text, has_tool_history_items,
    is_tool_history_rejection, messages_to_responses_input, ResponsesStreamState,
};
use crate::sse::{payload_of_line, step_payload, SseError, SseStep};
use crate::stream_registry::CancelHandle;
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
                StreamOutcome::Cancelled => return Err(RuntimeError::cancelled()),
                StreamOutcome::ProviderError { message, body } => {
                    // OpenAI 一族：文案与可重试标记都走 Python 的分类阶梯。
                    return Err(RuntimeError::openai_provider_error_stream(
                        &message,
                        Some(&body),
                    ));
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
                StreamOutcome::Cancelled => return Err(RuntimeError::cancelled()),
                StreamOutcome::ProviderError { message, .. } => {
                    // Anthropic：文案前缀与格式化器换成 Claude 一路，且与 Python 一致地不可重试。
                    return Err(RuntimeError::stream_interrupted_unretryable(format!(
                        "Claude 流式回复中断：{}",
                        format_anthropic_error(&message, "APIError")
                    )));
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

/// 一次失败请求的等价 SDK 视图（内核没有 SDK 异常对象，只有状态码与等价文案）。
struct SdkFailure {
    message: String,
    type_name: String,
    status_code: Option<u16>,
}

impl SdkFailure {
    fn http(message: String, status: u16) -> Self {
        Self {
            message,
            type_name: "APIStatusError".to_string(),
            status_code: Some(status),
        }
    }

    fn transport(failure: TransportFailure) -> Self {
        let (message, type_name) = failure.sdk_view();
        Self {
            message: message.to_string(),
            type_name: type_name.to_string(),
            status_code: None,
        }
    }
}

/// OpenAI Responses 运行时：与另外三路共用「传输 + 可放弃读取」骨架，
/// 差异在请求体（`responses.rs`）、两路降级重试与流收尾顺序。
///
/// 降级重试照 Python `_create_stream_with_retries`：网关不认 `prompt_cache_key` 时摘字段重发一次；
/// 网关不支持工具调用历史 item（HTTP 400，部分兼容网关对个别模型的已知限制）时把工具历史展平为
/// 纯文本重发一次，并记住这个组合（后续请求直接展平，不再先发一次必然 400 的请求）。
/// 两条降级都在流正常收尾时补发一条告警，顺序与 Python 一致：截断判定之后、缓冲冲刷之前。
pub struct ResponsesRuntime {
    endpoint: ChatEndpoint,
    agent: ureq::Agent,
    tool_history_unsupported: AtomicBool,
}

impl ModelRuntime for ResponsesRuntime {
    fn run_turn(
        &self,
        input: &ChatRequestInput<'_>,
        sink: &mut dyn TurnSink,
    ) -> Result<ModelReply, RuntimeError> {
        ResponsesRuntime::run_turn(self, input, sink)
    }
}

impl ResponsesRuntime {
    pub fn new(endpoint: ChatEndpoint) -> Self {
        Self {
            endpoint,
            agent: transport::build_agent(),
            tool_history_unsupported: AtomicBool::new(false),
        }
    }

    pub fn endpoint(&self) -> &ChatEndpoint {
        &self.endpoint
    }

    /// 跑完一次模型请求：请求体 → HTTP →（可降级重发）→ SSE → 内核事件 → 归并回复。
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

        let mut input_items = messages_to_responses_input(input.messages);
        if self.tool_history_unsupported.load(Ordering::Relaxed)
            && has_tool_history_items(&input_items)
        {
            input_items = flatten_tool_history_to_text(&input_items);
        }
        let plan = build_responses_request_with_items(input, &input_items)
            .map_err(|error| RuntimeError::configuration(error.message))?;
        let url = format!("{}/responses", self.endpoint.base_url.trim_end_matches('/'));
        let mut body = plan.body;

        let mut prompt_cache_warning = false;
        let mut tool_history_warning = false;
        let mut failure: Option<SdkFailure> = None;
        let mut response = match self.post(&url, api_key, &body, plan.timeout_seconds) {
            Ok(mut item) => {
                if item.status >= 400 {
                    let text = item.read_text();
                    failure = Some(SdkFailure::http(text, item.status));
                    None
                } else {
                    Some(item)
                }
            }
            Err(transport_failure) => {
                failure = Some(SdkFailure::transport(transport_failure));
                None
            }
        };

        // 策略 1：网关不认 prompt_cache_key → 摘掉该字段重发一次。
        let cache_rejected = failure.as_ref().is_some_and(|current| {
            body.get("prompt_cache_key").is_some()
                && RuntimeError::is_unsupported_prompt_cache_error(&current.message)
        });
        if cache_rejected {
            if let Value::Object(map) = &mut body {
                map.remove("prompt_cache_key");
            }
            match self.post(&url, api_key, &body, plan.timeout_seconds) {
                Ok(item) if item.status < 400 => {
                    response = Some(item);
                    prompt_cache_warning = true;
                    failure = None;
                }
                Ok(mut item) => {
                    let text = item.read_text();
                    failure = Some(SdkFailure::http(text, item.status));
                }
                Err(transport_failure) => {
                    failure = Some(SdkFailure::transport(transport_failure));
                }
            }
        }

        // 策略 2：网关不支持工具调用历史 item（400）→ 展平成纯文本重发一次，并记住该组合。
        let history_rejected = failure
            .as_ref()
            .is_some_and(|current| is_tool_history_rejection(&input_items, current.status_code));
        if history_rejected {
            let flattened = flatten_tool_history_to_text(&input_items);
            if let Value::Object(map) = &mut body {
                map.insert("input".to_string(), Value::Array(flattened));
            }
            match self.post(&url, api_key, &body, plan.timeout_seconds) {
                Ok(item) if item.status < 400 => {
                    response = Some(item);
                    self.tool_history_unsupported.store(true, Ordering::Relaxed);
                    tool_history_warning = true;
                    failure = None;
                }
                Ok(mut item) => {
                    let text = item.read_text();
                    failure = Some(SdkFailure::http(text, item.status));
                }
                Err(transport_failure) => {
                    failure = Some(SdkFailure::transport(transport_failure));
                }
            }
        }

        if let Some(current) = failure {
            return Err(responses_failure_error(&current));
        }
        let response = response.expect("失败分支已经返回");
        self.consume_stream(response, sink, prompt_cache_warning, tool_history_warning)
    }

    fn post(
        &self,
        url: &str,
        api_key: &str,
        body: &Value,
        timeout_seconds: f64,
    ) -> Result<HttpResponse, TransportFailure> {
        let Ok(body) = serde_json::to_string(body) else {
            // serde_json 对 Value 恒可序列化；这里只是不引入 panic 的兜底。
            return Err(TransportFailure::Other("请求体无法序列化。".to_string()));
        };
        let authorization = format!("Bearer {api_key}");
        let headers = [("Authorization", authorization.as_str())];
        transport::send(
            &self.agent,
            &HttpRequest {
                url,
                user_agent: self.endpoint.user_agent.as_str(),
                body: &body,
                timeout_seconds,
                headers: &headers,
            },
        )
    }

    fn consume_stream(
        &self,
        response: HttpResponse,
        sink: &mut dyn TurnSink,
        prompt_cache_warning: bool,
        tool_history_warning: bool,
    ) -> Result<ModelReply, RuntimeError> {
        let mut stream = StreamReader::spawn(response.body);
        let mut state = ResponsesStreamState::new();
        let mut events: Vec<ModelStreamEvent> = Vec::new();

        loop {
            if sink.cancelled() {
                return Err(RuntimeError::cancelled());
            }
            match stream.next(CANCEL_POLL) {
                StreamOutcome::Idle => continue,
                StreamOutcome::End => break,
                StreamOutcome::Cancelled => return Err(RuntimeError::cancelled()),
                StreamOutcome::ProviderError { message, .. } => {
                    return Err(responses_stream_error(&SdkFailure {
                        message,
                        type_name: "APIError".to_string(),
                        status_code: None,
                    }));
                }
                StreamOutcome::Io(message) => {
                    return Err(responses_stream_error(&SdkFailure {
                        message,
                        type_name: "APIConnectionError".to_string(),
                        status_code: None,
                    }));
                }
                StreamOutcome::Step(SseStep::Terminated) => break,
                StreamOutcome::Step(SseStep::Skip) => continue,
                StreamOutcome::Step(SseStep::Payload(value)) => {
                    let mut produced: Vec<ModelStreamEvent> = Vec::new();
                    state.handle_event(&value, &mut produced);
                    for event in produced {
                        if emit(&mut events, sink, event) == SinkFlow::Cancel {
                            return Err(RuntimeError::cancelled());
                        }
                    }
                }
            }
        }

        // Python 的收尾顺序：截断判定 → 降级告警 → 缓冲冲刷 → 结束事件。
        state.check_completeness()?;
        if prompt_cache_warning {
            let warning = ModelStreamEvent::ProviderWarning(ProviderWarning::new(
                "prompt_cache_unsupported",
                "当前网关不支持 prompt_cache_key，已自动移除后重试。",
            ));
            if emit(&mut events, sink, warning) == SinkFlow::Cancel {
                return Err(RuntimeError::cancelled());
            }
        }
        if tool_history_warning {
            let warning = ModelStreamEvent::ProviderWarning(ProviderWarning::new(
                "tool_history_flattened",
                "当前网关不支持工具调用历史 item（HTTP 400），已自动转为纯文本后重试；后续请求将直接使用该适配。",
            ));
            if emit(&mut events, sink, warning) == SinkFlow::Cancel {
                return Err(RuntimeError::cancelled());
            }
        }
        let mut tail: Vec<ModelStreamEvent> = Vec::new();
        state.flush_buffers(&mut tail)?;
        state.push_finished(&mut tail);
        for event in tail {
            if emit(&mut events, sink, event) == SinkFlow::Cancel {
                return Err(RuntimeError::cancelled());
            }
        }
        Ok(aggregate_stream_events(events))
    }
}

/// 建连（含降级重发）失败：Python 侧 `responses.create` 的异常被包成
/// `Responses 请求失败：{format_openai_error(exc)}`，可重试标记由异常本身决定。
fn responses_failure_error(failure: &SdkFailure) -> RuntimeError {
    let mapped = mapped_failure(failure);
    RuntimeError {
        kind: RuntimeErrorKind::RequestFailed,
        message: format!("Responses 请求失败：{}", mapped.message),
        retryable: is_retryable_model_request_error(&failure.message, failure.status_code),
        status_code: mapped.status_code,
    }
}

/// 流中断：`Responses 流式回复中断：{format_openai_error(exc)}`。
fn responses_stream_error(failure: &SdkFailure) -> RuntimeError {
    let mapped = mapped_failure(failure);
    RuntimeError {
        kind: RuntimeErrorKind::StreamInterrupted,
        message: format!("Responses 流式回复中断：{}", mapped.message),
        retryable: is_retryable_model_request_error(&failure.message, failure.status_code),
        status_code: None,
    }
}

fn mapped_failure(failure: &SdkFailure) -> ModelError {
    map_exception(
        &ExceptionView {
            message: failure.message.as_str(),
            type_name: failure.type_name.as_str(),
            status_code: failure.status_code.map(i64::from),
            ..ExceptionView::default()
        },
        &[],
    )
}

/// Google Gemini Generate Content 运行时：与另外两路共用「传输 + 可放弃读取」骨架，
/// 差异只在请求体（MLDev 线上映射）、鉴权头与流映射（见 [`crate::gemini`]）。
///
/// `ChatEndpoint.base_url` 是 API 根（默认 `https://generativelanguage.googleapis.com`），
/// 版本段与 `:streamGenerateContent` 由本模块补齐。
pub struct GeminiRuntime {
    endpoint: ChatEndpoint,
    agent: ureq::Agent,
}

impl ModelRuntime for GeminiRuntime {
    fn run_turn(
        &self,
        input: &ChatRequestInput<'_>,
        sink: &mut dyn TurnSink,
    ) -> Result<ModelReply, RuntimeError> {
        GeminiRuntime::run_turn(self, input, sink)
    }
}

impl GeminiRuntime {
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

        let plan = build_generate_content_request(input)
            .map_err(|error| RuntimeError::configuration(error.message))?;
        let body = serde_json::to_string(&generate_content_body(&plan))
            .map_err(|error| RuntimeError::configuration(format!("请求体无法序列化：{error}")))?;
        let url = format!(
            "{}/v1beta/{}:streamGenerateContent?alt=sse",
            self.endpoint.base_url.trim_end_matches('/'),
            gemini_model_path(&plan.model)
        );
        let headers = [("x-goog-api-key", api_key)];

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
        .map_err(|failure| gemini_create_error(failure.sdk_view()))?;

        if response.status >= 400 {
            let text = response.read_text();
            return Err(gemini_create_error((text.as_str(), "APIStatusError")));
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
        let mut state = GeminiStreamState::new();

        loop {
            if sink.cancelled() {
                return Err(RuntimeError::cancelled());
            }
            match stream.next(CANCEL_POLL) {
                StreamOutcome::Idle => continue,
                StreamOutcome::End => break,
                StreamOutcome::Cancelled => return Err(RuntimeError::cancelled()),
                StreamOutcome::ProviderError { message, .. } => {
                    // Gemini：文案前缀与格式化器换成 Gemini 一路，且与 Python 一致地不可重试。
                    return Err(RuntimeError::stream_interrupted_unretryable(format!(
                        "Gemini 流式回复中断：{}",
                        format_gemini_error(&message, "APIError")
                    )));
                }
                StreamOutcome::Io(message) => {
                    return Err(RuntimeError::stream_interrupted(format!(
                        "Gemini 流式回复中断：{}",
                        format_gemini_error(&message, "APIConnectionError")
                    )));
                }
                StreamOutcome::Step(SseStep::Terminated) => break,
                StreamOutcome::Step(SseStep::Skip) => continue,
                StreamOutcome::Step(SseStep::Payload(value)) => {
                    let mut produced: Vec<ModelStreamEvent> = Vec::new();
                    state.handle_chunk(&value, &mut produced);
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

/// 建连或状态码失败：Python 侧 `generate_content_stream` 的异常被包成 `INVALID_REQUEST`，
/// 文案走 `_format_gemini_error`。
fn gemini_create_error((message, type_name): (&str, &str)) -> RuntimeError {
    RuntimeError::from_model_error(ModelError {
        code: ModelErrorCode::InvalidRequest,
        message: format!(
            "Gemini 请求失败：{}",
            format_gemini_error(message, type_name)
        ),
        retryable: false,
        status_code: None,
    })
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
    /// 流内 error 负载（带出来源文案与错误体，供各 Provider 分类）。
    ProviderError {
        message: String,
        body: Value,
    },
    /// 读取失败（IO）。
    Io(String),
}

enum StreamOutcome {
    Step(SseStep),
    Idle,
    End,
    /// 流被**另一个线程**关闭（回合注册表里的取消），与 sink 自己的取消同义。
    Cancelled,
    ProviderError {
        message: String,
        body: Value,
    },
    Io(String),
}

/// 可放弃的 SSE 读取：建连与迭代都在后台线程里，消费方按 `CANCEL_POLL` 轮询，
/// 取消后立即返回；被放弃的线程在读到下一条数据或连接关闭时自行退出。
///
/// 每条流都登记进回合资源注册表（`stream_registry`，对应 Python 的 `register_stream`）：
/// 任一线程拿同一个回合归属调 `close_active_streams` 就能停掉它——消费方在下一次轮询
/// 看到 `Cancelled`，读取线程也在下一个数据边界退出。以前只有 sink 自己答取消，
/// 跨线程（审批取消 / 界面中断）的回合中断没有着力点。
struct StreamReader {
    receiver: mpsc::Receiver<StreamItem>,
    abandoned: Arc<AtomicBool>,
    /// 注册表里的身份；丢弃时注销，避免悬挂条目。
    handle: Arc<CancelHandle>,
    ended: bool,
}

impl StreamReader {
    fn spawn(body: Box<dyn Read + Send>) -> Self {
        let (sender, receiver) = mpsc::channel();
        let abandoned = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&abandoned);
        // 注册“关闭动作”= 置放弃位：读取线程下一个边界退出，消费方下一次轮询即返回取消。
        // 归属取当前线程的回合作用域（Python 的 `stream_owner_for(cancel_check)` 同义）。
        let handle = CancelHandle::with_close({
            let abandoned = Arc::clone(&abandoned);
            move || abandoned.store(true, Ordering::Relaxed)
        });
        crate::stream_registry::register_stream(
            Some(&handle),
            crate::stream_registry::current_stream_scope(),
            None,
        );

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
                    // 目前唯一的错误分支就是流内 error 负载：把文案与错误体带给消费方。
                    Err(SseError::Provider { message, body }) => {
                        let _ = sender.send(StreamItem::ProviderError { message, body });
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
            handle,
            ended: false,
        }
    }

    fn next(&mut self, poll: Duration) -> StreamOutcome {
        if self.ended {
            return StreamOutcome::End;
        }
        // 另一个线程关掉了这条流：立刻返回，不等下一次超时窗口。
        if self.abandoned.load(Ordering::Relaxed) {
            self.ended = true;
            return StreamOutcome::Cancelled;
        }
        match self.receiver.recv_timeout(poll) {
            Ok(StreamItem::Step(step)) => StreamOutcome::Step(step),
            Ok(StreamItem::ProviderError { message, body }) => {
                self.ended = true;
                StreamOutcome::ProviderError { message, body }
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
        crate::stream_registry::unregister_stream(&self.handle);
    }
}
