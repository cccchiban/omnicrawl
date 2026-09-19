//! `controllers/turn/loop.py` 视觉能力判定的跨语言对照。
//!
//! 期望值来自 Python 真实现：`python rust/tools/gen_controllers_fixture.py` 重新生成
//! `tests/fixtures/controllers_parity.json` 的 `vision_capability` 段——用不同形状的运行时
//! 快照与 `config.llm.native_vision` 组合真跑判定。本套件用同一批输入重放 Rust 实现。

use omnicrawl_controllers::turn::turn_loop::{model_supports_vision, native_vision_enabled};
use serde_json::Value;

const FIXTURE: &str = include_str!("fixtures/controllers_parity.json");

fn section() -> Value {
    let fixture: Value = serde_json::from_str(FIXTURE).expect("fixture 不是合法 JSON");
    fixture["vision_capability"].clone()
}

#[test]
fn model_supports_vision_matches_python() {
    let data = section();
    for case in data["support"].as_array().expect("support") {
        let label = case["label"].as_str().expect("label");
        assert_eq!(
            model_supports_vision(case["capability"].as_bool()),
            case["expected"].as_bool().expect("expected"),
            "运行时视觉能力（{label}）"
        );
    }
}

#[test]
fn native_vision_enabled_matches_python() {
    let data = section();
    for case in data["native"].as_array().expect("native") {
        let label = case["label"].as_str().expect("label");
        let supports = model_supports_vision(case["capability"].as_bool());
        assert_eq!(
            case["supports_vision"].as_bool().expect("supports_vision"),
            supports,
            "能力前提（{label}）"
        );
        assert_eq!(
            native_vision_enabled(case["override"].as_bool(), supports),
            case["expected"].as_bool().expect("expected"),
            "原生视觉开关（{label}）"
        );
    }
}
