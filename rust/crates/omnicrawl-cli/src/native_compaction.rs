use std::io::Read;
use std::rc::Rc;

use omnicrawl_compaction::AfterTurnReport;
use omnicrawl_controllers::context_compaction::{
    ContextCompactionService, MeasureInput, TokenUsageSample,
};
use omnicrawl_controllers::turn::compaction::format_compaction_savings_option;
use omnicrawl_ipc::bridge::KernelModelConfig;
use omnicrawl_llm::responses::messages_to_responses_input;
use omnicrawl_llm::transport::{HttpRequest, send, shared_agent};
use omnicrawl_protocol::{conform_tool_names, conversation_from_openai_messages};
use omnicrawl_session::utc_now;
use serde_json::{Value, json};

use crate::compaction::KernelSession;

pub(crate) fn enabled(session: &KernelSession, model: &KernelModelConfig) -> bool {
    session.config.native_gpt
        && model.protocol == "openai_responses"
        && omnicrawl_llm::is_openai_gpt_model(model.model.rsplit('/').next().unwrap_or_default())
        && omnicrawl_config::features::desensitization::load_desensitization_config(
            &omnicrawl_config::core::runtime::ConfigEnvironment::from_process(),
            None,
        )
        .is_ok_and(|config| !config.enabled)
}

pub(crate) fn after_turn(
    session: &KernelSession,
    model: &KernelModelConfig,
    api_key: &str,
    _history: &[Value],
    usage: TokenUsageSample,
    provider_input_tokens: i64,
    progress: Option<Rc<dyn Fn()>>,
) -> Result<AfterTurnReport, String> {
    let events = session
        .store
        .read_active_events(&session.session_id)
        .map_err(|error| error.message().to_string())?;
    let history = omnicrawl_session::project_session_history(&events)
        .into_iter()
        .map(|(_, message)| message)
        .collect::<Vec<_>>();
    let config = &session.config;
    let measurement = ContextCompactionService::new()
        .measure_after_complete_turn(MeasureInput {
            system_prompt: model.system_prompt.clone(),
            context_messages: model.context_messages.clone(),
            history_messages: history.to_vec(),
            tool_schemas: model.tools.clone(),
            recent_turns: config.recent_turns,
            target_summary_tokens: config.target_summary_tokens,
            next_user_reserve_tokens: config.next_user_reserve_tokens,
            trigger_context_tokens: config.trigger_context_tokens,
            context_window_tokens: config.context_window_tokens,
            usage,
            provider_input_tokens,
            emergency_context_ratio: config.emergency_context_ratio,
            nodes: Vec::new(),
        })
        .map_err(|error| error.message().to_string())?;
    let mut report =
        if measurement.snapshot.trigger_reached || measurement.snapshot.emergency_ratio_reached {
            compact(session, model, api_key, &history, progress)?
        } else {
            AfterTurnReport::default()
        };
    report.measurement_payload = measurement.event_payload;
    session.append(
        "context_compaction_measurement",
        report.measurement_payload.clone(),
    )?;
    Ok(report)
}

pub(crate) fn compact(
    session: &KernelSession,
    model: &KernelModelConfig,
    api_key: &str,
    history: &[Value],
    progress: Option<Rc<dyn Fn()>>,
) -> Result<AfterTurnReport, String> {
    if history.is_empty() {
        return Ok(AfterTurnReport::default());
    }
    let history = history
        .strip_prefix(model.context_messages.as_slice())
        .unwrap_or(history);
    let events = session
        .store
        .read_active_events(&session.session_id)
        .map_err(|error| error.message().to_string())?;
    let preserved = events
        .iter()
        .rev()
        .find(|event| event.event_type == "assistant_message")
        .and_then(|event| {
            history
                .last()
                .filter(|message| {
                    message.get("role").and_then(Value::as_str) == Some("assistant")
                        && message.get("tool_calls").is_none()
                        && message
                            .get("content")
                            .and_then(Value::as_str)
                            .is_some_and(|text| !text.is_empty())
                        && message.get("content") == event.payload.get("content")
                        && history.len() > 1
                })
                .map(|message| (event.event_id.clone(), message.clone()))
        });
    let compacted = if preserved.is_some() {
        &history[..history.len() - 1]
    } else {
        history
    };
    let mut messages = compacted.to_vec();
    let tools = model
        .tools
        .iter()
        .filter_map(omnicrawl_protocol::tool_spec_from_openai_item)
        .collect::<Vec<_>>();
    let (_, names) = conform_tool_names(&tools);
    names.restore_wire_names(&mut messages);
    let messages = conversation_from_openai_messages(&messages);
    let body = json!({
        "model": model.model,
        "instructions": model.system_prompt,
        "input": messages_to_responses_input(&messages),
    })
    .to_string();
    let base = if model.base_url.trim().is_empty() {
        "https://api.openai.com/v1"
    } else {
        model.base_url.trim_end_matches('/')
    };
    let url = format!("{base}/responses/compact");
    let authorization = format!("Bearer {api_key}");
    let accept = "application/json";
    if let Some(progress) = progress {
        progress();
    }
    let mut response = send(
        shared_agent(),
        &HttpRequest {
            url: &url,
            user_agent: &model.user_agent,
            body: &body,
            timeout_seconds: model.request_timeout_seconds.unwrap_or(120.0),
            headers: &[
                ("Authorization", &authorization),
                ("Accept", accept),
            ],
        },
    )
    .map_err(|error| format!("原生上下文压缩请求失败：{}", error.sdk_view().0))?;
    let mut text = String::new();
    response
        .body
        .by_ref()
        .take(32 * 1024 * 1024 + 1)
        .read_to_string(&mut text)
        .map_err(|_| "原生上下文压缩响应读取失败。".to_string())?;
    if text.len() > 32 * 1024 * 1024 {
        return Err("原生上下文压缩响应过大。".to_string());
    }
    if !(200..300).contains(&response.status) {
        return Err(format!(
            "原生上下文压缩失败：HTTP {}。可在设置中切回摘要压缩。",
            response.status
        ));
    }
    let value: Value =
        serde_json::from_str(&text).map_err(|_| "原生上下文压缩未返回合法 JSON。".to_string())?;
    let output = value
        .get("output")
        .and_then(Value::as_array)
        .filter(|items| {
            items.iter().any(|item| {
                item.get("type").and_then(Value::as_str) == Some("compaction")
                    && item
                        .get("encrypted_content")
                        .and_then(Value::as_str)
                        .is_some_and(|text| !text.is_empty())
            })
        })
        .ok_or_else(|| "原生上下文压缩未返回加密上下文。".to_string())?;
    let output_tokens = value
        .pointer("/usage/output_tokens")
        .and_then(Value::as_i64)
        .filter(|tokens| *tokens > 0)
        .ok_or_else(|| "原生上下文压缩缺少输出 Token 计量。".to_string())?;
    let remaining_ids: Vec<String> = preserved
        .as_ref()
        .map(|(id, _)| id.clone())
        .into_iter()
        .collect();
    let mut payload = json!({
        "mode": "openai_native", "content": "OpenAI 原生上下文压缩",
        "responses_compaction": output,
        "responses_compaction_model": model.model,
        "responses_compaction_base_url": model.base_url,
        "responses_compaction_tokens": output_tokens,
        "source_event_ids": events.iter().map(|event| &event.event_id).collect::<Vec<_>>(),
        "remaining_event_ids": remaining_ids, "remaining_message_count": remaining_ids.len(),
    });
    if session.config.archive_compacted_events {
        let archived = events
            .iter()
            .map(|event| event.to_dict())
            .collect::<Vec<_>>();
        let archive_id = session
            .store
            .archive_compacted_events(&session.session_id, &archived, None, utc_now())
            .map_err(|error| error.message().to_string())?;
        payload["archive_id"] = json!(archive_id);
    }
    session.append("compact_summary", payload)?;
    let saved = value
        .pointer("/usage/input_tokens")
        .and_then(Value::as_i64)
        .unwrap_or(0)
        - value
            .pointer("/usage/output_tokens")
            .and_then(Value::as_i64)
            .unwrap_or(0);
    let mut history = vec![
        json!({"role": "assistant", "content": "", "responses_compaction": output,
        "responses_compaction_model": model.model, "responses_compaction_base_url": model.base_url,
        "responses_compaction_tokens": output_tokens}),
    ];
    if let Some((_, message)) = preserved {
        history.push(message);
    }
    Ok(AfterTurnReport {
        compacted: true,
        history: Some(history),
        notices: format_compaction_savings_option(saved.max(0))
            .into_iter()
            .collect(),
        ..AfterTurnReport::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use omnicrawl_ipc::bridge::KernelSessionConfig;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::sync::mpsc;
    use std::thread;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    fn model(base_url: &str) -> KernelModelConfig {
        serde_json::from_value(json!({
            "model": "gpt-5", "protocol": "openai_responses", "base_url": base_url,
            "system_prompt": "instructions", "request_timeout_seconds": 2.0,
        }))
        .unwrap()
    }

    fn session() -> KernelSession {
        let root = std::env::temp_dir().join(format!(
            "omnicrawl-native-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let mut session = KernelSession::open(KernelSessionConfig {
            root: root.to_string_lossy().into_owned(),
            session_id: String::new(),
            memory_root: None,
            workspace_root: None,
            compaction: None,
        })
        .unwrap();
        session
            .append("user_message", json!({"content": "remember this"}))
            .unwrap();
        session
            .append("assistant_message", json!({"content": "remembered"}))
            .unwrap();
        session.reload_history().unwrap();
        session
    }

    fn server(
        status: u16,
        body: Value,
    ) -> (
        String,
        mpsc::Receiver<(String, Value)>,
        thread::JoinHandle<()>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (tx, rx) = mpsc::channel();
        let handle = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut reader = BufReader::new(stream);
            let mut request = String::new();
            reader.read_line(&mut request).unwrap();
            let mut length = 0;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" {
                    break;
                }
                if let Some((key, value)) = line.split_once(':') {
                    if key.eq_ignore_ascii_case("content-length") {
                        length = value.trim().parse().unwrap();
                    }
                }
            }
            let mut bytes = vec![0; length];
            reader.read_exact(&mut bytes).unwrap();
            tx.send((request, serde_json::from_slice(&bytes).unwrap()))
                .unwrap();
            let body = body.to_string();
            write!(reader.get_mut(), "HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        });
        (format!("http://{address}/v1"), rx, handle)
    }

    #[test]
    fn native_compaction_is_persisted_and_can_be_replayed() {
        let output =
            json!([{ "type": "compaction", "id": "cmp_test", "encrypted_content": "opaque" }]);
        let (url, rx, handle) = server(
            200,
            json!({"output": output, "usage": {"input_tokens": 200, "output_tokens": 20}}),
        );
        let mut session = session();
        let report = compact(&session, &model(&url), "test", &session.history, None).unwrap();
        let (path, request) = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(path.starts_with("POST /v1/responses/compact "));
        assert_eq!(request["instructions"], "instructions");
        assert_eq!(request["input"][0]["role"], "user");
        assert!(request.get("tools").is_none());
        assert!(report.compacted);
        session.reload_history().unwrap();
        assert_eq!(session.history, report.history.unwrap());
        assert_eq!(session.history[0]["responses_compaction"], output);
        let converted = conversation_from_openai_messages(&session.history[..1]);
        assert_eq!(
            messages_to_responses_input(&converted),
            output.as_array().unwrap().clone()
        );
        let mut switched = model(&url);
        switched.protocol = "openai_chat".to_string();
        session.prepare_model_history(&switched).unwrap();
        assert!(
            !session
                .history
                .iter()
                .any(|message| message.get("responses_compaction").is_some())
        );
        assert!(
            session
                .history
                .iter()
                .any(|message| message["content"] == "remember this")
        );
        handle.join().unwrap();
        std::fs::remove_dir_all(session.store.root()).unwrap();
    }

    #[test]
    fn failed_native_compaction_does_not_replace_the_history() {
        let (url, _rx, handle) = server(404, json!({"error": "unsupported"}));
        let session = session();
        let count = session
            .store
            .read_active_events(&session.session_id)
            .unwrap()
            .len();
        let error = compact(&session, &model(&url), "test", &session.history, None).unwrap_err();
        assert!(error.contains("HTTP 404"));
        assert_eq!(
            session
                .store
                .read_active_events(&session.session_id)
                .unwrap()
                .len(),
            count
        );
        handle.join().unwrap();
        std::fs::remove_dir_all(session.store.root()).unwrap();
    }
}
