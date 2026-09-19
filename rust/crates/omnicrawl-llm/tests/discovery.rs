//! 模型列表发现的本机回环验收：线上路径与鉴权头、解析与截断、失败降级文案。
//!
//! Python 侧这一步走 SDK 的 `models.list()`，内核按实测的线上形态直接发 GET，
//! 因此这里钉住的是「内核发出去的请求」与「解析结果的形状」。

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::thread;

use omnicrawl_llm::{discover_models, DiscoveryStatus, ProviderProfile};
use omnicrawl_protocol::Protocol;

struct StubReply {
    status: u16,
    body: String,
}

struct Captured {
    request_line: String,
    headers: Vec<(String, String)>,
}

impl Captured {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

fn spawn_get_server(replies: Vec<StubReply>) -> (String, Arc<Mutex<Vec<Captured>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("无法监听回环端口");
    let addr = listener.local_addr().expect("无法取本地地址");
    let captured: Arc<Mutex<Vec<Captured>>> = Arc::new(Mutex::new(Vec::new()));
    let store = Arc::clone(&captured);

    thread::spawn(move || {
        for reply in replies {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let mut buffer: Vec<u8> = Vec::new();
            let mut chunk = [0u8; 4096];
            let head_end = loop {
                let read = stream.read(&mut chunk).unwrap_or(0);
                if read == 0 {
                    return;
                }
                buffer.extend_from_slice(&chunk[..read]);
                if let Some(position) = buffer.windows(4).position(|item| item == b"\r\n\r\n") {
                    break position + 4;
                }
            };
            let head = String::from_utf8_lossy(&buffer[..head_end]).to_string();
            let mut lines = head.split("\r\n");
            let request_line = lines.next().unwrap_or_default().to_string();
            let mut headers = Vec::new();
            for line in lines {
                if let Some((name, value)) = line.split_once(':') {
                    headers.push((name.trim().to_string(), value.trim().to_string()));
                }
            }
            store.lock().expect("记录锁").push(Captured {
                request_line,
                headers,
            });

            let response = format!(
                "HTTP/1.1 {status} STAT\r\nContent-Type: application/json\r\n\
                 Content-Length: {length}\r\nConnection: close\r\n\r\n{body}",
                status = reply.status,
                length = reply.body.len(),
                body = reply.body,
            );
            let _ = stream.write_all(response.as_bytes());
        }
    });

    (format!("http://{addr}"), captured)
}

fn profile(provider: &str, base_url: String) -> ProviderProfile {
    ProviderProfile {
        id: "p1".to_string(),
        provider: provider.to_string(),
        base_url,
        api_key: "secret".to_string(),
        ..ProviderProfile::default()
    }
}

#[test]
fn openai_protocols_use_models_endpoint() {
    let body = r#"{"object":"list","data":[{"id":"gpt-5"},{"id":"gpt-5-mini"}]}"#;
    let (base_url, captured) = spawn_get_server(vec![
        StubReply {
            status: 200,
            body: body.to_string(),
        },
        StubReply {
            status: 200,
            body: body.to_string(),
        },
    ]);

    for protocol in [Protocol::OpenaiChatCompletions, Protocol::OpenaiResponses] {
        let result = discover_models(&profile("openai", base_url.clone()), protocol, 5.0);
        assert_eq!(result.status, DiscoveryStatus::Ok);
        assert_eq!(
            result
                .models
                .iter()
                .map(|model| model.model_id.as_str())
                .collect::<Vec<_>>(),
            vec!["gpt-5", "gpt-5-mini"]
        );
        assert_eq!(result.models[0].protocol, protocol);
    }

    let requests = captured.lock().expect("记录锁");
    assert_eq!(requests.len(), 2);
    for request in requests.iter() {
        assert_eq!(request.request_line, "GET /models HTTP/1.1");
        assert_eq!(request.header("authorization"), Some("Bearer secret"));
    }
}

#[test]
fn anthropic_uses_v1_models_with_its_headers() {
    let body = r#"{"data":[{"id":"claude-sonnet-4-5","display_name":"Claude Sonnet 4.5"}],"has_more":false}"#;
    let (base_url, captured) = spawn_get_server(vec![StubReply {
        status: 200,
        body: body.to_string(),
    }]);

    let result = discover_models(
        &profile("anthropic", base_url),
        Protocol::AnthropicMessages,
        5.0,
    );
    assert_eq!(result.status, DiscoveryStatus::Ok);
    // Python 侧 display_name 也取 model_id（不读响应里的 display_name）。
    assert_eq!(result.models.len(), 1);
    assert_eq!(result.models[0].model_id, "claude-sonnet-4-5");
    assert_eq!(result.models[0].display_name, "claude-sonnet-4-5");

    let requests = captured.lock().expect("记录锁");
    assert_eq!(requests[0].request_line, "GET /v1/models HTTP/1.1");
    assert_eq!(requests[0].header("x-api-key"), Some("secret"));
    assert_eq!(requests[0].header("anthropic-version"), Some("2023-06-01"));
}

#[test]
fn gemini_strips_models_prefix_and_dedupes() {
    let body = r#"{"models":[{"name":"models/gemini-2.5-flash"},{"name":"models/gemini-2.5-pro"},{"name":"models/gemini-2.5-flash"}]}"#;
    let (base_url, captured) = spawn_get_server(vec![StubReply {
        status: 200,
        body: body.to_string(),
    }]);

    let result = discover_models(
        &profile("gemini", base_url),
        Protocol::GeminiGenerateContent,
        5.0,
    );
    assert_eq!(result.status, DiscoveryStatus::Ok);
    assert_eq!(
        result
            .models
            .iter()
            .map(|model| model.model_id.as_str())
            .collect::<Vec<_>>(),
        vec!["gemini-2.5-flash", "gemini-2.5-pro"],
        "去掉 models/ 前缀并按 model_id 去重"
    );

    let requests = captured.lock().expect("记录锁");
    assert_eq!(requests[0].request_line, "GET /v1beta/models HTTP/1.1");
    assert_eq!(requests[0].header("x-goog-api-key"), Some("secret"));
}

#[test]
fn discovery_truncates_at_five_hundred_items() {
    let items: Vec<String> = (0..501)
        .map(|index| format!("{{\"id\":\"model-{index}\"}}"))
        .collect();
    let body = format!("{{\"data\":[{}]}}", items.join(","));
    let (base_url, _captured) = spawn_get_server(vec![StubReply { status: 200, body }]);

    let result = discover_models(
        &profile("openai", base_url),
        Protocol::OpenaiChatCompletions,
        5.0,
    );
    assert_eq!(result.models.len(), 500, "超过 500 条按 Python 口径截断");
}

#[test]
fn missing_api_key_is_unavailable() {
    let mut target = profile("gemini", "http://127.0.0.1:1".to_string());
    target.api_key = "   ".to_string();
    let result = discover_models(&target, Protocol::GeminiGenerateContent, 1.0);

    assert_eq!(result.status, DiscoveryStatus::Unavailable);
    assert_eq!(result.message, "缺少 API Key，无法发现模型。");
    assert!(result.models.is_empty());
}

#[test]
fn http_failure_is_unavailable_with_provider_text() {
    for (provider, protocol, marker) in [
        (
            "openai",
            Protocol::OpenaiChatCompletions,
            "模型列表发现失败：",
        ),
        (
            "anthropic",
            Protocol::AnthropicMessages,
            "Claude 模型列表发现失败：",
        ),
        (
            "gemini",
            Protocol::GeminiGenerateContent,
            "Gemini 模型列表发现失败：",
        ),
    ] {
        let (base_url, _captured) = spawn_get_server(vec![StubReply {
            status: 404,
            body: "{\"error\":{\"message\":\"404 not found\"}}".to_string(),
        }]);
        let result = discover_models(&profile(provider, base_url), protocol, 5.0);
        assert_eq!(
            result.status,
            DiscoveryStatus::Unavailable,
            "Provider {provider}"
        );
        assert!(
            result.message.starts_with(marker),
            "Provider {provider} 的文案：{}",
            result.message
        );
    }
}
