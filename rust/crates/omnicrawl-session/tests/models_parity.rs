//! 会话层模型的跨语言 parity：期望值来自 Python `omnicrawl/state/session_models.py`。
//!
//! 覆盖会话 id / 事件类型 / 相对路径校验、标题折叠、时间戳解析与格式化、
//! 会话事件与索引条目的字段校验、以及转录里一行的字节布局。

use omnicrawl_session::{
    clean_title, datetime_from_json, datetime_to_millis, event_type_from_json, format_datetime,
    new_session_id, normalize_event_type, normalize_relative_file_path, normalize_session_id,
    parse_datetime, path_from_json, read_payload_non_negative_int, session_id_from_json,
    SessionEvent, SessionIndexEntry, SessionStoreError, COMPACT_SUMMARY_PREFIX,
    EMPTY_SESSION_EVENT_TYPES, MESSAGE_EVENT_TYPES, MODEL_CONTEXT_EVENT_TYPES,
    SESSION_EVENT_VERSION, SUBAGENT_EVENT_TYPES,
};
use serde_json::{json, Map, Value};

const FIXTURE: &str = include_str!("fixtures/session_models_parity.json");

fn fixture() -> Value {
    serde_json::from_str(FIXTURE).expect("fixture 不是合法 JSON")
}

fn cases(fixture: &Value, section: &str) -> Vec<Value> {
    fixture[section]
        .as_array()
        .unwrap_or_else(|| panic!("fixture 缺少 {section}"))
        .clone()
}

/// 字符串走类型化入口，其余走 JSON 边界入口（Python 侧是同一个函数带类型检查）。
fn session_id(input: &Value) -> Result<String, SessionStoreError> {
    match input.as_str() {
        Some(text) => normalize_session_id(text),
        None => session_id_from_json(input),
    }
}

fn event_type(input: &Value) -> Result<String, SessionStoreError> {
    match input.as_str() {
        Some(text) => normalize_event_type(text),
        None => event_type_from_json(input),
    }
}

fn relative_path(input: &Value) -> Result<String, SessionStoreError> {
    match input.as_str() {
        Some(text) => normalize_relative_file_path(text),
        None => path_from_json(input),
    }
}

fn datetime(input: &Value) -> Result<chrono::DateTime<chrono::Utc>, SessionStoreError> {
    match input.as_str() {
        Some(text) => parse_datetime(text),
        None => datetime_from_json(input),
    }
}

fn compare_text(actual: Result<String, SessionStoreError>, case: &Value, label: &str) {
    match (actual, case.get("expected"), case.get("error")) {
        (Ok(value), Some(expected), None) => {
            assert_eq!(json!(value), *expected, "{label} 用例 {case}");
        }
        (Err(error), None, Some(expected)) => {
            assert_eq!(
                json!(error.message()),
                *expected,
                "{label} 用例 {case} 的错误文案"
            );
        }
        (actual, _, _) => panic!("{label} 与 fixture 期望不符：{actual:?} / {case}"),
    }
}

#[test]
fn naming_and_validation_matches_python() {
    let fixture = fixture();

    for case in cases(&fixture, "session_ids") {
        compare_text(session_id(&case["input"]), &case, "会话 id");
    }
    for case in cases(&fixture, "event_types") {
        compare_text(event_type(&case["input"]), &case, "事件类型");
    }
    for case in cases(&fixture, "relative_paths") {
        compare_text(relative_path(&case["input"]), &case, "转录路径");
    }
    for case in cases(&fixture, "titles") {
        compare_text(
            Ok(clean_title(
                case["input"].as_str().expect("标题输入是字符串"),
            )),
            &case,
            "标题折叠",
        );
    }
}

#[test]
fn timestamps_match_python() {
    for case in cases(&fixture(), "datetimes") {
        match (
            datetime(&case["input"]),
            case.get("expected_iso"),
            case.get("error"),
        ) {
            (Ok(value), Some(iso), None) => {
                assert_eq!(json!(format_datetime(value)), *iso, "时间戳用例 {case}");
                assert_eq!(
                    json!(datetime_to_millis(value)),
                    case["expected_millis"],
                    "时间戳毫秒用例 {case}"
                );
            }
            (Err(error), None, Some(expected)) => {
                assert_eq!(
                    json!(error.message()),
                    *expected,
                    "时间戳用例 {case} 的错误文案"
                );
            }
            (actual, _, _) => panic!("时间戳与 fixture 期望不符：{actual:?} / {case}"),
        }
    }
}

#[test]
fn session_events_match_python() {
    for case in cases(&fixture(), "events") {
        let parsed = SessionEvent::from_dict(&case["input"]);
        match (parsed, case.get("expected"), case.get("error")) {
            (Ok(event), Some(expected), None) => {
                assert_eq!(event.to_dict(), *expected, "事件用例 {case}");
            }
            (Err(error), None, Some(expected)) => {
                assert_eq!(
                    json!(error.message()),
                    *expected,
                    "事件用例 {case} 的错误文案"
                );
            }
            (actual, _, _) => panic!("事件与 fixture 期望不符：{actual:?} / {case}"),
        }
    }
}

#[test]
fn created_events_match_python() {
    for case in cases(&fixture(), "created_events") {
        let input = &case["input"];
        let payload: Map<String, Value> = input
            .get("payload")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let created = match input.get("now") {
            Some(now) => SessionEvent::create(
                input["session_id"].as_str().expect("会话 id 是字符串"),
                input["event_type"].as_str().expect("事件类型是字符串"),
                payload,
                input.get("parent_id").and_then(Value::as_str),
                parse_datetime(now.as_str().expect("now 是字符串")).expect("now 可解析"),
            ),
            None => unreachable!("fixture 的 create 用例都带 now"),
        };
        match (created, case.get("expected"), case.get("error")) {
            (Ok(event), Some(expected), None) => {
                assert!(
                    is_event_id(&event.event_id),
                    "生成的事件 id 必须形如 24 位小写十六进制：{}",
                    event.event_id
                );
                let mut actual = event.to_dict();
                actual["event_id"] = Value::Null;
                assert_eq!(actual, *expected, "创建事件用例 {case}");
            }
            (Err(error), None, Some(expected)) => {
                assert_eq!(
                    json!(error.message()),
                    *expected,
                    "创建事件用例 {case} 的错误文案"
                );
            }
            (actual, _, _) => panic!("创建事件与 fixture 期望不符：{actual:?} / {case}"),
        }
    }
}

#[test]
fn index_entries_match_python() {
    for case in cases(&fixture(), "index_entries") {
        let parsed = SessionIndexEntry::from_dict(&case["input"]);
        match (parsed, case.get("expected"), case.get("error")) {
            (Ok(entry), Some(expected), None) => {
                assert_eq!(entry.to_dict(), *expected, "索引用例 {case}");
            }
            (Err(error), None, Some(expected)) => {
                assert_eq!(
                    json!(error.message()),
                    *expected,
                    "索引用例 {case} 的错误文案"
                );
            }
            (actual, _, _) => panic!("索引与 fixture 期望不符：{actual:?} / {case}"),
        }
    }
}

#[test]
fn payload_counts_match_python() {
    for case in cases(&fixture(), "payload_counts") {
        let actual = read_payload_non_negative_int(&case["input"]);
        assert_eq!(json!(actual), case["expected"], "载荷计数用例 {case}");
    }
}

#[test]
fn transcript_lines_match_python_bytes() {
    for case in cases(&fixture(), "lines") {
        let event = SessionEvent::from_dict(&case["event"]).expect("fixture 事件应当合法");
        assert_eq!(
            json!(event.to_json_line()),
            case["line"],
            "转录行字节不一致：{case}"
        );
    }
}

#[test]
fn constants_match_python() {
    let fixture = fixture();
    let constants = &fixture["constants"];
    assert_eq!(
        json!(SESSION_EVENT_VERSION),
        constants["session_event_version"]
    );
    assert_eq!(
        json!(COMPACT_SUMMARY_PREFIX),
        constants["compact_summary_prefix"]
    );

    let sorted = |values: &[&str]| -> Value {
        let mut values: Vec<&str> = values.to_vec();
        values.sort_unstable();
        json!(values)
    };
    assert_eq!(
        sorted(MESSAGE_EVENT_TYPES),
        constants["message_event_types"]
    );
    assert_eq!(
        sorted(MODEL_CONTEXT_EVENT_TYPES),
        constants["model_context_event_types"]
    );
    assert_eq!(
        sorted(EMPTY_SESSION_EVENT_TYPES),
        constants["empty_session_event_types"]
    );
    assert_eq!(
        sorted(SUBAGENT_EVENT_TYPES),
        constants["subagent_event_types"]
    );
}

#[test]
fn generated_session_ids_are_accepted_by_the_python_pattern() {
    for _ in 0..32 {
        let session_id = new_session_id(chrono::Utc::now());
        let normalized = normalize_session_id(&session_id).unwrap_or_else(|error| {
            panic!("生成的会话 id 不合规：{session_id}，{}", error.message())
        });
        assert_eq!(normalized, session_id);
    }
}

fn is_event_id(value: &str) -> bool {
    value.len() == 24
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}
