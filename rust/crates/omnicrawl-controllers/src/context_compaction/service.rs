//! `omnicrawl/agent/context_compaction/service.py`：完整回合后的测量、模型摘要、校验、投影和降级编排。
//!
//! 编排只依赖注入件：摘要压缩器端口（SummaryCompactorPort）、账本载荷、摘要校验器、模型上下文投影与占位符守卫。
//! Session 与记忆的读写仍在宿主。

use serde_json::{Map, Value};

use crate::error::AgentError;
use crate::shared::python_round;

use super::ledger::UsageLedger;
use super::policy::{
    estimate_json_tokens, CompactionBatch, ContextBudgetSnapshot, DefaultContextBudgetManager,
    MeasureInput, SourceEvent,
};
use super::projection::{
    event_to_model_message, latest_final_reply_event, render_summary_markdown, ContextAssembler,
};
use super::summary::{ModelSummaryCompactor, ModelSummaryResult, SummaryGenerationError};
use super::validation::{SummaryValidation, SummaryValidationInput, SummaryValidator};

/// 摘要压缩器端口：一次调用产出结构化摘要，失败给出 [`SummaryGenerationError`]。
/// 落库前的占位符守卫：返回无法还原的占位符序号（非空即拒绝落库）。
pub type PlaceholderGuard = dyn Fn(&Value) -> Vec<i64>;

pub trait SummaryCompactorPort {
    fn compact(
        &self,
        batch: &CompactionBatch,
        target_summary_tokens: i64,
        validation_feedback: &[String],
    ) -> Result<ModelSummaryResult, SummaryGenerationError>;
}

impl SummaryCompactorPort for ModelSummaryCompactor {
    fn compact(
        &self,
        batch: &CompactionBatch,
        target_summary_tokens: i64,
        validation_feedback: &[String],
    ) -> Result<ModelSummaryResult, SummaryGenerationError> {
        ModelSummaryCompactor::compact(self, batch, target_summary_tokens, validation_feedback)
    }
}

/// 测量结果：预算快照 + 已序列化的测量事件载荷。
#[derive(Debug, Clone, PartialEq)]
pub struct ContextMeasurementResult {
    pub snapshot: ContextBudgetSnapshot,
    pub event_payload: Value,
}

/// service 的一次完整编排结果；Session I/O 仍由组合根执行。
#[derive(Debug, Clone, PartialEq)]
pub struct ContextCompactionOutcome {
    pub measurement_payload: Value,
    pub compact_payload: Option<Value>,
    pub history_projection: Option<Vec<Value>>,
    pub fallback_required: bool,
    pub diagnostic: String,
}

impl Default for ContextCompactionOutcome {
    fn default() -> Self {
        Self {
            measurement_payload: Value::Object(Map::new()),
            compact_payload: None,
            history_projection: None,
            fallback_required: false,
            diagnostic: String::new(),
        }
    }
}

/// 编排一次压缩流程，不直接读写 Session 或依赖 Agent。
#[derive(Default)]
pub struct ContextCompactionService {
    compactor: Option<Box<dyn SummaryCompactorPort>>,
    budget_manager: DefaultContextBudgetManager,
    validator: SummaryValidator,
    assembler: ContextAssembler,
    ledger: UsageLedger,
    placeholder_guard: Option<Box<PlaceholderGuard>>,
}

impl ContextCompactionService {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_compactor(mut self, compactor: Box<dyn SummaryCompactorPort>) -> Self {
        self.compactor = Some(compactor);
        self
    }

    /// 落库守卫：压缩产物含无法还原的占位符时不把它写进会话 / 记忆。
    pub fn with_placeholder_guard(mut self, guard: Box<PlaceholderGuard>) -> Self {
        self.placeholder_guard = Some(guard);
        self
    }

    pub fn measure_after_complete_turn(
        &self,
        input: MeasureInput,
    ) -> Result<ContextMeasurementResult, AgentError> {
        let snapshot = self.budget_manager.measure(&input)?;
        Ok(ContextMeasurementResult {
            event_payload: self.ledger.measurement_payload(&snapshot, &input.usage),
            snapshot,
        })
    }

    pub fn after_complete_turn(
        &self,
        source_events: &[SourceEvent],
        measure: MeasureInput,
        reasoning_effort: &str,
        preserve_exact_evidence: bool,
    ) -> Result<ContextCompactionOutcome, AgentError> {
        let measured = self.measure_after_complete_turn(measure)?;
        // 「压缩即丢弃」：整个窗口（含最近回合）都交给摘要模型，投影只保留
        // 摘要与最终回复锚点，因此批量选择不再接收保留窗口参数。
        let target_summary_tokens = measured.snapshot.target_summary_tokens;
        let batch = self.budget_manager.select_batch(source_events);
        let decision = DefaultContextBudgetManager::decide_auto_compaction(
            measured.snapshot.clone(),
            batch.as_ref(),
        );
        let mut measurement_payload = match measured.event_payload {
            Value::Object(map) => map,
            _ => Map::new(),
        };
        measurement_payload.insert("auto_decision".to_string(), Value::from(decision.reason));
        let measurement_payload = Value::Object(measurement_payload);
        let batch = match batch {
            Some(batch) if decision.should_compact => batch,
            _ => {
                return Ok(ContextCompactionOutcome {
                    measurement_payload,
                    ..ContextCompactionOutcome::default()
                })
            }
        };
        let retired_token_estimate = std::cmp::max(
            measured.snapshot.potential_retired_tokens,
            prompt_events_tokens(&batch.events),
        );
        self.compact_batch(
            &batch,
            source_events,
            target_summary_tokens,
            reasoning_effort,
            preserve_exact_evidence,
            retired_token_estimate,
            false,
            decision.reason,
            measurement_payload,
        )
    }

    /// 压缩当前未完成回合，并返回可用于同回合重试的上下文投影。
    pub fn recover_from_context_overflow(
        &self,
        source_events: &[SourceEvent],
        target_summary_tokens: i64,
        reasoning_effort: &str,
        preserve_exact_evidence: bool,
    ) -> Result<ContextCompactionOutcome, AgentError> {
        let batch = self.budget_manager.select_recovery_batch(source_events);
        let Some(batch) = batch else {
            return Ok(ContextCompactionOutcome {
                fallback_required: true,
                diagnostic: "没有可安全压缩的上下文超限回合。".to_string(),
                ..ContextCompactionOutcome::default()
            });
        };
        let retired_token_estimate = prompt_events_tokens(&batch.events);
        self.compact_batch(
            &batch,
            source_events,
            target_summary_tokens,
            reasoning_effort,
            preserve_exact_evidence,
            retired_token_estimate,
            false,
            "context_overflow_recovery",
            Value::Object(Map::new()),
        )
    }

    pub fn manual_compact(
        &self,
        source_events: &[SourceEvent],
        target_summary_tokens: i64,
        reasoning_effort: &str,
        preserve_exact_evidence: bool,
    ) -> Result<ContextCompactionOutcome, AgentError> {
        let batch = self.budget_manager.select_batch(source_events);
        let Some(batch) = batch else {
            return Ok(ContextCompactionOutcome {
                diagnostic: "当前会话内容太少，暂不需要模型压缩。".to_string(),
                ..ContextCompactionOutcome::default()
            });
        };
        let retired_token_estimate = prompt_events_tokens(&batch.events);
        self.compact_batch(
            &batch,
            source_events,
            target_summary_tokens,
            reasoning_effort,
            preserve_exact_evidence,
            retired_token_estimate,
            true,
            "manual_model_compaction",
            Value::Object(Map::new()),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn compact_batch(
        &self,
        batch: &CompactionBatch,
        source_events: &[SourceEvent],
        target_summary_tokens: i64,
        reasoning_effort: &str,
        preserve_exact_evidence: bool,
        retired_token_estimate: i64,
        manual: bool,
        decision_reason: &str,
        measurement_payload: Value,
    ) -> Result<ContextCompactionOutcome, AgentError> {
        let Some(compactor) = self.compactor.as_ref() else {
            return Ok(ContextCompactionOutcome {
                measurement_payload,
                fallback_required: true,
                diagnostic: "摘要模型调用器不可用。".to_string(),
                ..ContextCompactionOutcome::default()
            });
        };

        let mut feedback: Vec<String> = Vec::new();
        let mut generation: Option<ModelSummaryResult> = None;
        let mut validation: Option<SummaryValidation> = None;
        for _attempt in 0..2 {
            let generated = match compactor.compact(batch, target_summary_tokens, &feedback) {
                Ok(generated) => generated,
                Err(error) => {
                    return Ok(ContextCompactionOutcome {
                        measurement_payload,
                        fallback_required: true,
                        diagnostic: error.message().to_string(),
                        ..ContextCompactionOutcome::default()
                    })
                }
            };
            let checked = self.validator.validate(&SummaryValidationInput {
                structured: &generated.structured,
                source_events,
                target_summary_tokens,
                previous_summary: batch.previous_summary.as_ref(),
                preserve_exact_evidence,
                completeness_events: Some(&batch.events),
            });
            feedback = checked.errors.clone();
            let valid = checked.valid;
            validation = Some(checked);
            generation = Some(generated);
            if valid {
                break;
            }
        }

        let valid = validation
            .as_ref()
            .map(|checked| checked.valid)
            .unwrap_or(false);
        if generation.is_none() || !valid {
            return Ok(ContextCompactionOutcome {
                measurement_payload,
                fallback_required: true,
                diagnostic: if feedback.is_empty() {
                    "结构化摘要校验失败。".to_string()
                } else {
                    format!("结构化摘要校验失败：{}", feedback.join("; "))
                },
                ..ContextCompactionOutcome::default()
            });
        }

        let generated = generation.expect("已判定存在");
        let checked = validation.expect("已判定通过校验");
        let structured = checked.normalized.unwrap_or(Value::Object(Map::new()));
        let content = render_summary_markdown(&structured);
        let final_reply_event = latest_final_reply_event(source_events);
        // 保留窗口为空（压缩即丢弃）：投影 = 摘要 + 最终回复锚点。
        let projection =
            self.assembler
                .assemble(&structured, &batch.recent_events, final_reply_event);
        let recent_count = projection.len().saturating_sub(1);
        let mut remaining_event_ids: Vec<String> = batch
            .recent_events
            .iter()
            .map(|event| event.event_id.clone())
            .collect();
        if let Some(event) = final_reply_event {
            if !remaining_event_ids.contains(&event.event_id) {
                remaining_event_ids.push(event.event_id.clone());
            }
        }
        let compacted_count = batch
            .events
            .iter()
            .filter(|event| event_to_model_message(event).is_some())
            .count();
        let coverage = coverage_metrics(batch, &structured, retired_token_estimate);

        let mut payload = Map::new();
        payload.insert("schema_version".to_string(), Value::from(2));
        payload.insert("content".to_string(), Value::from(content));
        payload.insert("structured".to_string(), structured);
        payload.insert(
            "covered_event_ids".to_string(),
            Value::Array(
                batch
                    .covered_event_ids()
                    .into_iter()
                    .map(Value::from)
                    .collect(),
            ),
        );
        payload.insert(
            "compacted_event_ids".to_string(),
            Value::Array(
                batch
                    .events
                    .iter()
                    .map(|event| Value::from(event.event_id.clone()))
                    .collect(),
            ),
        );
        payload.insert("coverage".to_string(), coverage);
        payload.insert(
            "retired_token_estimate".to_string(),
            Value::from(retired_token_estimate),
        );
        payload.insert(
            "summary_input_tokens".to_string(),
            Value::from(generated.usage.input_tokens),
        );
        payload.insert(
            "summary_output_tokens".to_string(),
            Value::from(generated.usage.output_tokens),
        );
        payload.insert(
            "cached_input_tokens".to_string(),
            Value::from(generated.usage.cached_input_tokens),
        );
        payload.insert(
            "summary_profile".to_string(),
            Value::from(generated.profile.clone()),
        );
        payload.insert(
            "summary_provider".to_string(),
            Value::from(generated.provider.clone()),
        );
        payload.insert(
            "reasoning_effort".to_string(),
            Value::from(reasoning_effort),
        );
        let mut quality = Map::new();
        quality.insert("schema_valid".to_string(), Value::from(true));
        quality.insert("source_refs_valid".to_string(), Value::from(true));
        quality.insert("critical_facts_checked".to_string(), Value::from(true));
        quality.insert("attempts".to_string(), Value::from(generated.attempts));
        payload.insert("quality".to_string(), Value::Object(quality));
        payload.insert("model_generated".to_string(), Value::from(true));
        payload.insert("manual".to_string(), Value::from(manual));
        payload.insert(
            "single_large_turn".to_string(),
            Value::from(batch.single_large_turn),
        );
        payload.insert("decision_reason".to_string(), Value::from(decision_reason));
        payload.insert(
            "compacted_message_count".to_string(),
            Value::from(compacted_count as i64),
        );
        payload.insert(
            "remaining_message_count".to_string(),
            Value::from(recent_count as i64),
        );
        payload.insert(
            "remaining_event_ids".to_string(),
            Value::Array(remaining_event_ids.into_iter().map(Value::from).collect()),
        );
        payload.insert(
            "final_reply_event_id".to_string(),
            Value::from(
                final_reply_event
                    .map(|event| event.event_id.clone())
                    .unwrap_or_default(),
            ),
        );
        let compact_payload = Value::Object(payload);

        if let Some(guard) = self.placeholder_guard.as_ref() {
            let unresolved = guard(&compact_payload);
            if !unresolved.is_empty() {
                // 落库前拦截：把无法还原的占位符写进会话 / 记忆会让占位符长期显示。
                let numbers: Vec<String> = unresolved.iter().map(|seq| seq.to_string()).collect();
                return Ok(ContextCompactionOutcome {
                    measurement_payload,
                    fallback_required: true,
                    diagnostic: format!(
                        "压缩产物含无法还原的脱敏占位符（序号 {}），已丢弃本次模型摘要。",
                        numbers.join(", ")
                    ),
                    ..ContextCompactionOutcome::default()
                });
            }
        }

        Ok(ContextCompactionOutcome {
            measurement_payload,
            compact_payload: Some(compact_payload),
            history_projection: Some(projection),
            fallback_required: false,
            diagnostic: String::new(),
        })
    }
}

/// 批次内事件按「进模型的形状」估算 Token：与预算快照的口径一致。
fn prompt_events_tokens(events: &[SourceEvent]) -> i64 {
    estimate_json_tokens(&Value::Array(
        events.iter().map(|event| event.to_prompt_dict()).collect(),
    ))
}

/// 本次压缩的覆盖度指标：本批事件有多少被摘要显式引用。
///
/// coverage_ratio 反映「该记的没记」风险：引用越少，后续恢复越依赖归档 / 证据工具。
/// 字段计数用于确认完整性校验确实产出了条目。
fn coverage_metrics(
    batch: &CompactionBatch,
    structured: &Value,
    retired_token_estimate: i64,
) -> Value {
    let compacted_ids: Vec<String> = batch
        .events
        .iter()
        .map(|event| event.event_id.clone())
        .collect();
    let covered_ids = batch.covered_event_ids();
    let mut referenced: Vec<String> = Vec::new();

    fn visit(value: &Value, compacted_ids: &[String], referenced: &mut Vec<String>) {
        match value {
            Value::Object(map) => {
                if let Some(refs) = map.get("source_event_ids").and_then(Value::as_array) {
                    for event_id in refs {
                        if let Some(event_id) = event_id.as_str() {
                            if compacted_ids.iter().any(|item| item == event_id)
                                && !referenced.iter().any(|item| item == event_id)
                            {
                                referenced.push(event_id.to_string());
                            }
                        }
                    }
                }
                for nested in map.values() {
                    visit(nested, compacted_ids, referenced);
                }
            }
            Value::Array(items) => {
                for nested in items {
                    visit(nested, compacted_ids, referenced);
                }
            }
            _ => {}
        }
    }

    visit(structured, &compacted_ids, &mut referenced);
    let compacted_count = compacted_ids.len();
    let referenced_count = referenced.len();
    let covered_count = compacted_ids
        .iter()
        .filter(|event_id| covered_ids.contains(event_id))
        .count();

    const COUNTED_FIELDS: [&str; 14] = [
        "constraints",
        "decisions",
        "completed",
        "open_issues",
        "artifacts",
        "read_files",
        "modified_files",
        "failed_attempts",
        "excluded_approaches",
        "key_concepts",
        "problem_solving_process",
        "user_messages",
        "next_steps",
        "exact_evidence",
    ];
    let mut field_counts = Map::new();
    for field in COUNTED_FIELDS {
        let count = structured
            .get(field)
            .and_then(Value::as_array)
            .map(|items| items.len())
            .unwrap_or(0);
        field_counts.insert(field.to_string(), Value::from(count as i64));
    }

    let mut coverage = Map::new();
    coverage.insert(
        "compacted_event_count".to_string(),
        Value::from(compacted_count as i64),
    );
    coverage.insert(
        "covered_event_count".to_string(),
        Value::from(covered_count as i64),
    );
    coverage.insert(
        "referenced_event_count".to_string(),
        Value::from(referenced_count as i64),
    );
    coverage.insert(
        "coverage_ratio".to_string(),
        Value::from(if compacted_count == 0 {
            0.0
        } else {
            python_round(referenced_count as f64 / compacted_count as f64, 4)
        }),
    );
    coverage.insert(
        "retired_token_estimate".to_string(),
        Value::from(retired_token_estimate),
    );
    coverage.insert("field_counts".to_string(), Value::Object(field_counts));
    Value::Object(coverage)
}
