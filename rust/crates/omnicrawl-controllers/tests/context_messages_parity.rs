//! 上下文消息装配的跨语言对照。
//!
//! 期望值来自 Python 真实现：`python rust/tools/gen_controllers_fixture.py` 重新生成
//! `tests/fixtures/controllers_parity.json` 的 `context_messages` 段——项目规范消息由真函数产出，
//! 插件附加上下文则由探针真跑 `_context_messages`（基线消息用桩替换）得到。本套件用同一批输入
//! 重放 Rust 实现，逐项比对消息形状、顺序与收尾载荷。

use omnicrawl_controllers::turn::context_messages::{
    context_build_after_payload, plugin_context_messages, project_instructions_messages,
};
use serde_json::{json, Value};

const FIXTURE: &str = include_str!("fixtures/controllers_parity.json");

fn section() -> Value {
    let fixture: Value = serde_json::from_str(FIXTURE).expect("fixture 不是合法 JSON");
    fixture["context_messages"].clone()
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

#[test]
fn plugin_context_injection_matches_python() {
    let data = section();
    for case in data["inject"].as_array().expect("inject") {
        let label = case["label"].as_str().expect("label");
        // 与生成器里的基线保持一致：插件附加上下文一律追加在基线消息之后。
        let mut produced = vec![json!({"role": "user", "content": "基线消息"})];
        produced.extend(plugin_context_messages(Some(&case["additional"])));
        assert_eq!(
            Value::Array(produced.clone()),
            case["messages"],
            "注入结果（{label}）"
        );

        let trace = strings(&case["trace"]);
        assert_eq!(
            trace.first().map(String::as_str),
            Some("context.build.before"),
            "构建前钩子（{label}）"
        );
        assert_eq!(
            trace.last().map(String::as_str),
            Some("context.build.after"),
            "构建后钩子（{label}）"
        );
        assert_eq!(
            context_build_after_payload(produced.len()),
            case["after_payload"],
            "收尾载荷（{label}）"
        );
    }
}
