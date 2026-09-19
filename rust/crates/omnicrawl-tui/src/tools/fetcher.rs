//! 网页抓取工具执行体：并行抓取、跟随 HTTP 重定向与 meta refresh、正文提取。
//!
//! 语义基准是 `omnicrawl/net/fetcher.py`：只抓取用户明确给出的 URL、不执行 JavaScript、
//! 不绕过验证码；内网/本机目标直连而不走系统代理；`insecure=true` 跳过 TLS 校验。
//! 正文提取用 HTML5 容错解析（`lxml` 的 `main > article > body` + `text_content` 等价），
//! Python 侧的 `impersonate` 浏览器指纹参数在此被接受但**不生效**（Rust 传输不做 JA3/JA4
//! 模拟），这一点记录在 crate README 的已知差异里。

use std::sync::Arc;
use std::time::{Duration, Instant};

use regex::Regex;
use scraper::{ElementRef, Html, Node, Selector};
use serde_json::{Map, Value};

use super::error::{ToolError, ToolOutcome};
use super::web_search::{clamp_float, clamp_int, strip_tags, truncate};
use super::web_transport::{
    browser_headers, detect_windows_proxy, host_of, is_private_host, resolve_url, WebErrorKind,
    WebRequest, WebTransport, MAX_REDIRECTS,
};

pub const DEFAULT_TIMEOUT_SECONDS: f64 = 15.0;
pub const DEFAULT_MAX_CHARS: i64 = 8000;
pub const MAX_URLS: usize = 20;
pub const MAX_META_REDIRECTS: usize = 3;
pub const MAX_WORKERS: usize = 8;
const MIN_CHARS: i64 = 200;
const MAX_CHARS_LIMIT: i64 = 200000;
const NON_CONTENT_TAGS: [&str; 5] = ["script", "style", "noscript", "svg", "template"];

#[derive(Clone)]
pub struct FetcherOptions {
    pub transport: Arc<dyn WebTransport>,
    pub user_agent: String,
    pub request_timeout_seconds: f64,
    pub max_workers: usize,
    /// `None`：自动检测 Windows 系统代理；`Some("")`：不使用代理；其他为显式地址。
    pub proxy: Option<String>,
}

impl Default for FetcherOptions {
    fn default() -> Self {
        Self {
            transport: Arc::new(super::web_transport::UreqWebTransport::new()),
            user_agent: super::web_transport::DEFAULT_USER_AGENT.to_string(),
            request_timeout_seconds: DEFAULT_TIMEOUT_SECONDS,
            max_workers: MAX_WORKERS,
            proxy: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchEntry {
    pub url: String,
    pub error: Option<String>,
    pub status: u16,
    pub final_url: String,
    pub title: String,
    pub content: String,
}

impl FetchEntry {
    pub fn failed(url: &str, error: String) -> Self {
        Self {
            url: url.to_string(),
            error: Some(error),
            status: 0,
            final_url: String::new(),
            title: String::new(),
            content: String::new(),
        }
    }

    pub fn done(url: &str, status: u16, final_url: String, title: String, content: String) -> Self {
        Self {
            url: url.to_string(),
            error: None,
            status,
            final_url,
            title,
            content,
        }
    }
}

enum FetchError {
    /// 参数、状态码类错误：文案直接给模型，不再加工。
    Fatal(String),
    Transport(WebErrorKind, String),
}

struct FetchPlan {
    insecure: bool,
    max_chars: usize,
    max_html: bool,
    timeout: Duration,
}

pub fn fetcher(options: &FetcherOptions, arguments: &Map<String, Value>) -> ToolOutcome {
    let urls = parse_urls(arguments.get("urls"));
    if urls.is_empty() {
        return Err(ToolError::new(
            "urls 不能为空：请提供至少一个要抓取的 URL（逗号分隔或 JSON 数组）。",
        ));
    }
    let plan = FetchPlan {
        insecure: as_bool(arguments.get("insecure"), false),
        max_chars: clamp_int(
            arguments.get("max_chars"),
            DEFAULT_MAX_CHARS,
            MIN_CHARS,
            MAX_CHARS_LIMIT,
        ) as usize,
        max_html: as_bool(arguments.get("max_html"), false),
        timeout: Duration::from_secs_f64(clamp_float(
            arguments.get("timeout"),
            options.request_timeout_seconds,
            1.0,
            60.0,
        )),
    };

    let started = Instant::now();
    let parallel = as_bool(arguments.get("parallel"), true);
    let entries = if parallel && urls.len() > 1 {
        fetch_parallel(options, &urls, &plan)
    } else {
        urls.iter()
            .map(|url| fetch_one(options, url, &plan))
            .collect()
    };
    Ok(format_results(
        &urls,
        &entries,
        started.elapsed().as_secs_f64(),
    ))
}

fn fetch_parallel(options: &FetcherOptions, urls: &[String], plan: &FetchPlan) -> Vec<FetchEntry> {
    let workers = std::cmp::max(1, std::cmp::min(urls.len(), options.max_workers));
    let chunk_size = urls.len().div_ceil(workers);
    let mut collected: Vec<(usize, FetchEntry)> = Vec::new();
    std::thread::scope(|scope| {
        let handles: Vec<_> = urls
            .chunks(chunk_size)
            .enumerate()
            .map(|(chunk_index, chunk)| {
                let offset = chunk_index * chunk_size;
                scope.spawn(move || {
                    chunk
                        .iter()
                        .enumerate()
                        .map(|(index, url)| (offset + index, fetch_one(options, url, plan)))
                        .collect::<Vec<(usize, FetchEntry)>>()
                })
            })
            .collect();
        for handle in handles {
            match handle.join() {
                Ok(items) => collected.extend(items),
                Err(_) => collected.push((
                    usize::MAX,
                    FetchEntry::failed("", "抓取线程异常终止。".to_string()),
                )),
            }
        }
    });
    collected.sort_by_key(|(index, _)| *index);
    collected.into_iter().map(|(_, entry)| entry).collect()
}

fn fetch_one(options: &FetcherOptions, url: &str, plan: &FetchPlan) -> FetchEntry {
    match fetch_one_inner(options, url, plan) {
        Ok(entry) => entry,
        Err(FetchError::Fatal(message)) => FetchEntry::failed(url, message),
        Err(FetchError::Transport(kind, message)) => {
            FetchEntry::failed(url, friendly_error(kind, &message))
        }
    }
}

fn fetch_one_inner(
    options: &FetcherOptions,
    url: &str,
    plan: &FetchPlan,
) -> Result<FetchEntry, FetchError> {
    let mut current = url.to_string();
    let mut status = 0u16;
    let mut final_url = url.to_string();
    let mut html_text = String::new();
    for _ in 0..=MAX_META_REDIRECTS {
        let mut request = WebRequest::new(
            current.clone(),
            browser_headers(&options.user_agent),
            plan.timeout,
        );
        request.proxy = resolve_proxy(&current, options);
        request.insecure = plan.insecure;
        request.max_redirects = MAX_REDIRECTS;
        let response = options
            .transport
            .send(&request)
            .map_err(|error| FetchError::Transport(error.kind, error.message))?;
        status = response.status;
        if status >= 400 {
            return Err(FetchError::Fatal(format!(
                "HTTP {status}（目标返回错误状态码）"
            )));
        }
        html_text = response.text();
        final_url = response.final_url;
        match meta_refresh_target(&html_text, &final_url) {
            Some(target) => current = target,
            None => break,
        }
    }
    let title = extract_title(&html_text);
    let content = if plan.max_html {
        html_text
    } else {
        extract_main_text(&html_text, plan.max_chars)
    };
    Ok(FetchEntry::done(url, status, final_url, title, content))
}

/// `None` 表示自动检测系统代理；内网/本机目标一律直连。
fn resolve_proxy(url: &str, options: &FetcherOptions) -> Option<String> {
    let host = host_of(url).unwrap_or_default();
    if is_private_host(&host) {
        return None;
    }
    match options.proxy.as_deref() {
        None => detect_windows_proxy(),
        Some("") => None,
        Some(explicit) => Some(explicit.to_string()),
    }
}

fn friendly_error(kind: WebErrorKind, message: &str) -> String {
    match kind {
        WebErrorKind::Timeout => "请求超时。".to_string(),
        WebErrorKind::Connect => "无法连接目标（网络不可达或目标拒绝连接）。".to_string(),
        WebErrorKind::Tls => {
            "TLS 证书校验失败；内网自签名站点可在参数中加 insecure=true 重试。".to_string()
        }
        WebErrorKind::Other => format!("网络请求失败：{message}"),
    }
}

pub fn as_bool(value: Option<&Value>, default: bool) -> bool {
    match value {
        Some(Value::Bool(flag)) => *flag,
        Some(Value::Number(number)) => number.as_f64().map(|item| item != 0.0).unwrap_or(false),
        Some(Value::String(text)) => matches!(
            text.trim().to_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        ),
        _ => default,
    }
}

/// 解析 `urls` 参数：逗号分隔字符串、JSON 数组字符串或 JSON 数组。
pub fn parse_urls(value: Option<&Value>) -> Vec<String> {
    let mut parts: Vec<String> = Vec::new();
    match value {
        Some(Value::String(text)) => {
            let raw = text.trim();
            if raw.is_empty() {
                return Vec::new();
            }
            match serde_json::from_str::<Value>(raw) {
                Ok(Value::Array(items)) => parts = items.iter().map(scalar_text).collect(),
                Ok(_) => {}
                Err(_) => parts = raw.split(',').map(|part| part.trim().to_string()).collect(),
            }
        }
        Some(Value::Array(items)) => parts = items.iter().map(scalar_text).collect(),
        _ => {}
    }
    parts
        .into_iter()
        .filter(|part| !part.is_empty())
        .take(MAX_URLS)
        .collect()
}

fn scalar_text(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(text) => text.trim().to_string(),
        Value::Bool(flag) => if *flag { "True" } else { "False" }.to_string(),
        other => omnicrawl_controllers::json::python_repr(other)
            .trim()
            .to_string(),
    }
}

/// 解析 `<meta http-equiv="refresh" content="N; url=X">` 的跳转目标。
pub fn meta_refresh_target(html_text: &str, base_url: &str) -> Option<String> {
    let meta = Regex::new(
        r#"(?i)<meta\s+[^>]*http-equiv\s*=\s*["']?refresh["']?[^>]*content\s*=\s*["']([^"']*)["']?"#,
    )
    .ok()?;
    let url_pattern = Regex::new(r"(?i)url\s*=\s*([^\s;]+)").ok()?;
    for capture in meta.captures_iter(html_text) {
        let Some(url_match) = url_pattern.captures(&capture[1]) else {
            continue;
        };
        let target = url_match[1]
            .trim_matches(|character| character == '\'' || character == '"')
            .to_string();
        if target.is_empty() {
            continue;
        }
        return Some(resolve_url(base_url, &target));
    }
    None
}

/// 提取 `<title>` 文本；不存在时返回空串。
pub fn extract_title(html_text: &str) -> String {
    let Ok(pattern) = Regex::new(r"(?is)<title[^>]*>(.*?)</title>") else {
        return String::new();
    };
    match pattern.captures(html_text) {
        Some(capture) => collapse_whitespace(&strip_tags(&capture[1])),
        None => String::new(),
    }
}

/// 提取正文文本：优先 `main` → `article` → `body`，剔除脚本与样式后压缩空白。
pub fn extract_main_text(html_text: &str, max_chars: usize) -> String {
    let document = Html::parse_document(html_text);
    let container = ["main", "article", "body"].iter().find_map(|name| {
        Selector::parse(name)
            .ok()
            .and_then(|selector| document.select(&selector).next())
    });
    let mut text = String::new();
    match container {
        Some(element) => append_text(element, &mut text),
        None => append_text(document.root_element(), &mut text),
    }
    truncate(&collapse_whitespace(&text), max_chars)
}

fn append_text(element: ElementRef<'_>, out: &mut String) {
    for child in element.children() {
        match child.value() {
            Node::Text(text) => out.push_str(&text.text),
            Node::Element(_) => {
                let Some(child_element) = ElementRef::wrap(child) else {
                    continue;
                };
                if NON_CONTENT_TAGS.contains(&child_element.value().name()) {
                    continue;
                }
                append_text(child_element, out);
            }
            _ => {}
        }
    }
}

fn collapse_whitespace(text: &str) -> String {
    Regex::new(r"\s+")
        .expect("空白正则应当合法")
        .replace_all(text, " ")
        .trim()
        .to_string()
}

pub fn format_results(urls: &[String], entries: &[FetchEntry], elapsed: f64) -> String {
    let mut lines = vec![format!(
        "网页抓取完成（{} 个 URL，用时 {elapsed:.2}s）",
        urls.len()
    )];
    for (index, entry) in entries.iter().enumerate() {
        lines.push(format!("{}. {}", index + 1, entry.url));
        if let Some(error) = &entry.error {
            lines.push(format!("   失败: {error}"));
            continue;
        }
        lines.push(format!(
            "   状态: {}｜最终地址: {}",
            entry.status, entry.final_url
        ));
        if !entry.title.is_empty() {
            lines.push(format!("   标题: {}", truncate(&entry.title, 200)));
        }
        lines.push(format!("   内容: {}", entry.content));
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn urls_accept_comma_string_json_array_and_list() {
        assert_eq!(
            parse_urls(Some(&json!("https://a, https://b"))),
            vec!["https://a".to_string(), "https://b".to_string()]
        );
        assert_eq!(
            parse_urls(Some(&json!("[\"https://a\", \"https://b\"]"))),
            vec!["https://a".to_string(), "https://b".to_string()]
        );
        assert_eq!(
            parse_urls(Some(&json!(["https://a"]))),
            vec!["https://a".to_string()]
        );
        assert!(parse_urls(Some(&json!(""))).is_empty());
        // JSON 合法但不是数组：Python 也不做逗号回退。
        assert!(parse_urls(Some(&json!("{\"a\": 1}"))).is_empty());
    }

    #[test]
    fn meta_refresh_target_is_resolved_against_the_page() {
        let html = r#"<meta http-equiv="refresh" content="0; url=/next/page.html">"#;
        assert_eq!(
            meta_refresh_target(html, "https://example.com/a/index.html").as_deref(),
            Some("https://example.com/next/page.html")
        );
        assert!(meta_refresh_target("<html></html>", "https://example.com/").is_none());
    }

    #[test]
    fn main_text_prefers_main_then_article_then_body() {
        let html = "<html><body><p>body</p><article><p>article</p></article>\
<main><p>main</p></main></body></html>";
        assert_eq!(extract_main_text(html, 1000), "main");
        let html = "<html><body><p>body</p><article><p>article</p></article></body></html>";
        assert_eq!(extract_main_text(html, 1000), "article");
        let html = "<html><body><p>body</p></body></html>";
        assert_eq!(extract_main_text(html, 1000), "body");
    }

    #[test]
    fn scripts_and_styles_are_dropped_and_text_is_truncated() {
        let html = "<html><body><script>var x = 1;</script><style>p{}</style>\
<p>正文&nbsp;内容</p><noscript>降级</noscript></body></html>";
        assert_eq!(extract_main_text(html, 1000), "正文 内容");
        let long = format!("<html><body>{}</body></html>", "字".repeat(50));
        let text = extract_main_text(&long, 10);
        assert_eq!(text.chars().count(), 10);
        assert!(text.ends_with('…'));
    }

    #[test]
    fn titles_are_stripped_and_collapsed() {
        assert_eq!(
            extract_title("<html><head><title>\n 标题 &amp; 副标题 \n</title></head></html>"),
            "标题 & 副标题"
        );
        assert_eq!(extract_title("<html></html>"), "");
    }
}
