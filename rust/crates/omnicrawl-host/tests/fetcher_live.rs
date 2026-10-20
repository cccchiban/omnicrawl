//! 端到端：走 `fetcher` 工具执行体（默认传输就是指纹传输）抓真实站点。
//!
//! 默认 `#[ignore]`：依赖外网。手动执行：
//!
//! ```text
//! cargo test -p omnicrawl-host --test fetcher_live -- --ignored --nocapture --test-threads=1
//! ```

use serde_json::{json, Map, Value};

use omnicrawl_host::tools::fetcher::{self, FetcherOptions};

fn arguments(url: &str, impersonate: &str) -> Map<String, Value> {
    let mut map = Map::new();
    map.insert("urls".to_string(), Value::from(url));
    map.insert("parallel".to_string(), Value::from(false));
    map.insert("impersonate".to_string(), Value::from(impersonate));
    map.insert("timeout".to_string(), json!(25));
    map
}

fn options() -> FetcherOptions {
    FetcherOptions {
        // 显式禁用代理：本用例只验证直连通路。
        proxy: Some(String::new()),
        ..FetcherOptions::default()
    }
}

#[test]
#[ignore = "需要外网"]
fn the_fetcher_tool_fetches_real_pages_through_the_fingerprint_transport() {
    for (url, family) in [
        ("https://example.com/", "chrome"),
        ("https://www.wikipedia.org/", "firefox"),
        ("https://www.rust-lang.org/", "edge"),
    ] {
        let started = std::time::Instant::now();
        let output = fetcher::fetcher(&options(), &arguments(url, family))
            .unwrap_or_else(|error| panic!("{family} {url} 应当成功：{error}"));
        assert!(
            !output.contains("失败:"),
            "{family} {url} 抓取失败：\n{output}"
        );
        assert!(output.contains("状态: 200"), "{family} {url}：\n{output}");
        assert!(
            output.contains("标题:"),
            "{family} {url} 没有标题：\n{output}"
        );
        assert!(
            !output.contains("内容: \n"),
            "{family} {url} 正文为空：\n{output}"
        );
        println!(
            "{family} {url} → {:.2}s\n{}",
            started.elapsed().as_secs_f64(),
            output.lines().take(4).collect::<Vec<_>>().join("\n")
        );
    }
}

/// 只支持 http/1.1 的站点也必须能抓：ALPN 由服务端决定，缺 1.1 通路就等于丢掉半个互联网。
#[test]
#[ignore = "需要外网"]
fn a_http11_only_site_is_fetched_too() {
    let output = fetcher::fetcher(&options(), &arguments("https://www.python.org/", "chrome"))
        .unwrap_or_else(|error| panic!("python.org 应当成功：{error}"));
    println!("{output}");
    assert!(output.contains("状态: 200"), "{output}");
}

#[test]
#[ignore = "需要外网"]
fn the_fetcher_reaches_a_tls12_only_site() {
    // cnblogs.com 只支持到 TLS 1.2 且 robots.txt 全放行。这一条走的是完整的 `fetcher`
    // 工具执行体（含指纹传输、重定向、正文提取），是「1.2 回落」在真实调用链上的证据。
    //
    // 不用百度：它的 robots.txt 是 `Disallow: /`，按抓取边界就该拒绝（见 `robots` 模块）。
    let output = fetcher::fetcher(&options(), &arguments("https://www.cnblogs.com/", "chrome"))
        .unwrap_or_else(|error| panic!("cnblogs 应当能抓：{error}"));
    assert!(!output.contains("失败:"), "抓取失败：\n{output}");
    assert!(output.contains("状态: 200"), "{output}");
    assert!(!output.contains("TLS"), "不该再报 TLS 错误：\n{output}");
}

#[test]
#[ignore = "需要外网"]
fn the_fetcher_reaches_another_tls12_only_site() {
    // 换一家服务端实现，避免「恰好这一个站点能过」。
    let output = fetcher::fetcher(&options(), &arguments("https://www.qq.com/", "chrome"))
        .unwrap_or_else(|error| panic!("qq.com 应当能抓：{error}"));
    assert!(!output.contains("失败:"), "抓取失败：\n{output}");
    assert!(output.contains("状态: 200"), "{output}");
}

/// 真实站点的 cookie：`Set-Cookie` 要被收下，且在后续请求里回带回去。
///
/// 走的是默认传输（指纹栈 + h2/1.1），因此这条同时覆盖两条记录通路。
/// 断言只看「罐里有了」与「回带成功抓到了」，不假设具体站点的 cookie 名。
#[test]
#[ignore = "需要外网"]
fn cookies_from_a_real_site_are_stored_and_replayed() {
    let jar = std::sync::Arc::new(omnicrawl_host::tools::cookies::CookieJar::new());
    let options = FetcherOptions {
        proxy: Some(String::new()),
        cookie_jar: Some(jar.clone()),
        ..FetcherOptions::default()
    };
    // wikipedia.org 会下发多个 Set-Cookie，且 robots.txt 允许首页（已实机核对）。
    let output = fetcher::fetcher(&options, &arguments("https://www.wikipedia.org/", "chrome"))
        .unwrap_or_else(|error| panic!("wikipedia 应当能抓：{error}"));
    assert!(output.contains("状态: 200"), "{output}");
    assert!(
        jar.count() > 0,
        "该站会下发 Set-Cookie，罐里不该是空的：\n{output}"
    );

    // 再抓一次同一站点：这次请求必须带上罐里的 cookie。
    let again = fetcher::fetcher(&options, &arguments("https://www.wikipedia.org/", "chrome"))
        .unwrap_or_else(|error| panic!("第二次抓取应当成功：{error}"));
    assert!(again.contains("状态: 200"), "{again}");
}
