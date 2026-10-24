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

/// 大文本的脱敏必须是线性时间：工具输出动辄几百 KB，写入转录时每条 `tool_result`
/// 都会走两遍这条路径。
///
/// 曾经的最内层在每个字节位置都截取 32 个字符、转小写、造两个临时串，192 KB 的输出
/// 要 7 秒，一次会话写入几十条就把界面拖死。这里用 256 KB 的纯文本钉住复杂度：
/// 线性实现是百毫秒级，退化成「每字符一次分配」就会明显超时。
#[test]
fn large_text_redaction_stays_linear() {
    let text = "输".repeat(128 * 1024);
    let started = std::time::Instant::now();
    let redacted = redact_sensitive_text(&text);
    let elapsed = started.elapsed();
    assert_eq!(redacted, text, "没有命中任何规则时应当原样返回");
    assert!(
        elapsed < std::time::Duration::from_secs(3),
        "256 KB 文本脱敏耗时 {elapsed:?}，疑似退化为逐字符分配"
    );
}
