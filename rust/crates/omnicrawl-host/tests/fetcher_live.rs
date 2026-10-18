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
