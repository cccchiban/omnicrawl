//! Provider 运行时（Rust 内核）：流解析与请求构建。
//!
//! 语义基准是 Python 侧 `omnicrawl/llm/`：把 Provider 的分片与 SSE 负载映射成统一形式的
//! 流事件与工具调用缓冲，把会话消息组装成请求体，并把 Provider 负载里的用量归一化。
//! 全部为纯逻辑、无 I/O，传输层（HTTP、连接关闭、重试）由宿主负责。
//!
//! 边界要求：产出的每个事件都必须能独立序列化为一行 NDJSON，供进程主增量消费。

mod json;
mod openai_chat;
mod request;
mod sse;
mod usage;

pub use openai_chat::{
    arguments_json_complete, emit_tool_call_deltas, first_choice, ToolCallBuffer,
};
pub use request::{
    build_chat_request, build_prompt_cache_key, is_openai_gpt_model, sanitize_provider_options,
    should_send_prompt_cache_key, to_openai_messages, tool_specs_to_openai_functions, ChatRequest,
    ChatRequestInput, RequestError,
};
pub use sse::{decode_sse_data, iter_raw_sse_events, SseError};
pub use usage::usage_from_openai_payload;
