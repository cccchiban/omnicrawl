//! 顾问工具的真实传输回环：本地 `TcpListener` 喂固定 SSE，验证单轮补全与错误信封。
//! 只打 127.0.0.1，不访问公网。

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

use serde_json::{json, Map, Value};

use omnicrawl_tui::tools::advisor::{self, AdvisorOptions};

const TEXT_STREAM: &str = concat!(
    "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\"}}]}\n\n",
    "data: {\"choices\":[{\"delta\":{\"content\":\"plan: 继续\"}}]}\n\n",
    "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
    "data: [DONE]\n\n",
);

#[derive(Clone, Debug)]
struct CapturedRequest {
    path: String,
    authorization: String,
}

fn serve(status: u16, body: &'static str) -> (String, Arc<Mutex<Vec<CapturedRequest>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("绑定本地端口");
    let port = listener.local_addr().expect("本地地址").port();
    let seen: Arc<Mutex<Vec<CapturedRequest>>> = Arc::new(Mutex::new(Vec::new()));
    let captured = seen.clone();
    std::thread::spawn(move || {
        for _ in 0..4 {
            let Ok((stream, _)) = listener.accept() else {
                break;
            };
            let mut reader = BufReader::new(stream.try_clone().expect("克隆流"));
            let mut request_line = String::new();
            if reader.read_line(&mut request_line).is_err() {
                continue;
            }
            let path = request_line
                .split_whitespace()
                .nth(1)
                .unwrap_or("/")
                .to_string();
            let mut authorization = String::new();
            let mut length = 0usize;
            loop {
                let mut header = String::new();
                if reader.read_line(&mut header).unwrap_or(0) == 0 {
                    break;
                }
                let line = header.trim_end();
                if line.is_empty() {
                    break;
                }
                if let Some((key, value)) = line.split_once(':') {
                    if key.eq_ignore_ascii_case("authorization") {
                        authorization = value.trim().to_string();
                    }
                    if key.eq_ignore_ascii_case("content-length") {
                        length = value.trim().parse::<usize>().unwrap_or(0);
                    }
                }
            }
            let mut body_bytes = vec![0u8; length];
            if length > 0 {
                let _ = reader.read_exact(&mut body_bytes);
            }
            captured.lock().expect("请求锁").push(CapturedRequest {
                path,
                authorization,
            });

            let payload = if status >= 400 {
                "{\"error\":\"boom\"}"
            } else {
                body
            };
            let content_type = if status >= 400 {
                "application/json"
            } else {
                "text/event-stream"
            };
            let response = format!(
                "HTTP/1.1 {status} X\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                payload.len()
            );
            let mut stream = stream;
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        }
    });
    (format!("http://127.0.0.1:{port}"), seen)
}

fn arguments() -> Map<String, Value> {
    Map::new()
}

fn options(base_url: &str) -> AdvisorOptions {
    AdvisorOptions {
        enabled: true,
        model: "advisor-model".to_string(),
        base_url: base_url.to_string(),
        api_key: "test-key".to_string(),
        effort: "high".to_string(),
        messages: Arc::new(|| {
            vec![
                json!({"role": "user", "content": "改一下 read_image"}),
                json!({"role": "assistant", "content": "正在读取图片"}),
            ]
        }),
        tools: Arc::new(|| vec![("read".to_string(), "读取文件".to_string())]),
        ..AdvisorOptions::default()
    }
}

#[test]
fn advisor_calls_the_model_and_returns_guidance() {
    let (base_url, seen) = serve(200, TEXT_STREAM);
    let guidance = advisor::advisor(&options(&base_url), &arguments()).expect("顾问调用应当成功");
    assert_eq!(guidance, "plan: 继续");

    let captured = seen.lock().expect("请求锁").clone();
    assert!(
        captured
            .iter()
            .any(|item| item.path.contains("/chat/completions")),
        "{captured:?}"
    );
    assert_eq!(captured[0].authorization, "Bearer test-key");
}

#[test]
fn advisor_reports_request_failures() {
    let (base_url, _seen) = serve(500, "{\"error\":\"boom\"}");
    let error =
        advisor::advisor(&options(&base_url), &arguments()).expect_err("HTTP 500 应当转成错误信封");
    assert!(
        error.message.starts_with("顾问请求失败："),
        "{}",
        error.message
    );
}
