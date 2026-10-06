//! `controllers/turn/loop.py` 工具调用事件字段的跨语言对照。
//!
//! 数据集是冻结的对照契约：`python rust/tools/gen_controllers_fixture.py` 重新生成
//! `tests/fixtures/controllers_parity.json` 的 `tool_events` 段（直接调 `TurnLoopMixin` 的
//! 静态方法）。本套件用同一批输入重放 Rust 实现，比对字段内容、键序与「协议原文不落盘」边界。

use omnicrawl_controllers::turn::tool_events::{
    raw_tool_call_event_fields, strip_protocol_only_fields, PROTOCOL_ONLY_EVENT_FIELDS,
};
use serde_json::Value;

const FIXTURE: &str = include_str!("fixtures/controllers_parity.json");

fn section() -> Value {
    let fixture: Value = serde_json::from_str(FIXTURE).expect("fixture 不是合法 JSON");
    fixture["tool_events"].clone()
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
fn raw_tool_call_event_fields_match_python() {
    let data = section();
    for case in data["cases"].as_array().expect("cases") {
        let label = case["label"].as_str().expect("label");
        let fields = raw_tool_call_event_fields(
            &case["assistant_message"],
            case["call_id"].as_str().expect("call_id"),
            case["tool_name"].as_str().expect("tool_name"),
        );
        assert_eq!(
            Value::Object(fields.clone()),
            case["fields"],
            "协议字段（{label}）"
        );
        // 键序也要一致：恢复投影按这个顺序写入事件载荷。
        let keys: Vec<String> = fields.keys().cloned().collect();
        assert_eq!(keys, strings(&case["field_keys"]), "字段顺序（{label}）");

        // 协议原文可能含明文密钥，落盘前必须能被剔除。
        let mut stripped = fields.clone();
        strip_protocol_only_fields(&mut stripped);
        for key in PROTOCOL_ONLY_EVENT_FIELDS {
            assert!(
                !stripped.contains_key(key),
                "落盘字段不应含协议原文（{label}）"
            );
        }
    }
}
