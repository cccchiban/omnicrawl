//! 网页抓取工具执行体：并行抓取、跟随 HTTP 重定向与 meta refresh、正文提取。
//!
//! 语义基准是 `omnicrawl/net/fetcher.py`：只抓取用户明确给出的 URL、不绕过验证码；
//! 内网/本机目标直连而不走系统代理；`insecure=true` 跳过 TLS 校验。
//! 正文提取用 HTML5 容错解析（`lxml` 的 `main > article > body` + `text_content` 等价）。
//! 静态提取拿不到内容时（或用 `force_scripts=true` 显式要求时），再用本地 JS 沙箱
//! （[`omnicrawl_jsdom`]）执行页面脚本重建正文：沙箱按文档顺序跑内联与外链脚本，
//! 外链由本模块的 `ScriptLoader` 取回——它继承本次抓取的指纹身份、逐脚本过 robots.txt、
//! 并限制单脚本体积与总数；沙箱本身不持有网络能力，遇到挑战页仍是如实报错。
//! JS 侧环境按 `impersonate` 的档案族伪装成有头浏览器（见 `omnicrawl_jsdom::Profile`）。
//! Python 侧的 `impersonate` 浏览器指纹参数在此同样生效：默认传输是
//! [`super::tls_transport::TlsWebTransport`]（omnicrawl-tls 自研栈的浏览器档案），
//! 不再是「接受但不生效」——因为 Python 的默认值就是 `chrome`，它属于默认路径而非可选增强。

use std::sync::Arc;
use std::time::{Duration, Instant};

use regex::Regex;
use scraper::{ElementRef, Html, Node, Selector};
use serde_json::{Map, Value};

use super::cookies::{CookieJar, SharedJar};
use super::error::{ToolError, ToolOutcome};
use super::web_search::{clamp_float, clamp_int, strip_tags, truncate};
use super::web_transport::{
    detect_windows_proxy, host_of, is_private_host, resolve_url, WebErrorKind, WebRequest,
    WebTransport, MAX_REDIRECTS,
};
// 浏览器指纹传输：omnicrawl-tls 自研栈（TLS 档案 + HTTP/2 + HTTP/1.1），只给联网工具用。
use super::tls_transport::{normalize_impersonate, TlsWebTransport};

pub const DEFAULT_TIMEOUT_SECONDS: f64 = 15.0;
pub const DEFAULT_MAX_CHARS: i64 = 8000;
pub const MAX_URLS: usize = 20;
pub const MAX_META_REDIRECTS: usize = 3;
pub const MAX_WORKERS: usize = 8;
/// 正文短于这个字数（不计空白）时，视为「没抓到内容」，值得在本机沙箱里重建一次。
///
/// 取 200：正常文章页的正文远超这个量，而「外壳 + 脚本填充」的页面静态提取通常只剩
/// 导航语与占位符。低于这条线才付出一次 JS 执行的代价。
const THIN_TEXT_THRESHOLD: usize = 200;
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
    /// 进程内 cookie 罐：一次抓取内多个 URL 共用一份，`Set-Cookie` 会被后续请求回带。
    ///
    /// 不落盘：罐跟着进程走，退出即失。`None` 表示完全不做 cookie 处理。
    pub cookie_jar: Option<SharedJar>,
    /// TLS 会话缓存：`Some` 时第二次连同一站点会尝试恢复会话（默认 `None`，即关闭）。
    ///
    /// 由配置 `[tls_resumption] enabled` 控制。会话恢复改的是密钥调度，因此默认关闭；
    /// 开启后失败会回退完整握手，不会让本该成功的抓取失败。
    pub tls_session_cache: Option<Arc<omnicrawl_tls::session_cache::SessionCache>>,
}

impl Default for FetcherOptions {
    fn default() -> Self {
        Self {
            // 默认就走浏览器指纹传输：Python 的 `impersonate` 默认值是 chrome，
            // 用 ureq+rustls 的库指纹会在做 TLS 指纹校验的站点上直接失败。
            transport: Arc::new(TlsWebTransport::new()),
            user_agent: super::web_transport::DEFAULT_USER_AGENT.to_string(),
            request_timeout_seconds: DEFAULT_TIMEOUT_SECONDS,
            max_workers: MAX_WORKERS,
            proxy: None,
            cookie_jar: Some(Arc::new(CookieJar::new())),
            tls_session_cache: None,
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
    /// 归一化后的浏览器指纹族名（`chrome`/`firefox`/`safari`/`edge`）。
    /// 在计划阶段就归一化，避免每个 URL、每个重定向轮次重复解析。
    impersonate: String,
    /// 是否执行页面脚本（含 `<script src>`）：关闭后只做静态提取。
    run_scripts: bool,
    /// 正文够长也照常执行脚本：用于「必须拿到脚本渲染结果」的页面（指纹站点、动态面板）。
    force_scripts: bool,
}

pub fn fetcher(options: &FetcherOptions, arguments: &Map<String, Value>) -> ToolOutcome {
    let urls = parse_urls(arguments.get("urls"));
    if urls.is_empty() {
        return Err(ToolError::new(
            "urls 不能为空：请提供至少一个要抓取的 URL（逗号、换行分隔或 JSON 数组）。",
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
        impersonate: normalize_impersonate(arguments.get("impersonate")),
        run_scripts: as_bool(arguments.get("run_scripts"), true),
        force_scripts: as_bool(arguments.get("force_scripts"), false),
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
        // 每一轮都判：重定向后可能落到另一个站点，按新地址重新查一次许可。
        super::robots_cache::ensure_allowed(
            options.transport.as_ref(),
            &current,
            &options.user_agent,
        )
        .map_err(|error| FetchError::Fatal(error.message))?;
        let mut request = WebRequest::new(
            current.clone(),
            options
                .transport
                .navigation_headers(&options.user_agent, Some(plan.impersonate.as_str())),
            plan.timeout,
        );
        request.proxy = resolve_proxy(&current, options);
        request.insecure = plan.insecure;
        request.max_redirects = MAX_REDIRECTS;
        request.impersonate = Some(plan.impersonate.clone());
        request.cookie_jar = options.cookie_jar.clone();
        request.tls_session_cache = options.tls_session_cache.clone();
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
        let static_text = extract_main_text(&html_text, plan.max_chars);
        // 正文近乎为空时，页面很可能是「外壳 + 脚本填充」结构：在本机沙箱里执行页面脚本
        // （含 `<script src>`，由 `ScriptLoader` 取回），再重新取正文。
        // 沙箱本身不联网：外链取数、robots 校验与体积限制都在加载器里做。
        if plan.run_scripts && (plan.force_scripts || is_thin_text(&static_text)) {
            match rebuild_with_scripts(
                &html_text,
                plan.max_chars,
                plan.impersonate.as_str(),
                &final_url,
                options,
                plan,
            ) {
                Some(rendered) => rendered,
                None => static_text,
            }
        } else {
            static_text
        }
    };
    Ok(FetchEntry::done(url, status, final_url, title, content))
}

/// 正文短到不像「抓到了内容」时返回 `true`：用于决定是否值得再跑一次脚本重建。
fn is_thin_text(text: &str) -> bool {
    text.chars()
        .filter(|character| !character.is_whitespace())
        .count()
        < THIN_TEXT_THRESHOLD
}

/// 用本地 JS 沙箱重建正文；脚本没动过 DOM 或重建后仍无内容时返回 `None`（保持原样）。
///
/// 两个取数通道都交给沙箱：
/// - `ScriptLoader` 取 `<script src>` 的脚本正文；
/// - `HttpFetcher` 承接脚本内 `fetch`/XHR（SPA 外壳的正文就靠它）。
///
/// 沙箱自身仍然不持网络能力，访问控制全在这两个实现里。
fn rebuild_with_scripts(
    html_text: &str,
    max_chars: usize,
    family: &str,
    page_url: &str,
    options: &FetcherOptions,
    plan: &FetchPlan,
) -> Option<String> {
    let profile = omnicrawl_jsdom::Profile::for_family(family).with_href(page_url);
    let loader = ScriptLoader {
        transport: &options.transport,
        user_agent: &options.user_agent,
        impersonate: plan.impersonate.as_str(),
        timeout: plan.timeout,
        insecure: plan.insecure,
        proxy: resolve_proxy(page_url, options),
        count: std::sync::atomic::AtomicUsize::new(0),
        total_bytes: std::sync::atomic::AtomicUsize::new(0),
    };
    let fetcher = HttpFetcher {
        transport: &options.transport,
        user_agent: &options.user_agent,
        impersonate: plan.impersonate.as_str(),
        timeout: plan.timeout,
        insecure: plan.insecure,
        proxy: resolve_proxy(page_url, options),
        cookie_jar: options.cookie_jar.clone(),
        tls_session_cache: options.tls_session_cache.clone(),
        page_url: page_url.to_string(),
    };
    let outcome =
        omnicrawl_jsdom::run_with_sources(html_text, &profile, Some(&loader), Some(&fetcher));
    if !omnicrawl_jsdom::touches_dom(&outcome) {
        return None;
    }
    let rendered = extract_main_text(&outcome.html, max_chars);
    if is_thin_text(&rendered) {
        return None;
    }
    Some(rendered)
}

/// 外链脚本加载器：给 JS 沙箱取 `<script src=...>` 的正文。
///
/// 访问控制全在这一层，沙箱本身不持有网络能力：
/// - 只接受 `http(s)`；
/// - 每个新主机都先过 robots.txt（与顶层导航同一套规则，拒绝即跳过该脚本）；
/// - 单脚本体积上限、总脚本数与总字节数上限，避免把整站 bundle 拖进来；
/// - 复用顶层抓取的身份（同样的 `impersonate` 族与 UA），否则脚本请求的 TLS 指纹会和页面请求不一致。
struct ScriptLoader<'a> {
    transport: &'a Arc<dyn WebTransport>,
    user_agent: &'a str,
    impersonate: &'a str,
    timeout: Duration,
    insecure: bool,
    proxy: Option<String>,
    /// 已取脚本数：超上限后直接拒绝，避免页面挂上百个 `<script src>`。
    count: std::sync::atomic::AtomicUsize,
    /// 已取脚本的总字节数。
    total_bytes: std::sync::atomic::AtomicUsize,
}

/// 单个外链脚本最多取多少字节：超过就跳过，页面脚本通常远小于此。
const MAX_SCRIPT_BODY_BYTES: usize = 8 * 1024 * 1024;
/// 一份文档最多取多少个外链脚本。
const MAX_EXTERNAL_SCRIPTS: usize = 16;
/// 一份文档所有外链脚本的合计上限。
const MAX_EXTERNAL_TOTAL_BYTES: usize = 16 * 1024 * 1024;

impl omnicrawl_jsdom::ScriptSource for ScriptLoader<'_> {
    fn load(&self, url: &str) -> Result<String, String> {
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            return Err("非 http(s) 协议".to_string());
        }
        if self
            .count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            >= MAX_EXTERNAL_SCRIPTS
        {
            return Err("外链脚本数量超出上限".to_string());
        }
        // 每个脚本都要查许可：外链常来自别的域名，不能沿用页面的判断。
        super::robots_cache::ensure_allowed(self.transport.as_ref(), url, self.user_agent)
            .map_err(|error| error.message)?;
        let mut request = WebRequest::new(
            url.to_string(),
            self.transport
                .navigation_headers(self.user_agent, Some(self.impersonate)),
            self.timeout,
        );
        request.proxy = self.proxy.clone();
        request.insecure = self.insecure;
        request.impersonate = Some(self.impersonate.to_string());
        let response = self
            .transport
            .send(&request)
            .map_err(|error| format!("{}（HTTP 层）", error.message))?;
        if response.status >= 400 {
            return Err(format!("HTTP {}", response.status));
        }
        if response.body.len() > MAX_SCRIPT_BODY_BYTES {
            return Err(format!("脚本超过 {} 字节", MAX_SCRIPT_BODY_BYTES));
        }
        let used = self
            .total_bytes
            .fetch_add(response.body.len(), std::sync::atomic::Ordering::Relaxed);
        if used + response.body.len() > MAX_EXTERNAL_TOTAL_BYTES {
            return Err("外链脚本总字节数超出上限".to_string());
        }
        Ok(response.text())
    }
}

/// 脚本内请求的取数通道：给沙箱的 `fetch` / `XMLHttpRequest` 提供真实网络能力。
///
/// 访问控制与脚本加载器同源：
/// - 只接受 `http(s)`；
/// - **同站限定**：只允许页面所在站点（可注册域）发起的请求。跨站接口调用既不符合
///   「重建这一页正文」的目标，也是把抓取变成站外探测的入口，一律拒绝；
/// - 每个新主机都先过 robots.txt；
/// - 请求与响应体都有体积上限，总字节数与请求数由沙箱再兜一层。
///
/// 身份与顶层抓取一致（同样的 `impersonate` 族、UA、`cookie_jar`），
/// 因此接口请求拿到的是与页面同一会话的数据，而不是匿名视角。
struct HttpFetcher<'a> {
    transport: &'a Arc<dyn WebTransport>,
    user_agent: &'a str,
    impersonate: &'a str,
    timeout: Duration,
    insecure: bool,
    proxy: Option<String>,
    cookie_jar: Option<SharedJar>,
    tls_session_cache: Option<Arc<omnicrawl_tls::session_cache::SessionCache>>,
    /// 页面地址：用来判定「同站」，也是 `Sec-Fetch-Site` 的发起方。
    page_url: String,
}

/// 脚本内单个请求的请求体上限。
const MAX_XHR_REQUEST_BYTES: usize = 256 * 1024;
/// 脚本内单个响应的体积上限（沙箱自己也有一道，这里先挡在传输层）。
const MAX_XHR_RESPONSE_BYTES: usize = 4 * 1024 * 1024;

impl omnicrawl_jsdom::HttpSource for HttpFetcher<'_> {
    fn send(
        &self,
        request: &omnicrawl_jsdom::HttpRequest,
    ) -> Result<omnicrawl_jsdom::HttpResponse, String> {
        if !(request.url.starts_with("http://") || request.url.starts_with("https://")) {
            return Err("非 http(s) 协议".to_string());
        }
        if !same_site(&self.page_url, &request.url) {
            return Err(format!("跨站请求已拒绝：{}", request.url));
        }
        if request.body.len() > MAX_XHR_REQUEST_BYTES {
            return Err(format!("请求体超过 {MAX_XHR_REQUEST_BYTES} 字节"));
        }
        // 与顶层导航同一套许可规则：接口路径同样受 robots.txt 约束。
        super::robots_cache::ensure_allowed(self.transport.as_ref(), &request.url, self.user_agent)
            .map_err(|error| error.message)?;

        let headers = self
            .transport
            .navigation_headers(self.user_agent, Some(self.impersonate));
        let mut web = WebRequest::new(request.url.clone(), headers, self.timeout);
        web.method = request.method.clone();
        web.body = request.body.clone();
        web.proxy = self.proxy.clone();
        web.insecure = self.insecure;
        web.impersonate = Some(self.impersonate.to_string());
        web.cookie_jar = self.cookie_jar.clone();
        web.tls_session_cache = self.tls_session_cache.clone();
        web.initiator = Some(self.page_url.clone());
        web.purpose = super::fetch_metadata::Purpose::Api;
        // 脚本显式设置的头覆盖默认值：页面自己知道该发什么（如 `content-type`）。
        for (name, value) in &request.headers {
            web.headers
                .retain(|(key, _)| !key.eq_ignore_ascii_case(name));
            web.headers.push((name.clone(), value.clone()));
        }

        let response = self
            .transport
            .send(&web)
            .map_err(|error| format!("{}（HTTP 层）", error.message))?;
        if response.body.len() > MAX_XHR_RESPONSE_BYTES {
            return Err(format!("响应体超过 {MAX_XHR_RESPONSE_BYTES} 字节"));
        }
        Ok(omnicrawl_jsdom::HttpResponse {
            status: response.status,
            status_text: status_text(response.status).to_string(),
            redirected: response.final_url != request.url,
            url: response.final_url,
            headers: response.headers,
            body: response.body,
        })
    }
}

/// 两个地址是否同站（可注册域一致）：脚本内请求只允许打回页面自己的站点。
///
/// 用可注册域而不是完整源：站点常见 `api.example.com` 与 `www.example.com` 分列，
/// 按完整源判定会把登录态接口全挡掉。
fn same_site(page: &str, target: &str) -> bool {
    match (host_of(page), host_of(target)) {
        (Some(page), Some(target)) => registrable_site(&page) == registrable_site(&target),
        _ => false,
    }
}

fn registrable_site(host: &str) -> String {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    let labels: Vec<&str> = host.split('.').collect();
    if labels.len() <= 2 {
        return host;
    }
    // 两级后缀（`co.uk`、`com.cn`）按三段取；其余取末两段。
    const TWO_LEVEL_SUFFIXES: [&str; 10] = [
        "co.uk", "org.uk", "ac.uk", "gov.uk", "com.cn", "net.cn", "org.cn", "com.au", "co.jp",
        "com.br",
    ];
    let tail = labels[labels.len().saturating_sub(2)..].join(".");
    if TWO_LEVEL_SUFFIXES.contains(&tail.as_str()) && labels.len() >= 3 {
        labels[labels.len() - 3..].join(".")
    } else {
        tail
    }
}

fn status_text(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        204 => "No Content",
        301 => "Moved Permanently",
        302 => "Found",
        304 => "Not Modified",
        307 => "Temporary Redirect",
        308 => "Permanent Redirect",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        409 => "Conflict",
        410 => "Gone",
        422 => "Unprocessable Entity",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "",
    }
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
        // 握手谈不成与证书无关：这里必须透出真实原因，否则会把人引去查证书。
        WebErrorKind::Handshake => format!("TLS 握手失败：{message}"),
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

/// 解析 `urls` 参数：逗号或换行分隔字符串、JSON 数组字符串或 JSON 数组。
///
/// 换行分隔与逗号等价：模型常把多条网址一行一条地写进同一个字符串，按整串当单个 URL
/// 会拼出必然失败的地址。
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
                Err(_) => {
                    parts = raw
                        .split([',', '\n', '\r'])
                        .map(|part| part.trim().to_string())
                        .collect()
                }
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

    /// 测试用计划：不外链、不超时之外的默认值，避免用例意外发起网络请求。
    fn test_plan() -> FetchPlan {
        FetchPlan {
            insecure: false,
            max_chars: 8000,
            max_html: false,
            timeout: Duration::from_secs(5),
            impersonate: "chrome".to_string(),
            run_scripts: true,
            force_scripts: false,
        }
    }

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
    fn urls_split_on_newlines_too() {
        assert_eq!(
            parse_urls(Some(&json!("https://a\nhttps://b\n"))),
            vec!["https://a".to_string(), "https://b".to_string()]
        );
        assert_eq!(
            parse_urls(Some(&json!("https://a\r\n  https://b  \nhttps://c"))),
            vec![
                "https://a".to_string(),
                "https://b".to_string(),
                "https://c".to_string()
            ]
        );
        // 逗号与换行混用：两种分隔符等价，空段被丢弃。
        assert_eq!(
            parse_urls(Some(&json!("https://a,\n\nhttps://b\n\n"))),
            vec!["https://a".to_string(), "https://b".to_string()]
        );
        assert!(parse_urls(Some(&json!("\n \n"))).is_empty());
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
    fn thin_text_threshold_separates_content_from_shell() {
        assert!(is_thin_text(""));
        assert!(is_thin_text("加载中…"));
        // 空白不计入：两行空白 + 几个字仍算「没抓到内容」。
        assert!(is_thin_text("   \n\t 短 "));
        assert!(!is_thin_text(&"字".repeat(THIN_TEXT_THRESHOLD)));
    }

    #[test]
    fn script_built_body_is_recovered_when_static_extraction_is_empty() {
        let html = format!(
            "<html><body><div id=\"app\"></div><script>\
             document.getElementById('app').innerHTML = '<p>{}</p>';\
             </script></body></html>",
            "脚本填充的正文".repeat(40)
        );
        let static_text = extract_main_text(&html, 8000);
        assert!(
            is_thin_text(&static_text),
            "静态提取应当拿不到内容：{static_text}"
        );
        let rebuilt = rebuild_with_scripts(
            &html,
            8000,
            "chrome",
            "https://example.com/",
            &FetcherOptions::default(),
            &test_plan(),
        )
        .expect("应当重建出正文");
        assert!(rebuilt.contains("脚本填充的正文"), "{rebuilt}");
    }

    #[test]
    fn static_page_is_left_alone_without_running_scripts() {
        let html = format!(
            "<html><body><p>{}</p><script>document.body.textContent = 'x';</script></body></html>",
            "本来就够长的静态正文".repeat(40)
        );
        // 静态正文已经足够：调用方不会走到重建分支，这里直接验证判定为「不薄」。
        assert!(!is_thin_text(&extract_main_text(&html, 8000)));
    }

    #[test]
    fn rebuild_gives_up_when_scripts_do_not_touch_the_dom() {
        let html =
            "<html><body><div id=\"app\"></div><script>var unused = 1 + 1;</script></body></html>";
        assert!(
            rebuild_with_scripts(
                html,
                8000,
                "chrome",
                "https://example.com/",
                &FetcherOptions::default(),
                &test_plan(),
            )
            .is_none(),
            "脚本没动 DOM 时不应替换原正文"
        );
    }

    #[test]
    fn same_site_uses_the_registrable_domain() {
        assert!(same_site(
            "https://blog.example.com/a",
            "https://blog.example.com/api/x"
        ));
        // 子域与裸域同站：登录态接口常在裸域上。
        assert!(same_site(
            "https://blog.example.com/a",
            "https://example.com/api/x"
        ));
        assert!(!same_site(
            "https://example.com/a",
            "https://evil.com/api/x"
        ));
        // 两级后缀不能把 a.co.uk 与 b.co.uk 判成同站。
        assert!(!same_site("https://a.co.uk/", "https://b.co.uk/"));
        assert!(same_site("https://www.a.co.uk/", "https://api.a.co.uk/x"));
        assert_eq!(registrable_site("www.example.com"), "example.com");
        assert_eq!(registrable_site("deep.sub.example.com"), "example.com");
        assert_eq!(registrable_site("shop.example.co.uk"), "example.co.uk");
        assert_eq!(registrable_site("localhost"), "localhost");
    }

    #[test]
    fn http_fetcher_rejects_cross_site_before_touching_the_network() {
        use omnicrawl_jsdom::{HttpRequest, HttpSource};
        // 传输层用一个必定失败的桩：跨站请求必须在到达它之前就被拒。
        struct PanicTransport;
        impl WebTransport for PanicTransport {
            fn send(
                &self,
                _request: &WebRequest,
            ) -> Result<
                super::super::web_transport::WebResponse,
                super::super::web_transport::WebError,
            > {
                panic!("跨站请求不应到达传输层");
            }
        }
        let transport: Arc<dyn WebTransport> = Arc::new(PanicTransport);
        let fetcher = HttpFetcher {
            transport: &transport,
            user_agent: "test-agent",
            impersonate: "chrome",
            timeout: Duration::from_secs(1),
            insecure: false,
            proxy: None,
            cookie_jar: None,
            tls_session_cache: None,
            page_url: "https://example.com/page".to_string(),
        };
        let error = fetcher
            .send(&HttpRequest {
                url: "https://evil.com/steal".to_string(),
                method: "GET".to_string(),
                headers: Vec::new(),
                body: Vec::new(),
            })
            .expect_err("跨站请求必须被拒绝");
        assert!(error.contains("跨站请求已拒绝"), "{error}");
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
