//! 内核侧工具事件的落盘接线：`tool_call_requested` 与 `tool_result`。
//!
//! 语义基准是 `omnicrawl/agent/controllers/turn/loop.py` 的落盘点：
//!
//! - 模型每次请求工具时先落 `tool_call_requested`——参数走 `public_tool_arguments`
//!   投影（转录里不出现 shell 全文与密钥原文），并带上本批真正发往 Provider 的
//!   assistant 协议字段（`assistant_content` / 思考回传字段 / `function_name`）。
//! - 整批执行完、且输出预算 / 视觉 / 压缩都处理过之后落 `tool_result`：
//!   `output` 是展示全文，`model_output` 是模型可见输出；超长输出的 artifact 化、
//!   `output_sha256` / `storage` 等字段由会话存储（`prepare_event_payload`）补齐。
//!
//! 被拒绝的调用另有 `tool_call_denied`，落盘在 [`crate::approval_audit`]；同一次调用
//! 先有 `tool_call_requested`，被拒时再有 `tool_call_denied`，最后照旧有 `tool_result`
//! （观察里就是拒绝结果），与 Python 的事件顺序一致。
//!
//! 后台子任务批次**不落**这些事件：Python 在子代理循环里传
//! `persist_session_events=False`，Rust 侧对应 `RemoteTools::active_assistant` 为 `None`。

use omnicrawl_controllers::tool_args::public_tool_arguments;
use omnicrawl_controllers::turn::tool_events::{requested_event_payload, result_event_payload};
use omnicrawl_core::{AgentLoopObservation, ToolCall};
use omnicrawl_session::{utc_now, SessionStore};
use serde_json::{Map, Value};

/// 把本批全部调用写成 `tool_call_requested`，返回落盘条数。
///
/// `assistant_message` 是本批发往 Provider 的原始 assistant 消息（可能没有：宿主代答
/// 路径下回复里不带原文时就是 `None`）。写盘失败只少记一条，不阻断工具批次。
pub fn record_requested_calls(
    calls: &[ToolCall],
    assistant_message: Option<&Value>,
    store: &SessionStore,
    session_id: &str,
) -> usize {
    let mut written = 0;
    for call in calls {
        let payload = requested_event_payload(
            &call.name,
            public_tool_arguments(&call.name, &call.arguments),
            &call.id,
            &call.function_name,
            assistant_message,
        );
        if store
            .append_event(session_id, "tool_call_requested", payload, None, utc_now())
            .is_ok()
        {
            written += 1;
        }
    }
    written
}

/// 把整批观察写成 `tool_result`，返回落盘条数。
///
/// 走的是**处理过**的观察：输出预算、视觉代理与压缩旁路都在调用前生效，事件里的
/// `output` / `model_output` 与回填模型的文本同源（与 Python 的写盘时机一致）。
pub fn record_results(
    observations: &[AgentLoopObservation],
    store: &SessionStore,
    session_id: &str,
) -> usize {
    let mut written = 0;
    for observation in observations {
        let payload = result_event_payload(
            &observation.tool_call.name,
            &observation.tool_call.id,
            observation.result.ok,
            &observation.result.full_output,
            &observation.result.output,
            // 展示附件（HTML 预览）目前只活在宿主进程里：协议观察不带 `ui_artifact`，
            // 这里按 Python 的缺省写空对象；字段留着，免得旧会话与投影的键集不一致。
            Value::Object(Map::new()),
        );
        if store
            .append_event(session_id, "tool_result", payload, None, utc_now())
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
    use omnicrawl_core::ToolResult;
    use omnicrawl_session::project_session_history;
    use serde_json::json;
    use std::path::PathBuf;

    fn call(name: &str, id: &str) -> ToolCall {
        ToolCall {
            name: name.to_string(),
            arguments: json!({"path": "a.py"})
                .as_object()
                .cloned()
                .unwrap_or_default(),
            id: id.to_string(),
            function_name: name.to_string(),
        }
    }

    fn observation(call: ToolCall, ok: bool, output: &str, full_output: &str) -> AgentLoopObservation {
        AgentLoopObservation {
            message: json!({"role": "tool", "tool_call_id": &call.id, "content": output}),
            tool_call: call,
            result: ToolResult {
                ok,
                output: output.to_string(),
                full_output: full_output.to_string(),
                error_code: None,
                retryable: false,
            },
            followup_messages: Vec::new(),
        }
    }

    fn store(name: &str) -> (PathBuf, SessionStore, String) {
        let root = std::env::temp_dir().join(format!(
            "omnicrawl-cli-tool-events-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let store = SessionStore::open(&root);
        store.ensure().expect("会话根应当就绪");
        let session_id = store
            .start_session(&root.to_string_lossy(), "工具事件测试", omnicrawl_session::utc_now())
            .expect("应当能建会话")
            .session_id;
        (root, store, session_id)
    }

    #[test]
    fn requested_and_result_events_land_and_project_back_into_history() {
        let (_root, store, session_id) = store("round-trip");
        // 本批真正发往 Provider 的 assistant 原文：content + 思考 + 一条调用。
        let assistant = json!({
            "role": "assistant",
            "content": "先读一下文件。",
            "reasoning_content": "先看内容再回答。",
            "tool_calls": [{
                "id": "call-1",
                "type": "function",
                "function": {"name": "read_file", "arguments": "{\"path\": \"a.py\"}"},
            }],
        });

        assert_eq!(
            record_requested_calls(
                &[call("read_file", "call-1")],
                Some(&assistant),
                &store,
                &session_id
            ),
            1
        );
        assert_eq!(
            record_results(
                &[observation(call("read_file", "call-1"), true, "预览", "全文")],
                &store,
                &session_id
            ),
            1
        );

        let events = store.read_events(&session_id).expect("读事件");
        let requested = events
            .iter()
            .find(|event| event.event_type == "tool_call_requested")
            .expect("应当有 tool_call_requested 事件");
        assert_eq!(requested.payload["tool"], "read_file");
        assert_eq!(requested.payload["tool_call_id"], "call-1");
        assert_eq!(requested.payload["function_name"], "read_file");
        assert_eq!(requested.payload["assistant_content"], "先读一下文件。");
        assert_eq!(requested.payload["assistant_reasoning_content"], "先看内容再回答。");
        assert_eq!(requested.payload["arguments"], json!({"path": "a.py"}));
        // 协议原文（可能含明文密钥）不落盘。
        assert!(requested.payload.get("arguments_json").is_none());

        let result = events
            .iter()
            .find(|event| event.event_type == "tool_result")
            .expect("应当有 tool_result 事件");
        assert_eq!(result.payload["output"], "全文", "output 取展示全文");
        assert_eq!(result.payload["model_output"], "预览", "model_output 是模型可见输出");
        assert_eq!(result.payload["ui_artifact"], json!({}));
        // 哈希、体积与存放位置由会话存储补齐（与 Python 转录同键集）。
        assert_eq!(result.payload["storage"], "inline");
        assert!(result.payload["output_sha256"].is_string());
        assert_eq!(result.payload["output_size_chars"], 2);

        // 恢复投影：两条事件折回成「assistant tool_calls + tool 结果」协议消息，
        // 重启后（/resume）模型才看得到上一轮真正调用过什么。
        let history = project_session_history(&store.read_active_events(&session_id).expect("有效事件"));
        let calls: Vec<Value> = history
            .iter()
            .filter_map(|(_, message)| message.get("tool_calls").and_then(Value::as_array).cloned())
            .flatten()
            .collect();
        assert_eq!(calls.len(), 1, "投影应还原一条 tool_calls：{history:?}");
        assert_eq!(calls[0]["function"]["name"], "read_file");
        assert_eq!(calls[0]["id"], "call-1");
        let tool_message = history
            .iter()
            .map(|(_, message)| message)
            .find(|message| message["role"] == "tool")
            .expect("投影应还原工具结果消息");
        assert_eq!(tool_message["tool_call_id"], "call-1");
        assert!(
            tool_message["content"]
                .as_str()
                .unwrap_or_default()
                .contains("预览"),
            "工具结果消息带模型可见输出：{tool_message}"
        );
    }

    #[test]
    fn oversized_result_goes_to_an_artifact_like_python() {
        let (_root, store, session_id) = store("artifact");
        let long_output = "x".repeat(omnicrawl_session::TOOL_RESULT_INLINE_OUTPUT_CHARS + 100);
        assert_eq!(
            record_results(
                &[observation(call("grep", "call-2"), true, &long_output, &long_output)],
                &store,
                &session_id
            ),
            1
        );

        let events = store.read_events(&session_id).expect("读事件");
        let result = &events
            .iter()
            .find(|event| event.event_type == "tool_result")
            .expect("应当有 tool_result 事件")
            .payload;
        assert_eq!(result["storage"], "artifact", "超长输出落 artifact：{result:?}");
        assert!(result["artifact_path"].is_string(), "应带 artifact 路径");
        assert!(result["output_preview"].is_string(), "应带头部预览");
        assert_eq!(result["output_size_chars"], long_output.chars().count());
        // 模型可见输出不被存储改写：截断是输出预算那一层的事。
        assert_eq!(
            result["model_output"].as_str().map(str::len),
            Some(long_output.len())
        );
    }
}
