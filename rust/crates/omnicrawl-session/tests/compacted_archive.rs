//! 压缩事件归档：写进去的原始事件必须按 archive_id 字典序读回，且 id 与归属都受校验。

use std::path::PathBuf;

use omnicrawl_session::{utc_now, SessionStore, SessionStoreError};
use serde_json::{json, Value};

fn temp_root(tag: &str) -> PathBuf {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("时钟可用")
        .as_nanos();
    let path = std::env::temp_dir().join(format!("omnicrawl-archive-{tag}-{unique}"));
    std::fs::create_dir_all(&path).expect("建临时目录");
    path
}

fn sample_events() -> Vec<Value> {
    vec![
        json!({
            "event_id": "e1",
            "type": "user_message",
            "payload": {"content": "一"},
            "created_at": "2026-01-01T00:00:00.000000+00:00",
        }),
        json!({
            "event_id": "e2",
            "type": "assistant_message",
            "payload": {"content": "二"},
            "created_at": "2026-01-01T00:00:01.000000+00:00",
        }),
    ]
}

fn archive(
    session_id: &str,
    events: &[Value],
    id: Option<&str>,
) -> Result<String, SessionStoreError> {
    let root = temp_root("case");
    let store = SessionStore::open(&root);
    store.ensure().expect("建目录");
    let created = store
        .start_session("/workspace", "标题", utc_now())
        .expect("建会话");
    let target = if session_id.is_empty() {
        created.session_id.clone()
    } else {
        session_id.to_string()
    };
    let result = store.archive_compacted_events(&target, events, id, utc_now());
    std::fs::remove_dir_all(&root).ok();
    result
}

#[test]
fn archived_events_round_trip() {
    let root = temp_root("round-trip");
    let store = SessionStore::open(&root);
    store.ensure().expect("建目录");
    let created = store
        .start_session("/workspace", "标题", utc_now())
        .expect("建会话");
    let events = sample_events();

    let archive_id = store
        .archive_compacted_events(&created.session_id, &events, Some("archive-a"), utc_now())
        .expect("归档成功");
    assert_eq!(archive_id, "archive-a");
    assert_eq!(
        store
            .read_compacted_events(&created.session_id)
            .expect("读回归档"),
        events
    );

    let second = store
        .archive_compacted_events(&created.session_id, &events, Some("archive-b"), utc_now())
        .expect("第二次归档");
    assert_eq!(second, "archive-b");
    assert_eq!(
        store
            .read_compacted_events(&created.session_id)
            .expect("读回归档")
            .len(),
        events.len() * 2,
        "两个归档按 id 顺序合并"
    );

    let empty_session = store
        .start_session("/workspace", "另一个", utc_now())
        .expect("建会话");
    assert!(
        store
            .read_compacted_events(&empty_session.session_id)
            .expect("读回归档")
            .is_empty(),
        "没有归档时读出空列表"
    );

    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn archive_ids_are_validated() {
    let events = sample_events();
    let generated = archive("", &events, None).expect("生成 id");
    assert!(generated.len() > 8, "生成的归档 id 不是空壳：{generated}");
    assert!(
        generated.starts_with("compact-"),
        "生成的归档 id 带 compact- 前缀：{generated}"
    );
    assert_eq!(
        archive("", &events, Some("  "))
            .expect_err("空白 id 非法")
            .message(),
        "archive_id 必须是非空字符串。"
    );
    assert_eq!(
        archive("", &events, Some("a/b"))
            .expect_err("含分隔符非法")
            .message(),
        "archive_id 不能包含路径分隔符。"
    );
    assert!(
        archive("", &events, Some(""))
            .expect("空串按未给出处理")
            .starts_with("compact-"),
        "空串按未给出处理，生成新 id"
    );
    assert!(
        archive("20260101-000000-abcdef", &events, Some("x"))
            .expect_err("会话不存在")
            .message()
            .contains("未找到会话"),
        "归档只允许落在已有会话名下"
    );
}
