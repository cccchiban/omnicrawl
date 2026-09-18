//! `agent/controllers/advisor.py` 判定层的跨语言对照。
//!
//! 期望值来自 Python 真实现：`python rust/tools/gen_controllers_fixture.py` 重新生成
//! `tests/fixtures/controllers_parity.json` 的 `advisor` 段后，本套件用同一批消息分支与
//! 配置重放 Rust 实现逐条比对。模型选择、Runtime 引导与协议调用属于宿主，不在对照范围。

use omnicrawl_controllers::{advisor, ToolResult};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

const FIXTURE: &str = include_str!("fixtures/controllers_parity.json");

fn fixture() -> Value {
    serde_json::from_str(FIXTURE).expect("fixture 不是合法 JSON")
}

fn advisor_section() -> Value {
    fixture()["advisor"].clone()
}

fn strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .expect("字符串数组")
        .iter()
        .map(|item| item.as_str().expect("字符串").to_string())
        .collect()
}

fn messages(value: &Value) -> Vec<Value> {
    value.as_array().expect("消息数组").to_vec()
}

fn sha256_hex(text: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(text.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn template_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../omnicrawl/templates")
}

fn assert_result(actual: &ToolResult, expected: &Value, label: &str) {
    assert_eq!(
        actual.ok,
        expected["ok"].as_bool().expect("ok"),
        "成功标记（{label}）"
    );
    assert_eq!(
        sha256_hex(&actual.output),
        expected["output_sha256"].as_str().expect("output_sha256"),
        "输出（{label}）"
    );
    assert_eq!(
        sha256_hex(&actual.full_output),
        expected["full_output_sha256"]
            .as_str()
            .expect("full_output_sha256"),
        "展示文本（{label}）"
    );
}

#[test]
fn advisor_constants_match_python() {
    let data = advisor_section();
    let expected = &data["constants"];
    assert_eq!(
        advisor::ADVISOR_TOOL_NAME,
        expected["tool_name"].as_str().expect("tool_name")
    );
    assert_eq!(
        advisor::ADVISOR_SYSTEM_TEMPLATE_NAME,
        expected["template_name"].as_str().expect("template_name")
    );
    assert_eq!(
        advisor::ADVISOR_NUDGE_TEXT,
        expected["nudge_text"].as_str().expect("nudge_text")
    );
    assert_eq!(
        advisor::ADVISOR_EMPTY_ERROR,
        expected["empty_error"].as_str().expect("empty_error")
    );
    let prompt = advisor::advisor_system_prompt(&template_dir()).expect("读取顾问模板");
    assert_eq!(
        sha256_hex(&prompt),
        expected["system_prompt_sha256"]
            .as_str()
            .expect("system_prompt_sha256"),
        "顾问系统提示模板"
    );
    assert_eq!(
        prompt,
        expected["system_prompt"].as_str().expect("system_prompt")
    );
}

#[test]
fn advisor_branches_match_python() {
    let data = advisor_section();
    for case in data["branches"].as_array().expect("cases") {
        let label = case["label"].as_str().expect("label");
        let input = messages(&case["messages"]);
        assert_eq!(
            Value::Array(advisor::strip_inflight_advisor_call(input.clone())),
            case["stripped"],
            "剥离孤儿调用（{label}）"
        );
        assert_eq!(
            Value::Array(advisor::ensure_user_tail(input.clone())),
            case["with_user_tail"],
            "保证 user 尾（{label}）"
        );
        assert_eq!(
            Value::Array(advisor::build_advisor_branch(input)),
            case["branch"],
            "顾问分支（{label}）"
        );
    }
}

#[test]
fn advisor_inventory_matches_python() {
    let data = advisor_section();
    for case in data["inventory"].as_array().expect("cases") {
        let label = case["label"].as_str().expect("label");
        let tools: Vec<(String, String)> = case["tools"]
            .as_array()
            .expect("tools")
            .iter()
            .map(|item| {
                (
                    item["name"].as_str().expect("name").to_string(),
                    item["description"]
                        .as_str()
                        .expect("description")
                        .to_string(),
                )
            })
            .collect();
        assert_eq!(
            advisor::executor_tool_inventory(&tools),
            case["expected"].as_str().expect("expected"),
            "工具清单（{label}）"
        );
    }
}

#[test]
fn advisor_blacklist_matches_python() {
    let data = advisor_section();
    for case in data["blacklist"].as_array().expect("cases") {
        let label = case["label"].as_str().expect("label");
        assert_eq!(
            advisor::advisor_blacklisted(
                &strings(&case["disabled"]),
                case["catalog_key"].as_str().expect("catalog_key"),
                case["profile_id"].as_str().expect("profile_id"),
                case["model"].as_str().expect("model"),
            ),
            case["expected"].as_bool().expect("expected"),
            "顾问黑名单（{label}）"
        );
    }
}

#[test]
fn advisor_tool_call_matches_python() {
    let data = advisor_section();
    for case in data["tool_call"].as_array().expect("cases") {
        let label = case["label"].as_str().expect("label");
        let active = case["active"].as_bool().expect("active");
        let blacklisted = case["blacklisted"].as_bool().expect("blacklisted");
        let history = messages(&case["history"]);
        let expected_output = case["result"].as_str().expect("result");

        if !active {
            assert_eq!(expected_output, advisor::ADVISOR_DISABLED_ERROR, "{label}");
            continue;
        }
        if blacklisted {
            assert_eq!(
                expected_output,
                advisor::ADVISOR_BLACKLISTED_ERROR,
                "{label}"
            );
            continue;
        }
        if history.is_empty() {
            assert_eq!(
                expected_output,
                advisor::ADVISOR_NO_CONTEXT_ERROR,
                "{label}"
            );
            continue;
        }
        let status_text = advisor::advisor_status_text(
            case["model_key"].as_str().expect("model_key"),
            case["effort"].as_str().expect("effort"),
        );
        assert_eq!(
            vec![status_text, String::new()],
            strings(&case["statuses"]),
            "顾问状态文案（{label}）"
        );
        let expected_branches = case["branches"].as_array().expect("branches");
        assert_eq!(expected_branches.len(), 1, "分支数量（{label}）");
        assert_eq!(
            Value::Array(advisor::build_advisor_branch(history)),
            expected_branches[0],
            "转发分支（{label}）"
        );
    }
}

#[test]
fn advisor_envelope_matches_python() {
    let data = advisor_section();
    for case in data["envelope"].as_array().expect("cases") {
        let label = case["label"].as_str().expect("label");
        let usage = case["usage"].as_array().map(|items| {
            (
                items[0].as_i64().expect("input"),
                items[1].as_i64().expect("output"),
                items[2].as_i64().expect("cached"),
            )
        });
        let result = advisor::advisor_success_result(
            case["text"].as_str().expect("text"),
            case["selection"].as_str().expect("selection"),
            case["effort"].as_str().expect("effort"),
            usage,
        );
        assert_result(&result, &case["result"], label);
        assert_eq!(
            serde_json::to_string(&result.ui_artifact).expect("序列化"),
            serde_json::to_string(&case["ui_artifact"]).expect("序列化"),
            "顾问元数据（{label}）"
        );
    }
}
