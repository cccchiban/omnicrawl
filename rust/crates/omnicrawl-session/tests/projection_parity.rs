//! 会话投影的跨语言 parity：期望值来自 Python `omnicrawl/state/session_projection.py`。
//!
//! 覆盖回退过滤、标题投影、运行护栏状态、事件 → 模型消息、工具结果消息与协议配对补全。

use omnicrawl_session::{
    active_session_events, apply_run_guard_event, complete_tool_pairing, event_to_model_message,
    format_tool_result_content, function_tool_call, interrupted_tool_result_message,
    recover_run_guard_state, session_title_from_events, tool_result_message,
    tool_result_output_text, SessionEvent,
};
use serde_json::{json, Map, Value};

const FIXTURE: &str = include_str!("fixtures/session_projection_parity.json");

fn fixture() -> Value {
    serde_json::from_str(FIXTURE).expect("fixture 不是合法 JSON")
}

fn section(fixture: &Value, name: &str) -> Vec<Value> {
    fixture[name]
        .as_array()
        .unwrap_or_else(|| panic!("fixture 缺少 {name}"))
        .clone()
}

fn events(case: &Value) -> Vec<SessionEvent> {
    case["events"]
        .as_array()
        .expect("用例缺少 events")
        .iter()
        .map(|item| SessionEvent::from_dict(item).expect("事件可解析"))
        .collect()
}

fn payload_of(value: &Value) -> Map<String, Value> {
    value.as_object().cloned().unwrap_or_default()
}

#[test]
fn active_events_match_python() {
    for case in section(&fixture(), "active_events") {
        let ids: Vec<Value> = active_session_events(&events(&case))
            .iter()
            .map(|event| json!(event.event_id))
            .collect();
        assert_eq!(json!(ids), case["expected_ids"], "用例 {}", case["name"]);
    }
}

#[test]
fn titles_match_python() {
    for case in section(&fixture(), "titles") {
        let title = session_title_from_events(&events(&case), "新会话");
        assert_eq!(json!(title), case["expected"], "用例 {}", case["name"]);
    }
}

#[test]
fn run_guard_state_matches_python() {
    for case in section(&fixture(), "run_guard") {
        let (pending, todos) = recover_run_guard_state(&events(&case));
        assert_eq!(
            json!(pending),
            case["expected_pending"],
            "用例 {} 的待续文本",
            case["name"]
        );
        assert_eq!(
            json!(todos),
            case["expected_todos"],
            "用例 {} 的待办",
            case["name"]
        );
    }
}

#[test]
fn model_messages_match_python() {
    for case in section(&fixture(), "messages") {
        let event = SessionEvent::from_dict(&case["event"]).expect("事件可解析");
        let message = event_to_model_message(&event).unwrap_or(Value::Null);
        assert_eq!(message, case["expected"], "用例 {}", case["name"]);
    }
}

#[test]
fn tool_result_output_text_matches_python() {
    for case in section(&fixture(), "payload_text") {
        let text = tool_result_output_text(&payload_of(&case["payload"]));
        assert_eq!(json!(text), case["expected"], "用例 {}", case["name"]);
    }
}

#[test]
fn tool_messages_match_python() {
    for case in section(&fixture(), "tool_messages") {
        let kind = case["kind"].as_str().expect("用例类型");
        let actual = match kind {
            "format" => json!(format_tool_result_content(
                case["tool"].as_str().expect("tool"),
                case["ok"].as_bool().expect("ok"),
                case["output"].as_str().expect("output"),
            )),
            "result_message" => tool_result_message(
                case["tool"].as_str().expect("tool"),
                case["ok"].as_bool().expect("ok"),
                case["output"].as_str().expect("output"),
                case["tool_call_id"].as_str().expect("tool_call_id"),
            ),
            "interrupted" => interrupted_tool_result_message(
                case["tool"].as_str().expect("tool"),
                case["tool_call_id"].as_str().expect("tool_call_id"),
            ),
            "function_call" => function_tool_call(
                case["call_id"].as_str().expect("call_id"),
                case["function_name"].as_str().expect("function_name"),
                case["arguments"].as_str().expect("arguments"),
            ),
            other => panic!("未知用例类型：{other}"),
        };
        assert_eq!(actual, case["expected"], "用例 {}", case["name"]);
    }
}

#[test]
fn tool_pairing_matches_python() {
    for case in section(&fixture(), "pairing") {
        let messages = case["messages"].as_array().expect("messages").clone();
        assert_eq!(
            json!(complete_tool_pairing(&messages)),
            case["expected"],
            "用例 {}",
            case["name"]
        );
    }
}

#[test]
fn run_guard_event_matches_recovery_when_applied_one_by_one() {
    // 增量接口与整体恢复必须给出同一结果（Python 侧二者共用规则）。
    for case in section(&fixture(), "run_guard") {
        let mut pending = String::new();
        let mut todos: Vec<Value> = Vec::new();
        for event in events(&case) {
            let payload = payload_of(&event.to_dict()["payload"]);
            let (next_pending, next_todos) =
                apply_run_guard_event(pending, todos, &event.event_type, &payload);
            pending = next_pending;
            todos = next_todos;
        }
        assert_eq!(
            json!(pending),
            case["expected_pending"],
            "用例 {}",
            case["name"]
        );
        assert_eq!(
            json!(todos),
            case["expected_todos"],
            "用例 {}",
            case["name"]
        );
    }
}

/// 用 Python 侧的诊断字段做一次存在性检查，避免 fixture 空转。
#[test]
fn fixture_is_not_empty() {
    let fixture = fixture();
    for name in [
        "active_events",
        "titles",
        "run_guard",
        "messages",
        "payload_text",
        "tool_messages",
        "pairing",
    ] {
        assert!(!section(&fixture, name).is_empty(), "{name} 为空");
    }
}
