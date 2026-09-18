//! `turn/loop.py` 接线面的对照：回调轨迹、端口入参、收尾与失败分类。
//!
//! 数据集由 `rust/tools/gen_controllers_fixture.py` 用真实现生成：探针只补宿主属性与
//! 编排副作用，两个端口按脚本应答，循环本体、收尾与失败分类都走 `run_stream`。

use omnicrawl_controllers::turn::{
    run_stream, HostEventCallbacks, StreamPorts, TurnCallbacks, TurnReport,
};
use omnicrawl_core::{
    AgentLoopLimits, AgentLoopObservation, AgentModelReply, LoopError, ToolCall, ToolResult,
};
use omnicrawl_ipc::bridge::{method, HostEvent};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

const FIXTURE: &str = include_str!("fixtures/controllers_parity.json");

/// Python 端口的回调形参 → 接线交给宿主的报告方法；空串表示该入参由宿主自己持有。
const REPLY_PORT_PARAMS: &[(&str, &str)] = &[
    ("messages", "messages"),
    ("on_delta", "delta"),
    ("on_token_usage", "token_usage"),
    ("on_protocol_wait", "protocol_wait"),
    ("on_retry_status", "retry_status"),
    ("on_stream_rollback", "stream_rollback"),
];

const BATCH_PORT_PARAMS: &[(&str, &str)] = &[
    ("report_tool_start", "tool_start"),
    ("report_tool_result", "tool_result"),
    ("report_tool_output_update", "tool_output_update"),
    ("status", "status"),
    ("prompt", ""),
    ("record_tool_execution", ""),
];

fn fixture() -> Value {
    serde_json::from_str(FIXTURE).expect("fixture 不是合法 JSON")
}

fn sha256_hex(text: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(text.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn shorten(text: &str) -> String {
    if text.chars().count() <= 120 {
        return text.to_string();
    }
    let mut shortened: String = text.chars().take(120).collect();
    shortened.push('…');
    shortened
}

fn call_value(call: &ToolCall) -> Value {
    let arguments: Map<String, Value> = call.arguments.clone();
    json!({
        "name": call.name,
        "arguments": arguments,
        "id": call.id,
        "function_name": call.function_name,
    })
}

/// 与生成器的 `result_view` 同形状：长文本只留摘要，短文本连原文一起对照。
fn result_value(result: &ToolResult) -> Value {
    json!({
        "ok": result.ok,
        "output": shorten(&result.output),
        "output_len": result.output.chars().count(),
        "output_sha256": sha256_hex(&result.output),
        "full_output": shorten(&result.full_output),
        "full_output_len": result.full_output.chars().count(),
        "full_output_sha256": sha256_hex(&result.full_output),
        "model_images": 0,
        "error_code": result.error_code,
        "completed_at_is_none": true,
    })
}

/// 记录轨迹的回调面：`with_status` / `with_retry` 对应用例是否注册了这两个回调。
#[derive(Default)]
struct RecordingFace {
    trace: Vec<Value>,
    with_status: bool,
    with_retry: bool,
}

impl RecordingFace {
    fn push(&mut self, name: &str, args: Vec<Value>) {
        self.trace.push(json!([name, args]));
    }
}

impl TurnCallbacks for RecordingFace {
    fn on_delta(&mut self, text: &str) {
        self.push("on_delta", vec![json!(text)]);
    }

    fn on_status(&mut self, message: &str) {
        if self.with_status {
            self.push("on_status", vec![json!(message)]);
        }
    }

    fn on_retry_status(&mut self, message: &str) {
        if self.with_retry {
            self.push("on_retry_status", vec![json!(message)]);
        } else {
            // 未注册专用回调：与 trait 缺省一致地落到状态回调。
            self.on_status(message);
        }
    }

    fn on_protocol_wait(&mut self) {
        self.push("on_protocol_wait", Vec::new());
    }

    fn on_stream_rollback(&mut self) {
        self.push("on_stream_rollback", Vec::new());
    }

    fn on_token_usage(&mut self, input_tokens: i64, output_tokens: i64, cached_input_tokens: i64) {
        self.push(
            "on_token_usage",
            vec![
                json!(input_tokens),
                json!(output_tokens),
                json!(cached_input_tokens),
            ],
        );
    }

    fn on_reasoning_delta(&mut self, text: &str) {
        self.push("on_reasoning_delta", vec![json!(text)]);
    }

    fn on_tool_start(&mut self, step: usize, call: &ToolCall) {
        self.push("on_tool_start", vec![json!(step), call_value(call)]);
    }

    fn on_tool_result(&mut self, call: &ToolCall, result: &ToolResult) {
        self.push(
            "on_tool_result",
            vec![call_value(call), result_value(result)],
        );
    }

    fn on_tool_output_update(&mut self, call: &ToolCall, result: &ToolResult) {
        self.push(
            "on_tool_output_update",
            vec![call_value(call), result_value(result)],
        );
    }

    fn on_subagent_event(&mut self, name: &str, payload: &Value) {
        self.push("on_subagent_event", vec![json!(name), payload.clone()]);
    }

    fn on_todo_update(&mut self, todos: &Value) {
        self.push("on_todo_update", vec![todos.clone()]);
    }
}

/// 只注册 `on_delta` 与 `on_status` 的回调面：用来验证 trait 的重试回落缺省值。
#[derive(Default)]
struct MinimalFace {
    trace: Vec<Value>,
}

impl TurnCallbacks for MinimalFace {
    fn on_delta(&mut self, text: &str) {
        self.trace.push(json!(["on_delta", [text]]));
    }

    fn on_status(&mut self, message: &str) {
        self.trace.push(json!(["on_status", [message]]));
    }
}

#[derive(Default)]
struct PortRecord {
    messages: Option<Value>,
    batch_calls: Option<Value>,
    batch_first_step: Option<usize>,
}

/// 一个用例的观测结果。
struct Observed {
    ok: bool,
    final_text: Option<String>,
    error_message: Option<String>,
    cancelled: bool,
    terminal_event: Option<&'static str>,
    trace: Value,
    runner_messages: Option<Value>,
    turn_usage: Option<Value>,
    last_request_input_tokens: Option<i64>,
    gate_visible: Option<bool>,
    batch_calls: Option<Value>,
    batch_first_step: Option<usize>,
}

fn models_of(items: &[Value], field: &str) -> Option<String> {
    items.first()?.get(field)?.as_str().map(str::to_string)
}

fn script_of(scripts: &[Value], index: usize) -> Value {
    scripts
        .get(index)
        .cloned()
        .unwrap_or_else(|| json!({"content": ""}))
}

fn usages_of(script: &Value) -> Vec<(i64, i64, i64)> {
    script
        .get("usages")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    let values = item.as_array()?;
                    Some((
                        values.first()?.as_i64()?,
                        values.get(1)?.as_i64()?,
                        values.get(2)?.as_i64()?,
                    ))
                })
                .collect()
        })
        .unwrap_or_default()
}

fn run_case(case: &Value) -> Observed {
    let replies: Vec<Value> = case["replies"].as_array().cloned().unwrap_or_default();
    let batches: Vec<Value> = case["batches"].as_array().cloned().unwrap_or_default();
    let with_status = case["with_status"].as_bool().unwrap_or(false);
    let with_retry = case["with_retry"].as_bool().unwrap_or(false);
    let pause = case["pause"].as_bool().unwrap_or(false);
    let cancel_at = case["cancel_at"].as_u64().map(|value| value as usize);
    let context_messages: Vec<Value> = case["context_messages"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let history: Vec<Value> = case["history"].as_array().cloned().unwrap_or_default();
    let user_text = case["user_text"].as_str().unwrap_or_default();

    let mut face = RecordingFace {
        trace: Vec::new(),
        with_status,
        with_retry,
    };
    let mut record = PortRecord::default();
    let mut reply_index = 0usize;
    let mut batch_index = 0usize;
    let mut checks = 0usize;

    let mut cancel_check = move || {
        checks += 1;
        match cancel_at {
            Some(at) if checks == at => Err(LoopError::Cancelled("用户取消".to_string())),
            _ => Ok(()),
        }
    };
    let mut stop_check = move || pause;

    let mut request_reply = |messages: &mut Vec<Value>, report: &mut TurnReport<'_>| {
        record.messages = Some(Value::Array(messages.clone()));
        let script = script_of(&replies, reply_index);
        reply_index += 1;
        if let Some(error) = script.get("error").and_then(Value::as_str) {
            return Err(LoopError::ReplySource(error.to_string()));
        }
        for text in script
            .get("deltas")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
        {
            report.delta(text.as_str().unwrap_or_default());
        }
        for (input_tokens, output_tokens, cached) in usages_of(&script) {
            report.token_usage(input_tokens, output_tokens, cached);
        }
        if script["protocol_wait"].as_bool().unwrap_or(false) {
            report.protocol_wait();
        }
        if let Some(message) = script.get("retry").and_then(Value::as_str) {
            report.retry_status(message);
        }
        if script["rollback"].as_bool().unwrap_or(false) {
            report.stream_rollback();
        }
        if let Some(error) = script.get("error_after").and_then(Value::as_str) {
            return Err(LoopError::ReplySource(error.to_string()));
        }
        let content = script["content"].as_str().unwrap_or_default().to_string();
        let message = script
            .get("message")
            .filter(|value| !value.is_null())
            .cloned()
            .unwrap_or_else(|| json!({"role": "assistant", "content": content}));
        let tool_calls = script
            .get("tool_calls")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
            .iter()
            .map(|item| ToolCall {
                name: models_of(std::slice::from_ref(item), "name").unwrap_or_default(),
                arguments: item["arguments"].as_object().cloned().unwrap_or_default(),
                id: models_of(std::slice::from_ref(item), "id").unwrap_or_default(),
                function_name: models_of(std::slice::from_ref(item), "function_name")
                    .unwrap_or_default(),
            })
            .collect();
        Ok(AgentModelReply {
            message,
            content,
            tool_calls,
            reasoning: script["reasoning"].as_str().unwrap_or_default().to_string(),
            content_streamed: script["content_streamed"].as_bool().unwrap_or(false),
        })
    };

    let mut execute_tool_batch =
        |calls: &[ToolCall], first_step: usize, report: &mut TurnReport<'_>| {
            let script = script_of(&batches, batch_index);
            batch_index += 1;
            record.batch_calls = Some(json!(calls.iter().map(call_value).collect::<Vec<_>>()));
            record.batch_first_step = Some(first_step);
            if let Some(error) = script.get("error").and_then(Value::as_str) {
                return Err(LoopError::ToolBatch(error.to_string()));
            }
            let result = ToolResult {
                ok: true,
                output: "工具完成".to_string(),
                full_output: String::new(),
                error_code: None,
                retryable: false,
            };
            for name in script
                .get("reports")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
            {
                match name.as_str().unwrap_or_default() {
                    "start" => report.tool_start(first_step, &calls[0]),
                    "result" => report.tool_result(&calls[0], &result),
                    "update" => report.tool_output_update(&calls[0], &result),
                    _ => {}
                }
            }
            Ok(script
                .get("observations")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
                .iter()
                .map(|item| AgentLoopObservation {
                    tool_call: ToolCall {
                        name: models_of(std::slice::from_ref(&item["tool_call"]), "name")
                            .unwrap_or_default(),
                        arguments: item["tool_call"]["arguments"]
                            .as_object()
                            .cloned()
                            .unwrap_or_default(),
                        id: models_of(std::slice::from_ref(&item["tool_call"]), "id")
                            .unwrap_or_default(),
                        function_name: models_of(
                            std::slice::from_ref(&item["tool_call"]),
                            "function_name",
                        )
                        .unwrap_or_default(),
                    },
                    result: ToolResult {
                        ok: item["result"]["ok"].as_bool().unwrap_or(false),
                        output: item["result"]["output"]
                            .as_str()
                            .unwrap_or_default()
                            .to_string(),
                        full_output: item["result"]["full_output"]
                            .as_str()
                            .unwrap_or_default()
                            .to_string(),
                        error_code: item["result"]["error_code"].as_str().map(str::to_string),
                        retryable: item["result"]["retryable"].as_bool().unwrap_or(false),
                    },
                    message: item["message"].clone(),
                    followup_messages: item["followup_messages"]
                        .as_array()
                        .cloned()
                        .unwrap_or_default(),
                })
                .collect())
        };

    let ports = StreamPorts {
        request_reply: &mut request_reply,
        execute_tool_batch: &mut execute_tool_batch,
        cancel_check: Some(&mut cancel_check),
        stop_check: Some(&mut stop_check),
    };
    let result = run_stream(
        &context_messages,
        &history,
        user_text,
        AgentLoopLimits::default(),
        ports,
        &mut face,
    );

    match result {
        Ok(outcome) => Observed {
            ok: true,
            final_text: Some(outcome.final_text),
            error_message: None,
            cancelled: false,
            terminal_event: Some(if outcome.paused {
                "run_guard_paused"
            } else {
                "assistant_message"
            }),
            trace: Value::Array(face.trace),
            runner_messages: record.messages,
            turn_usage: Some(outcome.turn_usage.to_dict()),
            last_request_input_tokens: Some(outcome.last_request_input_tokens),
            gate_visible: None,
            batch_calls: record.batch_calls,
            batch_first_step: record.batch_first_step,
        },
        Err(failure) => {
            let cancelled = failure.is_cancelled();
            let terminal_event = if failure.is_input_error() {
                // 空输入在回合开始前就被拦下：没有任何回合事件。
                None
            } else if cancelled {
                Some("turn_cancelled")
            } else {
                Some("session_interrupted")
            };
            Observed {
                ok: false,
                final_text: None,
                error_message: Some(failure.message().to_string()),
                cancelled,
                terminal_event,
                trace: Value::Array(face.trace),
                runner_messages: record.messages,
                turn_usage: None,
                last_request_input_tokens: None,
                gate_visible: Some(failure.visible_output_seen || failure.tool_execution_seen),
                batch_calls: record.batch_calls,
                batch_first_step: record.batch_first_step,
            }
        }
    }
}

#[test]
fn wiring_matches_python_run_stream() {
    let fixture = fixture();
    let cases = fixture["turn_loop"]["cases"]
        .as_array()
        .expect("fixture 缺少 turn_loop.cases")
        .clone();
    assert!(!cases.is_empty(), "turn_loop 数据集为空");

    for case in &cases {
        let label = case["label"].as_str().expect("label");
        let observed = run_case(case);

        assert_eq!(
            observed.ok,
            case["ok"].as_bool().expect("ok"),
            "成败（{label}）"
        );
        assert_eq!(
            observed.final_text.as_deref(),
            case["final_text"].as_str(),
            "最终回复（{label}）"
        );
        assert_eq!(
            observed.error_message.as_deref(),
            case["error"]["message"].as_str(),
            "错误文案（{label}）"
        );
        assert_eq!(
            observed.cancelled,
            case["error"]["cancelled"].as_bool().unwrap_or(false),
            "取消分类（{label}）"
        );
        assert_eq!(
            observed.terminal_event,
            case["terminal_event"].as_str(),
            "回合终态事件（{label}）"
        );
        assert_eq!(observed.trace, case["trace"], "回调轨迹（{label}）");
        assert_eq!(
            observed.runner_messages.as_ref(),
            case.get("runner_messages").filter(|value| !value.is_null()),
            "循环收到的消息（{label}）"
        );
        if let Some(expected) = case.get("turn_usage").filter(|value| !value.is_null()) {
            assert_eq!(
                observed.turn_usage.as_ref(),
                Some(expected),
                "回合用量（{label}）"
            );
        }
        if let Some(expected) = case
            .get("last_request_input_tokens")
            .and_then(Value::as_i64)
        {
            assert_eq!(
                observed.last_request_input_tokens,
                Some(expected),
                "最近一次请求输入 token（{label}）"
            );
        }
        if let Some(expected) = case
            .get("recovery_gate")
            .filter(|value| !value.is_null())
            .and_then(|gate| gate["visible_output_seen"].as_bool())
        {
            assert_eq!(
                observed.gate_visible,
                Some(expected),
                "超限恢复判定入参（{label}）"
            );
        }
        let batch_port = case.get("batch_port").filter(|value| !value.is_null());
        assert_eq!(
            observed.batch_calls.as_ref(),
            batch_port.map(|port| port["calls"].clone()).as_ref(),
            "批次调用（{label}）"
        );
        assert_eq!(
            observed.batch_first_step,
            batch_port.and_then(|port| port["first_step"].as_u64().map(|v| v as usize)),
            "批次起始步号（{label}）"
        );
    }
}

#[test]
fn port_params_map_to_report_methods() {
    let fixture = fixture();
    let cases = fixture["turn_loop"]["cases"]
        .as_array()
        .expect("fixture 缺少 turn_loop.cases")
        .clone();

    let mut reply_seen: Vec<String> = Vec::new();
    let mut batch_seen: Vec<String> = Vec::new();
    for case in &cases {
        if let Some(params) = case["reply_port"]["params"].as_array() {
            for param in params {
                let name = param.as_str().expect("端口形参名").to_string();
                if !reply_seen.contains(&name) {
                    reply_seen.push(name);
                }
            }
        }
        if let Some(params) = case["batch_port"]["params"].as_array() {
            for param in params {
                let name = param.as_str().expect("端口形参名").to_string();
                if !batch_seen.contains(&name) {
                    batch_seen.push(name);
                }
            }
        }
    }

    reply_seen.sort();
    let mut expected_reply: Vec<String> = REPLY_PORT_PARAMS
        .iter()
        .map(|(param, _)| param.to_string())
        .collect();
    expected_reply.sort();
    assert_eq!(
        reply_seen, expected_reply,
        "回复端口的形参集合变了：Python 侧多一个、少一个或改名都要同步报告句柄"
    );

    batch_seen.sort();
    let mut expected_batch: Vec<String> = BATCH_PORT_PARAMS
        .iter()
        .map(|(param, _)| param.to_string())
        .collect();
    expected_batch.sort();
    assert_eq!(
        batch_seen, expected_batch,
        "工具批次端口的形参集合变了：接线代传的报告回调必须与协议 v1 的映射一致"
    );

    // 代传的回调必须在报告句柄上有对应方法（宿主自持的入参留空）。
    for (param, method) in REPLY_PORT_PARAMS {
        assert!(!method.is_empty(), "回复端口形参 {param} 缺少报告方法");
    }
    for (param, method) in BATCH_PORT_PARAMS {
        if ["prompt", "record_tool_execution"].contains(param) {
            assert!(method.is_empty(), "宿主自持的入参不该有报告方法：{param}");
        } else {
            assert!(!method.is_empty(), "批次端口形参 {param} 缺少报告方法");
        }
    }
}

#[test]
fn retry_status_defaults_to_status() {
    let mut face = MinimalFace::default();
    let mut cancel_check = || Ok(());
    let mut request_reply = |_messages: &mut Vec<Value>, report: &mut TurnReport<'_>| {
        report.retry_status("重试中");
        Ok(AgentModelReply {
            message: json!({"role": "assistant", "content": "ok"}),
            content: "ok".to_string(),
            tool_calls: Vec::new(),
            reasoning: String::new(),
            content_streamed: false,
        })
    };
    let mut execute_tool_batch =
        |_calls: &[ToolCall], _first_step: usize, _report: &mut TurnReport<'_>| Ok(Vec::new());
    let ports = StreamPorts {
        request_reply: &mut request_reply,
        execute_tool_batch: &mut execute_tool_batch,
        cancel_check: Some(&mut cancel_check),
        stop_check: None,
    };

    let outcome = run_stream(
        &[],
        &[],
        "做事",
        AgentLoopLimits::default(),
        ports,
        &mut face,
    )
    .expect("回合应当成功");

    assert_eq!(outcome.final_text, "ok");
    assert_eq!(
        face.trace,
        vec![
            json!(["on_status", ["重试中"]]),
            json!(["on_delta", ["ok"]]),
        ],
        "未注册 on_retry_status 时，重试提示要落到 on_status"
    );
}

#[test]
fn host_event_face_emits_protocol_notifications() {
    let mut events: Vec<HostEvent> = Vec::new();
    let mut cancel_check = || Ok(());
    let mut request_reply = |_messages: &mut Vec<Value>, report: &mut TurnReport<'_>| {
        report.delta("do");
        report.reasoning_delta("想");
        report.token_usage(10, 2, 3);
        report.protocol_wait();
        report.stream_rollback();
        Ok(AgentModelReply {
            message: json!({"role": "assistant", "content": "done"}),
            content: "done".to_string(),
            tool_calls: Vec::new(),
            reasoning: "想".to_string(),
            content_streamed: true,
        })
    };
    let mut execute_tool_batch =
        |_calls: &[ToolCall], _first_step: usize, _report: &mut TurnReport<'_>| Ok(Vec::new());
    let ports = StreamPorts {
        request_reply: &mut request_reply,
        execute_tool_batch: &mut execute_tool_batch,
        cancel_check: Some(&mut cancel_check),
        stop_check: None,
    };
    let outcome = {
        let mut emit = |event: HostEvent| events.push(event);
        let mut face = HostEventCallbacks::new(&mut emit);
        run_stream(
            &[],
            &[],
            "做事",
            AgentLoopLimits::default(),
            ports,
            &mut face,
        )
        .expect("回合应当成功")
    };

    let methods: Vec<&str> = events.iter().map(HostEvent::method).collect();
    assert_eq!(
        methods,
        vec![
            method::TURN_DELTA,
            method::TURN_REASONING_DELTA,
            method::TURN_TOKEN_USAGE,
            method::TURN_PROTOCOL_WAIT,
            method::TURN_STREAM_ROLLBACK,
        ],
        "回调面的线上形态就是协议 v1 的通知方法"
    );
    assert_eq!(
        outcome.finished_event("t1").method(),
        method::TURN_FINISHED,
        "回合结果对应 turn.finished，而不是某个回调"
    );
}
