//! 模型能力与协议解析的跨语言 parity：期望值来自 Python 真实现。
//!
//! 覆盖 `capabilities_from_mapping` / `merge_capabilities` / `to_dict` / 四个保守默认值，
//! 以及 `resolve_protocol` / `protocol_for_provider` 的选取结果与三条报错文案。
//! 对象一律按**序列化后的字符串**比对：workspace 开了 `preserve_order`，键序也是契约的一部分。

use omnicrawl_llm::{
    merge_capabilities, protocol_for_provider, resolve_protocol, ModelCapabilities,
};
use serde_json::{Map, Value};

const FIXTURE: &str = include_str!("fixtures/llm_registry_parity.json");

fn fixture() -> Value {
    serde_json::from_str(FIXTURE).expect("fixture 不是合法 JSON")
}

fn capabilities_of(value: &Value) -> ModelCapabilities {
    ModelCapabilities {
        streaming: value["streaming"].as_bool(),
        tools: value["tools"].as_bool(),
        parallel_tool_calls: value["parallel_tool_calls"].as_bool(),
        reasoning: value["reasoning"].as_bool(),
        vision: value["vision"].as_bool(),
        model_discovery: value["model_discovery"].as_bool(),
        prompt_cache: value["prompt_cache"].as_bool(),
        context_window_tokens: value["context_window_tokens"].as_i64().unwrap_or_default(),
        max_output_tokens: value["max_output_tokens"].as_i64().unwrap_or_default(),
    }
}

/// 未解析字段的序列化：None 为 null，键序与 Python 的字段声明序一致。
fn raw_json(capabilities: &ModelCapabilities) -> String {
    let mut map = Map::new();
    for (key, value) in [
        ("streaming", capabilities.streaming),
        ("tools", capabilities.tools),
        ("parallel_tool_calls", capabilities.parallel_tool_calls),
        ("reasoning", capabilities.reasoning),
        ("vision", capabilities.vision),
        ("model_discovery", capabilities.model_discovery),
        ("prompt_cache", capabilities.prompt_cache),
    ] {
        map.insert(key.to_string(), value.map_or(Value::Null, Value::Bool));
    }
    map.insert(
        "context_window_tokens".to_string(),
        Value::from(capabilities.context_window_tokens),
    );
    map.insert(
        "max_output_tokens".to_string(),
        Value::from(capabilities.max_output_tokens),
    );
    Value::Object(map).to_string()
}

#[test]
fn capability_mapping_matches_python() {
    let fixture = fixture();
    let cases = fixture["mapping"].as_array().expect("mapping");
    assert!(!cases.is_empty(), "数据集为空");
    for case in cases {
        let label = case["label"].as_str().unwrap_or("");
        let parsed = ModelCapabilities::from_mapping(&case["raw"]);
        assert_eq!(
            raw_json(&parsed),
            case["parsed"].to_string(),
            "解析结果（{label}）"
        );
        assert_eq!(
            Value::Object(parsed.to_map()).to_string(),
            case["to_dict"].to_string(),
            "to_dict（{label}）"
        );
    }
}

#[test]
fn capability_merge_matches_python() {
    let fixture = fixture();
    let cases = fixture["merge"].as_array().expect("merge");
    assert!(!cases.is_empty(), "数据集为空");
    for case in cases {
        let label = case["label"].as_str().unwrap_or("");
        let layers: Vec<Option<ModelCapabilities>> = case["layers"]
            .as_array()
            .expect("layers")
            .iter()
            .map(|layer| (!layer.is_null()).then(|| capabilities_of(layer)))
            .collect();
        let merged = merge_capabilities(&layers);
        assert_eq!(
            Value::Object(merged.to_map()).to_string(),
            case["merged"].to_string(),
            "合并结果（{label}）"
        );
    }
}

#[test]
fn conservative_defaults_match_python() {
    let fixture = fixture();
    let cases = fixture["defaults"].as_array().expect("defaults");
    assert!(!cases.is_empty(), "数据集为空");
    for case in cases {
        let label = case["label"].as_str().unwrap_or("");
        let capabilities = match label {
            "openai_chat" => ModelCapabilities::conservative_openai_chat(),
            "openai_responses" => ModelCapabilities::conservative_openai_responses(),
            "anthropic" => ModelCapabilities::conservative_anthropic(),
            "gemini" => ModelCapabilities::conservative_gemini(),
            other => panic!("数据集里的默认值没有对应实现：{other}"),
        };
        assert_eq!(
            Value::Object(capabilities.to_map()).to_string(),
            case["to_dict"].to_string(),
            "保守默认值（{label}）"
        );
    }
}

#[test]
fn protocol_resolution_matches_python() {
    let fixture = fixture();
    let cases = fixture["protocols"].as_array().expect("protocols");
    assert!(!cases.is_empty(), "数据集为空");
    for case in cases {
        let label = case["label"].as_str().unwrap_or("");
        let provider = case["provider"].as_str().expect("provider");
        let default_protocol = case["default_protocol"].as_str().expect("default_protocol");
        let requested = case["requested"].as_str().expect("requested");
        match resolve_protocol(provider, default_protocol, requested) {
            Ok(protocol) => assert_eq!(
                protocol.as_str(),
                case["protocol"].as_str().unwrap_or_default(),
                "选定协议（{label}）"
            ),
            Err(error) => assert_eq!(
                error.message,
                case["error"].as_str().unwrap_or_default(),
                "报错文案（{label}）"
            ),
        }
    }
}

#[test]
fn provider_default_protocol_matches_python() {
    let fixture = fixture();
    let cases = fixture["provider_defaults"]
        .as_array()
        .expect("provider_defaults");
    assert!(!cases.is_empty(), "数据集为空");
    for case in cases {
        let label = case["label"].as_str().unwrap_or("");
        let provider = case["provider"].as_str().expect("provider");
        let preferred = case["preferred"].as_str().expect("preferred");
        match protocol_for_provider(provider, preferred) {
            Ok(protocol) => assert_eq!(
                protocol.as_str(),
                case["protocol"].as_str().unwrap_or_default(),
                "Provider 默认协议（{label}）"
            ),
            Err(error) => assert_eq!(
                error.message,
                case["error"].as_str().unwrap_or_default(),
                "报错文案（{label}）"
            ),
        }
    }
}
