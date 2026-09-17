//! 跨语言 parity：用 Python 真实现产出的期望值校验 Rust 的 usage 归一化。
//!
//! fixture 由 `rust/tools/gen_llm_usage_fixture.py` 生成，覆盖输入/输出 token 的多种
//! 字段名、缓存命中写法、推理 token 的三种来源，以及非整数取值视为缺失的判定。

use omnicrawl_llm::usage_from_openai_payload;
use serde_json::Value;

const FIXTURE: &str = include_str!("fixtures/openai_chat_usage_parity.json");

#[test]
fn usage_matches_python() {
    let fixture: Value = serde_json::from_str(FIXTURE).expect("fixture 不是合法 JSON");
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
