//! `controllers/tools/approval.py` 编排面的跨语言对照。
//!
//! 数据集是冻结的对照契约：`python rust/tools/gen_controllers_fixture.py` 重新生成
//! `tests/fixtures/controllers_parity.json` 的 `approval_flow` 段——探针按真实流程跑
//! `_approve_tool_for_batch` / `_execute_approved_tool`，记录钩子与落盘事件的轨迹、
//! 事件载荷与展示文本。本套件用内核阶段表推导期望轨迹，再与 Python 轨迹逐项比对。

use omnicrawl_controllers::approval::{
    approved_event_payload, denied_event_payload, display_text, effective_approval_mode,
    ApprovalPhase, ExecutionPhase, APPROVAL_PHASES, EXECUTION_PHASES,
};
use serde_json::Value;

const FIXTURE: &str = include_str!("fixtures/controllers_parity.json");

fn section() -> Value {
    let fixture: Value = serde_json::from_str(FIXTURE).expect("fixture 不是合法 JSON");
    fixture["approval_flow"].clone()
}

fn strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .expect("字符串数组")
        .iter()
        .map(|item| item.as_str().expect("字符串").to_string())
        .collect()
}

/// 用内核阶段表推导本节拍的期望轨迹；短路点由对照输入（拒绝钩子、Schema 结果、是否被拒）决定。
fn expected_trace(case: &Value) -> Vec<String> {
    let denied_hook = case["denied_hook"].as_str();
    let schema_failed = !case["schema_issues"]
        .as_array()
        .expect("schema_issues")
        .is_empty();
    let rejected = case["rejected"].as_bool().expect("rejected");

    let mut trace = vec![ApprovalPhase::PluginCallBefore.label().to_string()];
    if denied_hook == Some("tool.call.before") {
        trace.push("event:tool_call_denied".to_string());
        return trace;
    }
    trace.push(ApprovalPhase::SchemaValidation.label().to_string());
    if schema_failed {
        trace.push("event:tool_call_denied".to_string());
        return trace;
    }
    trace.push(ApprovalPhase::PluginApprovalBefore.label().to_string());
    if denied_hook == Some("tool.approval.before") {
        trace.push("event:tool_call_denied".to_string());
        return trace;
    }
    trace.push(ApprovalPhase::Decision.label().to_string());
    if rejected {
        trace.push("event:tool_call_denied".to_string());
        trace.push(ApprovalPhase::PluginApprovalAfter.label().to_string());
        return trace;
    }
    trace.push(ApprovalPhase::PluginApprovalAfter.label().to_string());
    trace.push("event:tool_call_approved".to_string());
    trace.push(ExecutionPhase::PluginExecuteBefore.label().to_string());
    if denied_hook == Some("tool.execute.before") {
        return trace;
    }
    trace.push(ExecutionPhase::ToolRun.label().to_string());
    if case["run_failure"].is_string() {
        trace.push(ExecutionPhase::PluginExecuteError.label().to_string());
        return trace;
    }
    trace.push(ExecutionPhase::PluginExecuteAfter.label().to_string());
    trace
}

#[test]
fn flow_traces_match_python() {
    let data = section();
    let cases = data["flow"].as_array().expect("flow");
    assert!(cases.len() >= 8, "对照场景过少：{}", cases.len());
    for case in cases {
        let label = case["label"].as_str().expect("label");
        assert_eq!(
            expected_trace(case),
            strings(&case["trace"]),
            "编排轨迹（{label}）"
        );
    }
}

#[test]
fn phase_tables_match_python_first_case() {
    let data = section();
    let case = &data["flow"].as_array().expect("flow")[0];
    let mut expected: Vec<String> = APPROVAL_PHASES
        .iter()
        .map(|p| p.label().to_string())
        .collect();
    expected.push("event:tool_call_approved".to_string());
    expected.extend(EXECUTION_PHASES.iter().map(|p| p.label().to_string()));
    assert_eq!(expected, strings(&case["trace"]), "全放行路径的阶段表展开");
}

#[test]
fn event_payloads_match_builders() {
    let data = section();
    let mut seen = 0;
    for case in data["flow"].as_array().expect("flow") {
        let label = case["label"].as_str().expect("label");
        for event in case["events"].as_array().expect("events") {
            let event_type = event["type"].as_str().expect("type");
            let payload = &event["payload"];
            let produced = match event_type {
                "tool_call_denied" => denied_event_payload(
                    payload["tool"].as_str().expect("tool"),
                    &payload["arguments"],
                    payload["reason"].as_str().expect("reason"),
                ),
                "tool_call_approved" => approved_event_payload(
                    payload["tool"].as_str().expect("tool"),
                    &payload["arguments"],
                    payload["mode"].as_str().expect("mode"),
                ),
                other => panic!("未预期的落盘事件：{other}（{label}）"),
            };
            assert_eq!(produced, *payload, "事件载荷（{label}）");
            seen += 1;
        }
    }
    assert!(seen >= 5, "事件载荷对照样本过少：{seen}");
}

#[test]
fn effective_mode_matches_python() {
    let data = section();
    for case in data["mode"].as_array().expect("mode") {
        let label = case["label"].as_str().expect("label");
        assert_eq!(
            effective_approval_mode(case["override"].as_str(), case["configured"].as_str()),
            case["mode"].as_str().expect("mode"),
            "生效模式（{label}）"
        );
    }
}

#[test]
fn display_text_matches_python() {
    let data = section();
    let mut seen = 0;
    for case in data["flow"].as_array().expect("flow") {
        if !case["executed"].as_bool().expect("executed") {
            continue;
        }
        // 只有工具成功返回的路径才有展示文本：插件拒绝或工具抛错的结果里没有 full_output。
        if !case["result_ok"].as_bool().unwrap_or(false) {
            continue;
        }
        let label = case["label"].as_str().expect("label");
        let full_input = case["full_output_input"]
            .as_str()
            .expect("full_output_input");
        let output = case["output"].as_str().expect("output");
        assert_eq!(
            display_text(full_input, output),
            case["full_output"].as_str().expect("full_output"),
            "展示文本（{label}）"
        );
        seen += 1;
    }
    assert!(seen >= 2, "展示文本对照样本过少：{seen}");
}
