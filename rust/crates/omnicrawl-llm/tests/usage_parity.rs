//! 跨语言 parity：用 Python 真实现产出的期望值校验 Rust 的 usage 归一化。
//!
//! fixture 是冻结的对照契约，覆盖输入/输出 token 的多种
//! 字段名、缓存命中写法、推理 token 的三种来源，以及非整数取值视为缺失的判定；
//! 数值边界（负值不归零）单独成组，见 `openai_chat_usage_boundary_parity.json`。

use omnicrawl_llm::usage_from_openai_payload;
use serde_json::Value;

const FIXTURE: &str = include_str!("fixtures/openai_chat_usage_parity.json");
const BOUNDARY_FIXTURE: &str = include_str!("fixtures/openai_chat_usage_boundary_parity.json");

fn assert_fixture_matches(fixture: &str) {
    let fixture: Value = serde_json::from_str(fixture).expect("fixture 不是合法 JSON");
    for case in fixture["usage"].as_array().expect("缺少 usage") {
        let actual = serde_json::to_value(usage_from_openai_payload(&case["payload"]))
            .expect("用量可序列化");
        assert_eq!(
            actual, case["expected"],
            "用量用例 {} 不一致",
            case["payload"]
        );
    }
}

#[test]
fn usage_matches_python() {
    assert_fixture_matches(FIXTURE);
}

/// 数值边界：Python 的 `TokenUsage` 是有符号普通 `int`，负值原样保留、不在此层归零。
#[test]
fn usage_boundaries_match_python() {
    assert_fixture_matches(BOUNDARY_FIXTURE);
}
