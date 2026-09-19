//! 模型、LLM、渠道与视觉相关配置。

pub mod channels;
pub mod llm;
pub mod llm_multi;
pub mod model_catalog;
pub mod model_store;
pub mod vision;

use omnicrawl_llm::resolve_protocol;
use omnicrawl_protocol::Protocol;

use crate::core::runtime::ConfigEnvironment;
use crate::error::ConfigError;
use crate::toml::Table;

/// 一个可连接的 Provider 凭据与默认协议配置。
///
/// 对应 Python `omnicrawl/llm/registry.py` 的 `ProviderProfile`：内核 runtime 只用到它的子集，
/// 配置侧要完整保留（发现开关、默认窗口、超时与重试都由它带出来）。
#[derive(Debug, Clone, PartialEq)]
pub struct ProviderProfile {
    pub id: String,
    pub provider: String,
    pub enabled: bool,
    pub base_url: String,
    pub api_key: String,
    pub api_key_env: String,
    pub user_agent: String,
    pub default_protocol: String,
    pub discovery_enabled: bool,
    pub default_context_window_tokens: i64,
    pub provider_options: Table,
    pub request_timeout_seconds: f64,
    pub request_retry_count: i64,
    pub discovery_timeout_seconds: f64,
}

impl ProviderProfile {
    /// 生效协议：调用方指定 > Profile 默认 > Provider 默认，再校验与 Provider 匹配。
    pub fn resolve_protocol(&self, protocol: &str) -> Result<Protocol, ConfigError> {
        resolve_protocol(&self.provider, &self.default_protocol, protocol)
            .map_err(|error| ConfigError::new(error.message))
    }

    /// `api_key_env` 指向的环境变量优先，其次明文（对应 `openai_common.resolve_api_key`）。
    pub fn resolve_api_key(&self, env: &ConfigEnvironment) -> String {
        let name = self.api_key_env.trim();
        if !name.is_empty() {
            let value = env.get(name).unwrap_or_default();
            let value = value.trim();
            if !value.is_empty() {
                return value.to_string();
            }
        }
        self.api_key.trim().to_string()
    }
}
