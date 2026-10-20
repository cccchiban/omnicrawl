//! 真实站点上的缓存协商与 HSTS 验证（默认 `#[ignore]`，依赖外网）。
//!
//! ```text
//! cargo test -p omnicrawl-host --test web_cache_hsts_live -- --ignored --nocapture --test-threads=1
//! ```

use std::sync::Arc;
use std::time::Duration;

use omnicrawl_host::tools::cookies::CookieJar;
use omnicrawl_host::tools::hsts;
use omnicrawl_host::tools::http_cache;
use omnicrawl_host::tools::tls_transport::TlsWebTransport;
use omnicrawl_host::tools::web_transport::{cache_key_for, WebRequest, WebTransport};

fn request(url: &str, extra: Vec<(String, String)>) -> WebRequest {
    let mut headers = vec![("user-agent".to_string(), "Mozilla/5.0 probe".to_string())];
    headers.extend(extra);
    let mut request = WebRequest::new(url, headers, Duration::from_secs(25));
    request.cookie_jar = Some(Arc::new(CookieJar::new()));
    request.impersonate = Some("chrome".to_string());
    request
}

/// 真实站点上的协商闭环：第二次请求带上验证器，304 时正文被还原。
///
/// 判据不能只看状态码（还原后也是 200）。关键区分在这里：只带 `If-None-Match` 时
/// 服务端确实回 304 且**正文为空**；若走缓存后第二次仍拿到完整正文，那就只能是还原路径。
#[test]
#[ignore = "需要外网"]
fn a_real_site_restores_the_body_from_a_304() {
    let transport = TlsWebTransport::new();
    let url = "https://httpbin.org/etag/abc";

    // 先证明这个服务端支持条件请求：手工只带验证器必须拿到 304。
    let mut plain = request(url, Vec::new());
    plain.url = url.to_string();
    let first = transport.send(&plain).expect("首次抓取");
    assert_eq!(first.status, 200, "首次应当 200");
    let etag = first
        .header("etag")
        .expect("该地址应当给 ETag")
        .to_string();
    // 手工带验证器、且不经过缓存：传输层仍会走还原分支，但没有副本，
    // 因此必须如实报错而不是伪装成功。
    let probe = transport.send(&request(
        url,
        vec![("if-none-match".to_string(), etag.clone())],
    ));
    match probe {
        Err(error) => assert!(
            error.message.contains("304"),
            "应当是 304 还原失败：{}",
            error.message
        ),
        // 服务端若因为首次已写入缓存而回 200，则正文必须来自还原而不是重发。
        Ok(response) => assert_eq!(response.status, 200),
    }

    // 走缓存：第二次由传输层自动带验证器、自动还原。
    // 用独立的地址避免与其它用例共享进程级缓存状态。
    let url = "https://httpbin.org/etag/probe";
    let stored = transport.send(&request(url, Vec::new())).expect("写入缓存");
    assert_eq!(stored.status, 200);
    let restored = transport
        .send(&request(url, Vec::new()))
        .expect("第二跳应当由 304 还原");
    assert_eq!(restored.status, 200, "还原后必须报 200");
    assert_eq!(
        restored.body, stored.body,
        "还原出来的正文必须与副本一致"
    );
    assert!(!restored.body.is_empty(), "正文不能为空");

    // 键必须落在最终地址上：协商与存储共用同一套算法。
    let key = cache_key_for(&request(&stored.final_url, Vec::new()));
    let mut prepared = Vec::new();
    http_cache::apply_conditional(&key, "GET", &mut prepared);
    assert_eq!(
        prepared.iter().find(|(name, _)| name == "if-none-match"),
        Some(&("if-none-match".to_string(), "probe".to_string())),
        "最终地址的键上必须带着本次写入的验证器：{prepared:?}"
    );
    // HTTP/2 伪头不得泄漏到响应头里（它会与真实状态码自相矛盾）。
    assert!(
        !restored.headers.iter().any(|(name, _)| name.starts_with(':')),
        "响应头里不该出现伪头：{:?}",
        restored.headers
    );
}

/// 真实站点的 HSTS 声明必须被学习，且明文来源不得写入策略。
#[test]
#[ignore = "需要外网"]
fn a_real_hsts_header_is_learned_from_https_only() {
    let transport = TlsWebTransport::new();
    let url = "https://www.cloudflare.com/";
    let response = transport.send(&request(url, Vec::new())).expect("抓取");
    let declared = response.header("strict-transport-security");
    println!("{url} → strict-transport-security={declared:?}");
    assert!(declared.is_some(), "{url} 应当声明 HSTS");

    assert!(
        hsts::is_protected("www.cloudflare.com"),
        "声明过的主机必须被判为受保护"
    );
    assert_eq!(
        hsts::upgrade("http://www.cloudflare.com/x").as_deref(),
        Some("https://www.cloudflare.com/x")
    );
    // 该站声明了 includeSubDomains：`cloudflare.com` 的子域也必须升级。
    // 注意声明主机是 `www.cloudflare.com`，它的子域是 `*.www.cloudflare.com`；
    // 裸域 `cloudflare.com` 是**上级**，不在一条 `includeSubDomains` 的覆盖范围内。
    assert_eq!(
        hsts::upgrade("http://cdn.www.cloudflare.com/x").as_deref(),
        Some("https://cdn.www.cloudflare.com/x")
    );

    // 明文来源不得学习。
    hsts::record(
        "http://plaintext-probe.example.com/",
        &[(
            "strict-transport-security".to_string(),
            "max-age=31536000".to_string(),
        )],
    );
    assert!(
        !hsts::is_protected("plaintext-probe.example.com"),
        "明文响应不得写入策略"
    );
}
