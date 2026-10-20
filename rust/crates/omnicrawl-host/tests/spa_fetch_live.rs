//! 真实站点端到端：SPA 外壳（`<div id="root">` + bundle + 接口）能否被重建出正文。
//!
//! 这类测试要联网，默认 `#[ignore]`；用
//! `cargo test -p omnicrawl-host --test spa_fetch_live -- --ignored --nocapture` 运行。
//!
//! 选 chiban.fyi 作为样本：正文完全由脚本 `fetch` 站点自己的 `/api/feed` 后渲染，
//! 静态提取只剩空壳，因此它同时检验「外链脚本执行」与「脚本内请求」两条通路。

use serde_json::json;

use omnicrawl_host::tools::fetcher::{fetcher, FetcherOptions};

fn fetch_page(url: &str) -> String {
    let options = FetcherOptions::default();
    let arguments = json!({ "urls": url })
        .as_object()
        .cloned()
        .expect("参数应是对象");
    let outcome = fetcher(&options, &arguments);
    match outcome {
        Ok(text) => text,
        Err(error) => panic!("抓取失败：{}", error.message),
    }
}

/// 首页正文由脚本拉接口渲染：重建后应当出现文章标题。
#[test]
#[ignore = "需要联网访问 chiban.fyi"]
fn spa_shell_gets_rendered_from_script_fetch() {
    let text = fetch_page("https://chiban.fyi/");
    println!("=== 抓取结果 ===\n{text}");
    assert!(
        text.chars().filter(|c| !c.is_whitespace()).count() >= 200,
        "重建后正文仍然过短，脚本内请求没有生效：\n{text}"
    );
}

/// 单篇文章页：正文是站点自己渲染的，重建后应包含文章内容而非空壳。
#[test]
#[ignore = "需要联网访问 chiban.fyi"]
fn spa_article_page_gets_rendered() {
    let text = fetch_page("https://chiban.fyi/agentde-gong-ju-xi-tong");
    println!("=== 文章页 ===\n{text}");
    assert!(
        text.chars().filter(|c| !c.is_whitespace()).count() >= 200,
        "文章页重建后仍然过短：\n{text}"
    );
}
