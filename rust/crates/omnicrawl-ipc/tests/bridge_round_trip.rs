//! 桥接层往返：每个方法的负载样本解帧后回组帧必须一致，样本必须覆盖全部方法。

use omnicrawl_ipc::bridge::{
    initialize_result, method, unsupported_version_error, Command, HostEvent, ModelHookRequest,
    ModelHookResult, ModelRequest, ToolBatch, ToolBatchResult,
};
use omnicrawl_ipc::frame::{error_code, Frame, Id};
use omnicrawl_ipc::version::{negotiate_version, VersionError, PROTOCOL_VERSION};
use serde_json::{json, Value};

const TOOL_CALL: &str =
    r#"{"name":"read_file","arguments":{"path":"a.py"},"id":"c1","function_name":"read_file"}"#;
const TOOL_RESULT: &str =
    r#"{"ok":true,"output":"done","full_output":"","error_code":null,"retryable":false}"#;

/// 内核 → 宿主的notifications样本；覆盖 `HostEvent::METHODS` 的全部方法。
fn host_event_samples() -> Vec<(&'static str, Value)> {
    vec![
        (method::TURN_DELTA, json!({"text": "你好"})),
        (
            method::TURN_REASONING_DELTA,
            json!({"text": "先看文件\n再改"}),
        ),
        (method::TURN_STATUS, json!({"message": "正在压缩上下文"})),
        (
            method::TURN_NOTICE,
            json!({"message": "检测到 1 处疑似畸形脱敏占位符，已按原样保留。"}),
        ),
        (
            method::TURN_RETRY_STATUS,
            json!({"message": "网关超时，正在重试"}),
        ),
        (method::TURN_PROTOCOL_WAIT, json!({})),
        (method::TURN_STREAM_ROLLBACK, json!({})),
        (
            method::TURN_TOKEN_USAGE,
            json!({"input_tokens": 120, "output_tokens": 8, "cached_input_tokens": 64}),
        ),
        (
            method::TOOL_STARTED,
            json!({"step": 1, "call": serde_json::from_str::<Value>(TOOL_CALL).unwrap()}),
        ),
        (
            method::TOOL_FINISHED,
            json!({
                "call": serde_json::from_str::<Value>(TOOL_CALL).unwrap(),
                "result": serde_json::from_str::<Value>(TOOL_RESULT).unwrap(),
            }),
        ),
        (
            method::TOOL_OUTPUT_UPDATE,
            json!({
                "call": serde_json::from_str::<Value>(TOOL_CALL).unwrap(),
                "result": serde_json::from_str::<Value>(TOOL_RESULT).unwrap(),
            }),
        ),
        (
            method::SUBAGENT_EVENT,
            json!({"name": "explore", "payload": {"status": "ok", "files": 3}}),
        ),
        (
            method::TODO_UPDATE,
            json!({"todos": [{"id": "a", "step": "读代码", "completed": true}]}),
        ),
        (
            method::TURN_FINISHED,
            json!({
                "turn_id": "t1",
                "final_text": "完成",
                "reasoning": "先定位再改",
                "model_turns": 2,
                "tool_calls": 1,
                "paused": false,
                "post_compaction_context_tokens": 52000,
            }),
        ),
        (
            method::TURN_CONTEXT_COMPACTION,
            json!({
                "post_turn_context_tokens": 130000,
                "trigger_context_tokens": 120000,
                "turn_id": "t1",
                "post_compaction_context_tokens": 12000,
            }),
        ),
        (
            method::TURN_MODEL_RESPONSE_AFTER,
            json!({"model": "gpt-4o", "content": "完成", "tool_call_count": 1}),
        ),
        (
            method::TURN_MODEL_REQUEST_ERROR,
            json!({"error": "Agent 模型流中断：连接重置", "model": "gpt-4o"}),
        ),
        (
            method::TURN_TOOL_CALL_STARTED,
            json!({"call_id": "call-1", "tool": "bash"}),
        ),
        (
            method::TURN_TOOL_CALL_ARGUMENTS,
            json!({"call_id": "call-1", "delta": "{\"command\": \"ls\""}),
        ),
        (
            method::TURN_TOOL_OUTPUT_COMPRESSION,
            json!({"call_id": "call-1", "tool": "bash", "phase": "finished", "before_chars": 12345, "after_chars": 1234, "output": "压缩后的正文", "error": ""}),
        ),
    ]
}

#[test]
fn host_event_samples_cover_every_method() {
    let covered: Vec<&str> = host_event_samples().iter().map(|(name, _)| *name).collect();
    for declared in HostEvent::METHODS {
        assert!(covered.contains(declared), "方法 {declared} 缺少负载样本");
    }
    assert_eq!(
        covered.len(),
        HostEvent::METHODS.len(),
        "样本数量与方法数量不符"
    );
}

#[test]
fn host_events_round_trip_through_lines() {
    for (name, params) in host_event_samples() {
        let frame = Frame::notification(name, params);
        let line = frame.to_line();
        assert!(!line.contains('\n'), "方法 {name} 的帧不是单行");
        let parsed = Frame::parse(&line).expect("回读帧");
        let event = HostEvent::from_frame(&parsed).expect("解帧");
        assert_eq!(event.method(), name);
        assert_eq!(event.to_frame(), parsed, "方法 {name} 回组帧不一致");
    }
}

/// 宿主 → 内核的命令样本；覆盖 `Command::METHODS` 的全部方法。
fn command_samples() -> Vec<(&'static str, Value)> {
    vec![
        (
            method::INITIALIZE,
            json!({"protocol_version": "1.0", "client": {"name": "cli"}}),
        ),
        (
            method::TURN_SUBMIT,
            json!({"turn_id": "t1", "user_text": "读一下 loop.py", "images": []}),
        ),
        (method::TURN_CANCEL, json!({"turn_id": "t1"})),
        (method::TURN_UNDO, json!({})),
        (method::SESSION_COMPACT, json!({})),
        (
            method::SUBAGENT_QUERY,
            json!({"action": "list", "task_id": ""}),
        ),
        (
            method::SESSION_SETTINGS,
            json!({
                "model": {"tools": [], "context_window_tokens": 200000},
                "compaction": {"trigger_context_tokens": 160000},
            }),
        ),
        (method::SESSION_LIST, json!({"archived": true, "limit": 10})),
        (method::SESSION_RENAME, json!({"title": "新的会话标题"})),
        (method::SESSION_ARCHIVE, json!({})),
        (
            method::SESSION_HISTORY,
            json!({"query": "测试", "limit": 20}),
        ),
        (method::SESSION_EVENTS, json!({})),
        (method::SESSION_NEW, json!({})),
        (
            method::SESSION_RESUME,
            json!({"session_id": "20260101-000000-abcdef"}),
        ),
        (
            method::SUBAGENT_RUN,
            json!({
                "agent_type": "review",
                "description": "评审当前代码变更",
                "prompt": "请评审当前工作区的代码变更。",
            }),
        ),
        (
            method::SESSION_APPEND,
            json!({"role": "assistant", "content": "[评审报告]\n未发现问题。"}),
        ),
        (
            method::WORKSPACE_SWITCH,
            json!({"path": "D:/work/demo"}),
        ),
        (method::SHUTDOWN, json!({})),
    ]
}

/// `session.append` 的 `role` 默认是 assistant：省略时的回组帧要补上同一个默认值。
#[test]
fn session_append_defaults_to_assistant_role() {
    let frame = Frame::request(
        Id::Number(1),
        method::SESSION_APPEND,
        json!({"content": "报告"}),
    );
    let parsed = Frame::parse(&frame.to_line()).expect("回读帧");
    let pending = Command::from_frame(&parsed).expect("解帧");
    match pending.command {
        Command::SessionAppend(params) => {
            assert_eq!(params.role, "assistant");
            assert_eq!(params.content, "报告");
        }
        other => panic!("应当是 session.append：{other:?}"),
    }

    // `session.list` 与 `session.history` 的 limit 都有默认值，缺省时不该解帧失败。
    for name in [method::SESSION_LIST, method::SESSION_HISTORY] {
        let frame = Frame::request(Id::Number(2), name, json!({}));
        let parsed = Frame::parse(&frame.to_line()).expect("回读帧");
        Command::from_frame(&parsed)
            .unwrap_or_else(|error| panic!("方法 {name} 应当能用默认负载解帧：{error}"));
    }
}

#[test]
fn commands_round_trip_and_cover_every_method() {
    let samples = command_samples();
    let covered: Vec<&str> = samples.iter().map(|(name, _)| *name).collect();
    for declared in Command::METHODS {
        assert!(covered.contains(declared), "方法 {declared} 缺少负载样本");
    }
    assert_eq!(
        covered.len(),
        Command::METHODS.len(),
        "样本数量与方法数量不符"
    );

    for (name, params) in samples {
        let frame = Frame::request(Id::Number(1), name, params);
        let parsed = Frame::parse(&frame.to_line()).expect("回读帧");
        let pending = Command::from_frame(&parsed).expect("解帧");
        assert_eq!(pending.id, Id::Number(1));
        assert_eq!(pending.command.method(), name);
        assert_eq!(
            pending.command.to_frame(Id::Number(1)),
            parsed,
            "方法 {name} 回组帧不一致"
        );
    }
}

#[test]
fn tool_batch_round_trips_with_its_result() {
    let calls = json!([
        serde_json::from_str::<Value>(TOOL_CALL).unwrap(),
        {"name": "grep", "arguments": {"pattern": "todo"}, "id": "c2", "function_name": "grep"},
    ]);
    let frame = Frame::request(
        Id::Text("r-1".into()),
        method::TOOL_BATCH,
        json!({"turn_id": "t1", "step": 2, "calls": calls}),
    );
    let batch = ToolBatch::from_frame(&frame).expect("解 tool.batch");
    assert_eq!(batch.turn_id, "t1");
    assert_eq!(batch.step, 2);
    assert_eq!(batch.calls.len(), 2);
    assert_eq!(batch.to_frame(Id::Text("r-1".into())), frame);

    let result = json!({
        "observations": [{
            "tool_call": serde_json::from_str::<Value>(TOOL_CALL).unwrap(),
            "result": serde_json::from_str::<Value>(TOOL_RESULT).unwrap(),
            "message": {"role": "tool", "content": "done"},
            "followup_messages": [],
        }],
    });
    let parsed = ToolBatchResult::from_result(&result).expect("解观察批次");
    assert_eq!(parsed.to_result(), result);
}

#[test]
fn model_hook_request_and_result_round_trip() {
    let request = ModelHookRequest {
        messages: vec![json!({"role": "user", "content": "你好"})],
        model: "gpt-4o".into(),
    };
    let frame = request.to_frame(Id::Number(5));
    assert!(frame.is_request());
    assert_eq!(frame.method(), Some(method::MODEL_HOOK));
    assert_eq!(
        ModelHookRequest::from_frame(&frame).expect("解 model.hook"),
        request
    );

    let result = ModelHookResult {
        messages: vec![json!({"role": "user", "content": "你好（已改写）"})],
    };
    assert_eq!(
        ModelHookResult::from_result(&result.to_result()).expect("解 model.hook 结果"),
        result
    );
}

#[test]
fn model_reply_request_round_trips() {
    let request = ModelRequest {
        turn_id: "t1".into(),
        messages: vec![json!({"role": "user", "content": "你好"})],
    };
    let frame = request.to_frame(Id::Number(7));
    assert!(frame.is_request());
    assert_eq!(frame.method(), Some(method::MODEL_REPLY));
    assert_eq!(
        ModelRequest::from_frame(&frame).expect("解 model.reply"),
        request
    );

    assert!(ModelRequest::from_frame(&HostEvent::ProtocolWait.to_frame()).is_err());
    assert!(ModelRequest::from_frame(&Command::Shutdown.to_frame(Id::Number(1))).is_err());
}

#[test]
fn wrong_direction_and_bad_payloads_are_rejected() {
    let notification = HostEvent::ProtocolWait.to_frame();
    assert!(Command::from_frame(&notification).is_err());

    let request = Command::Shutdown.to_frame(Id::Number(1));
    assert!(HostEvent::from_frame(&request).is_err());
    assert!(ToolBatch::from_frame(&request).is_err());

    let unknown = Frame::request(Id::Number(1), "does.not.exist", json!({}));
    assert!(Command::from_frame(&unknown).is_err());

    let bad_params = Frame::request(Id::Number(1), method::TURN_SUBMIT, json!({"turn_id": "t1"}));
    assert!(Command::from_frame(&bad_params).is_err());

    let bad_event = Frame::notification("turn.token_usage_bogus", json!({}));
    assert!(HostEvent::from_frame(&bad_event).is_err());
}

#[test]
fn version_negotiation_compares_major_only() {
    assert_eq!(negotiate_version("1.0"), Ok(PROTOCOL_VERSION));
    assert_eq!(negotiate_version("1"), Ok(PROTOCOL_VERSION));
    assert_eq!(negotiate_version("1.7.3"), Ok(PROTOCOL_VERSION));
    assert_eq!(negotiate_version(" 1.0 "), Ok(PROTOCOL_VERSION));
    assert!(matches!(
        negotiate_version("2.0"),
        Err(VersionError::UnsupportedVersion { .. })
    ));
    assert!(negotiate_version("").is_err());
    assert!(negotiate_version("v1").is_err());
}

#[test]
fn initialize_helpers_use_the_declared_version_and_error_code() {
    assert_eq!(
        initialize_result(),
        json!({"protocol_version": PROTOCOL_VERSION})
    );
    let error = unsupported_version_error("2.0");
    assert_eq!(error.code, error_code::UNSUPPORTED_PROTOCOL_VERSION);
    assert_eq!(
        error.data,
        Some(json!({"supported": PROTOCOL_VERSION, "host": "2.0"}))
    );
}
