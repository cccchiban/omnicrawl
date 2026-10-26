//! 抓取接口的运行期配置与抓取执行体装配。
//!
//! 配置来自 `config.toml` 的 `[fetch_api]`。本服务不做鉴权，因此只接受回环地址。
//! 抓取执行体（`FetcherOptions`）与 TUI / 本地 API 同一口径：同一条指纹传输、
//! 同一份 `[tls_resumption]` 会话缓存判定。

use std::sync::Arc;

use omnicrawl_config::core::runtime::ConfigEnvironment;
use omnicrawl_config::features::fetch_api::{load_fetch_api_configuration, FetchApiConfig};
use omnicrawl_host::tools::fetcher::{
    tls_session_cache_from_config, FetcherOptions, MAX_URLS, MAX_WORKERS,
};

/// 抓取接口的运行期配置。
#[derive(Clone)]
pub struct FetchSettings {
    /// `[fetch_api]` 段：监听地址与默认抓取参数。
    pub api: FetchApiConfig,
    /// 抓取执行体（传输、UA、超时、代理、会话缓存）。
    pub fetcher: Arc<FetcherOptions>,
}

impl std::fmt::Debug for FetchSettings {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("FetchSettings")
            .field("api", &self.api)
            .finish()
    }
}

impl FetchSettings {
    /// 服务能否真正监听。
    pub fn ready(&self) -> bool {
        self.api.usable()
    }

    /// 不能监听时的可读原因；可用时返回 `None`。
    pub fn unavailable_reason(&self) -> Option<String> {
        if !self.api.enabled {
            return Some(
                "抓取接口未启用：请在 config.toml 的 [fetch_api] 段设置 enabled = true。"
                    .to_string(),
            );
        }
        None
    }

    /// 监听地址文本。
    pub fn address(&self) -> String {
        self.api.address()
    }

    /// 单次请求允许的 URL 上限：配置给了更小的正整数就收窄，否则用执行体自身的上限。
    pub fn url_limit(&self) -> usize {
        match self.api.max_urls {
            0 => MAX_URLS,
            value => (value as usize).clamp(1, MAX_URLS),
        }
    }
}

/// 按配置装配运行期；缺段与坏值都按默认值回落。
pub fn load_settings(environment: &ConfigEnvironment) -> FetchSettings {
    let api = load_fetch_api_configuration(environment, None);
    let fetcher = FetcherOptions {
        max_workers: MAX_WORKERS,
        tls_session_cache: tls_session_cache_from_config(environment),
        ..FetcherOptions::default()
    };
    FetchSettings {
        api,
        fetcher: Arc::new(fetcher),
    }
}
