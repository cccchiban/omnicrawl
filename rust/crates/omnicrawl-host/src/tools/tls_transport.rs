//! fetcher 与 web_search 的浏览器指纹传输：omnicrawl-tls 自研栈，不再依赖 BoringSSL。
//!
//! ## 为什么不用 ureq
//!
//! [`super::web_transport::UreqWebTransport`] 是 ureq + rustls：TLS ClientHello 与
//! HTTP/2 设置都是固定的库指纹，任何做 TLS 指纹校验（JA3/JA4）的站点都会直接拒绝。
//! `omnicrawl/net/fetcher.py` 把这件事委托给 curl_cffi 的 `impersonate`，且默认值就是
//! `chrome`——它属于默认路径，不是可选增强。
//!
//! ## 为什么不再用 wreq
//!
//! 上一版用 `wreq`（BoringSSL 后端）加 `wreq-util` 的设备档案。功能上没问题，代价是整条
//! 依赖链要求 C/C++ 工具链、CMake、NASM 与 libclang，交叉编译处处受限，而且那套档案
//! **不透明**：换个 wreq-util 版本，线上指纹就变了，没有任何本地证据可对照。
//!
//! 现在改用 [`omnicrawl_tls::driver`]：ClientHello 的扩展顺序、密码套件排列、GREASE、
//! 记录层与握手编排全部在本仓库内，档案数值是对真实浏览器抓包校准出来的
//! （见 `omnicrawl-tls` 的 `profile` 模块文档）。因此这条通路既是纯 Rust，也能被审计。
//!
//! ## 线程模型与连接复用
//!
//! 本版本是阻塞实现，不需要异步运行时，`WebTransport::send` 直接在当前线程跑完。
//!
//! 连接**不跨请求复用**：一个 `Connection` 绑定一个 `:authority`，而复用需要处理
//! GOAWAY、连接过期与服务端主动关闭。抓取场景每次都要访问不同主机，复用的命中率远低于
//! 它带来的状态复杂度——每跳一条新连接是这里刻意的选择。
//!
//! ## 与 wreq 版本的行为差异
//!
//! | 维度 | 现在 | 说明 |
//! |---|---|---|
//! | 内容压缩 | 按族声明 `accept-encoding`（`gzip, deflate, br`） | 只声明本实现真能解开的编码，由 `decode_content` 统一还原 |
//! | HTTP/2 设置 | 只发 MAX_FRAME_SIZE 与 INITIAL_WINDOW_SIZE | 不发 `ENABLE_PUSH=0`（nginx 会 GOAWAY） |
//! | 连接专有头 | 一律不发 | h2 禁止；1.1 通路自动补 `Connection: close` |
//! | 明文 http:// | 走 ureq 分支 | 自研栈只做 TLS，明文没有指纹可模拟 |
//!
//! 不声明 `zstd`：它的 Rust 绑定 `zstd-sys` 需要 C 工具链，与本仓库「纯 Rust 构建」的
//! 约定冲突。声明一个自己解不开的编码并不「更像浏览器」——只会让服务端回不可用的字节。
//!
//! ## 指纹档案到 HTTP 头的一致性
//!
//! TLS 层用哪个族，HTTP 头就必须是同一族同一版本的。因此 UA 与 Client Hints 由
//! [`family_headers`] 按族一次性给出，**不再**沿用 `web_transport::DEFAULT_USER_AGENT`
//! （那串是写死的 Chrome/124，与档案的版本对不上，正是上一版记录过的自相矛盾）。

use std::time::Instant;

use omnicrawl_tls::driver::{Connection, DriverError, Response};
use omnicrawl_tls::hpack::Header;
use omnicrawl_tls::profile::Family;
use omnicrawl_tls::session::HandshakeError;
use omnicrawl_tls::verify::{TrustAnchors, VerifyError};
use serde_json::Value;

use super::cookies;
use super::fetch_metadata;
use super::referrer;
use super::web_transport::{
    decode_content, resolve_url, UreqWebTransport, WebError, WebErrorKind, WebRequest, WebResponse,
    WebTransport, REDIRECT_STATUSES,
};

/// Python `_IMPERSONATE_OPTIONS` 的顺序（`omnicrawl/net/fetcher.py:62`）。
///
/// Python 用 `name == option or name.startswith(option)` 做前缀匹配，所以 `chrome124`
/// 会落到 `chrome`。四个名字互不为前缀，因此顺序不影响结果。
pub const IMPERSONATE_OPTIONS: [&str; 4] = ["chrome", "firefox", "safari", "edge"];

/// 未显式给 `impersonate` 时的档案：对齐 Python `_normalize_impersonate` 的兜底。
pub const DEFAULT_IMPERSONATE: &str = "chrome";

/// 归一化 `impersonate` 参数，语义对齐 Python `_normalize_impersonate`。
///
/// Python 实现是 `str(value or "chrome").strip().lower()` 加前缀匹配，未知值回退
/// `chrome`。其中 `value or "chrome"` 走的是 Python 真值语义：`None`、`False`、`0`
/// 与 `""` 都算假值，会落到默认 `chrome`；`True` 则先 `str()` 成 `"True"` 再参与
/// 前缀匹配（同样匹配不上而回退 `chrome`）。这里逐条对齐，不图省事只判 `None`。
pub fn normalize_impersonate(value: Option<&Value>) -> String {
    let raw = match value {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Bool(true)) => "True".to_string(),
        Some(Value::Number(number)) => {
            // 0 在 Python 里是假值，会走 `or "chrome"`；其余数字按 str() 表示。
            if number.as_f64() == Some(0.0) {
                String::new()
            } else {
                number.to_string()
            }
        }
        // None / Bool(false) / 数字 0 / 空串：Python 的假值分支 → 默认 chrome。
        _ => String::new(),
    };
    let name = raw.trim().to_lowercase();
    for option in IMPERSONATE_OPTIONS {
        if name == option || name.starts_with(option) {
            return option.to_string();
        }
    }
    "chrome".to_string()
}

/// 归一化后的族名映射到 TLS 档案。
fn family_for(name: &str) -> Family {
    Family::parse(name)
}

/// 一个族的版本号：UA 与 `sec-ch-ua` 必须与 TLS 档案同源。
///
/// 数值与 `omnicrawl-tls` 的档案注释一一对应；`edge` 与 Chrome 共用同一份 Chromium
/// 档案（Edge 就是 Chromium），但 UA 里的产品名不同。
struct FamilyIdentity {
    /// 完整 UA。
    user_agent: &'static str,
    /// `sec-ch-ua` 的内容；`None` 表示该族不发 Client Hints。
    client_hints: Option<&'static str>,
    /// `sec-ch-ua-platform`；`None` 表示不发。
    platform: Option<&'static str>,
    /// 该族是否发 `Sec-Fetch-User`（Safari 不发）。
    fetch_user: bool,
    /// `accept` 的默认值。
    accept: &'static str,
    /// `accept-encoding` 的值。只声明本实现真能解开的编码：
    /// 声明了却解不开，等于把二进制塞给正文提取；不声明又明显不像浏览器。
    ///
    /// 各族的顺序与浏览器实际发送一致（`br` 在 Chromium/Firefox 里优先于 `gzip`）。
    /// 不声明 `zstd`：它需要 C 依赖（zstd-sys），本项目不引入 C/C++ 工具链。
    accept_encoding: &'static str,
}

/// Chromium 145（本机 Edge 抓包版本，Chrome/Edge 共用同一份 TLS 档案）。
///
/// Chrome 与 Edge 的 ClientHello 在结构上一致（同属 Chromium），差异只在 UA 里的产品名
/// 与 `sec-ch-ua` 的品牌列表，所以两族共享 TLS 档案、各自的 HTTP 标识。
const CHROMIUM_ACCEPT: &str = "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,\
image/webp,image/apng,*/*;q=0.8,application/signed-exchange;v=b3;q=0.7";

fn identity_for(family: &str) -> FamilyIdentity {
    match family {
        "firefox" => FamilyIdentity {
            user_agent: "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:151.0) \
Gecko/20100101 Firefox/151.0",
            // Firefox 不发 Client Hints。
            client_hints: None,
            platform: None,
            fetch_user: true,
            accept: "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
            accept_encoding: "gzip, deflate, br",
        },
        "safari" => FamilyIdentity {
            user_agent: "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) \
AppleWebKit/605.1.15 (KHTML, like Gecko) Version/26.0 Safari/605.1.15",
            client_hints: None,
            platform: None,
            // Safari 不发 Sec-Fetch-User（与上一版记录一致）。
            fetch_user: false,
            accept: "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
            accept_encoding: "gzip, deflate, br",
        },
        // chrome 与 edge 都走 Chromium 档案，兜底分支同 chrome。
        other => {
            let edge = other == "edge";
            FamilyIdentity {
                user_agent: if edge {
                    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
(KHTML, like Gecko) Chrome/145.0.0.0 Safari/537.36 Edg/145.0.0.0"
                } else {
                    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
(KHTML, like Gecko) Chrome/145.0.0.0 Safari/537.36"
                },
                client_hints: Some(if edge {
                    "\"Microsoft Edge\";v=\"145\", \"Chromium\";v=\"145\", \"Not.A/Brand\";v=\"24\""
                } else {
                    "\"Google Chrome\";v=\"145\", \"Chromium\";v=\"145\", \"Not.A/Brand\";v=\"24\""
                }),
                platform: Some("\"Windows\""),
                fetch_user: true,
                accept: CHROMIUM_ACCEPT,
                // Chromium 实际发 `gzip, deflate, br, zstd`；本实现没有 zstd 解压，
                // 因此只声明到 `br`——声明自己解不开的编码并不「更像浏览器」，
                // 只会让服务端选一个本端处理不了的编码。
                accept_encoding: "gzip, deflate, br",
            }
        }
    }
}

/// 一次顶层文档导航该带哪些浏览器头。
///
/// 顺序刻意保持浏览器的实际顺序（不是字典序）：HTTP/2 的伪头与普通头顺序本身不进
/// JA3/JA4，但对端做头部顺序校验的实现并不少见。
pub fn family_headers(family: &str) -> Vec<(String, String)> {
    let identity = identity_for(family);
    let mut headers = vec![
        ("user-agent".to_string(), identity.user_agent.to_string()),
        ("accept".to_string(), identity.accept.to_string()),
        (
            "accept-encoding".to_string(),
            identity.accept_encoding.to_string(),
        ),
        (
            "accept-language".to_string(),
            "zh-CN,zh;q=0.9,en;q=0.8".to_string(),
        ),
    ];
    if let Some(hints) = identity.client_hints {
        headers.push(("sec-ch-ua".to_string(), hints.to_string()));
        headers.push(("sec-ch-ua-mobile".to_string(), "?0".to_string()));
        headers.push((
            "sec-ch-ua-platform".to_string(),
            identity.platform.unwrap_or("\"Windows\"").to_string(),
        ));
    }
    // `sec-fetch-*` 不在这里写死：它们描述的是「这次请求在干什么」，必须按
    // 发起方与用途算（见 `fetch_metadata`）。写死成导航取值时，一次跨站重定向
    // 会被描述成「地址栏直接输入」，那是说错话而不是像不像的问题。
    headers.push(("upgrade-insecure-requests".to_string(), "1".to_string()));
    headers
}

/// 浏览器指纹传输：TLS/HTTP2 走 omnicrawl-tls，明文 http:// 退到 ureq。
pub struct TlsWebTransport {
    /// 明文请求的兜底传输。`http://` 没有 TLS 可模拟，用 ureq 是唯一合理选择。
    plain: UreqWebTransport,
}

impl TlsWebTransport {
    pub fn new() -> Self {
        Self {
            plain: UreqWebTransport::new(),
        }
    }
}

impl Default for TlsWebTransport {
    fn default() -> Self {
        Self::new()
    }
}

/// 一条已解析的 URL：scheme 决定走哪条通路。
#[derive(Debug, Clone, PartialEq, Eq)]
struct Target {
    scheme: String,
    host: String,
    port: u16,
    /// origin-form 目标（含查询串），HTTP/2 的 `:path` 与 1.1 的请求行都用它。
    path: String,
}

fn parse_target(url: &str) -> Result<Target, WebError> {
    let rest = match url.split_once("://") {
        Some((scheme, rest)) => (scheme.to_lowercase(), rest),
        None => {
            return Err(WebError::new(
                WebErrorKind::Other,
                format!("URL 缺少协议前缀：{url}"),
            ))
        }
    };
    let (scheme, rest) = rest;
    if scheme != "http" && scheme != "https" {
        return Err(WebError::new(
            WebErrorKind::Other,
            format!("不支持的协议：{scheme}"),
        ));
    }
    // 去掉 fragment：它不参与请求。
    let rest = rest.split('#').next().unwrap_or(rest);
    // 路径可能以 `/` 开头，也可能直接以 `?` 开头（`https://example.com?q=1`）。
    // 只按 `/` 切会把查询串当成主机名的一部分。
    let split = rest.find(['/', '?']).unwrap_or(rest.len());
    let (authority, path) = rest.split_at(split);
    if authority.is_empty() {
        return Err(WebError::new(
            WebErrorKind::Other,
            format!("URL 缺少主机名：{url}"),
        ));
    }
    // userinfo 不参与 TLS 与主机名（`user@host` 的主机是 `host`）。
    let host_port = authority
        .rsplit_once('@')
        .map_or(authority, |(_, tail)| tail);
    let (host, port) = match host_port.rsplit_once(':') {
        Some((host, port_text)) if !host.ends_with(']') || port_text.parse::<u16>().is_ok() => {
            match port_text.parse::<u16>() {
                Ok(port) => (host.to_string(), port),
                Err(_) => {
                    return Err(WebError::new(
                        WebErrorKind::Other,
                        format!("URL 端口非法：{port_text}"),
                    ))
                }
            }
        }
        _ => (
            host_port.to_string(),
            if scheme == "https" { 443 } else { 80 },
        ),
    };
    if host.is_empty() {
        return Err(WebError::new(
            WebErrorKind::Other,
            format!("URL 缺少主机名：{url}"),
        ));
    }
    let path = if path.is_empty() { "/" } else { path };
    Ok(Target {
        scheme,
        host,
        port,
        path: path.to_string(),
    })
}

/// 把某族的一跳请求跑完：建连 → 发请求 → 收响应。
fn send_once(
    request: &WebRequest,
    target: &Target,
    family: &str,
    trust: Option<TrustAnchors>,
) -> Result<(Response, String), WebError> {
    let tls_family = family_for(family);
    // 会话恢复只在「没走代理」时启用：代理通路是独立的连接实现，票据复用尚未在那里验证，
    // 贸然套用会让本来正常的代理抓取多一层风险。
    let use_resumption = request.tls_session_cache.is_some()
        && !matches!(request.proxy.as_deref(), Some(proxy) if !proxy.trim().is_empty());
    let mut connection = if use_resumption {
        match request.tls_session_cache.as_deref() {
            Some(cache) => Connection::connect_to_with_resumption(
                &target.host,
                target.port,
                tls_family,
                trust,
                request.timeout,
                cache,
            ),
            None => unreachable!("use_resumption 已经要求缓存存在"),
        }
    } else {
        match request.proxy.as_deref() {
            Some(proxy) if !proxy.trim().is_empty() => Connection::connect_via_proxy(
                proxy,
                &target.host,
                target.port,
                tls_family,
                trust,
                request.timeout,
            ),
            _ => Connection::connect_to(
                &target.host,
                target.port,
                tls_family,
                trust,
                request.timeout,
            ),
        }
    }
    .map_err(|error| classify_driver(&error))?;

    // 逐跳回带：罐里的 cookie 在每跳重新计算（重定向可能换主机或路径）。
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
    let mut metadata = fetch_metadata::FetchMetadata::compute(
        request.purpose,
        request.initiator.as_deref(),
        &request.url,
    );
    // Safari 不发 `Sec-Fetch-User`（族差异与其它头同源，见 `identity_for`）。
    metadata.user &= identity_for(&family).fetch_user;
    fetch_metadata::apply(&mut headers, &metadata);
    // 条件请求头：有可复用副本时先问「变了没有」，304 时用副本还原正文。
    // 键与传输层的「存」共用同一个函数，否则会存进去却永远查不到。
    super::http_cache::apply_conditional(
        &super::web_transport::cache_key_for(request),
        &request.method,
        &mut headers,
    );
    let headers = to_hpack_headers(&headers);
    // 高熵 Client Hints 只在站点通过 `Accept-CH` 声明之后才补：真实浏览器就是这样，
    // 没被要求就先发反而是明显的自动化特征（见 `client_hints`）。
    let headers = apply_negotiated_hints(headers, &request.url, family);
    let response = connection
        .request_with_headers(
            &request.method,
            &target.path,
            &headers,
            if request.body.is_empty() {
                None
            } else {
                Some(&request.body)
            },
        )
        .map_err(|error| classify_driver(&error))?;
    let protocol = connection.protocol();
    Ok((response, protocol.as_str().to_string()))
}

/// 按协商声明给出某个提示头的值；本实现没有真实值可给时返回 `None`（宁可不发）。
///
/// 与 `identity_for` 共用同一份档案，因此头里的平台/品牌与实际 TLS 档案必然一致。
/// 族级把关也在这里：Firefox 与 Safari 没有 Client Hints 体系，任何高熵提示都不发
/// ——服务端声明了也一样，真实浏览器在那种情况下同样不会发。
pub(crate) fn client_hint_value(hint: &str, family: &str) -> Option<String> {
    let identity = identity_for(family);
    // `client_hints` 为 `None` 的族（Firefox/Safari）没有这套头。
    identity.client_hints?;
    match hint {
        "sec-ch-ua" => identity.client_hints.map(str::to_string),
        "sec-ch-ua-mobile" => Some("\"?0\"".to_string()),
        "sec-ch-ua-platform" => identity.platform.map(str::to_string),
        "sec-ch-ua-platform-version" => {
            super::client_hints::platform_version(family).map(str::to_string)
        }
        // 架构/位数/型号/完整版本没有真实值可给（会随设备变），一律不发。
        _ => None,
    }
}

/// 在已编码的头集合上按协商结果补高熵 Client Hints。
///
/// 抽成独立函数是为了能直接断言「最终要发出去的字节」——只测协商状态无法证明头真的发出去了。
fn apply_negotiated_hints(mut headers: Vec<Header>, url: &str, family: &str) -> Vec<Header> {
    for hint in super::client_hints::hints_for(url) {
        // 声明里可能是 `sec-ch-ua` 这类基础提示；值缺失就跳过，绝不发空头。
        let Some(value) = client_hint_value(&hint, family) else {
            continue;
        };
        headers.retain(|header| !header.name.eq_ignore_ascii_case(&hint));
        headers.push(Header::new(hint, value));
    }
    headers
}

fn to_hpack_headers(headers: &[(String, String)]) -> Vec<Header> {
    headers
        .iter()
        // 逐跳与专有头在两条通路上都不该由调用方给：h2 禁止它们，1.1 由驱动层自己补。
        //
        // `accept-encoding` 不在此列：它由档案族按「本实现真能解压的编码」给出，
        // 属于正常的请求头。把它过滤掉会让服务端收不到压缩偏好（与浏览器不一致），
        // 而解压已由 `decode_content` 统一处理。
        .filter(|(name, _)| {
            !matches!(
                name.to_lowercase().as_str(),
                "host"
                    | "connection"
                    | "keep-alive"
                    | "proxy-connection"
                    | "transfer-encoding"
                    | "upgrade"
            )
        })
        .map(|(name, value)| Header::new(name.to_lowercase(), value))
        .collect()
}

/// 把驱动层错误映射到内核统一的分类。
///
/// 只有**证书链本身**的问题才算 `Tls`：握手阶段的其他失败（对端只支持 1.2、套件不匹配、
/// Alert、密钥派生失败）都属于「这个站点谈不成」，把它们也报成「证书校验失败」会让人
/// 去查证书、甚至加 `insecure=true` 重试，而那条路根本走不到证书校验。
fn classify_driver(error: &DriverError) -> WebError {
    match error {
        DriverError::Io(io) => match io.kind() {
            std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock => {
                WebError::new(WebErrorKind::Timeout, "请求超时。".to_string())
            }
            _ => WebError::new(WebErrorKind::Connect, io.to_string()),
        },
        DriverError::Handshake(inner) => match inner {
            HandshakeError::Verification(verify) if is_certificate_problem(verify) => {
                WebError::new(WebErrorKind::Tls, inner.to_string())
            }
            _ => WebError::new(WebErrorKind::Handshake, inner.to_string()),
        },
        DriverError::UnsupportedAlpn(protocol) => WebError::new(
            WebErrorKind::Other,
            match protocol {
                Some(value) => format!("对端选了不支持的 ALPN 协议：{value}"),
                None => "对端没有协商 ALPN，无法确定协议。".to_string(),
            },
        ),
        other => WebError::new(WebErrorKind::Other, other.to_string()),
    }
}

/// 这类失败才真的是「证书不可信」，`insecure=true` 有机会绕过。
fn is_certificate_problem(error: &VerifyError) -> bool {
    matches!(
        error,
        VerifyError::UntrustedCertificate(_)
            | VerifyError::WrongHostName(_)
            | VerifyError::CertificateNotValidYet(_)
            | VerifyError::UnsupportedSignatureScheme(_)
            | VerifyError::BadSignature
    )
}

impl WebTransport for TlsWebTransport {
    fn send(&self, request: &WebRequest) -> Result<WebResponse, WebError> {
        let family = request
            .impersonate
            .clone()
            .unwrap_or_else(|| DEFAULT_IMPERSONATE.to_string());

        let mut current = request.url.clone();
        let mut redirects = 0u32;
        let started = Instant::now();
        // 每一跳的发起方：首跳沿用调用方给的（通常 `None`），重定向后变成上一跳地址。
        let mut previous: Option<String> = request.initiator.clone();
        // `Referer` 策略随响应头逐跳收窄，起点是请求带来的值（默认 strict-origin-when-cross-origin）。
        let mut policy = request.referrer_policy;
        // 304 但本地没有副本时，剔掉那个键并重来一次；只重试一次，避免与服务端互相踢球。
        let mut retried_without_copy = false;
        loop {
            // HSTS：声明过的站点不再走明文，第一跳与重定向目标都要判。
            // 升级后握手失败就如实报错，绝不回退明文（回退会把降级攻击变成默认行为）。
            if let Some(upgraded) = super::hsts::upgrade(&current) {
                current = upgraded;
            }
            let target = parse_target(&current)?;
            if target.scheme == "http" {
                // 明文没有 TLS 指纹可言：直接在当前线程跑 ureq，但把 URL 换成当前跳。
                let mut hop = request.clone();
                hop.url = current.clone();
                hop.initiator = previous.clone();
                hop.referrer_policy = policy;
                hop.impersonate = None;
                return self.plain.send(&hop);
            }

            // 证书校验：`insecure=true` 时用 `Inspect`（只验签名不验链），否则验链。
            let trust = if request.insecure {
                None
            } else {
                Some(TrustAnchors::webpki_roots())
            };
            let remaining = request.timeout.saturating_sub(started.elapsed());
            if remaining.is_zero() {
                return Err(WebError::new(
                    WebErrorKind::Timeout,
                    "请求超时。".to_string(),
                ));
            }
            let mut hop = request.clone();
            hop.timeout = remaining;
            // URL 也跟着当前跳走：`send_once` 要用它算这一跳该带哪些 cookie。
            hop.url = current.clone();
            // `SameSite` 需要知道这一跳是谁发起的（首跳没有发起方）。
            hop.initiator = previous.clone();
            hop.referrer_policy = policy;
            // 键必须在发送**前**定下：这一响应可能带 `Set-Cookie`，收割之后罐里就变了，
            // 而 `cache_key_for` 含 Cookie——用收割后的状态当键会与查时用的键对不上。
            let hop_key = super::web_transport::cache_key_for(&hop);
            let (response, _protocol) = send_once(&hop, &target, &family, trust)?;
            // 响应头里的 `Referrer-Policy` 只收窄不收窄放宽，逐跳累积（每一跳都可能再收一次）。
            let hop_headers: Vec<(String, String)> = response
                .headers
                .iter()
                .map(|header| (header.name.clone(), header.value.clone()))
                .collect();
            // 从这一响应学习 HSTS（只认 https 来源，见 `hsts` 模块）。
            super::hsts::record(&current, &hop_headers);
            policy = referrer::policy_from_response(policy, &hop_headers);
            // 收割这一跳的 cookie：重定向链上的 `Set-Cookie` 必须立即生效。
            cookies::harvest(
                request.cookie_jar.as_deref(),
                &current,
                response
                    .headers
                    .iter()
                    .filter(|header| header.name.eq_ignore_ascii_case("set-cookie"))
                    .map(|header| header.value.as_str()),
            );

            if request.method == "GET" && REDIRECT_STATUSES.contains(&response.status) {
                // 声明可能出现在重定向响应上，先记下来再跟下一跳。
                super::client_hints::record(
                    &current,
                    &response
                        .headers
                        .iter()
                        .map(|header| (header.name.clone(), header.value.clone()))
                        .collect::<Vec<_>>(),
                );
                if let Some(location) = response.header("location") {
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
            let content_encoding = response.header("content-encoding").map(str::to_string);
            // 响应头按到达顺序原样带出：`accept-ch` 协商与 `content-type` 判定都靠它。
            //
            // HTTP/2 的伪头（`:status`）必须在这里滤掉：它不是响应头，状态码另有字段承载。
            // 漏出去会污染 `WebResponse::headers`——实测中它会让 304 的还原结果看起来
            // 还带着 `:status: 304`，与返回的 200 自相矛盾。
            let headers: Vec<(String, String)> = response
                .headers
                .iter()
                .filter(|header| !header.name.starts_with(':'))
                .map(|header| (header.name.to_lowercase(), header.value.clone()))
                .collect();
            // 304：服务端说「你手上那份还有效」。正文在缓存副本里，原样交出去只会让上层
            // 拿到一个空页面，因此这里用副本还原成 200（见 `http_cache` 模块文档）。
            // 只有 GET 会带条件头，也只有 GET 有可还原的副本：别的方法收到 304 时如实上报，
            // 绝不拿一份 GET 的正文冒充它的结果。
            if response.status == 304 && request.method == "GET" {
                if let Some(reused) = super::http_cache::reuse(&hop_key, &headers) {
                    super::client_hints::record(&current, &reused.headers);
                    return Ok(WebResponse {
                        status: 200,
                        // 复用同一个 `location` 变量语义：最终地址就是当前跳的 URL。
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
            let body = decode_content(response.body, content_encoding.as_deref())?;
            // 存的是已解码正文：存压缩体会让 304 还原出来的是二进制。
            super::http_cache::store(&hop_key, response.status, &body, &headers);
            // 记住这一响应对该 origin 的 Client Hints 声明，供后续请求补头。
            super::client_hints::record(&current, &headers);
            return Ok(WebResponse {
                status: response.status,
                // 复用同一个 `location` 变量语义：最终地址就是当前跳的 URL。
                final_url: current,
                body,
                headers,
            });
        }
    }

    /// 按族给出整套浏览器头。
    ///
    /// 与 wreq 版本的差别：那时档案头由客户端注入、请求头会覆盖默认头，所以传输层必须
    /// **少写**。现在指纹传输不注入任何头，整套导航头都得由这里给全——少一个就不是
    /// 浏览器了。
    fn navigation_headers(
        &self,
        _user_agent: &str,
        impersonate: Option<&str>,
    ) -> Vec<(String, String)> {
        // 调用方给的 `user_agent` 会被档案族自己的 UA 覆盖：指纹要求 UA 与 TLS 档案同源。
        family_headers(impersonate.unwrap_or(DEFAULT_IMPERSONATE))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn target(url: &str) -> Result<Target, WebError> {
        parse_target(url)
    }

    #[test]
    fn urls_are_split_into_scheme_host_port_and_path() {
        let parsed = target("https://example.com/a/b?q=1").expect("应当解析");
        assert_eq!(parsed.scheme, "https");
        assert_eq!(parsed.host, "example.com");
        assert_eq!(parsed.port, 443);
        assert_eq!(parsed.path, "/a/b?q=1");

        // 默认端口按协议走。
        assert_eq!(target("http://example.com").expect("解析").port, 80);
        assert_eq!(target("https://example.com").expect("解析").path, "/");
        // 显式端口优先。
        let parsed = target("https://example.com:8443/x").expect("解析");
        assert_eq!(parsed.port, 8443);
        assert_eq!(parsed.path, "/x");
        // 协议大小写不敏感。
        assert_eq!(
            target("HTTPS://example.com/").expect("解析").scheme,
            "https"
        );
    }

    #[test]
    fn fragments_and_userinfo_do_not_leak_into_the_request() {
        assert_eq!(
            target("https://example.com/a#frag").expect("解析").path,
            "/a"
        );
        // userinfo 不能进主机名，否则会去连一个叫 `user` 的主机。
        let parsed = target("https://user:pass@example.com/p").expect("解析");
        assert_eq!(parsed.host, "example.com");
        assert_eq!(parsed.path, "/p");
    }

    #[test]
    fn a_query_without_a_path_stays_a_query() {
        assert_eq!(
            target("https://example.com?q=1").expect("解析").path,
            "?q=1",
            "没有路径时 `?` 之后仍是查询串"
        );
    }

    #[test]
    fn malformed_urls_are_rejected_with_a_reason() {
        for (url, needle) in [
            ("example.com/a", "缺少协议前缀"),
            ("ftp://example.com/", "不支持的协议"),
            ("https://", "缺少主机名"),
            ("https://example.com:不是端口/", "端口非法"),
        ] {
            let error = target(url).expect_err(url);
            assert!(
                error.message.contains(needle),
                "{url} → {}（期望包含 {needle}）",
                error.message
            );
        }
    }

    /// 高熵提示必须「服务端声明之后才发」：这是与真实浏览器一致的关键。
    ///
    /// 只测协商状态不够——要断言最终**发出去的头集合**里到底有没有它。
    #[test]
    fn negotiated_hints_reach_the_wire_only_after_accept_ch() {
        // 协商状态是进程级全局的：与其它用例并发时必须串行，否则 reset 会互相清空。
        let _serial = super::super::client_hints::test_guard();
        super::super::client_hints::reset();
        let url = "https://hints-wire.example.com/page";
        let base = || to_hpack_headers(&family_headers("chrome"));

        // 未声明：最终头集合里不得出现高熵提示。
        let before = apply_negotiated_hints(base(), url, "chrome");
        let names: Vec<&str> = before.iter().map(|header| header.name.as_str()).collect();
        assert!(
            !names.contains(&"sec-ch-ua-platform-version"),
            "未被要求的平台版本不得预先发送：{names:?}"
        );

        // 声明之后：必须带上，且值与档案一致、格式带引号。
        super::super::client_hints::record(
            url,
            &[(
                "accept-ch".to_string(),
                "Sec-CH-UA-Platform-Version".to_string(),
            )],
        );
        let after = apply_negotiated_hints(base(), url, "chrome");
        let value = after
            .iter()
            .find(|header| header.name == "sec-ch-ua-platform-version")
            .map(|header| header.value.clone())
            .expect("声明后必须发送平台版本");
        assert!(value.starts_with('"') && value.ends_with('"'), "{value}");
        // 同一族里平台与品牌与该档案的其它头必须自洽。
        let platform = after
            .iter()
            .find(|header| header.name == "sec-ch-ua-platform")
            .map(|header| header.value.clone());
        assert_eq!(platform.as_deref(), Some("\"Windows\""));

        super::super::client_hints::reset();
    }

    /// 逐跳头必须被丢掉：h2 会因它们直接判协议错误，1.1 由驱动层自己补。
    ///
    /// 但 `accept-encoding` **不是**逐跳头：它由档案族给出、由 `decode_content` 解压，
    /// 必须原样送到对端，否则服务端收不到压缩偏好（与浏览器不一致）。
    #[test]
    fn hop_by_hop_headers_are_dropped_before_encoding() {
        let headers = vec![
            ("User-Agent".to_string(), "UA".to_string()),
            ("Host".to_string(), "example.com".to_string()),
            ("Connection".to_string(), "keep-alive".to_string()),
            ("Transfer-Encoding".to_string(), "chunked".to_string()),
            (
                "Accept-Encoding".to_string(),
                "gzip, deflate, br".to_string(),
            ),
            ("Upgrade".to_string(), "h2c".to_string()),
            ("Accept".to_string(), "*/*".to_string()),
        ];
        let kept = to_hpack_headers(&headers);
        let names: Vec<&str> = kept.iter().map(|header| header.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["user-agent", "accept-encoding", "accept"],
            "名字必须转小写且滤掉逐跳头（accept-encoding 保留）"
        );
        assert_eq!(
            kept.iter()
                .find(|header| header.name == "accept-encoding")
                .map(|header| header.value.as_str()),
            Some("gzip, deflate, br"),
            "压缩偏好必须原样送达"
        );
    }

    #[test]
    fn impersonate_normalization_mirrors_python() {
        // 大小写与前缀（curl_cffi 支持 chrome124 这类带版本后缀的写法）。
        assert_eq!(normalize_impersonate(Some(&json!("chrome124"))), "chrome");
        assert_eq!(normalize_impersonate(Some(&json!("Chrome"))), "chrome");
        assert_eq!(normalize_impersonate(Some(&json!(" FIREFOX "))), "firefox");
        assert_eq!(normalize_impersonate(Some(&json!("firefox133"))), "firefox");
        assert_eq!(normalize_impersonate(Some(&json!("safari"))), "safari");
        assert_eq!(normalize_impersonate(Some(&json!("edge"))), "edge");
        // 未知值、空值、缺失与 Python 假值一律回退 chrome。
        assert_eq!(normalize_impersonate(Some(&json!("curl"))), "chrome");
        assert_eq!(normalize_impersonate(Some(&json!(""))), "chrome");
        assert_eq!(normalize_impersonate(Some(&json!("   "))), "chrome");
        assert_eq!(normalize_impersonate(None), "chrome");
        assert_eq!(normalize_impersonate(Some(&Value::Null)), "chrome");
        assert_eq!(normalize_impersonate(Some(&json!(0))), "chrome");
        assert_eq!(normalize_impersonate(Some(&json!(false))), "chrome");
        assert_eq!(normalize_impersonate(Some(&json!(1))), "chrome");
        assert_eq!(normalize_impersonate(Some(&json!(true))), "chrome");
    }

    /// 头的版本必须与 TLS 档案同族：上一版 wreq 通路踩过这个坑
    /// （TLS 是 Chrome149、头里却写 Chrome/124）。
    #[test]
    fn each_family_supplies_a_consistent_browser_header_set() {
        for family in ["chrome", "firefox", "safari", "edge"] {
            let headers = family_headers(family);
            let value = |name: &str| {
                headers
                    .iter()
                    .find(|(key, _)| key == name)
                    .map(|(_, value)| value.as_str())
                    .unwrap_or_default()
                    .to_string()
            };
            for required in ["user-agent", "accept", "accept-language"] {
                assert!(
                    !value(required).is_empty(),
                    "{family} 缺少 {required}：{headers:?}"
                );
            }
            // `sec-fetch-*` 不在这里出现：它们描述「这次请求在干什么」，由 `send_once`
            // 按发起方与用途算（见 `fetch_metadata`）。族头像里若还留着固定值，
            // 跨站重定向就会被描述成「地址栏直接输入」。
            assert_eq!(
                value("sec-fetch-dest"),
                "",
                "{family} 不该在族头像里写死 sec-fetch-dest：{headers:?}"
            );
            assert_eq!(value("upgrade-insecure-requests"), "1", "{family}");
            // h2 禁止连接专有头。
            assert_eq!(value("connection"), "", "{family}");
            // `accept-encoding` 必须发（浏览器必发），且只声明本实现真能解压的编码：
            // 声明 zstd 而解不开，等于让服务端回一个处理不了的编码。
            let encoding = value("accept-encoding");
            assert!(!encoding.is_empty(), "{family} 应当声明 accept-encoding");
            for token in encoding.split(',').map(str::trim) {
                assert!(
                    matches!(token, "gzip" | "deflate" | "br"),
                    "{family} 声明了本实现不支持的编码：{token}"
                );
            }
            assert!(!encoding.contains("zstd"), "{family} 不得声明 zstd");
        }

        // 只有 Chromium 系发 Client Hints。
        let chrome = family_headers("chrome");
        assert!(chrome.iter().any(|(key, _)| key == "sec-ch-ua"));
        assert_eq!(
            chrome
                .iter()
                .find(|(key, _)| key == "sec-ch-ua-platform")
                .map(|(_, value)| value.as_str()),
            Some("\"Windows\"")
        );
        assert!(
            !family_headers("firefox")
                .iter()
                .any(|(key, _)| key.starts_with("sec-ch-ua")),
            "Firefox 不发 Client Hints"
        );

        // Safari 不发 Sec-Fetch-User（其余族都发）。这个差异现在由族身份上的 `fetch_user`
        // 标记表达，由 `send_once` 在算 `sec-fetch-*` 时读取（族头像里不再有该头）。
        assert!(
            !identity_for("safari").fetch_user,
            "Safari 不该发 Sec-Fetch-User"
        );
        for family in ["chrome", "firefox", "edge"] {
            assert!(
                identity_for(family).fetch_user,
                "{family} 应当发 Sec-Fetch-User"
            );
        }

        // UA 里的版本必须与 TLS 档案同源。
        assert!(family_headers("chrome")
            .iter()
            .any(|(_, value)| value.contains("Chrome/145")));
        assert!(family_headers("edge")
            .iter()
            .any(|(_, value)| value.contains("Edg/145")));
        assert!(family_headers("firefox")
            .iter()
            .any(|(_, value)| value.contains("Firefox/151")));
        assert!(family_headers("safari")
            .iter()
            .any(|(_, value)| value.contains("Version/26")));
    }

    #[test]
    fn a_family_name_maps_to_the_matching_tls_family() {
        assert_eq!(family_for("chrome"), Family::Chrome);
        assert_eq!(family_for("edge"), Family::Edge);
        assert_eq!(family_for("firefox"), Family::Firefox);
        assert_eq!(family_for("safari"), Family::Safari);
        // 未知族按 chrome 兜底，与 `normalize_impersonate` 一致。
        assert_eq!(family_for("unknown"), Family::Chrome);
    }
}
