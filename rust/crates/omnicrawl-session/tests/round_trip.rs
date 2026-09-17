//! 会话写入方的自洽性：内核生成的镜像必须能被自己读回，且再写一遍字节不变。
//!
//! Python 侧的对照见 `models_parity.rs`；这里只钉不依赖 Python 的自身约束。

use std::collections::HashSet;

use omnicrawl_session::{
    new_session_id, utc_now, SessionEvent, SessionIndexEntry, SessionStoreError,
};
use serde_json::{json, Map, Value};

fn payload(value: Value) -> Map<String, Value> {
    value.as_object().cloned().expect("载荷是 JSON 对象")
}

#[test]
fn event_line_round_trips() {
    let event = SessionEvent::create(
        "20260918-030529-abcdef",
        "assistant_message",
        payload(json!({"blocks": [{"Text": {"text": "中文"}}], "count": 2})),
        None,
        utc_now(),
    )
    .expect("创建事件");

    let line = event.to_json_line();
    assert!(!line.contains('\n'), "一行里不能有换行：{line}");
    assert!(!line.contains(", "), "分隔符必须紧凑：{line}");

    let decoded: Value = serde_json::from_str(&line).expect("行是合法 JSON");
    let parsed = SessionEvent::from_dict(&decoded).expect("行可解析");
    assert_eq!(parsed, event);
    assert_eq!(parsed.to_json_line(), line, "再写一遍必须字节一致");
}

#[test]
fn event_dict_round_trips() {
    let event = SessionEvent::create(
        "20260918-030529-abcdef",
        "tool_result",
        payload(json!({"ok": true})),
        Some("  0123456789abcdef01234567  "),
        utc_now(),
    )
    .expect("创建事件");

    assert_eq!(event.parent_id.as_deref(), Some("0123456789abcdef01234567"));
    let parsed = SessionEvent::from_dict(&event.to_dict()).expect("事件对象可解析");
    assert_eq!(parsed, event);
}

#[test]
fn index_entry_round_trips() {
    let entry = SessionIndexEntry::from_dict(&json!({
        "session_id": "20260918-030529-abcdef",
        "title": "会话标题",
        "workspace_root": "D:/work/demo",
        "path": "sessions/20260918-030529-abcdef.jsonl",
        "created_at": "2026-09-18T03:05:29.123456+00:00",
        "updated_at": "2026-09-18T04:00:00+00:00",
        "event_count": 7,
        "message_count": 3,
        "last_event_type": "assistant_message",
        "archived_at": "2026-09-18T05:00:00+00:00",
    }))
    .expect("索引条目");

    let parsed = SessionIndexEntry::from_dict(&entry.to_dict()).expect("索引可解析");
    assert_eq!(parsed, entry);
}

#[test]
fn generated_session_ids_are_unique() {
    let now = utc_now();
    let ids: HashSet<String> = (0..512).map(|_| new_session_id(now)).collect();
    assert_eq!(ids.len(), 512, "同一时刻生成的会话 id 也不能撞车");
}

#[test]
fn malformed_lines_are_rejected_with_concise_errors() {
    let error = SessionEvent::from_dict(&json!({"version": 1})).expect_err("缺字段必须失败");
    assert_eq!(error.message(), "会话事件缺少字段：session_id。");

    let error: SessionStoreError = SessionEvent::from_dict(&json!([])).expect_err("非对象必须失败");
    assert_eq!(error.message(), "会话事件缺少字段：version。");
}
