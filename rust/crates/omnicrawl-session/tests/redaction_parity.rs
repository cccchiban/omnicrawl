//! 凭据脱敏的跨语言 parity：期望值来自 Python `omnicrawl/common/redaction.py`。
//!
//! 六轮替换的顺序、`\b` 边界与量词下界都是语义的一部分，因此用例覆盖命中与**不该命中**两侧。

use omnicrawl_session::{redact_sensitive_text, redact_sensitive_values};
use serde_json::{json, Value};

const FIXTURE: &str = include_str!("fixtures/redaction_parity.json");

fn fixture() -> Value {
    serde_json::from_str(FIXTURE).expect("fixture 不是合法 JSON")
}

#[test]
fn text_redaction_matches_python() {
    for case in fixture()["texts"].as_array().expect("texts") {
        let text = case["input"].as_str().expect("输入是字符串");
        assert_eq!(
            json!(redact_sensitive_text(text)),
            case["expected"],
            "文本用例 {case}"
        );
    }
}

#[test]
fn value_redaction_matches_python() {
    for case in fixture()["values"].as_array().expect("values") {
        assert_eq!(
            redact_sensitive_values(&case["input"]),
            case["expected"],
            "结构用例 {case}"
        );
    }
}

#[test]
fn long_lists_are_truncated_to_hundred() {
    let fixture = fixture();
    let length = fixture["long_list"]["length"].as_u64().expect("length") as usize;
    let expected = fixture["long_list"]["expected_length"]
        .as_u64()
        .expect("expected_length") as usize;
    let items: Vec<Value> = (0..length).map(|index| json!(index)).collect();
    let redacted = redact_sensitive_values(&Value::Array(items));
    assert_eq!(
        redacted.as_array().map(Vec::len),
        Some(expected),
        "超长列表应当只保留前 {expected} 项"
    );
}
