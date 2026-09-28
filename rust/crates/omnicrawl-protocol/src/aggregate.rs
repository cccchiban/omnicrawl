//! 流事件归并：把 Adapter 事件流拼成一次完整的模型回复。

use crate::codec::parse_arguments_object;
use crate::event::{
    ModelReply, ModelStreamEvent, ProviderWarning, ReasoningDelta, TextDelta,
    ToolCallArgumentsDelta, ToolCallCompleted, ToolCallStarted, UsageReported,
};
use crate::message::{
    ConversationMessage, MessageBlock, Role, TextBlock, TokenUsage, ToolCallBlock,
};

/// 尚未收到 Completed 的工具调用缓冲。
///
/// Python 侧用 dict 保存，插入顺序即后续补全顺序；这里用 Vec 保持同样的顺序语义，
/// 工具调用数量很小，线性查找足够。
#[derive(Default)]
struct PendingToolCalls {
    entries: Vec<PendingToolCall>,
}

struct PendingToolCall {
    call_id: String,
    name: String,
    arguments: String,
}

impl PendingToolCalls {
    fn start(&mut self, call_id: &str, name: &str) {
        match self
            .entries
            .iter_mut()
            .find(|entry| entry.call_id == call_id)
        {
            Some(entry) => {
                entry.name = name.to_string();
                entry.arguments.clear();
            }
            None => self.entries.push(PendingToolCall {
                call_id: call_id.to_string(),
                name: name.to_string(),
                arguments: String::new(),
            }),
        }
    }

    fn append_arguments(&mut self, call_id: &str, delta: &str) {
        match self
            .entries
            .iter_mut()
            .find(|entry| entry.call_id == call_id)
        {
            Some(entry) => entry.arguments.push_str(delta),
            None => self.entries.push(PendingToolCall {
                call_id: call_id.to_string(),
                name: String::new(),
                arguments: delta.to_string(),
            }),
        }
    }

    fn take_completed(&mut self, call_id: &str) {
        self.entries.retain(|entry| entry.call_id != call_id);
    }

    fn drain_unfinished(self) -> Vec<PendingToolCall> {
        self.entries
    }
}

/// 把 Adapter 流事件归并为一次模型回复。
pub fn aggregate_stream_events(events: impl IntoIterator<Item = ModelStreamEvent>) -> ModelReply {
    let events: Vec<ModelStreamEvent> = events.into_iter().collect();
    aggregate_stream_events_ref(events.iter())
}

/// 归并的事件序列视图：调用方已经持有事件表（或还要继续用这条流）时走这个入口。
///
/// 归并内部只读事件字段、按需复制字符串，不再先把整条流克隆一份。
pub fn aggregate_stream_events_ref<'a, I>(events: I) -> ModelReply
where
    I: IntoIterator<Item = &'a ModelStreamEvent>,
{
    let mut content_parts: Vec<String> = Vec::new();
    let mut reasoning_parts: Vec<String> = Vec::new();
    let mut tool_calls: Vec<ToolCallBlock> = Vec::new();
    let mut pending = PendingToolCalls::default();
    let mut usage: Option<TokenUsage> = None;
    let mut finish_reason = "stop".to_string();
    let mut content_streamed = false;
    let mut warnings: Vec<ProviderWarning> = Vec::new();

    for event in events {
        match event {
            ModelStreamEvent::TextDelta(TextDelta { text }) => {
                if !text.is_empty() {
                    content_parts.push(text.clone());
                    content_streamed = true;
                }
            }
            ModelStreamEvent::ReasoningDelta(ReasoningDelta { text }) => {
                if !text.is_empty() {
                    reasoning_parts.push(text.clone());
                }
            }
            ModelStreamEvent::ToolCallStarted(ToolCallStarted { call_id, name }) => {
                pending.start(call_id, name);
            }
            ModelStreamEvent::ToolCallArgumentsDelta(ToolCallArgumentsDelta { call_id, delta }) => {
                pending.append_arguments(call_id, delta);
            }
            ModelStreamEvent::ToolCallCompleted(ToolCallCompleted {
                call_id,
                name,
                arguments,
            }) => {
                tool_calls.push(ToolCallBlock::new(
                    call_id.clone(),
                    name.clone(),
                    arguments.clone(),
                ));
                pending.take_completed(call_id);
            }
            ModelStreamEvent::UsageReported(UsageReported {
                input_tokens,
                output_tokens,
                cached_input_tokens,
                reasoning_tokens,
            }) => {
                usage = Some(TokenUsage {
                    input_tokens: *input_tokens,
                    output_tokens: *output_tokens,
                    cached_input_tokens: *cached_input_tokens,
                    reasoning_tokens: *reasoning_tokens,
                });
            }
            ModelStreamEvent::Finished {
                finish_reason: reason,
            } => {
                finish_reason = if reason.is_empty() {
                    "stop".to_string()
                } else {
                    reason.clone()
                };
            }
            ModelStreamEvent::ProviderWarning(warning) => warnings.push(warning.clone()),
        }
    }

    // 未以 Completed 结束但已有缓冲的工具调用：尽量解析 JSON 参数。
    for entry in pending.drain_unfinished() {
        let arguments = parse_arguments_object(&entry.arguments);
        let name = entry.name.trim().to_string();
        if !name.is_empty() {
            tool_calls.push(ToolCallBlock::new(entry.call_id, name, arguments));
        }
    }

    let content = content_parts.concat();
    let reasoning = reasoning_parts.concat().trim().to_string();
    let mut blocks: Vec<MessageBlock> = Vec::new();
    if !content.is_empty() {
        blocks.push(MessageBlock::Text(TextBlock::new(content.clone())));
    }
    blocks.extend(tool_calls.iter().cloned().map(MessageBlock::ToolCall));

    ModelReply {
        assistant_message: ConversationMessage {
            role: Role::Assistant,
            blocks,
            reasoning: String::new(),
            tools: Vec::new(),
        },
        content,
        reasoning,
        tool_calls,
        usage,
        finish_reason,
        content_streamed,
        warnings,
    }
}
