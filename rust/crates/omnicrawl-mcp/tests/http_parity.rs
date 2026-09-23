//! Streamable HTTP 客户端的对照测试：本机回环服务端按数据集回放响应，
//! 逐项核对请求形状（路径、请求头、正文）与归一化结果。

mod common;

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use omnicrawl_mcp::config::{McpServerConfig, TextMap, MCP_TRANSPORT_STREAMABLE_HTTP};
use omnicrawl_mcp::http::StreamableHttpMcpConnection;
use omnicrawl_mcp::jsonrpc::McpCallError;
use serde_json::{json, Map, Value};

#[derive(Clone)]
enum Replay {
    Respond {
        status: u16,
        headers: Vec<(String, String)>,
        body: String,
    },
    /// 不响应：让客户端自己超时（对应 Python 假客户端的超时分支）。
    Hang,
    /// 接受连接后立刻关闭：制造非超时的传输错误。
    Close,
}

#[derive(Clone, Debug)]
struct RecordedRequest {
    path: String,
    headers: Vec<(String, String)>,
    body: String,
}

fn spawn_replay(responses: Vec<Replay>) -> (u16, Arc<Mutex<Vec<RecordedRequest>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("回环监听失败");
    let port = listener.local_addr().expect("取本地端口").port();
    let recorded: Arc<Mutex<Vec<RecordedRequest>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&recorded);
    thread::spawn(move || {
        for response in responses {
            let (mut stream, _) = match listener.accept() {
                Ok(pair) => pair,
                Err(_) => return,
            };
            if let Some(request) = read_request(&mut stream) {
                sink.lock().expect("记录锁").push(request);
            }
            match response {
                Replay::Hang => thread::sleep(Duration::from_secs(3)),
                Replay::Close => {}
                Replay::Respond {
                    status,
                    headers,
                    body,
                } => {
                    let mut head = format!(
                        "HTTP/1.1 {status} {}\r\nContent-Length: {}\r\nConnection: close\r\n",
                        reason_of(status),
                        body.len()
                    );
                    for (name, value) in &headers {
                        head.push_str(&format!("{name}: {value}\r\n"));
                    }
                    head.push_str("\r\n");
                    let _ = stream.write_all(head.as_bytes());
                    let _ = stream.write_all(body.as_bytes());
                    let _ = stream.flush();
                }
            }
        }
    });
    (port, recorded)
}

fn reason_of(status: u16) -> &'static str {
    match status {
        200 => "OK",
        202 => "Accepted",
        500 => "Internal Server Error",
        _ => "Status",
    }
}

fn read_request(stream: &mut TcpStream) -> Option<RecordedRequest> {
    let mut buffer: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 1024];
    let header_end = loop {
        let read = stream.read(&mut chunk).ok()?;
        if read == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(position) = find_header_end(&buffer) {
            break position;
        }
    };
    let head = String::from_utf8_lossy(&buffer[..header_end]).to_string();
    let mut lines = head.lines();
    let request_line = lines.next().unwrap_or_default().to_string();
    let path = request_line
        .split_whitespace()
        .nth(1)
        .unwrap_or("/")
        .to_string();
    let mut headers: Vec<(String, String)> = Vec::new();
    let mut content_length = 0usize;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim().to_string();
        if name.trim().eq_ignore_ascii_case("content-length") {
            content_length = value.parse().unwrap_or(0);
        }
        headers.push((name.trim().to_string(), value));
    }
    let mut body = buffer[header_end..].to_vec();
    while body.len() < content_length {
        let read = stream.read(&mut chunk).ok()?;
        if read == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..read]);
    }
    body.truncate(content_length);
    Some(RecordedRequest {
        path,
        headers,
        body: String::from_utf8_lossy(&body).to_string(),
    })
}

fn find_header_end(buffer: &[u8]) -> Option<usize> {
    buffer
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|position| position + 4)
        .or_else(|| {
            buffer
                .windows(2)
                .position(|window| window == b"\n\n")
                .map(|position| position + 2)
        })
}

#[test]
fn http_transport_matches_python() {
    let data = common::fixture();
    let cases = common::cases(&data, "http");
    assert!(cases.len() >= 10, "数据集太小：{}", cases.len());

    for case in cases {
        let name = common::field(&case, "name").as_str().unwrap_or_default();
        let steps = common::field(&case, "steps")
            .as_array()
            .cloned()
            .unwrap_or_default();
        let expected_requests = case
            .get("requests")
            .and_then(|value| value.as_array())
            .cloned()
            .unwrap_or_default();
        let responses: Vec<Replay> = common::field(&case, "responses")
            .as_array()
            .cloned()
            .unwrap_or_default()
            .iter()
            .map(replay_of)
            .collect();

        let (port, recorded) = spawn_replay(responses);
        let mut server = McpServerConfig::new("remote", MCP_TRANSPORT_STREAMABLE_HTTP);
        server.url = Some(format!("http://127.0.0.1:{port}/mcp"));
        server.timeout_seconds = case
            .get("timeout_seconds")
            .and_then(|value| value.as_i64())
            .unwrap_or(30);
        server.headers = record_headers(&case);
        let connection = StreamableHttpMcpConnection::new(&server).expect("构造 HTTP 连接");

        for (index, step) in steps.iter().enumerate() {
            // 驱动参数取自数据集记录的请求体：方法、params 与「是不是通知」都在里面。
            let sent = expected_requests
                .get(index)
                .map(|request| common::field(request, "body").clone())
                .unwrap_or(Value::Null);
            let method = sent
                .get("method")
                .and_then(|value| value.as_str())
                .unwrap_or_default()
                .to_string();
            let params = sent
                .get("params")
                .and_then(|value| value.as_object())
                .cloned()
                .unwrap_or_default();
            let notification = sent.get("id").is_none();
            let actual = match connection.request(&method, &params, notification) {
                Ok(result) => json!({"result": result}),
                Err(error) => json!({
                    "error": error.message(),
                    "error_kind": match error {
                        McpCallError::Timeout(_) => "timeout",
                        _ => "MCPClientError",
                    },
                }),
            };
            compare_step(name, index, &method, &actual, step);
        }

        let recorded = recorded.lock().expect("记录锁").clone();
        assert_eq!(
            recorded.len(),
            expected_requests.len(),
            "用例 {name} 的请求次数不一致"
        );
        for (index, expected) in expected_requests.iter().enumerate() {
            compare_request(name, index, &recorded[index], expected);
        }
    }
}

fn replay_of(value: &Value) -> Replay {
    if value.get("raise_timeout").is_some() {
        return Replay::Hang;
    }
    if value.get("raise_http_error").is_some() {
        return Replay::Close;
    }
    Replay::Respond {
        status: value
            .get("status")
            .and_then(|item| item.as_u64())
            .unwrap_or(200) as u16,
        headers: value
            .get("headers")
            .and_then(|item| item.as_object())
            .map(|items| {
                items
                    .iter()
                    .filter_map(|(name, item)| {
                        item.as_str().map(|text| (name.clone(), text.to_string()))
                    })
                    .collect()
            })
            .unwrap_or_default(),
        body: value
            .get("body")
            .and_then(|item| item.as_str())
            .unwrap_or_default()
            .to_string(),
    }
}

fn record_headers(case: &Value) -> TextMap {
    let mut map: BTreeMap<String, String> = BTreeMap::new();
    if let Some(headers) = case.get("headers").and_then(|value| value.as_object()) {
        for (name, value) in headers {
            if let Some(text) = value.as_str() {
                map.insert(name.clone(), text.to_string());
            }
        }
    }
    map
}

fn compare_step(name: &str, index: usize, method: &str, actual: &Value, expected: &Value) {
    if let Some(result) = expected.get("result") {
        assert_eq!(
            actual.get("result"),
            Some(result),
            "用例 {name} 第 {index} 步 {method} 的结果不一致"
        );
        return;
    }
    let expected_error = expected
        .get("error")
        .and_then(|value| value.as_str())
        .unwrap_or_default();
    let actual_error = actual
        .get("error")
        .and_then(|value| value.as_str())
        .unwrap_or_default();
    if name == "transport_error" {
        // 两侧的底层连接错误文案不同：只核对前缀（见 crate README 的已知差异）。
        assert!(
            actual_error.starts_with("MCP Server HTTP 请求失败："),
            "用例 {name} 的错误文案不符：{actual_error}"
        );
        return;
    }
    assert_eq!(
        actual_error, expected_error,
        "用例 {name} 第 {index} 步 {method} 的错误文案不一致"
    );
    assert_eq!(
        actual.get("error_kind"),
        expected.get("error_kind"),
        "用例 {name} 第 {index} 步 {method} 的错误类型不一致"
    );
}

fn compare_request(name: &str, index: usize, actual: &RecordedRequest, expected: &Value) {
    let url = common::field(expected, "url").as_str().unwrap_or_default();
    let path = url
        .split("://")
        .nth(1)
        .and_then(|rest| rest.split_once('/'))
        .map(|(_, tail)| format!("/{tail}"))
        .unwrap_or_else(|| "/".to_string());
    assert_eq!(
        actual.path, path,
        "用例 {name} 第 {index} 个请求的路径不一致"
    );

    let actual_body: Value = serde_json::from_str(&actual.body)
        .unwrap_or_else(|error| panic!("用例 {name} 第 {index} 个请求体不是 JSON：{error}"));
    let expected_body = common::field(expected, "body");
    let expected_body = if expected_body.is_null() {
        Map::new()
    } else {
        expected_body.as_object().cloned().unwrap_or_default()
    };
    assert_eq!(
        actual_body,
        Value::Object(expected_body.clone()),
        "用例 {name} 第 {index} 个请求体不一致"
    );

    let mut actual_headers: BTreeMap<String, String> = BTreeMap::new();
    for (header, value) in &actual.headers {
        let key = header.to_lowercase();
        assert!(
            actual_headers.insert(key.clone(), value.clone()).is_none(),
            "用例 {name} 第 {index} 个请求出现重复请求头：{key}"
        );
    }
    for (header, value) in common::field(expected, "headers")
        .as_object()
        .cloned()
        .unwrap_or_default()
    {
        let expected_value = value.as_str().map(|text| text.to_string());
        assert_eq!(
            actual_headers.get(&header).cloned(),
            expected_value,
            "用例 {name} 第 {index} 个请求的请求头 {header} 不一致"
        );
    }
}
