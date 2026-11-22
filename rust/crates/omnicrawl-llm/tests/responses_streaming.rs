mod common;

use omnicrawl_llm::{
    ChatEndpoint, ResponsesRuntime, ResponsesStreamState, RuntimeErrorKind, SinkFlow, TurnSink,
};
use omnicrawl_protocol::{aggregate_stream_events, ModelStreamEvent};
use serde_json::json;

#[test]
fn arguments_stream_before_completion_with_the_provider_call_id() {
    let mut state = ResponsesStreamState::new();
    let mut events = Vec::new();
    state.handle_event(&json!({
        "type": "response.output_item.added",
        "item": {"type": "function_call", "id": "fc_1", "call_id": "call_1", "name": "write_file", "arguments": ""}
    }), &mut events);
    assert!(
        matches!(&events[..], [ModelStreamEvent::ToolCallStarted(call)] if call.call_id == "call_1")
    );
    for delta in ["{\"path\":\"a.txt\",\"content\":\"", "你好", "\"}"] {
        events.clear();
        state.handle_event(
            &json!({
                "type": "response.function_call_arguments.delta", "item_id": "fc_1", "delta": delta
            }),
            &mut events,
        );
        assert!(
            matches!(&events[..], [ModelStreamEvent::ToolCallArgumentsDelta(part)]
            if part.call_id == "call_1" && part.delta == delta)
        );
    }
    events.clear();
    state.handle_event(
        &json!({
            "type": "response.function_call_arguments.done", "item_id": "fc_1",
            "name": "write_file", "arguments": "{\"path\":\"a.txt\",\"content\":\"你好\"}"
        }),
        &mut events,
    );
    assert!(
        matches!(&events[..], [ModelStreamEvent::ToolCallCompleted(call)]
        if call.call_id == "call_1" && call.arguments["content"] == "你好")
    );
    state.finish(&mut events).unwrap();
}

#[test]
fn interleaved_calls_complete_once_without_orphaned_item_ids() {
    let mut state = ResponsesStreamState::new();
    let mut events = Vec::new();
    for (id, call_id, name) in [
        ("fc_a", "call_a", "write_file"),
        ("fc_b", "call_b", "Edit_file"),
    ] {
        state.handle_event(
            &json!({
                "type": "response.output_item.added",
                "item": {"type": "function_call", "id": id, "call_id": call_id, "name": name}
            }),
            &mut events,
        );
    }
    for (id, delta) in [
        ("fc_a", "{\"content\":\""),
        ("fc_b", "{\"new_text\":\""),
        ("fc_a", "甲\"}"),
        ("fc_b", "乙\"}"),
    ] {
        state.handle_event(
            &json!({
                "type": "response.function_call_arguments.delta", "item_id": id, "delta": delta
            }),
            &mut events,
        );
    }
    for (id, call_id, name, args) in [
        ("fc_a", "call_a", "write_file", "{\"content\":\"甲\"}"),
        ("fc_b", "call_b", "Edit_file", "{\"new_text\":\"乙\"}"),
    ] {
        state.handle_event(&json!({
            "type": "response.output_item.done",
            "item": {"type": "function_call", "id": id, "call_id": call_id, "name": name, "arguments": args}
        }), &mut events);
        state.handle_event(
            &json!({
                "type": "response.function_call_arguments.done", "item_id": id,
                "name": name, "arguments": args
            }),
            &mut events,
        );
    }
    state.finish(&mut events).unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, ModelStreamEvent::ToolCallArgumentsDelta(_)))
            .count(),
        4
    );
    let reply = aggregate_stream_events(events);
    assert_eq!(reply.tool_calls.len(), 2);
    assert_eq!(reply.tool_calls[0].call_id, "call_a");
    assert_eq!(reply.tool_calls[1].call_id, "call_b");
    assert_eq!(reply.tool_calls[0].arguments["content"], "甲");
    assert_eq!(reply.tool_calls[1].arguments["new_text"], "乙");
}

#[test]
fn initial_arguments_and_late_names_are_visible_without_repeating_fragments() {
    for initial in ["", "{\"content\":\""] {
        let mut state = ResponsesStreamState::new();
        let mut events = Vec::new();
        state.handle_event(&json!({
            "type": "response.output_item.added",
            "item": {"type": "function_call", "id": "fc_1", "call_id": "call_1", "name": "write_file", "arguments": initial}
        }), &mut events);
        for delta in if initial.is_empty() {
            vec!["{\"content\":\"", "你好\"}"]
        } else {
            vec!["你好\"}"]
        } {
            state.handle_event(&json!({
                "type": "response.function_call_arguments.delta", "item_id": "fc_1", "delta": delta
            }), &mut events);
        }
        let text: String = events
            .iter()
            .filter_map(|event| match event {
                ModelStreamEvent::ToolCallArgumentsDelta(part) => Some(part.delta.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "{\"content\":\"你好\"}");
        state.finish(&mut events).unwrap();
        assert_eq!(aggregate_stream_events(events).tool_calls.len(), 1);
    }

    let mut state = ResponsesStreamState::new();
    let mut events = Vec::new();
    state.handle_event(&json!({
        "type": "response.function_call_arguments.delta", "item_id": "call_1", "delta": "{\"content\":\""
    }), &mut events);
    assert!(events.is_empty());
    state.handle_event(&json!({
        "type": "response.function_call_arguments.delta", "item_id": "call_1", "name": "write_file", "delta": "你好\"}"
    }), &mut events);
    assert!(
        matches!(&events[..], [ModelStreamEvent::ToolCallStarted(_), ModelStreamEvent::ToolCallArgumentsDelta(part)]
        if part.delta == "{\"content\":\"你好\"}")
    );
}

#[test]
fn streamed_partial_arguments_do_not_make_a_truncated_call_executable() {
    let mut state = ResponsesStreamState::new();
    let mut events = Vec::new();
    state.handle_event(&json!({
        "type": "response.output_item.added",
        "item": {"type": "function_call", "id": "fc_1", "call_id": "call_1", "name": "write_file"}
    }), &mut events);
    state.handle_event(&json!({
        "type": "response.function_call_arguments.delta", "item_id": "fc_1", "delta": "{\"content\":\"半截"
    }), &mut events);
    assert!(events
        .iter()
        .any(|event| matches!(event, ModelStreamEvent::ToolCallArgumentsDelta(_))));
    assert!(state.finish(&mut events).is_err());
    assert!(!events
        .iter()
        .any(|event| matches!(event, ModelStreamEvent::ToolCallCompleted(_))));
}

#[test]
fn runtime_delivers_argument_fragments_before_completing_the_call() {
    let body = concat!(
        "data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"function_call\",\"id\":\"fc_1\",\"call_id\":\"call_1\",\"name\":\"write_file\"}}\n\n",
        "data: {\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"fc_1\",\"delta\":\"{\\\"content\\\":\\\"hello\\\"}\"}\n\n",
        "data: {\"type\":\"response.function_call_arguments.done\",\"item_id\":\"fc_1\",\"name\":\"write_file\",\"arguments\":\"{\\\"content\\\":\\\"hello\\\"}\"}\n\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"output\":[]}}\n\n",
    );
    let server = common::StubServer::spawn(vec![
        common::Reply::sse(200, body),
        common::Reply::sse(200, body),
    ]);
    let runtime = ResponsesRuntime::new(ChatEndpoint {
        base_url: server.base_url(),
        api_key: "test".into(),
        user_agent: String::new(),
    });
    let case = common::CaseInput::simple("test-model", "write");
    let mut sink = common::RecordingSink::default();
    let reply = runtime.run_turn(&case.chat_input(), &mut sink).unwrap();
    assert_eq!(
        common::event_tags(&sink.events),
        vec![
            "tool_started",
            "tool_arguments",
            "tool_completed",
            "finished"
        ]
    );
    assert_eq!(reply.tool_calls.len(), 1);
    assert_eq!(reply.tool_calls[0].call_id, "call_1");
    assert_eq!(reply.tool_calls[0].arguments["content"], "hello");

    struct CancelOnArguments;
    impl TurnSink for CancelOnArguments {
        fn on_event(&mut self, event: ModelStreamEvent) -> SinkFlow {
            match event {
                ModelStreamEvent::ToolCallArgumentsDelta(_) => SinkFlow::Cancel,
                ModelStreamEvent::ToolCallCompleted(_) => panic!("参数增量应先到达接收端"),
                _ => SinkFlow::Continue,
            }
        }
    }
    let error = runtime
        .run_turn(&case.chat_input(), &mut CancelOnArguments)
        .unwrap_err();
    assert_eq!(error.kind, RuntimeErrorKind::Cancelled);
}
