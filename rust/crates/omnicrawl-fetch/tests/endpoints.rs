//! 抓取接口的端到端测试：真实回环上跑一遍信封、端点与错误码。
//!
//! 目标站点是测试内的假服务（`127.0.0.1`）：内网地址直连、不过系统代理，
//! 因此这些用例不触网也能验证请求形状、正文提取与响应信封。

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;

use omnicrawl_config::core::runtime::ConfigEnvironment;
use omnicrawl_config::features::fetch_api::FetchApiConfig;
use omnicrawl_fetch::{build_router, AppState, FetchSettings};
use omnicrawl_host::tools::FetcherOptions;
use serde_json::{json, Value};

/// 起一个假站点，回一份带标题与正文的 HTML。
fn spawn_site(body: &'static str) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").expect("绑定站点端口");
    let address = listener.local_addr().expect("站点地址");
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            serve_site(stream, body);
        }
    });
    address
}

fn serve_site(mut stream: TcpStream, body: &str) {
    let mut reader = BufReader::new(stream.try_clone().expect("克隆流"));
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).unwrap_or(0) == 0 {
        return;
    }
    // robots.txt：站点明确没有限制文件，按允许处理。
    if request_line.contains("robots.txt") {
        let response = "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
        let _ = stream.write_all(response.as_bytes());
        let _ = stream.flush();
        return;
    }
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header).unwrap_or(0) == 0 {
            break;
        }
        if header.trim_end().is_empty() {
            break;
        }
    }
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}

/// 起抓取接口本身；返回监听地址。
fn spawn_api(max_urls: i64, impersonate: &str) -> SocketAddr {
    let settings = FetchSettings {
        api: FetchApiConfig {
            enabled: true,
            host: "127.0.0.1".to_string(),
            port: 0,
            impersonate: impersonate.to_string(),
            max_urls,
        },
        fetcher: Arc::new(FetcherOptions {
            // 显式禁用代理：测试不读系统代理。
            proxy: Some(String::new()),
            ..FetcherOptions::default()
        }),
    };
    let environment = ConfigEnvironment::new("C:\\oc-fetch-test", "win32");
    let state = AppState::new(settings, &environment);
    let listener = TcpListener::bind("127.0.0.1:0").expect("绑定接口端口");
    listener.set_nonblocking(true).expect("设为非阻塞");
    let address = listener.local_addr().expect("接口地址");
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("构建 tokio 运行时");
        runtime.block_on(async move {
            let listener = tokio::net::TcpListener::from_std(listener).expect("接管监听");
            let _ = axum::serve(listener, build_router(state)).await;
        });
    });
    address
}

struct Reply {
    status: u16,
    body: String,
}

impl Reply {
    fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or_else(|_| json!({"raw": self.body}))
    }
}

fn post(address: SocketAddr, path: &str, body: &Value) -> Reply {
    request(address, "POST", path, Some(body))
}

fn get(address: SocketAddr, path: &str) -> Reply {
    request(address, "GET", path, None)
}

fn request(address: SocketAddr, method: &str, path: &str, body: Option<&Value>) -> Reply {
    let mut stream = TcpStream::connect(address).expect("连接接口");
    let body_text = body.map(Value::to_string).unwrap_or_default();
    let mut head = format!("{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n");
    if !body_text.is_empty() {
        head.push_str("Content-Type: application/json\r\n");
        head.push_str(&format!("Content-Length: {}\r\n", body_text.len()));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes()).expect("写请求头");
    if !body_text.is_empty() {
        stream.write_all(body_text.as_bytes()).expect("写请求体");
    }
    let mut reply = String::new();
    stream.read_to_string(&mut reply).expect("读响应");
    let (head, body) = reply.split_once("\r\n\r\n").unwrap_or((reply.as_str(), ""));
    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse().ok())
        .unwrap_or(0);
    Reply {
        status,
        body: body.to_string(),
    }
}

const PAGE: &str = "<html><head><title>示例页</title></head><body><main><p>正文内容</p></main></body></html>";

#[test]
fn endpoints_are_callable_without_credentials() {
    let address = spawn_api(0, "");
    let health = get(address, "/health");
    assert_eq!(health.status, 200, "{}", health.body);
    assert_eq!(health.json()["service"], "omnicrawl-fetch");

    // 未知路径仍是 404（不是被鉴权层挡下的 401）。
    let unknown = get(address, "/v1/nope");
    assert_eq!(unknown.status, 404, "{}", unknown.body);
    assert_eq!(unknown.json()["error"]["code"], "NOT_FOUND");
}

#[test]
fn fetch_returns_structured_results_for_loopback_pages() {
    let site = spawn_site(PAGE);
    let address = spawn_api(0, "");
    let url = format!("http://{site}/page");

    let reply = post(address, "/v1/fetch", &json!({"urls": [url]}));
    assert_eq!(reply.status, 200, "{}", reply.body);
    let data = &reply.json()["data"];
    assert_eq!(data["count"], 1);
    let result = &data["results"][0];
    assert_eq!(result["status"], 200);
    assert_eq!(result["title"], "示例页");
    assert!(
        result["content"]
            .as_str()
            .unwrap_or_default()
            .contains("正文内容"),
        "{}",
        result["content"]
    );
    assert_eq!(result["error"], Value::Null);
}

#[test]
fn a_single_url_may_be_passed_as_url_instead_of_urls() {
    let site = spawn_site(PAGE);
    let address = spawn_api(0, "");
    let reply = post(address, "/v1/fetch", &json!({"url": format!("http://{site}/x")}));
    assert_eq!(reply.status, 200, "{}", reply.body);
    assert_eq!(reply.json()["data"]["count"], 1);
}

#[test]
fn max_html_returns_the_raw_document() {
    let site = spawn_site(PAGE);
    let address = spawn_api(0, "");
    let reply = post(
        address,
        "/v1/fetch",
        &json!({"urls": [format!("http://{site}/raw")], "max_html": true}),
    );
    assert_eq!(reply.status, 200, "{}", reply.body);
    let content = reply.json()["data"]["results"][0]["content"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(content.contains("<main>"), "{content}");
}

#[test]
fn the_url_limit_truncates_the_request_list() {
    let site = spawn_site(PAGE);
    let address = spawn_api(1, "");
    let reply = post(
        address,
        "/v1/fetch",
        &json!({"urls": [
            format!("http://{site}/a"),
            format!("http://{site}/b"),
            format!("http://{site}/c"),
        ]}),
    );
    assert_eq!(reply.status, 200, "{}", reply.body);
    assert_eq!(reply.json()["data"]["count"], 1);
}

#[test]
fn missing_urls_is_rejected_with_a_readable_code() {
    let address = spawn_api(0, "");
    let reply = post(address, "/v1/fetch", &json!({"max_chars": 500}));
    assert_eq!(reply.status, 400, "{}", reply.body);
    assert_eq!(reply.json()["error"]["code"], "INVALID_REQUEST");
    assert!(reply.json()["error"]["message"]
        .as_str()
        .unwrap_or_default()
        .contains("urls"));

    // 非对象请求体同样按 400 报错，而不是 500。
    let reply = post(address, "/v1/fetch", &json!([1, 2, 3]));
    assert_eq!(reply.status, 400, "{}", reply.body);
}

#[test]
fn status_reports_listen_and_limits() {
    let address = spawn_api(5, "firefox");
    let reply = get(address, "/v1/status");
    assert_eq!(reply.status, 200, "{}", reply.body);
    let data = &reply.json()["data"];
    assert_eq!(data["ready"], true);
    assert_eq!(data["max_urls"], 5);
    assert_eq!(data["impersonate"], "firefox");
    assert_eq!(data["unavailable_reason"], Value::Null);
}
