//! 内核侧的审批审计接线：宿主拒绝的调用落进会话转录。
//!
//! 拒绝事实由宿主经 `tool.batch` 的观察回填（`result.error_code = denied`），内核据此补
//! `tool_call_denied` 事件——会话投影与历史靠这个事件还原「用户拒绝执行」的上下文。
//! 语义基准是 `omnicrawl/agent/controllers/tools/approval.py` 的拒绝分支。

use omnicrawl_controllers::approval::denied_event_payload;
use omnicrawl_controllers::tool_args::public_tool_arguments;
use omnicrawl_core::AgentLoopObservation;
use omnicrawl_session::{utc_now, SessionStore};

/// 把被拒绝的调用写进会话转录，返回落盘条数。
///
/// 参数按 `public_tool_arguments` 投影后再落盘：会话转录里不出现 shell 全文或密钥原文。
/// 写盘失败只少记一条审计，不阻断工具批次。
pub fn record_denied_calls(
    observations: &[AgentLoopObservation],
    store: &SessionStore,
    session_id: &str,
) -> usize {
    let mut written = 0;
    for observation in observations {
        if observation.result.error_code.as_deref() != Some(omnicrawl_ipc::DENIED_ERROR_CODE) {
            continue;
        }
        let tool = observation.tool_call.name.as_str();
        let payload = denied_event_payload(
            tool,
            &public_tool_arguments(tool, &observation.tool_call.arguments),
            &observation.result.output,
        );
        let payload = match payload.as_object().cloned() {
            Some(payload) => payload,
            None => continue,
        };
        if store
            .append_event(session_id, "tool_call_denied", payload, None, utc_now())
            .is_ok()
        {
            written += 1;
        }
    }
    written
}

#[cfg(test)]
mod tests {
    use super::*;
    use omnicrawl_core::{ToolCall, ToolResult};
    use serde_json::{json, Map, Value};

    fn observation(
        tool: &str,
        arguments: Value,
        error_code: Option<&str>,
        output: &str,
    ) -> AgentLoopObservation {
        AgentLoopObservation {
            tool_call: ToolCall {
                name: tool.to_string(),
                arguments: arguments.as_object().cloned().unwrap_or_default(),
                id: "call-1".to_string(),
                function_name: tool.to_string(),
            },
            result: ToolResult {
                ok: error_code.is_none(),
                output: output.to_string(),
                full_output: output.to_string(),
                error_code: error_code.map(str::to_string),
                retryable: false,
            },
            message: json!({"role": "tool", "tool_call_id": "call-1", "content": output}),
            followup_messages: Vec::new(),
        }
    }

    fn store(name: &str) -> (std::path::PathBuf, SessionStore, String) {
        let root = std::env::temp_dir().join(format!(
            "omnicrawl-cli-approval-audit-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let store = SessionStore::open(&root);
        store.ensure().expect("会话根应当就绪");
        let session_id = store
            .start_session(
                &root.to_string_lossy(),
                "审批审计测试",
                omnicrawl_session::utc_now(),
            )
            .expect("应当能建会话")
            .session_id;
        (root, store, session_id)
    }

    #[test]
    fn denied_call_lands_as_session_event() {
        let (_root, store, session_id) = store("denied");
        let mut arguments = Map::new();
        arguments.insert("command".to_string(), json!("rm -rf build"));
        let observations = vec![observation(
            "bash",
            Value::Object(arguments),
            Some(omnicrawl_ipc::DENIED_ERROR_CODE),
            "用户取消执行：bash。",
        )];

        assert_eq!(record_denied_calls(&observations, &store, &session_id), 1);

        let events = store.read_events(&session_id).expect("读事件");
        let denied = events
            .iter()
            .find(|event| event.event_type == "tool_call_denied")
            .expect("应当有 tool_call_denied 事件");
        assert_eq!(denied.payload["tool"], json!("bash"));
        assert_eq!(denied.payload["reason"], json!("用户取消执行：bash。"));
        assert!(denied.payload["arguments"].is_object());
    }

    #[test]
    fn approved_and_failed_calls_are_not_audited() {
        let (_root, store, session_id) = store("approved");
        let observations = vec![
            observation("bash", json!({}), None, "命令输出"),
            observation("bash", json!({}), Some("tool_panicked"), "崩了"),
        ];

        assert_eq!(record_denied_calls(&observations, &store, &session_id), 0);

        let events = store.read_events(&session_id).expect("读事件");
        assert!(events
            .iter()
            .all(|event| event.event_type != "tool_call_denied"));
    }
}
