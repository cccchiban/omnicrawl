//! 网页搜索工具执行体：查询 Bing、DuckDuckGo、雅虎、Tavily 或 Exa 并返回标题、链接与摘要。
//!
//! 抓页面型引擎（Bing / DuckDuckGo / 雅虎）的语义基准是 `omnicrawl/net/web_search.py`：同一套
//! 端点与查询参数、同一套页面解析规则（正则而非 DOM）、同一套拦截检测与文案，只访问公开
//! 搜索页面，检测到验证码即如实报错。接口型引擎（Tavily / Exa）走各自的 JSON 检索接口，
//! 凭据取 `[web_search]` 段（见 [`WebSearchOptions`]）。

use std::str::FromStr;
use std::sync::Arc;
use std::thread::sleep;
use std::time::{Duration, Instant};

use omnicrawl_config::features::web_search::WEB_SEARCH_ENGINES;
use regex::Regex;
use serde_json::{Map, Value};

use super::error::{ToolError, ToolOutcome};
use super::web_transport::{
    browser_headers, detect_windows_proxy, host_of, is_private_host, WebErrorKind, WebRequest,
    WebTransport, DEFAULT_USER_AGENT, MAX_REDIRECTS,
};

pub const DEFAULT_TIMEOUT_SECONDS: f64 = 10.0;
pub const MAX_RESULTS_PER_ENGINE: i64 = 10;
/// 支持的全部引擎；清单的唯一来源在配置域（界面候选与校验读同一张表）。
pub const SUPPORTED_ENGINES: [&str; 5] = WEB_SEARCH_ENGINES;

/// Tavily 检索接口（`POST`，Bearer 鉴权）。
const TAVILY_ENDPOINT: &str = "https://api.tavily.com/search";
/// Exa 检索接口（`POST`，`x-api-key` 鉴权）。
const EXA_ENDPOINT: &str = "https://api.exa.ai/search";

const CAPTCHA_MARKERS: [&str; 7] = [
    "captcha",
    "unusual traffic",
    "enable cookies",
    "verify you're human",
    "请输入验证码",
    "人机验证",
    "检测到异常流量",
];

const DDG_REGIONS: [(&str, &str); 7] = [
    ("zh", "cn-zh"),
    ("zh-cn", "cn-zh"),
    ("zh-tw", "tw-zh"),
    ("en", "us-en"),
    ("en-us", "us-en"),
    ("en-gb", "uk-en"),
    ("ja", "jp-ja"),
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebSearchResult {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

#[derive(Clone)]
pub struct WebSearchOptions {
    pub transport: Arc<dyn WebTransport>,
    pub user_agent: String,
    pub timeout_seconds: f64,
    pub max_retries: usize,
    pub request_delay_seconds: f64,
    /// `None`：自动检测 Windows 系统代理；`Some("")`：不使用代理；其他为显式地址。
    pub proxy: Option<String>,
    /// 未显式指定 `engine` 时用的引擎（`[web_search] engine`）。
    pub default_engine: String,
    /// Tavily API Key（空串表示未配置）。
    pub tavily_api_key: String,
    /// Exa API Key（空串表示未配置）。
    pub exa_api_key: String,
}

impl Default for WebSearchOptions {
    fn default() -> Self {
        Self {
            transport: Arc::new(super::web_transport::UreqWebTransport::new()),
            user_agent: DEFAULT_USER_AGENT.to_string(),
            timeout_seconds: DEFAULT_TIMEOUT_SECONDS,
            max_retries: 1,
            request_delay_seconds: 0.0,
            proxy: None,
            default_engine: DEFAULT_ENGINE.to_string(),
            tavily_api_key: String::new(),
            exa_api_key: String::new(),
        }
    }
}

impl WebSearchOptions {
    /// 指定引擎下生效的 API Key；抓页面型引擎没有凭据，返回空串。
    pub fn api_key_for(&self, engine: &str) -> &str {
        match engine {
            "tavily" => self.tavily_api_key.as_str(),
            "exa" => self.exa_api_key.as_str(),
            _ => "",
        }
    }

    /// 未显式给 `engine` 时的缺省引擎：配置值不合法（手改坏）时回落到无需凭据的默认引擎。
    fn resolve_default_engine(&self) -> &'static str {
        let engine = self.default_engine.trim().to_lowercase();
        SUPPORTED_ENGINES
            .iter()
            .copied()
            .find(|candidate| *candidate == engine.as_str())
            .unwrap_or(DEFAULT_ENGINE)
    }
}

/// 无需凭据的默认引擎（配置缺失或非法时的兜底）。
pub const DEFAULT_ENGINE: &str = "bing";

/// 按 `[web_search]` 段装配搜索运行期（TUI 与 API 共用同一口径）。
///
/// 读不出配置时用默认选项（默认引擎 + 未配置凭据）：接口型引擎在被调用时会如实报
/// 「缺 API Key」，而不是让工具整组不可用。
pub fn web_search_options_from_config(
    environment: &omnicrawl_config::core::runtime::ConfigEnvironment,
) -> WebSearchOptions {
    let configuration =
        omnicrawl_config::features::web_search::load_web_search_configuration(environment, None);
    let configuration = match configuration {
        Ok(configuration) => configuration,
        Err(_) => omnicrawl_config::features::web_search::WebSearchConfiguration::default(),
    };
    WebSearchOptions {
        default_engine: configuration.engine,
        tavily_api_key: configuration.tavily_api_key,
        exa_api_key: configuration.exa_api_key,
        ..WebSearchOptions::default()
    }
}

/// 一次请求的失败分类：`Fatal` 不重试（参数、拦截、解析），`Network` 允许重试。
enum SearchFailure {
    Fatal(ToolError),
    Network(String),
}

/// 剥离 HTML 标签、还原实体并把连续空白压缩为单个空格。
pub fn strip_tags(text: &str) -> String {
    let tag = Regex::new(r"<[^>]+>").expect("标签正则应当合法");
    let spaced = tag.replace_all(text, " ");
    let decoded = decode_entities(&spaced);
    Regex::new(r"\s+")
        .expect("空白正则应当合法")
        .replace_all(&decoded, " ")
        .trim()
        .to_string()
}

/// 用 HTML5 分词器解码实体（`html.unescape` 的等价实现）；无实体时原样返回。
fn decode_entities(text: &str) -> String {
    if !text.contains('&') {
        return text.to_string();
    }
    let document = scraper::Html::parse_fragment(text);
    document.root_element().text().collect::<String>()
}

fn parse_bing(html_text: &str) -> Vec<WebSearchResult> {
    let block_re = Regex::new(r#"(?s)<li[^>]*class="[^"]*b_algo[^"]*"[^>]*>(.*?)</li>"#)
        .expect("bing 区块正则");
    let anchor_re =
        Regex::new(r#"(?s)<h2[^>]*>\s*<a[^>]*href="([^"]*)"[^>]*>(.*?)</a>"#).expect("bing 锚点");
    let paragraph_re = Regex::new(r"(?s)<p[^>]*>(.*?)</p>").expect("bing 摘要");

    let mut results = Vec::new();
    for block in block_re.captures_iter(html_text) {
        let block = &block[1];
        let Some(anchor) = anchor_re.captures(block) else {
            continue;
        };
        let url = anchor[1].to_string();
        let title = strip_tags(&anchor[2]);
        if !url.starts_with("http") || title.is_empty() {
            continue;
        }
        let snippet = paragraph_re
            .captures(block)
            .map(|paragraph| strip_tags(&paragraph[1]))
            .unwrap_or_default();
        results.push(WebSearchResult {
            title,
            url,
            snippet,
        });
    }
    results
}

fn parse_duckduckgo(html_text: &str) -> Vec<WebSearchResult> {
    let anchor_re =
        Regex::new(r#"(?s)<a[^>]*class="[^"]*result__a[^"]*"[^>]*href="([^"]*)"[^>]*>(.*?)</a>"#)
            .expect("ddg 锚点");
    let snippet_re = Regex::new(r#"(?s)<a[^>]*class="[^"]*result__snippet[^"]*"[^>]*>(.*?)</a>"#)
        .expect("ddg 摘要");

    let mut results: Vec<WebSearchResult> = Vec::new();
    for capture in anchor_re.captures_iter(html_text) {
        let url = clean_duckduckgo_url(&capture[1]);
        let title = strip_tags(&capture[2]);
        if !url.starts_with("http") || title.is_empty() {
            continue;
        }
        results.push(WebSearchResult {
            title,
            url,
            snippet: String::new(),
        });
    }
    for (index, capture) in snippet_re.captures_iter(html_text).enumerate() {
        if index >= results.len() {
            break;
        }
        let snippet = strip_tags(&capture[1]);
        if !snippet.is_empty() {
            results[index].snippet = snippet;
        }
    }
    results
}

fn parse_yahoo(html_text: &str) -> Vec<WebSearchResult> {
    let anchor_re = Regex::new(
        r#"(?s)<a[^>]*class="[^"]*d-ib[^"]*"[^>]*href="([^"]*)"[^>]*>.*?<h3[^>]*class="[^"]*title[^"]*"[^>]*>(.*?)</h3>"#,
    )
    .expect("yahoo 锚点");
    let snippet_re = Regex::new(r#"(?s)<div[^>]*class="[^"]*compText[^"]*"[^>]*>(.*?)</div>"#)
        .expect("yahoo 摘要");

    let mut results: Vec<WebSearchResult> = Vec::new();
    for capture in anchor_re.captures_iter(html_text) {
        let url = clean_yahoo_url(&capture[1]);
        let title = strip_tags(&capture[2]);
        if !url.starts_with("http") || title.is_empty() {
            continue;
        }
        results.push(WebSearchResult {
            title,
            url,
            snippet: String::new(),
        });
    }
    for (index, capture) in snippet_re.captures_iter(html_text).enumerate() {
        if index >= results.len() {
            break;
        }
        let snippet = strip_tags(&capture[1]);
        if !snippet.is_empty() && snippet.chars().count() > 4 {
            results[index].snippet = snippet;
        }
    }
    results
}

fn parse_results(engine: &str, html_text: &str) -> Vec<WebSearchResult> {
    match engine {
        "duckduckgo" => parse_duckduckgo(html_text),
        "yahoo" => parse_yahoo(html_text),
        _ => parse_bing(html_text),
    }
}

/// 还原雅虎跳转参数 `RU=`；普通链接原样返回。
fn clean_yahoo_url(raw: &str) -> String {
    let decoded = decode_entities(raw);
    let pattern = Regex::new(r"(?:/|&|\?)RU=([^/&]+)").expect("yahoo RU 正则");
    match pattern.captures(&decoded) {
        Some(capture) => percent_decode(&capture[1], false),
        None => decoded,
    }
}

/// 还原 DuckDuckGo 的 `uddg=` 跳转参数（先按查询串语义解码，再去百分号转义）。
fn clean_duckduckgo_url(raw: &str) -> String {
    let Ok(uri) = ureq::http::Uri::from_str(raw) else {
        return raw.to_string();
    };
    let Some(query) = uri.query() else {
        return raw.to_string();
    };
    for pair in query.split('&') {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        if key == "uddg" && !value.is_empty() {
            return percent_decode(&percent_decode(value, true), false);
        }
    }
    raw.to_string()
}

fn percent_decode(value: &str, plus_as_space: bool) -> String {
    let bytes = value.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' if index + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(byte) => {
                        out.push(byte);
                        index += 3;
                    }
                    Err(_) => {
                        out.push(bytes[index]);
                        index += 1;
                    }
                }
            }
            b'+' if plus_as_space => {
                out.push(b' ');
                index += 1;
            }
            other => {
                out.push(other);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

fn endpoint(engine: &str, query: &str, language: Option<&str>) -> (String, Vec<(String, String)>) {
    if engine == "bing" {
        return (
            "https://www.bing.com/search".to_string(),
            vec![
                ("q".to_string(), query.to_string()),
                ("count".to_string(), MAX_RESULTS_PER_ENGINE.to_string()),
                (
                    "setlang".to_string(),
                    language.unwrap_or("zh-CN").to_string(),
                ),
            ],
        );
    }
    if engine == "yahoo" {
        return (
            "https://search.yahoo.com/search".to_string(),
            vec![
                ("p".to_string(), query.to_string()),
                ("n".to_string(), MAX_RESULTS_PER_ENGINE.to_string()),
            ],
        );
    }
    let lookup = language.unwrap_or("zh-CN").trim().to_lowercase();
    let region = DDG_REGIONS
        .iter()
        .find(|(key, _)| *key == lookup)
        .map(|(_, value)| (*value).to_string())
        .unwrap_or_else(|| "us-en".to_string());
    (
        "https://html.duckduckgo.com/html/".to_string(),
        vec![
            ("q".to_string(), query.to_string()),
            ("kl".to_string(), region),
        ],
    )
}

fn build_url(base: &str, params: &[(String, String)]) -> String {
    let encoded: Vec<String> = params
        .iter()
        .map(|(key, value)| format!("{}={}", encode_component(key), encode_component(value)))
        .collect();
    format!("{base}?{}", encoded.join("&"))
}

/// `application/x-www-form-urlencoded` 的百分号编码（空格写成 `+`，与 httpx 的 params 一致）。
fn encode_component(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                encoded.push(*byte as char)
            }
            b' ' => encoded.push('+'),
            other => encoded.push_str(&format!("%{other:02X}")),
        }
    }
    encoded
}

fn check_captcha(text: &str) -> Result<(), ToolError> {
    let chars: Vec<char> = text.chars().collect();
    let head: String = chars.iter().take(4000).collect();
    let tail: String = chars
        .iter()
        .skip(chars.len().saturating_sub(1000))
        .collect();
    let low = format!("{head}{tail}").to_lowercase();
    if CAPTCHA_MARKERS.iter().any(|marker| low.contains(marker)) {
        return Err(ToolError::new(
            "搜索引擎要求人机验证或检测到异常流量，已停止请求（不会绕过验证码）。\
可稍后重试或换用其他引擎。",
        ));
    }
    Ok(())
}

fn friendly_network_error(kind: WebErrorKind, message: &str) -> String {
    match kind {
        WebErrorKind::Timeout => "请求超时。".to_string(),
        WebErrorKind::Connect => "无法连接到搜索引擎（网络不可达）。".to_string(),
        _ => format!("网络请求失败：{message}"),
    }
}

fn status_message(status: u16) -> String {
    match status {
        429 => "请求过于频繁（HTTP 429），请稍后重试。".to_string(),
        403 | 451 => "搜索引擎拒绝访问（HTTP 403/451），可能触发了反爬限制。".to_string(),
        other => format!("搜索引擎返回 HTTP {other}。"),
    }
}

pub fn web_search(options: &WebSearchOptions, arguments: &Map<String, Value>) -> ToolOutcome {
    let query = argument_text(arguments, "query");
    if query.is_empty() {
        return Err(ToolError::new("query 不能为空。"));
    }
    let engine = match argument_text(arguments, "engine").to_lowercase() {
        value if value.is_empty() => options.resolve_default_engine().to_string(),
        value => value,
    };
    if !SUPPORTED_ENGINES.contains(&engine.as_str()) {
        return Err(ToolError::new(format!(
            "engine 必须是 {} 之一。",
            SUPPORTED_ENGINES.join("、")
        )));
    }
    let max_results = clamp_int(arguments.get("max_results"), 5, 1, MAX_RESULTS_PER_ENGINE);
    let language = match arguments.get("language") {
        Some(Value::String(text)) if !text.trim().is_empty() => Some(text.trim().to_string()),
        Some(value) => {
            let text = scalar_text(value);
            if text.trim().is_empty() {
                None
            } else {
                Some(text.trim().to_string())
            }
        }
        None => None,
    };

    if matches!(engine.as_str(), "tavily" | "exa") {
        return api_search(options, &engine, &query, max_results);
    }

    let started = Instant::now();
    let mut last_error: Option<String> = None;
    for attempt in 0..=options.max_retries {
        match request_html(options, &engine, &query, language.as_deref()) {
            Ok(html_text) => {
                let results = parse_results(&engine, &html_text);
                if results.is_empty() {
                    return Err(ToolError::new("未找到相关搜索结果（或页面结构无法解析）。"));
                }
                let elapsed = started.elapsed().as_secs_f64();
                return Ok(format_results(
                    &engine,
                    &query,
                    &results[..results.len().min(max_results as usize)],
                    elapsed,
                ));
            }
            Err(SearchFailure::Fatal(error)) => return Err(error),
            Err(SearchFailure::Network(message)) => {
                last_error = Some(message);
                if attempt < options.max_retries {
                    let delay = if options.request_delay_seconds > 0.0 {
                        options.request_delay_seconds
                    } else {
                        0.4 * (attempt as f64 + 1.0)
                    };
                    sleep(Duration::from_secs_f64(delay));
                }
            }
        }
    }
    Err(ToolError::new(format!(
        "搜索失败（{engine}）：{}",
        last_error.unwrap_or_default()
    )))
}

fn request_html(
    options: &WebSearchOptions,
    engine: &str,
    query: &str,
    language: Option<&str>,
) -> Result<String, SearchFailure> {
    let (base, params) = endpoint(engine, query, language);
    let url = build_url(&base, &params);
    let headers = browser_headers(&options.user_agent);
    let proxy = resolve_proxy(&url, options);
    let mut request = WebRequest::new(url, headers, timeout(options.timeout_seconds));
    request.proxy = proxy;
    request.max_redirects = MAX_REDIRECTS;
    let response = options.transport.send(&request).map_err(|error| {
        SearchFailure::Network(friendly_network_error(error.kind, &error.message))
    })?;
    if response.status >= 400 {
        return Err(SearchFailure::Network(status_message(response.status)));
    }
    let text = response.text();
    check_captcha(&text).map_err(SearchFailure::Fatal)?;
    Ok(text)
}

/// 接口型引擎（Tavily / Exa）的检索：一次 `POST`，响应体是 JSON 结果列表。
///
/// 与抓页面型引擎分开：这两条通路不回 HTML，因此没有页面解析与验证码检测；凭据缺失、
/// 鉴权失败或响应结构异常都如实报错，不静默回落到抓页面通路。
fn api_search(
    options: &WebSearchOptions,
    engine: &str,
    query: &str,
    max_results: i64,
) -> ToolOutcome {
    let api_key = options.api_key_for(engine).trim().to_string();
    if api_key.is_empty() {
        return Err(ToolError::new(format!(
            "缺少 {} 的 API Key：请在 config.toml 的 [web_search] 段填写 {}。",
            engine_label(engine),
            key_field(engine)
        )));
    }
    let (url, headers, payload) = api_request(engine, query, max_results, &api_key);
    let started = Instant::now();
    let mut request =
        WebRequest::post_json(url, headers, &payload, timeout(options.timeout_seconds));
    request.proxy = resolve_proxy(&request.url, options);
    let response = options.transport.send(&request).map_err(|error| {
        ToolError::new(format!(
            "搜索失败（{engine}）：{}",
            friendly_network_error(error.kind, &error.message)
        ))
    })?;
    if response.status >= 400 {
        return Err(ToolError::new(format!(
            "搜索失败（{engine}）：{}",
            api_status_message(engine, response.status, &response.text())
        )));
    }
    let results = parse_api_results(engine, &response.text())?;
    if results.is_empty() {
        return Err(ToolError::new("未找到相关搜索结果。"));
    }
    Ok(format_results(
        engine,
        query,
        &results[..results.len().min(max_results as usize)],
        started.elapsed().as_secs_f64(),
    ))
}

/// 一条引擎的请求三件套：地址、请求头与 JSON 请求体。
fn api_request(
    engine: &str,
    query: &str,
    max_results: i64,
    api_key: &str,
) -> (String, Vec<(String, String)>, String) {
    let mut headers = vec![("Content-Type".to_string(), "application/json".to_string())];
    let payload = if engine == "exa" {
        headers.push(("x-api-key".to_string(), api_key.to_string()));
        serde_json::json!({
            "query": query,
            "numResults": max_results,
            "contents": { "text": true },
        })
    } else {
        headers.push(("Authorization".to_string(), format!("Bearer {api_key}")));
        serde_json::json!({
            "query": query,
            "max_results": max_results,
        })
    };
    (endpoint_of(engine), headers, payload.to_string())
}

fn endpoint_of(engine: &str) -> String {
    match engine {
        "exa" => EXA_ENDPOINT.to_string(),
        _ => TAVILY_ENDPOINT.to_string(),
    }
}

fn engine_label(engine: &str) -> &'static str {
    match engine {
        "exa" => "Exa",
        _ => "Tavily",
    }
}

/// 凭据所在的配置键名（缺凭据的报错文案用）。
fn key_field(engine: &str) -> &'static str {
    match engine {
        "exa" => "exa_api_key",
        _ => "tavily_api_key",
    }
}

/// 解析接口返回的 `results` 数组；结构异常（不是 JSON、缺 `results`）如实报错。
fn parse_api_results(engine: &str, body: &str) -> Result<Vec<WebSearchResult>, ToolError> {
    let parsed: Value = serde_json::from_str(body).map_err(|error| {
        ToolError::new(format!("搜索失败（{engine}）：响应不是合法 JSON：{error}"))
    })?;
    let items = parsed
        .get("results")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            ToolError::new(format!(
                "搜索失败（{engine}）：响应缺少 results 数组：{}",
                truncate(body.trim(), 200)
            ))
        })?;
    Ok(items
        .iter()
        .filter_map(|item| {
            let url = item.get("url").and_then(Value::as_str)?.trim().to_string();
            if url.is_empty() {
                return None;
            }
            let title = item
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .trim()
                .to_string();
            // Tavily 的摘要字段是 `content`；Exa 在 `contents.text` 下给正文。
            let snippet = item
                .get("content")
                .and_then(Value::as_str)
                .or_else(|| item.get("text").and_then(Value::as_str))
                .or_else(|| {
                    item.get("contents")
                        .and_then(|contents| contents.get("text"))
                        .and_then(Value::as_str)
                })
                .unwrap_or_default()
                .trim()
                .to_string();
            Some(WebSearchResult {
                title: if title.is_empty() { url.clone() } else { title },
                url,
                snippet: snippet.replace('\n', " "),
            })
        })
        .collect())
}

/// 接口返回的错误说明：优先取响应体里的 `detail.error` / `error` 文案。
fn api_status_message(engine: &str, status: u16, body: &str) -> String {
    let detail = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|parsed| {
            parsed
                .get("detail")
                .and_then(|detail| detail.get("error"))
                .and_then(Value::as_str)
                .or_else(|| parsed.get("error").and_then(Value::as_str))
                .or_else(|| parsed.get("detail").and_then(Value::as_str))
                .map(str::to_string)
        })
        .unwrap_or_else(|| truncate(body.trim(), 200));
    match status {
        401 | 403 => format!(
            "{} 拒绝访问（HTTP {status}），请检查 [web_search] 段里的 {}。",
            engine_label(engine),
            key_field(engine)
        ),
        429 => "请求过于频繁（HTTP 429），请稍后重试。".to_string(),
        other if detail.is_empty() => format!("{} 返回 HTTP {other}。", engine_label(engine)),
        other => format!("{} 返回 HTTP {other}：{detail}", engine_label(engine)),
    }
}

/// `None` 表示自动检测系统代理；`Some("")` 表示禁用；其余为显式地址。
fn resolve_proxy(url: &str, options: &WebSearchOptions) -> Option<String> {
    match options.proxy.as_deref() {
        None => {
            let host = host_of(url).unwrap_or_default();
            if is_private_host(&host) {
                None
            } else {
                detect_windows_proxy()
            }
        }
        Some("") => None,
        Some(explicit) => Some(explicit.to_string()),
    }
}

fn timeout(seconds: f64) -> Duration {
    Duration::from_secs_f64(seconds.max(1.0))
}

fn format_results(engine: &str, query: &str, results: &[WebSearchResult], elapsed: f64) -> String {
    let mut lines = vec![format!(
        "来源：{engine}｜查询：{query}｜用时：{elapsed:.2}s｜共 {} 条",
        results.len()
    )];
    for (index, result) in results.iter().enumerate() {
        let title = result.title.trim().replace('\n', " ");
        lines.push(format!("{}. {}", index + 1, truncate(&title, 150)));
        lines.push(format!("   {}", result.url));
        let snippet = result.snippet.trim().replace('\n', " ");
        if !snippet.is_empty() {
            lines.push(format!("   {}", truncate(&snippet, 300)));
        }
    }
    lines.join("\n")
}

pub fn truncate(text: &str, limit: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= limit {
        return text.to_string();
    }
    let mut head: String = chars[..limit.saturating_sub(1)].iter().collect();
    head.push('…');
    head
}

pub fn clamp_int(value: Option<&Value>, default: i64, minimum: i64, maximum: i64) -> i64 {
    let parsed = match value {
        Some(Value::Bool(flag)) => Some(i64::from(*flag)),
        Some(Value::Number(number)) => number
            .as_i64()
            .or_else(|| number.as_f64().map(|item| item.trunc() as i64)),
        Some(Value::String(text)) => text.trim().parse::<i64>().ok(),
        _ => None,
    };
    parsed
        .map(|item| item.clamp(minimum, maximum))
        .unwrap_or(default)
}

pub fn clamp_float(value: Option<&Value>, default: f64, minimum: f64, maximum: f64) -> f64 {
    let parsed = match value {
        Some(Value::Bool(flag)) => Some(if *flag { 1.0 } else { 0.0 }),
        Some(Value::Number(number)) => number.as_f64(),
        Some(Value::String(text)) => text.trim().parse::<f64>().ok(),
        _ => None,
    };
    parsed
        .map(|item| item.clamp(minimum, maximum))
        .unwrap_or(default)
}

/// `str(value or "").strip()` 的可用子集（数字与布尔也要能读成文本）。
fn argument_text(arguments: &Map<String, Value>, key: &str) -> String {
    match arguments.get(key) {
        None | Some(Value::Null) => String::new(),
        Some(value) => scalar_text(value).trim().to_string(),
    }
}

fn scalar_text(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(text) => text.clone(),
        Value::Bool(flag) => if *flag { "True" } else { "False" }.to_string(),
        other => omnicrawl_controllers::json::python_repr(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn arguments(value: Value) -> Map<String, Value> {
        value.as_object().cloned().unwrap_or_default()
    }

    #[test]
    fn strip_tags_decodes_entities_and_collapses_space() {
        assert_eq!(
            strip_tags("<b>标题</b>&amp;\n  正文&nbsp;结尾"),
            "标题 & 正文 结尾"
        );
    }

    #[test]
    fn yahoo_and_duckduckgo_redirects_are_unwrapped() {
        assert_eq!(
            clean_yahoo_url(
                "https://r.search.yahoo.com/_ylt=x/RU=https%3A%2F%2Fexample.com%2Fa/RK=2"
            ),
            "https://example.com/a"
        );
        assert_eq!(
            clean_duckduckgo_url("//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fb&rut=x"),
            "https://example.com/b"
        );
    }

    #[test]
    fn bing_results_carry_title_url_and_snippet() {
        let html = r#"<li class="b_algo"><h2><a href="https://example.com/1">标题 <b>一</b></a></h2><p>摘要一</p></li>"#;
        let results = parse_results("bing", html);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "标题 一");
        assert_eq!(results[0].url, "https://example.com/1");
        assert_eq!(results[0].snippet, "摘要一");
    }

    #[test]
    fn captcha_pages_are_refused() {
        let error = check_captcha("<html>Please verify you're human</html>")
            .expect_err("拦截特征应当被识别");
        assert!(error.message.contains("人机验证"), "{}", error.message);
    }

    #[test]
    fn engine_and_query_are_validated_before_any_request() {
        let options = WebSearchOptions::default();
        assert_eq!(
            web_search(&options, &arguments(json!({"query": " "})))
                .expect_err("空查询应当被拒绝")
                .message,
            "query 不能为空。"
        );
        assert_eq!(
            web_search(
                &options,
                &arguments(json!({"query": "x", "engine": "google"}))
            )
            .expect_err("不支持的引擎应当被拒绝")
            .message,
            "engine 必须是 bing、duckduckgo、yahoo、tavily、exa 之一。"
        );
    }

    #[test]
    fn default_engine_comes_from_config_and_falls_back() {
        let mut options = WebSearchOptions::default();
        assert_eq!(options.resolve_default_engine(), "bing", "缺配置时用 bing");
        options.default_engine = "  Tavily ".to_string();
        assert_eq!(
            options.resolve_default_engine(),
            "tavily",
            "配置值大小写与空白都归一"
        );
        options.default_engine = "google".to_string();
        assert_eq!(
            options.resolve_default_engine(),
            "bing",
            "配置被改坏时回落到无需凭据的默认引擎"
        );
    }

    #[test]
    fn api_engines_require_their_own_key_before_any_request() {
        let options = WebSearchOptions::default();
        assert_eq!(
            web_search(
                &options,
                &arguments(json!({"query": "x", "engine": "tavily"}))
            )
            .expect_err("缺 Tavily 密钥应当被拒绝")
            .message,
            "缺少 Tavily 的 API Key：请在 config.toml 的 [web_search] 段填写 tavily_api_key。"
        );
        assert_eq!(
            web_search(&options, &arguments(json!({"query": "x", "engine": "exa"})))
                .expect_err("缺 Exa 密钥应当被拒绝")
                .message,
            "缺少 Exa 的 API Key：请在 config.toml 的 [web_search] 段填写 exa_api_key。"
        );
        // 只有 Tavily 的密钥时，Exa 仍然缺凭据（两把钥匙不互相顶替）。
        let only_tavily = WebSearchOptions {
            tavily_api_key: "tvly-x".to_string(),
            ..WebSearchOptions::default()
        };
        assert!(web_search(
            &only_tavily,
            &arguments(json!({"query": "x", "engine": "exa"}))
        )
        .is_err());
    }

    #[test]
    fn api_requests_carry_engine_specific_url_headers_and_body() {
        let (url, headers, payload) = api_request("tavily", "rust 工具", 3, "tvly-key");
        assert_eq!(url, "https://api.tavily.com/search");
        assert_eq!(
            headers,
            vec![
                ("Content-Type".to_string(), "application/json".to_string()),
                ("Authorization".to_string(), "Bearer tvly-key".to_string()),
            ]
        );
        let body: Value = serde_json::from_str(&payload).expect("请求体是 JSON");
        assert_eq!(body["query"], "rust 工具");
        assert_eq!(body["max_results"], 3);

        let (url, headers, payload) = api_request("exa", "rust 工具", 4, "exa-key");
        assert_eq!(url, "https://api.exa.ai/search");
        assert_eq!(
            headers,
            vec![
                ("Content-Type".to_string(), "application/json".to_string()),
                ("x-api-key".to_string(), "exa-key".to_string()),
            ]
        );
        let body: Value = serde_json::from_str(&payload).expect("请求体是 JSON");
        assert_eq!(body["query"], "rust 工具");
        assert_eq!(body["numResults"], 4);
        assert_eq!(body["contents"]["text"], true);
    }

    #[test]
    fn api_results_cover_both_shapes_and_skip_broken_items() {
        let tavily = r#"{"results":[
            {"title":"甲","url":"https://a.example","content":"摘要甲"},
            {"title":"靠 url 兜底","url":"https://b.example"},
            {"title":"没 url","url":""},
            {"title":"缺字段"}
        ]}"#;
        let results = parse_api_results("tavily", tavily).expect("Tavily 结构");
        assert_eq!(results.len(), 2, "没有 url 的条目应当被丢弃");
        assert_eq!(results[0].title, "甲");
        assert_eq!(results[0].snippet, "摘要甲");
        assert_eq!(
            results[1].title, "靠 url 兜底",
            "缺摘要时留下空串而不是报错"
        );

        let exa = r#"{"results":[{"title":"乙","url":"https://c.example","text":"正文乙"}]}"#;
        let results = parse_api_results("exa", exa).expect("Exa 结构");
        assert_eq!(results[0].snippet, "正文乙");

        // Exa 也可能把正文放在嵌套的 `contents.text` 里，一并认。
        let nested = r#"{"results":[{"url":"https://d.example","contents":{"text":"嵌套正文"}}]}"#;
        let results = parse_api_results("exa", nested).expect("Exa 嵌套结构");
        assert_eq!(results[0].snippet, "嵌套正文");
        assert_eq!(results[0].title, "https://d.example", "缺标题时用 url 顶上");

        let error = parse_api_results("tavily", "not json").expect_err("非 JSON 应当报错");
        assert!(
            error.message.contains("响应不是合法 JSON"),
            "{}",
            error.message
        );
        let error = parse_api_results("tavily", r#"{"answer":"x"}"#).expect_err("缺 results");
        assert!(
            error.message.contains("缺少 results 数组"),
            "{}",
            error.message
        );
    }

    #[test]
    fn api_status_messages_explain_auth_and_rate_limits() {
        let unauthorized = api_status_message(
            "tavily",
            401,
            r#"{"detail":{"error":"Unauthorized: missing or invalid API key."}}"#,
        );
        assert!(unauthorized.contains("Tavily 拒绝访问"), "{unauthorized}");
        assert!(unauthorized.contains("tavily_api_key"), "{unauthorized}");

        let exa_forbidden = api_status_message("exa", 403, r#"{"error":"forbidden"}"#);
        assert!(exa_forbidden.contains("exa_api_key"), "{exa_forbidden}");

        let limited = api_status_message("tavily", 429, "");
        assert!(limited.contains("请求过于频繁"), "{limited}");

        let bad_request =
            api_status_message("tavily", 400, r#"{"detail":{"error":"Invalid topic."}}"#);
        assert_eq!(
            bad_request, "Tavily 返回 HTTP 400：Invalid topic.",
            "带明细的错误应当原样带出"
        );
    }
}
