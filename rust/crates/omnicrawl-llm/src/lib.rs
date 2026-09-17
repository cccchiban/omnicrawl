//! Provider 流解析（Rust 内核）。
//!
//! 语义基准是 Python 侧 `omnicrawl/llm/providers/openai_chat.py`：把 Provider 的分片
//! 与 SSE 负载映射成统一形式的流事件和工具调用缓冲。全部为纯逻辑、无 I/O，
//! 传输层（HTTP、连接关闭、重试）由宿主负责。
//!
//! 边界要求：产出的每个事件都必须能独立序列化为一行 NDJSON，供进程主增量消费。

mod json;
mod openai_chat;
mod sse;

pub use openai_chat::{
    arguments_json_complete, emit_tool_call_deltas, first_choice, ToolCallBuffer,
};
pub use sse::{decode_sse_data, iter_raw_sse_events, SseError};
