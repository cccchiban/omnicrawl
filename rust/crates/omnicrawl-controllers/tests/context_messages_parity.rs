//! 上下文消息装配的跨语言对照。
//!
//! 数据集是冻结的对照契约：`python rust/tools/gen_controllers_fixture.py` 重新生成
//! `tests/fixtures/controllers_parity.json` 的 `context_messages` 段——项目规范消息由真函数产出。
//! 本套件用同一批输入重放 Rust 实现，逐项比对消息形状与顺序。

use omnicrawl_controllers::turn::context_messages::project_instructions_messages;
use serde_json::Value;

const FIXTURE: &str = include_str!("fixtures/controllers_parity.json");

fn section() -> Value {
    let fixture: Value = serde_json::from_str(FIXTURE).expect("fixture 不是合法 JSON");
    fixture["context_messages"].clone()
}

#[test]
fn project_instructions_messages_match_python() {
    let data = section();
    for case in data["project"].as_array().expect("project") {
        let label = case["label"].as_str().expect("label");
        let produced =
            project_instructions_messages(case["instructions"].as_str().expect("instructions"));
        assert_eq!(
            Value::Array(produced),
            case["messages"],
            "项目规范消息（{label}）"
        );
    }
}
