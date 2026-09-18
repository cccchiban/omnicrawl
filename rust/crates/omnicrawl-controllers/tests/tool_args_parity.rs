//! `agent/toolkit/{tools,host_tools}.py` 参数层的跨语言对照。
//!
//! 期望值来自 Python 真实现：`python rust/tools/gen_controllers_fixture.py` 重新生成
//! `tests/fixtures/controllers_parity.json` 的 `tool_args` 段后，本套件用同一批工具 schema 与
//! 参数重放 Rust 实现逐字段比对。

use omnicrawl_controllers::tool_args;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

const FIXTURE: &str = include_str!("fixtures/controllers_parity.json");

fn fixture() -> Value {
    serde_json::from_str(FIXTURE).expect("fixture 不是合法 JSON")
}

fn section() -> Value {
    fixture()["tool_args"].clone()
}

fn object(value: &Value) -> Map<String, Value> {
    value.as_object().cloned().expect("对象参数")
}

fn strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .expect("字符串数组")
        .iter()
        .map(|item| item.as_str().expect("字符串").to_string())
        .collect()
}

fn sorted_strings(value: &Value) -> Vec<String> {
    let mut items = strings(value);
    items.sort();
    items
}

fn serialized(value: &Value) -> String {
    serde_json::to_string(value).expect("序列化")
}

fn pairs_from(value: &Value) -> Vec<(String, String)> {
    let mut pairs: Vec<(String, String)> = value
        .as_object()
        .expect("别名表")
        .iter()
        .map(|(key, item)| (key.clone(), item.as_str().expect("目标名").to_string()))
        .collect();
    pairs.sort();
    pairs
}

fn sha256_hex(text: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(text.as_bytes());
    format!("{:x}", hasher.finalize())
}

#[test]
fn tool_args_constants_match_python() {
    let data = section();
    let expected = &data["constants"];
    assert_eq!(
        tool_args::TODO_TOOL_NAME,
        expected["todo_tool_name"].as_str().expect("todo")
    );
    assert_eq!(
        tool_args::ASK_USER_TOOL_NAME,
        expected["ask_user_tool_name"].as_str().expect("ask_user")
    );
    assert_eq!(
        tool_args::PAUSE_WORK_TOOL_NAME,
        expected["pause_work_tool_name"]
            .as_str()
            .expect("pause_work")
    );
    assert_eq!(
        tool_args::ADVISOR_TOOL_NAME,
        expected["advisor_tool_name"].as_str().expect("advisor")
    );
    assert_eq!(
        tool_args::INVOKE_TOOL_NAME,
        expected["invoke_tool_name"].as_str().expect("invoke_tool")
    );

    let mut tool_aliases: Vec<(String, String)> = tool_args::TOOL_NAME_ALIASES
        .iter()
        .map(|(alias, target)| (alias.to_string(), target.to_string()))
        .collect();
    tool_aliases.sort();
    assert_eq!(tool_aliases, pairs_from(&expected["tool_name_aliases"]));

    let mut argument_aliases: Vec<(String, String)> = tool_args::ARGUMENT_NAME_ALIASES
        .iter()
        .map(|(alias, target)| (alias.to_string(), target.to_string()))
        .collect();
    argument_aliases.sort();
    assert_eq!(
        argument_aliases,
        pairs_from(&expected["argument_name_aliases"])
    );
}

#[test]
fn tool_args_identifier_matches_python() {
    let data = section();
    for case in data["identifier"].as_array().expect("cases") {
        let value = case["value"].as_str().expect("value");
        assert_eq!(
            tool_args::normalize_identifier(value),
            case["expected"].as_str().expect("expected"),
            "标识符归一化（{value:?}）"
        );
    }
}

#[test]
fn tool_args_tool_names_match_python() {
    let data = section();
    for case in data["tool_names"].as_array().expect("cases") {
        let label = case["label"].as_str().expect("label");
        let tools = strings(&case["tools"]);
        let refs: Vec<&str> = tools.iter().map(String::as_str).collect();
        assert_eq!(
            tool_args::normalize_tool_name(case["raw_name"].as_str().expect("raw_name"), &refs),
            case["expected"].as_str().expect("expected"),
            "工具名归一化（{label}）"
        );
    }
}

#[test]
fn tool_args_argument_keys_match_python() {
    let data = section();
    for case in data["argument_keys"].as_array().expect("cases") {
        let name = case["tool_name"].as_str().expect("tool_name");
        let schema = case["schema"].as_str().expect("schema");
        assert_eq!(
            sorted_strings(&Value::Array(
                tool_args::tool_argument_keys(name, &[(name, schema)])
                    .into_iter()
                    .map(Value::from)
                    .collect()
            )),
            sorted_strings(&case["expected"]),
            "参数名集合（{name}）"
        );
    }
    for case in data["blank_keys"].as_array().expect("cases") {
        let name = case["tool_name"].as_str().expect("tool_name");
        let schema = case["schema"].as_str().expect("schema");
        assert_eq!(
            sorted_strings(&Value::Array(
                tool_args::optional_blank_ignored_keys(name, &[(name, schema)])
                    .into_iter()
                    .map(Value::from)
                    .collect()
            )),
            sorted_strings(&case["expected"]),
            "可选空串字段（{name}）"
        );
    }
}

#[test]
fn tool_args_normalize_matches_python() {
    let data = section();
    for case in data["normalize_arguments"].as_array().expect("cases") {
        let label = case["label"].as_str().expect("label");
        let name = case["tool_name"].as_str().expect("tool_name");
        let schema = tool_schema(name);
        let arguments = object(&case["arguments"]);
        let tools: Vec<(&str, &str)> = schema
            .map(|schema| vec![(name, schema)])
            .unwrap_or_default();
        let normalized = tool_args::normalize_tool_arguments(name, &arguments, &tools);
        assert_eq!(
            serialized(&Value::Object(normalized)),
            serialized(&case["expected"]),
            "参数归一化（{label}）"
        );
    }
    for case in data["normalize_call"].as_array().expect("cases") {
        let label = case["label"].as_str().expect("label");
        let name = case["name"].as_str().expect("name");
        let tools: Vec<(&str, &str)> = tool_schema(name)
            .map(|schema| vec![(name, schema)])
            .unwrap_or_default();
        let arguments = object(&case["arguments"]);
        let (normalized_name, normalized_arguments) =
            tool_args::normalize_tool_call(name, &arguments, &tools);
        assert_eq!(
            normalized_name,
            case["expected_name"].as_str().expect("expected_name"),
            "调用名归一化（{label}）"
        );
        assert_eq!(
            serialized(&Value::Object(normalized_arguments)),
            serialized(&case["expected_arguments"]),
            "调用参数归一化（{label}）"
        );
    }
}

/// 数据集里用到的工具 schema（与生成器中的 TOOL_SCHEMAS 保持一致）。
fn tool_schema(name: &str) -> Option<&'static str> {
    match name {
        "read" => Some(
            r#"{"type":"object","properties":{"path":{"type":"string","minLength":1},"start_line":{"type":"integer"},"max_lines":{"type":"integer"},"note":{"type":"string"}},"required":["path"]}"#,
        ),
        "grep" => Some(
            r#"{"type":"object","properties":{"pattern":{"type":"string","minLength":1},"path":{"type":"string"},"context_lines":{"type":"integer"},"case_sensitive":{"type":"boolean"}},"required":["pattern"]}"#,
        ),
        "write" => Some(
            r#"{"type":"object","properties":{"path":{"type":"string","minLength":1},"content":{"type":"string"},"comment":{"type":"string","minLength":1}},"required":["path"]}"#,
        ),
        "bash" => Some(
            r#"{"type":"object","properties":{"command":{"type":"string","minLength":1},"timeout_seconds":{"type":"integer","minimum":1,"maximum":3600}},"required":["command"],"additionalProperties":false}"#,
        ),
        _ => None,
    }
}

#[test]
fn tool_args_public_projection_matches_python() {
    let data = section();
    for case in data["public_arguments"].as_array().expect("cases") {
        let label = case["label"].as_str().expect("label");
        let actual = tool_args::public_tool_arguments(
            case["tool_name"].as_str().expect("tool_name"),
            &object(&case["arguments"]),
        );
        assert_eq!(
            serialized(&actual),
            serialized(&case["expected"]),
            "参数投影（{label}）"
        );
    }
}

#[test]
fn tool_args_compact_matches_python() {
    let data = section();
    for case in data["compact_schema"].as_array().expect("cases") {
        let label = case["label"].as_str().expect("label");
        let schema = match &case["schema"] {
            Value::String(text) => text.clone(),
            other => serialized(other),
        };
        assert_eq!(
            serialized(&tool_args::compact_tool_schema(&schema)),
            serialized(&case["expected"]),
            "Schema 压缩（{label}）"
        );
    }
    for case in data["compact_description"].as_array().expect("cases") {
        assert_eq!(
            tool_args::compact_tool_description(case["value"].as_str().expect("value")),
            case["expected"].as_str().expect("expected")
        );
    }
}

#[test]
fn tool_args_validation_matches_python() {
    let data = section();
    for case in data["validate"].as_array().expect("cases") {
        let label = case["label"].as_str().expect("label");
        let schema = case["schema"].as_str().expect("schema");
        let issues = tool_args::validate_tool_arguments(schema, &case["arguments"]);
        let actual: Vec<Value> = issues
            .iter()
            .map(|issue| {
                let mut map = Map::new();
                map.insert("path".to_string(), Value::from(issue.path.clone()));
                map.insert("message".to_string(), Value::from(issue.message.clone()));
                Value::Object(map)
            })
            .collect();
        assert_eq!(
            serialized(&Value::Array(actual)),
            serialized(&case["issues"]),
            "参数校验（{label}）"
        );
        let result = tool_args::tool_validation_error_result("probe", schema, &issues);
        assert_eq!(
            result.output,
            case["error_result"].as_str().expect("error_result"),
            "校验失败信封（{label}）"
        );
    }
}

#[test]
fn tool_args_envelopes_match_python() {
    let data = section();
    for case in data["envelopes"].as_array().expect("cases") {
        let label = case["label"].as_str().expect("label");
        let extra = case["extra"].as_object();
        let result = tool_args::error_result(
            case["code"].as_str().expect("code"),
            case["message"].as_str().expect("message"),
            case["tool_name"].as_str().expect("tool_name"),
            case["retryable"].as_bool().expect("retryable"),
            extra,
        );
        assert_eq!(
            result.output,
            case["expected"].as_str().expect("expected"),
            "错误信封（{label}）"
        );
        assert!(!result.ok, "错误信封恒为失败（{label}）");
    }
}

#[test]
fn tool_args_mcp_envelopes_match_python() {
    let data = section();
    for case in data["mcp"].as_array().expect("cases") {
        let label = case["label"].as_str().expect("label");
        let expected = &case["result"];
        let expected_output = expected["output"].as_str().expect("output");
        if expected_output == "arguments 必须是 JSON 对象。" {
            let result = tool_args::mcp_prompt_arguments_error();
            assert!(!result.ok, "参数错误信封（{label}）");
            assert_eq!(result.output, expected_output, "参数错误文案（{label}）");
            continue;
        }
        let fields = &case["fields"];
        let item_name = fields
            .get("tool_name")
            .or_else(|| fields.get("uri"))
            .or_else(|| fields.get("prompt_name"))
            .and_then(Value::as_str)
            .expect("item name")
            .to_string();
        let payload = tool_args::mcp_fields(
            fields["ok"].as_bool().expect("ok"),
            fields["server_name"].as_str().expect("server_name"),
            &item_name,
            fields["audit_id"].as_str().unwrap_or_default(),
            fields["duration_ms"].as_i64().expect("duration_ms"),
            fields["error_code"].as_str().unwrap_or_default(),
            fields["retryable"].as_bool().expect("retryable"),
            fields["output"].as_str().expect("output"),
            fields["full_output"].as_str().unwrap_or_default(),
        );
        let result = match case["label"].as_str().expect("label") {
            "resource" => tool_args::mcp_resource_result(payload),
            "prompt" => tool_args::mcp_prompt_result(payload),
            _ => tool_args::mcp_tool_result(payload),
        };
        assert_eq!(result.ok, expected["ok"].as_bool().expect("ok"), "{label}");
        assert_eq!(
            sha256_hex(&result.output),
            expected["output_sha256"].as_str().expect("output_sha256"),
            "输出（{label}）"
        );
        assert_eq!(
            sha256_hex(&result.full_output),
            expected["full_output_sha256"]
                .as_str()
                .expect("full_output_sha256"),
            "展示文本（{label}）"
        );
    }
}

#[test]
fn tool_args_read_helpers_match_python() {
    let data = section();
    let helpers = &data["read_helpers"];

    for case in helpers["required_list"].as_array().expect("cases") {
        let arguments = object(&case["arguments"]);
        assert_eq!(
            tool_args::read_required_string_list(&arguments, "items"),
            strings(&case["expected"])
        );
    }
    for case in helpers["optional_list"].as_array().expect("cases") {
        let arguments = object(&case["arguments"]);
        assert_eq!(
            tool_args::read_optional_string_list(&arguments, "items"),
            case["expected"].as_array().map(|items| {
                items
                    .iter()
                    .map(|item| item.as_str().unwrap_or_default().to_string())
                    .collect::<Vec<_>>()
            })
        );
    }
    for case in helpers["limited_int"].as_array().expect("cases") {
        let arguments = object(&case["arguments"]);
        assert_eq!(
            tool_args::read_limited_int(
                &arguments,
                "max_events",
                case["default"].as_i64().expect("default"),
                case["maximum"].as_i64().expect("maximum")
            ),
            case["expected"].as_i64().expect("expected"),
            "限值整数（{:?}）",
            case["arguments"]
        );
    }
    for case in helpers["json_result"].as_array().expect("cases") {
        assert_eq!(
            tool_args::json_tool_result(&case["data"]).output,
            case["expected"].as_str().expect("expected")
        );
    }
    for case in helpers["bounded_int"].as_array().expect("cases") {
        let value = case["value"].as_i64().map(Value::from);
        assert_eq!(
            tool_args::bounded_int(
                value.as_ref(),
                case["default"].as_i64().expect("default"),
                case["minimum"].as_i64().expect("minimum"),
                case["maximum"].as_i64().expect("maximum")
            ),
            case["expected"].as_i64().expect("expected"),
            "目录有界整数（{value:?}）"
        );
    }
}
