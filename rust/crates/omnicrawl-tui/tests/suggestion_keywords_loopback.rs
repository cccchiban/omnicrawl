//! 关键词识别的真实传输回环：本地 `TcpListener` 喂固定 SSE，验证「提问 → 检索词」。
//! 只打 127.0.0.1，不访问公网。

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

use omnicrawl_tui::tools::suggestion::{keyword_client, KeywordChannel};

const KEYWORDS_STREAM: &str = concat!(
    "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\"}}]}\n\n",
    "data: {\"choices\":[{\"delta\":{\"content\":\"{\\\"skills\\\": [\\\"word\\\", \\\"文档\\\"]}\"}}]}\n\n",
    "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
    "data: [DONE]\n\n",
);

#[derive(Clone, Debug)]
struct CapturedRequest {
    path: String,
    authorization: String,
    body: String,
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
                body: String::from_utf8_lossy(&body_bytes).to_string(),
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

fn channel(base_url: &str) -> KeywordChannel {
    KeywordChannel {
        model: "keyword-model".to_string(),
        provider: "openai".to_string(),
        protocol: "openai_chat_completions".to_string(),
        base_url: base_url.to_string(),
        api_key: "test-key".to_string(),
        timeout_seconds: 10.0,
    }
}

#[test]
fn the_prompt_is_recognised_into_skill_keywords() {
    let (base_url, seen) = serve(200, KEYWORDS_STREAM);
    let client = keyword_client(channel(&base_url));
    let keywords = client
        .keywords("帮我把文档里的标题批量替换一下")
        .expect("识别应当成功");

    assert_eq!(keywords.query(), "word 文档");

    let captured = seen.lock().expect("请求锁").clone();
    assert!(
        captured
            .iter()
            .any(|item| item.path.contains("/chat/completions")),
        "{captured:?}"
    );
    assert_eq!(captured[0].authorization, "Bearer test-key");
    assert!(
        captured[0].body.contains("批量替换"),
        "识别请求必须带上用户提问：{}",
        captured[0].body
    );
    // 思考强度固定 low：只做一次词性归纳，别在这一步花掉几秒。
    assert!(
        captured[0].body.contains("\"reasoning_effort\":\"low\""),
        "识别请求必须固定 low 推理强度：{}",
        captured[0].body
    );
}

#[test]
fn a_failed_request_is_reported_so_the_caller_can_fall_back() {
    let (base_url, _seen) = serve(500, "{\"error\":\"boom\"}");
    let client = keyword_client(channel(&base_url));
    let error = client
        .keywords("改一下文档")
        .expect_err("HTTP 500 应当转成错误");
    assert!(error.starts_with("关键词识别请求失败："), "{error}");
}
