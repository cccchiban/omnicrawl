//! 外网指纹冒烟：验证 `impersonate` 档案真的改变了 TLS 指纹，而不只是被解析后丢掉。
//!
//! 默认 `#[ignore]`：CI 不应该依赖外网可用性。手动验证：
//!
//! ```text
//! cargo test -p omnicrawl-host --test tls_fingerprint -- --ignored --nocapture
//! ```
//!
//! 断言刻意不写死具体的 JA3/JA4 哈希：那会随档案校准而变。这里只验证三件不会因版本
//! 变化而失真的性质：指纹存在、换族会变、HTTP 头与 TLS 档案同源。

use std::time::Duration;

use omnicrawl_host::tools::tls_transport::TlsWebTransport;
use omnicrawl_host::tools::web_transport::{WebRequest, WebTransport};

/// 回显本次 TLS/HTTP 指纹的公开端点。
const ECHO_URL: &str = "https://tls.browserleaks.com/json";

fn fetch_echo(impersonate: &str) -> serde_json::Value {
    let transport = TlsWebTransport::new();
    let mut request = WebRequest::new(
        ECHO_URL,
        transport.navigation_headers("ignored", Some(impersonate)),
        Duration::from_secs(25),
    );
    request.impersonate = Some(impersonate.to_string());
    let response = transport
        .send(&request)
        .unwrap_or_else(|error| panic!("抓取 {ECHO_URL} 失败：{}", error.message));
    assert_eq!(response.status, 200, "指纹回显端点应当返回 200");
    serde_json::from_slice(&response.body).expect("指纹回显端点应当返回 JSON")
}

#[test]
#[ignore = "需要外网：验证浏览器档案真的改变了 TLS 指纹"]
fn distinct_impersonate_profiles_yield_distinct_ja4() {
    let chrome = fetch_echo("chrome");
    let firefox = fetch_echo("firefox");
    let chrome_ja4 = chrome["ja4"].as_str().unwrap_or_default().to_string();
    let firefox_ja4 = firefox["ja4"].as_str().unwrap_or_default().to_string();
    println!("chrome  ja4 = {chrome_ja4}");
    println!("firefox ja4 = {firefox_ja4}");

    // 指纹必须存在（不是空串或解析失败）。
    assert!(!chrome_ja4.is_empty(), "chrome 档案应当产出 JA4");
    assert!(!firefox_ja4.is_empty(), "firefox 档案应当产出 JA4");
    // 两个族必须产出不同指纹——否则说明档案根本没生效（例如参数被忽略）。
    assert_ne!(
        chrome_ja4, firefox_ja4,
        "chrome 与 firefox 的 JA4 相同，说明浏览器档案没有生效"
    );
}

/// 关键的一致性检查：`sec-ch-ua` 声称的版本与 TLS 档案必须是同一个版本。
///
/// 上一版 wreq 通路记录过一次真实事故：TLS 层是 Chrome149 档案、HTTP 头却自称
/// Chrome/124，被站点当作伪造流量。这里把「HTTP 头里出现的版本号」与「UA 的版本号」
/// 对齐后一并核对。
#[test]
#[ignore = "需要外网：验证 HTTP 头与 TLS 档案同源"]
fn http_headers_agree_with_the_tls_profile() {
    let chrome = fetch_echo("chrome");
    println!("chrome = {chrome}");
    assert_eq!(
        chrome["user_agent"].as_str().unwrap_or_default(),
        omnicrawl_host::tools::tls_transport::family_headers("chrome")
            .iter()
            .find(|(name, _)| name == "user-agent")
            .map(|(_, value)| value.as_str())
            .unwrap_or_default(),
        "回显端点看到的 UA 应当就是本地给出的那一份"
    );
    // HTTP/2 指纹里的 SETTINGS 必须就是 Chromium 抓包那一组：回显端点用 akamai_text
    // 展示它（`1:65536;2:0;4:6291456;6:262144` 正是本机 Edge 145 的抓包值）。
    let akamai = chrome["akamai_text"].as_str().unwrap_or_default();
    assert!(
        akamai.starts_with("1:65536;2:0;4:6291456;6:262144"),
        "HTTP/2 SETTINGS 指纹与抓包不一致：{akamai}"
    );
    // TLS 层必须认成 1.3 + 浏览器形态的密码套件数（13 个），且 ALPN 是 h2。
    let ja4 = chrome["ja4"].as_str().unwrap_or_default();
    assert!(
        ja4.starts_with("t13d1513h2_"),
        "JA4 应当是 TLS1.3 / h2 / 13 个套件：{ja4}"
    );
}

/// 真实站点的端到端：能连上、拿到 200、响应体是页面。
#[test]
#[ignore = "需要外网"]
fn a_real_site_is_fetched_through_the_fingerprint_transport() {
    let transport = TlsWebTransport::new();
    for (url, family) in [
        ("https://example.com/", "chrome"),
        ("https://www.wikipedia.org/", "firefox"),
        ("https://cloudflare.com/", "safari"),
    ] {
        let mut request = WebRequest::new(
            url,
            transport.navigation_headers("ignored", Some(family)),
            Duration::from_secs(25),
        );
        request.impersonate = Some(family.to_string());
        let response = transport
            .send(&request)
            .unwrap_or_else(|error| panic!("{family} 抓取 {url} 失败：{}", error.message));
        assert_eq!(response.status, 200, "{family} {url}");
        assert!(!response.body.is_empty(), "{family} {url} 的响应体不该为空");
        println!("{family} {url} → {} 字节", response.body.len());
    }
}
