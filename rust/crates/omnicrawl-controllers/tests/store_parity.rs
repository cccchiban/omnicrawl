//! `controllers/session/store.py` 事件投影编排的跨语言对照。
//!
//! 期望值来自 Python 真实现：`python rust/tools/gen_controllers_fixture.py` 重新生成
//! `tests/fixtures/controllers_parity.json` 的 `store` 段后，本套件用同一批入参重放
//! Rust 实现，比对内存事件的逐字段形状，以及「按落盘与否选投影方式」这条规则。

use omnicrawl_controllers::store::{
    ephemeral_event, ephemeral_event_id, next_sequence, projection_feed, ProjectionFeed,
};
use serde_json::Value;

const FIXTURE: &str = include_str!("fixtures/controllers_parity.json");

fn section() -> Value {
    let fixture: Value = serde_json::from_str(FIXTURE).expect("fixture 不是合法 JSON");
    fixture["store"].clone()
}

#[test]
fn ephemeral_events_match_python() {
    let data = section();
    for case in data["ephemeral"].as_array().expect("ephemeral") {
        let label = case["label"].as_str().expect("label");
        let sequence = case["sequence"].as_u64().expect("sequence") as u32;
        let session_id = case["session_id"].as_str();
        let event_type = case["event_type"].as_str().expect("event_type");
        let payload = case["payload"].as_object().expect("payload").clone();
        let created_at = case["created_at"].as_str().expect("created_at");

        let event = ephemeral_event(sequence, session_id, event_type, &payload, created_at)
            .expect("内存事件构造应当成功");
        assert_eq!(event.to_dict(), case["event"], "内存事件（{label}）");
        assert_eq!(
            next_sequence(sequence) as u64,
            case["next_sequence"].as_u64().expect("next_sequence"),
            "推进后的序号（{label}）"
        );
    }
}

#[test]
fn projection_feed_matches_python() {
    let data = section();
    for case in data["feed"].as_array().expect("feed") {
        let label = case["label"].as_str().expect("label");
        let has_projector = case["has_projector"].as_bool().expect("has_projector");
        let persisted = case["persisted"].as_bool().expect("persisted");
        let py_feed = case["feed"].as_array().expect("feed");
        let original_payload = &case["facade_calls"][0]["payload"];

        match projection_feed(has_projector, persisted) {
            ProjectionFeed::Skip => {
                assert!(py_feed.is_empty(), "无投影器不应喂任何事件（{label}）")
            }
            ProjectionFeed::Persisted => {
                assert_eq!(py_feed.len(), 1, "落盘事件只喂一次（{label}）");
                assert_eq!(
                    py_feed[0]["payload"], *original_payload,
                    "落盘事件必须用未脱敏的原始 payload 投影（{label}）"
                );
            }
            ProjectionFeed::Ephemeral => {
                assert_eq!(py_feed.len(), 1, "未落盘事件只喂一次（{label}）");
                assert_eq!(
                    py_feed[0]["event_id"].as_str(),
                    Some(ephemeral_event_id(1, "tool_result").as_str()),
                    "未落盘事件用内存事件 ID（{label}）"
                );
            }
        }
    }
}
