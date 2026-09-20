//! 有状态投影的跨语言 parity：期望值来自 Python `TurnHistoryProjector` 与三个 `project_*` 入口。

use omnicrawl_session::{
    project_compaction_boundary_history, project_history_messages, project_session_history,
    SessionEvent, TurnHistoryProjector,
};
use serde_json::{json, Map, Value};

const FIXTURE: &str = include_str!("fixtures/session_history_parity.json");

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
        .expect("缺少 events")
        .iter()
        .map(|item| SessionEvent::from_dict(item).expect("事件可解析"))
        .collect()
}

fn entries_json(entries: &[(String, Value)]) -> Value {
    json!(entries
        .iter()
        .map(|(anchor, message)| json!([anchor, message]))
        .collect::<Vec<Value>>())
}

fn payload_of(value: &Value) -> Map<String, Value> {
    value.as_object().cloned().unwrap_or_default()
}

#[test]
fn history_projection_matches_python() {
    for case in section(&fixture(), "history") {
        let projected = project_history_messages(&events(&case));
        assert_eq!(
            entries_json(&projected),
            case["expected"],
            "用例 {}",
            case["name"]
        );
    }
}

#[test]
fn session_history_matches_python() {
    for case in section(&fixture(), "session_history") {
        let projected = project_session_history(&events(&case));
        assert_eq!(
            entries_json(&projected),
            case["expected"],
            "用例 {}",
            case["name"]
        );
    }
}

#[test]
fn compaction_boundary_matches_python() {
    for case in section(&fixture(), "boundary") {
        let messages =
            project_compaction_boundary_history(&payload_of(&case["payload"]), &events(&case));
        assert_eq!(json!(messages), case["expected"], "用例 {}", case["name"]);
    }
}

#[test]
fn history_with_provider_matches_python() {
    for case in section(&fixture(), "history_with_provider") {
        let table = case["provider"].clone();
        let provider: Vec<(String, String)> = table
            .as_object()
            .expect("provider 必须是对象")
            .iter()
            .map(|(call_id, entry)| {
                (
                    call_id.clone(),
                    entry["kind"].as_str().unwrap_or_default().to_string(),
                )
            })
            .collect();
        let lookup = move |call_id: &str, _tool: &str| -> Option<String> {
            let kind = provider
                .iter()
                .find(|(id, _)| id == call_id)
                .map(|(_, kind)| kind.clone())?;
            match kind.as_str() {
                "text" => table[call_id]["text"].as_str().map(str::to_string),
                "empty" => Some(String::new()),
                _ => None,
            }
        };
        let mut projector = TurnHistoryProjector::with_raw_arguments_provider(Box::new(lookup));
        for event in events(&case) {
            projector.feed(&event);
        }
        assert_eq!(
            entries_json(&projector.take()),
            case["expected"],
            "用例 {}",
            case["name"]
        );
    }
}

#[test]
fn incremental_feed_matches_whole_stream() {
    // 逐条 feed + 分批取走与整体投影必须给出同一序列（前缀缓存因此不失效）。
    //
    // 压缩摘要那类用例不在此列：摘要边界会**替换**已累积条目，取走之后再遇到边界就丢了
    // 保留窗口——那是调用方语义（边界前不得 drain），不是投影器的不一致。
    for case in section(&fixture(), "history") {
        let all = events(&case);
        if all
            .iter()
            .any(|event| event.event_type == "compact_summary")
        {
            continue;
        }
        let mut projector = TurnHistoryProjector::new();
        let mut incremental = Vec::new();
        for (index, event) in all.iter().enumerate() {
            projector.feed(event);
            if index + 1 < all.len() {
                incremental.extend(projector.drain());
            }
        }
        incremental.extend(projector.take());
        assert_eq!(
            entries_json(&incremental),
            case["expected"],
            "用例 {} 的增量投影与整体投影不一致",
            case["name"]
        );
    }
}

#[test]
fn reset_discards_state() {
    let case = section(&fixture(), "history")
        .into_iter()
        .find(|case| case["name"] == "tool_batch_then_reply")
        .expect("存在工具批次用例");
    let all = events(&case);
    let mut projector = TurnHistoryProjector::new();
    for event in &all {
        projector.feed(event);
    }
    projector.reset();
    assert!(projector.take().is_empty(), "reset 之后不应残留条目");
    for event in &all {
        projector.feed(event);
    }
    assert_eq!(
        entries_json(&projector.take()),
        case["expected"],
        "重放同一事件流应得到同一结果"
    );
}
