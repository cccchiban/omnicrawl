//! 联网工具共用的阻塞式 HTTP 传输：浏览器外壳、系统代理检测、内网直连与重定向跟随。
//!
//! 语义基准是 `omnicrawl/net/web_search.py`（桌面浏览器请求头、Windows 系统代理检测）与
//! `omnicrawl/net/fetcher.py`（内网/本机目标直连、302/meta refresh 跟随、`insecure` 跳过
//! 证书校验）。测试用记录型实现替换 [`WebTransport`]，不必起网络。

use std::io::Read;
// 只有 Windows 的系统代理解析（`registry_value`）需要起子进程读注册表。
#[cfg(windows)]
use std::process::Command;
use std::str::FromStr;
use std::time::Duration;

use super::cookies::{self, SharedJar};
use super::referrer;

pub const DEFAULT_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) \
AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36";

/// 与 httpx 默认一致的重定向上限。
pub const MAX_REDIRECTS: u32 = 20;
/// 会带 `location` 的重定向状态码（303/307/308 语义不同，但抓取场景都跟随）。
pub const REDIRECT_STATUSES: [u16; 5] = [301, 302, 303, 307, 308];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebErrorKind {
    Timeout,
    Connect,
    /// 证书链不可信（只有这类才可能被 `insecure=true` 绕过）。
    Tls,
    /// TLS 握手谈不成：对端只支持 1.2、套件不匹配、Alert 等。
    Handshake,
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
    /// 响应头（名字已转小写，同名多值按到达顺序保留）。
    ///
    /// 除 `accept-ch` 协商外，正文解码也要读它：`content-type` 的 `charset` 决定
    /// GBK 等站点怎么解（见 [`charset`](super::charset)）。因此真实通路必须填充它，
    /// 只有不涉及文本的内部桩才给空向量。
    pub headers: Vec<(String, String)>,
}

impl WebResponse {
    /// 取单个响应头（名字大小写不敏感；多值时用 `header_values`）。
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    /// 取某个响应头的全部值（`set-cookie` 这类多值头需要）。
    pub fn header_values<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a str> + 'a {
        self.headers
            .iter()
            .filter(move |(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
    /// 文本视图：按 `content-type` 的 `charset` 解码（网页抓取用）。
    ///
    /// 不能一律按 UTF-8 强解：GBK 站点会得到满屏替换字符而状态码仍是 200。
    pub fn text(&self) -> String {
        super::charset::decode_body(&self.body, self.header("content-type")).0
    }
}

/// 按 `content-encoding` 还原响应体。
///
/// 本栈按档案族声明 `accept-encoding`（`gzip, deflate, br`），因此正常情况下服务端会回
/// 压缩体；此外服务端并不总守约定：`www.163.com` 在 h2 下即使没收到声明也会回
/// `content-encoding: gzip`。若把那些字节当文本，正文会变成乱码而状态码仍是 200——
/// 比明确报错更难排查，因此统一在出口还原。认不出的编码明确报错，绝不把压缩字节透给上层。
pub fn decode_content(body: Vec<u8>, encoding: Option<&str>) -> Result<Vec<u8>, WebError> {
    let declared = encoding.unwrap_or("").to_lowercase();
    let mut encodings: Vec<&str> = declared
        .split(',')
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .collect();
    // 没声明编码却带着 gzip 魔数：同样是压缩体，按 gzip 处理。
    if encodings.is_empty() && body.starts_with(&[0x1f, 0x8b]) {
        encodings.push("gzip");
    }
    // 多层编码按声明的逆序还原。
    let mut body = body;
    for encoding in encodings.iter().rev() {
        body = decode_one_encoding(body, encoding)?;
    }
    Ok(body)
}

fn decode_one_encoding(body: Vec<u8>, encoding: &str) -> Result<Vec<u8>, WebError> {
    match encoding {
        "" | "identity" => Ok(body),
        "gzip" | "x-gzip" => inflate_gzip(&body),
        "deflate" => inflate_deflate(&body),
        "br" => inflate_brotli(&body),
        // `zstd` 需要 C 依赖（zstd-sys），本项目不引入 C/C++ 工具链，因此不声明该编码；
        // 服务端若仍然回了它，只能如实报错而不是把二进制当文本。
        other => Err(WebError::new(
            WebErrorKind::Other,
            format!("服务端返回了本实现不支持的压缩编码 {other}。"),
        )),
    }
}

fn inflate_brotli(body: &[u8]) -> Result<Vec<u8>, WebError> {
    let mut out = Vec::new();
    brotli::BrotliDecompress(&mut std::io::Cursor::new(body), &mut out)
        .map_err(|error| WebError::new(WebErrorKind::Other, format!("brotli 解压失败：{error}")))?;
    Ok(out)
}

fn inflate_gzip(body: &[u8]) -> Result<Vec<u8>, WebError> {
    // 传输层可能已经解过压却仍保留着响应头：没有 gzip 魔数就说明正文已是明文。
    if !body.starts_with(&[0x1f, 0x8b]) {
        return Ok(body.to_vec());
    }
    let mut out = Vec::new();
    flate2::read::GzDecoder::new(body)
        .read_to_end(&mut out)
        .map_err(|error| WebError::new(WebErrorKind::Other, format!("gzip 解压失败：{error}")))?;
    Ok(out)
}

/// `deflate` 在实践中有两种写法：zlib 包装与裸流脚本。先按 zlib 解，失败再按裸流解。
fn inflate_deflate(body: &[u8]) -> Result<Vec<u8>, WebError> {
    let mut out = Vec::new();
    if flate2::read::ZlibDecoder::new(body)
        .read_to_end(&mut out)
        .is_ok()
    {
        return Ok(out);
    }
    out.clear();
    flate2::read::DeflateDecoder::new(body)
        .read_to_end(&mut out)
        .map_err(|error| {
            WebError::new(WebErrorKind::Other, format!("deflate 解压失败：{error}"))
        })?;
    Ok(out)
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
    /// [`super::tls_transport::TlsWebTransport`] 会据此挑选 TLS 档案与 HTTP 头。
    pub impersonate: Option<String>,
    /// 本次请求要用的 cookie 罐：传输层逐跳自动回带与收割。
    ///
    /// 放在请求上而不是传输上，是因为罐的生命周期归调用方（抓取工具跨请求共用一份），
    /// 传输只是无状态的执行体。`None` 表示不处理 cookie。
    pub cookie_jar: Option<SharedJar>,
    /// 发起这次请求的地址；`None` 表示没有跨站发起方（调用方直接抓取）。
    ///
    /// 两处要用它：`SameSite` 判「跨站」得比较发起方与目标的站点；`Referer` 的值直接来自它。
    /// 重定向链由传输层逐跳填写（发起方是上一跳）。
    pub initiator: Option<String>,
    /// `Referer` 的当前策略，随响应头收窄（见 [`referrer`](super::referrer)）。
    pub referrer_policy: super::referrer::Policy,
    /// 这次请求的用途：决定 `Sec-Fetch-*` 的取值（见
    /// [`fetch_metadata`](super::fetch_metadata)）。
    pub purpose: super::fetch_metadata::Purpose,
    /// TLS 会话缓存：`Some` 时尝试复用票据（默认关闭，由配置 `[tls_resumption]` 控制）。
    ///
    /// 放在请求上而不是传输上，与 `cookie_jar` 同理：缓存的生命周期归调用方，
    /// 一次抓取的多个 URL 共用一份。
    pub tls_session_cache: Option<std::sync::Arc<omnicrawl_tls::session_cache::SessionCache>>,
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
            cookie_jar: None,
            initiator: None,
            referrer_policy: super::referrer::Policy::DEFAULT,
            purpose: super::fetch_metadata::Purpose::Navigation,
            tls_session_cache: None,
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
            cookie_jar: None,
            initiator: None,
            referrer_policy: super::referrer::Policy::DEFAULT,
            // 带请求体的 POST 不是导航：浏览器里没有「navigate + JSON body」这种请求。
            purpose: super::fetch_metadata::Purpose::Api,
            tls_session_cache: None,
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
            cookie_jar: None,
            initiator: None,
            referrer_policy: super::referrer::Policy::DEFAULT,
            // 带请求体的 POST 不是导航：浏览器里没有「navigate + JSON body」这种请求。
            purpose: super::fetch_metadata::Purpose::Api,
            tls_session_cache: None,
        }
    }
}

pub trait WebTransport: Send + Sync {
    /// 按 `request.method` 发送一次请求；失败只回报分类与文本。
    fn send(&self, request: &WebRequest) -> Result<WebResponse, WebError>;

    /// 一次顶层文档导航（GET 页面）该带哪些浏览器头。
    ///
    /// 默认给整套 [`browser_headers`]：没有设备档案的传输只能靠这套手写头凑出桌面浏览器外壳。
    ///
    /// [`super::tls_transport::TlsWebTransport`] 会覆盖本方法：指纹传输不注入任何默认头，
    /// 整套导航头（UA、Client Hints、`sec-fetch-*`、`accept`）必须由它按族给全，否则
    /// HTTP 层的版本与 TLS 档案就对不上。
    ///
    /// `impersonate` 是本次请求归一化后的档案族名（`chrome`/`firefox`/`safari`/`edge`）。
    fn navigation_headers(
        &self,
        user_agent: &str,
        impersonate: Option<&str>,
    ) -> Vec<(String, String)> {
        let _ = impersonate;
        browser_headers(user_agent)
    }
}

/// 桌面浏览器常规请求头（顺序与 Python 的字典一致，便于对照与排障）。
///
/// 只给**没有指纹档案**的传输用（ureq、测试桩）。带档案的传输走
/// [`WebTransport::navigation_headers`]，由 [`super::tls_transport::family_headers`] 按族
/// 生成同一批头——档案值才是与 TLS 指纹同源的那一份，手写这套用在档案传输上会把版本与
/// 平台写矛盾（这里的 UA 是 Chrome/124+Windows，而 TLS 档案是 Chromium 145）。
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

/// scheme 的默认端口；未知 scheme 返回 0（调用方通常会再 `.max(1)`）。
pub fn default_port(scheme: &str) -> u16 {
    match scheme {
        "https" => 443,
        "http" => 80,
        _ => 0,
    }
}

/// 把 `authority` 拆成主机与端口，支持 IPv6 字面量（`[::1]:8080`）。
///
/// 方括号里的冒号不是端口分隔符，因此不能直接用 `rsplit_once(':')`。
pub fn split_authority<'a>(authority: &'a str, scheme: &str) -> Option<(&'a str, u16)> {
    if let Some(rest) = authority.strip_prefix('[') {
        let end = rest.find(']')?;
        let host = &rest[..end];
        let port = match rest[end + 1..].strip_prefix(':') {
            Some(value) => value.parse::<u16>().ok()?.max(1),
            None => default_port(scheme).max(1),
        };
        return Some((host, port));
    }
    match authority.rsplit_once(':') {
        Some((host, port)) if port.chars().all(|character| character.is_ascii_digit()) => {
            Some((host, port.parse::<u16>().ok()?.max(1)))
        }
        _ => Some((authority, default_port(scheme).max(1))),
    }
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
        // 逐跳回带：罐里该给的 cookie 在每一跳都重新算（重定向可能换主机或路径）。
        let mut headers = request.headers.clone();
        // 语境来自请求本身：`initiator` 决定是否跨站，`SameSite` 据此决定带不带。
        cookies::apply_header_in(
            request.cookie_jar.as_deref(),
            &request.url,
            &cookies::RequestContext::new(request.initiator.as_deref(), true, &request.method),
            &mut headers,
        );
        // `Referer`：值由发起方与策略共同决定（跨源默认只交源）。
        referrer::apply_header(
            request.referrer_policy,
            request.initiator.as_deref(),
            &request.url,
            &mut headers,
        );
        // `Sec-Fetch-*`：描述这次请求在干什么，取值随发起方与用途变化。
        let metadata = super::fetch_metadata::FetchMetadata::compute(
            request.purpose,
            request.initiator.as_deref(),
            &request.url,
        );
        super::fetch_metadata::apply(&mut headers, &metadata);
        // 条件请求头：有可复用副本时先问「变了没有」，304 时用副本还原正文。
        super::http_cache::apply_conditional(
            &cache_key_for(request),
            &request.method,
            &mut headers,
        );
        for (key, value) in &headers {
            builder = builder.header(key.as_str(), value.as_str());
        }
        // 站点声明过的高熵 Client Hints 在这里补：与 TLS 通路同一套协商状态。
        let family = family_of(request.impersonate.as_deref());
        for hint in super::client_hints::hints_for(&request.url) {
            if let Some(value) = super::tls_transport::client_hint_value(&hint, &family) {
                builder = builder.header(hint.as_str(), value.as_str());
            }
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
                // ureq 按自身特性可能已解压，但服务端仍带着 content-encoding 头；
                // 这里把声明交给出口，由 decode_content 决定是否需要再解一次。
                let content_encoding = response
                    .headers()
                    .get("content-encoding")
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_string);
                // `Set-Cookie` 可以有多个同名头，`get` 只返回第一个。
                let set_cookies: Vec<String> = response
                    .headers()
                    .get_all("set-cookie")
                    .iter()
                    .filter_map(|value| value.to_str().ok())
                    .map(str::to_string)
                    .collect();
                // 全部响应头必须在这里取：`into_body` 之后 `response` 已被消费。
                let headers: Vec<(String, String)> = response
                    .headers()
                    .iter()
                    .filter_map(|(name, value)| {
                        value
                            .to_str()
                            .ok()
                            .map(|value| (name.as_str().to_lowercase(), value.to_string()))
                    })
                    .collect();
                let mut raw = Vec::new();
                let _ = response.into_body().into_reader().read_to_end(&mut raw);
                Ok(RawResponse {
                    status,
                    location,
                    content_encoding,
                    set_cookies,
                    body: raw,
                    headers,
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
    /// 服务端声明的压缩方式；由出口统一决定是否解压。
    content_encoding: Option<String>,
    /// 本条响应的全部 `Set-Cookie`（同名多值，不能只取第一个）。
    set_cookies: Vec<String>,
    body: Vec<u8>,
    /// 全部响应头（名字转小写），供 `accept-ch` 协商等上层判定使用。
    headers: Vec<(String, String)>,
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
        // 每一跳的发起方：首跳沿用调用方给的（通常 `None`），重定向后变成上一跳地址。
        let mut previous: Option<String> = request.initiator.clone();
        // `Referer` 策略随响应头逐跳收窄，起点是请求带来的值。
        let mut policy = request.referrer_policy;
        // 304 但本地没有副本时，剔掉那个键并重来一次；只重试一次，避免与服务端互相踢球。
        let mut retried_without_copy = false;
        loop {
            // HSTS：声明过的站点不再走明文，第一跳与重定向目标都要判。
            // 升级后握手失败就如实报错，绝不回退明文（回退会把降级攻击变成默认行为）。
            if let Some(upgraded) = super::hsts::upgrade(&current) {
                current = upgraded;
            }
            let mut hop = request.clone();
            hop.url = current.clone();
            hop.initiator = previous.clone();
            hop.referrer_policy = policy;
            // 键必须在发送**前**定下：这一响应可能带 `Set-Cookie`，收割之后罐里就变了，
            // 而 `cache_key_for` 含 Cookie——用收割后的状态当键会与查时用的键对不上。
            let hop_key = cache_key_for(&hop);
            let response = self.send_once(&hop)?;
            // 从这一响应学习 HSTS（只认 https 来源，见 `hsts` 模块）。
            super::hsts::record(&current, &response.headers);
            // 响应头里的 `Referrer-Policy` 只收窄不放宽，逐跳累积。
            policy = super::referrer::policy_from_response(policy, &response.headers);
            // 收割这一跳的 cookie：重定向链上的 `Set-Cookie` 必须立即生效，
            // 否则「登录跳转后带不上会话」的站点永远跟不进去。
            cookies::harvest(
                request.cookie_jar.as_deref(),
                &current,
                response.set_cookies.iter(),
            );
            // 只跟随 GET 的重定向：POST（图片生成）由服务端直接应答，改写方法会丢请求体。
            if request.method == "GET" && REDIRECT_STATUSES.contains(&response.status) {
                if let Some(location) = response.location.as_deref() {
                    if !location.trim().is_empty() {
                        if redirects >= request.max_redirects {
                            return Err(WebError::new(WebErrorKind::Other, "重定向次数过多。"));
                        }
                        redirects += 1;
                        // 下一跳的发起方就是这一跳：`SameSite` 靠它判跨站。
                        previous = Some(current.clone());
                        current = resolve_url(&current, location);
                        continue;
                    }
                }
            }
            // 304：服务端说「你手上那份还有效」。正文在缓存副本里，原样交出去只会让上层
            // 拿到一个空页面，因此这里用副本还原成 200（见 `http_cache` 模块文档）。
            // 只有 GET 会带条件头，也只有 GET 有可还原的副本：别的方
            // 法收到 304 时如实上报，绝不拿一份 GET 的正文冒充它的结果。
            if response.status == 304 && request.method == "GET" {
                if let Some(reused) = super::http_cache::reuse(&hop_key, &response.headers) {
                    super::client_hints::record(&current, &reused.headers);
                    return Ok(WebResponse {
                        status: 200,
                        final_url: current,
                        body: reused.body,
                        headers: reused.headers,
                    });
                }
                // 没有副本（换了进程、超限被清、或键在收割 cookie 后变了）：剔除这个键
                // 再重试一次。直接报错会让一次本该成功的抓取失败，而重试是无条件请求，
                // 服务端必然回完整响应。只重试一次：仍拿 304 就说明对方行为异常，如实上报。
                super::http_cache::forget(&hop_key);
                if !retried_without_copy {
                    retried_without_copy = true;
                    continue;
                }
                return Err(WebError::new(
                    WebErrorKind::Other,
                    "服务端回了 304，但本进程没有可还原的缓存副本。".to_string(),
                ));
            }
            let final_url = current;
            let body = decode_content(response.body, response.content_encoding.as_deref())?;
            // 存的是已解码正文：存压缩体会让 304 还原出来的是二进制。
            super::http_cache::store(&hop_key, response.status, &body, &response.headers);
            // 明文通路同样记声明：切换通路不应丢掉协商结果（须在 `final_url` 移入结构体前借用）。
            super::client_hints::record(final_url.as_str(), &response.headers);
            return Ok(WebResponse {
                status: response.status,
                final_url,
                body,
                headers: response.headers,
            });
        }
    }
}

/// 一次请求的缓存键：URL 加上这次实际会发出的 `Cookie` 头。
///
/// 必须**重新算一遍** `Cookie` 而不能读 `request.headers`：反序列化时那份不含 jar 里的
/// cookie，用它当键会让「存」与「查」落到两个不同的键上——带 cookie 的请求永远命中不了。
pub fn cache_key_for(request: &WebRequest) -> String {
    let mut headers = request.headers.clone();
    cookies::apply_header_in(
        request.cookie_jar.as_deref(),
        &request.url,
        &cookies::RequestContext::new(request.initiator.as_deref(), true, &request.method),
        &mut headers,
    );
    super::http_cache::key_for(&request.url, &headers)
}

/// 明文通路用的档案族名：`impersonate` 已是归一化后的族名，未知值回退 `chrome`。
fn family_of(impersonate: Option<&str>) -> String {
    match impersonate {
        Some("firefox") => "firefox".to_string(),
        Some("safari") => "safari".to_string(),
        Some("edge") => "edge".to_string(),
        _ => "chrome".to_string(),
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

    /// 响应头必须真的驱动正文解码：模块本身对了、接线没接上是最容易漏的一类缺陷。
    #[test]
    fn text_uses_the_charset_from_response_headers() {
        let response = WebResponse {
            status: 200,
            final_url: "https://example.com/gbk".to_string(),
            // 「中文」的 GBK 字节。
            body: vec![0xD6, 0xD0, 0xCE, 0xC4],
            headers: vec![(
                "content-type".to_string(),
                "text/html; charset=gbk".to_string(),
            )],
        };
        assert_eq!(response.text(), "中文");

        // 没有 content-type 时回退 UTF-8，不得因为缺少头而失败。
        let plain = WebResponse {
            status: 200,
            final_url: "https://example.com/plain".to_string(),
            body: "正文".as_bytes().to_vec(),
            headers: Vec::new(),
        };
        assert_eq!(plain.text(), "正文");
    }

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

    fn gzip(text: &str) -> Vec<u8> {
        use std::io::Write;
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(text.as_bytes()).expect("压缩");
        encoder.finish().expect("收尾")
    }

    #[test]
    fn a_gzip_body_is_restored_from_its_header() {
        let payload = "<html><title>163</title></html>";
        let decoded = decode_content(gzip(payload), Some("gzip")).expect("应当解压");
        assert_eq!(String::from_utf8(decoded).expect("UTF-8"), payload);
    }

    /// 163 的实测形态：不声明 accept-encoding 也回 gzip，且响应头就是唯一的线索。
    #[test]
    fn a_gzip_body_without_a_declared_encoding_is_still_restored() {
        let payload = "网页正文";
        let decoded = decode_content(gzip(payload), None).expect("应当按魔数解压");
        assert_eq!(String::from_utf8(decoded).expect("UTF-8"), payload);
    }

    /// `br` 是 Chromium/Firefox/Safari 的首选编码，必须能还原成原文。
    #[test]
    fn a_brotli_body_is_restored_byte_for_byte() {
        let payload = "网页正文，压缩后必须逐字节还原。".repeat(30);
        let raw = payload.as_bytes().to_vec();
        let mut compressed = Vec::new();
        let params = brotli::enc::BrotliEncoderParams::default();
        brotli::BrotliCompress(&mut std::io::Cursor::new(&raw), &mut compressed, &params)
            .expect("压缩");
        assert!(compressed.len() < raw.len(), "压测片段应当变小");

        let decoded = decode_content(compressed, Some("br")).expect("br 应当能解压");
        assert_eq!(decoded, raw, "解压结果必须与原文一致");
    }

    /// 不认识的编码必须明确报错：把压缩字节当文本会得到「状态 200 + 乱码」，更难排查。
    #[test]
    fn an_unsupported_encoding_is_reported_instead_of_passed_through() {
        let error = decode_content(b"bytes".to_vec(), Some("zstd")).expect_err("应当报错");
        assert!(
            error.message.contains("不支持的压缩编码"),
            "{}",
            error.message
        );
    }

    #[test]
    fn a_plain_body_is_passed_through() {
        let payload = "<html>纯文本</html>".as_bytes().to_vec();
        assert_eq!(
            decode_content(payload.clone(), None).expect("原样"),
            payload
        );
        assert_eq!(
            decode_content(payload.clone(), Some("identity")).expect("原样"),
            payload
        );
    }

    #[test]
    fn an_unknown_encoding_is_reported_instead_of_leaked() {
        let error = decode_content(vec![0x28, 0xb5, 0x2f, 0xfd], Some("br"))
            .expect_err("不支持的编码必须报错");
        assert!(error.message.contains("br"), "实际：{}", error.message);
    }

    #[test]
    fn chained_encodings_are_restored_in_reverse_order() {
        // 先 gzip 再 gzip：声明顺序是「编码顺序」，还原必须逆序。
        let payload = "多层编码";
        let decoded =
            decode_content(gzip_bytes(gzip(payload)), Some("gzip, gzip")).expect("应当逐层解压");
        assert_eq!(String::from_utf8(decoded).expect("UTF-8"), payload);
    }

    fn gzip_bytes(bytes: Vec<u8>) -> Vec<u8> {
        use std::io::Write;
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(&bytes).expect("压缩");
        encoder.finish().expect("收尾")
    }
}
