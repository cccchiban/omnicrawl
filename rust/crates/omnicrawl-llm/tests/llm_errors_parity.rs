//! 错误分类（`omnicrawl/llm/errors.py` 的 `map_openai_exception`）的跨语言 parity。
//!
//! 期望值来自 Python 真实现：数据集记录分类函数真正读到的字段（错误文本、类型名、
//! `exc.body`、`exc.response.json()`、状态码）与它给出的码 / 文案 / 可重试标记 / 状态码。

use omnicrawl_llm::{map_exception, ExceptionView};
use serde_json::Value;

const FIXTURE: &str = include_str!("fixtures/llm_errors_parity.json");

#[test]
fn error_classification_matches_python() {
    let fixture: Value = serde_json::from_str(FIXTURE).expect("fixture 不是合法 JSON");
    let cases = fixture["cases"].as_array().expect("cases");
    assert!(!cases.is_empty(), "数据集为空");

    for case in cases {
        let label = case["label"].as_str().unwrap_or("");
        let known_models: Vec<&str> = case["known_models"]
            .as_array()
            .expect("known_models")
            .iter()
            .map(|item| item.as_str().expect("known_models"))
            .collect();
        let view = ExceptionView {
            message: case["message"].as_str().expect("message"),
            type_name: case["type_name"].as_str().expect("type_name"),
            body: case.get("body").filter(|value| !value.is_null()),
            response_json: case.get("response_json").filter(|value| !value.is_null()),
            status_code: case["status_code"].as_i64(),
            response_status_code: case["response_status_code"].as_i64(),
        };
        let mapped = map_exception(&view, &known_models);
        let expected = &case["expected"];
        assert_eq!(
            mapped.code.as_str(),
            expected["code"].as_str().expect("code"),
            "分类码（{label}）"
        );
        assert_eq!(
            mapped.message,
            expected["message"].as_str().expect("message"),
            "文案（{label}）"
        );
        assert_eq!(
            mapped.retryable,
            expected["retryable"].as_bool().expect("retryable"),
            "可重试标记（{label}）"
        );
        assert_eq!(
            mapped.status_code.map(u64::from),
            expected["status_code"].as_u64(),
            "状态码（{label}）"
        );
    }
}
