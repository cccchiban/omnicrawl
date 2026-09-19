//! `agent/subagents/read_only_commands.py` 判定构件的跨语言对照。
//!
//! 期望值来自 Python 真实现：`python rust/tools/gen_controllers_fixture.py` 重新生成
//! `tests/fixtures/controllers_parity.json` 的 `read_only_commands` 段（直接调私有判定函数）。
//! 本套件用同一批输入重放 Rust 实现，比对可执行名、命令分段与两类写入判定。

use omnicrawl_controllers::subagents::read_only::{
    browser_cli_denial_reason, curl_denial_reason, normalized_executable, split_shell_segments,
};
use serde_json::Value;

const FIXTURE: &str = include_str!("fixtures/controllers_parity.json");

fn section() -> Value {
    let fixture: Value = serde_json::from_str(FIXTURE).expect("fixture 不是合法 JSON");
    fixture["read_only_commands"].clone()
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
fn normalized_executable_matches_python() {
    let data = section();
    for case in data["normalized"].as_array().expect("normalized") {
        let token = case["token"].as_str().expect("token");
        assert_eq!(
            normalized_executable(token),
            case["expected"].as_str().expect("expected"),
            "可执行名归一化（{token:?}）"
        );
    }
}

#[test]
fn split_shell_segments_matches_python() {
    let data = section();
    for case in data["split"].as_array().expect("split") {
        let command = case["command"].as_str().expect("command");
        let expected_error = case["error"].as_str().expect("error");
        match split_shell_segments(command) {
            Ok(segments) => {
                assert!(
                    expected_error.is_empty(),
                    "Python 侧报错（{command:?}）：{expected_error}"
                );
                assert_eq!(segments, strings(&case["segments"]), "分段（{command:?}）");
            }
            Err(error) => assert_eq!(error, expected_error, "拒绝原因（{command:?}）"),
        }
    }
}

#[test]
fn curl_denial_reason_matches_python() {
    let data = section();
    for case in data["curl"].as_array().expect("curl") {
        let label = case["label"].as_str().expect("label");
        let arguments = strings(&case["arguments"]);
        assert_eq!(
            curl_denial_reason(&arguments).unwrap_or_default(),
            case["reason"].as_str().expect("reason"),
            "curl 写入判定（{label}）"
        );
    }
}

#[test]
fn browser_cli_denial_reason_matches_python() {
    let data = section();
    for case in data["browser"].as_array().expect("browser") {
        let label = case["label"].as_str().expect("label");
        let arguments = strings(&case["arguments"]);
        assert_eq!(
            browser_cli_denial_reason(&arguments).unwrap_or_default(),
            case["reason"].as_str().expect("reason"),
            "浏览器 CLI 判定（{label}）"
        );
    }
}
