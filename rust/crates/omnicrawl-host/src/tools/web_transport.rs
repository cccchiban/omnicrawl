//! 联网工具共用的阻塞式 HTTP 传输：浏览器外壳、系统代理检测、内网直连与重定向跟随。
//!
//! 语义基准是 `omnicrawl/net/web_search.py`（桌面浏览器请求头、Windows 系统代理检测）与
//! `omnicrawl/net/fetcher.py`（内网/本机目标直连、302/meta refresh 跟随、`insecure` 跳过
//! 证书校验）。测试用记录型实现替换 [`WebTransport`]，不必起网络。

use std::io::Read;
use std::process::Command;
use std::str::FromStr;
use std::time::Duration;

pub const DEFAULT_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) \
AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36";

/// 与 httpx 默认一致的重定向上限。
pub const MAX_REDIRECTS: u32 = 20;
const REDIRECT_STATUSES: [u16; 5] = [301, 302, 303, 307, 308];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebErrorKind {
    Timeout,
    Connect,
    Tls,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebError {
    pub kind: WebErrorKind,
    pub message: String,
}

impl WebError {
    pub fn new(kind: WebErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebResponse {
    pub status: u16,
    pub final_url: String,
    /// 原始响应体：图片等多字节内容不能被文本解码破坏。
    pub body: Vec<u8>,
}

impl WebResponse {
    /// 文本视图：非 UTF-8 字节按替换字符处理（网页抓取用）。
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).to_string()
    }
}

#[derive(Debug, Clone)]
pub struct WebRequest {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// `None` 表示不使用代理（调用方已决定）；显式代理地址原样传入。
    pub proxy: Option<String>,
    pub insecure: bool,
    pub timeout: Duration,
    pub max_redirects: u32,
    /// 浏览器指纹档案名（`chrome`/`firefox`/`safari`/`edge`），语义对齐 Python
    /// `omnicrawl/net/fetcher.py` 的 `impersonate` 参数（含前缀匹配与未知值回退 `chrome`）。
    ///
    /// `None` 表示调用方没有要求指纹模拟：普通传输（ureq）会忽略它，只有
    /// [`super::wreq_transport::WreqWebTransport`] 会据此挑选浏览器档案。
    pub impersonate: Option<String>,
}

impl WebRequest {
    pub fn new(url: impl Into<String>, headers: Vec<(String, String)>, timeout: Duration) -> Self {
        Self {
            method: "GET".to_string(),
            url: url.into(),
            headers,
            body: Vec::new(),
            proxy: None,
            insecure: false,
            timeout,
            max_redirects: MAX_REDIRECTS,
            impersonate: None,
        }
    }

    pub fn post_json(
        url: impl Into<String>,
        headers: Vec<(String, String)>,
        body: &str,
        timeout: Duration,
    ) -> Self {
        Self {
            method: "POST".to_string(),
            url: url.into(),
            headers,
            body: body.as_bytes().to_vec(),
            proxy: None,
            insecure: false,
            timeout,
            max_redirects: 0,
            impersonate: None,
        }
    }

    pub fn post_bytes(
        url: impl Into<String>,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
        timeout: Duration,
    ) -> Self {
        Self {
            method: "POST".to_string(),
            url: url.into(),
            headers,
            body,
            proxy: None,
            insecure: false,
            timeout,
            max_redirects: 0,
            impersonate: None,
        }
    }
}

pub trait WebTransport: Send + Sync {
    /// 按 `request.method` 发送一次请求；失败只回报分类与文本。
    fn send(&self, request: &WebRequest) -> Result<WebResponse, WebError>;
}

/// 桌面浏览器常规请求头（顺序与 Python 的字典一致，便于对照与排障）。
pub fn browser_headers(user_agent: &str) -> Vec<(String, String)> {
    let pairs = [
        ("User-Agent", user_agent),
        (
            "Accept",
            "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,image/apng,*/*;q=0.8",
        ),
        ("Accept-Language", "zh-CN,zh;q=0.9,en;q=0.8"),
        ("Connection", "keep-alive"),
        ("Upgrade-Insecure-Requests", "1"),
        ("Sec-Fetch-Dest", "document"),
        ("Sec-Fetch-Mode", "navigate"),
        ("Sec-Fetch-Site", "none"),
        ("Sec-Fetch-User", "?1"),
        ("Cache-Control", "no-cache"),
    ];
    pairs
        .iter()
        .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
        .collect()
}

pub fn host_of(url: &str) -> Option<String> {
    let uri = ureq::http::Uri::from_str(url).ok()?;
    uri.host().map(|value| value.to_string())
}

/// 内网/本机主机名：localhost、`.local` 与常见内网 IPv4 段（对齐 Python 侧正则）。
pub fn is_private_host(host: &str) -> bool {
    let lowered = host.trim().to_lowercase();
    if lowered.is_empty() {
        return false;
    }
    if lowered == "localhost" || lowered.ends_with(".localhost") || lowered.ends_with(".local") {
        return true;
    }
    let parts: Vec<&str> = lowered.split('.').collect();
    if parts.len() != 4 {
        return false;
    }
    let mut numbers = Vec::with_capacity(4);
    for part in parts {
        match part.parse::<u32>() {
            Ok(value) if value <= 255 => numbers.push(value),
            _ => return false,
        }
    }
    matches!(
        (numbers[0], numbers[1]),
        (127, _) | (10, _) | (192, 168) | (172, 16..=31)
    )
}

/// 相对地址解析（`urllib.parse.urljoin` 的常用分支）。
pub fn resolve_url(base: &str, target: &str) -> String {
    let target = target.trim();
    if target.is_empty() {
        return base.to_string();
    }
    if has_scheme(target) {
        return target.to_string();
    }
    let Ok(base_uri) = ureq::http::Uri::from_str(base) else {
        return target.to_string();
    };
    let scheme = base_uri.scheme_str().unwrap_or("https");
    let authority = match base_uri.authority() {
        Some(value) => value.as_str().to_string(),
        None => return target.to_string(),
    };
    if let Some(rest) = target.strip_prefix("//") {
        return format!("{scheme}://{rest}");
    }
    let base_path = base_uri.path();
    if let Some(fragment) = target.strip_prefix('#') {
        let query = base_uri
            .query()
            .map(|value| format!("?{value}"))
            .unwrap_or_default();
        return format!("{scheme}://{authority}{base_path}{query}#{fragment}");
    }
    if let Some(query) = target.strip_prefix('?') {
        return format!("{scheme}://{authority}{base_path}?{query}");
    }
    if let Some(rest) = target.strip_prefix('/') {
        return format!("{scheme}://{authority}/{}", normalize_path(rest));
    }
    let directory = match base_path.rfind('/') {
        Some(index) => &base_path[..=index],
        None => "/",
    };
    format!(
        "{scheme}://{authority}/{}",
        normalize_path(&format!("{directory}{target}"))
    )
}

fn has_scheme(value: &str) -> bool {
    match value.find(':') {
        Some(index) => {
            index > 0
                && value[..index]
                    .chars()
                    .all(|character| character.is_ascii_alphanumeric() || "+-.".contains(character))
        }
        None => false,
    }
}

/// 去掉 `.` 与 `..` 段；`..` 越出根时忽略（与 urljoin 的规范化一致）。
fn normalize_path(path: &str) -> String {
    let mut segments: Vec<&str> = Vec::new();
    for segment in path.split('/') {
        match segment {
            "" | "." => continue,
            ".." => {
                segments.pop();
            }
            other => segments.push(other),
        }
    }
    let mut normalized = segments.join("/");
    if path.ends_with('/') && !normalized.is_empty() {
        normalized.push('/');
    }
    normalized
}

#[cfg(windows)]
const PROXY_REGISTRY_PATH: &str = r"Software\Microsoft\Windows\CurrentVersion\Internet Settings";

/// 读取 Windows 系统代理（注册表 `ProxyEnable` / `ProxyServer`）。
///
/// 未启用、非 Windows 或读取失败时返回 `None`；多协议形式优先取 `http=` 条目，缺协议时补
/// `http://`。
pub fn detect_windows_proxy() -> Option<String> {
    if !cfg!(windows) {
        return None;
    }
    let enabled = registry_value("ProxyEnable")?;
    if parse_dword(&enabled) == 0 {
        return None;
    }
    let mut server = registry_value("ProxyServer")?.trim().to_string();
    if server.is_empty() {
        return None;
    }
    if server.contains('=') {
        for part in server.split(';') {
            let part = part.trim();
            if part.to_lowercase().starts_with("http=") {
                server = part
                    .split_once('=')
                    .map(|(_, value)| value.trim().to_string())?;
                break;
            }
        }
    }
    if server.is_empty() {
        return None;
    }
    if !server.contains("://") {
        server = format!("http://{server}");
    }
    Some(server)
}

#[cfg(windows)]
fn registry_value(name: &str) -> Option<String> {
    let output = Command::new("reg")
        .args(["query", PROXY_REGISTRY_PATH, "/v", name])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    for line in text.lines() {
        let trimmed = line.trim();
        if !trimmed.starts_with(name) {
            continue;
        }
        let mut parts = trimmed.split_whitespace();
        parts.next()?;
        parts.next()?;
        return parts.next().map(str::to_string);
    }
    None
}

#[cfg(not(windows))]
fn registry_value(_name: &str) -> Option<String> {
    None
}

fn parse_dword(raw: &str) -> u32 {
    let raw = raw.trim();
    match raw.strip_prefix("0x").or_else(|| raw.strip_prefix("0X")) {
        Some(hex) => u32::from_str_radix(hex, 16).unwrap_or(0),
        None => raw.parse::<u32>().unwrap_or(0),
    }
}

/// 真实传输：ureq + rustls，手动跟随重定向（要拿到最终地址与逐跳状态码）。
pub struct UreqWebTransport;

impl UreqWebTransport {
    pub fn new() -> Self {
        Self
    }

    fn send_once(&self, request: &WebRequest) -> Result<RawResponse, WebError> {
        let mut config = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .max_redirects(0)
            .timeout_connect(Some(request.timeout))
            .timeout_recv_response(Some(request.timeout))
            .timeout_recv_body(Some(request.timeout));
        if request.insecure {
            config = config.tls_config(
                ureq::tls::TlsConfig::builder()
                    .disable_verification(true)
                    .build(),
            );
        }
        if let Some(proxy) = request.proxy.as_deref() {
            let parsed = ureq::Proxy::new(proxy).map_err(|error| {
                WebError::new(WebErrorKind::Other, format!("代理地址无效：{error}"))
            })?;
            config = config.proxy(Some(parsed));
        }
        let agent: ureq::Agent = config.build().into();
        let mut builder = ureq::http::Request::builder()
            .method(request.method.as_str())
            .uri(request.url.as_str());
        for (key, value) in &request.headers {
            builder = builder.header(key.as_str(), value.as_str());
        }
        let http_request = builder
            .body(request.body.clone())
            .map_err(|error| WebError::new(WebErrorKind::Other, error.to_string()))?;
        match agent.run(http_request) {
            Ok(response) => {
                let status = response.status().as_u16();
                let location = response
                    .headers()
                    .get("location")
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_string);
                let mut raw = Vec::new();
                let _ = response.into_body().into_reader().read_to_end(&mut raw);
                Ok(RawResponse {
                    status,
                    location,
                    body: raw,
                })
            }
            Err(error) => Err(classify(&error)),
        }
    }
}

/// 一跳的原始响应：`location` 只用于自行跟随重定向。
struct RawResponse {
    status: u16,
    location: Option<String>,
    body: Vec<u8>,
}

impl Default for UreqWebTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl WebTransport for UreqWebTransport {
    fn send(&self, request: &WebRequest) -> Result<WebResponse, WebError> {
        let mut current = request.url.clone();
        let mut redirects = 0;
        loop {
            let mut hop = request.clone();
            hop.url = current.clone();
            let response = self.send_once(&hop)?;
            // 只跟随 GET 的重定向：POST（图片生成）由服务端直接应答，改写方法会丢请求体。
            if request.method == "GET" && REDIRECT_STATUSES.contains(&response.status) {
                if let Some(location) = response.location.as_deref() {
                    if !location.trim().is_empty() {
                        if redirects >= request.max_redirects {
                            return Err(WebError::new(WebErrorKind::Other, "重定向次数过多。"));
                        }
                        redirects += 1;
                        current = resolve_url(&current, location);
                        continue;
                    }
                }
            }
            return Ok(WebResponse {
                status: response.status,
                final_url: current,
                body: response.body,
            });
        }
    }
}

fn classify(error: &ureq::Error) -> WebError {
    let kind = match error {
        ureq::Error::Timeout(_) => WebErrorKind::Timeout,
        ureq::Error::ConnectionFailed | ureq::Error::HostNotFound => WebErrorKind::Connect,
        ureq::Error::Tls(_) => WebErrorKind::Tls,
        _ => WebErrorKind::Other,
    };
    WebError::new(kind, error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_hosts_match_python_rules() {
        for host in [
            "localhost",
            "api.localhost",
            "printer.local",
            "127.0.0.1",
            "10.1.2.3",
            "192.168.1.10",
            "172.16.0.1",
            "172.31.255.254",
        ] {
            assert!(is_private_host(host), "{host} 应当判为内网");
        }
        for host in [
            "example.com",
            "172.32.0.1",
            "172.15.0.1",
            "8.8.8.8",
            "999.1.1.1",
        ] {
            assert!(!is_private_host(host), "{host} 不该判为内网");
        }
    }

    #[test]
    fn relative_urls_follow_the_base_page() {
        let base = "https://example.com/a/b/page.html?x=1#frag";
        assert_eq!(
            resolve_url(base, "c.html"),
            "https://example.com/a/b/c.html"
        );
        assert_eq!(
            resolve_url(base, "/root.html"),
            "https://example.com/root.html"
        );
        assert_eq!(
            resolve_url(base, "../up.html"),
            "https://example.com/a/up.html"
        );
        assert_eq!(
            resolve_url(base, "//cdn.example.com/x.js"),
            "https://cdn.example.com/x.js"
        );
        assert_eq!(
            resolve_url(base, "https://other.example.org/z"),
            "https://other.example.org/z"
        );
        assert_eq!(
            resolve_url(base, "?y=2"),
            "https://example.com/a/b/page.html?y=2"
        );
    }

    #[test]
    fn dword_values_accept_hex_and_decimal() {
        assert_eq!(parse_dword("0x1"), 1);
        assert_eq!(parse_dword("0"), 0);
        assert_eq!(parse_dword("2"), 2);
    }
}
