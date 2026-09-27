//! 契约 parity：内核声明的宿主事件必须与 Python 侧 `run_stream` 的回调一一对应。
//!
//! fixture 由 `rust/tools/gen_host_bridge_fixture.py` 反射 Python 真实现生成：回调多一个、
//! 少一个或改名，这里都会红。

use omnicrawl_ipc::bridge::{method, Command, HostEvent};
use serde_json::Value;

const FIXTURE: &str = include_str!("fixtures/host_bridge.json");

/// 内核自产的通知：不对应 `run_stream` 的回调。
///
/// `turn.finished` 对应 `run_stream` 的返回值；其余三个由 agent runtime 内部的触发点
/// （压缩计量、模型请求前后）产生，Python 侧不经宿主回调下发，而在 Rust 里由宿主
/// 收到通知后分发同名插件 Hook。
const KERNEL_ORIGIN_EVENTS: &[&str] = &[
    method::TURN_FINISHED,
    method::TURN_CONTEXT_COMPACTION,
    method::TURN_MODEL_RESPONSE_AFTER,
    method::TURN_MODEL_REQUEST_ERROR,
];

/// **Rust 侧新增**（Python 没有对映）的事件：工具调用随模型流增量渲染、
/// 工具输出压缩的阶段计量、会话区提示（脱敏告警落点）。Python 只在批次执行时才画卡片，
/// 也不外发压缩进度与这条提示，所以它们没有 Python 回调可对应，断言时按「已声明但无回调」放行。
const RUST_ONLY_EVENTS: &[&str] = &[
    method::TURN_TOOL_CALL_STARTED,
    method::TURN_TOOL_CALL_ARGUMENTS,
    method::TURN_TOOL_OUTPUT_COMPRESSION,
    method::TURN_NOTICE,
];

/// Python 回调名 → 协议 v1 方法名。
///
/// `cancel_check` 是宿主 → 内核的通知；`turn.finished` 不在表里，因为它对应
/// `run_stream` 的返回值而不是某个回调。
const CALLBACK_METHODS: &[(&str, &str)] = &[
    ("cancel_check", method::TURN_CANCEL),
    ("on_delta", method::TURN_DELTA),
    ("on_protocol_wait", method::TURN_PROTOCOL_WAIT),
    ("on_reasoning_delta", method::TURN_REASONING_DELTA),
    ("on_retry_status", method::TURN_RETRY_STATUS),
    ("on_status", method::TURN_STATUS),
    ("on_stream_rollback", method::TURN_STREAM_ROLLBACK),
    ("on_subagent_event", method::SUBAGENT_EVENT),
    ("on_todo_update", method::TODO_UPDATE),
    ("on_token_usage", method::TURN_TOKEN_USAGE),
    ("on_tool_output_update", method::TOOL_OUTPUT_UPDATE),
    ("on_tool_result", method::TOOL_FINISHED),
    ("on_tool_start", method::TOOL_STARTED),
];

fn fixture() -> Value {
    serde_json::from_str(FIXTURE).expect("fixture 不是合法 JSON")
}

#[test]
fn every_python_callback_maps_to_exactly_one_method() {
    let fixture = fixture();
    let declared: Vec<String> = fixture["run_stream"]["callbacks"]
        .as_array()
        .expect("fixture 缺少 run_stream.callbacks")
        .iter()
        .map(|value| value.as_str().expect("回调名必须是字符串").to_string())
        .collect();
    let mut expected: Vec<String> = CALLBACK_METHODS
        .iter()
        .map(|(callback, _)| callback.to_string())
        .collect();
    expected.sort();

    assert_eq!(
        declared, expected,
        "Python 侧回调与协议 v1 映射表不一致（回调增删或改名都要同步协议）"
    );
}

#[test]
fn mapping_uses_only_declared_methods() {
    let events: Vec<&str> = HostEvent::METHODS.to_vec();
    for (callback, mapped) in CALLBACK_METHODS {
        let is_declared = events.contains(mapped) || *mapped == method::TURN_CANCEL;
        assert!(is_declared, "回调 {callback} 映射到未声明的方法 {mapped}");
    }
    let mapped: Vec<&str> = CALLBACK_METHODS.iter().map(|(_, m)| *m).collect();
    for declared in HostEvent::METHODS {
        if KERNEL_ORIGIN_EVENTS.contains(declared) {
            // 由回合收尾直接产生，不加回调；由下面两个内核自产事件测试覆盖。
            continue;
        }
        if RUST_ONLY_EVENTS.contains(declared) {
            continue;
        }
        assert!(
            mapped.contains(declared),
            "方法 {declared} 没有对应的 Python 回调"
        );
    }
}

#[test]
fn finished_event_comes_from_the_turn_result_not_a_callback() {
    let callbacks: Vec<&str> = CALLBACK_METHODS.iter().map(|(_, mapped)| *mapped).collect();
    assert!(
        !callbacks.contains(&method::TURN_FINISHED),
        "turn.finished 对应 run_stream 的返回值，不应绑定回调"
    );
    assert!(HostEvent::METHODS.contains(&method::TURN_FINISHED));
    // `cancel_check` 映射到宿主命令 `turn.cancel`，不是内核→宿主事件，要先排除。
    let callback_events = CALLBACK_METHODS
        .iter()
        .filter(|(_, mapped)| *mapped != method::TURN_CANCEL)
        .count();
    assert_eq!(
        callback_events + KERNEL_ORIGIN_EVENTS.len() + RUST_ONLY_EVENTS.len(),
        HostEvent::METHODS.len(),
        "事件数应等于（非 cancel 的回调数 + 内核自产事件数 + Rust 侧新增事件数）"
    );
}

#[test]
fn context_compaction_is_a_kernel_origin_event() {
    // 压缩计量由内核回合收尾发出，宿主收到后分发 `context.compaction.after_turn`。
    assert!(HostEvent::METHODS.contains(&method::TURN_CONTEXT_COMPACTION));
    assert!(
        !CALLBACK_METHODS
            .iter()
            .any(|(_, mapped)| *mapped == method::TURN_CONTEXT_COMPACTION),
        "turn.context_compaction 不对应 run_stream 回调"
    );
}

#[test]
fn method_spaces_do_not_overlap() {
    // 需要宿主响应的请求、单向通知、宿主命令三者方法名不得重叠，
    // 否则两侧会把同一条帧理解成不同方向的消息。
    for request in [method::TOOL_BATCH, method::MODEL_REPLY, method::MODEL_HOOK] {
        assert!(
            !HostEvent::METHODS.contains(&request),
            "{request} 需要宿主响应，不能同时是单向通知"
        );
        assert!(
            !Command::METHODS.contains(&request),
            "{request} 是内核发给宿主的请求，不是宿主命令"
        );
    }
    for event in HostEvent::METHODS {
        assert!(
            !Command::METHODS.contains(event),
            "单向通知 {event} 与宿主命令重名"
        );
    }
}

#[test]
fn loop_ports_are_pinned() {
    let fixture = fixture();
    let mut ports: Vec<String> = fixture["loop_ports"]
        .as_array()
        .expect("fixture 缺少 loop_ports")
        .iter()
        .map(|value| value.as_str().expect("端口名必须是字符串").to_string())
        .collect();
    ports.sort();
    assert_eq!(
        ports,
        vec![
            "cancel_check".to_string(),
            "execute_tool_batch".to_string(),
            "request_reply".to_string(),
            "stop_check".to_string(),
        ],
        "循环端口是协议 v1 的映射基准：execute_tool_batch → tool.batch，cancel_check → turn.cancel，\
         request_reply 过渡期经 model.reply 由宿主代答，stop_check 由内核判定"
    );
    let other: Vec<String> = fixture["run_stream"]["other_params"]
        .as_array()
        .expect("fixture 缺少 run_stream.other_params")
        .iter()
        .map(|value| value.as_str().expect("入参名必须是字符串").to_string())
        .collect();
    assert_eq!(
        other,
        vec!["user_text".to_string()],
        "run_stream 的非回调入参只有 user_text"
    );
}
