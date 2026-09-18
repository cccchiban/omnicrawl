//! 出站屏蔽 / 入站还原的编排（设计稿 §4.1/§4.2）。
//!
//! 语义基准是 Python `omnicrawl/llm/desensitization/middleware.py` 里**与运行时无关**的那部分：
//! 逐消息屏蔽（只动字符串值：文本块、工具调用参数、工具结果、思考内容）、碰撞扫描用的文本收集、
//! 事件流逐条还原、以及工具参数里「本进程分配过、但已无法还原」的 fail-closed 检查。
//!
//! 未搬：`DesensitizationRuntime` 装饰器本体与 `maybe_wrap_runtime`——Python 侧它包在
//! `ModelRuntime` 协议（`stream_turn` 产出事件流）外面；内核目前只有 `OpenAiChatRuntime`，
//! 先要有 Rust 侧的运行时抽象才能等价装饰（另开一片），因此本模块只提供可复用的编排件。
//! 另未搬：`_MessageMaskMemo`（逐消息屏蔽结果缓存，纯性能优化）。

use omnicrawl_protocol::{
    ConversationMessage, MessageBlock, ModelStreamEvent, Role, ToolResultBlock, ToolSpec,
};
use serde_json::Value;

use super::engine::{mask_structured_value, mask_text, MaskContext};
use super::stream::StreamRestorer;
use super::DesensitizationError;
use super::{find_placeholders, SequenceRegistry};

/// 屏蔽一条消息：只动字符串值，键、结构与图片块不动。
pub fn mask_message(
    message: &ConversationMessage,
    ctx: &mut MaskContext<'_>,
) -> ConversationMessage {
    let blocks: Vec<MessageBlock> = message
        .blocks
        .iter()
        .map(|block| mask_block(block, &message.role, ctx))
        .collect();
    let reasoning = if message.reasoning.is_empty() {
        message.reasoning.clone()
    } else {
        mask_text(&message.reasoning, ctx)
    };
    if blocks == message.blocks && reasoning == message.reasoning {
        return message.clone();
    }
    ConversationMessage {
        role: message.role.clone(),
        blocks,
        reasoning,
        tools: message.tools.clone(),
    }
}

fn mask_block(block: &MessageBlock, role: &Role, ctx: &mut MaskContext<'_>) -> MessageBlock {
    match block {
        // 只处理 user / assistant 文本；system 角色文本与工具声明属结构定义。
        MessageBlock::Text(text) => {
            let is_dialogue = matches!(role, Role::User | Role::Assistant);
            if !is_dialogue || text.text.is_empty() {
                return block.clone();
            }
            let masked = mask_text(&text.text, ctx);
            if masked == text.text {
                return block.clone();
            }
            MessageBlock::Text(omnicrawl_protocol::TextBlock::new(masked))
        }
        MessageBlock::ToolCall(call) => {
            let masked = mask_structured_value(&Value::Object(call.arguments.clone()), ctx);
            match masked {
                Value::Object(arguments) if arguments != call.arguments => {
                    let mut updated = call.clone();
                    updated.arguments = arguments;
                    MessageBlock::ToolCall(updated)
                }
                _ => block.clone(),
            }
        }
        MessageBlock::ToolResult(result) => {
            if result.content.is_empty() {
                return block.clone();
            }
            let masked = mask_text(&result.content, ctx);
            if masked == result.content {
                return block.clone();
            }
            MessageBlock::ToolResult(ToolResultBlock {
                content: masked,
                ..result.clone()
            })
        }
        // 图片块不参与文本匹配。
        MessageBlock::Image(_) => block.clone(),
    }
}

/// 批量屏蔽消息（保持顺序）。
pub fn mask_messages(
    messages: &[ConversationMessage],
    ctx: &mut MaskContext<'_>,
) -> Vec<ConversationMessage> {
    messages
        .iter()
        .map(|message| mask_message(message, ctx))
        .collect()
}

/// 出站内容里的全部文本（含 system 提示词与工具声明），用于占位符序号碰撞扫描。
pub fn collect_request_texts(
    system_prompt: &str,
    messages: &[ConversationMessage],
    tools: &[ToolSpec],
) -> Vec<String> {
    let mut texts = vec![system_prompt.to_string()];
    for message in messages {
        texts.push(message.reasoning.clone());
        for block in &message.blocks {
            match block {
                MessageBlock::Text(text) => texts.push(text.text.clone()),
                MessageBlock::ToolCall(call) => {
                    texts.push(json_text(&Value::Object(call.arguments.clone())))
                }
                MessageBlock::ToolResult(result) => texts.push(result.content.clone()),
                MessageBlock::Image(_) => {}
            }
        }
        for spec in &message.tools {
            push_tool_spec_texts(&mut texts, spec);
        }
    }
    for spec in tools {
        push_tool_spec_texts(&mut texts, spec);
    }
    texts
}

/// 消息自身参与屏蔽的文本字段（与屏蔽路径保持一致）。
pub fn iter_message_texts(message: &ConversationMessage) -> Vec<String> {
    let mut texts = vec![message.reasoning.clone()];
    for block in &message.blocks {
        match block {
            MessageBlock::Text(text) => texts.push(text.text.clone()),
            MessageBlock::ToolResult(result) => texts.push(result.content.clone()),
            MessageBlock::ToolCall(call) => {
                texts.push(json_text(&Value::Object(call.arguments.clone())))
            }
            MessageBlock::Image(_) => {}
        }
    }
    texts
}

fn push_tool_spec_texts(texts: &mut Vec<String>, spec: &ToolSpec) {
    texts.push(spec.name.clone());
    texts.push(spec.description.clone());
    texts.push(json_text(&Value::Object(spec.parameters.clone())));
}

/// Python `json.dumps(value, ensure_ascii=False, default=str)` 的等价实现（失败给空串）。
fn json_text(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_default()
}

/// 工具参数里「本进程分配过、但已无法还原」的占位符序号（写路径的 fail-closed 依据）。
pub fn assigned_but_unresolved(registry: &SequenceRegistry, value: &Value) -> Vec<u64> {
    let index = registry.stable_index();
    let mut found = Vec::new();
    for text in iter_arg_strings(value) {
        for (_, _, seq) in find_placeholders(&text) {
            let assigned = index
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .assigned(seq);
            if assigned && !found.contains(&seq) {
                found.push(seq);
            }
        }
    }
    found
}

fn iter_arg_strings(value: &Value) -> Vec<String> {
    let mut texts = Vec::new();
    collect_arg_strings(value, &mut texts);
    texts
}

fn collect_arg_strings(value: &Value, texts: &mut Vec<String>) {
    match value {
        Value::String(text) => texts.push(text.clone()),
        Value::Object(entries) => {
            for item in entries.values() {
                collect_arg_strings(item, texts);
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_arg_strings(item, texts);
            }
        }
        _ => {}
    }
}

/// 把一条流事件映射为本层应外发的事件（逐事件还原，保持顺序）。
///
/// 与 Python `DesensitizationRuntime._map_event` 一致：文本 / 推理走尾部挂起缓冲，工具参数增量
/// 走同一套缓冲，工具调用完成时做结构化还原（并做 fail-closed 检查），其余事件原样透传。
pub fn map_event(
    event: &ModelStreamEvent,
    restorer: &mut StreamRestorer<'_>,
    registry: &SequenceRegistry,
) -> Result<Vec<ModelStreamEvent>, DesensitizationError> {
    match event {
        ModelStreamEvent::TextDelta(delta) => {
            restorer.note_text(&delta.text);
            let text = restorer.feed_text(&delta.text)?;
            Ok(if text.is_empty() {
                Vec::new()
            } else {
                vec![ModelStreamEvent::TextDelta(
                    omnicrawl_protocol::TextDelta::new(text),
                )]
            })
        }
        ModelStreamEvent::ReasoningDelta(delta) => {
            restorer.note_reasoning(&delta.text);
            let text = restorer.feed_reasoning(&delta.text)?;
            Ok(if text.is_empty() {
                Vec::new()
            } else {
                vec![ModelStreamEvent::ReasoningDelta(
                    omnicrawl_protocol::ReasoningDelta::new(text),
                )]
            })
        }
        ModelStreamEvent::ToolCallArgumentsDelta(delta) => {
            let text = restorer.feed_tool_arguments(&delta.call_id, &delta.delta)?;
            Ok(if text.is_empty() {
                Vec::new()
            } else {
                vec![ModelStreamEvent::ToolCallArgumentsDelta(
                    omnicrawl_protocol::ToolCallArgumentsDelta::new(&delta.call_id, text),
                )]
            })
        }
        ModelStreamEvent::ToolCallCompleted(completed) => {
            restorer.note_tool_call();
            let restored =
                restorer.restore_arguments(&Value::Object(completed.arguments.clone()))?;
            let Value::Object(arguments) = restored else {
                return Ok(vec![event.clone()]);
            };
            let leaked = assigned_but_unresolved(registry, &Value::Object(arguments.clone()));
            if !leaked.is_empty() {
                let sequences = leaked
                    .iter()
                    .map(|seq| seq.to_string())
                    .collect::<Vec<String>>()
                    .join(", ");
                return Err(DesensitizationError::new(format!(
                    "工具参数里出现本进程分配过、但已无法还原的脱敏占位符（序号 {sequences}）；\
已按 fail-closed 中止，避免把占位符写进文件。"
                )));
            }
            let mut updated = completed.clone();
            updated.arguments = arguments;
            Ok(vec![ModelStreamEvent::ToolCallCompleted(updated)])
        }
        ModelStreamEvent::Finished { finish_reason } => {
            restorer.note_completed(finish_reason);
            Ok(vec![event.clone()])
        }
        // ToolCallStarted / UsageReported / ProviderWarning 原样透传（不含文本内容）。
        _ => Ok(vec![event.clone()]),
    }
}
