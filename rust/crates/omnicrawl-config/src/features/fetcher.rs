//! 抓取（`fetcher` 工具）的引擎配置。
//!
//! 本地抓取直连目标站点，网络受限或目标站点对直连不友好时拿不到正文。启用这一组引擎后，
//! 抓取改由第三方抓取接口完成：它们在自己的出口上取页面，再把清洗过的正文交回来。
//! 这里只管「用哪个引擎」与「该引擎的凭据从哪来」——请求形状与响应解析在
//! `omnicrawl_host::tools::fetch_engines`，凭据本身仍留在 `[web_search]` 段里，
//! 与联网搜索共用同一把 Key，不要求用户填两遍。
//!
//! Python 侧没有对映实现，因此不进 parity 数据集，自带 `tests/fetcher_config.rs`。

use std::path::{Path, PathBuf};

use crate::core::runtime::{load_config_data, save_config_data, ConfigEnvironment};
use crate::error::ConfigError;
use crate::toml::{Table, Value};
use crate::value::python_str;

/// 抓取引擎：本地直连，或把抓取交给带抓取接口的服务。
///
/// `balanced` 不是某一家，而是在多个可用远程引擎之间轮转：额度是各家分别计费的，
/// 平摊之后单一渠道的消耗慢得多。轮转状态在进程内，不落盘。
pub const FETCH_ENGINES: [&str; 9] = [
    "local",
    "tavily",
    "exa",
    "firecrawl",
    "anysearch",
    "tinyfish",
    "deepseek",
    "keenable",
    "balanced",
];

/// 默认走本地抓取：不依赖任何凭据，语义与既有实现完全一致。
pub const DEFAULT_FETCH_ENGINE: &str = "local";

/// 负载均衡的引擎名。
pub const BALANCED_FETCH_ENGINE: &str = "balanced";

/// 能提供抓取接口的引擎（`local` 与 `balanced` 除外）。
///
/// 顺序即负载均衡的轮转顺序，与设置页的候选顺序一致。
pub const REMOTE_FETCH_ENGINES: [&str; 7] = [
    "tavily",
    "exa",
    "firecrawl",
    "anysearch",
    "tinyfish",
    "deepseek",
    "keenable",
];

/// 抓取引擎选择。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetcherConfiguration {
    /// `[fetcher] engine`：`local`、某个远程引擎名，或 `balanced`。
    pub engine: String,
    /// 远程引擎出错时是否回落到本地抓取。
    ///
    /// 默认 `true`：抓取接口是按次计费的，一次额度耗尽不该让整次抓取直接失败；
    /// 回落到本地至少还能拿到直连能看到的那一份。关掉它等于要求「要么用接口要么失败」。
    pub fallback_local: bool,
}

impl Default for FetcherConfiguration {
    fn default() -> Self {
        Self {
            engine: DEFAULT_FETCH_ENGINE.to_string(),
            fallback_local: true,
        }
    }
}

impl FetcherConfiguration {
    /// 取值域校验与归一化：引擎名大小写不敏感。
    pub fn normalize(mut self) -> Result<Self, ConfigError> {
        let engine = self.engine.trim().to_lowercase();
        let engine = if engine.is_empty() {
            DEFAULT_FETCH_ENGINE.to_string()
        } else {
            engine
        };
        if !FETCH_ENGINES.contains(&engine.as_str()) {
            return Err(ConfigError::new(format!(
                "fetcher.engine 仅支持 {}。",
                FETCH_ENGINES.join("、")
            )));
        }
        self.engine = engine;
        Ok(self)
    }

    /// 这份配置是否需要远程引擎参与（`local` 不需要）。
    pub fn uses_remote(&self) -> bool {
        self.engine != DEFAULT_FETCH_ENGINE
    }
}

/// 读取抓取引擎配置；缺少 `fetcher` 段时返回默认配置。
pub fn load_fetcher_configuration(
    env: &ConfigEnvironment,
    config_path: Option<&Path>,
) -> Result<FetcherConfiguration, ConfigError> {
    let data = load_config_data(env, config_path)?;
    let section = match data.get("fetcher") {
        None => Table::new(),
        Some(Value::String(text)) if text.is_empty() => Table::new(),
        Some(Value::Table(section)) => section.clone(),
        Some(_) => return Err(ConfigError::new("配置段 fetcher 必须是对象。")),
    };
    FetcherConfiguration {
        engine: text_field(&section, "engine", DEFAULT_FETCH_ENGINE),
        // 未写这个键时取 true：缺省就该有兜底，而不是把失败暴露给模型。
        fallback_local: section
            .get("fallback_local")
            .and_then(|value| match value {
                Value::Boolean(flag) => Some(*flag),
                _ => None,
            })
            .unwrap_or(true),
    }
    .normalize()
}

/// 保留其他配置段，只更新整份抓取引擎配置。
pub fn save_fetcher_configuration(
    env: &ConfigEnvironment,
    configuration: &FetcherConfiguration,
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
        "fallback_local".to_string(),
        Value::Boolean(configuration.fallback_local),
    );
    data.insert("fetcher".to_string(), Value::Table(section));
    save_config_data(env, &data, config_path)
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
