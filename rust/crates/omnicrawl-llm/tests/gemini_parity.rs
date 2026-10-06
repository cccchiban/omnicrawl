//! Gemini Generate Content 的跨语言 parity：数据集是冻结的对照契约。
//!
//! 覆盖 `_to_gemini_contents`、`_sanitize_options`、请求 kwargs（contents + config）、
//! 真 SDK 线上请求体（路径 + body）、流事件映射、`usage_from_gemini_payload` 与 `_format_gemini_error`。

mod common;

use std::collections::BTreeMap;

use omnicrawl_llm::{
    build_generate_content_request, format_gemini_error, gemini_model_path, generate_content_body,
    sanitize_gemini_options, to_gemini_contents, usage_from_gemini_payload, ChatRequestInput,
    GeminiStreamState,
};
use omnicrawl_protocol::{ConversationMessage, GenerationOptions, ModelStreamEvent, ToolSpec};
use serde_json::{Map, Value};

use common::event_to_json;

const FIXTURE: &str = include_str!("fixtures/gemini_parity.json");

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

/// 请求体按键集合逐项比对：键序由两侧各自的组装顺序决定，不追字节一致。
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
fn contents_match_python() {
    let fixture = fixture();
    let cases = fixture["contents"].as_array().expect("contents");
    assert!(!cases.is_empty(), "数据集为空");
    for case in cases {
        let label = case["label"].as_str().unwrap_or("");
        let messages = messages_of(&case["messages"]);
        let built = Value::Array(to_gemini_contents(&messages));
        assert_eq!(built, case["expected"], "contents（{label}）");
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
        match sanitize_gemini_options(&options) {
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
fn request_kwargs_match_python() {
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
        let request = build_generate_content_request(&input)
            .unwrap_or_else(|error| panic!("请求构建失败（{label}）：{}", error.message));
        let expected = case["kwargs"].as_object().expect("kwargs");
        assert_eq!(
            Value::Array(request.contents.clone()),
            expected["contents"],
            "contents（{label}）"
        );
        assert_eq!(
            Value::Object(request.config.clone()),
            expected["config"],
            "config（{label}）"
        );
        assert_eq!(
            Value::String(request.model.clone()),
            expected["model"],
            "model（{label}）"
        );
    }
}

#[test]
fn wire_body_matches_python_sdk() {
    let fixture = fixture();
    let cases = fixture["wire"].as_array().expect("wire");
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
        let request = build_generate_content_request(&input)
            .unwrap_or_else(|error| panic!("请求构建失败（{label}）：{}", error.message));
        let path = format!(
            "/v1beta/{}:streamGenerateContent?alt=sse",
            gemini_model_path(&request.model)
        );
        assert_eq!(
            path,
            case["path"].as_str().expect("path"),
            "路径（{label}）"
        );
        assert_same_object(&generate_content_body(&request), &case["body"], label);
    }
}

#[test]
fn stream_events_match_python() {
    let fixture = fixture();
    let cases = fixture["stream"].as_array().expect("stream");
    assert!(!cases.is_empty(), "数据集为空");
    for case in cases {
        let label = case["label"].as_str().unwrap_or("");
        let mut state = GeminiStreamState::new();
        let mut events: Vec<ModelStreamEvent> = Vec::new();
        for payload in case["payloads"].as_array().expect("payloads") {
            state.handle_chunk(payload, &mut events);
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
        let produced = usage_from_gemini_payload(&case["payload"]).map(|usage| {
            serde_json::json!({
                "input_tokens": usage.input_tokens,
                "output_tokens": usage.output_tokens,
                "cached_input_tokens": usage.cached_input_tokens,
                "reasoning_tokens": usage.reasoning_tokens,
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
        let produced = format_gemini_error(
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
