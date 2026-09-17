//! HTTP 传输：内核自己向 Provider 发请求，并把响应体交给流解析。
//!
//! 只负责「发一次、给回可增量读取的响应体」：重试、错误分类与取消由 runtime 决定。
//! 用阻塞式 HTTP 客户端，不引入异步运行时——内核的回合循环本来就是阻塞的。
//!
//! 超时按 Python 侧的语义拆开：建连、等响应头、以及**每次**读取响应体的空闲超时
//! （不是整个响应体的总时限，否则长回复会被拦腰截断）。

use std::io::Read;
use std::time::Duration;

use ureq::http::Response;
use ureq::Body;

/// 一次请求：URL、凭据、请求体与超时。
pub struct HttpRequest<'a> {
    pub url: &'a str,
    pub api_key: &'a str,
    pub user_agent: &'a str,
    pub body: &'a str,
    pub timeout_seconds: f64,
}

/// 一次响应：状态码 + 可增量读取的响应体。
pub struct HttpResponse {
    pub status: u16,
    pub body: Box<dyn Read + Send>,
}

impl HttpResponse {
    /// 读完整响应体（错误响应才有必要）；读取失败按空串处理。
    ///
    /// 只借用不消费：调用方读完正文后仍要按状态码决定是报错还是重发。
    pub fn read_text(&mut self) -> String {
        let mut text = String::new();
        let _ = self.body.read_to_string(&mut text);
        text
    }
}

pub struct TransportError {
    pub message: String,
}

/// 复用同一份连接池与 TLS 配置；状态码不转错误，好让 runtime 读到错误正文。
pub fn build_agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .build()
        .into()
}

pub fn send(
    agent: &ureq::Agent,
    request: &HttpRequest<'_>,
) -> Result<HttpResponse, TransportError> {
    let timeout = Duration::from_secs_f64(request.timeout_seconds.max(1.0));
    let mut builder = agent
        .post(request.url)
        .config()
        .timeout_connect(Some(timeout))
        .timeout_recv_response(Some(timeout))
        .timeout_recv_body(Some(timeout))
        .build()
        .header("Authorization", format!("Bearer {}", request.api_key))
        .header("Content-Type", "application/json")
        .header("Accept", "text/event-stream");
    if !request.user_agent.trim().is_empty() {
        builder = builder.header("User-Agent", request.user_agent);
    }

    match builder.send(request.body) {
        Ok(response) => Ok(split(response)),
        Err(error) => Err(TransportError {
            message: describe(&error, timeout),
        }),
    }
}

fn split(response: Response<Body>) -> HttpResponse {
    let status = response.status().as_u16();
    HttpResponse {
        status,
        body: Box::new(response.into_body().into_reader()),
    }
}

fn describe(error: &ureq::Error, timeout: Duration) -> String {
    match error {
        ureq::Error::Timeout(_) => format!("请求超时（{} 秒内无响应）。", timeout.as_secs()),
        ureq::Error::HostNotFound => "域名解析失败。".to_string(),
        ureq::Error::ConnectionFailed => "无法建立连接。".to_string(),
        ureq::Error::Io(error) => format!("连接中断：{error}"),
        other => format!("传输失败：{other}"),
    }
}
