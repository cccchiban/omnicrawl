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
//! | 内容压缩 | 不声明 `accept-encoding` | 本栈不解压，声明了会拿到二进制 |
//! | HTTP/2 设置 | 只发 MAX_FRAME_SIZE 与 INITIAL_WINDOW_SIZE | 不发 `ENABLE_PUSH=0`（nginx 会 GOAWAY） |
//! | 连接专有头 | 一律不发 | h2 禁止；1.1 通路自动补 `Connection: close` |
//! | 明文 http:// | 走 ureq 分支 | 自研栈只做 TLS，明文没有指纹可模拟 |
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
use omnicrawl_tls::verify::TrustAnchors;
use serde_json::Value;

use super::web_transport::{
    resolve_url, UreqWebTransport, WebError, WebErrorKind, WebRequest, WebResponse, WebTransport,
    REDIRECT_STATUSES,
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
        },
        "safari" => FamilyIdentity {
            user_agent: "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) \
AppleWebKit/605.1.15 (KHTML, like Gecko) Version/26.0 Safari/605.1.15",
            client_hints: None,
            platform: None,
            // Safari 不发 Sec-Fetch-User（与上一版记录一致）。
            fetch_user: false,
            accept: "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
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
    headers.push(("sec-fetch-dest".to_string(), "document".to_string()));
    headers.push(("sec-fetch-mode".to_string(), "navigate".to_string()));
    headers.push(("sec-fetch-site".to_string(), "none".to_string()));
    if identity.fetch_user {
        headers.push(("sec-fetch-user".to_string(), "?1".to_string()));
    }
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
    let mut connection = match request.proxy.as_deref() {
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
    .map_err(|error| classify_driver(&error))?;

    let headers = to_hpack_headers(&request.headers);
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

fn to_hpack_headers(headers: &[(String, String)]) -> Vec<Header> {
    headers
        .iter()
        // 逐跳与专有头在两条通路上都不该由调用方给：h2 禁止它们，1.1 由驱动层自己补。
        .filter(|(name, _)| {
            !matches!(
                name.to_lowercase().as_str(),
                "host"
                    | "connection"
                    | "keep-alive"
                    | "proxy-connection"
                    | "transfer-encoding"
                    | "upgrade"
                    | "accept-encoding"
            )
        })
        .map(|(name, value)| Header::new(name.to_lowercase(), value))
        .collect()
}

/// 把驱动层错误映射到内核统一的分类。
fn classify_driver(error: &DriverError) -> WebError {
    match error {
        DriverError::Io(io) => match io.kind() {
            std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock => {
                WebError::new(WebErrorKind::Timeout, "请求超时。".to_string())
            }
            _ => WebError::new(WebErrorKind::Connect, io.to_string()),
        },
        DriverError::Handshake(inner) => WebError::new(WebErrorKind::Tls, inner.to_string()),
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

impl WebTransport for TlsWebTransport {
    fn send(&self, request: &WebRequest) -> Result<WebResponse, WebError> {
        let family = request
            .impersonate
            .clone()
            .unwrap_or_else(|| DEFAULT_IMPERSONATE.to_string());

        let mut current = request.url.clone();
        let mut redirects = 0u32;
        let started = Instant::now();
        loop {
            let target = parse_target(&current)?;
            if target.scheme == "http" {
                // 明文没有 TLS 指纹可言：直接在当前线程跑 ureq，但把 URL 换成当前跳。
                let mut hop = request.clone();
                hop.url = current.clone();
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
            let (response, _protocol) = send_once(&hop, &target, &family, trust)?;

            if request.method == "GET" && REDIRECT_STATUSES.contains(&response.status) {
                if let Some(location) = response.header("location") {
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
                // 复用同一个 `location` 变量语义：最终地址就是当前跳的 URL。
                final_url: current,
                body: response.body,
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

    /// 逐跳头必须被丢掉：h2 会因它们直接判协议错误，1.1 由驱动层自己补。
    #[test]
    fn hop_by_hop_headers_are_dropped_before_encoding() {
        let headers = vec![
            ("User-Agent".to_string(), "UA".to_string()),
            ("Host".to_string(), "example.com".to_string()),
            ("Connection".to_string(), "keep-alive".to_string()),
            ("Transfer-Encoding".to_string(), "chunked".to_string()),
            ("Accept-Encoding".to_string(), "gzip".to_string()),
            ("Upgrade".to_string(), "h2c".to_string()),
            ("Accept".to_string(), "*/*".to_string()),
        ];
        let kept = to_hpack_headers(&headers);
        let names: Vec<&str> = kept.iter().map(|header| header.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["user-agent", "accept"],
            "名字必须转小写且滤掉逐跳头"
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
            for required in [
                "user-agent",
                "accept",
                "accept-language",
                "sec-fetch-dest",
                "sec-fetch-mode",
                "sec-fetch-site",
            ] {
                assert!(
                    !value(required).is_empty(),
                    "{family} 缺少 {required}：{headers:?}"
                );
            }
            assert_eq!(value("sec-fetch-dest"), "document", "{family}");
            assert_eq!(value("sec-fetch-mode"), "navigate", "{family}");
            assert_eq!(value("sec-fetch-site"), "none", "{family}");
            assert_eq!(value("upgrade-insecure-requests"), "1", "{family}");
            // 绝不发这两个：h2 禁止 Connection；Accept-Encoding 一发就得能解压。
            assert_eq!(value("connection"), "", "{family}");
            assert_eq!(value("accept-encoding"), "", "{family}");
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

        // Safari 不发 Sec-Fetch-User（其余族都发）。
        assert!(!family_headers("safari")
            .iter()
            .any(|(key, _)| key == "sec-fetch-user"));
        for family in ["chrome", "firefox", "edge"] {
            assert!(
                family_headers(family)
                    .iter()
                    .any(|(key, _)| key == "sec-fetch-user"),
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
