//! `omnicrawl/agent/context_compaction/{models,policy}.py` 的移植：上下文预算、完整回合批次
//! 选择与自动压缩决策（本模块同时承载该包的数据模型）。
//!
//! 这一层是纯判定：Token 估算（无第三方 tokenizer）、回合结束时的预算测量、压缩批次与
//! 超限恢复批次的选择、`trigger_reached` / `emergency_ratio_reached` 的判定。摘要模型调用、
//! Session I/O 与投影仍是宿主的活（`service.py` 未搬）。

use crate::error::AgentError;
use crate::json::python_dumps_compact_sorted;
use serde_json::{Map, Value};

pub const COMPACT_SUMMARY_PREFIX: &str = "会话压缩摘要：\n";

const MODEL_CONTEXT_EVENT_TYPES: [&str; 5] = [
    "user_message",
    "assistant_message",
    "tool_call_requested",
    "tool_call_denied",
    "tool_result",
];

/// 一次完整回合或摘要调用内累计的供应商 Token 用量。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TokenUsageSample {
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cached_input_tokens: i64,
}

impl TokenUsageSample {
    pub fn new(
        input_tokens: i64,
        output_tokens: i64,
        cached_input_tokens: i64,
    ) -> Result<Self, AgentError> {
        for (name, value) in [
            ("input_tokens", input_tokens),
            ("output_tokens", output_tokens),
            ("cached_input_tokens", cached_input_tokens),
        ] {
            if value < 0 {
                return Err(AgentError::new(format!("{name} 必须是非负整数。")));
            }
        }
        Ok(Self {
            input_tokens,
            output_tokens,
            cached_input_tokens,
        })
    }

    pub fn add(&self, input: i64, output: i64, cached: i64) -> Self {
        Self {
            input_tokens: self.input_tokens + std::cmp::max(0, input),
            output_tokens: self.output_tokens + std::cmp::max(0, output),
            cached_input_tokens: self.cached_input_tokens + std::cmp::max(0, cached),
        }
    }

    pub fn to_dict(&self) -> Value {
        let mut map = Map::new();
        map.insert("input_tokens".to_string(), Value::from(self.input_tokens));
        map.insert("output_tokens".to_string(), Value::from(self.output_tokens));
        map.insert(
            "cached_input_tokens".to_string(),
            Value::from(self.cached_input_tokens),
        );
        Value::Object(map)
    }
}

/// 完整回合结束后的上下文预算与下一请求估算。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ContextBudgetSnapshot {
    pub stable_context_tokens: i64,
    pub existing_summary_tokens: i64,
    pub cold_history_tokens: i64,
    pub recent_history_tokens: i64,
    pub next_user_reserve_tokens: i64,
    pub target_summary_tokens: i64,
    pub estimated_next_input_tokens: i64,
    pub post_turn_context_tokens: i64,
    pub simulated_compacted_input_tokens: i64,
    pub potential_retired_tokens: i64,
    pub trigger_context_tokens: i64,
    pub context_window_tokens: i64,
    pub trigger_reached: bool,
    pub emergency_ratio_reached: bool,
    pub cache_hit_ratio: f64,
    pub provider_input_tokens: i64,
}

impl ContextBudgetSnapshot {
    pub fn to_dict(&self) -> Value {
        let mut map = Map::new();
        map.insert(
            "stable_context_tokens".to_string(),
            Value::from(self.stable_context_tokens),
        );
        map.insert(
            "existing_summary_tokens".to_string(),
            Value::from(self.existing_summary_tokens),
        );
        map.insert(
            "cold_history_tokens".to_string(),
            Value::from(self.cold_history_tokens),
        );
        map.insert(
            "recent_history_tokens".to_string(),
            Value::from(self.recent_history_tokens),
        );
        map.insert(
            "next_user_reserve_tokens".to_string(),
            Value::from(self.next_user_reserve_tokens),
        );
        map.insert(
            "target_summary_tokens".to_string(),
            Value::from(self.target_summary_tokens),
        );
        map.insert(
            "estimated_next_input_tokens".to_string(),
            Value::from(self.estimated_next_input_tokens),
        );
        map.insert(
            "post_turn_context_tokens".to_string(),
            Value::from(self.post_turn_context_tokens),
        );
        map.insert(
            "simulated_compacted_input_tokens".to_string(),
            Value::from(self.simulated_compacted_input_tokens),
        );
        map.insert(
            "potential_retired_tokens".to_string(),
            Value::from(self.potential_retired_tokens),
        );
        map.insert(
            "trigger_context_tokens".to_string(),
            Value::from(self.trigger_context_tokens),
        );
        map.insert(
            "context_window_tokens".to_string(),
            Value::from(self.context_window_tokens),
        );
        map.insert(
            "trigger_reached".to_string(),
            Value::from(self.trigger_reached),
        );
        map.insert(
            "emergency_ratio_reached".to_string(),
            Value::from(self.emergency_ratio_reached),
        );
        map.insert(
            "cache_hit_ratio".to_string(),
            Value::from(self.cache_hit_ratio),
        );
        map.insert(
            "provider_input_tokens".to_string(),
            Value::from(self.provider_input_tokens),
        );
        Value::Object(map)
    }
}

/// 从 Session 事实源复制出的只读事件。
#[derive(Debug, Clone, PartialEq)]
pub struct SourceEvent {
    pub event_id: String,
    pub event_type: String,
    pub payload: Value,
}

impl SourceEvent {
    pub fn to_prompt_dict(&self) -> Value {
        let mut map = Map::new();
        map.insert("event_id".to_string(), Value::from(self.event_id.clone()));
        map.insert("type".to_string(), Value::from(self.event_type.clone()));
        map.insert("payload".to_string(), self.payload.clone());
        Value::Object(map)
    }

    /// ID、类型与短预览：正文留在复用的原文上下文里。
    pub fn to_index_dict(&self, preview_chars: usize) -> Value {
        let mut map = Map::new();
        map.insert("event_id".to_string(), Value::from(self.event_id.clone()));
        map.insert("type".to_string(), Value::from(self.event_type.clone()));
        map.insert("preview".to_string(), Value::from(self.preview(preview_chars)));
        Value::Object(map)
    }

    /// 与 [`Self::to_index_dict`] 同形，但把不透明的 24 位事件 ID 换成短引用（如 `E12`）。
    ///
    /// 摘要模型逐字复制 24 位十六进制 ID 的失败率很高（实测会整段编造，一处不符就让整份摘要
    /// 作废），短引用则易抄且由代码还原真 ID：索引里不再出现真 ID，模型也就无从抄错。
    pub fn to_ref_index_dict(&self, reference: &str, preview_chars: usize) -> Value {
        let mut map = Map::new();
        map.insert("ref".to_string(), Value::from(reference));
        map.insert("type".to_string(), Value::from(self.event_type.clone()));
        map.insert("preview".to_string(), Value::from(self.preview(preview_chars)));
        Value::Object(map)
    }

    /// 索引里的内容预览：超长时截断并补省略号。
    fn preview(&self, preview_chars: usize) -> String {
        let serialized = compact_serialize(&self.payload);
        if preview_chars > 0 && preview_chars < serialized.chars().count() {
            return format!(
                "{}…",
                serialized.chars().take(preview_chars).collect::<String>()
            );
        }
        serialized
    }
}

fn compact_serialize(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_default()
}

/// 本次交给摘要模型的完整事件批次。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CompactionBatch {
    pub events: Vec<SourceEvent>,
    pub recent_events: Vec<SourceEvent>,
    pub previous_summary: Option<Value>,
    pub previous_covered_event_ids: Vec<String>,
    pub single_large_turn: bool,
}

impl CompactionBatch {
    pub fn covered_event_ids(&self) -> Vec<String> {
        let mut ordered = self.previous_covered_event_ids.clone();
        ordered.extend(self.events.iter().map(|event| event.event_id.clone()));
        dedup_preserve_order(ordered)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutoCompactionDecision {
    pub should_compact: bool,
    pub reason: &'static str,
}

fn dedup_preserve_order(values: Vec<String>) -> Vec<String> {
    let mut seen: Vec<String> = Vec::new();
    for value in values {
        if value.is_empty() || seen.contains(&value) {
            continue;
        }
        seen.push(value);
    }
    seen
}

/// 本地、确定性且无第三方 tokenizer 依赖的上下文预算管理器。
#[derive(Debug, Clone, Default)]
pub struct DefaultContextBudgetManager;

/// 测量入参（消息一律按 Python 的 `Mapping` 形状给 JSON）。
#[derive(Debug, Clone, Default)]
pub struct MeasureInput {
    pub system_prompt: String,
    pub context_messages: Vec<Value>,
    pub history_messages: Vec<Value>,
    pub tool_schemas: Vec<Value>,
    pub recent_turns: i64,
    pub target_summary_tokens: i64,
    pub next_user_reserve_tokens: i64,
    pub trigger_context_tokens: i64,
    pub context_window_tokens: i64,
    pub usage: TokenUsageSample,
    pub provider_input_tokens: i64,
    pub emergency_context_ratio: f64,
}

/// 按 Token 计数直接测量（`measure_from_token_counts`）。
#[derive(Debug, Clone, Default)]
pub struct TokenCountInput {
    pub stable_context_tokens: i64,
    pub existing_summary_tokens: i64,
    pub cold_history_tokens: i64,
    pub recent_history_tokens: i64,
    pub next_user_reserve_tokens: i64,
    pub target_summary_tokens: i64,
    pub trigger_context_tokens: i64,
    pub context_window_tokens: i64,
    pub usage: TokenUsageSample,
    pub emergency_context_ratio: f64,
    pub provider_input_tokens: i64,
}

impl DefaultContextBudgetManager {
    pub fn measure(&self, input: &MeasureInput) -> Result<ContextBudgetSnapshot, AgentError> {
        if input.recent_turns <= 0 {
            return Err(AgentError::new("recent_turns 必须是正整数。"));
        }

        let mut history = input.history_messages.clone();
        let mut summary_messages: Vec<Value> = Vec::new();
        if !history.is_empty() && is_compact_summary(&history[0]) {
            summary_messages.push(history.remove(0));
        }

        let cutoff = recent_turn_cutoff(&history, input.recent_turns);
        let cold_messages = &history[..cutoff];
        let recent_messages = &history[cutoff..];
        let mut stable_context_tokens = estimate_text_tokens(&input.system_prompt);
        stable_context_tokens += estimate_messages_tokens(&input.context_messages);
        stable_context_tokens += estimate_json_tokens(&Value::Array(input.tool_schemas.clone()));

        self.measure_from_token_counts(&TokenCountInput {
            stable_context_tokens,
            existing_summary_tokens: estimate_messages_tokens(&summary_messages),
            cold_history_tokens: estimate_messages_tokens(cold_messages),
            recent_history_tokens: estimate_messages_tokens(recent_messages),
            next_user_reserve_tokens: input.next_user_reserve_tokens,
            target_summary_tokens: input.target_summary_tokens,
            trigger_context_tokens: input.trigger_context_tokens,
            context_window_tokens: input.context_window_tokens,
            usage: input.usage,
            emergency_context_ratio: input.emergency_context_ratio,
            provider_input_tokens: input.provider_input_tokens,
        })
    }

    pub fn measure_from_token_counts(
        &self,
        input: &TokenCountInput,
    ) -> Result<ContextBudgetSnapshot, AgentError> {
        let counts = [
            ("stable_context_tokens", input.stable_context_tokens),
            ("existing_summary_tokens", input.existing_summary_tokens),
            ("cold_history_tokens", input.cold_history_tokens),
            ("recent_history_tokens", input.recent_history_tokens),
            ("next_user_reserve_tokens", input.next_user_reserve_tokens),
            ("target_summary_tokens", input.target_summary_tokens),
            ("trigger_context_tokens", input.trigger_context_tokens),
            ("context_window_tokens", input.context_window_tokens),
        ];
        for (name, value) in counts {
            if value < 0 {
                return Err(AgentError::new(format!("{name} 必须是非负整数。")));
            }
        }
        if input.trigger_context_tokens <= 0 || input.context_window_tokens <= 0 {
            return Err(AgentError::new("触发阈值和上下文窗口必须是正整数。"));
        }
        if !(input.emergency_context_ratio > 0.0 && input.emergency_context_ratio < 1.0) {
            return Err(AgentError::new(
                "emergency_context_ratio 必须满足 0 < value < 1。",
            ));
        }
        if input.provider_input_tokens < 0 {
            return Err(AgentError::new("provider_input_tokens 必须是非负整数。"));
        }

        // 预估下一请求输入必须包含全部历史：压缩前冷历史仍原样进入下一次模型请求。
        let estimated_next_input_tokens = input.stable_context_tokens
            + input.existing_summary_tokens
            + input.cold_history_tokens
            + input.recent_history_tokens
            + input.next_user_reserve_tokens;
        let compactable_tokens = input.existing_summary_tokens + input.cold_history_tokens;
        // target_summary_tokens <= 0 表示无摘要预算上限：保守按「不承诺节省」处理。
        let simulated_summary_tokens = if input.target_summary_tokens <= 0 {
            compactable_tokens
        } else {
            std::cmp::min(compactable_tokens, input.target_summary_tokens)
        };
        let potential_retired_tokens =
            std::cmp::max(0, compactable_tokens - simulated_summary_tokens);
        let cache_hit_ratio = if input.usage.input_tokens > 0 {
            (std::cmp::min(input.usage.cached_input_tokens, input.usage.input_tokens) as f64)
                / (input.usage.input_tokens as f64)
        } else {
            0.0
        };
        let emergency_tokens =
            ((input.context_window_tokens as f64) * input.emergency_context_ratio).ceil() as i64;
        // 触发口径＝回合结束后实际上下文（不含下一轮用户预留）；与供应商回报取大值。
        let post_turn_context_tokens = std::cmp::max(
            input.stable_context_tokens
                + input.existing_summary_tokens
                + input.cold_history_tokens
                + input.recent_history_tokens,
            input.provider_input_tokens,
        );

        Ok(ContextBudgetSnapshot {
            stable_context_tokens: input.stable_context_tokens,
            existing_summary_tokens: input.existing_summary_tokens,
            cold_history_tokens: input.cold_history_tokens,
            recent_history_tokens: input.recent_history_tokens,
            next_user_reserve_tokens: input.next_user_reserve_tokens,
            target_summary_tokens: input.target_summary_tokens,
            estimated_next_input_tokens,
            simulated_compacted_input_tokens: estimated_next_input_tokens
                - potential_retired_tokens,
            potential_retired_tokens,
            trigger_context_tokens: input.trigger_context_tokens,
            context_window_tokens: input.context_window_tokens,
            post_turn_context_tokens,
            trigger_reached: post_turn_context_tokens >= input.trigger_context_tokens,
            emergency_ratio_reached: estimated_next_input_tokens >= emergency_tokens,
            cache_hit_ratio,
            provider_input_tokens: input.provider_input_tokens,
        })
    }

    /// 选择由上下文超限中断的未完成回合，供一次性恢复重试使用。
    pub fn select_recovery_batch(&self, events: &[SourceEvent]) -> Option<CompactionBatch> {
        let (boundary_index, previous_summary, previous_covered) = summary_boundary(events);
        let carried_events = carry_events(events, boundary_index, previous_summary.as_ref());
        let mut candidates = carried_events;
        if boundary_index >= 0 {
            candidates.extend(events[(boundary_index as usize) + 1..].iter().cloned());
        } else {
            candidates.extend(events.iter().cloned());
        }
        let recoverable: Vec<SourceEvent> = candidates
            .into_iter()
            .filter(|event| is_model_context_event(&event.event_type))
            .collect();
        match recoverable.last() {
            Some(last) if last.event_type == "user_message" => Some(CompactionBatch {
                events: recoverable,
                recent_events: Vec::new(),
                previous_summary,
                previous_covered_event_ids: previous_covered,
                single_large_turn: true,
            }),
            _ => None,
        }
    }

    /// 从最后摘要边界之后选择全部完整回合，不截断工具链或未完成回合。
    pub fn select_batch(&self, events: &[SourceEvent]) -> Option<CompactionBatch> {
        let (boundary_index, previous_summary, previous_covered) = summary_boundary(events);
        let mut candidates = carry_events(events, boundary_index, previous_summary.as_ref());
        if boundary_index >= 0 {
            candidates.extend(events[(boundary_index as usize) + 1..].iter().cloned());
        } else {
            candidates.extend(events.iter().cloned());
        }
        let turns = complete_turns(&candidates);
        if turns.is_empty() {
            return None;
        }
        let compact_events: Vec<SourceEvent> =
            turns.iter().flat_map(|turn| turn.iter().cloned()).collect();
        if compact_events.is_empty() {
            return None;
        }
        Some(CompactionBatch {
            events: compact_events,
            recent_events: Vec::new(),
            previous_summary,
            previous_covered_event_ids: previous_covered,
            single_large_turn: turns.len() == 1,
        })
    }

    pub fn decide_auto_compaction(
        snapshot: ContextBudgetSnapshot,
        batch: Option<&CompactionBatch>,
    ) -> AutoCompactionDecision {
        if !snapshot.trigger_reached {
            return AutoCompactionDecision {
                should_compact: false,
                reason: "below_trigger_threshold",
            };
        }
        if batch.is_none() {
            return AutoCompactionDecision {
                should_compact: false,
                reason: "no_complete_batch",
            };
        }
        AutoCompactionDecision {
            should_compact: true,
            reason: "trigger_reached",
        }
    }
}

fn is_model_context_event(event_type: &str) -> bool {
    MODEL_CONTEXT_EVENT_TYPES.contains(&event_type)
}

fn summary_boundary(events: &[SourceEvent]) -> (i64, Option<Value>, Vec<String>) {
    let mut boundary_index: i64 = -1;
    let mut previous_summary: Option<Value> = None;
    let mut previous_covered: Vec<String> = Vec::new();
    for (index, event) in events.iter().enumerate() {
        if event.event_type != "compact_summary" {
            continue;
        }
        boundary_index = index as i64;
        previous_summary = Some(event.payload.clone());
        previous_covered = event
            .payload
            .get("covered_event_ids")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .filter(|item| !item.is_empty())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
    }
    (boundary_index, previous_summary, previous_covered)
}

fn carry_events(
    events: &[SourceEvent],
    boundary_index: i64,
    previous_summary: Option<&Value>,
) -> Vec<SourceEvent> {
    if boundary_index < 0 {
        return Vec::new();
    }
    let Some(summary) = previous_summary else {
        return Vec::new();
    };
    let before_boundary = &events[..boundary_index as usize];

    let remaining_ids: Vec<String> = summary
        .get("remaining_event_ids")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .filter(|item| !item.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    if !remaining_ids.is_empty() {
        return before_boundary
            .iter()
            .filter(|event| remaining_ids.contains(&event.event_id))
            .cloned()
            .collect();
    }

    let remaining_count = summary
        .get("remaining_message_count")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    if remaining_count > 0 {
        let model_events: Vec<SourceEvent> = before_boundary
            .iter()
            .filter(|event| is_model_context_event(&event.event_type))
            .cloned()
            .collect();
        let keep = std::cmp::min(remaining_count as usize, model_events.len());
        return model_events[model_events.len() - keep..].to_vec();
    }
    Vec::new()
}

fn complete_turns(events: &[SourceEvent]) -> Vec<Vec<SourceEvent>> {
    let mut turns: Vec<Vec<SourceEvent>> = Vec::new();
    let mut pending: Vec<SourceEvent> = Vec::new();
    for event in events {
        if event.event_type == "user_message" {
            pending = vec![event.clone()];
            continue;
        }
        if pending.is_empty() {
            continue;
        }
        pending.push(event.clone());
        if event.event_type == "assistant_message" {
            turns.push(pending.clone());
            pending.clear();
        }
    }
    turns
}

pub fn estimate_messages_tokens(messages: &[Value]) -> i64 {
    let mut total = 0;
    for message in messages {
        total += 4;
        total += estimate_text_tokens(&value_text(message.get("role")));
        total += estimate_value_tokens(&message.get("content").cloned().unwrap_or(Value::from("")));
        for key in ["name", "tool_call_id", "tool_calls"] {
            if let Some(value) = message.get(key) {
                total += estimate_value_tokens(value);
            }
        }
    }
    total
}

fn value_text(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Null) | None => String::new(),
        Some(Value::Bool(true)) => "True".to_string(),
        Some(Value::Bool(false)) => "False".to_string(),
        Some(other) => other.to_string(),
    }
}

pub fn estimate_value_tokens(value: &Value) -> i64 {
    match value {
        Value::String(text) => estimate_text_tokens(text),
        other => estimate_json_tokens(other),
    }
}

pub fn estimate_json_tokens(value: &Value) -> i64 {
    estimate_text_tokens(&python_dumps_compact_sorted(value))
}

/// CJK 字符按 1 token、其余按 4 字符 1 token 折算。
pub fn estimate_text_tokens(text: &str) -> i64 {
    if text.is_empty() {
        return 0;
    }
    let total = text.chars().count() as i64;
    let cjk = text.chars().filter(|item| is_cjk(*item)).count() as i64;
    let rest = total - cjk;
    cjk + (rest + 3) / 4
}

fn is_cjk(item: char) -> bool {
    matches!(item as u32,
        0x3400..=0x4dbf | 0x4e00..=0x9fff | 0xf900..=0xfaff | 0x3040..=0x30ff | 0xac00..=0xd7af)
}

fn recent_turn_cutoff(messages: &[Value], recent_turns: i64) -> usize {
    let user_indexes: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter(|(_, message)| message.get("role").and_then(Value::as_str) == Some("user"))
        .map(|(index, _)| index)
        .collect();
    if user_indexes.len() as i64 <= recent_turns {
        return 0;
    }
    user_indexes[user_indexes.len() - recent_turns as usize]
}

fn is_compact_summary(message: &Value) -> bool {
    value_text(message.get("content"))
        .trim()
        .starts_with(COMPACT_SUMMARY_PREFIX.trim())
}
