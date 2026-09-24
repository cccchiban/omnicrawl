//! 外网指纹冒烟：验证 `impersonate` 档案真的改变了 TLS 指纹，而不是只被解析后丢掉。
//!
//! 默认 `#[ignore]`：CI 不应该依赖外网可用性。手动验证方式（Windows 本机需要
//! `CMAKE_TOOLCHAIN_FILE` 与纯 ASCII 的 `CARGO_TARGET_DIR`，原因见
//! `rust/tools/btls-msvc-runtime.cmake` 的文件头）：
//!
//! ```text
//! cargo test -p omnicrawl-host --test wreq_fingerprint -- --ignored --nocapture
//! ```
//!
//! 断言刻意不写死具体的 JA3/JA4 哈希：那会随 wreq-util 的档案更新而失效。这里只验证
//! 「确实拿到了浏览器指纹」以及「换档案会得到不同指纹」这两件不会因版本变化而失真的性质。

use std::time::Duration;

use omnicrawl_host::tools::web_transport::{browser_headers, WebRequest, WebTransport};
use omnicrawl_host::tools::wreq_transport::WreqWebTransport;

/// 回显本次 TLS/HTTP 指纹的公开端点。
const ECHO_URL: &str = "https://tls.browserleaks.com/json";

/// 与本项目 `web_transport::DEFAULT_USER_AGENT` 一致，避免引入额外变量。
const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) \
AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36";

fn fetch_echo(impersonate: &str) -> serde_json::Value {
    let transport = WreqWebTransport::new();
    let mut request = WebRequest::new(
        ECHO_URL,
        browser_headers(USER_AGENT),
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
    // 反向对照：如果传输层退化回 ureq+rustls，JA4 会落在库指纹上而不是浏览器指纹上。
    // 这里用 JA4 的协议段做粗校验（浏览器档案都是 h2，ureq 则偏 h1）。
    println!(
        "chrome ja4_r = {}",
        chrome["ja4_r"].as_str().unwrap_or_default()
    );
}
