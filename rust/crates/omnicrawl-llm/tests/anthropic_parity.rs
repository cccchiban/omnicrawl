//! Anthropic Claude Messages 的跨语言 parity：期望值来自 Python 真实现。
//!
//! 覆盖 `_to_anthropic_messages`、`_sanitize_options`、`_format_anthropic_error`、
//! 请求 kwargs 摊平后的线上请求体、流事件映射与 `usage_from_anthropic_payload`。

mod common;

use std::collections::BTreeMap;

use omnicrawl_llm::{
    build_anthropic_request, format_anthropic_error, sanitize_anthropic_options,
    to_anthropic_messages, usage_from_anthropic_payload, AnthropicStreamState, ChatRequestInput,
};
use omnicrawl_protocol::{ConversationMessage, GenerationOptions, ModelStreamEvent, ToolSpec};
use serde_json::{Map, Value};

use common::event_to_json;

const FIXTURE: &str = include_str!("fixtures/anthropic_parity.json");

fn fixture() -> Value {
    serde_json::from_str(FIXTURE).expect("fixture 不是合法 JSON")
}

fn messages_of(value: &Value) -> Vec<ConversationMessage> {
    value
        .as_array()
        .expect("messages")
        .iter()
        .map(|item| serde_json::from_value(item.clone()).expect("消息反序列化"))
        .collect()
}

fn tools_of(value: &Value) -> Vec<ToolSpec> {
    value
        .as_array()
        .expect("tools")
        .iter()
        .map(|item| serde_json::from_value(item.clone()).expect("工具反序列化"))
        .collect()
}

fn options_of(value: &Value) -> GenerationOptions {
    GenerationOptions {
        max_output_tokens: value["max_output_tokens"].as_u64().map(|item| item as u32),
        temperature: value["temperature"].as_f64(),
        reasoning_effort: value["reasoning_effort"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
        tool_choice: value["tool_choice"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
        request_timeout_seconds: value["request_timeout_seconds"]
            .as_f64()
            .unwrap_or_default(),
        request_retry_count: 5,
        provider_options: value["provider_options"]
            .as_object()
            .cloned()
            .unwrap_or_default(),
    }
}

/// 请求体按键集合逐项比对：`provider_options` 的摊平位置由 SDK 决定，键序不追。
fn assert_same_object(actual: &Value, expected: &Value, label: &str) {
    let actual = actual.as_object().expect("实际值应为对象");
    let expected = expected.as_object().expect("期望值应为对象");
    let mut actual_keys: Vec<&String> = actual.keys().collect();
    let mut expected_keys: Vec<&String> = expected.keys().collect();
    actual_keys.sort();
    expected_keys.sort();
    assert_eq!(actual_keys, expected_keys, "键集合（{label}）");
    for key in expected_keys {
        assert_eq!(actual[key], expected[key], "字段 {key}（{label}）");
    }
}

#[test]
fn messages_match_python() {
    let fixture = fixture();
    let cases = fixture["messages"].as_array().expect("messages");
    assert!(!cases.is_empty(), "数据集为空");
    for case in cases {
        let label = case["label"].as_str().unwrap_or("");
        let messages = messages_of(&case["messages"]);
        let built = Value::Array(to_anthropic_messages(&messages));
        assert_eq!(built, case["expected"], "messages（{label}）");
    }
}

#[test]
fn options_match_python() {
    let fixture = fixture();
    let cases = fixture["options"].as_array().expect("options");
    assert!(!cases.is_empty(), "数据集为空");
    for case in cases {
        let label = case["label"].as_str().unwrap_or("");
        let options: Map<String, Value> = case["options"].as_object().cloned().unwrap_or_default();
        match sanitize_anthropic_options(&options) {
            Ok(result) => {
                assert!(case["ok"].as_bool().expect("ok"), "应为通过（{label}）");
                assert_eq!(Value::Object(result), case["result"], "校验结果（{label}）");
            }
            Err(error) => {
                assert!(!case["ok"].as_bool().expect("ok"), "应为拒绝（{label}）");
                assert_eq!(
                    error.message,
                    case["error"].as_str().expect("error"),
                    "错误文案（{label}）"
                );
            }
        }
    }
}

#[test]
fn request_body_matches_python() {
    let fixture = fixture();
    let cases = fixture["request"].as_array().expect("request");
    assert!(!cases.is_empty(), "数据集为空");
    let identity: BTreeMap<String, String> = BTreeMap::new();

    for case in cases {
        let label = case["label"].as_str().unwrap_or("");
        let messages = messages_of(&case["messages"]);
        let tools = tools_of(&case["tools"]);
        let options = options_of(&case["options"]);
        let input = ChatRequestInput {
            model: case["model"].as_str().expect("model"),
            system_prompt: case["system_prompt"].as_str().expect("system_prompt"),
            messages: &messages,
            tools: &tools,
            options: &options,
            profile_request_timeout_seconds: 180.0,
            prompt_cache_capable: false,
            prompt_cache_identity: &identity,
        };
        let descriptor = case["descriptor_max_output_tokens"]
            .as_u64()
            .map(|value| value as u32);
        let request = build_anthropic_request(&input, descriptor)
            .unwrap_or_else(|error| panic!("请求构建失败（{label}）：{}", error.message));
        assert_same_object(&request.body, &case["kwargs"], label);
    }
}

#[test]
fn stream_events_match_python() {
    let fixture = fixture();
    let cases = fixture["stream"].as_array().expect("stream");
    assert!(!cases.is_empty(), "数据集为空");
    for case in cases {
        let label = case["label"].as_str().unwrap_or("");
        let mut state = AnthropicStreamState::new();
        let mut events: Vec<ModelStreamEvent> = Vec::new();
        for payload in case["payloads"].as_array().expect("payloads") {
            state.handle_event(payload, &mut events);
        }
        state.finish(&mut events);
        let produced: Vec<Value> = events.iter().map(event_to_json).collect();
        assert_eq!(
            Value::Array(produced),
            case["expected"],
            "流事件（{label}）"
        );
    }
}

#[test]
fn usage_matches_python() {
    let fixture = fixture();
    let cases = fixture["usage"].as_array().expect("usage");
    assert!(!cases.is_empty(), "数据集为空");
    for case in cases {
        let label = case["label"].as_str().unwrap_or("");
        let produced = usage_from_anthropic_payload(&case["payload"]).map(|usage| {
            serde_json::json!({
                "input_tokens": usage.input_tokens,
                "output_tokens": usage.output_tokens,
                "cached_input_tokens": usage.cached_input_tokens,
            })
        });
        let expected = if case["expected"].is_null() {
            None
        } else {
            Some(case["expected"].clone())
        };
        assert_eq!(produced, expected, "用量（{label}）");
    }
}

#[test]
fn error_text_matches_python() {
    let fixture = fixture();
    let cases = fixture["errors"].as_array().expect("errors");
    assert!(!cases.is_empty(), "数据集为空");
    for case in cases {
        let produced = format_anthropic_error(
            case["message"].as_str().expect("message"),
            case["type_name"].as_str().expect("type_name"),
        );
        assert_eq!(
            produced,
            case["expected"].as_str().expect("expected"),
            "错误文案（{}）",
            case["type_name"].as_str().unwrap_or("")
        );
    }
}
