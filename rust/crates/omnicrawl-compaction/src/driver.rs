//! `omnicrawl/agent/controllers/turn/compaction.py` 的会话与记忆编排。
//!
//! 顺序与 Python 一致：测量 → 触发判定 → 摘要 → 归档被压缩窗口 → 落盘压缩事件与摘要 →
//! 记忆回写与自动召回 → 重建运行期历史 → 生成可见的压缩边界提示。

use std::sync::Arc;

use serde_json::{json, Map, Value};

use omnicrawl_controllers::context_compaction::{
    estimate_json_tokens, ContextCompactionService, MeasureInput, SourceEvent, TokenUsageSample,
};
use omnicrawl_controllers::shared::CONTEXT_OVERFLOW_RECOVERY_PROMPT;
use omnicrawl_controllers::turn::compaction::{
    archive_compacted_event_ids, compaction_memory_requests, compaction_recall_event_payload,
    compaction_recall_hits, compaction_recall_query, compaction_recall_text,
    format_compaction_notice, RECALL_MAX_RESULTS,
};
use omnicrawl_session::{project_compaction_boundary_history, utc_now, MemoryStore, SessionStore};

/// 压缩策略与阈值：对应 Python 的 `config.context_compaction` 加模型上下文窗口。
#[derive(Debug, Clone)]
pub struct CompactionConfig {
    pub recent_turns: i64,
    pub target_summary_tokens: i64,
    pub next_user_reserve_tokens: i64,
    pub trigger_context_tokens: i64,
    pub context_window_tokens: i64,
    pub emergency_context_ratio: f64,
    pub reasoning_effort: String,
    pub preserve_exact_evidence: bool,
    pub archive_compacted_events: bool,
    pub auto_memory_recall: bool,
}

impl Default for CompactionConfig {
    fn default() -> Self {
        Self {
            recent_turns: 3,
            target_summary_tokens: 2000,
            next_user_reserve_tokens: 4000,
            trigger_context_tokens: 100_000,
            context_window_tokens: 128_000,
            emergency_context_ratio: 0.9,
            reasoning_effort: "medium".to_string(),
            preserve_exact_evidence: true,
            archive_compacted_events: true,
            auto_memory_recall: true,
        }
    }
}

/// 一次回合结束边界的事实源，由宿主提供。
pub struct TurnBoundary<'a> {
    pub session_id: &'a str,
    pub system_prompt: &'a str,
    pub context_messages: &'a [Value],
    pub history_messages: &'a [Value],
    pub tool_schemas: &'a [Value],
    pub usage: TokenUsageSample,
    pub last_request_input_tokens: i64,
}

/// 回合结束边界的处理结果。
#[derive(Debug, Clone, Default)]
pub struct AfterTurnReport {
    pub measurement_payload: Value,
    pub compacted: bool,
    pub notice: Option<String>,
    /// 压缩发生时的运行期历史（摘要 + 召回注入 + 保留窗口）。
    pub history: Option<Vec<Value>>,
    pub diagnostic: String,
    /// 摘要正文（`compact_summary` 载荷的 `content`）：显式压缩要回给客户端。
    pub summary: String,
}

/// 上下文压缩驱动：测量、压缩、归档、记忆回写、自动召回与历史重建。
pub struct CompactionDriver {
    store: Arc<SessionStore>,
    service: ContextCompactionService,
    config: CompactionConfig,
    memory: Option<MemoryStore>,
}

impl CompactionDriver {
    pub fn new(
        store: Arc<SessionStore>,
        service: ContextCompactionService,
        config: CompactionConfig,
    ) -> Self {
        Self {
            store,
            service,
            config,
            memory: None,
        }
    }

    /// 记忆落点：会话级记忆（缺省时由宿主换成项目级）。
    pub fn with_memory(mut self, memory: MemoryStore) -> Self {
        self.memory = Some(memory);
        self
    }

    /// 回合结束边界：实际上下文达到阈值时压缩，并返回可见提示与重建后的历史。
    pub fn after_turn(&self, boundary: &TurnBoundary<'_>) -> Result<AfterTurnReport, String> {
        let events = self.source_events(boundary.session_id)?;
        let measure = MeasureInput {
            system_prompt: boundary.system_prompt.to_string(),
            context_messages: boundary.context_messages.to_vec(),
            history_messages: boundary.history_messages.to_vec(),
            tool_schemas: boundary.tool_schemas.to_vec(),
            recent_turns: self.config.recent_turns,
            target_summary_tokens: self.config.target_summary_tokens,
            next_user_reserve_tokens: self.config.next_user_reserve_tokens,
            trigger_context_tokens: self.config.trigger_context_tokens,
            context_window_tokens: self.config.context_window_tokens,
            usage: boundary.usage,
            provider_input_tokens: std::cmp::max(0, boundary.last_request_input_tokens),
            emergency_context_ratio: self.config.emergency_context_ratio,
        };
        let outcome = self
            .service
            .after_complete_turn(
                &events,
                measure,
                self.config.reasoning_effort.as_str(),
                self.config.preserve_exact_evidence,
            )
            .map_err(|error| error.message().to_string())?;

        let mut report = AfterTurnReport {
            measurement_payload: outcome.measurement_payload.clone(),
            diagnostic: outcome.diagnostic.clone(),
            ..AfterTurnReport::default()
        };
        let Some(compact_payload) = outcome.compact_payload.clone() else {
            // 未触发压缩的回合同样写入测量事件：转录与投影依赖逐回合的上下文计量。
            self.append_event(
                boundary.session_id,
                "context_compaction_measurement",
                &report.measurement_payload,
            )?;
            if outcome.fallback_required {
                // 模型摘要失败：记一条失败事件，让转录留下「本回合曾经尝试压缩」。
                self.append_event(
                    boundary.session_id,
                    "context_compaction_failed",
                    &json!({"mode": "automatic_model", "reason": outcome.diagnostic}),
                )?;
            }
            return Ok(report);
        };

        self.apply_compaction(
            boundary.session_id,
            &compact_payload,
            boundary.history_messages,
            &mut report,
        )?;
        Ok(report)
    }

    /// 显式压缩（`session.compact` 命令）：不做阈值判定，直接请求一次模型摘要。
    ///
    /// 与回合结束边界的差别只有触发方式；载荷落盘、归档、记忆回写、召回与历史重建共用
    /// [`CompactionDriver::apply_compaction`]，两条路径不会漂移。摘要在 `report.summary`。
    pub fn manual_compact(&self, session_id: &str) -> Result<AfterTurnReport, String> {
        let events = self.source_events(session_id)?;
        let outcome = self
            .service
            .manual_compact(
                &events,
                self.config.target_summary_tokens,
                self.config.reasoning_effort.as_str(),
                self.config.preserve_exact_evidence,
            )
            .map_err(|error| error.message().to_string())?;

        let mut report = AfterTurnReport {
            measurement_payload: outcome.measurement_payload.clone(),
            diagnostic: outcome.diagnostic.clone(),
            ..AfterTurnReport::default()
        };
        let Some(compact_payload) = outcome.compact_payload.clone() else {
            let reason = if outcome.diagnostic.is_empty() {
                "当前会话内容太少，暂不需要压缩。".to_string()
            } else {
                outcome.diagnostic.clone()
            };
            // 与 Python 的 `manual_model` 模式一致：失败也留事件，便于诊断。
            self.append_event(
                session_id,
                "context_compaction_failed",
                &json!({"mode": "manual_model", "reason": reason}),
            )?;
            return Err(reason);
        };
        self.apply_compaction(session_id, &compact_payload, &[], &mut report)?;
        Ok(report)
    }

    /// 压缩载荷的收尾：归档、事件、记忆回写、召回、历史重建与提示文案。
    ///
    /// `history_messages` 只在测量载荷缺少 `estimated_next_input_tokens` 时用于估算压缩前的
    /// Token 数（回合结束边界传当轮历史；显式压缩没有当轮历史，传空）。
    fn apply_compaction(
        &self,
        session_id: &str,
        compact_payload: &Value,
        history_messages: &[Value],
        report: &mut AfterTurnReport,
    ) -> Result<(), String> {
        let mut payload = compact_payload.as_object().cloned().unwrap_or_default();
        // 归档被压缩窗口的原始事件（二级存储），并把 archive_id 回写事件。
        if let Some((archive_id, archived_count)) = self.archive_events(session_id, &payload)? {
            payload.insert("archive_id".to_string(), Value::from(archive_id.clone()));
            if let Some(measurement) = report.measurement_payload.as_object_mut() {
                measurement.insert("archive_id".to_string(), Value::from(archive_id));
                measurement.insert(
                    "archived_event_count".to_string(),
                    Value::from(archived_count),
                );
            }
        }
        if let Some(coverage) = payload.get("coverage").cloned() {
            if coverage.is_object() {
                if let Some(measurement) = report.measurement_payload.as_object_mut() {
                    measurement.insert("coverage".to_string(), coverage);
                }
            }
        }
        let payload = Value::Object(payload);
        self.append_event(
            session_id,
            "context_compaction_measurement",
            &report.measurement_payload,
        )?;
        self.append_event(session_id, "compact_summary", &payload)?;
        self.write_memories(&payload);
        let recall_text = self.remember_recall(&payload, session_id);
        let history = self.rebuild_history(&payload, session_id, recall_text.as_deref())?;
        // after 一律用替换后的真实历史计算：无预算上限模式下模拟值会误导显示。
        let before_tokens = report
            .measurement_payload
            .get("estimated_next_input_tokens")
            .and_then(Value::as_i64)
            .unwrap_or_else(|| estimate_json_tokens(&Value::Array(history_messages.to_vec())));
        let after_tokens = estimate_json_tokens(&Value::Array(history.clone()));
        report.notice = format_compaction_notice(Some(before_tokens), Some(after_tokens));
        report.summary = payload
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        report.history = Some(history);
        report.compacted = true;
        Ok(())
    }

    /// 上下文超限后的恢复：压缩当前未完成回合，返回可直接重试的历史。
    ///
    /// 与回合结束边界不同：这里额外落「恢复提示」用户消息与
    /// `context_overflow_recovery` 事件，历史末尾追加该提示，让模型基于摘要继续。
    pub fn recover_after_overflow(
        &self,
        session_id: &str,
        previous_history: &[Value],
    ) -> Result<AfterTurnReport, String> {
        let events = self.source_events(session_id)?;
        let outcome = self
            .service
            .recover_from_context_overflow(
                &events,
                self.config.target_summary_tokens,
                &self.config.reasoning_effort,
                self.config.preserve_exact_evidence,
            )
            .map_err(|error| error.message().to_string())?;
        let Some(compact_payload) = outcome.compact_payload.clone() else {
            let diagnostic = if outcome.diagnostic.is_empty() {
                "模型摘要未生成可用投影。".to_string()
            } else {
                outcome.diagnostic.clone()
            };
            self.append_event(
                session_id,
                "context_overflow_recovery_failed",
                &json!({"reason": diagnostic}),
            )?;
            return Ok(AfterTurnReport {
                measurement_payload: Value::Null,
                compacted: false,
                notice: None,
                history: None,
                diagnostic,
                summary: String::new(),
            });
        };

        let mut payload = compact_payload.as_object().cloned().unwrap_or_default();
        let archive_id = match self.archive_events(session_id, &payload)? {
            Some((archive_id, _count)) => {
                payload.insert("archive_id".to_string(), Value::from(archive_id.clone()));
                archive_id
            }
            None => String::new(),
        };
        let single_large_turn = payload
            .get("single_large_turn")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let payload = Value::Object(payload);
        self.append_event(session_id, "compact_summary", &payload)?;
        self.write_memories(&payload);
        let recall_text = self.remember_recall(&payload, session_id);
        self.append_event(
            session_id,
            "context_overflow_recovery",
            &json!({
                "mode": "model_summary",
                "decision_reason": "context_overflow_recovery",
                "single_large_turn": single_large_turn,
                "archive_id": archive_id,
            }),
        )?;
        self.append_event(
            session_id,
            "user_message",
            &json!({"content": CONTEXT_OVERFLOW_RECOVERY_PROMPT}),
        )?;

        let mut history = self.rebuild_history(&payload, session_id, recall_text.as_deref())?;
        history.push(json!({"role": "user", "content": CONTEXT_OVERFLOW_RECOVERY_PROMPT}));
        let before_tokens = estimate_json_tokens(&Value::Array(previous_history.to_vec()));
        let after_tokens = estimate_json_tokens(&Value::Array(history.clone()));
        Ok(AfterTurnReport {
            measurement_payload: Value::Null,
            compacted: true,
            notice: format_compaction_notice(Some(before_tokens), Some(after_tokens)),
            history: Some(history),
            diagnostic: String::new(),
            summary: String::new(),
        })
    }

    fn source_events(&self, session_id: &str) -> Result<Vec<SourceEvent>, String> {
        let events = self
            .store
            .read_active_events(session_id)
            .map_err(|error| error.message().to_string())?;
        Ok(events
            .into_iter()
            .map(|event| SourceEvent {
                event_id: event.event_id.clone(),
                event_type: event.event_type.clone(),
                payload: Value::Object(event.payload.clone()),
            })
            .collect())
    }

    fn append_event(
        &self,
        session_id: &str,
        event_type: &str,
        payload: &Value,
    ) -> Result<(), String> {
        let payload = payload.as_object().cloned().unwrap_or_default();
        self.store
            .append_event(session_id, event_type, payload, None, utc_now())
            .map(|_| ())
            .map_err(|error| error.message().to_string())
    }

    /// 归档本次被压缩的事件；未启用、没有事件或事件流里找不到目标时返回 None。
    fn archive_events(
        &self,
        session_id: &str,
        payload: &Map<String, Value>,
    ) -> Result<Option<(String, i64)>, String> {
        let wanted = archive_compacted_event_ids(
            self.config.archive_compacted_events,
            &Value::Object(payload.clone()),
        );
        if wanted.is_empty() {
            return Ok(None);
        }
        let raw: Vec<Value> = self
            .store
            .read_active_events(session_id)
            .map_err(|error| error.message().to_string())?
            .into_iter()
            .filter(|event| wanted.contains(&event.event_id))
            .map(|event| event.to_dict())
            .collect();
        if raw.is_empty() {
            return Ok(None);
        }
        let archive_id = self
            .store
            .archive_compacted_events(session_id, &raw, None, utc_now())
            .map_err(|error| error.message().to_string())?;
        Ok(Some((archive_id, wanted.len() as i64)))
    }

    /// 压缩结果写入当前会话级记忆；失败不影响压缩。
    fn write_memories(&self, payload: &Value) {
        let Some(memory) = self.memory.as_ref() else {
            return;
        };
        let requests = compaction_memory_requests(payload);
        if requests.is_empty() {
            return;
        }
        let _ = memory.write(&requests);
    }

    /// 压缩后自动检索长期记忆：命中即记录事件并返回要注入的文本。
    ///
    /// 与 Python 一致的两条提前返回：没有结构化摘要，或检索结果为空。
    fn remember_recall(&self, payload: &Value, session_id: &str) -> Option<String> {
        if !self.config.auto_memory_recall {
            return None;
        }
        let memory = self.memory.as_ref()?;
        let query = compaction_recall_query(payload)?;
        let results = memory.search(&query, &[], RECALL_MAX_RESULTS as u32).ok()?;
        if results.is_empty() {
            return None;
        }
        let hits = compaction_recall_hits(&results);
        let event_payload = compaction_recall_event_payload(&query, &hits);
        let _ = self.append_event(session_id, "compaction_memory_recall", &event_payload);
        let text = compaction_recall_text(&results);
        if text.is_empty() {
            return None;
        }
        Some(text)
    }

    /// 用压缩边界重建运行期历史，并把召回结果紧跟摘要注入。
    fn rebuild_history(
        &self,
        payload: &Value,
        session_id: &str,
        recall_text: Option<&str>,
    ) -> Result<Vec<Value>, String> {
        let events = self
            .store
            .read_active_events(session_id)
            .map_err(|error| error.message().to_string())?;
        let summary = payload.as_object().cloned().unwrap_or_default();
        let mut history = project_compaction_boundary_history(&summary, &events);
        if let Some(text) = recall_text {
            if !history.is_empty() {
                let mut message = Map::new();
                message.insert("role".to_string(), Value::from("assistant"));
                message.insert("content".to_string(), Value::from(text));
                history.insert(1, Value::Object(message));
            }
        }
        Ok(history)
    }
}
