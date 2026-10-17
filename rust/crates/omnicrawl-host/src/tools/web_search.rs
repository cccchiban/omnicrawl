//! 网页搜索工具执行体：查询搜索引擎并返回标题、链接与摘要。
//!
//! 抓页面型引擎（Bing / DuckDuckGo / GitHub / arXiv / Hugging Face）的语义基准是
//! `omnicrawl/net/web_search.py`：同一套端点与查询参数、同一套页面解析规则（正则而非 DOM）、
//! 同一套拦截检测与文案，只访问公开搜索页面，检测到
//! 验证码或 JS 挑战即如实报错、不绕过。接口型引擎（Tavily / Exa / Firecrawl）走各自的
//! JSON 检索接口，凭据取 `[web_search]` 段（见 [`WebSearchOptions`]）。
//!
//! `mixed` 是混合搜索：并发查询全部可用渠道，按 URL 去重后合并成一份结果。
//!
//! 请求走 [`super::wreq_transport::WreqWebTransport`]（wreq + wreq-util 设备档案），与
//! `fetcher` 同一个「模拟有头浏览器」环境：TLS/HTTP2 指纹与 UA、Client Hints 同源，
//! 不做手写头拼接——手写会把档案值顶掉，反而暴露成非浏览器流量。

use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;
use std::thread::sleep;
use std::time::{Duration, Instant};

use omnicrawl_config::features::web_search::WEB_SEARCH_ENGINES;
use regex::Regex;
use scraper::Html;
use serde_json::{Map, Value};

use super::error::{ToolError, ToolOutcome};
use super::web_transport::{
    detect_windows_proxy, host_of, is_private_host, WebErrorKind, WebRequest, WebTransport,
    MAX_REDIRECTS,
};
use super::wreq_transport::{normalize_impersonate, WreqWebTransport};

pub const DEFAULT_TIMEOUT_SECONDS: f64 = 10.0;
/// 单次搜索返回的条数上限，同时是调用方未指定 `max_results` 时的默认条数。
pub const MAX_RESULTS_PER_ENGINE: i64 = 20;
/// 支持的全部引擎；清单的唯一来源在配置域（界面候选与校验读同一张表）。
pub const SUPPORTED_ENGINES: [&str; 9] = WEB_SEARCH_ENGINES;

/// 混合搜索的引擎名（并发多路 + 去重合并）。
pub const MIXED_ENGINE: &str = omnicrawl_config::features::web_search::MIXED_WEB_SEARCH_ENGINE;

/// 抓页面型引擎：直接抓公开搜索页并解析，无需凭据。
///
/// 顺序即混合搜索的请求顺序（也是失败清单的排列顺序）。
const PAGE_ENGINES: [&str; 5] = ["bing", "duckduckgo", "github", "arxiv", "huggingface"];
/// 混合搜索参与的接口型渠道（需要各自配置凭据，未配的跳过）。
const API_ENGINES: [&str; 3] = ["tavily", "exa", "firecrawl"];

/// Tavily 检索接口（`POST`，Bearer 鉴权）。
const TAVILY_ENDPOINT: &str = "https://api.tavily.com/search";
/// Exa 检索接口（`POST`，`x-api-key` 鉴权）。
const EXA_ENDPOINT: &str = "https://api.exa.ai/search";
/// Firecrawl 检索接口（`POST`，Bearer 鉴权）。
const FIRECRAWL_ENDPOINT: &str = "https://api.firecrawl.dev/v1/search";

/// 拦截特征：命中即判定为验证码 / JS 挑战页，如实报错而不尝试绕过。
///
/// 只列**拦截页专有**的文案。像 `captcha`、`awswaf`、`challenge.js` 这类词在正常页面里也
/// 会作为站点配置出现（Hugging Face 的 `captchaApiKey`、GitHub 的 `octocaptcha…` 特性名），
/// 拿它们做判据会把正常结果页误判成拦截页。
const INTERCEPTION_MARKERS: [&str; 7] = [
    "unusual traffic",
    "enable cookies",
    "verify you're human",
    "verify that you're not a robot",
    "javascript is disabled",
    "请输入验证码",
    "人机验证",
];

/// 仅用于验证码挑战页的特征（reCAPTCHA 表单与异常流量提示页）。
const CAPTCHA_PAGE_MARKERS: [&str; 3] = [
    "g-recaptcha",
    "recaptcha/api.js",
    "detected unusual traffic",
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
    /// Firecrawl API Key（空串表示未配置）。
    pub firecrawl_api_key: String,
    /// 混合搜索的并发度数（每路一个线程）；0 与 1 都表示串行。
    pub mixed_parallelism: usize,
    /// 抓页面型引擎用的浏览器指纹档案（`chrome`/`firefox`/`safari`/`edge`）。
    ///
    /// 与 `fetcher` 的 `impersonate` 同一口径：默认 `chrome`，由
    /// [`super::wreq_transport::normalize_impersonate`] 归一化后交给指纹传输挑设备档案。
    pub impersonate: String,
}

impl Default for WebSearchOptions {
    fn default() -> Self {
        Self {
            // 默认就走浏览器指纹传输：抓页面型引擎所在站点会按 TLS 指纹与 HTTP2 特征
            // 判别客户端，ureq+rustls 的库指纹会被直接拦下。
            transport: Arc::new(WreqWebTransport::new()),
            user_agent: super::web_transport::DEFAULT_USER_AGENT.to_string(),
            timeout_seconds: DEFAULT_TIMEOUT_SECONDS,
            max_retries: 1,
            request_delay_seconds: 0.0,
            proxy: None,
            default_engine: DEFAULT_ENGINE.to_string(),
            tavily_api_key: String::new(),
            exa_api_key: String::new(),
            firecrawl_api_key: String::new(),
            mixed_parallelism: 4,
            impersonate: super::wreq_transport::DEFAULT_IMPERSONATE.to_string(),
        }
    }
}

impl WebSearchOptions {
    /// 指定引擎下生效的 API Key；抓页面型引擎没有凭据，返回空串。
    pub fn api_key_for(&self, engine: &str) -> &str {
        match engine {
            "tavily" => self.tavily_api_key.as_str(),
            "exa" => self.exa_api_key.as_str(),
            "firecrawl" => self.firecrawl_api_key.as_str(),
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
        firecrawl_api_key: configuration.firecrawl_api_key,
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
    let document = Html::parse_fragment(text);
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

fn parse_results(engine: &str, html_text: &str) -> Vec<WebSearchResult> {
    match engine {
        "duckduckgo" => parse_duckduckgo(html_text),
        "github" => parse_github(html_text),
        "arxiv" => parse_arxiv(html_text),
        "huggingface" => parse_huggingface(html_text),
        _ => parse_bing(html_text),
    }
}

/// 去掉结果高亮包裹标签（`<em>`、`<span class="search-hit">` 等）而**不**插入空格。
///
/// 搜索引擎把命中词裹一层标记：直接交给 [`strip_tags`] 会把词切开
/// （`flosse/<em>rust</em>-web` → `flosse/ rust -web`），标题里的人为空格比丢标记更糟。
fn strip_highlight(text: &str) -> String {
    let highlight =
        Regex::new(r"(?i)</?(?:em|b|strong|i|mark|span|u)\b[^>]*>").expect("高亮标签正则应当合法");
    strip_tags(&highlight.replace_all(text, ""))
}

/// GitHub 结果页：每条结果一个 `div.resultRow`，标题在 `.search-title` 里，摘要在 `.Content` 里。
fn parse_github(html_text: &str) -> Vec<WebSearchResult> {
    let anchor_re = Regex::new(
        r#"<div class="search-title[^"]*"[^>]*>\s*<a[^>]*href="([^"]+)"[^>]*>(.*?)</a>"#,
    )
    .expect("github 锚点");
    let description_re =
        Regex::new(r#"<div class="Content[^"]*"[^>]*>(.*?)</div>"#).expect("github 摘要");

    let mut results = Vec::new();
    for block in html_text.split("class=\"resultRow").skip(1) {
        let Some(anchor) = anchor_re.captures(block) else {
            continue;
        };
        let href = anchor[1].trim();
        // 只要仓库首页链接（`/owner/repo`）；其余是语言、话题、star 等附属链接。
        let parts: Vec<&str> = href.trim_matches('/').split('/').collect();
        if !href.starts_with('/') || parts.len() != 2 || parts.iter().any(|part| part.is_empty()) {
            continue;
        }
        let title = strip_highlight(&anchor[2]);
        if title.is_empty() {
            continue;
        }
        let snippet = description_re
            .captures(block)
            .map(|description| strip_highlight(&description[1]))
            .unwrap_or_default();
        results.push(WebSearchResult {
            title,
            url: format!("https://github.com{href}"),
            snippet,
        });
    }
    results
}

/// arXiv 结果页：每条结果一个 `li.arxiv-result`，标题 `.title is-5`，摘要 `.abstract-full`。
fn parse_arxiv(html_text: &str) -> Vec<WebSearchResult> {
    let block_re = Regex::new(r#"(?s)<li class="arxiv-result">(.*?)</li>"#).expect("arxiv 区块");
    let title_re = Regex::new(r#"(?s)<p class="title is-5[^"]*">(.*?)</p>"#).expect("arxiv 标题");
    let link_re = Regex::new(r#"(?s)<p class="list-title[^"]*">\s*<a[^>]*href="([^"]+)""#)
        .expect("arxiv 链接");
    let full_re =
        Regex::new(r#"(?s)<span class="abstract-full[^"]*"[^>]*>(.*?)</p>"#).expect("arxiv 全文");
    let short_re =
        Regex::new(r#"(?s)<span class="abstract-short[^"]*"[^>]*>(.*?)<a class="is-size-7""#)
            .expect("arxiv 摘要");

    let mut results = Vec::new();
    for block in block_re.captures_iter(html_text) {
        let block = &block[1];
        let (Some(title), Some(link)) = (title_re.captures(block), link_re.captures(block)) else {
            continue;
        };
        let title = strip_highlight(&title[1]);
        let url = link[1].trim().to_string();
        if !url.starts_with("http") || title.is_empty() {
            continue;
        }
        // 折叠态（`abstract-short`）以省略号截断，展开态才是完整摘要：优先后者。
        let snippet = full_re
            .captures(block)
            .or_else(|| short_re.captures(block))
            .map(|abstract_text| strip_highlight(&abstract_text[1]))
            .unwrap_or_default();
        results.push(WebSearchResult {
            title,
            url,
            snippet,
        });
    }
    results
}

/// Hugging Face 模型列表页：每条结果一个 `article.overview-card-wrapper`，元信息在第二个描述行里。
fn parse_huggingface(html_text: &str) -> Vec<WebSearchResult> {
    let article_re =
        Regex::new(r#"(?s)<article[^>]*class="overview-card-wrapper[^"]*"[^>]*>(.*?)</article>"#)
            .expect("hf 结果块");
    let link_re = Regex::new(r#"<a[^>]*href="([^"]+)""#).expect("hf 链接");
    let title_re = Regex::new(r#"(?s)<h4[^>]*>(.*?)</h4>"#).expect("hf 标题");
    let meta_re = Regex::new(
        r#"(?s)class="mr-1 flex items-center overflow-hidden whitespace-nowrap[^"]*"[^>]*>(.*?)</div>"#,
    )
    .expect("hf 元信息");
    let separator_re = Regex::new(r"(?:\s*•\s*)+").expect("hf 分隔符");

    let mut results = Vec::new();
    for article in article_re.captures_iter(html_text) {
        let article = &article[1];
        let (Some(link), Some(title)) = (link_re.captures(article), title_re.captures(article))
        else {
            continue;
        };
        let href = link[1].trim();
        let title = strip_tags(&title[1]);
        if !href.starts_with('/') || title.is_empty() {
            continue;
        }
        // 元信息里任务、参数量、更新时间与下载量之间用图标分隔，取文本后会留下一串孤立的 `•`。
        let snippet = meta_re
            .captures(article)
            .map(|meta| {
                separator_re
                    .replace_all(&strip_tags(&meta[1]), " • ")
                    .trim_matches(|character: char| character == '•' || character.is_whitespace())
                    .to_string()
            })
            .unwrap_or_default();
        results.push(WebSearchResult {
            title,
            url: format!("https://huggingface.co{href}"),
            snippet,
        });
    }
    results
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
    let query = query.to_string();
    match engine {
        "duckduckgo" => {
            let lookup = language.unwrap_or("zh-CN").trim().to_lowercase();
            let region = DDG_REGIONS
                .iter()
                .find(|(key, _)| *key == lookup)
                .map(|(_, value)| (*value).to_string())
                .unwrap_or_else(|| "us-en".to_string());
            (
                "https://html.duckduckgo.com/html/".to_string(),
                vec![("q".to_string(), query), ("kl".to_string(), region)],
            )
        }
        // GitHub 搜索页：仓库搜索是站点默认口径（`type=repositories`）。
        "github" => (
            "https://github.com/search".to_string(),
            vec![
                ("q".to_string(), query),
                ("type".to_string(), "repositories".to_string()),
            ],
        ),
        // arXiv 检索页：`searchtype=all` 对应全字段检索。
        "arxiv" => (
            "https://arxiv.org/search/".to_string(),
            vec![
                ("searchtype".to_string(), "all".to_string()),
                ("query".to_string(), query),
            ],
        ),
        // Hugging Face 模型列表页：`search` 即关键词过滤。
        "huggingface" => (
            "https://huggingface.co/models".to_string(),
            vec![("search".to_string(), query)],
        ),
        _ => (
            "https://www.bing.com/search".to_string(),
            vec![
                ("q".to_string(), query),
                ("count".to_string(), MAX_RESULTS_PER_ENGINE.to_string()),
                (
                    "setlang".to_string(),
                    language.unwrap_or("zh-CN").to_string(),
                ),
            ],
        ),
    }
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

/// 拦截检测：页面里出现验证码、异常流量或 JS 挑战的专有文案就如实报错，不尝试绕过。
fn check_interception(text: &str) -> Result<(), ToolError> {
    let chars: Vec<char> = text.chars().collect();
    let head: String = chars.iter().take(4000).collect();
    let tail: String = chars
        .iter()
        .skip(chars.len().saturating_sub(1000))
        .collect();
    let low = format!("{head}{tail}").to_lowercase();
    if INTERCEPTION_MARKERS
        .iter()
        .chain(CAPTCHA_PAGE_MARKERS.iter())
        .any(|marker| low.contains(marker))
    {
        return Err(ToolError::new(
            "搜索引擎要求人机验证、JS 挑战或检测到异常流量，已停止请求（不会绕过验证）。\
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
    let max_results = clamp_int(
        arguments.get("max_results"),
        MAX_RESULTS_PER_ENGINE,
        1,
        MAX_RESULTS_PER_ENGINE,
    );
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

    if engine == MIXED_ENGINE {
        return mixed_search(options, &query, max_results, language.as_deref());
    }

    if matches!(engine.as_str(), "tavily" | "exa" | "firecrawl") {
        return api_search(options, &engine, &query, max_results);
    }

    let started = Instant::now();
    let results = page_results(options, &engine, &query, language.as_deref())?;
    if results.is_empty() {
        return Err(ToolError::new("未找到相关搜索结果（或页面结构无法解析）。"));
    }
    let elapsed = started.elapsed().as_secs_f64();
    Ok(format_results(
        &engine,
        &query,
        &results[..results.len().min(max_results as usize)],
        elapsed,
    ))
}

/// 抓页面型引擎的单路检索：带重试，返回解析后的结果（可能为空，截断与空结果判定交给调用方）。
fn page_results(
    options: &WebSearchOptions,
    engine: &str,
    query: &str,
    language: Option<&str>,
) -> Result<Vec<WebSearchResult>, ToolError> {
    let mut last_error: Option<String> = None;
    for attempt in 0..=options.max_retries {
        match request_html(options, engine, query, language) {
            Ok(html_text) => return Ok(parse_results(engine, &html_text)),
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

/// 单路检索：抓页面型与接口型统一入口（混合搜索按路并发调用它）。
fn channel_results(
    options: &WebSearchOptions,
    engine: &str,
    query: &str,
    max_results: i64,
    language: Option<&str>,
) -> Result<Vec<WebSearchResult>, ToolError> {
    if API_ENGINES.contains(&engine) {
        api_results(options, engine, query, max_results)
    } else {
        page_results(options, engine, query, language)
    }
}

/// 混合搜索：并发查询全部可用渠道，按 URL 去重后合并，最终条数截断到 `max_results`。
///
/// 「可用」= 两个抓页面型渠道，加上已配置凭据的接口型渠道；凭据未配置的接口型渠道直接
/// 跳过（不是失败）。单路失败只记录在案的失败清单里并继续合并其余渠道，全部失败才报错。
fn mixed_search(
    options: &WebSearchOptions,
    query: &str,
    max_results: i64,
    language: Option<&str>,
) -> ToolOutcome {
    let engines = mixed_engines(options);
    let started = Instant::now();
    let mut merged: Vec<MergedResult> = Vec::new();
    let mut index_of: HashMap<String, usize> = HashMap::new();
    let mut served: Vec<&str> = Vec::new();
    let mut failures: Vec<String> = Vec::new();
    let mut duplicates = 0usize;

    for batch in engines.chunks(options.mixed_parallelism.max(1)) {
        let collected: Vec<Result<Vec<WebSearchResult>, ToolError>> = std::thread::scope(|scope| {
            let handles: Vec<_> = batch
                .iter()
                .copied()
                .map(|engine| {
                    scope.spawn(move || {
                        channel_results(options, engine, query, max_results, language)
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|handle| {
                    handle
                        .join()
                        .unwrap_or_else(|_| Err(ToolError::new("搜索线程异常终止。")))
                })
                .collect()
        });
        for (engine, outcome) in batch.iter().copied().zip(collected) {
            match outcome {
                Ok(results) => {
                    // 请求成功即算「参与过」：即便一条都没解析出来，也要出现在表头里。
                    served.push(engine);
                    for result in results {
                        let key = url_key(&result.url);
                        let existing = index_of.get(&key).copied();
                        match existing {
                            Some(index) => {
                                duplicates += 1;
                                merged[index].merge(&result, engine);
                            }
                            None => {
                                index_of.insert(key, merged.len());
                                merged.push(MergedResult::new(result, engine));
                            }
                        }
                    }
                }
                Err(error) => failures.push(failure_note(engine, &error.message)),
            }
        }
    }

    if served.is_empty() {
        return Err(ToolError::new(format!(
            "混合搜索全部渠道均未返回结果：{}",
            failures.join("；")
        )));
    }
    if merged.is_empty() {
        return Err(ToolError::new(format!(
            "未找到相关搜索结果（已请求 {}）。",
            served.join("、")
        )));
    }
    let elapsed = started.elapsed().as_secs_f64();
    Ok(format_mixed_results(
        &served,
        &failures,
        query,
        &merged[..merged.len().min(max_results as usize)],
        duplicates,
        elapsed,
    ))
}

/// 混合搜索参与的路：抓页面型全上，接口型只上有凭据的。
fn mixed_engines(options: &WebSearchOptions) -> Vec<&'static str> {
    let mut engines: Vec<&'static str> = PAGE_ENGINES.to_vec();
    for engine in API_ENGINES {
        if !options.api_key_for(engine).trim().is_empty() {
            engines.push(engine);
        }
    }
    engines
}

/// 合并中的一条结果：保留首次出现的标题与摘要，记下提供过它的渠道。
struct MergedResult {
    result: WebSearchResult,
    sources: Vec<&'static str>,
}

impl MergedResult {
    fn new(result: WebSearchResult, engine: &'static str) -> Self {
        Self {
            result,
            sources: vec![engine],
        }
    }

    /// 重复 URL 的合并：补上已有摘要的缺口，并把它算作这条结果的来源之一。
    fn merge(&mut self, other: &WebSearchResult, engine: &'static str) {
        if self.result.snippet.trim().is_empty() && !other.snippet.trim().is_empty() {
            self.result.snippet = other.snippet.clone();
        }
        if self.result.title.trim().is_empty() && !other.title.trim().is_empty() {
            self.result.title = other.title.clone();
        }
        if !self.sources.contains(&engine) {
            self.sources.push(engine);
        }
    }
}

/// 去重键：忽略首尾空白、大小写与末尾斜杠，其余按原样比对。
fn url_key(url: &str) -> String {
    let trimmed = url.trim();
    trimmed.strip_suffix('/').unwrap_or(trimmed).to_lowercase()
}

/// 单条失败说明：接口型通路的错误文案已自带「搜索失败（engine）：」前缀，这里剥掉再带上渠道名。
fn failure_note(engine: &str, message: &str) -> String {
    let text = one_line(message);
    let prefix = format!("搜索失败（{engine}）：");
    let detail = text.strip_prefix(prefix.as_str()).unwrap_or(text.as_str());
    format!("{engine}：{detail}")
}

fn one_line(text: &str) -> String {
    text.replace('\n', " ").trim().to_string()
}

fn request_html(
    options: &WebSearchOptions,
    engine: &str,
    query: &str,
    language: Option<&str>,
) -> Result<String, SearchFailure> {
    let (base, params) = endpoint(engine, query, language);
    let url = build_url(&base, &params);
    // 与 fetcher 同一口径的「模拟有头浏览器」请求：走传输层给的导航头（指纹档案传输只补
    // 档案没带的头），并把档案名一并交给传输层挑设备档案。
    let impersonate = normalize_impersonate(Some(&Value::String(options.impersonate.clone())));
    let headers = options
        .transport
        .navigation_headers(&options.user_agent, Some(impersonate.as_str()));
    let proxy = resolve_proxy(&url, options);
    let mut request = WebRequest::new(url, headers, timeout(options.timeout_seconds));
    request.proxy = proxy;
    request.max_redirects = MAX_REDIRECTS;
    request.impersonate = Some(impersonate);
    let response = options.transport.send(&request).map_err(|error| {
        SearchFailure::Network(friendly_network_error(error.kind, &error.message))
    })?;
    if response.status >= 400 {
        return Err(SearchFailure::Network(status_message(response.status)));
    }
    let text = response.text();
    check_interception(&text).map_err(SearchFailure::Fatal)?;
    Ok(text)
}

/// 接口型引擎（Tavily / Exa / Firecrawl）的检索：一次 `POST`，响应体是 JSON 结果列表。
///
/// 与抓页面型引擎分开：这三条通路不回 HTML，因此没有页面解析与验证码检测；凭据缺失、
/// 鉴权失败或响应结构异常都如实报错，不静默回落到抓页面通路。
fn api_search(
    options: &WebSearchOptions,
    engine: &str,
    query: &str,
    max_results: i64,
) -> ToolOutcome {
    let started = Instant::now();
    let results = api_results(options, engine, query, max_results)?;
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

/// 接口型引擎的单路检索：返回结果（可能为空，截断与空结果判定交给调用方）。
fn api_results(
    options: &WebSearchOptions,
    engine: &str,
    query: &str,
    max_results: i64,
) -> Result<Vec<WebSearchResult>, ToolError> {
    let api_key = options.api_key_for(engine).trim().to_string();
    if api_key.is_empty() {
        return Err(ToolError::new(format!(
            "缺少 {} 的 API Key：请在 config.toml 的 [web_search] 段填写 {}。",
            engine_label(engine),
            key_field(engine)
        )));
    }
    let (url, headers, payload) = api_request(engine, query, max_results, &api_key);
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
    Ok(results)
}

/// 一条引擎的请求三件套：地址、请求头与 JSON 请求体。
fn api_request(
    engine: &str,
    query: &str,
    max_results: i64,
    api_key: &str,
) -> (String, Vec<(String, String)>, String) {
    let mut headers = vec![("Content-Type".to_string(), "application/json".to_string())];
    let payload = match engine {
        "exa" => {
            headers.push(("x-api-key".to_string(), api_key.to_string()));
            serde_json::json!({
                "query": query,
                "numResults": max_results,
                "contents": { "text": true },
            })
        }
        "firecrawl" => {
            headers.push(("Authorization".to_string(), format!("Bearer {api_key}")));
            serde_json::json!({
                "query": query,
                "limit": max_results,
            })
        }
        _ => {
            headers.push(("Authorization".to_string(), format!("Bearer {api_key}")));
            serde_json::json!({
                "query": query,
                "max_results": max_results,
            })
        }
    };
    (endpoint_of(engine), headers, payload.to_string())
}

fn endpoint_of(engine: &str) -> String {
    match engine {
        "exa" => EXA_ENDPOINT.to_string(),
        "firecrawl" => FIRECRAWL_ENDPOINT.to_string(),
        _ => TAVILY_ENDPOINT.to_string(),
    }
}

fn engine_label(engine: &str) -> &'static str {
    match engine {
        "exa" => "Exa",
        "firecrawl" => "Firecrawl",
        _ => "Tavily",
    }
}

/// 凭据所在的配置键名（缺凭据的报错文案用）。
fn key_field(engine: &str) -> &'static str {
    match engine {
        "exa" => "exa_api_key",
        "firecrawl" => "firecrawl_api_key",
        _ => "tavily_api_key",
    }
}

/// 解析接口返回的结果数组；结构异常（不是 JSON、缺结果数组）如实报错。
///
/// Tavily / Exa 把列表放在 `results`，Firecrawl 放在 `data`。
fn parse_api_results(engine: &str, body: &str) -> Result<Vec<WebSearchResult>, ToolError> {
    let parsed: Value = serde_json::from_str(body).map_err(|error| {
        ToolError::new(format!("搜索失败（{engine}）：响应不是合法 JSON：{error}"))
    })?;
    let field = if engine == "firecrawl" {
        "data"
    } else {
        "results"
    };
    let items = parsed.get(field).and_then(Value::as_array).ok_or_else(|| {
        ToolError::new(format!(
            "搜索失败（{engine}）：响应缺少 {field} 数组：{}",
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
            // Tavily 的摘要字段是 `content`；Exa 在 `contents.text` 下给正文；
            // Firecrawl 是 `description`（开了抓取时另有 `markdown`）。
            let snippet = item
                .get("content")
                .and_then(Value::as_str)
                .or_else(|| item.get("text").and_then(Value::as_str))
                .or_else(|| item.get("description").and_then(Value::as_str))
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

/// 混合搜索的输出：表头写明参与的渠道、去重掉多少条、哪些渠道失败。
fn format_mixed_results(
    served: &[&str],
    failures: &[String],
    query: &str,
    results: &[MergedResult],
    duplicates: usize,
    elapsed: f64,
) -> String {
    let mut lines = vec![format!(
        "来源：mixed（{}）｜查询：{query}｜用时：{elapsed:.2}s｜共 {} 条（去重 {duplicates} 条）",
        served.join("+"),
        results.len()
    )];
    if !failures.is_empty() {
        lines.push(format!("失败渠道：{}", failures.join("；")));
    }
    for (index, merged) in results.iter().enumerate() {
        let title = merged.result.title.trim().replace('\n', " ");
        lines.push(format!("{}. {}", index + 1, truncate(&title, 150)));
        lines.push(format!("   {}", merged.result.url));
        let snippet = merged.result.snippet.trim().replace('\n', " ");
        if !snippet.is_empty() {
            lines.push(format!("   {}", truncate(&snippet, 300)));
        }
        if merged.sources.len() > 1 {
            lines.push(format!("   来源：{}", merged.sources.join("+")));
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
    use crate::tools::web_transport::{WebError, WebResponse};
    use serde_json::json;
    use std::sync::Mutex;

    fn arguments(value: Value) -> Map<String, Value> {
        value.as_object().cloned().unwrap_or_default()
    }

    /// 按 URL 片段分发的传输桩：命中不了任何路由就报连接失败（用来造单路失败）。
    struct RouteTransport {
        routes: Mutex<Vec<(String, String)>>,
        requests: Mutex<Vec<String>>,
    }

    impl RouteTransport {
        fn new(routes: &[(&str, &str)]) -> Self {
            Self {
                routes: Mutex::new(
                    routes
                        .iter()
                        .map(|(marker, body)| ((*marker).to_string(), (*body).to_string()))
                        .collect(),
                ),
                requests: Mutex::new(Vec::new()),
            }
        }

        fn take_requests(&self) -> Vec<String> {
            std::mem::take(&mut *self.requests.lock().expect("请求锁"))
        }
    }

    impl WebTransport for RouteTransport {
        fn send(&self, request: &WebRequest) -> Result<WebResponse, WebError> {
            self.requests
                .lock()
                .expect("请求锁")
                .push(request.url.clone());
            let routes = self.routes.lock().expect("路由锁");
            for (marker, body) in routes.iter() {
                if request.url.contains(marker.as_str()) {
                    return Ok(WebResponse {
                        status: 200,
                        final_url: request.url.clone(),
                        body: body.clone().into_bytes(),
                    });
                }
            }
            Err(WebError::new(WebErrorKind::Connect, "没有匹配的路由"))
        }
    }

    /// 混合搜索用例的选项：桩 + 关代理 + 不重试 + 串行（请求集合可比）。
    fn route_options(transport: Arc<RouteTransport>) -> WebSearchOptions {
        WebSearchOptions {
            transport,
            proxy: Some(String::new()),
            max_retries: 0,
            mixed_parallelism: 1,
            ..WebSearchOptions::default()
        }
    }

    /// 混合搜索里「该渠道参与、但一条结果都没解析出来」用的页面。
    const EMPTY_PAGE: &str = "<html><body>没有结果</body></html>";

    /// 把输出里的耗时字段抹平（真实时钟读数不参与断言）。
    fn normalize(text: &str) -> String {
        Regex::new(r"用时：[\d.]+s")
            .expect("耗时正则")
            .replace_all(text, "用时：{ELAPSED}s")
            .to_string()
    }

    #[test]
    fn strip_tags_decodes_entities_and_collapses_space() {
        assert_eq!(
            strip_tags("<b>标题</b>&amp;\n  正文&nbsp;结尾"),
            "标题 & 正文 结尾"
        );
    }

    #[test]
    fn duckduckgo_redirects_are_unwrapped() {
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
        let error = check_interception("<html>Please verify you're human</html>")
            .expect_err("拦截特征应当被识别");
        assert!(error.message.contains("人机验证"), "{}", error.message);
        // AWS WAF 挑战页（无 JS 客户端拿到的挑战页）同样按拦截处理。
        assert!(check_interception("<noscript>JavaScript is disabled</noscript>").is_err());
        // 正常页面里的同名词不算拦截：HF 的 `captchaApiKey`、GitHub 的 `octocaptcha…` 特性名。
        assert!(check_interception(
            r#"{"captchaApiKey":"5bd0","features":["octocaptcha_origin"]}"#
        )
        .is_ok());
        assert!(check_interception("<html><body><li>正常结果</li></body></html>").is_ok());
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
                &arguments(json!({"query": "x", "engine": "not_an_engine"}))
            )
            .expect_err("不支持的引擎应当被拒绝")
            .message,
            "engine 必须是 bing、duckduckgo、github、arxiv、huggingface、\
tavily、exa、firecrawl、mixed 之一。"
        );
        // 雅虎已下线：它不再是合法引擎。
        assert!(web_search(
            &options,
            &arguments(json!({"query": "x", "engine": "yahoo"}))
        )
        .is_err());
    }

    #[test]
    fn max_results_defaults_to_the_cap_and_is_clamped() {
        assert_eq!(MAX_RESULTS_PER_ENGINE, 20);
        // 没给 max_results：默认就是上限（20）。
        assert_eq!(
            clamp_int(None, MAX_RESULTS_PER_ENGINE, 1, MAX_RESULTS_PER_ENGINE),
            20
        );
        // 超出上限夹到 20，下限夹到 1。
        assert_eq!(
            clamp_int(
                Some(&json!(99)),
                MAX_RESULTS_PER_ENGINE,
                1,
                MAX_RESULTS_PER_ENGINE
            ),
            20
        );
        assert_eq!(
            clamp_int(
                Some(&json!(0)),
                MAX_RESULTS_PER_ENGINE,
                1,
                MAX_RESULTS_PER_ENGINE
            ),
            1
        );
    }

    #[test]
    fn bing_endpoint_asks_for_the_cap() {
        let (url, params) = endpoint("bing", "rust", None);
        assert_eq!(url, "https://www.bing.com/search");
        assert!(
            params.contains(&("count".to_string(), "20".to_string())),
            "{params:?}"
        );
    }

    #[test]
    fn page_engines_use_their_site_search_endpoints() {
        let (url, params) = endpoint("github", "rust", None);
        assert_eq!(url, "https://github.com/search");
        assert!(params.contains(&("type".to_string(), "repositories".to_string())));
        assert!(params.contains(&("q".to_string(), "rust".to_string())));

        let (url, params) = endpoint("arxiv", "rust", None);
        assert_eq!(url, "https://arxiv.org/search/");
        assert!(params.contains(&("searchtype".to_string(), "all".to_string())));
        assert!(params.contains(&("query".to_string(), "rust".to_string())));

        let (url, params) = endpoint("huggingface", "bert", None);
        assert_eq!(url, "https://huggingface.co/models");
        assert_eq!(params, vec![("search".to_string(), "bert".to_string())]);
    }

    #[test]
    fn github_search_rows_become_owner_repo_results() {
        let html = r#"<div class="resultRow__x"><h3 class="header__y"><div class="search-title title__z">
<a class="prc-Link" href="/rwf2/Rocket"><span>rwf2/Rocket</span></a></div></h3>
<div class="Content__a"><span>A <em>web</em> <em>framework</em> for Rust.</span></div>
<ul class="footer__b"><li><a href="/topics/rust">rust</a></li></ul></div>"#;
        let results = parse_results("github", html);
        assert_eq!(results.len(), 1);
        // 高亮标签直接删掉而不是换成空格：`rust-web-framework` 不能被切成 `rust - web`。
        assert_eq!(results[0].title, "rwf2/Rocket");
        assert_eq!(results[0].url, "https://github.com/rwf2/Rocket");
        assert_eq!(results[0].snippet, "A web framework for Rust.");

        // 语言、话题等附属链接（非 `/owner/repo`）不得被当成结果。
        let noise = r#"<div class="resultRow__x"><a href="/topics/rust">rust</a></div>"#;
        assert!(parse_results("github", noise).is_empty());
    }

    #[test]
    fn arxiv_results_prefer_the_full_abstract_and_skip_broken_blocks() {
        let html = r#"<li class="arxiv-result">
<p class="list-title is-inline-block"><a href="https://arxiv.org/abs/2610.22367">arXiv:2610.22367</a></p>
<p class="title is-5 mathjax">Discovering <span class="search-hit mathjax">large</span> models</p>
<p class="abstract mathjax"><span class="abstract-short">短摘要&hellip;</span>
<span class="abstract-full">完整摘要文本</span></p></li>
<li class="arxiv-result"><p class="title is-5">缺链接</p></li>"#;
        let results = parse_results("arxiv", html);
        assert_eq!(results.len(), 1, "缺链接的结果块应当被跳过");
        assert_eq!(results[0].title, "Discovering large models");
        assert_eq!(results[0].url, "https://arxiv.org/abs/2610.22367");
        assert_eq!(results[0].snippet, "完整摘要文本");
    }

    #[test]
    fn huggingface_cards_keep_the_meta_line_as_snippet() {
        let html = r#"<article class="overview-card-wrapper group/repo"><a href="/google-bert/bert-base-cased">
<header><h4 class="text-md">google-bert/bert-base-cased</h4></header>
<div class="mr-1 flex items-center overflow-hidden whitespace-nowrap text-sm"><svg></svg>Fill-Mask
<span>•</span> 0.1B <span>•</span> Updated Feb 19, 2024</div></a></article>"#;
        let results = parse_results("huggingface", html);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "google-bert/bert-base-cased");
        assert_eq!(
            results[0].url,
            "https://huggingface.co/google-bert/bert-base-cased"
        );
        assert_eq!(
            results[0].snippet,
            "Fill-Mask • 0.1B • Updated Feb 19, 2024"
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
        options.default_engine = "not_an_engine".to_string();
        assert_eq!(
            options.resolve_default_engine(),
            "bing",
            "配置被改坏时回落到无需凭据的默认引擎"
        );
        // 新引擎也是合法配置值：不再落到兜底分支。
        options.default_engine = " HuggingFace ".to_string();
        assert_eq!(options.resolve_default_engine(), "huggingface");
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
        assert_eq!(
            web_search(
                &options,
                &arguments(json!({"query": "x", "engine": "firecrawl"}))
            )
            .expect_err("缺 Firecrawl 密钥应当被拒绝")
            .message,
            "缺少 Firecrawl 的 API Key：请在 config.toml 的 [web_search] 段填写 firecrawl_api_key。"
        );
        // 只有 Tavily 的密钥时，Exa 仍然缺凭据（三把钥匙不互相顶替）。
        let only_tavily = WebSearchOptions {
            tavily_api_key: "tvly-x".to_string(),
            ..WebSearchOptions::default()
        };
        assert!(web_search(
            &only_tavily,
            &arguments(json!({"query": "x", "engine": "exa"}))
        )
        .is_err());
        assert!(web_search(
            &only_tavily,
            &arguments(json!({"query": "x", "engine": "firecrawl"}))
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

        let (url, headers, payload) = api_request("firecrawl", "rust 工具", 5, "fc-key");
        assert_eq!(url, "https://api.firecrawl.dev/v1/search");
        assert_eq!(
            headers,
            vec![
                ("Content-Type".to_string(), "application/json".to_string()),
                ("Authorization".to_string(), "Bearer fc-key".to_string()),
            ]
        );
        let body: Value = serde_json::from_str(&payload).expect("请求体是 JSON");
        assert_eq!(body["query"], "rust 工具");
        assert_eq!(body["limit"], 5);
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

        // Firecrawl 的结果在 `data` 里，摘要字段是 `description`。
        let firecrawl = r#"{"success":true,"data":[
            {"title":"丙","url":"https://e.example","description":"摘要丙"},
            {"url":"https://f.example"}
        ]}"#;
        let results = parse_api_results("firecrawl", firecrawl).expect("Firecrawl 结构");
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].snippet, "摘要丙");
        assert_eq!(results[1].title, "https://f.example");

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
        let error =
            parse_api_results("firecrawl", r#"{"success":true}"#).expect_err("缺 data 数组");
        assert!(
            error.message.contains("缺少 data 数组"),
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

    #[test]
    fn mixed_engines_include_page_channels_and_configured_api_channels() {
        let options = WebSearchOptions::default();
        assert_eq!(
            mixed_engines(&options),
            vec!["bing", "duckduckgo", "github", "arxiv", "huggingface"],
            "没有凭据时跑全部无需凭据的抓页面渠道"
        );
        let configured = WebSearchOptions {
            tavily_api_key: "tvly-x".to_string(),
            firecrawl_api_key: " fc-x ".to_string(),
            ..WebSearchOptions::default()
        };
        assert_eq!(
            mixed_engines(&configured),
            vec![
                "bing",
                "duckduckgo",
                "github",
                "arxiv",
                "huggingface",
                "tavily",
                "firecrawl"
            ],
            "已配凭据的接口型渠道按固定顺序追加，未配的跳过"
        );
    }

    #[test]
    fn mixed_search_merges_channels_and_dedupes_by_url() {
        let transport = Arc::new(RouteTransport::new(&[
            (
                "bing.com",
                r#"<li class="b_algo"><h2><a href="https://a.example/x">甲</a></h2><p>来自 bing</p></li>
<li class="b_algo"><h2><a href="https://b.example/">乙</a></h2><p>只出现在 bing</p></li>"#,
            ),
            (
                "duckduckgo.com",
                r#"<a class="result__a" href="https://a.example/x/">甲的另一份</a>
<a class="result__a" href="https://c.example">丙</a>"#,
            ),
            (
                "api.tavily.com",
                r#"{"results":[{"title":"丁","url":"https://d.example","content":"摘要丁"}]}"#,
            ),
            // 另外三个抓页面渠道给「无结果」页面：它们参与（进表头）但不贡献条目。
            ("github.com", EMPTY_PAGE),
            ("arxiv.org", EMPTY_PAGE),
            ("huggingface.co", EMPTY_PAGE),
        ]));
        let options = WebSearchOptions {
            tavily_api_key: "tvly-x".to_string(),
            ..route_options(transport.clone())
        };
        let output = web_search(
            &options,
            &arguments(json!({"query": "rust", "engine": "mixed", "max_results": 5})),
        )
        .expect("混合搜索应当成功");

        // 五路抓页面渠道 + 已配凭据的 tavily 都请求了；没配凭据的 exa / firecrawl 不请求。
        let requests = transport.take_requests();
        assert_eq!(requests.len(), 6, "{requests:?}");
        assert!(requests.iter().any(|url| url.contains("bing.com")));
        assert!(requests.iter().any(|url| url.contains("duckduckgo.com")));
        assert!(requests.iter().any(|url| url.contains("github.com")));
        assert!(requests.iter().any(|url| url.contains("arxiv.org")));
        assert!(requests.iter().any(|url| url.contains("huggingface.co")));
        assert!(requests.iter().any(|url| url.contains("api.tavily.com")));
        assert!(!requests.iter().any(|url| url.contains("api.exa.ai")));

        // `https://a.example/x` 与 `https://a.example/x/` 是同一份：合并后来源标注两路。
        let normalized = normalize(&output);
        assert!(
            normalized.starts_with("来源：mixed（bing+duckduckgo+github+arxiv+huggingface+tavily）｜查询：rust｜用时：{ELAPSED}s｜共 4 条（去重 1 条）"),
            "{normalized}"
        );
        assert!(
            normalized
                .contains("1. 甲\n   https://a.example/x\n   来自 bing\n   来源：bing+duckduckgo"),
            "{normalized}"
        );
        assert!(
            normalized.contains("2. 乙\n   https://b.example/"),
            "{normalized}"
        );
        assert!(
            normalized.contains("3. 丙\n   https://c.example"),
            "{normalized}"
        );
        assert!(
            normalized.contains("4. 丁\n   https://d.example\n   摘要丁"),
            "{normalized}"
        );
        assert!(
            !normalized.contains("甲的另一份"),
            "先到的标题保留：{normalized}"
        );
        assert!(
            !normalized.contains("失败渠道"),
            "给了路由的渠道都不该失败：{normalized}"
        );
    }

    #[test]
    fn mixed_search_truncates_to_max_results_after_merging() {
        let transport = Arc::new(RouteTransport::new(&[(
            "bing.com",
            r#"<li class="b_algo"><h2><a href="https://a.example">甲</a></h2><p>摘要甲</p></li>
<li class="b_algo"><h2><a href="https://b.example">乙</a></h2><p>摘要乙</p></li>
<li class="b_algo"><h2><a href="https://c.example">丙</a></h2><p>摘要丙</p></li>"#,
        )]));
        let options = route_options(transport);
        let output = web_search(
            &options,
            &arguments(json!({"query": "rust", "engine": "mixed", "max_results": 2})),
        )
        .expect("混合搜索应当成功");
        let normalized = normalize(&output);
        assert!(normalized.contains("共 2 条（去重 0 条）"), "{normalized}");
        assert!(normalized.contains("https://b.example"), "{normalized}");
        assert!(!normalized.contains("https://c.example"), "{normalized}");
    }

    #[test]
    fn mixed_search_reports_failed_channels_but_keeps_the_rest() {
        // 除 bing 外的渠道都没有路由：它们失败，bing 仍然给出结果。
        let transport = Arc::new(RouteTransport::new(&[(
            "bing.com",
            r#"<li class="b_algo"><h2><a href="https://a.example">甲</a></h2><p>摘要甲</p></li>"#,
        )]));
        let options = WebSearchOptions {
            tavily_api_key: "tvly-x".to_string(),
            ..route_options(transport)
        };
        let output = web_search(
            &options,
            &arguments(json!({"query": "rust", "engine": "mixed"})),
        )
        .expect("单路失败不该让整次搜索失败");
        let normalized = normalize(&output);
        assert!(
            normalized.contains("来源：mixed（bing）｜查询：rust"),
            "{normalized}"
        );
        assert!(
            normalized.contains("失败渠道：duckduckgo：无法连接到搜索引擎（网络不可达）。"),
            "{normalized}"
        );
        for engine in ["github", "arxiv", "huggingface"] {
            assert!(
                normalized.contains(&format!("{engine}：无法连接到搜索引擎（网络不可达）。")),
                "{engine} 的失败也要如实列出：{normalized}"
            );
        }
        assert!(
            normalized.contains("tavily：无法连接到搜索引擎（网络不可达）。"),
            "{normalized}"
        );
        assert!(normalized.contains("https://a.example"), "{normalized}");
    }

    #[test]
    fn mixed_search_fails_only_when_every_channel_fails() {
        let transport = Arc::new(RouteTransport::new(&[]));
        let options = route_options(transport);
        let error = web_search(
            &options,
            &arguments(json!({"query": "rust", "engine": "mixed"})),
        )
        .expect_err("全部渠道失败应当报错");
        assert!(
            error
                .message
                .starts_with("混合搜索全部渠道均未返回结果：bing："),
            "{}",
            error.message
        );
        assert!(error.message.contains("duckduckgo："), "{}", error.message);
        assert!(error.message.contains("huggingface："), "{}", error.message);
    }

    #[test]
    fn mixed_search_reports_when_no_channel_has_results() {
        // 所有抓页面渠道都能打开但一条结果都解析不出来。
        let transport = Arc::new(RouteTransport::new(&[
            ("bing.com", EMPTY_PAGE),
            ("duckduckgo.com", EMPTY_PAGE),
            ("github.com", EMPTY_PAGE),
            ("arxiv.org", EMPTY_PAGE),
            ("huggingface.co", EMPTY_PAGE),
        ]));
        let options = route_options(transport);
        let error = web_search(
            &options,
            &arguments(json!({"query": "rust", "engine": "mixed"})),
        )
        .expect_err("没有任何结果应当报错");
        assert_eq!(
            error.message,
            "未找到相关搜索结果（已请求 bing、duckduckgo、github、arxiv、huggingface）。"
        );
    }

    #[test]
    fn mixed_search_skips_uncallable_channels_instead_of_failing() {
        // 未配 tavily 凭据：它根本不参与，也不会出现在失败清单里。
        let transport = Arc::new(RouteTransport::new(&[
            (
                "bing.com",
                r#"<li class="b_algo"><h2><a href="https://a.example">甲</a></h2><p>摘要甲</p></li>"#,
            ),
            (
                "duckduckgo.com",
                r#"<a class="result__a" href="https://b.example">乙</a>"#,
            ),
            ("github.com", EMPTY_PAGE),
            ("arxiv.org", EMPTY_PAGE),
            ("huggingface.co", EMPTY_PAGE),
        ]));
        let options = route_options(transport.clone());
        let output = web_search(
            &options,
            &arguments(json!({"query": "rust", "engine": "mixed"})),
        )
        .expect("抓页面渠道都该成功");
        let normalized = normalize(&output);
        assert!(
            normalized.contains("来源：mixed（bing+duckduckgo+github+arxiv+huggingface）"),
            "{normalized}"
        );
        assert!(!normalized.contains("失败渠道"), "{normalized}");
        let requests = transport.take_requests();
        assert_eq!(requests.len(), 5, "只请求五个抓页面渠道：{requests:?}");
        assert!(!requests.iter().any(|url| url.contains("api.")));
    }

    #[test]
    fn url_key_ignores_trailing_slash_and_case() {
        assert_eq!(
            url_key(" https://Example.com/A/ "),
            url_key("https://example.com/A")
        );
        assert_ne!(
            url_key("https://example.com/a/b"),
            url_key("https://example.com/a")
        );
    }
}
