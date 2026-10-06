//! 跨语言 parity：用 Python 真实现产出的期望值校验 Rust 流解析层。
//!
//! fixture 是冻结的对照契约，覆盖四组：参数完整性、
//! 工具调用分片归并、SSE 负载解码、SSE 负载流消费。

use std::collections::{BTreeMap, BTreeSet};

use omnicrawl_llm::{
    arguments_json_complete, decode_sse_data, emit_tool_call_deltas, first_choice,
    iter_raw_sse_events, SseError, ToolCallBuffer,
};
use omnicrawl_protocol::ModelStreamEvent;
use serde_json::{json, Map, Value};

const FIXTURE: &str = include_str!("fixtures/openai_chat_stream_parity.json");

fn fixture() -> Value {
    serde_json::from_str(FIXTURE).expect("fixture 不是合法 JSON")
}

fn event_to_json(event: &ModelStreamEvent) -> Value {
    match event {
        ModelStreamEvent::ToolCallStarted(started) => json!({
            "kind": "tool_call_started",
            "call_id": started.call_id,
            "name": started.name,
        }),
        ModelStreamEvent::ToolCallArgumentsDelta(delta) => json!({
            "kind": "tool_call_arguments_delta",
            "call_id": delta.call_id,
            "delta": delta.delta,
        }),
        other => panic!("未预期的分片事件：{other:?}"),
    }
}

#[test]
fn arguments_json_completeness_matches_python() {
    for case in fixture()["args_complete"]
        .as_array()
        .expect("缺少 args_complete")
    {
        let actual = json!(arguments_json_complete(&case["value"]));
        assert_eq!(
            &actual, &case["expected"],
            "参数完整性用例 {:?} 不一致",
            case["value"]
        );
    }
}

#[test]
fn tool_call_deltas_match_python() {
    for case in fixture()["tool_call_deltas"]
        .as_array()
        .expect("缺少 tool_call_deltas")
    {
        let deltas = case["deltas"].as_array().cloned().unwrap_or_default();
        let mut buffers: BTreeMap<u64, ToolCallBuffer> = BTreeMap::new();
        let mut started: BTreeSet<u64> = BTreeSet::new();
        let events = emit_tool_call_deltas(&deltas, &mut buffers, &mut started);
        let events: Vec<Value> = events.iter().map(event_to_json).collect();
        let buffers: Map<String, Value> = buffers
            .iter()
            .map(|(index, buffer)| {
                (
                    index.to_string(),
                    json!({"id": buffer.id, "name": buffer.name, "arguments": buffer.arguments}),
                )
            })
            .collect();
        let actual = json!({
            "name": case["name"],
            "deltas": deltas,
            "events": events,
            "buffers": Value::Object(buffers),
            "started": started.iter().copied().collect::<Vec<_>>(),
        });
        assert_eq!(&actual, case, "分片归并用例 {} 不一致", case["name"]);
    }
}

#[test]
fn sse_decode_matches_python() {
    for case in fixture()["sse_decode"].as_array().expect("缺少 sse_decode") {
        let actual =
            decode_sse_data(case["payload"].as_str().unwrap_or_default()).unwrap_or(Value::Null);
        assert_eq!(
            &actual, &case["expected"],
            "SSE 解码用例 {:?} 不一致",
            case["payload"]
        );
    }
}

#[test]
fn sse_stream_matches_python() {
    for case in fixture()["sse_stream"].as_array().expect("缺少 sse_stream") {
        let payloads: Vec<String> = case["payloads"]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        let outcome = iter_raw_sse_events(payloads.iter().map(String::as_str));
        let (events, error) = match outcome {
            Ok(events) => (events, Value::Null),
            Err(SseError::Provider { message, body }) => (
                Vec::new(),
                json!({"type": "APIError", "message": message, "body": body}),
            ),
        };
        let actual = json!({
            "name": case["name"],
            "payloads": payloads,
            "events": events,
            "error": error,
        });
        let mut expected = case.as_object().cloned().unwrap_or_default();
        let closed = expected
            .remove("closed")
            .and_then(|value| value.as_bool())
            .unwrap_or(false);
        // 关闭连接是宿主的传输层职责（Python 在 finally 里做），内核不参与，
        // 因此这里只校验 Python 侧确实关过，再比对内核产出的部分。
        assert!(closed, "用例 {} 期望传输层关闭连接", case["name"]);
        assert_eq!(
            actual,
            Value::Object(expected),
            "SSE 流用例 {} 不一致",
            case["name"]
        );
    }
}

#[test]
fn first_choice_matches_python() {
    for case in fixture()["first_choice"]
        .as_array()
        .expect("缺少 first_choice")
    {
        let actual = first_choice(&case["chunk"]).cloned().unwrap_or(Value::Null);
        assert_eq!(
            &actual, &case["expected"],
            "首选项用例 {:?} 不一致",
            case["chunk"]
        );
    }
}
