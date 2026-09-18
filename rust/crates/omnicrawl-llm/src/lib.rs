//! Provider 运行时（Rust 内核）：请求构建、HTTP 传输、流解析、用量归一化与回合装配。
//!
//! 语义基准是 Python 侧 `omnicrawl/llm/`：把会话消息组装成请求体、把 Provider 的分片与
//! SSE 负载映射成统一形式的流事件与工具调用缓冲、把负载里的用量归一化，并由
//! `OpenAiChatRuntime` 把一次回合从请求串到归并回复。
//! 传输之外没有别的 I/O；通用重试与能力门禁留在调用方。

mod anthropic;
mod capabilities;
pub mod desensitization;
mod errors;
mod json;
mod openai_chat;
mod registry;
mod request;
mod responses;
mod runtime;
mod sse;
mod transport;
mod usage;

pub use anthropic::{
    build_anthropic_request, format_anthropic_error, sanitize_anthropic_options,
    to_anthropic_messages, AnthropicRequest, AnthropicStreamState, ANTHROPIC_VERSION,
};
pub use capabilities::{merge_capabilities, ModelCapabilities};
pub use errors::{
    http_status_error, http_status_error_with_body, map_exception, ExceptionView, ModelError,
    ModelErrorCode, RuntimeError, RuntimeErrorKind, CONTEXT_LENGTH_EXCEEDED_MESSAGE,
};
pub use openai_chat::{
    arguments_json_complete, emit_tool_call_deltas, first_choice, ToolCallBuffer,
};
pub use registry::{protocol_for_provider, resolve_protocol, validate_protocol_matches_provider};
pub use request::{
    build_chat_request, build_prompt_cache_key, is_openai_gpt_model, sanitize_provider_options,
    should_send_prompt_cache_key, to_openai_messages, tool_specs_to_openai_functions, ChatRequest,
    ChatRequestInput, RequestError,
};
pub use responses::{
    build_responses_request, flatten_tool_history_to_text, has_tool_history_items,
    is_tool_history_rejection, messages_to_responses_input, tools_for_responses,
    ResponsesStreamState,
};
pub use runtime::{
    AnthropicRuntime, ChatEndpoint, DiscardSink, ModelRuntime, OpenAiChatRuntime, SinkFlow,
    TurnSink,
};
pub use sse::{
    decode_sse_data, iter_raw_sse_events, payload_of_line, step_payload, SseError, SseStep,
};
pub use usage::{usage_from_anthropic_payload, usage_from_openai_payload};
