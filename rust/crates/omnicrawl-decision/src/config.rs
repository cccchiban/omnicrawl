//! 决策 REST 服务的运行期配置与出站决策渠道装配。
//!
//! 配置来自 `decision_models.toml` 的两段：`[api]`（本服务监听什么地址）
//! 与默认决策渠道（请求发往哪个决策服务）。没有渠道就不该启动——没有渠道就无从决策。
//! 本服务不做鉴权，因此只接受回环地址。

use omnicrawl_config::core::runtime::ConfigEnvironment;
use omnicrawl_config::features::decision_model::{
    load_decision_api_configuration, load_decision_model_configuration, DecisionApiConfig,
};
use omnicrawl_host::review::DecisionReviewOptions;

/// 决策 REST 服务的运行期配置。
#[derive(Clone)]
pub struct DecisionSettings {
    /// `[api]` 段：监听地址。
    pub api: DecisionApiConfig,
    /// 默认决策渠道；`None` 表示没有可用渠道（未配置或全部关闭）。
    pub channel: Option<DecisionReviewOptions>,
}

impl std::fmt::Debug for DecisionSettings {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // 渠道里带凭据，因此只报「有没有」，不把地址与密钥打进日志。
        formatter
            .debug_struct("DecisionSettings")
            .field("api", &self.api)
            .field("channel", &self.channel.is_some())
            .finish()
    }
}

impl DecisionSettings {
    /// 服务能否真正监听：`[api]` 已启用，并且有可用的决策渠道。
    pub fn ready(&self) -> bool {
        self.api.usable() && self.channel.is_some()
    }

    /// 不能监听时的可读原因；可用时返回 `None`。
    pub fn unavailable_reason(&self) -> Option<String> {
        if !self.api.enabled {
            return Some(
                "决策接口未启用：请在 decision_models.toml 的 [api] 段设置 enabled = true。"
                    .to_string(),
            );
        }
        if self.channel.is_none() {
            return Some(
                "没有可用的决策渠道：请检查 decision_models.toml 的 channels 段与 default_key。"
                    .to_string(),
            );
        }
        None
    }

    /// 监听地址文本。
    pub fn address(&self) -> String {
        self.api.address()
    }
}

/// 读配置装配运行期；缺段与坏值都按默认值回落（与决策功能其余部分同一口径）。
pub fn load_settings(environment: &ConfigEnvironment) -> DecisionSettings {
    let api = load_decision_api_configuration(environment, None);
    let channel = load_decision_model_configuration(environment, None)
        .ok()
        .and_then(|configuration| configuration.active_channel().cloned())
        .map(|channel| DecisionReviewOptions {
            mode: channel.mode,
            model: channel.model,
            base_url: channel.base_url,
            api_key: channel.api_key,
            api_key_env: channel.api_key_env,
        });
    DecisionSettings { api, channel }
}
