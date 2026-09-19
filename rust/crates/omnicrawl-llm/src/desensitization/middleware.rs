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

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use omnicrawl_protocol::{
    ConversationMessage, MessageBlock, ModelStreamEvent, Role, ToolResultBlock, ToolSpec,
};
use serde_json::Value;
use sha2::{Digest, Sha256};

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

/// Python `json.dumps(value, ensure_ascii=False, default=str)` 的等价写法：文本收集与碰撞扫描
/// 逐字节比对，分隔符与键序都要一致。
fn json_text(value: &Value) -> String {
    crate::json::dumps(value)
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

// ── 逐消息屏蔽结果缓存（§5.6 性能） ────────────────────────────────────────

/// 缓存预算（字符）：同时也是「被屏蔽值原文」在缓存里的驻留上限。
pub const MEMO_MAX_CHARS: usize = 1024 * 1024;

/// 命中时返回的快照：屏蔽后的 blocks / reasoning、本周期新增的 (序号, 原文) 对、引用的序号。
type MemoSnapshot = (Vec<MessageBlock>, String, Vec<(u64, String)>, Vec<u64>);

struct MemoEntry {
    blocks: Vec<MessageBlock>,
    reasoning: String,
    pairs: Vec<(u64, String)>,
    chars: usize,
    seqs: Vec<u64>,
}

#[derive(Default)]
struct MemoState {
    order: Vec<Vec<u8>>,
    entries: HashMap<Vec<u8>, MemoEntry>,
    chars: usize,
}

/// 逐消息屏蔽结果缓存。
///
/// 历史每轮全量重发且只追加：已出现过且逐字未变的消息不必重复走匹配引擎。条目还保存
/// 本轮新增的 (序号, 原文) 对，用于把缓存结果重新登记进本周期的还原集合。
///
/// 淘汰策略是「最近用过的」而不是「最久没用的」：历史是每轮从头到尾顺序全扫，
/// 工作集超出预算时 LRU 会正好淘汰下一轮马上要用的条目（抖动到 0 命中）。
pub struct MessageMaskMemo {
    max_chars: usize,
    state: Mutex<MemoState>,
    hits: AtomicU64,
    misses: AtomicU64,
}

impl Default for MessageMaskMemo {
    fn default() -> Self {
        Self::new(MEMO_MAX_CHARS)
    }
}

impl MessageMaskMemo {
    pub fn new(max_chars: usize) -> Self {
        Self {
            max_chars,
            state: Mutex::new(MemoState::default()),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
        }
    }

    pub fn clear(&self) {
        let mut state = memo_lock(&self.state);
        state.order.clear();
        state.entries.clear();
        state.chars = 0;
    }

    /// 命中计数（测试与观测用）。
    pub fn stats(&self) -> (u64, u64) {
        (
            self.hits.load(Ordering::Relaxed),
            self.misses.load(Ordering::Relaxed),
        )
    }

    fn get(&self, key: &[u8]) -> Option<MemoSnapshot> {
        let mut state = memo_lock(&self.state);
        let Some(entry) = state.entries.remove(key) else {
            drop(state);
            self.misses.fetch_add(1, Ordering::Relaxed);
            return None;
        };
        self.hits.fetch_add(1, Ordering::Relaxed);
        let snapshot = (
            entry.blocks.clone(),
            entry.reasoning.clone(),
            entry.pairs.clone(),
            entry.seqs.clone(),
        );
        state.order.retain(|item| item != key);
        state.order.push(key.to_vec());
        state.entries.insert(key.to_vec(), entry);
        Some(snapshot)
    }

    fn put(
        &self,
        key: Vec<u8>,
        blocks: Vec<MessageBlock>,
        reasoning: String,
        pairs: Vec<(u64, String)>,
        chars: usize,
        seqs: Vec<u64>,
    ) {
        if self.max_chars == 0 {
            return;
        }
        let mut state = memo_lock(&self.state);
        if let Some(previous) = state.entries.remove(&key) {
            state.chars = state.chars.saturating_sub(previous.chars);
            state.order.retain(|item| item != &key);
        }
        state.chars += chars;
        state.order.push(key.clone());
        state.entries.insert(
            key,
            MemoEntry {
                blocks,
                reasoning,
                pairs,
                chars,
                seqs,
            },
        );
        while state.chars > self.max_chars && state.order.len() > 1 {
            let Some(evicted) = state.order.pop() else {
                break;
            };
            if let Some(entry) = state.entries.remove(&evicted) {
                state.chars = state.chars.saturating_sub(entry.chars);
            }
        }
    }
}

fn memo_lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|error| error.into_inner())
}

fn digest_text(digest: &mut Sha256, tag: &[u8], text: &str) {
    digest.update(tag);
    digest.update(text.chars().count().to_string().as_bytes());
    digest.update(b"\x1f");
    digest.update(text.as_bytes());
}

/// 角色文本（缓存键用；与 Python 侧的字符串角色同形）。
fn role_text(role: &Role) -> String {
    match role {
        Role::System => "system".to_string(),
        Role::User => "user".to_string(),
        Role::Assistant => "assistant".to_string(),
        Role::Tool => "tool".to_string(),
        Role::Other(value) => value.clone(),
    }
}

/// 缓存键：覆盖「复用安全」所需的全部字段，而不只是被屏蔽的文本。
///
/// 命中缓存后是整块复用缓存里的 blocks，因此任何决定消息身份的字段都必须进键
/// （`call_id` / 函数名 / 图片数据 / `ok`）：只按正文做键会让两条正文相同的消息互相串号。
pub fn message_digest(message: &ConversationMessage) -> Vec<u8> {
    let mut digest = Sha256::new();
    digest_text(&mut digest, b"r", &role_text(&message.role));
    if !message.reasoning.is_empty() {
        digest_text(&mut digest, b"q", &message.reasoning);
    }
    for block in &message.blocks {
        match block {
            MessageBlock::Text(text) => digest_text(&mut digest, b"t", &text.text),
            MessageBlock::ToolCall(call) => {
                digest_text(&mut digest, b"c", &call.call_id);
                digest_text(&mut digest, b"n", &call.name);
                digest_text(&mut digest, b"p", &call.provider_call_id);
                digest_text(
                    &mut digest,
                    b"a",
                    &json_text(&Value::Object(call.arguments.clone())),
                );
            }
            MessageBlock::ToolResult(result) => {
                digest_text(&mut digest, b"s", &result.call_id);
                digest_text(&mut digest, b"k", if result.ok { "1" } else { "0" });
                digest_text(&mut digest, b"v", &result.content);
            }
            MessageBlock::Image(image) => {
                digest_text(&mut digest, b"i", &image.media_type);
                // detail 是枚举：缓存键只要求稳定区分，取它的调试表示即可。
                digest_text(&mut digest, b"d", &format!("{:?}", image.detail));
                digest_text(&mut digest, b"b", &image.data_base64);
            }
        }
    }
    digest.finalize()[..16].to_vec()
}

/// 参与屏蔽的字符总数（预算口径）。
fn masked_chars(message: &ConversationMessage) -> usize {
    let mut total = message.reasoning.chars().count();
    for block in &message.blocks {
        match block {
            MessageBlock::Text(text) => total += text.text.chars().count(),
            MessageBlock::ToolResult(result) => total += result.content.chars().count(),
            MessageBlock::ToolCall(call) => {
                total += json_text(&Value::Object(call.arguments.clone()))
                    .chars()
                    .count()
            }
            MessageBlock::Image(_) => {}
        }
    }
    total
}

/// 消息文本里出现的占位符序号（缓存复用前据此确认本周期可还原）。
pub fn referenced_sequences(message: &ConversationMessage) -> Vec<u64> {
    let mut seqs: Vec<u64> = Vec::new();
    for text in message_texts(message) {
        for (_, _, seq) in find_placeholders(&text) {
            if !seqs.contains(&seq) {
                seqs.push(seq);
            }
        }
    }
    seqs
}

/// 遍历消息中参与屏蔽的文本字段（与屏蔽路径保持一致）。
fn message_texts(message: &ConversationMessage) -> Vec<String> {
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

/// 带缓存的单条消息屏蔽：命中即复用，未命中才走引擎并记入缓存。
pub fn mask_message_cached(
    message: &ConversationMessage,
    ctx: &mut MaskContext<'_>,
    memo: Option<&MessageMaskMemo>,
) -> ConversationMessage {
    let Some(memo) = memo else {
        return mask_message(message, ctx);
    };
    let key = message_digest(message);
    if let Some((blocks, reasoning, pairs, seqs)) = memo.get(&key) {
        for (seq, value) in &pairs {
            ctx.cycle.adopt(value, *seq);
        }
        if seqs.iter().all(|seq| ctx.cycle.lookup(*seq).is_some()) {
            if blocks == message.blocks && reasoning == message.reasoning {
                return message.clone();
            }
            return ConversationMessage {
                role: message.role.clone(),
                blocks,
                reasoning,
                tools: message.tools.clone(),
            };
        }
        // 缓存里的占位符引用了本周期无法还原的序号（登记过该值的消息已被压缩 / 丢弃），
        // 退回重扫，把序号重新登记进本周期。
    }
    let registered_before = ctx.cycle.store_len();
    let masked = mask_message(message, ctx);
    memo.put(
        key,
        masked.blocks.clone(),
        masked.reasoning.clone(),
        ctx.cycle.pairs_from(registered_before),
        masked_chars(&masked),
        referenced_sequences(&masked),
    );
    masked
}

/// 批量屏蔽（带缓存，保持顺序）。
pub fn mask_messages_cached(
    messages: &[ConversationMessage],
    ctx: &mut MaskContext<'_>,

    memo: Option<&MessageMaskMemo>,
) -> Vec<ConversationMessage> {
    messages
        .iter()
        .map(|message| mask_message_cached(message, ctx, memo))
        .collect()
}

// ── 掩码记忆的行为测试 ────────────────────────────────────────────────────

#[cfg(test)]
mod memo_tests {
    use super::*;
    use omnicrawl_protocol::TextBlock;

    fn user_message(text: &str) -> ConversationMessage {
        ConversationMessage {
            role: Role::User,
            blocks: vec![MessageBlock::Text(TextBlock::new(text))],
            reasoning: String::new(),
            tools: Vec::new(),
        }
    }

    #[test]
    fn memo_is_disabled_with_zero_budget() {
        let memo = MessageMaskMemo::new(0);
        memo.put(
            vec![1],
            Vec::new(),
            String::new(),
            Vec::new(),
            10,
            Vec::new(),
        );
        assert!(memo.get(&[1]).is_none(), "预算为 0 时不缓存");
    }

    #[test]
    fn memo_hits_after_put() {
        let memo = MessageMaskMemo::new(100);
        memo.put(
            vec![1],
            Vec::new(),
            "masked".to_string(),
            vec![(7, "原文".to_string())],
            10,
            vec![7],
        );
        let hit = memo.get(&[1]).expect("命中");
        assert_eq!(hit.1, "masked");
        assert_eq!(hit.2, vec![(7, "原文".to_string())]);
        assert_eq!(hit.3, vec![7]);
        assert_eq!(memo.stats(), (1, 0));
    }

    #[test]
    fn memo_evicts_most_recently_used() {
        let memo = MessageMaskMemo::new(25);
        for key in [1u8, 2, 3] {
            memo.put(
                vec![key],
                Vec::new(),
                String::new(),
                Vec::new(),
                10,
                Vec::new(),
            );
        }
        assert!(memo.get(&[1]).is_some(), "最早的条目留下");
        assert!(memo.get(&[2]).is_some(), "次早的条目留下");
        assert!(
            memo.get(&[3]).is_none(),
            "刚放入的条目被淘汰（与 Python 的淘汰口径一致）"
        );
    }

    #[test]
    fn digest_distinguishes_identity_fields() {
        let plain = user_message("same text");
        let mut as_result = user_message("same text");
        as_result.blocks = vec![MessageBlock::ToolResult(ToolResultBlock::new(
            "call-1",
            true,
            "same text",
        ))];
        assert_ne!(message_digest(&plain), message_digest(&as_result));

        let mut other_call = user_message("same text");
        other_call.blocks = vec![MessageBlock::ToolResult(ToolResultBlock::new(
            "call-2",
            true,
            "same text",
        ))];
        assert_ne!(
            message_digest(&as_result),
            message_digest(&other_call),
            "call_id 不同即不同条目"
        );

        let mut failed = user_message("same text");
        failed.blocks = vec![MessageBlock::ToolResult(ToolResultBlock::new(
            "call-1",
            false,
            "same text",
        ))];
        assert_ne!(
            message_digest(&as_result),
            message_digest(&failed),
            "ok 不同即不同条目"
        );
    }

    #[test]
    fn referenced_sequences_are_deduplicated() {
        let placeholder = format!(
            "{}{}{}{}",
            crate::desensitization::FULLWIDTH_OPEN_BRACE,
            crate::desensitization::PLACEHOLDER_MARKER,
            ":3",
            crate::desensitization::FULLWIDTH_CLOSE_BRACE
        );
        let message = user_message(&format!("{placeholder} 与 {placeholder}"));
        assert_eq!(referenced_sequences(&message), vec![3]);
    }
}
