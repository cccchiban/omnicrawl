//! 会话一致性诊断的对照测试。
//!
//! 期望值由 `rust/tools/gen_session_consistency_fixture.py` 驱动 Python 真实现生成
//! （`omnicrawl/state/session_consistency.py`）；这里用同一份输入重放 Rust 实现，
//! 按序列化后的字符串逐字段比对——workspace 开了 `preserve_order`，键序也是契约的一部分。

use omnicrawl_session::{
    build_index_entry_from_events, compare_index_entry, SessionEvent, SessionIndexEntry,
};
use serde_json::Value;

fn fixture() -> Value {
    serde_json::from_str(include_str!("fixtures/session_consistency_parity.json"))
        .expect("对照数据集必须是合法 JSON")
}

fn events_of(case: &Value) -> Vec<SessionEvent> {
    case["events"]
        .as_array()
        .expect("用例必须带事件数组")
        .iter()
        .map(|item| SessionEvent::from_dict(item).expect("用例事件必须合法"))
        .collect()
}

#[test]
fn 索引重建与诊断逐字段对照() {
    let data = fixture();
    let cases = data["cases"].as_array().expect("数据集必须有 cases");
    assert!(!cases.is_empty(), "数据集不能为空");

    for case in cases {
        let name = case["name"].as_str().unwrap_or("(未命名)");
        let session_id = case["session_id"].as_str().expect("用例必须有会话 id");
        let relative_path = case["relative_path"].as_str().expect("用例必须有相对路径");
        let events = events_of(case);

        let rebuilt = build_index_entry_from_events(session_id, relative_path, &events)
            .unwrap_or_else(|error| panic!("用例「{name}」重建失败：{error}"));
        assert_eq!(
            serde_json::to_string(&rebuilt.to_dict()).expect("重建条目可序列化"),
            serde_json::to_string(&case["expected"]["rebuilt"]).expect("期望值可序列化"),
            "用例「{name}」重建条目不一致"
        );

        let current = SessionIndexEntry::from_dict(&case["current_index"])
            .unwrap_or_else(|error| panic!("用例「{name}」现有索引条目非法：{error}"));
        let issues: Vec<Value> = compare_index_entry(&current, &rebuilt)
            .iter()
            .map(|issue| issue.to_dict())
            .collect();
        assert_eq!(
            serde_json::to_string(&Value::Array(issues)).expect("诊断列表可序列化"),
            serde_json::to_string(&case["expected"]["issues"]).expect("期望诊断可序列化"),
            "用例「{name}」诊断列表不一致"
        );
    }
}

#[test]
fn 空转录报错文案与_python_一致() {
    let data = fixture();
    let empty = &data["empty_events"];
    let error = build_index_entry_from_events(
        empty["session_id"].as_str().expect("会话 id"),
        empty["relative_path"].as_str().expect("相对路径"),
        &[],
    )
    .expect_err("空转录必须报错");
    assert_eq!(
        error.message(),
        empty["message"].as_str().expect("期望文案")
    );
}
