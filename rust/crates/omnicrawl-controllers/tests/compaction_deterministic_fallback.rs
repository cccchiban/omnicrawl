//! 确定性兜底：模型摘要失败或校验不通过时，压缩仍然产出一份可用的摘要。
//!
//! 配置 `context_compaction.failure_fallback` 只允许 `deterministic`，但此前只有配置解析
//! 在读它——没有任何消费方，于是「摘要失败」等于「本回合完全不压缩」，上下文只会继续涨，
//! 下一回合又原样触发一次。本套件钉住兜底真的接在两条失败路径上。

use serde_json::{json, Value};

use omnicrawl_controllers::context_compaction::{
    deterministic_structured_summary, CompactionBatch, ContextCompactionService, MeasureInput,
    ModelSummaryCompactor, ModelSummaryResult, SourceEvent, SummaryCompactorPort,
    SummaryGenerationError, SummaryModelCall, SummaryModelResponse, TokenUsageSample,
};

const SUMMARY_PROMPT: &str = "你是会话状态压缩器。";

fn events() -> Vec<SourceEvent> {
    let raw = vec![
        ("u1", "user_message", json!({"content": "把配置项接上"})),
        (
            "c1",
            "tool_call_requested",
            json!({"tool": "write_file", "tool_call_id": "w1", "arguments": {"path": "src/a.rs"}}),
        ),
        (
            "r1",
            "tool_result",
            json!({"tool": "write_file", "tool_call_id": "w1", "ok": true, "output": "已写入"}),
        ),
        (
            "c2",
            "tool_call_requested",
            json!({"tool": "bash", "tool_call_id": "b1"}),
        ),
        (
            "r2",
            "tool_result",
            json!({"tool": "bash", "tool_call_id": "b1", "ok": false, "output": "boom"}),
        ),
        ("a1", "assistant_message", json!({"content": "改完了"})),
    ];
    raw.into_iter()
        .map(|(event_id, event_type, payload)| SourceEvent {
            event_id: event_id.to_string(),
            event_type: event_type.to_string(),
            payload,
        })
        .collect()
}

fn measure() -> MeasureInput {
    MeasureInput {
        system_prompt: "系统提示词".to_string(),
        context_messages: vec![],
        history_messages: vec![json!({"role": "user", "content": "把配置项接上"})],
        tool_schemas: vec![],
        recent_turns: 1,
        target_summary_tokens: 0,
        next_user_reserve_tokens: 100,
        trigger_context_tokens: 1,
        context_window_tokens: 200_000,
        usage: TokenUsageSample::default(),
        provider_input_tokens: 0,
        emergency_context_ratio: 0.9,
        nodes: Vec::new(),
    }
}

/// 恒失败的摘要模型调用。
struct FailingModel;

impl SummaryModelCall for FailingModel {
    fn call(&self, _messages: &[Value]) -> Result<SummaryModelResponse, SummaryGenerationError> {
        Err(SummaryGenerationError::new(
            "摘要模型请求失败：upstream 503",
        ))
    }
}

/// 恒返回不合格结构的摘要模型调用（引用不存在的事件）。
struct InvalidModel;

impl SummaryModelCall for InvalidModel {
    fn call(&self, _messages: &[Value]) -> Result<SummaryModelResponse, SummaryGenerationError> {
        Ok(SummaryModelResponse::new(
            json!({"objective": ["目标"], "decisions": [{"text": "决策", "source_event_ids": ["nope"]}]})
                .to_string(),
        ))
    }
}

/// 恒产出合法结构化摘要的压缩器（用于确认兜底不会顶掉正常路径）。
///
/// 结构由兜底生成器搭出骨架再改掉目标：这样它必然过校验，测试只关注「模型路径没被顶掉」。
struct GoodCompactor;

impl SummaryCompactorPort for GoodCompactor {
    fn compact(
        &self,
        batch: &CompactionBatch,
        _target_summary_tokens: i64,
        _feedback: &[String],
    ) -> Result<ModelSummaryResult, SummaryGenerationError> {
        let mut structured = deterministic_structured_summary(&batch.events, None);
        structured["objective"] = json!(["模型给的目标"]);
        Ok(ModelSummaryResult {
            structured,
            usage: TokenUsageSample::default(),
            profile: "fake".to_string(),
            provider: "kernel".to_string(),
            attempts: 1,
        })
    }
}

fn service_with_model(model: Box<dyn SummaryModelCall>) -> ContextCompactionService {
    let compactor =
        ModelSummaryCompactor::new(model, SUMMARY_PROMPT, 64_000).expect("压缩器可构造");
    ContextCompactionService::new()
        .with_compactor(Box::new(compactor))
        .with_deterministic_fallback(true)
}

/// 模型请求失败：兜底接手，压缩仍然落地。
#[test]
fn model_error_falls_back_to_deterministic_summary() {
    let outcome = service_with_model(Box::new(FailingModel))
        .after_complete_turn(&events(), measure(), "low", false)
        .expect("编排不抛错");

    let payload = outcome.compact_payload.expect("兜底后应有压缩载荷");
    assert_eq!(
        payload["model_generated"],
        Value::Bool(false),
        "兜底产物必须标为非模型生成"
    );
    assert_eq!(
        payload["structured"]["user_messages"][0]["text"],
        Value::from("把配置项接上"),
        "用户消息逐字保留"
    );
    assert_eq!(
        payload["structured"]["modified_files"][0]["path"],
        Value::from("src/a.rs"),
        "成功写入的文件被覆盖"
    );
    assert_eq!(
        payload["structured"]["failed_attempts"][0]["source_event_ids"],
        json!(["c2", "r2"]),
        "失败调用被覆盖"
    );
}

/// 模型返回的结构校验不通过：兜底接手，并且不再上报 fail-closed。
#[test]
fn invalid_structure_falls_back_to_deterministic_summary() {
    let outcome = service_with_model(Box::new(InvalidModel))
        .after_complete_turn(&events(), measure(), "low", false)
        .expect("编排不抛错");

    assert!(
        !outcome.fallback_required,
        "兜底成功就不该再标降级：{}",
        outcome.diagnostic
    );
    let payload = outcome.compact_payload.expect("兜底后应有压缩载荷");
    assert_eq!(payload["model_generated"], Value::Bool(false));
    assert!(outcome.history_projection.is_some(), "压缩后要有上下文投影");
}

/// 关掉兜底时保持旧行为：失败即降级上报，不产出压缩载荷。
#[test]
fn fallback_disabled_keeps_the_fail_closed_behaviour() {
    let compactor =
        ModelSummaryCompactor::new(Box::new(FailingModel), SUMMARY_PROMPT, 64_000).expect("可构造");
    let outcome = ContextCompactionService::new()
        .with_compactor(Box::new(compactor))
        .after_complete_turn(&events(), measure(), "low", false)
        .expect("编排不抛错");

    assert!(outcome.fallback_required, "关掉兜底时仍按失败上报");
    assert!(outcome.compact_payload.is_none());
    assert!(
        outcome.diagnostic.contains("摘要模型请求失败"),
        "诊断保留模型侧原因：{}",
        outcome.diagnostic
    );
}

/// 模型摘要正常时必须原样采用，兜底不参与。
#[test]
fn healthy_model_summary_is_not_replaced_by_the_fallback() {
    let service = ContextCompactionService::new()
        .with_compactor(Box::new(GoodCompactor))
        .with_deterministic_fallback(true);
    let outcome = service
        .after_complete_turn(&events(), measure(), "low", false)
        .expect("编排不抛错");

    let payload = outcome.compact_payload.expect("正常路径应产出载荷");
    assert_eq!(payload["model_generated"], Value::Bool(true));
    assert_eq!(
        payload["structured"]["objective"][0],
        Value::from("模型给的目标")
    );
}
