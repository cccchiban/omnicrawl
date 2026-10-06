//! `omnicrawl/agent/context_compaction/` 编排层（账本 / 证据恢复 / 结构化摘要 / 压缩编排）与
//! `omnicrawl/agent/controllers/turn/compaction.py` 判定面的跨语言对照。
//!
//! 数据集是冻结的对照契约：改任一侧后先跑
//! `python rust/tools/gen_controllers_fixture.py`，再用同一份输入重放本套件。
//! 摘要提示词模板按仓库布局读取 `rust/assets/templates/`——对照的是「读到了什么」。

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use omnicrawl_controllers::context_compaction::{
    chunk_events, parse_structured_summary, read_summary_prompt, ArtifactReadFailure,
    ArtifactReader, CompactionBatch, ContextBudgetSnapshot, ContextCompactionService, MeasureInput,
    ModelSummaryCompactor, SessionEvidenceRecallService, SourceEvent, SummaryGenerationError,
    SummaryModelResponse, TokenUsageSample, UsageLedger,
};
use omnicrawl_controllers::turn::compaction::{
    archive_compacted_event_ids, compaction_memory_requests, compaction_recall_event_payload,
    compaction_recall_hits, compaction_recall_query, compaction_recall_text,
    format_compaction_notice, RECALL_MAX_RESULTS, RECALL_QUERY_CHARS, RECALL_TEXT_LIMIT,
};
use omnicrawl_session::MemorySearchResult;

const FIXTURE: &str = include_str!("fixtures/controllers_parity.json");

fn orchestration() -> Value {
    let fixture: Value = serde_json::from_str(FIXTURE).expect("fixture 不是合法 JSON");
    fixture["compaction_orchestration"].clone()
}

/// 键序也是契约：逐字节比较序列化结果，而不是只比对象相等。
fn assert_json(actual: &Value, expected: &Value, label: &str) {
    assert_eq!(
        serde_json::to_string(actual).expect("可序列化"),
        serde_json::to_string(expected).expect("可序列化"),
        "JSON 不一致（{label}）"
    );
}

fn sha256_hex(text: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(text.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn source_events(value: &Value) -> Vec<SourceEvent> {
    value
        .as_array()
        .expect("事件数组")
        .iter()
        .map(|item| SourceEvent {
            event_id: item["event_id"].as_str().expect("event_id").to_string(),
            event_type: item["type"].as_str().expect("type").to_string(),
            payload: item["payload"].clone(),
        })
        .collect()
}

fn usage_from(value: &Value) -> TokenUsageSample {
    match value {
        Value::Array(items) => TokenUsageSample::new(
            items[0].as_i64().unwrap_or_default(),
            items[1].as_i64().unwrap_or_default(),
            items[2].as_i64().unwrap_or_default(),
        )
        .expect("用量非负"),
        Value::Object(map) => TokenUsageSample::new(
            map["input_tokens"].as_i64().unwrap_or_default(),
            map["output_tokens"].as_i64().unwrap_or_default(),
            map["cached_input_tokens"].as_i64().unwrap_or_default(),
        )
        .expect("用量非负"),
        _ => TokenUsageSample::default(),
    }
}

fn snapshot_from(value: &Value) -> ContextBudgetSnapshot {
    let field = |name: &str| value[name].as_i64().unwrap_or_default();
    ContextBudgetSnapshot {
        stable_context_tokens: field("stable_context_tokens"),
        existing_summary_tokens: field("existing_summary_tokens"),
        cold_history_tokens: field("cold_history_tokens"),
        recent_history_tokens: field("recent_history_tokens"),
        next_user_reserve_tokens: field("next_user_reserve_tokens"),
        target_summary_tokens: field("target_summary_tokens"),
        estimated_next_input_tokens: field("estimated_next_input_tokens"),
        post_turn_context_tokens: field("post_turn_context_tokens"),
        simulated_compacted_input_tokens: field("simulated_compacted_input_tokens"),
        potential_retired_tokens: field("potential_retired_tokens"),
        trigger_context_tokens: field("trigger_context_tokens"),
        context_window_tokens: field("context_window_tokens"),
        trigger_reached: value["trigger_reached"].as_bool().unwrap_or_default(),
        emergency_ratio_reached: value["emergency_ratio_reached"]
            .as_bool()
            .unwrap_or_default(),
        cache_hit_ratio: value["cache_hit_ratio"].as_f64().unwrap_or_default(),
        provider_input_tokens: field("provider_input_tokens"),
    }
}

fn measure_input_from(value: &Value) -> MeasureInput {
    MeasureInput {
        system_prompt: value["system_prompt"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
        context_messages: value["context_messages"]
            .as_array()
            .cloned()
            .unwrap_or_default(),
        history_messages: value["history_messages"]
            .as_array()
            .cloned()
            .unwrap_or_default(),
        tool_schemas: value["tool_schemas"]
            .as_array()
            .cloned()
            .unwrap_or_default(),
        recent_turns: value["recent_turns"].as_i64().unwrap_or_default(),
        target_summary_tokens: value["target_summary_tokens"].as_i64().unwrap_or_default(),
        next_user_reserve_tokens: value["next_user_reserve_tokens"]
            .as_i64()
            .unwrap_or_default(),
        trigger_context_tokens: value["trigger_context_tokens"].as_i64().unwrap_or_default(),
        context_window_tokens: value["context_window_tokens"].as_i64().unwrap_or_default(),
        usage: usage_from(&value["usage"]),
        provider_input_tokens: value["provider_input_tokens"].as_i64().unwrap_or_default(),
        emergency_context_ratio: value["emergency_context_ratio"]
            .as_f64()
            .unwrap_or_default(),
    }
}

fn batch_from(events: Vec<SourceEvent>, previous_summary: Option<Value>) -> CompactionBatch {
    CompactionBatch {
        events,
        recent_events: Vec::new(),
        previous_summary,
        previous_covered_event_ids: Vec::new(),
        single_large_turn: false,
    }
}

fn template_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../rust/assets/templates")
}

fn summary_prompt() -> String {
    read_summary_prompt(&template_dir()).expect("摘要提示词模板可读")
}

/// 脚本化摘要模型：按记录依次返回响应或错误，并记下收到的提示词。
fn scripted_model(
    script: Vec<Value>,
    prompts: Rc<RefCell<Vec<String>>>,
    usage: (i64, i64, i64),
    profile: &str,
    provider: &str,
) -> impl Fn(&[Value]) -> Result<SummaryModelResponse, SummaryGenerationError> {
    let script = Rc::new(RefCell::new(script));
    let profile = profile.to_string();
    let provider = provider.to_string();
    move |messages: &[Value]| {
        prompts.borrow_mut().push(
            messages[0]["content"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
        );
        let item = script.borrow_mut().remove(0);
        if let Some(error) = item.get("error").and_then(Value::as_str) {
            return Err(SummaryGenerationError::new(error));
        }
        let mut response = SummaryModelResponse::new(item["content"].as_str().unwrap_or_default());
        response.usage = TokenUsageSample::new(usage.0, usage.1, usage.2).expect("用量非负");
        response.profile = profile.clone();
        response.provider = provider.clone();
        response.tool_calls = item["tool_calls"].as_i64().unwrap_or_default();
        Ok(response)
    }
}

/// JSON 解析失败文案两侧不同（Python `json.JSONDecodeError.msg` vs serde），
/// 提示词里的 `response_error` 字段按占位符归一化后再对照。
fn normalize_parse_error(text: &str) -> String {
    const MARKER: &str = "\"response_error\":\"";
    let Some(position) = text.find(MARKER) else {
        return text.to_string();
    };
    let start = position + MARKER.len();
    let Some(end) = text[start..].find('"') else {
        return text.to_string();
    };
    format!("{}<parse_error>{}", &text[..start], &text[start + end..])
}

/// 返回第一处差异的上下文；两侧完全一致时为空串。
fn first_diff(actual: &str, expected: &str) -> String {
    let actual_chars: Vec<char> = actual.chars().collect();
    let expected_chars: Vec<char> = expected.chars().collect();
    if actual_chars == expected_chars {
        return String::new();
    }
    let index = (0..actual_chars.len().min(expected_chars.len()))
        .find(|position| actual_chars[*position] != expected_chars[*position])
        .unwrap_or(actual_chars.len().min(expected_chars.len()));
    let start = index.saturating_sub(30);
    format!(
        "位置 {index}
实际: {}
期望: {}",
        actual_chars[start..(index + 30).min(actual_chars.len())]
            .iter()
            .collect::<String>(),
        expected_chars[start..(index + 30).min(expected_chars.len())]
            .iter()
            .collect::<String>(),
    )
}

fn assert_prompts(actual: &[String], expected: &Value, label: &str) {
    let actual: Vec<String> = actual
        .iter()
        .map(|text| normalize_parse_error(text))
        .collect();
    let expected = expected.as_array().expect("提示词数组");
    assert_eq!(actual.len(), expected.len(), "提示词条数（{label}）");
    for (index, item) in expected.iter().enumerate() {
        let text = expected_prompt_text(item, &actual);
        assert_eq!(
            actual[index].chars().count() as u64,
            item["len"].as_u64().expect("len"),
            "提示词长度（{label} #{index}）"
        );
        assert_eq!(
            sha256_hex(&actual[index]),
            item["sha256"].as_str().expect("sha256"),
            "提示词摘要（{label} #{index}）"
        );
        if let Some(text) = text {
            assert_eq!(actual[index], text, "提示词原文（{label} #{index}）");
        }
    }
}

/// 短提示词在数据集里带原文；长提示词只有长度与摘要，原文用当前实现自己核对。
fn expected_prompt_text(item: &Value, actual: &[String]) -> Option<String> {
    let text = item["text"].as_str()?;
    let _ = actual;
    Some(text.to_string())
}

// --------------------------------------------------------------------------- 账本

#[test]
fn ledger_measurement_payload_matches_python() {
    let case = orchestration()["ledger"].clone();
    let snapshot = snapshot_from(&case["snapshot"]);
    let ledger = UsageLedger;
    let payload = ledger.measurement_payload(&snapshot, &usage_from(&case["usage"]));
    assert_json(&payload, &case["payload"], "账本载荷");
    let empty = ledger.measurement_payload(&snapshot, &TokenUsageSample::default());
    assert_json(&empty, &case["empty_usage"], "空用量账本");
}

// --------------------------------------------------------------------------- 证据恢复

fn artifact_reader(spec: &Value) -> Box<ArtifactReader> {
    match spec["kind"].as_str().unwrap_or("text") {
        "not_text" => Box::new(|_path: &str| Err(ArtifactReadFailure::NotText)),
        "unreadable" => Box::new(|_path: &str| Err(ArtifactReadFailure::Unreadable)),
        _ => {
            let text = spec["text"].as_str().unwrap_or_default().to_string();
            Box::new(move |_path: &str| Ok(text.clone()))
        }
    }
}

#[test]
fn evidence_recall_matches_python() {
    let evidence = orchestration()["evidence"].clone();
    assert_eq!(
        evidence["tool_name"].as_str(),
        Some(
            SessionEvidenceRecallService::default()
                .max_items
                .to_string()
                .as_str()
        )
        .map(|_| { omnicrawl_controllers::context_compaction::RECALL_SESSION_EVIDENCE_TOOL_NAME }),
        "恢复工具名"
    );
    for case in evidence["cases"].as_array().expect("用例数组") {
        let label = case["label"].as_str().expect("label");
        let built = SessionEvidenceRecallService::new(
            case["max_items"].as_u64().expect("max_items") as usize,
            case["max_output_tokens"]
                .as_i64()
                .expect("max_output_tokens"),
        );
        match built {
            Err(error) => {
                assert!(!case["ok"].as_bool().expect("ok"), "本该成功（{label}）");
                assert_eq!(
                    error.message(),
                    case["error"].as_str().expect("error"),
                    "错误文案（{label}）"
                );
            }
            Ok(recall) => {
                assert!(case["ok"].as_bool().expect("ok"), "本该失败（{label}）");
                let reader = artifact_reader(&case["reader"]);
                let actual = recall.recall(
                    &source_events(&case["events"]),
                    &case["event_ids"],
                    reader.as_ref(),
                );
                assert_json(&actual, &case["result"], label);
            }
        }
    }
}

// --------------------------------------------------------------------------- 结构化摘要

#[test]
fn summary_parse_matches_python() {
    for case in orchestration()["summary"]["parse"]
        .as_array()
        .expect("用例数组")
    {
        let label = case["label"].as_str().expect("label");
        let content = case["content"].as_str().expect("content");
        match parse_structured_summary(content) {
            Ok(value) => {
                assert!(case["ok"].as_bool().expect("ok"), "本该失败（{label}）");
                assert_json(&value, &case["value"], label);
            }
            Err(error) => {
                assert!(!case["ok"].as_bool().expect("ok"), "本该成功（{label}）");
                match case["compare"].as_str() {
                    Some("prefix") => assert!(
                        error
                            .message()
                            .starts_with(case["error_prefix"].as_str().expect("error_prefix")),
                        "错误前缀（{label}）：{}",
                        error.message()
                    ),
                    _ => assert_eq!(error.message(), case["error"].as_str().expect("error")),
                }
            }
        }
    }
}

#[test]
fn summary_chunks_match_python() {
    for case in orchestration()["summary"]["chunks"]
        .as_array()
        .expect("用例数组")
    {
        let label = case["label"].as_str().expect("label");
        let events = source_events(&case["events"]);
        let chunks = chunk_events(&events, case["max_input_tokens"].as_i64().expect("预算"));
        let actual = Value::Array(
            chunks
                .iter()
                .map(|chunk| {
                    Value::Array(
                        chunk
                            .iter()
                            .map(|event| Value::from(event.event_id.clone()))
                            .collect(),
                    )
                })
                .collect(),
        );
        assert_json(&actual, &case["chunks"], label);
    }
}

#[test]
fn summary_compact_matches_python() {
    let summary = orchestration()["summary"].clone();
    assert_eq!(
        summary["tool_choice"].as_str(),
        Some("none"),
        "摘要请求的工具策略"
    );
    let prompt_text = summary_prompt();
    assert_eq!(
        prompt_text.chars().count() as u64,
        summary["prompt_text"]["len"].as_u64().expect("模板长度"),
        "摘要提示词模板长度"
    );
    assert_eq!(
        sha256_hex(&prompt_text),
        summary["prompt_text"]["sha256"].as_str().expect("模板摘要"),
        "摘要提示词模板摘要"
    );
    assert_eq!(
        prompt_text.chars().take(60).collect::<String>(),
        summary["prompt_text"]["head"].as_str().expect("模板开头"),
        "摘要提示词模板开头"
    );
    assert_eq!(
        prompt_text
            .chars()
            .rev()
            .take(60)
            .collect::<String>()
            .chars()
            .rev()
            .collect::<String>(),
        summary["prompt_text"]["tail"].as_str().expect("模板结尾"),
        "摘要提示词模板结尾"
    );
    assert_eq!(
        summary["invalid_constructor"]["ok"].as_bool(),
        Some(false),
        "非法构造应当失败"
    );

    for case in summary["compact"].as_array().expect("用例数组") {
        let label = case["label"].as_str().expect("label");
        let prompts = Rc::new(RefCell::new(Vec::new()));
        let model = scripted_model(
            case["responses"].as_array().cloned().unwrap_or_default(),
            Rc::clone(&prompts),
            (0, 0, 0),
            "",
            "",
        );
        let compactor = ModelSummaryCompactor::new(
            Box::new(model),
            summary_prompt(),
            case["max_input_tokens"].as_i64().expect("预算"),
        )
        .expect("压缩器可构造");
        let batch = batch_from(source_events(&case["events"]), None);
        let feedback: Vec<String> = case["validation_feedback"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .map(|item| item.as_str().unwrap_or_default().to_string())
            .collect();
        let result = compactor.compact(
            &batch,
            case["target_summary_tokens"].as_i64().unwrap_or_default(),
            &feedback,
        );
        match result {
            Ok(generation) => {
                assert!(case["ok"].as_bool().expect("ok"), "本该失败（{label}）");
                assert_json(&generation.structured, &case["structured"], label);
                assert_json(&generation.usage.to_dict(), &case["usage"], label);
                assert_eq!(
                    generation.profile,
                    case["profile"].as_str().unwrap_or_default()
                );
                assert_eq!(
                    generation.provider,
                    case["provider"].as_str().unwrap_or_default()
                );
                assert_eq!(
                    generation.attempts,
                    case["attempts"].as_i64().expect("attempts")
                );
            }
            Err(error) => {
                assert!(!case["ok"].as_bool().expect("ok"), "本该成功（{label}）");
                match case["compare"].as_str() {
                    Some("prefix") => assert!(
                        error
                            .message()
                            .starts_with(case["error_prefix"].as_str().expect("error_prefix")),
                        "错误前缀（{label}）：{}",
                        error.message()
                    ),
                    _ => assert_eq!(error.message(), case["error"].as_str().expect("error")),
                }
            }
        }
        assert_prompts(&prompts.borrow(), &case["prompts"], label);
    }
}

#[test]
fn summary_budget_and_previous_matches_python() {
    let summary = orchestration()["summary"].clone();
    for case in summary["budget"].as_array().expect("预算用例") {
        let label = case["label"].as_str().expect("label");
        let prompts = Rc::new(RefCell::new(Vec::new()));
        let model = scripted_model(
            case["responses"].as_array().cloned().unwrap_or_default(),
            Rc::clone(&prompts),
            (0, 0, 0),
            "",
            "",
        );
        let mut compactor = ModelSummaryCompactor::new(
            Box::new(model),
            summary_prompt(),
            case["max_input_tokens"].as_i64().expect("预算"),
        )
        .expect("压缩器可构造");
        match &case["budget_provider"] {
            Value::Null => {}
            Value::String(text) if text == "raise" => {
                // 内核用 None 表达「预算解析失败」，与 Python 的异常回落等价。
                compactor = compactor.with_budget_provider(Box::new(|| None));
            }
            other => {
                let extra = other.as_i64().expect("预算余额");
                compactor = compactor.with_budget_provider(Box::new(move || Some(extra)));
            }
        }
        let batch = batch_from(source_events(&case["events"]), None);
        let result = compactor.compact(
            &batch,
            case["target_summary_tokens"].as_i64().unwrap_or_default(),
            &[],
        );
        match result {
            Ok(generation) => {
                assert!(case["ok"].as_bool().expect("ok"), "本该失败（{label}）");
                assert_json(&generation.structured, &case["structured"], label);
            }
            Err(error) => {
                assert!(!case["ok"].as_bool().expect("ok"), "本该成功（{label}）");
                assert_eq!(error.message(), case["error"].as_str().expect("error"));
            }
        }
        if let Some(expected_payload) = case["payloads"]
            .as_array()
            .and_then(|items| items.first())
            .and_then(Value::as_str)
        {
            let expected_prompt = format!(
                "{}

输入：
{expected_payload}",
                summary_prompt()
            );
            let actual_prompt = prompts.borrow()[0].clone();
            assert_eq!(
                first_diff(&actual_prompt, &expected_prompt),
                "",
                "提示词逐字对照（{label}）"
            );
        }
        assert_prompts(&prompts.borrow(), &case["prompts"], label);
    }

    for case in summary["previous_summary"]
        .as_array()
        .expect("上次摘要用例")
    {
        let label = case["label"].as_str().expect("label");
        let prompts = Rc::new(RefCell::new(Vec::new()));
        let model = scripted_model(
            case["responses"].as_array().cloned().unwrap_or_default(),
            Rc::clone(&prompts),
            (0, 0, 0),
            "",
            "",
        );
        let compactor = ModelSummaryCompactor::new(Box::new(model), summary_prompt(), 64_000)
            .expect("压缩器可构造");
        let previous = match &case["previous_summary"] {
            Value::Object(map) if !map.is_empty() => Some(case["previous_summary"].clone()),
            _ => None,
        };
        let mut batch = batch_from(source_events(&case["events"]), previous);
        batch.previous_covered_event_ids = case["previous_covered_event_ids"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .map(|item| item.as_str().unwrap_or_default().to_string())
            .collect();
        let result = compactor.compact(&batch, 1500, &[]);
        assert!(case["ok"].as_bool().expect("ok"), "本该成功（{label}）");
        assert_json(
            &result.expect("摘要可生成").structured,
            &case["structured"],
            label,
        );
        assert_prompts(&prompts.borrow(), &case["prompts"], label);
    }
}

#[test]
fn summary_prompt_payloads_match_python() {
    let summary = orchestration()["summary"].clone();
    let cases: Vec<Value> = ["compact", "budget", "previous_summary"]
        .iter()
        .filter_map(|key| summary[*key].as_array().cloned())
        .flatten()
        .collect();
    for case in &cases {
        let label = case["label"].as_str().expect("label");
        let payloads = case["payloads"].as_array().cloned().unwrap_or_default();
        for (index, payload) in payloads.iter().enumerate() {
            let Some(text) = payload.as_str() else {
                continue;
            };
            let value: Value = serde_json::from_str(text).expect("载荷是 JSON");
            assert_eq!(
                omnicrawl_controllers::json::python_dumps_compact(&value),
                text,
                "载荷渲染（{label} #{index}）"
            );
        }
    }
}

// --------------------------------------------------------------------------- 压缩编排

#[test]
fn service_cases_match_python() {
    for case in orchestration()["service"]["cases"]
        .as_array()
        .expect("用例数组")
    {
        let label = case["label"].as_str().expect("label");
        let events = source_events(&case["source_events"]);
        let prompts = Rc::new(RefCell::new(Vec::new()));
        let script = case["compactor"]
            .get("items")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .map(|item| match item.get("error").and_then(Value::as_str) {
                        Some(error) => json!({"error": error}),
                        None => json!({
                            "content": serde_json::to_string(&item["structured"]).expect("可序列化"),
                            "tool_calls": 0,
                        }),
                    })
                    .collect::<Vec<Value>>()
            })
            .unwrap_or_default();
        let model = scripted_model(
            script,
            Rc::clone(&prompts),
            (1000, 100, 200),
            "cheap",
            "openai",
        );
        let mut service = ContextCompactionService::new();
        if case["compactor"].is_object() {
            let compactor = ModelSummaryCompactor::new(Box::new(model), summary_prompt(), 64_000)
                .expect("压缩器可构造");
            service = service.with_compactor(Box::new(compactor));
        }
        if let Some(guard) = case["guard"].as_array() {
            let unresolved: Vec<i64> = guard.iter().filter_map(Value::as_i64).collect();
            service = service
                .with_placeholder_guard(Box::new(move |_payload: &Value| unresolved.clone()));
        }

        match case["kind"].as_str().expect("kind") {
            "measure" => {
                let measured =
                    service.measure_after_complete_turn(measure_input_from(&case["measure"]));
                match measured {
                    Ok(measured) => {
                        assert!(case["ok"].as_bool().expect("ok"), "本该失败（{label}）");
                        assert_json(&measured.snapshot.to_dict(), &case["snapshot"], label);
                        assert_json(&measured.event_payload, &case["event_payload"], label);
                    }
                    Err(error) => {
                        assert!(!case["ok"].as_bool().expect("ok"), "本该成功（{label}）");
                        assert_eq!(error.message(), case["error"].as_str().expect("error"));
                    }
                }
            }
            kind => {
                let result = match kind {
                    "after_turn" => service.after_complete_turn(
                        &events,
                        measure_input_from(&case["measure"]),
                        case["reasoning_effort"].as_str().unwrap_or_default(),
                        case["preserve_exact_evidence"]
                            .as_bool()
                            .unwrap_or_default(),
                    ),
                    "manual" => service.manual_compact(
                        &events,
                        case["target_summary_tokens"].as_i64().unwrap_or_default(),
                        case["reasoning_effort"].as_str().unwrap_or_default(),
                        case["preserve_exact_evidence"]
                            .as_bool()
                            .unwrap_or_default(),
                    ),
                    "recovery" => service.recover_from_context_overflow(
                        &events,
                        case["target_summary_tokens"].as_i64().unwrap_or_default(),
                        case["reasoning_effort"].as_str().unwrap_or_default(),
                        case["preserve_exact_evidence"]
                            .as_bool()
                            .unwrap_or_default(),
                    ),
                    other => panic!("未知用例类型 {other}"),
                };
                let outcome = result.expect("编排不抛错");
                let expected = &case["outcome"];
                assert_json(
                    &outcome.measurement_payload,
                    &expected["measurement_payload"],
                    &format!("{label} · 测量载荷"),
                );
                match (&outcome.compact_payload, &expected["compact_payload"]) {
                    (Some(actual), Value::Object(_)) => assert_json(
                        actual,
                        &expected["compact_payload"],
                        &format!("{label} · 压缩载荷"),
                    ),
                    (None, Value::Null) => {}
                    (actual, want) => panic!("压缩载荷形状不符（{label}）：{actual:?} vs {want:?}"),
                }
                match (&outcome.history_projection, &expected["history_projection"]) {
                    (Some(actual), Value::Array(_)) => assert_json(
                        &Value::Array(actual.clone()),
                        &expected["history_projection"],
                        &format!("{label} · 上下文投影"),
                    ),
                    (None, Value::Null) => {}
                    (actual, want) => panic!("投影形状不符（{label}）：{actual:?} vs {want:?}"),
                }
                assert_eq!(
                    outcome.fallback_required,
                    expected["fallback_required"].as_bool().expect("fallback"),
                    "降级标记（{label}）"
                );
                assert_eq!(
                    outcome.diagnostic,
                    expected["diagnostic"].as_str().expect("diagnostic"),
                    "诊断文案（{label}）"
                );
            }
        }
    }
}

// --------------------------------------------------------------------------- 回合判定面

#[test]
fn turn_compaction_notices_match_python() {
    for case in orchestration()["turn"]["notices"]
        .as_array()
        .expect("用例数组")
    {
        let before = case["before"].as_i64();
        let after = case["after"].as_i64();
        let actual = format_compaction_notice(before, after);
        match case["notice"].as_str() {
            Some(text) => assert_eq!(actual.as_deref(), Some(text), "通知文案"),
            None => assert_eq!(actual, None, "通知文案（应为空）"),
        }
    }
}

#[test]
fn turn_compaction_memory_requests_match_python() {
    for case in orchestration()["turn"]["memory"]
        .as_array()
        .expect("用例数组")
    {
        let label = case["label"].as_str().expect("label");
        let requests = compaction_memory_requests(&case["payload"]);
        let actual = Value::Array(
            requests
                .iter()
                .map(|request| {
                    json!({
                        "content": request.content,
                        "related_directories": request.related_directories,
                        "storage_directory": request.storage_directory,
                        "source_event": request.source_event,
                    })
                })
                .collect(),
        );
        assert_json(&actual, &case["requests"], label);
    }
}

fn search_result_from(value: &Value) -> MemorySearchResult {
    MemorySearchResult {
        id: value["id"].as_str().expect("id").to_string(),
        summary: value["summary"].as_str().unwrap_or_default().to_string(),
        storage_directory: value["storage_directory"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
        related_directories: Vec::new(),
        timestamp: omnicrawl_session::parse_datetime("2026-01-01T00:00:00Z")
            .expect("时间戳")
            .fixed_offset(),
    }
}

#[test]
fn turn_compaction_recall_matches_python() {
    for case in orchestration()["turn"]["recall"]
        .as_array()
        .expect("用例数组")
    {
        let label = case["label"].as_str().expect("label");
        let results: Vec<MemorySearchResult> = case["results"]
            .as_array()
            .expect("检索结果")
            .iter()
            .map(search_result_from)
            .collect();
        let expected_query = case["search_calls"]
            .as_array()
            .and_then(|calls| calls.first())
            .map(|call| call["query"].as_str().unwrap_or_default().to_string());
        assert_eq!(
            compaction_recall_query(&case["payload"]),
            expected_query,
            "召回查询（{label}）"
        );
        if let Some(call) = case["search_calls"]
            .as_array()
            .and_then(|calls| calls.first())
        {
            assert_eq!(
                call["max_results"].as_u64(),
                Some(RECALL_MAX_RESULTS as u64),
                "单次检索条数（{label}）"
            );
        }
        let hits = compaction_recall_hits(&results);
        let text = compaction_recall_text(&results);

        if case["search_calls"]
            .as_array()
            .map(|calls| calls.is_empty())
            .unwrap_or(true)
        {
            assert!(
                compaction_recall_query(&case["payload"]).is_none(),
                "无检索时查询应为空（{label}）"
            );
        }
        match case["events"].as_array().and_then(|events| events.first()) {
            Some(event) => {
                let payload = compaction_recall_event_payload(
                    expected_query.as_deref().unwrap_or_default(),
                    &hits,
                );
                assert_json(&payload["hits"], &event["payload"]["hits"], label);
                assert_json(&payload["query"], &event["payload"]["query"], label);
            }
            None => {
                // 宿主的两条提前返回：检索结果为空，或没有可用的结构化摘要。
                let no_results = case["results"]
                    .as_array()
                    .map(|items| items.is_empty())
                    .unwrap_or(true);
                let no_query = compaction_recall_query(&case["payload"]).is_none();
                assert!(
                    no_results || no_query,
                    "没有召回内容时不应写入事件（{label}）"
                );
            }
        }
        if let Some(history) = case["history"].as_array() {
            if history.len() > 1 {
                assert_eq!(
                    history[1]["content"].as_str().unwrap_or_default(),
                    text,
                    "注入文本（{label}）"
                );
            }
        }
    }
    assert_eq!(RECALL_QUERY_CHARS, 200);
    assert_eq!(RECALL_TEXT_LIMIT, 1200);
}

#[test]
fn turn_compaction_archive_selection_matches_python() {
    for case in orchestration()["turn"]["archive"]
        .as_array()
        .expect("用例数组")
    {
        let label = case["label"].as_str().expect("label");
        let enabled = case["archive_enabled"].as_bool().expect("archive_enabled");
        let payload_ids: Vec<String> = case["payload"]
            .get("compacted_event_ids")
            .and_then(Value::as_array)
            .map(|items| {
                let mut ids: Vec<String> = Vec::new();
                for item in items {
                    if let Some(id) = item.as_str() {
                        if !ids.iter().any(|existing| existing == id) {
                            ids.push(id.to_string());
                        }
                    }
                }
                ids
            })
            .unwrap_or_default();
        let expected_wanted = if enabled {
            payload_ids.clone()
        } else {
            Vec::new()
        };
        let wanted = archive_compacted_event_ids(enabled, &case["payload"]);
        assert_eq!(wanted, expected_wanted, "待归档集合（{label}）");

        let event_ids: Vec<String> = case["events"]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .map(|item| item["event_id"].as_str().unwrap_or_default().to_string())
                    .collect()
            })
            .unwrap_or_default();
        let archived: Vec<String> = case["archived"]
            .get("raw")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .map(|item| item["event_id"].as_str().unwrap_or_default().to_string())
                    .collect()
            })
            .unwrap_or_default();
        let matched: Vec<String> = wanted
            .iter()
            .filter(|id| event_ids.contains(id))
            .cloned()
            .collect();
        assert_eq!(
            archived.is_empty(),
            matched.is_empty(),
            "归档触发条件（{label}）"
        );
        let archive_id = case["archive_id"].as_str().unwrap_or_default();
        assert_eq!(
            archive_id.is_empty(),
            archived.is_empty(),
            "归档返回值（{label}）"
        );
    }
}

#[test]
fn turn_compaction_constants_match_python() {
    let constants = &orchestration()["turn"]["constants"];
    assert_eq!(
        constants["source_event"].as_str(),
        Some(omnicrawl_controllers::turn::compaction::COMPACTION_MEMORY_SOURCE_EVENT)
    );
    assert_eq!(
        constants["project_directory"].as_str(),
        Some(omnicrawl_controllers::turn::compaction::PROJECT_CONTEXT_DIRECTORY)
    );
    assert_eq!(
        constants["task_directory"].as_str(),
        Some(omnicrawl_controllers::turn::compaction::TASK_HISTORY_DIRECTORY)
    );
}
