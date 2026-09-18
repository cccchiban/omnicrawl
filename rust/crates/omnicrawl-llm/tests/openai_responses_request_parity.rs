//! OpenAI Responses 请求构建的跨语言 parity：期望值来自 Python 真实现。
//!
//! 覆盖 `messages_to_responses_input`（含 reasoning item 的 SHA-1 id 与分块边界）、
//! `_tools_for_responses`、`_flatten_tool_history_to_text`、工具历史判定的 400 分支，
//! 以及 `_build_responses_kwargs` 摊平后的线上请求体与传输层 timeout。

use std::collections::BTreeMap;

use omnicrawl_llm::{
    build_responses_request, flatten_tool_history_to_text, has_tool_history_items,
    is_tool_history_rejection, messages_to_responses_input, tools_for_responses, ChatRequestInput,
};
use omnicrawl_protocol::{ConversationMessage, GenerationOptions, ToolSpec};
use serde_json::Value;

const FIXTURE: &str = include_str!("fixtures/openai_responses_request_parity.json");

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

/// 请求体按键集合逐项比对：`extra_body` 的摊平位置由 SDK 决定，键序不追。
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
fn input_items_match_python() {
    let fixture = fixture();
    let cases = fixture["messages"].as_array().expect("messages");
    assert!(!cases.is_empty(), "数据集为空");
    for case in cases {
        let label = case["label"].as_str().unwrap_or("");
        let messages = messages_of(&case["messages"]);
        let items = Value::Array(messages_to_responses_input(&messages));
        assert_eq!(
            items.to_string(),
            case["items"].to_string(),
            "input items（{label}）"
        );
    }
}

#[test]
fn request_tools_match_python() {
    let fixture = fixture();
    let cases = fixture["tools"].as_array().expect("tools");
    assert!(!cases.is_empty(), "数据集为空");
    for case in cases {
        let label = case["label"].as_str().unwrap_or("");
        let messages = messages_of(&case["messages"]);
        let tools = tools_of(&case["request_tools"]);
        let options = GenerationOptions::default();
        let identity = BTreeMap::new();
        let input = ChatRequestInput {
            model: "gpt-5-codex",
            system_prompt: "",
            messages: &messages,
            tools: &tools,
            options: &options,
            profile_request_timeout_seconds: 180.0,
            prompt_cache_capable: false,
            prompt_cache_identity: &identity,
        };
        let built = Value::Array(tools_for_responses(&input));
        assert_eq!(
            built.to_string(),
            case["expected"].to_string(),
            "tools（{label}）"
        );
    }
}

#[test]
fn flattened_tool_history_matches_python() {
    let fixture = fixture();
    let cases = fixture["flatten"].as_array().expect("flatten");
    assert!(!cases.is_empty(), "数据集为空");
    for case in cases {
        let label = case["label"].as_str().unwrap_or("");
        let items = case["items"].as_array().expect("items").clone();
        let flattened = Value::Array(flatten_tool_history_to_text(&items));
        assert_eq!(
            flattened.to_string(),
            case["expected"].to_string(),
            "展平结果（{label}）"
        );
    }
}

#[test]
fn tool_history_detection_matches_python() {
    let fixture = fixture();
    let cases = fixture["history_detection"]
        .as_array()
        .expect("history_detection");
    assert!(!cases.is_empty(), "数据集为空");
    for case in cases {
        let label = case["label"].as_str().unwrap_or("");
        let items = case["items"].as_array().expect("items").clone();
        let status = case["status"].as_u64().map(|value| value as u16);
        assert_eq!(
            has_tool_history_items(&items),
            case["has_tool_history"]
                .as_bool()
                .expect("has_tool_history"),
            "工具历史判定（{label}）"
        );
        assert_eq!(
            is_tool_history_rejection(&items, status),
            case["is_rejection"].as_bool().expect("is_rejection"),
            "400 降级判定（{label}）"
        );
    }
}

#[test]
fn request_body_matches_python() {
    let fixture = fixture();
    let mut checked = 0;
    for group in ["kwargs", "kwargs_extra"] {
        for case in fixture[group].as_array().expect(group) {
            let label = case["label"].as_str().unwrap_or("");
            let messages = messages_of(&case["messages"]);
            let tools = tools_of(&case["tools"]);
            let options = options_of(&case["options"]);
            let identity: BTreeMap<String, String> = case["prompt_cache_identity"]
                .as_object()
                .expect("prompt_cache_identity")
                .iter()
                .map(|(key, value)| {
                    (
                        key.clone(),
                        value.as_str().expect("prompt_cache_identity").to_string(),
                    )
                })
                .collect();
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
            let request = build_responses_request(&input)
                .unwrap_or_else(|error| panic!("请求构建失败（{label}）：{}", error.message));
            assert_same_object(&request.body, &case["body"], label);
            assert_eq!(
                request.timeout_seconds,
                case["timeout_seconds"].as_f64().expect("timeout_seconds"),
                "传输层超时（{label}）"
            );
            checked += 1;
        }
    }
    assert!(checked > 0, "数据集为空");
}
