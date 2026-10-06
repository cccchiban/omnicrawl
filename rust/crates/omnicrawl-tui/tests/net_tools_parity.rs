//! 对照：Rust 联网工具 vs Python 真实现。
//!
//! 数据集是冻结的对照契约：
//! `web_search` 把 httpx 客户端换成记录型桩，因此引擎端点、查询参数编码与页面解析都可
//! 逐字对照；`fetcher` 的抓取链路依赖 `curl_cffi` 会话，无从注入，对照的是它的纯函数
//! （URL 解析、内网判定、meta refresh、标题与正文提取、截断、格式化）。
//!
//! 两侧的耗时字段都会规范化成 `{ELAPSED}`：它是真实时钟读数，不构成可对照的语义。

use std::sync::{Arc, Mutex};

use regex::Regex;
use serde_json::{Map, Value};

use omnicrawl_tui::tools::fetcher::{self, FetchEntry};
use omnicrawl_tui::tools::web_search::{self, WebSearchOptions};
use omnicrawl_tui::tools::web_transport::{
    is_private_host, WebError, WebRequest, WebResponse, WebTransport,
};

const FIXTURE: &str = include_str!("fixtures/net_tools_parity.json");

fn fixture() -> Value {
    serde_json::from_str(FIXTURE).expect("对照数据集必须是合法 JSON")
}

fn arguments(value: &Value) -> Map<String, Value> {
    value.as_object().cloned().unwrap_or_default()
}

fn strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .expect("字符串数组")
        .iter()
        .map(|item| item.as_str().unwrap_or_default().to_string())
        .collect()
}

fn normalize(text: &str) -> String {
    Regex::new(r"用时：[\d.]+s")
        .expect("耗时正则应当合法")
        .replace_all(text, "用时：{ELAPSED}s")
        .to_string()
}

/// 记录型传输：返回测试给定的页面，并记下每次请求的完整 URL。
struct CaseTransport {
    body: Mutex<String>,
    requests: Mutex<Vec<String>>,
}

impl CaseTransport {
    fn new() -> Self {
        Self {
            body: Mutex::new(String::new()),
            requests: Mutex::new(Vec::new()),
        }
    }

    fn set_body(&self, body: &str) {
        *self.body.lock().expect("页面锁") = body.to_string();
    }

    fn take_requests(&self) -> Vec<String> {
        std::mem::take(&mut *self.requests.lock().expect("请求锁"))
    }
}

impl WebTransport for CaseTransport {
    fn send(&self, request: &WebRequest) -> Result<WebResponse, WebError> {
        self.requests
            .lock()
            .expect("请求锁")
            .push(request.url.clone());
        Ok(WebResponse {
            status: 200,
            final_url: request.url.clone(),
            body: self.body.lock().expect("页面锁").clone().into_bytes(),
        })
    }
}

#[test]
fn web_search_cases_match_python() {
    let data = fixture();
    let section = &data["web_search"];
    let transport = Arc::new(CaseTransport::new());
    let options = WebSearchOptions {
        transport: transport.clone(),
        timeout_seconds: section["options"]["timeout_seconds"]
            .as_f64()
            .unwrap_or(10.0),
        max_retries: section["options"]["max_retries"].as_u64().unwrap_or(0) as usize,
        // 显式禁用代理：对照不读系统代理、不起网络。
        proxy: Some(String::new()),
        ..WebSearchOptions::default()
    };

    for case in section["cases"].as_array().expect("搜索用例") {
        transport.set_body(case["html"].as_str().unwrap_or_default());
        let _ = transport.take_requests();
        let outcome = web_search::web_search(&options, &arguments(&case["arguments"]));
        assert_eq!(
            transport.take_requests(),
            strings(&case["requests"]),
            "请求 URL 不一致：{:?}",
            case["arguments"]
        );

        let expected = case["output"].as_str().unwrap_or_default();
        let ok = case["ok"].as_bool().unwrap_or(false);
        match outcome {
            Ok(output) => {
                assert!(ok, "本应失败，实际成功：{output}");
                assert_eq!(normalize(&output), expected, "用例 {:?}", case["arguments"]);
            }
            Err(error) => {
                assert!(!ok, "本应成功，实际失败：{}", error.message);
                assert_eq!(
                    normalize(&error.formatted()),
                    expected,
                    "用例 {:?}",
                    case["arguments"]
                );
            }
        }
    }
}

fn entry_from(value: &Value) -> FetchEntry {
    match value.get("error").and_then(Value::as_str) {
        Some(error) => {
            FetchEntry::failed(value["url"].as_str().unwrap_or_default(), error.to_string())
        }
        None => FetchEntry::done(
            value["url"].as_str().unwrap_or_default(),
            value["status"].as_u64().unwrap_or(0) as u16,
            value["final_url"].as_str().unwrap_or_default().to_string(),
            value["title"].as_str().unwrap_or_default().to_string(),
            value["content"].as_str().unwrap_or_default().to_string(),
        ),
    }
}

#[test]
fn fetcher_helpers_match_python() {
    let data = fixture();
    let section = &data["fetcher"];

    for case in section["urls"].as_array().expect("URL 用例") {
        assert_eq!(
            fetcher::parse_urls(Some(&case["input"])),
            strings(&case["expected"]),
            "urls 输入 {:?}",
            case["input"]
        );
    }

    for case in section["private_hosts"].as_array().expect("内网主机用例") {
        let host = case["host"].as_str().unwrap_or_default();
        assert_eq!(
            is_private_host(host),
            case["expected"].as_bool().unwrap_or(false),
            "主机 {host}"
        );
    }

    for case in section["meta_refresh"]
        .as_array()
        .expect("meta refresh 用例")
    {
        let html = case["html"].as_str().unwrap_or_default();
        let base = case["base"].as_str().unwrap_or_default();
        assert_eq!(
            fetcher::meta_refresh_target(html, base),
            case["expected"].as_str().map(str::to_string),
            "页面 {html}"
        );
    }

    for case in section["titles"].as_array().expect("标题用例") {
        let html = case["html"].as_str().unwrap_or_default();
        assert_eq!(
            fetcher::extract_title(html),
            case["expected"].as_str().unwrap_or_default(),
            "页面 {html}"
        );
    }

    for case in section["main_text"].as_array().expect("正文用例") {
        let html = case["html"].as_str().unwrap_or_default();
        let max_chars = case["max_chars"].as_u64().unwrap_or(8000) as usize;
        assert_eq!(
            fetcher::extract_main_text(html, max_chars),
            case["expected"].as_str().unwrap_or_default(),
            "页面 {html}"
        );
    }

    for case in section["truncate"].as_array().expect("截断用例") {
        let text = case["text"].as_str().unwrap_or_default();
        let limit = case["limit"].as_u64().unwrap_or(0) as usize;
        assert_eq!(
            web_search::truncate(text, limit),
            case["expected"].as_str().unwrap_or_default(),
            "文本 {text} 限长 {limit}"
        );
    }

    for case in section["bools"].as_array().expect("布尔用例") {
        assert_eq!(
            fetcher::as_bool(
                Some(&case["input"]),
                case["default"].as_bool().unwrap_or(false)
            ),
            case["expected"].as_bool().unwrap_or(false),
            "输入 {:?}",
            case["input"]
        );
    }

    for case in section["clamp_int"].as_array().expect("整数夹取用例") {
        assert_eq!(
            web_search::clamp_int(
                Some(&case["input"]),
                case["default"].as_i64().unwrap_or(0),
                case["minimum"].as_i64().unwrap_or(0),
                case["maximum"].as_i64().unwrap_or(0)
            ),
            case["expected"].as_i64().unwrap_or(0),
            "输入 {:?}",
            case["input"]
        );
    }

    for case in section["clamp_float"].as_array().expect("浮点夹取用例") {
        assert_eq!(
            web_search::clamp_float(
                Some(&case["input"]),
                case["default"].as_f64().unwrap_or(0.0),
                case["minimum"].as_f64().unwrap_or(0.0),
                case["maximum"].as_f64().unwrap_or(0.0)
            ),
            case["expected"].as_f64().unwrap_or(0.0),
            "输入 {:?}",
            case["input"]
        );
    }

    for case in section["format"].as_array().expect("格式化用例") {
        let urls = strings(&case["urls"]);
        let entries: Vec<FetchEntry> = case["entries"]
            .as_array()
            .expect("结果条目")
            .iter()
            .map(entry_from)
            .collect();
        let elapsed = case["elapsed"].as_f64().unwrap_or(0.0);
        assert_eq!(
            fetcher::format_results(&urls, &entries, elapsed),
            case["expected"].as_str().unwrap_or_default()
        );
    }
}
