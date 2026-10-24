//! 真实站点连通性验证。
//!
//! 这些用例**默认不跑**：它们依赖外网、DNS、以及目标站点当下的行为，
//! 放进常规测试会让 CI 随机失败。用 `--ignored` 手动执行：
//!
//! ```bash
//! cargo test -p omnicrawl-tls --test live_sites -- --ignored --nocapture
//! ```
//!
//! 目的不是「跑通就行」，而是让前六层在真实世界里过一次：
//! 服务端会用它自己的密钥交换、自己的证书链、自己的 HPACK 动态表、
//! 自己的流量控制策略——这些都不是测试里能模拟出来的。

use std::time::Duration;

use omnicrawl_tls::driver::{Connection, DriverError};
use omnicrawl_tls::profile::Family;

const TIMEOUT: Duration = Duration::from_secs(15);

/// 只验 CertificateVerify 签名、不验证书链（`PeerPolicy::Inspect`）。
///
/// 这里刻意不用 `VerifyChain`：那需要一份受信根集合，而本 crate 不内置根证书。
/// 签名校验已经能证明「对端持有叶证书私钥」，足以验证协议栈的正确性。
fn connect(host: &str, family: Family) -> Connection<std::net::TcpStream> {
    Connection::connect_to(host, 443, family, None, TIMEOUT)
        .unwrap_or_else(|error| panic!("连接 {host} 失败：{error}"))
}

fn run_get(host: &str, path: &str, family: Family) -> u16 {
    let mut connection = connect(host, family);
    assert_eq!(connection.alpn(), Some("h2"), "{host} 应当协商出 h2");
    let response = connection
        .get(path)
        .unwrap_or_else(|error| panic!("向 {host}{path} 发请求失败：{error}"));
    response.status
}

#[test]
#[ignore = "需要外网"]
fn cloudflare_answers_a_get_over_h2() {
    let status = run_get("cloudflare.com", "/", Family::Chrome);
    assert!((200..500).contains(&status), "意外的状态码 {status}");
}

#[test]
#[ignore = "需要外网"]
fn example_com_answers_a_get_over_h2() {
    // example.com 是 IANA 的保留域名，页面极小，适合验证最基础的通路。
    let mut connection = connect("example.com", Family::Chrome);
    let response = connection.get("/").expect("请求应当成功");
    assert_eq!(response.status, 200, "example.com 应返回 200");
    assert!(
        !response.body.is_empty(),
        "响应体不该为空（长度 {}）",
        response.body.len()
    );
    // 内容里应当出现 IANA 的标记，确认拿到的是真页面而不是错误页。
    let text = String::from_utf8_lossy(&response.body);
    assert!(
        text.to_lowercase().contains("example"),
        "响应内容看起来不对：{}",
        &text.chars().take(200).collect::<String>()
    );
}

#[test]
#[ignore = "需要外网"]
fn a_larger_site_exercises_multiple_records_and_frames() {
    // 找一个首页体积较大的站点，迫使响应跨越多个 TLS 记录与多个 DATA 帧。
    // 用维基百科的主页而非条目页：条目路径会随条目改名而 301，主页不会。
    let mut connection = connect("www.wikipedia.org", Family::Chrome);
    let response = connection.get("/").expect("请求");
    assert_eq!(response.status, 200, "主页应当直接返回 200");
    assert!(
        response.body.len() > 5_000,
        "预期一个较大的页面，实际 {} 字节",
        response.body.len()
    );
    assert_eq!(
        response
            .header("content-type")
            .map(|value| value.contains("text/html")),
        Some(true),
        "应当是 HTML 页面"
    );
}

#[test]
#[ignore = "需要外网"]
fn a_redirect_is_reported_as_a_status_and_location() {
    // 3xx 是「成功拿到了响应」而不是错误：驱动层不跟随重定向，如实报出来。
    let mut connection = connect("www.wikipedia.org", Family::Chrome);
    let response = connection
        .get("/wiki/Transport_Layer_Security")
        .expect("请求应当完成");
    assert!(
        (200..400).contains(&response.status),
        "预期 2xx/3xx，实际 {}",
        response.status
    );
    if response.status == 301 || response.status == 302 {
        assert!(
            response.header("location").is_some(),
            "3xx 必须带 location 头"
        );
    }
}

#[test]
#[ignore = "需要外网"]
fn every_family_can_complete_a_handshake_against_a_real_server() {
    // 四份档案都要能真的握手成功。这一步同时验证了档案的结构合法性与
    // 服务端对我们这套扩展排列的接受度。
    for family in [
        Family::Chrome,
        Family::Firefox,
        Family::Safari,
        Family::Edge,
    ] {
        let mut connection = Connection::connect_to("example.com", 443, family, None, TIMEOUT)
            .unwrap_or_else(|error| panic!("{} 家族握手失败：{error}", family.as_str()));
        assert_eq!(
            connection.alpn(),
            Some("h2"),
            "{} 家族应当协商出 h2",
            family.as_str()
        );
        let response = connection
            .get("/")
            .unwrap_or_else(|error| panic!("{} 家族请求失败：{error}", family.as_str()));
        assert_eq!(
            response.status,
            200,
            "{} 家族拿到状态码 {}",
            family.as_str(),
            response.status
        );
    }
}

#[test]
#[ignore = "需要外网"]
fn a_second_request_on_the_same_connection_reuses_the_session() {
    let mut connection = connect("example.com", Family::Chrome);
    let first = connection.get("/").expect("第一个请求");
    assert_eq!(first.status, 200);
    let second = connection.get("/").expect("第二个请求");
    assert_eq!(second.status, 200);
    // 复用连接时第二个响应不该比第一个大出太多（HPACK 动态表命中）。
    assert!(
        second.body.len() == first.body.len(),
        "同一页面两次请求的响应体长度应当一致：{} vs {}",
        first.body.len(),
        second.body.len()
    );
}

#[test]
#[ignore = "需要外网"]
fn a_404_is_reported_as_a_status_not_an_error() {
    // 关键的语义检查：4xx 是「成功拿到了响应」，不是传输失败。
    let mut connection = connect("example.com", Family::Firefox);
    let response = connection
        .get("/this-path-does-not-exist-omnicrawl")
        .expect("请求应当完成");
    assert!(
        (400..500).contains(&response.status),
        "预期 4xx，实际 {}",
        response.status
    );
}

#[test]
#[ignore = "需要外网"]
fn a_head_request_returns_headers_without_a_body() {
    let mut connection = connect("example.com", Family::Chrome);
    let response = connection
        .request("HEAD", "/", None)
        .expect("HEAD 请求应当完成");
    assert_eq!(response.status, 200);
    assert!(
        response.body.is_empty(),
        "HEAD 不该有响应体，实际 {} 字节",
        response.body.len()
    );
}

#[test]
#[ignore = "需要外网"]
fn an_unreachable_host_reports_an_error_instead_of_hanging() {
    // 超时必须真的生效：否则这个测试会永久挂住而不是失败。
    let result = Connection::connect_to(
        "no-such-host-omnicrawl-test.invalid",
        443,
        Family::Chrome,
        None,
        Duration::from_secs(3),
    );
    match result {
        Err(DriverError::Io(_)) | Err(DriverError::BadResponse(_)) => {}
        Err(other) => panic!("应当是 IO 或解析错误，得到 {other:?}"),
        Ok(_) => panic!("不存在的域名不该连上"),
    }
}

#[test]
#[ignore = "需要外网"]
fn a_tls12_only_site_completes_a_handshake_and_serves_http11() {
    // 百度只支持到 TLS 1.2。这一条曾经断言「必须失败」并只检查报错文案，因为当时
    // 实现只有 1.3；现在 1.2 已经支持，因此改为断言真实可用：
    // 握手走完、版本是 1.2、并且能取到页面。
    let mut connection =
        Connection::connect_to("www.baidu.com", 443, Family::Chrome, None, TIMEOUT)
            .unwrap_or_else(|error| panic!("百度应当能用 TLS 1.2 握手：{error}"));
    assert_eq!(
        connection.version(),
        Some(omnicrawl_tls::session::Version::Tls12),
        "百度是 1.2-only 站点，协商结果必须是 1.2"
    );
    let response = connection.get("/").expect("请求应当完成");
    assert!(
        (200..400).contains(&response.status),
        "意外的状态码 {}",
        response.status
    );
    assert!(!response.body.is_empty(), "响应体不该为空");
}

#[test]
#[ignore = "需要外网"]
fn a_second_tls12_only_site_also_works() {
    // 换一家不同的服务端实现：QQ 主站同样只到 1.2，且过去被判为「回落」失败。
    let mut connection = Connection::connect_to("www.qq.com", 443, Family::Chrome, None, TIMEOUT)
        .unwrap_or_else(|error| panic!("qq.com 应当能用 TLS 1.2 握手：{error}"));
    assert_eq!(
        connection.version(),
        Some(omnicrawl_tls::session::Version::Tls12),
        "qq.com 的协商结果应当是 1.2"
    );
    let response = connection.get("/").expect("请求应当完成");
    assert!(
        (200..400).contains(&response.status),
        "意外的状态码 {}",
        response.status
    );
}

#[test]
#[ignore = "需要外网"]
fn a_hello_retry_request_for_secp256r1_completes_the_handshake() {
    // gitee 的 key_share 只认 P-256：服务端会发 HelloRetryRequest 要求换组。
    // 修复前这里会直接报「只支持 X25519」，修复后应当能一路走到 get。
    let mut connection = Connection::connect_to("gitee.com", 443, Family::Chrome, None, TIMEOUT)
        .unwrap_or_else(|error| panic!("gitee.com 握手失败：{error}"));
    let response = connection.get("/").expect("请求应当完成");
    assert!(
        (200..400).contains(&response.status),
        "意外的状态码 {}",
        response.status
    );
    assert!(!response.body.is_empty(), "响应体不该为空");
}
