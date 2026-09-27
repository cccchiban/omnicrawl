//! HTTP 传输：内核自己向 Provider 发请求，并把响应体交给流解析。
//!
//! 只负责「发一次、给回可增量读取的响应体」：重试、错误分类与取消由 runtime 决定。
//! 用阻塞式 HTTP 客户端，不引入异步运行时——内核的回合循环本来就是阻塞的。
//!
//! 超时按 Python 侧的语义拆开：建连、等响应头、以及**每次**读取响应体的空闲超时
//! （不是整个响应体的总时限，否则长回复会被拦腰截断）。

use std::io::Read;
use std::sync::OnceLock;
use std::time::Duration;

use ureq::http::Response;
use ureq::Body;

/// 一次请求：URL、凭据与协议头、请求体与超时。
pub struct HttpRequest<'a> {
    pub url: &'a str,
    pub user_agent: &'a str,
    pub body: &'a str,
    pub timeout_seconds: f64,
    /// 凭据与协议头：OpenAI 兼容侧给 `Authorization: Bearer …`，Anthropic 给 `x-api-key`。
    pub headers: &'a [(&'a str, &'a str)],
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

/// 传输层失败的类型（对应的 SDK 异常与文案由 [`TransportFailure::sdk_view`] 给出）。
pub enum TransportFailure {
    /// 超时。
    Timeout,
    /// 域名解析或建连失败。
    Connection,
    /// 读取期中断：带上底层文本，交给同一张分类阶梯。
    Io(String),
    /// 其他传输失败（含 TLS）：带上原始文本。
    Other(String),
}

impl TransportFailure {
    /// SDK 等价文案与类型名：内核只提供类型，分支判定仍走 Python 那张关键词阶梯，
    /// 免得两处各写一套「什么算超时、什么算连接失败」。
    pub fn sdk_view(&self) -> (&str, &str) {
        match self {
            Self::Timeout => ("Request timed out.", "APITimeoutError"),
            Self::Connection => ("Connection error.", "APIConnectionError"),
            Self::Io(text) | Self::Other(text) => (text.as_str(), "APIConnectionError"),
        }
    }
}

/// 复用同一份连接池与 TLS 配置；状态码不转错误，好让 runtime 读到错误正文。
pub fn build_agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .build()
        .into()
}

/// 进程级共享 agent：连接池与 TLS 会话跨回合、跨运行时复用。
///
/// 每个运行时各持一个 agent 时，每次重建运行时（回合边界、渠道切换）都会丢掉连接池，
/// 于是每次模型请求都要重做 TCP 握手与 TLS 握手——这段开销全部落在「回车 → 首字」的
/// 区间里。连接池按 `scheme://authority` 分键，不同 Base URL 之间不会串连接；
/// 池里的连接取出前会探测存活，服务端已关闭的连接不会被复用。
pub fn shared_agent() -> &'static ureq::Agent {
    static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
    AGENT.get_or_init(build_agent)
}

pub fn send(
    agent: &ureq::Agent,
    request: &HttpRequest<'_>,
) -> Result<HttpResponse, TransportFailure> {
    let timeout = Duration::from_secs_f64(request.timeout_seconds.max(1.0));
    let mut builder = agent
        .post(request.url)
        .config()
        .timeout_connect(Some(timeout))
        .timeout_recv_response(Some(timeout))
        .timeout_recv_body(Some(timeout))
        .build()
        .header("Content-Type", "application/json")
        .header("Accept", "text/event-stream");
    for (name, value) in request.headers {
        builder = builder.header(*name, *value);
    }
    if !request.user_agent.trim().is_empty() {
        builder = builder.header("User-Agent", request.user_agent);
    }

    match builder.send(request.body) {
        Ok(response) => Ok(split(response)),
        Err(error) => Err(classify(&error)),
    }
}

/// GET 往返（模型列表发现用）：状态码同样不转错误，正文留给调用方判断。
pub fn get(
    agent: &ureq::Agent,
    url: &str,
    user_agent: &str,
    timeout_seconds: f64,
    headers: &[(&str, &str)],
) -> Result<HttpResponse, TransportFailure> {
    let timeout = Duration::from_secs_f64(timeout_seconds.max(1.0));
    let mut builder = agent
        .get(url)
        .config()
        .timeout_connect(Some(timeout))
        .timeout_recv_response(Some(timeout))
        .timeout_recv_body(Some(timeout))
        .build()
        .header("Accept", "application/json");
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    if !user_agent.trim().is_empty() {
        builder = builder.header("User-Agent", user_agent);
    }

    match builder.call() {
        Ok(response) => Ok(split(response)),
        Err(error) => Err(classify(&error)),
    }
}

fn split(response: Response<Body>) -> HttpResponse {
    let status = response.status().as_u16();
    HttpResponse {
        status,
        body: Box::new(response.into_body().into_reader()),
    }
}

fn classify(error: &ureq::Error) -> TransportFailure {
    match error {
        ureq::Error::Timeout(_) => TransportFailure::Timeout,
        ureq::Error::HostNotFound | ureq::Error::ConnectionFailed => TransportFailure::Connection,
        ureq::Error::Io(error) => TransportFailure::Io(error.to_string()),
        other => TransportFailure::Other(other.to_string()),
    }
}
