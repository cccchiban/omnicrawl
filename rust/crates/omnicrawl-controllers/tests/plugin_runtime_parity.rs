//! `controllers/plugins.py` 运行期编排的跨语言对照。
//!
//! 数据集是冻结的对照契约：`python rust/tools/gen_controllers_fixture.py` 重新生成
//! `tests/fixtures/controllers_parity.json` 的 `plugin_runtime` 段——探针用不同形状的
//! PluginManager 真跑冻结、回合钩子与会话生命周期钩子。本套件用同一批输入重放 Rust 的
//! 决策，逐项比对（含「标准上下文原样采用、普通对象才归一化」这条差别）。

use omnicrawl_controllers::plugins::{
    frozen_dispatch_context, session_hook_id, session_lifecycle_hooks, turn_hook_action,
    FrozenShape, TurnHookAction,
};
use serde_json::Value;

const FIXTURE: &str = include_str!("fixtures/controllers_parity.json");

/// 与生成器里 `_Probe.current_session_id` 保持一致。
const CURRENT_SESSION_ID: &str = "current-session";

fn section() -> Value {
    let fixture: Value = serde_json::from_str(FIXTURE).expect("fixture 不是合法 JSON");
    fixture["plugin_runtime"].clone()
}

fn strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .expect("字符串数组")
        .iter()
        .map(|item| item.as_str().expect("字符串").to_string())
        .collect()
}

fn optional_strings(value: &Value) -> Option<Vec<String>> {
    if value.is_null() {
        return None;
    }
    Some(strings(value))
}

#[test]
fn frozen_context_matches_python() {
    let data = section();
    for case in data["context"].as_array().expect("context") {
        let label = case["label"].as_str().expect("label");
        let standard = &case["standard_context"];
        let raw_handlers = optional_strings(&case["raw_handlers"]);
        let raw_source = case["raw_source"].as_str();

        let shape = if standard.is_null() {
            FrozenShape::Missing
        } else if standard.as_bool().expect("standard_context") {
            FrozenShape::Standard {
                handlers: raw_handlers.as_deref().unwrap_or(&[]),
                source: raw_source.unwrap_or(""),
            }
        } else {
            FrozenShape::Object {
                handlers: raw_handlers.as_deref(),
                source: raw_source,
            }
        };

        let context = frozen_dispatch_context(shape);
        assert_eq!(
            context.handlers,
            strings(&case["handlers"]),
            "handlers（{label}）"
        );
        assert_eq!(
            context.source,
            case["source"].as_str().expect("source"),
            "source（{label}）"
        );

        let freeze_called = strings(&case["trace"]).iter().any(|item| item == "freeze");
        assert_eq!(
            freeze_called,
            !standard.is_null(),
            "只有拿到管理器时才调用 freeze（{label}）"
        );
    }
}

#[test]
fn turn_hook_action_matches_python() {
    let data = section();
    for case in data["turn"].as_array().expect("turn") {
        let label = case["label"].as_str().expect("label");
        let manager_kind = case["manager_kind"].as_str().expect("manager_kind");
        let hook_available = case["hook_available"].as_bool().expect("hook_available");
        let trace = strings(&case["trace"]);

        match turn_hook_action(manager_kind != "none", hook_available) {
            TurnHookAction::Skip => {
                assert!(trace.is_empty(), "跳过时不应调用回合钩子（{label}）");
            }
            TurnHookAction::Call => {
                assert_eq!(
                    trace,
                    ["begin_turn", "end_turn"],
                    "回合钩子调用顺序（{label}）"
                );
                assert!(
                    case["begin_error"].is_null() && case["end_error"].is_null(),
                    "钩子异常必须被吞掉（{label}）"
                );
            }
        }
    }
}

#[test]
fn session_lifecycle_hooks_match_python() {
    let data = section();
    for case in data["session"].as_array().expect("session") {
        let label = case["label"].as_str().expect("label");
        let trace = strings(&case["trace"]);
        if !case["state_present"].as_bool().expect("state_present") {
            assert!(trace.is_empty(), "没有会话状态时不发 Hook（{label}）");
            continue;
        }

        let resume_session_id = case["resume_session_id"]
            .as_str()
            .expect("resume_session_id");
        let session_id = session_hook_id(case["state_session_id"].as_str(), CURRENT_SESSION_ID);
        let (before, after) = session_lifecycle_hooks(resume_session_id);
        let mut expected: Vec<String> = Vec::new();
        if !before.is_empty() {
            expected.push(format!("{before}|{session_id}"));
        }
        expected.push(format!("{after}|{session_id}"));

        assert_eq!(expected, trace, "会话生命周期 Hook（{label}）");
    }
}
