//! OmniCrawl 协议内核（Rust）。
//!
//! 与 Python 侧 `omnicrawl/llm/protocol.py` 语义对齐：Provider 无关的消息块、工具调用、
//! 流事件与流事件归并。全部为纯逻辑、无 I/O，可在嵌入式 Linux 上直接复用。
//! 类型命名与 Python 的对应关系见 `rust/README.md`。

pub mod aggregate;
pub mod codec;
pub mod event;
pub mod identity;
pub mod message;

pub use aggregate::aggregate_stream_events;
pub use codec::{
    blocks_from_openai_content_parts, conversation_from_openai_messages, parse_arguments_object,
    tool_spec_from_openai_item, tools_from_conversation_messages,
};
pub use event::{
    ModelReply, ModelStreamEvent, ProviderWarning, ReasoningDelta, TextDelta,
    ToolCallArgumentsDelta, ToolCallCompleted, ToolCallStarted, UsageReported,
};
pub use identity::{ModelIdentity, Protocol, Provider};
pub use message::{
    ConversationMessage, GenerationOptions, ImageBlock, ImageDetail, MessageBlock, Role, TextBlock,
    TokenUsage, ToolCallBlock, ToolResultBlock, ToolSpec,
};
