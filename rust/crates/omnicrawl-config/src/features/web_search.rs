//! 联网搜索（`web_search` 工具）的配置。
//!
//! 与前几段分开是刻意的：`[tools]` 只管工具是否进表，`[web_search]` 管这台机器
//! 用哪条搜索通路、以及 API 类引擎的凭据。Python 侧没有对映实现（那套实现随
//! 移植一起下线），因此不进 parity 数据集，自带 `tests/web_search_config.rs`。

use std::path::{Path, PathBuf};

use crate::core::runtime::{load_config_data, save_config_data, ConfigEnvironment};
use crate::error::ConfigError;
use crate::toml::{Table, Value};
use crate::value::python_str;

/// 可选引擎：抓页面型（DuckDuckGo/GitHub/arXiv/Hugging Face）、
/// 接口型（Tavily/Exa/Firecrawl/AnySearch/TinyFish/DeepSeek/Grok/Keenable）与混合搜索。
///
/// `mixed` 不是单一通路，而是一次并发查询多个渠道并把结果去重合并。
/// 这里是引擎清单的唯一来源：宿主用它校验工具参数，设置界面用它铺候选。
pub const WEB_SEARCH_ENGINES: [&str; 13] = [
    "duckduckgo",
    "github",
    "arxiv",
    "huggingface",
    "tavily",
    "exa",
    "firecrawl",
    "anysearch",
    "tinyfish",
    "deepseek",
    "grok",
    "keenable",
    "mixed",
];

/// DeepSeek 搜索引擎名。
pub const DEEPSEEK_WEB_SEARCH_ENGINE: &str = "deepseek";
/// Grok（xAI）搜索引擎名。
pub const GROK_WEB_SEARCH_ENGINE: &str = "grok";
/// Keenable 搜索引擎名。
pub const KEENABLE_WEB_SEARCH_ENGINE: &str = "keenable";

/// DeepSeek 搜索接口的默认地址（`POST /v1/tools/search`）。
pub const DEFAULT_DEEPSEEK_SEARCH_URL: &str = "https://api.deepseek.com/v1/tools/search";
/// Grok 搜索接口的默认地址（Responses API + `web_search` 服务端工具）。
pub const DEFAULT_GROK_SEARCH_URL: &str = "https://api.x.ai/v1/responses";
/// Grok 搜索默认模型。
pub const DEFAULT_GROK_SEARCH_MODEL: &str = "grok-4.7";
/// Keenable 搜索接口的默认地址（`POST /v1/search`；匿名调用改走 `/v1/search/public`）。
pub const DEFAULT_KEENABLE_SEARCH_URL: &str = "https://api.keenable.ai/v1/search";

/// 混合搜索的引擎名。
pub const MIXED_WEB_SEARCH_ENGINE: &str = "mixed";

/// 默认引擎：无需凭据即可使用。
pub const DEFAULT_WEB_SEARCH_ENGINE: &str = "duckduckgo";

/// 联网搜索选用的引擎与接口型引擎的凭据。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebSearchConfiguration {
    /// 用哪个引擎（`[web_search] engine`）；配成 `mixed` 即综合搜索。
    /// 工具参数不能覆盖它：引擎由使用者在设置里决定。
    pub engine: String,
    /// Tavily API Key（明文）。空串表示未配置。
    pub tavily_api_key: String,
    /// Exa API Key（明文）。空串表示未配置。
    pub exa_api_key: String,
    /// Firecrawl API Key（明文）。空串表示未配置。
    pub firecrawl_api_key: String,
    /// AnySearch API Key（明文）。空串表示匿名调用（按 IP 限流、走每日免费额度）。
    pub anysearch_api_key: String,
    /// TinyFish API Key（明文）。必填：Search 接口没有匿名通道。
    pub tinyfish_api_key: String,
    /// GitHub Token（明文，`repo` 之外无需任何权限）。空串表示未配置。
    ///
    /// 只影响 GitHub 引擎：带 Token 请求会走 REST 检索接口（认证后 30 次/分钟），
    /// 不带 Token 仍抓公开搜索页并共享未认证的 10 次/分钟额度。
    pub github_token: String,
    /// DeepSeek 搜索接口地址（明文）。空串表示用内置默认地址。
    ///
    /// 可填第三方代理或自建网关：协议需与本家一致（发送 `query`/`count`，
    /// 回 `results[]`）。已带路径的 URL 原样使用；只写到主机名的会被补上
    /// 默认地址的路径（见 [`complete_search_endpoint`]）。
    pub deepseek_search_url: String,
    /// DeepSeek 搜索 API Key（明文）。空串表示未配置。
    pub deepseek_api_key: String,
    /// Grok（xAI）搜索接口地址（明文）。空串表示用内置默认地址。
    ///
    /// 该通路调 Responses API 的 `web_search` 服务端工具，因此填第三方兼容
    /// 网关时对方必须接受同一请求体（`model`/`input`/`tools`）。只写到主机名
    /// 的会被补上默认地址的路径（见 [`complete_search_endpoint`]）。
    pub grok_search_url: String,
    /// Grok 搜索 API Key（明文）。空串表示未配置。
    pub grok_api_key: String,
    /// Grok 搜索使用的模型名。空串表示用内置默认模型。
    ///
    /// 第三方网关上的模型名常与官方不同，因此可自行填写。
    pub grok_model: String,
    /// Keenable 搜索 API Key（明文）。空串表示走匿名通道（按 IP 限流）。
    pub keenable_api_key: String,
}

impl Default for WebSearchConfiguration {
    fn default() -> Self {
        Self {
            engine: DEFAULT_WEB_SEARCH_ENGINE.to_string(),
            tavily_api_key: String::new(),
            exa_api_key: String::new(),
            firecrawl_api_key: String::new(),
            anysearch_api_key: String::new(),
            tinyfish_api_key: String::new(),
            github_token: String::new(),
            deepseek_search_url: String::new(),
            deepseek_api_key: String::new(),
            grok_search_url: String::new(),
            grok_api_key: String::new(),
            grok_model: String::new(),
            keenable_api_key: String::new(),
        }
    }
}

impl WebSearchConfiguration {
    /// 取值域校验与归一化：引擎名大小写不敏感，凭据去首尾空白。
    pub fn normalize(mut self) -> Result<Self, ConfigError> {
        let engine = self.engine.trim().to_lowercase();
        let engine = if engine.is_empty() {
            DEFAULT_WEB_SEARCH_ENGINE.to_string()
        } else {
            engine
        };
        if !WEB_SEARCH_ENGINES.contains(&engine.as_str()) {
            return Err(ConfigError::new(format!(
                "web_search.engine 仅支持 {}。",
                WEB_SEARCH_ENGINES.join("、")
            )));
        }
        self.engine = engine;
        self.tavily_api_key = self.tavily_api_key.trim().to_string();
        self.exa_api_key = self.exa_api_key.trim().to_string();
        self.firecrawl_api_key = self.firecrawl_api_key.trim().to_string();
        self.anysearch_api_key = self.anysearch_api_key.trim().to_string();
        self.tinyfish_api_key = self.tinyfish_api_key.trim().to_string();
        self.github_token = self.github_token.trim().to_string();
        self.deepseek_search_url = self.deepseek_search_url.trim().to_string();
        self.deepseek_api_key = self.deepseek_api_key.trim().to_string();
        self.grok_search_url = self.grok_search_url.trim().to_string();
        self.grok_api_key = self.grok_api_key.trim().to_string();
        self.grok_model = self.grok_model.trim().to_string();
        self.keenable_api_key = self.keenable_api_key.trim().to_string();
        for url in [&self.deepseek_search_url, &self.grok_search_url] {
            if !url.is_empty() && !is_http_url(url) {
                return Err(ConfigError::new(format!(
                    "web_search 里的搜索接口地址必须是 http(s) 地址：{url}"
                )));
            }
        }
        Ok(self)
    }

    /// 指定引擎下生效的 API Key；抓页面型引擎没有凭据，返回空串。
    ///
    /// GitHub、AnySearch 与 Keenable 不在「缺凭据即报错」之列：GitHub 的 Token 只影响额度，
    /// AnySearch 与 Keenable 没有 Key 也能走匿名通道，三者都单独读取。
    pub fn api_key_for(&self, engine: &str) -> &str {
        match engine.trim().to_lowercase().as_str() {
            "tavily" => self.tavily_api_key.as_str(),
            "exa" => self.exa_api_key.as_str(),
            "firecrawl" => self.firecrawl_api_key.as_str(),
            "anysearch" => self.anysearch_api_key.as_str(),
            "tinyfish" => self.tinyfish_api_key.as_str(),
            "deepseek" => self.deepseek_api_key.as_str(),
            "grok" => self.grok_api_key.as_str(),
            "keenable" => self.keenable_api_key.as_str(),
            _ => "",
        }
    }

    /// DeepSeek 引擎实际使用的接口地址：配了就用配的，否则用内置默认值。
    pub fn deepseek_endpoint(&self) -> String {
        complete_search_endpoint(&self.deepseek_search_url, DEFAULT_DEEPSEEK_SEARCH_URL)
    }

    /// Grok 引擎实际使用的接口地址：配了就用配的，否则用内置默认值。
    pub fn grok_endpoint(&self) -> String {
        complete_search_endpoint(&self.grok_search_url, DEFAULT_GROK_SEARCH_URL)
    }

    /// Grok 引擎实际使用的模型名：配了就用配的，否则用内置默认值。
    pub fn grok_model_name(&self) -> &str {
        if self.grok_model.is_empty() {
            DEFAULT_GROK_SEARCH_MODEL
        } else {
            self.grok_model.as_str()
        }
    }
}

/// 读取联网搜索配置；缺少 `web_search` 段时返回默认配置。
pub fn load_web_search_configuration(
    env: &ConfigEnvironment,
    config_path: Option<&Path>,
) -> Result<WebSearchConfiguration, ConfigError> {
    let data = load_config_data(env, config_path)?;
    let section = match data.get("web_search") {
        None => Table::new(),
        Some(Value::String(text)) if text.is_empty() => Table::new(),
        Some(Value::Table(section)) => section.clone(),
        Some(_) => return Err(ConfigError::new("配置段 web_search 必须是对象。")),
    };
    WebSearchConfiguration {
        engine: text_field(&section, "engine", DEFAULT_WEB_SEARCH_ENGINE),
        tavily_api_key: text_field(&section, "tavily_api_key", ""),
        exa_api_key: text_field(&section, "exa_api_key", ""),
        firecrawl_api_key: text_field(&section, "firecrawl_api_key", ""),
        anysearch_api_key: text_field(&section, "anysearch_api_key", ""),
        tinyfish_api_key: text_field(&section, "tinyfish_api_key", ""),
        github_token: text_field(&section, "github_token", ""),
        deepseek_search_url: text_field(&section, "deepseek_search_url", ""),
        deepseek_api_key: text_field(&section, "deepseek_api_key", ""),
        grok_search_url: text_field(&section, "grok_search_url", ""),
        grok_api_key: text_field(&section, "grok_api_key", ""),
        grok_model: text_field(&section, "grok_model", ""),
        keenable_api_key: text_field(&section, "keenable_api_key", ""),
    }
    .normalize()
}

/// 保留其他配置段，只更新整份联网搜索配置。
pub fn save_web_search_configuration(
    env: &ConfigEnvironment,
    configuration: &WebSearchConfiguration,
    config_path: Option<&Path>,
) -> Result<PathBuf, ConfigError> {
    let configuration = configuration.clone().normalize()?;
    let mut data = load_config_data(env, config_path)?;
    let mut section = Table::new();
    section.insert(
        "engine".to_string(),
        Value::String(configuration.engine.clone()),
    );
    section.insert(
        "tavily_api_key".to_string(),
        Value::String(configuration.tavily_api_key.clone()),
    );
    section.insert(
        "exa_api_key".to_string(),
        Value::String(configuration.exa_api_key.clone()),
    );
    section.insert(
        "firecrawl_api_key".to_string(),
        Value::String(configuration.firecrawl_api_key.clone()),
    );
    section.insert(
        "anysearch_api_key".to_string(),
        Value::String(configuration.anysearch_api_key.clone()),
    );
    section.insert(
        "tinyfish_api_key".to_string(),
        Value::String(configuration.tinyfish_api_key.clone()),
    );
    section.insert(
        "github_token".to_string(),
        Value::String(configuration.github_token.clone()),
    );
    section.insert(
        "deepseek_search_url".to_string(),
        Value::String(configuration.deepseek_search_url.clone()),
    );
    section.insert(
        "deepseek_api_key".to_string(),
        Value::String(configuration.deepseek_api_key.clone()),
    );
    section.insert(
        "grok_search_url".to_string(),
        Value::String(configuration.grok_search_url.clone()),
    );
    section.insert(
        "grok_api_key".to_string(),
        Value::String(configuration.grok_api_key.clone()),
    );
    section.insert(
        "grok_model".to_string(),
        Value::String(configuration.grok_model.clone()),
    );
    section.insert(
        "keenable_api_key".to_string(),
        Value::String(configuration.keenable_api_key.clone()),
    );
    data.insert("web_search".to_string(), Value::Table(section));
    save_config_data(env, &data, config_path)
}

/// 是否是 http(s) 地址：自定义搜索接口只接受这两种协议。
fn is_http_url(url: &str) -> bool {
    let lower = url.to_lowercase();
    (lower.starts_with("http://") || lower.starts_with("https://")) && url.len() > "https://".len()
}

/// 搜索接口地址的补全规则：留空回落内置默认地址；只写到主机名（含裸域名、
/// 只带斜杠或端口）的补上默认地址的路径；已经带路由的原样使用。
///
/// 网关常被填成裸域名，而该地址是原样投递的请求目标——不补路径就会打到站点
/// 根路径，收到 HTML 而不是接口响应。
pub fn complete_search_endpoint(configured: &str, fallback: &str) -> String {
    let trimmed = configured.trim();
    if trimmed.is_empty() {
        return fallback.to_string();
    }
    let Some(scheme_end) = trimmed.find("://") else {
        return trimmed.to_string();
    };
    let authority = &trimmed[scheme_end + 3..];
    let head = &authority[..authority.find(['?', '#']).unwrap_or(authority.len())];
    match head.find('/') {
        Some(at) if head[at..].trim_end_matches('/').len() > 1 => trimmed.to_string(),
        _ if !authority[head.len()..].is_empty() => trimmed.to_string(),
        _ => format!(
            "{}{}{}",
            &trimmed[..scheme_end + 3],
            head.trim_end_matches('/'),
            endpoint_path(fallback)
        ),
    }
}

/// 默认地址的路径部分（含前导斜杠）；没有路径时返回空串。
fn endpoint_path(url: &str) -> &str {
    let Some(scheme_end) = url.find("://") else {
        return "";
    };
    let authority = &url[scheme_end + 3..];
    match authority.find('/') {
        Some(at) => &authority[at..],
        None => "",
    }
}

/// Python `str(section.get(name) or fallback)`。
fn text_field(section: &Table, name: &str, fallback: &str) -> String {
    let text = python_str(section.get(name));
    if text.is_empty() {
        fallback.to_string()
    } else {
        text
    }
}
