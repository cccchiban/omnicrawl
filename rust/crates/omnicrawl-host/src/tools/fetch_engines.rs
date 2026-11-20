//! 第三方抓取接口的适配：把「取一个网址的正文」交给各家接口，再把回包收敛成同一种结果。
//!
//! 六家的请求与回包形状各不相同（请求字段、鉴权头、正文所在字段、失败怎么表达），
//! 差异全部收在这一个文件里；`fetcher` 只关心「拿到正文 / 没拿到」。
//!
//! | 引擎 | 端点 | 鉴权 | 正文 |
//! | --- | --- | --- | --- |
//! | Tavily | `POST /extract` | `Authorization: Bearer` | `results[].raw_content` |
//! | Exa | `POST /contents` | `x-api-key` | `results[].text` |
//! | Firecrawl | `POST /v2/scrape` | `Authorization: Bearer` | `data.markdown` |
//! | AnySearch | `POST /v1/extract` | `Authorization: Bearer`（可匿名） | `data.content` |
//! | TinyFish | `POST https://api.fetch.tinyfish.ai` | `X-API-Key` | `results[].text` |
//! | DeepSeek | `POST /v1/tools/fetch` | `Authorization: Bearer` | `content` |
//!
//! 两处「回包里也有失败」的行为必须照做：Tavily 单网址失败进 `failed_results`、TinyFish
//! 进 `errors[]`，两者 HTTP 状态都还是 200。只看状态码会把失败当成功，正文变成空串。
//!
//! 抓取接口和本地抓取共用同一条指纹传输（[`WebTransport`]）：出网行为、代理判定与
//! TLS 档案都一致，切引擎不会让整条链路的网络特征突然改变。

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use omnicrawl_core::diagnostics;
use serde_json::{json, Value};

use super::web_transport::{host_of, is_private_host, WebRequest, WebTransport};

/// Tavily 抽取接口。
pub const TAVILY_ENDPOINT: &str = "https://api.tavily.com/extract";
/// Exa 正文接口。
pub const EXA_ENDPOINT: &str = "https://api.exa.ai/contents";
/// Firecrawl 单页抓取接口（v2）。
pub const FIRECRAWL_ENDPOINT: &str = "https://api.firecrawl.dev/v2/scrape";
/// AnySearch 网页内容抽取接口。
pub const ANYSEARCH_ENDPOINT: &str = "https://api.anysearch.com/v1/extract";
/// TinyFish 的抓取端点就是它的主机名，没有额外路径。
pub const TINYFISH_ENDPOINT: &str = "https://api.fetch.tinyfish.ai";
/// DeepSeek 抓取接口。
pub const DEEPSEEK_ENDPOINT: &str = "https://api.deepseek.com/v1/tools/fetch";

/// 回包里能取到的正文上限（字符）：接口自己的上限可能更高，但模型的上下文不该被一节撑满。
///
/// 调用方的 `max_chars` 会再往下裁，这里只是防止一次回包本身过大。
const MAX_BODY_CHARS: usize = 200_000;

/// 一个引擎的抓取结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemotePage {
    pub title: String,
    pub content: String,
}

/// 抓不到时给出的可读原因。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteError(pub String);

/// 一次远程抓取需要的全部输入。
pub struct RemoteRequest<'a> {
    /// 引擎名（`tavily`/`exa`/…）。
    pub engine: &'a str,
    pub url: &'a str,
    pub api_key: &'a str,
    /// 传输层：与本地抓取同一份，出网特征一致。
    pub transport: &'a dyn WebTransport,
    /// 代理决定：`None` 直连，`Some` 用显式地址。
    pub proxy: Option<String>,
    pub insecure: bool,
    /// 浏览器指纹档案：接口调用也走同一套身份。
    pub impersonate: Option<String>,
    pub timeout: Duration,
}

/// 按引擎分派一次远程抓取。
pub fn fetch_remote(request: &RemoteRequest<'_>) -> Result<RemotePage, RemoteError> {
    let prepared = prepare(request);
    let mut web = WebRequest::post_json(
        prepared.endpoint,
        prepared.headers,
        &prepared.payload.to_string(),
        request.timeout,
    );
    web.proxy = request.proxy.clone();
    web.insecure = request.insecure;
    web.impersonate = request.impersonate.clone();
    let response = request
        .transport
        .send(&web)
        .map_err(|error| RemoteError(format!("{} 接口请求失败：{}", label(request.engine), error.message)))?;
    let body = response.text();
    if response.status >= 400 {
        return Err(RemoteError(format!(
            "{} 接口返回 HTTP {}：{}",
            label(request.engine),
            response.status,
            detail_of(&body)
        )));
    }
    parse_page(request.engine, request.url, &body)
}

/// 一个引擎的请求三件套：地址、请求头与 JSON 请求体。
struct Prepared {
    endpoint: &'static str,
    headers: Vec<(String, String)>,
    payload: Value,
}

fn prepare(request: &RemoteRequest<'_>) -> Prepared {
    let mut headers = vec![("Content-Type".to_string(), "application/json".to_string())];
    let key = request.api_key;
    let payload = match request.engine {
        "exa" => {
            headers.push(("x-api-key".to_string(), key.to_string()));
            // `text` 只要正文；只要一种视图就不会被按视图分别计费。
            json!({
                "ids": [request.url],
                "text": { "maxCharacters": MAX_BODY_CHARS },
            })
        }
        "firecrawl" => {
            headers.push(("Authorization".to_string(), format!("Bearer {key}")));
            json!({
                "url": request.url,
                "formats": ["markdown"],
                // 只要正文：导航、页脚与广告留着只会挤占上下文。
                "onlyMainContent": true,
            })
        }
        "anysearch" => {
            // 与搜索一致：有 Key 才带，带了无效 Key 会被 401 且不回落到匿名额度。
            if !key.is_empty() {
                headers.push(("Authorization".to_string(), format!("Bearer {key}")));
            }
            json!({ "url": request.url })
        }
        "tinyfish" => {
            headers = vec![("X-API-Key".to_string(), key.to_string())];
            headers.push(("Content-Type".to_string(), "application/json".to_string()));
            // 一次只提一个网址：这个端点支持批量，但按条取回才能把失败落到具体那一条。
            json!({ "urls": [request.url], "format": "markdown" })
        }
        "deepseek" => {
            headers.push(("Authorization".to_string(), format!("Bearer {key}")));
            json!({ "url": request.url })
        }
        _ => {
            headers.push(("Authorization".to_string(), format!("Bearer {key}")));
            json!({
                "urls": request.url,
                "format": "markdown",
                // basic 已经能拿到正文，advanced 更慢也更贵。
                "extract_depth": "basic",
            })
        }
    };
    Prepared {
        endpoint: endpoint_of(request.engine),
        headers,
        payload,
    }
}

fn endpoint_of(engine: &str) -> &'static str {
    match engine {
        "exa" => EXA_ENDPOINT,
        "firecrawl" => FIRECRAWL_ENDPOINT,
        "anysearch" => ANYSEARCH_ENDPOINT,
        "tinyfish" => TINYFISH_ENDPOINT,
        "deepseek" => DEEPSEEK_ENDPOINT,
        _ => TAVILY_ENDPOINT,
    }
}

/// 引擎的中文名（报错文案用）。
pub fn label(engine: &str) -> &'static str {
    match engine {
        "exa" => "Exa",
        "firecrawl" => "Firecrawl",
        "anysearch" => "AnySearch",
        "tinyfish" => "TinyFish",
        "deepseek" => "DeepSeek",
        _ => "Tavily",
    }
}

/// 凭据所在的配置键名：抓取接口与联网搜索共用同一把 Key，报错要指向同一个字段。
pub fn key_field(engine: &str) -> &'static str {
    match engine {
        "exa" => "exa_api_key",
        "firecrawl" => "firecrawl_api_key",
        "anysearch" => "anysearch_api_key",
        "tinyfish" => "tinyfish_api_key",
        "deepseek" => "deepseek_api_key",
        _ => "tavily_api_key",
    }
}

/// 这个引擎的 Key 能不能留空（只有 AnySearch 有匿名额度）。
pub fn key_optional(engine: &str) -> bool {
    engine == "anysearch"
}

/// 解析回包：六家的正文位置与失败表达都不同。
fn parse_page(engine: &str, url: &str, body: &str) -> Result<RemotePage, RemoteError> {
    let parsed: Value = serde_json::from_str(body).map_err(|_| {
        RemoteError(format!(
            "{} 接口返回的不是 JSON：{}",
            label(engine),
            take(body, 200)
        ))
    })?;
    match engine {
        "tavily" => parse_tavily(url, &parsed),
        "exa" => parse_exa(url, &parsed),
        "firecrawl" => parse_firecrawl(&parsed),
        "anysearch" => parse_anysearch(&parsed),
        "tinyfish" => parse_tinyfish(url, &parsed),
        _ => parse_deepseek(&parsed),
    }
}

/// Tavily：成功的进 `results`，单条失败进 `failed_results`，两者可以同时存在。
fn parse_tavily(url: &str, parsed: &Value) -> Result<RemotePage, RemoteError> {
    let results = parsed
        .get("results")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let matched = results
        .iter()
        .find(|item| same_url(item.get("url"), url))
        .or_else(|| results.first());
    if let Some(item) = matched {
        let content = text_of(item.get("raw_content"));
        if !content.trim().is_empty() {
            return Ok(RemotePage {
                title: text_of(item.get("title")),
                content: take(&content, MAX_BODY_CHARS),
            });
        }
    }
    let failure = parsed
        .get("failed_results")
        .and_then(Value::as_array)
        .and_then(|items| items.first());
    let reason = failure
        .map(|item| text_of(item.get("error")))
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| "回包里没有正文".to_string());
    Err(RemoteError(format!("Tavily 未能取出内容：{reason}")))
}

/// Exa：`results[]` 里取 `text`；`statuses[]` 给出每个网址的成功与失败。
fn parse_exa(url: &str, parsed: &Value) -> Result<RemotePage, RemoteError> {
    let results = parsed
        .get("results")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let matched = results
        .iter()
        .find(|item| same_url(item.get("url").or_else(|| item.get("id")), url))
        .or_else(|| results.first());
    if let Some(item) = matched {
        let content = text_of(item.get("text"));
        if !content.trim().is_empty() {
            return Ok(RemotePage {
                title: text_of(item.get("title")),
                content: take(&content, MAX_BODY_CHARS),
            });
        }
    }
    let reason = parsed
        .get("statuses")
        .and_then(Value::as_array)
        .and_then(|items| {
            items
                .iter()
                .find(|item| text_of(item.get("status")) != "success")
        })
        .map(|item| {
            let status = text_of(item.get("status"));
            let error = text_of(item.get("error"));
            if error.is_empty() {
                status
            } else {
                format!("{status}：{error}")
            }
        })
        .filter(|text: &String| !text.is_empty())
        .unwrap_or_else(|| "回包里没有正文".to_string());
    Err(RemoteError(format!("Exa 未能取出内容：{reason}")))
}

/// Firecrawl：`success` 为假时 `error` 才是原因；成功时正文在 `data.markdown`。
fn parse_firecrawl(parsed: &Value) -> Result<RemotePage, RemoteError> {
    if parsed.get("success").and_then(Value::as_bool) == Some(false) {
        let reason = text_of(parsed.get("error"));
        return Err(RemoteError(format!(
            "Firecrawl 未能取出内容：{}",
            if reason.is_empty() {
                "未给出原因".to_string()
            } else {
                reason
            }
        )));
    }
    let data = parsed.get("data").unwrap_or(&Value::Null);
    let content = text_of(data.get("markdown"));
    if content.trim().is_empty() {
        return Err(RemoteError("Firecrawl 未能取出内容：回包里没有正文".to_string()));
    }
    let metadata = data.get("metadata").unwrap_or(&Value::Null);
    let title = text_of(metadata.get("title"));
    Ok(RemotePage {
        title,
        content: take(&content, MAX_BODY_CHARS),
    })
}

/// AnySearch：`code` 非 0 即失败；正文在 `data.content`。
fn parse_anysearch(parsed: &Value) -> Result<RemotePage, RemoteError> {
    let code = parsed.get("code").and_then(Value::as_i64).unwrap_or(0);
    if code != 0 {
        let reason = text_of(parsed.get("message"));
        return Err(RemoteError(format!(
            "AnySearch 未能取出内容：{}",
            if reason.is_empty() {
                format!("错误码 {code}")
            } else {
                take(&reason, 400)
            }
        )));
    }
    let data = parsed.get("data").unwrap_or(&Value::Null);
    let content = text_of(data.get("content"));
    if content.trim().is_empty() {
        return Err(RemoteError("AnySearch 未能取出内容：回包里没有正文".to_string()));
    }
    Ok(RemotePage {
        title: text_of(data.get("title")),
        content: take(&content, MAX_BODY_CHARS),
    })
}

/// TinyFish：单条失败进 `errors[]`，HTTP 状态仍是 200。
fn parse_tinyfish(url: &str, parsed: &Value) -> Result<RemotePage, RemoteError> {
    let results = parsed
        .get("results")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let matched = results
        .iter()
        .find(|item| same_url(item.get("url"), url))
        .or_else(|| results.first());
    if let Some(item) = matched {
        let content = text_of(item.get("text"));
        if !content.trim().is_empty() {
            return Ok(RemotePage {
                title: text_of(item.get("title")),
                content: take(&content, MAX_BODY_CHARS),
            });
        }
    }
    let reason = parsed
        .get("errors")
        .and_then(Value::as_array)
        .and_then(|items| items.first())
        .map(|item| text_of(item.get("error")))
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| "回包里没有正文".to_string());
    Err(RemoteError(format!("TinyFish 未能取出内容：{reason}")))
}

/// DeepSeek：正文就是顶层 `content`。
fn parse_deepseek(parsed: &Value) -> Result<RemotePage, RemoteError> {
    let content = text_of(parsed.get("content"));
    if content.trim().is_empty() {
        let reason = text_of(parsed.get("error"));
        return Err(RemoteError(format!(
            "DeepSeek 未能取出内容：{}",
            if reason.is_empty() {
                "回包里没有正文".to_string()
            } else {
                take(&reason, 400)
            }
        )));
    }
    Ok(RemotePage {
        title: text_of(parsed.get("title")),
        content: take(&content, MAX_BODY_CHARS),
    })
}

/// 报错里带出的回包片段：给出可定位的线索，又不把整份回包塞进上下文。
fn detail_of(body: &str) -> String {
    match serde_json::from_str::<Value>(body) {
        Ok(parsed) => {
            for key in ["error", "message", "detail"] {
                let text = text_of(parsed.get(key));
                if !text.is_empty() {
                    return take(&text, 400);
                }
            }
            take(body, 400)
        }
        Err(_) => take(body, 400),
    }
}

/// 取一个 JSON 值当文本：字符串原样，其余走 JSON 表示（接口偶尔回对象）。
fn text_of(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Null) | None => String::new(),
        Some(other) => other.to_string(),
    }
}

/// 网址比对：忽略末尾斜杠与大小写的主机差异，只看「是不是同一个地址」。
fn same_url(value: Option<&Value>, url: &str) -> bool {
    let text = text_of(value);
    let normalized = |item: &str| item.trim().trim_end_matches('/').to_lowercase();
    !text.is_empty() && normalized(&text) == normalized(url)
}

fn take(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    text.chars().take(limit).collect()
}

/// 负载均衡的轮转游标：进程内计数，不落盘。
///
/// 用原子量而不是取模随机：随机会让同一次批量抓取里的相邻两条落到同一家，
/// 轮转则能把额度真正摊开。
static BALANCED_CURSOR: AtomicUsize = AtomicUsize::new(0);

/// 从可用引擎里挑下一个（轮转）。
///
/// `available` 必须已按配置过滤掉缺凭据的引擎；空列表由调用方处理。下标推进到这里
/// 是因为一个批量里每条网址都要前进一位，不能每次从 0 开始。
pub fn next_balanced(available: &[String]) -> Option<&String> {
    if available.is_empty() {
        return None;
    }
    let index = BALANCED_CURSOR.fetch_add(1, Ordering::Relaxed) % available.len();
    available.get(index)
}

/// 远程引擎用的代理决定：接口地址都在公网，因此与本地抓取同一套判定。
pub fn resolve_proxy(url: &str, configured: Option<&str>) -> Option<String> {
    match configured {
        Some(explicit) if explicit.is_empty() => None,
        Some(explicit) => Some(explicit.to_string()),
        None => {
            let host = host_of(url).unwrap_or_default();
            if is_private_host(&host) {
                None
            } else {
                super::web_transport::detect_windows_proxy()
            }
        }
    }
}

/// 远程抓取引擎的运行期配置：用谁、凭据、失败了要不要回落。
#[derive(Clone, Debug, Default)]
pub struct RemoteFetchOptions {
    /// `local`、某个引擎名，或 `balanced`。空串按 `local` 处理。
    pub engine: String,
    /// 各引擎的凭据，键与 `[web_search]` 段里的字段名一致。
    pub api_keys: Vec<(String, String)>,
    /// 远程失败时是否回落到本地抓取。
    pub fallback_local: bool,
}

impl RemoteFetchOptions {
    /// 这一段是否真的会走远程接口（`local` 与空串都不走）。
    pub fn uses_remote(&self) -> bool {
        !self.engine.is_empty() && self.engine != "local"
    }

    /// 指定引擎的凭据；未配置时为空串。
    pub fn api_key_for(&self, engine: &str) -> &str {
        self.api_keys
            .iter()
            .find(|(name, _)| name == engine)
            .map(|(_, key)| key.as_str())
            .unwrap_or("")
    }

    /// `balanced` 时可轮转的引擎：有凭据的引擎，AnySearch 无凭据也能匿名参与。
    ///
    /// 顺序按 [`REMOTE_FETCH_ENGINES`] 固定，轮转结果因此可复现。
    pub fn balanced_candidates(&self) -> Vec<String> {
        omnicrawl_config::features::fetcher::REMOTE_FETCH_ENGINES
            .iter()
            .filter(|engine| !self.api_key_for(engine).is_empty() || key_optional(engine))
            .map(|engine| (*engine).to_string())
            .collect()
    }
}

/// 按 `[fetcher]` 段与 `[web_search]` 的凭据装配远程抓取运行期。
///
/// 凭据刻意与联网搜索共用：两个功能打的是同一批服务，要求用户填两遍只会让两边不同步。
/// 读不出配置时按 `local` 处理——抓取是基础能力，不能因为一段配置坏掉就不能用。
pub fn remote_fetch_options_from_config(
    environment: &omnicrawl_config::core::runtime::ConfigEnvironment,
) -> RemoteFetchOptions {
    let Ok(configuration) =
        omnicrawl_config::features::fetcher::load_fetcher_configuration(environment, None)
    else {
        return RemoteFetchOptions::default();
    };
    let Ok(search) =
        omnicrawl_config::features::web_search::load_web_search_configuration(environment, None)
    else {
        return RemoteFetchOptions {
            engine: configuration.engine,
            api_keys: Vec::new(),
            fallback_local: configuration.fallback_local,
        };
    };
    let api_keys = omnicrawl_config::features::fetcher::REMOTE_FETCH_ENGINES
        .iter()
        .map(|engine| ((*engine).to_string(), search.api_key_for(engine).to_string()))
        .collect();
    RemoteFetchOptions {
        engine: configuration.engine,
        api_keys,
        fallback_local: configuration.fallback_local,
    }
}

/// 一次远程抓取的入参：本地抓取计划里与远程有关的那几项。
pub struct RemotePlan<'a> {
    pub url: &'a str,
    pub insecure: bool,
    pub impersonate: &'a str,
    pub timeout: Duration,
    pub max_chars: usize,
    /// 抓取工具自己的代理决定（`None` 自动检测、`Some("")` 禁用、其余显式地址）。
    pub proxy: Option<String>,
}

/// 抓到的一页：标题与已经清洗过的正文。
pub struct RemoteEntry {
    pub title: String,
    pub content: String,
}

/// 一次抓取该不该走远程接口，以及走了之后的结果。
///
/// `None` 表示「这次交给本地直连」：配置没启用远程（`local`/空串），或远程失败且允许回落。
pub fn remote_entry(
    transport: &dyn WebTransport,
    options: &RemoteFetchOptions,
    plan: &RemotePlan<'_>,
) -> Option<Result<RemoteEntry, RemoteError>> {
    if !options.uses_remote() {
        return None;
    }
    match run(transport, options, plan) {
        Ok(page) => Some(Ok(RemoteEntry {
            title: page.title,
            content: take(&page.content, plan.max_chars),
        })),
        Err(error) => {
            if options.fallback_local {
                diagnostics::warn(format!(
                    "[host] 远程抓取不可用，改用本地直连：{}：{}",
                    plan.url, error.0
                ));
                None
            } else {
                Some(Err(error))
            }
        }
    }
}

/// 按配置选一个引擎并发起抓取。
///
/// `balanced` 时先取有凭据的引擎列表再轮转；一个可用引擎都没有（所有 Key 都空、
/// 只剩没有匿名额度的那些）是最常见的配置错误，因此直接给出该去填哪个字段。
fn run(
    transport: &dyn WebTransport,
    options: &RemoteFetchOptions,
    plan: &RemotePlan<'_>,
) -> Result<RemotePage, RemoteError> {
    let engine = if options.engine == omnicrawl_config::features::fetcher::BALANCED_FETCH_ENGINE {
        let candidates = options.balanced_candidates();
        match next_balanced(&candidates) {
            Some(engine) => engine.clone(),
            None => {
                return Err(RemoteError(
                    "负载均衡没有可用引擎：请至少为一个带抓取接口的服务填写 API Key。".to_string(),
                ))
            }
        }
    } else {
        options.engine.clone()
    };

    let api_key = options.api_key_for(&engine);
    if api_key.is_empty() && !key_optional(&engine) {
        return Err(RemoteError(format!(
            "{} 缺少 API Key：请填写 web_search.{}。",
            label(&engine),
            key_field(&engine)
        )));
    }

    let endpoint = endpoint_of(&engine);
    fetch_remote(&RemoteRequest {
        engine: &engine,
        url: plan.url,
        api_key,
        transport,
        proxy: resolve_proxy(endpoint, plan.proxy.as_deref()),
        insecure: plan.insecure,
        impersonate: Some(plan.impersonate.to_string()),
        timeout: plan.timeout,
    })
}

#[cfg(test)]
mod tests {
    //! 远程抓取引擎适配层的用例：请求形状、回包解析与轮转。
    //!
    //! 全部离线：传输层用记录型桩替换，因此这些用例既不出网也不依赖任何真实凭据。

    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use serde_json::json;

    use super::*;
    use crate::tools::web_transport::{WebError, WebResponse};

    /// 记录发出的请求并按脚本回一个响应。
    struct StubTransport {
        response: WebResponse,
        seen: Mutex<Vec<WebRequest>>,
    }

    impl StubTransport {
        fn new(status: u16, body: &str) -> Arc<Self> {
            Arc::new(Self {
                response: WebResponse {
                    status,
                    final_url: String::new(),
                    body: body.as_bytes().to_vec(),
                    headers: Vec::new(),
                },
                seen: Mutex::new(Vec::new()),
            })
        }

        fn sent(&self) -> Vec<WebRequest> {
            self.seen.lock().expect("锁").clone()
        }
    }

    impl WebTransport for StubTransport {
        fn send(&self, request: &WebRequest) -> Result<WebResponse, WebError> {
            self.seen.lock().expect("锁").push(request.clone());
            Ok(self.response.clone())
        }

        fn navigation_headers(&self, user_agent: &str, _impersonate: Option<&str>) -> Vec<(String, String)> {
            vec![("User-Agent".to_string(), user_agent.to_string())]
        }
    }

    fn request<'a>(engine: &'a str, transport: &'a StubTransport) -> RemoteRequest<'a> {
        RemoteRequest {
            engine,
            url: "https://example.com/article",
            api_key: "test-key",
            transport,
            proxy: Some(String::new()),
            insecure: false,
            impersonate: Some("chrome".to_string()),
            timeout: Duration::from_secs(5),
        }
    }

    fn body_of(request: &WebRequest) -> Value {
        serde_json::from_slice(&request.body).expect("请求体是 JSON")
    }

    #[test]
    fn each_engine_posts_its_own_request_shape() {
        let cases = [
            ("tavily", TAVILY_ENDPOINT, "Authorization", "Bearer test-key"),
            ("exa", EXA_ENDPOINT, "x-api-key", "test-key"),
            ("firecrawl", FIRECRAWL_ENDPOINT, "Authorization", "Bearer test-key"),
            ("anysearch", ANYSEARCH_ENDPOINT, "Authorization", "Bearer test-key"),
            ("tinyfish", TINYFISH_ENDPOINT, "X-API-Key", "test-key"),
            ("deepseek", DEEPSEEK_ENDPOINT, "Authorization", "Bearer test-key"),
        ];
        for (engine, endpoint, header, value) in cases {
            let transport = StubTransport::new(500, "{}");
            let _ = fetch_remote(&request(engine, &transport));
            let sent = transport.sent();
            let sent = sent.first().expect("应当发过一次请求");
            assert_eq!(sent.url, endpoint, "{engine} 的端点");
            assert_eq!(sent.method, "POST", "{engine} 用 POST");
            assert!(
                sent.headers
                    .iter()
                    .any(|(name, value_at)| name == header && value_at == value),
                "{engine} 的鉴权头：{:?}",
                sent.headers
            );
            // 单网址引擎的请求体都带这个地址，避免适配层把 body 写空。
            let body = body_of(sent);
            assert!(
                body.to_string().contains("example.com"),
                "{engine} 的请求体应带上目标地址：{body}"
            );
        }
    }

    /// Tavily 与 TinyFish 的单条失败仍在 200 里，必须读出来而不是当成功。
    #[test]
    fn per_url_failures_inside_a_200_response_are_errors() {
        let tavily = StubTransport::new(
            200,
            &json!({
                "results": [],
                "failed_results": [{"url": "https://example.com/article", "error": "Failed to retrieve content"}],
            })
            .to_string(),
        );
        let error = fetch_remote(&request("tavily", &tavily)).expect_err("空结果应当报错");
        assert!(error.0.contains("Failed to retrieve content"), "{}", error.0);

        let tiny = StubTransport::new(
            200,
            &json!({
                "results": [],
                "errors": [{"url": "https://example.com/article", "error": "DNS error"}],
            })
            .to_string(),
        );
        let error = fetch_remote(&request("tinyfish", &tiny)).expect_err("空结果应当报错");
        assert!(error.0.contains("DNS error"), "{}", error.0);
    }

    #[test]
    fn each_engine_reads_content_from_its_own_field() {
        let cases = [
            ("tavily", json!({"results": [{"url": "https://example.com/article", "raw_content": "塔维正文"}]}), ""),
            ("exa", json!({"results": [{"url": "https://example.com/article", "text": "Exa 正文"}], "statuses": []}), ""),
            ("firecrawl", json!({"success": true, "data": {"markdown": "火爬正文", "metadata": {"title": "标题"}}}), "标题"),
            ("anysearch", json!({"code": 0, "data": {"title": "任意标题", "content": "任意正文"}}), "任意标题"),
            ("tinyfish", json!({"results": [{"url": "https://example.com/article", "text": "小鱼正文"}], "errors": []}), ""),
            ("deepseek", json!({"title": "深寻标题", "content": "深寻正文"}), "深寻标题"),
        ];
        for (engine, payload, expected_title) in cases {
            let transport = StubTransport::new(200, &payload.to_string());
            let page = fetch_remote(&request(engine, &transport)).expect("应当解析出正文");
            assert!(page.content.contains("正文"), "{engine} 的正文：{}", page.content);
            assert_eq!(page.title, expected_title, "{engine} 的标题");
        }
    }

    #[test]
    fn error_status_and_error_body_are_reported() {
        let transport = StubTransport::new(401, &json!({"error": "Invalid API key"}).to_string());
        let error = fetch_remote(&request("tavily", &transport)).expect_err("401 应当报错");
        assert!(error.0.contains("HTTP 401"), "{}", error.0);
        assert!(error.0.contains("Invalid API key"), "{}", error.0);
    }

    /// AnySearch 有匿名额度：没有 Key 时不带 Authorization，而不是带一个空 Bearer。
    #[test]
    fn anysearch_anonymous_requests_carry_no_authorization() {
        let transport = StubTransport::new(200, "{}");
        let mut anonymous = request("anysearch", &transport);
        anonymous.api_key = "";
        let _ = fetch_remote(&anonymous);
        let sent = transport.sent();
        let sent = sent.first().expect("应当发过一次请求");
        assert!(
            !sent
                .headers
                .iter()
                .any(|(name, _)| name.eq_ignore_ascii_case("authorization")),
            "匿名请求不该带鉴权头：{:?}",
            sent.headers
        );
        assert!(key_optional("anysearch"));
        assert!(!key_optional("tavily"));
    }

    /// 轮转要真的均摊：连续四次调用应覆盖全部两家候选。
    #[test]
    fn balanced_rotation_covers_every_candidate() {
        let available = vec!["tavily".to_string(), "exa".to_string()];
        let picked: Vec<String> = (0..4)
            .map(|_| next_balanced(&available).expect("有候选").clone())
            .collect();
        let mut sorted = picked.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), 2, "四轮里两家都应被选中：{picked:?}");
        assert!(next_balanced(&[]).is_none(), "没有候选时返回 None");
    }
}
