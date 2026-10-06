//! `controllers/turn/loop.py` 回合文本判定与收尾消息的跨语言对照。
//!
//! 数据集是冻结的对照契约：`python rust/tools/gen_controllers_fixture.py` 重新生成
//! `tests/fixtures/controllers_parity.json` 的 `turn_text` 段（直接调 `TurnLoopMixin` 的
//! 静态/实例方法）。本套件用同一批输入重放 Rust 实现，逐项比对文案与消息形状。

use omnicrawl_controllers::turn::turn_text::{
    assistant_message, cancelled_turn_summary, is_continue_last_task_request,
    resolve_continue_request,
};
use serde_json::Value;

const FIXTURE: &str = include_str!("fixtures/controllers_parity.json");

fn section() -> Value {
    let fixture: Value = serde_json::from_str(FIXTURE).expect("fixture 不是合法 JSON");
    fixture["turn_text"].clone()
}

fn strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .expect("字符串数组")
        .iter()
        .map(|item| item.as_str().expect("字符串").to_string())
        .collect()
}

#[test]
fn continue_detection_matches_python() {
    let data = section();
    for case in data["continue"].as_array().expect("continue") {
        let text = case["text"].as_str().expect("text");
        assert_eq!(
            is_continue_last_task_request(text),
            case["expected"].as_bool().expect("expected"),
            "继续识别（{text:?}）"
        );
    }
}

#[test]
fn resolve_continue_matches_python() {
    let data = section();
    for case in data["resolve"].as_array().expect("resolve") {
        let text = case["text"].as_str().expect("text");
        assert_eq!(
            resolve_continue_request(text, case["pending"].as_str()),
            case["resolved"].as_str().expect("resolved"),
            "任务还原（{text:?}）"
        );
    }
}

#[test]
fn cancelled_summary_matches_python() {
    let data = section();
    for case in data["summary"].as_array().expect("summary") {
        let label = case["label"].as_str().expect("label");
        let expected = case["summary"].as_str().expect("summary");
        // 空数组代表「有快照但没执行工具」，与 null（没有快照）是两种文案。
        let tools = if case["executed_tools"].is_null() {
            None
        } else {
            Some(strings(&case["executed_tools"]))
        };
        assert_eq!(
            cancelled_turn_summary(tools.as_deref()),
            expected,
            "取消摘要（{label}）"
        );
    }
}

#[test]
fn assistant_message_matches_python() {
    let data = section();
    for case in data["message"].as_array().expect("message") {
        let label = case["label"].as_str().expect("label");
        let content = case["content"].as_str().expect("content");
        let reasoning = case["reasoning"].as_str().expect("reasoning");
        assert_eq!(
            assistant_message(content, reasoning),
            case["message"],
            "助手消息（{label}）"
        );
    }
}
