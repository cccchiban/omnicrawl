//! `agent/context_compaction/{models,policy}.py` 的跨语言对照。
//!
//! 数据集是冻结的对照契约：`python rust/tools/gen_controllers_fixture.py` 重新生成
//! `tests/fixtures/controllers_parity.json` 的 `context_compaction` 段后，本套件用同一批计数、
//! 消息与事件序列重放 Rust 实现逐字段比对（快照按序列化后的字符串比对，键序也是契约）。

use omnicrawl_controllers::context_compaction as compaction;
use omnicrawl_controllers::context_compaction::{
    CompactionBatch as Batch, ContextBudgetSnapshot as Snapshot,
    DefaultContextBudgetManager as Manager, MeasureInput, SourceEvent as Event, TokenCountInput,
    TokenUsageSample as Usage,
};
use serde_json::{Map, Value};

const FIXTURE: &str = include_str!("fixtures/controllers_parity.json");

fn fixture() -> Value {
    serde_json::from_str(FIXTURE).expect("fixture 不是合法 JSON")
}

fn section() -> Value {
    fixture()["context_compaction"].clone()
}

fn serialized(value: &Value) -> String {
    serde_json::to_string(value).expect("序列化")
}

fn usage_from(values: &Value) -> Usage {
    let items = values.as_array().expect("usage 三元组");
    Usage {
        input_tokens: items[0].as_i64().expect("input"),
        output_tokens: items[1].as_i64().expect("output"),
        cached_input_tokens: items[2].as_i64().expect("cached"),
    }
}

fn events_from(values: &Value) -> Vec<Event> {
    values
        .as_array()
        .expect("events")
        .iter()
        .map(|item| Event {
            event_id: item["event_id"].as_str().expect("event_id").to_string(),
            event_type: item["type"].as_str().expect("type").to_string(),
            payload: item["payload"].clone(),
        })
        .collect()
}

fn batch_view(batch: Option<&Batch>) -> Value {
    let Some(batch) = batch else {
        return Value::Null;
    };
    let mut map = Map::new();
    map.insert(
        "events".to_string(),
        Value::Array(batch.events.iter().map(Event::to_prompt_dict).collect()),
    );
    map.insert(
        "recent_events".to_string(),
        Value::Array(
            batch
                .recent_events
                .iter()
                .map(Event::to_prompt_dict)
                .collect(),
        ),
    );
    map.insert(
        "previous_summary".to_string(),
        batch.previous_summary.clone().unwrap_or(Value::Null),
    );
    map.insert(
        "previous_covered_event_ids".to_string(),
        Value::Array(
            batch
                .previous_covered_event_ids
                .iter()
                .map(|item| Value::from(item.clone()))
                .collect(),
        ),
    );
    map.insert(
        "single_large_turn".to_string(),
        Value::from(batch.single_large_turn),
    );
    map.insert(
        "covered_event_ids".to_string(),
        Value::Array(
            batch
                .covered_event_ids()
                .into_iter()
                .map(Value::from)
                .collect(),
        ),
    );
    Value::Object(map)
}

#[test]
fn compaction_estimates_match_python() {
    let data = section();
    for case in data["estimate_text"].as_array().expect("cases") {
        assert_eq!(
            compaction::estimate_text_tokens(case["value"].as_str().expect("value")),
            case["expected"].as_i64().expect("expected"),
            "文本估算（{:?}）",
            case["value"]
        );
    }
    for case in data["estimate_value"].as_array().expect("cases") {
        assert_eq!(
            compaction::estimate_value_tokens(&case["value"]),
            case["expected"].as_i64().expect("expected"),
            "值估算（{:?}）",
            case["value"]
        );
    }
    for case in data["estimate_json"].as_array().expect("cases") {
        assert_eq!(
            compaction::estimate_json_tokens(&case["value"]),
            case["expected"].as_i64().expect("expected"),
            "JSON 估算（{:?}）",
            case["value"]
        );
    }
    for case in data["estimate_messages"].as_array().expect("cases") {
        let messages = case["messages"].as_array().expect("messages").to_vec();
        assert_eq!(
            compaction::estimate_messages_tokens(&messages),
            case["expected"].as_i64().expect("expected"),
            "消息估算"
        );
    }
    assert_eq!(
        compaction::COMPACT_SUMMARY_PREFIX,
        data["constants"]["summary_prefix"]
            .as_str()
            .expect("prefix")
    );
}

#[test]
fn compaction_usage_matches_python() {
    let data = section();
    for case in data["usage"].as_array().expect("cases") {
        let label = case["label"].as_str().expect("label");
        let values = case["values"].as_array().expect("values");
        let result = Usage::new(
            values[0].as_i64().expect("input"),
            values[1].as_i64().expect("output"),
            values[2].as_i64().expect("cached"),
        );
        match result {
            Err(error) => {
                assert!(!case["ok"].as_bool().expect("ok"), "本该成功（{label}）");
                assert_eq!(
                    error.message(),
                    case["error"].as_str().expect("error"),
                    "错误文案（{label}）"
                );
            }
            Ok(usage) => {
                assert_eq!(
                    serialized(&usage.to_dict()),
                    serialized(&case["to_dict"]),
                    "用量字典（{label}）"
                );
                assert_eq!(
                    serialized(&usage.add(10, -5, 3).to_dict()),
                    serialized(&case["added"]),
                    "累计用量（{label}）"
                );
            }
        }
    }
}

#[test]
fn compaction_measure_from_counts_matches_python() {
    let data = section();
    for case in data["measure_from_counts"].as_array().expect("cases") {
        let label = case["label"].as_str().expect("label");
        let input = &case["input"];
        let counts = TokenCountInput {
            stable_context_tokens: input["stable_context_tokens"].as_i64().expect("stable"),
            existing_summary_tokens: input["existing_summary_tokens"].as_i64().expect("summary"),
            cold_history_tokens: input["cold_history_tokens"].as_i64().expect("cold"),
            recent_history_tokens: input["recent_history_tokens"].as_i64().expect("recent"),
            next_user_reserve_tokens: input["next_user_reserve_tokens"].as_i64().expect("reserve"),
            target_summary_tokens: input["target_summary_tokens"].as_i64().expect("target"),
            trigger_context_tokens: input["trigger_context_tokens"].as_i64().expect("trigger"),
            context_window_tokens: input["context_window_tokens"].as_i64().expect("window"),
            usage: usage_from(&case["usage"]),
            emergency_context_ratio: input["emergency_context_ratio"].as_f64().expect("ratio"),
            provider_input_tokens: input["provider_input_tokens"].as_i64().expect("provider"),
        };
        let result = Manager.measure_from_token_counts(&counts);
        match result {
            Err(error) => {
                assert!(!case["ok"].as_bool().expect("ok"), "本该成功（{label}）");
                assert_eq!(
                    error.message(),
                    case["error"].as_str().expect("error"),
                    "错误文案（{label}）"
                );
            }
            Ok(snapshot) => assert_eq!(
                serialized(&snapshot.to_dict()),
                serialized(&case["snapshot"]),
                "快照（{label}）"
            ),
        }
    }
}

#[test]
fn compaction_measure_matches_python() {
    let data = section();
    for case in data["measure"].as_array().expect("cases") {
        let label = case["label"].as_str().expect("label");
        let input = &case["input"];
        let measure = MeasureInput {
            system_prompt: input["system_prompt"].as_str().expect("system").to_string(),
            context_messages: input["context_messages"]
                .as_array()
                .expect("context_messages")
                .to_vec(),
            history_messages: input["history_messages"]
                .as_array()
                .expect("history_messages")
                .to_vec(),
            tool_schemas: input["tool_schemas"]
                .as_array()
                .expect("tool_schemas")
                .to_vec(),
            recent_turns: input["recent_turns"].as_i64().expect("recent_turns"),
            target_summary_tokens: input["target_summary_tokens"].as_i64().expect("target"),
            next_user_reserve_tokens: input["next_user_reserve_tokens"].as_i64().expect("reserve"),
            trigger_context_tokens: input["trigger_context_tokens"].as_i64().expect("trigger"),
            context_window_tokens: input["context_window_tokens"].as_i64().expect("window"),
            usage: usage_from(&case["usage"]),
            provider_input_tokens: input["provider_input_tokens"].as_i64().expect("provider"),
            emergency_context_ratio: input["emergency_context_ratio"].as_f64().expect("ratio"),
        };
        let result = Manager.measure(&measure);
        match result {
            Err(error) => {
                assert!(!case["ok"].as_bool().expect("ok"), "本该成功（{label}）");
                assert_eq!(
                    error.message(),
                    case["error"].as_str().expect("error"),
                    "错误文案（{label}）"
                );
            }
            Ok(snapshot) => assert_eq!(
                serialized(&snapshot.to_dict()),
                serialized(&case["snapshot"]),
                "快照（{label}）"
            ),
        }
    }
}

#[test]
fn compaction_decision_matches_python() {
    let data = section();
    for case in data["decisions"].as_array().expect("cases") {
        let label = case["label"].as_str().expect("label");
        let snapshot = Snapshot {
            trigger_reached: case["trigger_reached"].as_bool().expect("trigger_reached"),
            ..Snapshot::default()
        };
        let batch = if case["has_batch"].as_bool().expect("has_batch") {
            Some(Batch::default())
        } else {
            None
        };
        let decision = Manager::decide_auto_compaction(snapshot, batch.as_ref());
        assert_eq!(
            decision.should_compact,
            case["should_compact"].as_bool().expect("should_compact"),
            "是否压缩（{label}）"
        );
        assert_eq!(
            decision.reason,
            case["reason"].as_str().expect("reason"),
            "决策原因（{label}）"
        );
    }
}

#[test]
fn compaction_batches_match_python() {
    let data = section();
    for case in data["batches"].as_array().expect("cases") {
        let label = case["label"].as_str().expect("label");
        let events = events_from(&case["events"]);
        let manager = Manager;
        assert_eq!(
            serialized(&batch_view(manager.select_batch(&events).as_ref())),
            serialized(&case["selected"]),
            "压缩批次（{label}）"
        );
        assert_eq!(
            serialized(&batch_view(manager.select_recovery_batch(&events).as_ref())),
            serialized(&case["recovery"]),
            "恢复批次（{label}）"
        );
    }
}

#[test]
fn compaction_source_events_match_python() {
    let data = section();
    for case in data["source_events"].as_array().expect("cases") {
        let label = case["label"].as_str().expect("label");
        let event = Event {
            event_id: case["event_id"].as_str().expect("event_id").to_string(),
            event_type: case["type"].as_str().expect("type").to_string(),
            payload: case["payload"].clone(),
        };
        assert_eq!(
            serialized(&event.to_prompt_dict()),
            serialized(&case["prompt_dict"]),
            "提示字典（{label}）"
        );
        assert_eq!(
            serialized(&event.to_index_dict(200)),
            serialized(&case["index_dict"]),
            "索引字典（{label}）"
        );
        assert_eq!(
            serialized(&event.to_index_dict(50)),
            serialized(&case["index_dict_50"]),
            "索引字典 50（{label}）"
        );
        assert_eq!(
            serialized(&event.to_index_dict(0)),
            serialized(&case["index_dict_zero"]),
            "索引字典 0（{label}）"
        );
    }
}

#[test]
fn compaction_summary_validation_matches_python() {
    let data = section();
    for case in data["validation"].as_array().expect("cases") {
        let label = case["label"].as_str().expect("label");
        let events = events_from(&case["events"]);
        let completeness: Option<Vec<Event>> =
            case["completeness_event_ids"].as_array().map(|ids| {
                ids.iter()
                    .filter_map(Value::as_str)
                    .filter_map(|id| events.iter().find(|event| event.event_id == id).cloned())
                    .collect::<Vec<Event>>()
            });
        let previous = match case.get("previous_summary") {
            None | Some(Value::Null) => None,
            Some(other) => Some(other.clone()),
        };
        let result = compaction::validate_summary(
            &case["structured"],
            &events,
            case["target_summary_tokens"]
                .as_i64()
                .expect("target_summary_tokens"),
            previous.as_ref(),
            case["preserve_exact_evidence"]
                .as_bool()
                .expect("preserve_exact_evidence"),
            completeness.as_deref(),
        );
        assert_eq!(
            result.valid,
            case["valid"].as_bool().expect("valid"),
            "校验结论（{label}）"
        );
        assert_eq!(
            Value::Array(
                result
                    .errors
                    .iter()
                    .map(|item| Value::from(item.clone()))
                    .collect()
            ),
            case["errors"],
            "错误列表（{label}）"
        );
        match (result.normalized.as_ref(), &case["normalized"]) {
            (Some(_), Value::Null) => panic!("本该没有 normalized：{label}"),
            (None, Value::Null) => {}
            (None, expected) => panic!("缺少 normalized：{label}（期望 {expected}）"),
            (Some(actual), expected) => assert_eq!(
                serialized(actual),
                serialized(expected),
                "归一化结果（{label}）"
            ),
        }
    }
}

#[test]
fn compaction_projection_matches_python() {
    let data = section();
    let projection = &data["projection"];
    assert_eq!(
        compaction::render_summary_markdown(&projection["structured_full"]),
        projection["rendered_full"].as_str().expect("rendered_full")
    );
    assert_eq!(
        compaction::render_summary_markdown(&projection["structured_empty"]),
        projection["rendered_empty"]
            .as_str()
            .expect("rendered_empty")
    );

    for case in projection["messages"].as_array().expect("messages") {
        let event = Event {
            event_id: case["event_id"].as_str().expect("event_id").to_string(),
            event_type: case["type"].as_str().expect("type").to_string(),
            payload: case["payload"].clone(),
        };
        let actual = compaction::event_to_model_message(&event);
        match (actual, &case["expected"]) {
            (None, Value::Null) => {}
            (None, expected) => panic!("本该有消息：{expected}"),
            (Some(actual), Value::Null) => panic!("本该没有消息：{actual}"),
            (Some(actual), expected) => assert_eq!(
                serialized(&actual),
                serialized(expected),
                "事件投影（{}）",
                case["event_id"]
            ),
        }
    }

    let events = events_from(&projection["events"]);
    // 生成器只用前两条事件做装配对照（e1 既在最近原文里、又作为最终回复锚点）
    let recent = &events[..2];
    let structured = &projection["structured_full"];
    let anchor = recent.iter().find(|event| event.event_id == "e1");
    assert_eq!(
        serialized(&Value::Array(compaction::assemble_summary_history(
            structured, recent, anchor
        ))),
        serialized(&projection["assembled_with_anchor"]),
        "装配（带锚点）"
    );
    assert_eq!(
        serialized(&Value::Array(compaction::assemble_summary_history(
            structured, recent, None
        ))),
        serialized(&projection["assembled_without_anchor"]),
        "装配（无锚点）"
    );
    assert_eq!(
        serialized(&Value::Array(compaction::assemble_summary_history(
            structured,
            &[],
            None
        ))),
        serialized(&projection["assembled_recent_only"]),
        "装配（无事件）"
    );
    assert_eq!(
        compaction::recent_message_count(&events),
        projection["recent_message_count"]
            .as_u64()
            .expect("recent_message_count") as usize,
        "最近消息计数"
    );
    assert_eq!(
        compaction::latest_final_reply_event(&events).map(|event| event.event_id.clone()),
        projection["latest_final_reply"]
            .as_str()
            .map(str::to_string),
        "最近最终回复"
    );
    let empty_events = vec![
        Event {
            event_id: "x1".to_string(),
            event_type: "assistant_message".to_string(),
            payload: serde_json::json!({"content": "  "}),
        },
        Event {
            event_id: "x2".to_string(),
            event_type: "user_message".to_string(),
            payload: serde_json::json!({"content": "问"}),
        },
    ];
    assert_eq!(
        compaction::latest_final_reply_event(&empty_events).is_none(),
        projection["latest_final_reply_none"]
            .as_bool()
            .expect("latest_final_reply_none"),
        "无最终回复"
    );
}
