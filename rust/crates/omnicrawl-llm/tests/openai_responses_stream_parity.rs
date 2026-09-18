//! OpenAI Responses 流事件映射的跨语言 parity：期望值来自 Python 真实现。
//!
//! 同一串事件负载喂给两侧，比对产出的事件序列（统一投影成 `kind` 形式）、
//! 以及截断类错误的消息、可重试标记与错误码。

use omnicrawl_llm::{ResponsesStreamState, RuntimeErrorKind};
use omnicrawl_protocol::ModelStreamEvent;
use serde_json::{json, Value};

const FIXTURE: &str = include_str!("fixtures/openai_responses_stream_parity.json");

fn fixture() -> Value {
    serde_json::from_str(FIXTURE).expect("fixture 不是合法 JSON")
}

/// 与生成器同一套投影：只保留两侧都能表达的事实。
fn event_to_json(event: &ModelStreamEvent) -> Value {
    match event {
        ModelStreamEvent::TextDelta(delta) => json!({"kind": "text_delta", "text": delta.text}),
        ModelStreamEvent::ReasoningDelta(delta) => {
            json!({"kind": "reasoning_delta", "text": delta.text})
        }
        ModelStreamEvent::ToolCallStarted(started) => json!({
            "kind": "tool_call_started",
            "call_id": started.call_id,
            "name": started.name,
        }),
        ModelStreamEvent::ToolCallCompleted(completed) => json!({
            "kind": "tool_call_completed",
            "call_id": completed.call_id,
            "name": completed.name,
            "arguments": Value::Object(completed.arguments.clone()),
        }),
        ModelStreamEvent::UsageReported(usage) => json!({
            "kind": "usage",
            "input_tokens": usage.input_tokens,
            "output_tokens": usage.output_tokens,
            "cached_input_tokens": usage.cached_input_tokens,
            "reasoning_tokens": usage.reasoning_tokens,
        }),
        ModelStreamEvent::Finished { finish_reason } => {
            json!({"kind": "finished", "finish_reason": finish_reason})
        }
        ModelStreamEvent::ProviderWarning(warning) => json!({
            "kind": "warning",
            "code": warning.code,
            "message": warning.message,
        }),
        ModelStreamEvent::ToolCallArgumentsDelta(delta) => json!({
            "kind": "tool_call_arguments_delta",
            "call_id": delta.call_id,
            "delta": delta.delta,
        }),
    }
}

#[test]
fn responses_stream_mapping_matches_python() {
    let fixture = fixture();
    let cases = fixture["cases"].as_array().expect("cases");
    assert!(!cases.is_empty(), "数据集为空");

    for case in cases {
        let label = case["label"].as_str().unwrap_or("");
        let payloads = case["payloads"].as_array().expect("payloads");
        let mut state = ResponsesStreamState::new();
        let mut events: Vec<ModelStreamEvent> = Vec::new();
        let mut failure: Option<(String, bool)> = None;
        for payload in payloads {
            state.handle_event(payload, &mut events);
        }
        match state.finish(&mut events) {
            Ok(()) => {}
            Err(error) => {
                failure = Some((error.message.clone(), error.retryable));
                assert_eq!(
                    error.kind,
                    RuntimeErrorKind::StreamInterrupted,
                    "错误种类（{label}）"
                );
            }
        }

        let projected: Vec<Value> = events.iter().map(event_to_json).collect();
        assert_eq!(json!(projected), case["events"], "事件序列（{label}）");
        match (&failure, case.get("error")) {
            (Some((message, retryable)), Some(expected)) => {
                assert_eq!(
                    message.as_str(),
                    expected.as_str().expect("error"),
                    "错误文案（{label}）"
                );
                assert_eq!(
                    *retryable,
                    case["retryable"].as_bool().expect("retryable"),
                    "可重试标记（{label}）"
                );
                assert_eq!(
                    case["code"].as_str().expect("code"),
                    "STREAM_INTERRUPTED",
                    "错误码（{label}）"
                );
            }
            (None, None) => {}
            (Some((message, _)), None) => panic!("期望成功但报错（{label}）：{message}"),
            (None, Some(expected)) => panic!("期望报错但成功（{label}）：{expected}"),
        }
    }
}
