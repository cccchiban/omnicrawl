//! `omnicrawl/agent/context_compaction/{models,policy}.py` 的移植：上下文预算、完整回合批次
//! 选择与自动压缩决策（本模块同时承载该包的数据模型）。
//!
//! 这一层是纯判定：Token 估算（无第三方 tokenizer）、回合结束时的预算测量、压缩批次与
//! 超限恢复批次的选择、`trigger_reached` / `emergency_ratio_reached` 的判定。摘要模型调用、
//! Session I/O 与投影仍是宿主的活（`service.py` 未搬）。

use std::collections::BTreeSet;

use crate::error::AgentError;
use crate::json::python_dumps_compact_sorted;
use crate::turn::tool_prune::EVICTED_EVENT_TYPE;
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
    /// 价格来源：供应商上报的输入 Token，或纯启发式。
    ///
    /// `provider_input_tokens` 仍是「取大值」后的口径（与 Python 对齐），这里是计量器自己的
    /// 判断：只有供应商数字不低于启发式锚点时才采信它，后续只叠加有符号增量。
    pub baseline: Option<super::meter::MeasurementBaseline>,
    /// 相对锚点的有符号增量：压缩、淘汰让它变负。
    pub surface_delta_tokens: i64,
    /// 逐事件价格（模型可见事件），供范围选择从尾部累加。
    pub nodes: Vec<super::meter::TokenNode>,
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
        map.insert("payload".to_string(), prompt_payload(&self.payload));
        Value::Object(map)
    }

    /// ID、类型与短预览：正文留在复用的原文上下文里。
    pub fn to_index_dict(&self, preview_chars: usize) -> Value {
        let mut map = Map::new();
        map.insert("event_id".to_string(), Value::from(self.event_id.clone()));
        map.insert("type".to_string(), Value::from(self.event_type.clone()));
        map.insert(
            "preview".to_string(),
            Value::from(self.preview(preview_chars)),
        );
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
        map.insert(
            "preview".to_string(),
            Value::from(self.preview(preview_chars)),
        );
        Value::Object(map)
    }

    /// 索引里的内容预览：超长时截断并补省略号。
    fn preview(&self, preview_chars: usize) -> String {
        let serialized = compact_serialize(&prompt_payload(&self.payload));
        if preview_chars > 0 && preview_chars < serialized.chars().count() {
            return format!(
                "{}…",
                serialized.chars().take(preview_chars).collect::<String>()
            );
        }
        serialized
    }
}

/// 交给摘要模型的载荷：图片的 Base64 正｛Desensitized:708｝换成一行描述。
///
/// 用户消息可以带粘贴的图片，图片原文（几百 KB 的 Base64）**不属于可摘要的内容**：
/// 留在提示里只会把摘要输入撑爆、让 Token 估算与实际上下文完全脱节（估算按字符数算，
/// Base64 又全是高熵字符）。这里只留张数与大小，摘要模型仍然知道「这条消息带了图」。
fn prompt_payload(payload: &Value) -> Value {
    let Some(images) = payload.get("images").and_then(Value::as_array) else {
        return payload.clone();
    };
    if images.is_empty() {
        return payload.clone();
    }
    let summarized: Vec<Value> = images
        .iter()
        .map(|image| {
            let media_type = image
                .get("media_type")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let bytes = image
                .get("data_base64")
                .and_then(Value::as_str)
                .map(|data| data.len())
                .unwrap_or(0);
            Value::from(format!("<图片 {media_type}，约 {} 字节>", bytes / 4 * 3))
        })
        .collect();
    let mut prompt = payload.clone();
    if let Value::Object(map) = &mut prompt {
        map.insert("images".to_string(), Value::Array(summarized));
    }
    prompt
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
    /// 已投影好的逐事件价格；有值时预算快照会一并发出去，供范围选择按尾部累加。
    pub nodes: Vec<super::meter::TokenNode>,
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
    pub nodes: Vec<super::meter::TokenNode>,
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
        // 稳定上下文与历史的计价走同一入口（`meter::conversation_*`），宿主回报的
        // 「压缩后大小」也用它，三处不可能各算各的。
        let stable_context_tokens = super::meter::conversation_tokens(
            &input.system_prompt,
            &input.context_messages,
            &input.tool_schemas,
            &[],
            &estimate_text_tokens,
        );

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
            nodes: input.nodes.clone(),
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
        // 触发口径＝回合结束后实际上下文（不含下一轮用户预留）。
        //
        // 供应商回报只有「不低于启发式锚点」时才采信：摘要调用尚未回来时供应商数字会滞后，
        // 此时相信它会低估压力、让这一回合错过压缩窗口。两条路只与估算比较，不叠加。
        let heuristic_context_tokens = input.stable_context_tokens
            + input.existing_summary_tokens
            + input.cold_history_tokens
            + input.recent_history_tokens;
        let baseline = if input.provider_input_tokens > 0
            && input.provider_input_tokens >= heuristic_context_tokens
        {
            super::meter::MeasurementBaseline::Usage(input.provider_input_tokens)
        } else {
            super::meter::MeasurementBaseline::Estimated(heuristic_context_tokens)
        };
        // 有符号增量＝当前启发式表面 − 采信基准。采信供应商采样时它是负值（启发式表面更小），
        // 之后压缩再减一次也不会重复计数；采信启发式时恒为 0。
        let surface_delta_tokens = heuristic_context_tokens - baseline.tokens();
        let post_turn_context_tokens =
            std::cmp::max(heuristic_context_tokens, input.provider_input_tokens);

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
            baseline: Some(baseline),
            surface_delta_tokens,
            nodes: Vec::new(),
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
        let recoverable: Vec<SourceEvent> = drop_evicted_tool_events(candidates, events)
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
        let candidates = drop_evicted_tool_events(candidates, events);
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

    /// 按逐事件价格选择压缩窗口：从尾部往前累加，凑够保留量即停。
    ///
    /// 与 [`Self::select_batch`]（整个历史一次压完）不同，这里按**价格**而不是回合边界
    /// 决定窗口，且不劈开工具调用/结果对——劈开会让摘要模型看到孤立的 `tool_result`，
    /// 完整性校验必然判为不合格。
    ///
    /// `nodes` 的价格账（顺序即模型可见顺序），`retain_tokens` 是要留在上下文里的量，
    /// 返回**需要压缩**的事件 ID（保留窗口之前的那一段）。
    pub fn select_range_by_tokens(
        nodes: &[super::meter::TokenNode],
        retain_tokens: i64,
    ) -> Vec<String> {
        if nodes.is_empty() {
            return Vec::new();
        }
        let mut kept: i64 = 0;
        let mut boundary = nodes.len();
        while boundary > 0 && kept < retain_tokens {
            boundary -= 1;
            kept += nodes[boundary].tokens;
        }
        // 边界前移，直到保留窗口的第一条不再是孤立的工具结果：连它的调用请求一起留下。
        while boundary > 0
            && matches!(
                nodes[boundary].event_type.as_str(),
                "tool_result" | "tool_call_denied"
            )
        {
            boundary -= 1;
        }
        nodes[..boundary]
            .iter()
            .map(|node| node.event_id.clone())
            .collect()
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

/// 单条 inline 摘录的字符上限。
const DETERMINISTIC_EXCERPT_CHARS: usize = 400;

/// 确定性摘要：不调用模型，按事件顺序抽取正文，产出与模型摘要同形的结构。
///
/// 模型摘要不可用或校验不通过时用它兜底（配置 `failure_fallback = "deterministic"` 承诺的
/// 就是这条路）。此前只有配置解析在读这个字段、没有消费方，于是「摘要失败」等于「本回合
/// 完全不压缩」，上下文只会继续涨。
///
/// 抽取而不是概括是有意为之：兜底路径没有模型可判断重要性，能逐字保留的原文比有损改写更
/// 可靠；条数不做裁剪，避免漏掉校验要求的覆盖项（失败调用、写入文件、用户消息）。
pub fn deterministic_structured_summary(
    events: &[SourceEvent],
    previous_summary: Option<&Value>,
) -> Value {
    let mut objective: Vec<Value> = Vec::new();
    let mut user_messages: Vec<Value> = Vec::new();
    let mut failed_attempts: Vec<Value> = Vec::new();
    let mut modified_files: Vec<Value> = Vec::new();
    let mut completed: Vec<Value> = Vec::new();
    let mut exact_evidence: Vec<Value> = Vec::new();
    let mut last_reply: Option<String> = None;
    let mut results: Vec<(String, &SourceEvent)> = Vec::new();

    for event in events {
        let content = event
            .payload
            .get("content")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty());
        match event.event_type.as_str() {
            "user_message" => {
                let Some(content) = content else { continue };
                objective.push(Value::from(excerpt(content, DETERMINISTIC_EXCERPT_CHARS)));
                user_messages.push(single_ref_entry(content, &event.event_id));
            }
            "assistant_message" => {
                let Some(content) = content else { continue };
                last_reply = Some(content.to_string());
                completed.push(single_ref_entry(
                    &excerpt(content, DETERMINISTIC_EXCERPT_CHARS),
                    &event.event_id,
                ));
            }
            "tool_result" => {
                let call_id = event
                    .payload
                    .get("tool_call_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                results.push((call_id, event));
            }
            _ => {}
        }
    }

    let call_by_id: Vec<(&str, &SourceEvent)> = events
        .iter()
        .filter(|event| event.event_type == "tool_call_requested")
        .filter_map(|event| {
            event
                .payload
                .get("tool_call_id")
                .and_then(Value::as_str)
                .map(|call_id| (call_id, event))
        })
        .collect();

    for (call_id, result) in &results {
        let requested = call_by_id
            .iter()
            .find(|(existing, _)| existing == call_id)
            .map(|(_, event)| *event);
        let tool = result
            .payload
            .get("tool")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let output = result
            .payload
            .get("model_output")
            .or_else(|| result.payload.get("output"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        let ok = result
            .payload
            .get("ok")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let mut refs: Vec<String> = Vec::new();
        if let Some(requested) = requested {
            refs.push(requested.event_id.clone());
        }
        refs.push(result.event_id.clone());

        if ok {
            let path = requested
                .and_then(|event| event.payload.get("arguments"))
                .and_then(Value::as_object)
                .and_then(|arguments| arguments.get("path"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .trim()
                .to_string();
            if !path.is_empty() && matches!(tool.as_str(), "write_file" | "Edit_file") {
                let mut item = Map::new();
                item.insert("path".to_string(), Value::from(path));
                item.insert(
                    "description".to_string(),
                    Value::from("确定性兜底：写入已执行，变更内容见归档原文。"),
                );
                item.insert(
                    "source_event_ids".to_string(),
                    Value::Array(refs.iter().cloned().map(Value::from).collect()),
                );
                modified_files.push(Value::Object(item));
            }
            continue;
        }

        let summary = if tool.is_empty() {
            "工具调用失败。".to_string()
        } else {
            format!(
                "{tool} 失败：{}",
                excerpt(output.trim(), DETERMINISTIC_EXCERPT_CHARS)
            )
        };
        failed_attempts.push(entry(&summary, &refs));
        // 精确证据要求逐字一致，截断过的文本不再等于原文，因此只收完整片段。
        let raw = output.trim();
        if !raw.is_empty() && raw.chars().count() <= DETERMINISTIC_EXCERPT_CHARS {
            exact_evidence.push(entry(raw, &refs));
        }
    }

    let current_state = last_reply
        .as_deref()
        .map(|text| excerpt(text, DETERMINISTIC_EXCERPT_CHARS))
        .unwrap_or_else(|| "（压缩窗口内没有可抽取的助手回复。）".to_string());

    let mut structured = Map::new();
    structured.insert("objective".to_string(), Value::Array(objective));
    structured.insert(
        "current_state".to_string(),
        Value::Array(vec![Value::from(current_state)]),
    );
    structured.insert(
        "constraints".to_string(),
        Value::Array(previous_constraints(previous_summary)),
    );
    structured.insert("decisions".to_string(), Value::Array(Vec::new()));
    structured.insert("completed".to_string(), Value::Array(completed));
    structured.insert("open_issues".to_string(), Value::Array(Vec::new()));
    structured.insert("artifacts".to_string(), Value::Array(Vec::new()));
    structured.insert("read_files".to_string(), Value::Array(Vec::new()));
    structured.insert("modified_files".to_string(), Value::Array(modified_files));
    structured.insert("failed_attempts".to_string(), Value::Array(failed_attempts));
    structured.insert("excluded_approaches".to_string(), Value::Array(Vec::new()));
    structured.insert("key_concepts".to_string(), Value::Array(Vec::new()));
    structured.insert(
        "problem_solving_process".to_string(),
        Value::Array(Vec::new()),
    );
    structured.insert("user_messages".to_string(), Value::Array(user_messages));
    structured.insert("next_steps".to_string(), Value::Array(Vec::new()));
    structured.insert("exact_evidence".to_string(), Value::Array(exact_evidence));
    Value::Object(structured)
}

/// 上一次摘要里的约束：确定性兜底必须原样保留，否则会被判为「既有约束丢失」。
///
/// 入参是 `compact_summary` 的**载荷**（结构化摘要嵌套在 `structured` 下），与
/// `validate_previous_constraints` 读同一层；结构不符时返回空。
fn previous_constraints(previous_summary: Option<&Value>) -> Vec<Value> {
    let Some(previous_summary) = previous_summary else {
        return Vec::new();
    };
    let structured = previous_summary
        .get("structured")
        .filter(|value| value.is_object())
        .unwrap_or(previous_summary);
    let Some(items) = structured.get("constraints").and_then(Value::as_array) else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| item.as_object())
        .filter_map(|item| {
            let text = item.get("text").and_then(Value::as_str)?;
            let refs = item.get("source_event_ids")?.as_array()?.clone();
            let mut entry = Map::new();
            entry.insert("text".to_string(), Value::from(text));
            entry.insert("source_event_ids".to_string(), Value::Array(refs));
            Some(Value::Object(entry))
        })
        .collect()
}

/// 带来源引用的条目。
fn entry(text: &str, refs: &[String]) -> Value {
    let mut item = Map::new();
    item.insert("text".to_string(), Value::from(text));
    item.insert(
        "source_event_ids".to_string(),
        Value::Array(refs.iter().cloned().map(Value::from).collect()),
    );
    Value::Object(item)
}

/// 单事件条目的便捷封装。
fn single_ref_entry(text: &str, event_id: &str) -> Value {
    let mut item = Map::new();
    item.insert("text".to_string(), Value::from(text));
    item.insert(
        "source_event_ids".to_string(),
        Value::Array(vec![Value::from(event_id)]),
    );
    Value::Object(item)
}

/// 按字符截断并补省略号。
fn excerpt(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    format!("{}…", text.chars().take(limit).collect::<String>())
}

/// 收集被 `tool_call_evicted` 标注淘汰的调用 ID。
fn evicted_call_ids(events: &[SourceEvent]) -> BTreeSet<String> {
    let mut ids: BTreeSet<String> = BTreeSet::new();
    for event in events {
        if event.event_type != EVICTED_EVENT_TYPE {
            continue;
        }
        for call_id in event
            .payload
            .get("evicted_call_ids")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            let call_id = call_id.trim();
            if !call_id.is_empty() {
                ids.insert(call_id.to_string());
            }
        }
    }
    ids
}

/// 丢掉被淘汰调用的工具事件（`tool_call_requested` / `tool_result` / `tool_call_denied`）。
///
/// 淘汰的语义是「这次调用的原文已离开模型上下文」，所以压缩窗口必须与历史投影用同一份口径：
/// 若把已淘汰的工具事件照旧算进待压缩窗口，索引里会列出模型已无从对齐的条目、完整性校验
/// 还会要求模型为它们写 `failed_attempts`，摘要必然被反复判为不合格（实测带淘汰标注的会话
/// 触发压缩 4 次、成功 0 次）。
///
/// 淘汰标注本身不是模型上下文事件，交由调用方的类型过滤处理；没有 `tool_call_id` 的旧事件
/// 无从对应，按未淘汰处理——宁可多留一条，也不静默丢内容。
fn drop_evicted_tool_events(
    candidates: Vec<SourceEvent>,
    all_events: &[SourceEvent],
) -> Vec<SourceEvent> {
    let evicted = evicted_call_ids(all_events);
    if evicted.is_empty() {
        return candidates;
    }
    candidates
        .into_iter()
        .filter(|event| {
            if !matches!(
                event.event_type.as_str(),
                "tool_call_requested" | "tool_result" | "tool_call_denied"
            ) {
                return true;
            }
            let call_id = event
                .payload
                .get("tool_call_id")
                .and_then(Value::as_str)
                .map(str::trim)
                .unwrap_or_default();
            call_id.is_empty() || !evicted.contains(call_id)
        })
        .collect()
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

/// 一批模型可见消息的 Token 估算（含多模态投影，见 [`super::meter`]）。
///
/// 多模态 `content` 里的图片只按句柄计价：Base64 与模型消耗的视觉 Token 无关，算进去会让
/// 阈值判断提前一个数量级触发。
pub fn estimate_messages_tokens(messages: &[Value]) -> i64 {
    super::meter::messages_tokens(messages, &estimate_text_tokens)
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
///
/// 规则本体在 [`super::meter::text_tokens`]：TUI 也按同一份实现计价，两边不会漂。
pub fn estimate_text_tokens(text: &str) -> i64 {
    super::meter::text_tokens(text)
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

#[cfg(test)]
mod prompt_payload_tests {
    use super::*;

    fn event(payload: Value) -> SourceEvent {
        SourceEvent {
            event_id: "e1".to_string(),
            event_type: "user_message".to_string(),
            payload,
        }
    }

    /// 图片的 Base64 不进摘要提示：只留类型与大小，否则摘要输入｛Desensitized:721｝Context 估算都被撑爆。
    #[test]
    fn image_payload_is_summarized_before_reaching_the_summary_prompt() {
        let prompt = event(serde_json::json!({
            "content": "看图",
            "images": [
                {"media_type": "image/png", "data_base64": "AAAAAAAA", "detail": "auto"},
            ],
        }))
        .to_prompt_dict();
        let images = prompt["payload"]["images"].as_array().expect("images 仍在");
        assert_eq!(images.len(), 1);
        assert!(
            images[0].as_str().unwrap_or_default().contains("image/png"),
            "只留类型与大小：{:?}",
            images[0]
        );
        assert!(
            !serde_json::to_string(&prompt).unwrap().contains("AAAAAAAA"),
            "Base64 原文不该出现在提示里"
        );
        assert_eq!(
            prompt["payload"]["content"],
            Value::from("看图"),
            "正文照旧"
        );
    }

    /// 没有图片的事件原样进提示：这条清洗只针对 `images` 字段。
    #[test]
    fn events_without_images_are_untouched() {
        let payload = serde_json::json!({"content": "纯文本", "pending_user_text": "纯文本"});
        let prompt = event(payload.clone()).to_prompt_dict();
        assert_eq!(prompt["payload"], payload);
    }

    /// 索引预览同样走清洗：短预览不能把 Base64 抄进摘要模型的输入。
    #[test]
    fn index_preview_also_hides_the_base64() {
        let index = event(serde_json::json!({
            "content": "看图",
            "images": [{"media_type": "image/png", "data_base64": "ZZZZZZZZ"}],
        }))
        .to_index_dict(200);
        let preview = index["preview"].as_str().unwrap_or_default();
        assert!(
            !preview.contains("ZZZZZZZZ"),
            "预览里不该有 Base64：{preview}"
        );
    }
}

#[cfg(test)]
mod eviction_tests {
    use super::*;

    fn event(event_id: &str, event_type: &str, payload: Value) -> SourceEvent {
        SourceEvent {
            event_id: event_id.to_string(),
            event_type: event_type.to_string(),
            payload,
        }
    }

    fn bash_call(event_id: &str, call_id: &str, ok: bool) -> SourceEvent {
        event(
            event_id,
            "tool_result",
            serde_json::json!({
                "tool": "bash",
                "tool_call_id": call_id,
                "ok": ok,
                "output": "输出",
            }),
        )
    }

    /// 被淘汰调用的工具事件不进待压缩窗口；未淘汰的照旧保留。
    #[test]
    fn evicted_calls_leave_the_compaction_window() {
        let events = vec![
            event("u1", "user_message", serde_json::json!({"content": "问"})),
            event(
                "c1",
                "tool_call_requested",
                serde_json::json!({"tool": "bash", "tool_call_id": "keep"}),
            ),
            bash_call("r1", "keep", false),
            event(
                "c2",
                "tool_call_requested",
                serde_json::json!({"tool": "bash", "tool_call_id": "drop"}),
            ),
            bash_call("r2", "drop", false),
            event(
                "m1",
                "tool_call_evicted",
                serde_json::json!({"evicted_call_ids": ["drop"], "evicted_tool_names": ["bash"]}),
            ),
            event(
                "a1",
                "assistant_message",
                serde_json::json!({"content": "答"}),
            ),
        ];
        let batch = DefaultContextBudgetManager
            .select_batch(&events)
            .expect("有完整回合");
        let ids: Vec<&str> = batch
            .events
            .iter()
            .map(|item| item.event_id.as_str())
            .collect();
        assert!(
            ids.contains(&"c1") && ids.contains(&"r1"),
            "未淘汰的调用保留：{ids:?}"
        );
        assert!(
            !ids.contains(&"c2") && !ids.contains(&"r2"),
            "淘汰调用的请求与结果都必须离开窗口：{ids:?}"
        );
        // 淘汰标注本身不是工具事件，批次照旧带上它（投影与校验都按事件类型自行忽略）。
        assert!(ids.contains(&"m1"), "标注不属于被剔除的对象：{ids:?}");
    }

    /// 没有 `tool_call_id` 的旧事件无从对应，按未淘汰处理（宁可多留，不静默丢内容）。
    #[test]
    fn tool_events_without_call_id_are_kept() {
        let events = vec![
            event("u1", "user_message", serde_json::json!({"content": "问"})),
            event(
                "c1",
                "tool_call_requested",
                serde_json::json!({"tool": "read"}),
            ),
            event(
                "r1",
                "tool_result",
                serde_json::json!({"tool": "read", "ok": true}),
            ),
            event(
                "m1",
                "tool_call_evicted",
                serde_json::json!({"evicted_call_ids": ["x"]}),
            ),
            event(
                "a1",
                "assistant_message",
                serde_json::json!({"content": "答"}),
            ),
        ];
        let batch = DefaultContextBudgetManager
            .select_batch(&events)
            .expect("有完整回合");
        let ids: Vec<&str> = batch
            .events
            .iter()
            .map(|item| item.event_id.as_str())
            .collect();
        assert!(
            ids.contains(&"c1") && ids.contains(&"r1"),
            "无 ID 不参与淘汰：{ids:?}"
        );
    }

    /// 恢复批次的豁免必须与普通批次同一口径，否则超限恢复会把已淘汰原文捞回来。
    #[test]
    fn recovery_batch_also_drops_evicted_calls() {
        let events = vec![
            event("u1", "user_message", serde_json::json!({"content": "问"})),
            event(
                "c1",
                "tool_call_requested",
                serde_json::json!({"tool": "bash", "tool_call_id": "drop"}),
            ),
            bash_call("r1", "drop", false),
            event(
                "m1",
                "tool_call_evicted",
                serde_json::json!({"evicted_call_ids": ["drop"]}),
            ),
            event(
                "u2",
                "user_message",
                serde_json::json!({"content": "还没答完"}),
            ),
        ];
        let batch = DefaultContextBudgetManager
            .select_recovery_batch(&events)
            .expect("最后一条是用户消息时可选");
        let ids: Vec<&str> = batch
            .events
            .iter()
            .map(|item| item.event_id.as_str())
            .collect();
        assert!(
            !ids.contains(&"c1") && !ids.contains(&"r1"),
            "恢复批次同样剔除：{ids:?}"
        );
    }

    /// 确定性兜底产出与模型摘要同形的结构，并覆盖校验关心的三类事实。
    #[test]
    fn deterministic_fallback_covers_the_checked_facts() {
        let events = vec![
            event(
                "u1",
                "user_message",
                serde_json::json!({"content": "目标一"}),
            ),
            event(
                "c1",
                "tool_call_requested",
                serde_json::json!({"tool": "write_file", "tool_call_id": "w1",
                                   "arguments": {"path": "src/a.py"}}),
            ),
            event(
                "r1",
                "tool_result",
                serde_json::json!({"tool": "write_file", "tool_call_id": "w1", "ok": true,
                                   "output": "已写入"}),
            ),
            event(
                "c2",
                "tool_call_requested",
                serde_json::json!({"tool": "bash", "tool_call_id": "b1"}),
            ),
            event(
                "r2",
                "tool_result",
                serde_json::json!({"tool": "bash", "tool_call_id": "b1", "ok": false,
                                   "output": "boom"}),
            ),
            event(
                "a1",
                "assistant_message",
                serde_json::json!({"content": "答"}),
            ),
        ];
        let structured = deterministic_structured_summary(&events, None);
        assert_eq!(structured["objective"][0], Value::from("目标一"));
        assert_eq!(
            structured["user_messages"][0]["text"],
            Value::from("目标一"),
            "用户消息逐字保留"
        );
        assert_eq!(
            structured["modified_files"][0]["path"],
            Value::from("src/a.py"),
            "成功写入的文件必须被覆盖"
        );
        assert_eq!(
            structured["failed_attempts"][0]["source_event_ids"],
            serde_json::json!(["c2", "r2"]),
            "失败调用要带上请求与结果两个事件"
        );
        assert_eq!(
            structured["exact_evidence"][0]["text"],
            Value::from("boom"),
            "失败输出完整时进精确证据"
        );
    }

    /// 上一次摘要的约束必须原样保留，否则兜底摘要会被判为「既有约束丢失」。
    ///
    /// 入参是 `compact_summary` 的载荷形状（结构化摘要嵌在 `structured` 下）。
    #[test]
    fn deterministic_fallback_carries_previous_constraints() {
        let previous = serde_json::json!({
            "structured": {
                "constraints": [{"text": "不要改公开 API", "source_event_ids": ["old"]}],
            },
        });
        let events = vec![event(
            "u1",
            "user_message",
            serde_json::json!({"content": "继续"}),
        )];
        let structured = deterministic_structured_summary(&events, Some(&previous));
        assert_eq!(
            structured["constraints"][0]["text"],
            Value::from("不要改公开 API")
        );
        assert_eq!(
            structured["constraints"][0]["source_event_ids"],
            serde_json::json!(["old"])
        );

        // 旧版纯文本载荷没有结构化约束：按空处理，不报错。
        let legacy = serde_json::json!({"content": "旧摘要"});
        let structured = deterministic_structured_summary(&events, Some(&legacy));
        assert_eq!(structured["constraints"], serde_json::json!([]));
    }
}
