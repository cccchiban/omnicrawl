//! 网页抓取接口（`config.toml` 的 `[fetch_api]` 段）。
//!
//! `fetcher` 工具本来只在 Agent 回合内可用：模型要抓网页就走一次工具调用。本段把同一份
//! 抓取能力以 REST 形式暴露给本机其它程序（Python 等批量脚本、CLI、Skill），它们不必经
//! 主模型就能抓网页——批量脚本要的是通道，不是对话。
//!
//! 与 `[api]`（本地 HTTP 服务）分开是刻意的：那套要 Bearer 令牌且承载会话与审批，
//! 这套**不做鉴权、不做参数校验**，是纯粹的抓取通道。因此只接受回环地址，
//! 「只有本机程序能调用」是唯一的准入控制。
//!
//! 抓取语义与 `fetcher` 工具完全同源（同一个 `fetch_batch`），差别只在入参形状：
//! 工具收模型给的宽松参数（`urls` 支持逗号/换行串），接口收 JSON 字段。

use std::path::{Path, PathBuf};

use crate::core::runtime::{load_config_data, save_config_data, ConfigEnvironment, get_section};
use crate::error::ConfigError;
use crate::toml::{Table, Value};
use crate::value::{python_str, truthy};

/// `config.toml` 里的段名。
pub const FETCH_API_SECTION: &str = "fetch_api";

/// 默认监听端口：与本地 API（8765）、决策接口（8767）错开。
pub const DEFAULT_FETCH_API_PORT: i64 = 8768;

/// 只接受回环地址（无鉴权的本地通道）。
pub const FETCH_API_LOOPBACK_HOSTS: [&str; 3] = ["127.0.0.1", "localhost", "::1"];

/// 网页抓取接口的运行期配置。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchApiConfig {
    /// 是否随宿主启动常驻服务。
    pub enabled: bool,
    pub host: String,
    pub port: i64,
    /// 固定浏览器指纹族（`chrome`/`firefox`/`safari`/`edge`），空串表示按请求参数。
    pub impersonate: String,
    /// 单次请求最多抓多少个 URL；0 表示用抓取执行体自身的上限（20）。
    pub max_urls: i64,
}

impl Default for FetchApiConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            host: "127.0.0.1".to_string(),
            port: DEFAULT_FETCH_API_PORT,
            impersonate: String::new(),
            max_urls: 0,
        }
    }
}

impl FetchApiConfig {
    /// 取值域校验与归一化（错误文案带 `fetch_api` 前缀，与段名一致）。
    pub fn normalize(mut self) -> Result<Self, ConfigError> {
        let host = self.host.trim().to_string();
        if !FETCH_API_LOOPBACK_HOSTS.contains(&host.to_lowercase().as_str()) {
            return Err(ConfigError::new(
                "抓取接口 fetch_api.host 仅允许回环地址（127.0.0.1 / localhost / ::1）。",
            ));
        }
        self.host = host;
        if !(1..=65535).contains(&self.port) {
            return Err(ConfigError::new(
                "抓取接口 fetch_api.port 必须是 1 到 65535 的整数。",
            ));
        }
        if !(0..=20).contains(&self.max_urls) {
            return Err(ConfigError::new(
                "抓取接口 fetch_api.max_urls 必须是 0 到 20 的整数（0 表示用默认上限 20）。",
            ));
        }
        self.impersonate = self.impersonate.trim().to_lowercase();
        Ok(self)
    }

    /// 服务能否真正监听（开关打开）。
    pub fn usable(&self) -> bool {
        self.enabled
    }

    /// 监听地址文本。
    pub fn address(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}

/// 读取 `[fetch_api]` 段；缺段或读不出来时返回默认值（未启用）。
pub fn load_fetch_api_configuration(
    env: &ConfigEnvironment,
    config_path: Option<&Path>,
) -> FetchApiConfig {
    let Ok(data) = load_config_data(env, config_path) else {
        return FetchApiConfig::default();
    };
    let Ok(section) = get_section(&data, FETCH_API_SECTION) else {
        return FetchApiConfig::default();
    };
    let defaults = FetchApiConfig::default();
    FetchApiConfig {
        enabled: section.get("enabled").map(truthy).unwrap_or(defaults.enabled),
        host: text_field(&section, "host", &defaults.host),
        port: match section.get("port") {
            Some(Value::Integer(value)) => *value,
            Some(Value::Float(value)) => *value as i64,
            _ => defaults.port,
        },
        impersonate: text_field(&section, "impersonate", ""),
        max_urls: match section.get("max_urls") {
            Some(Value::Integer(value)) => *value,
            Some(Value::Float(value)) => *value as i64,
            _ => defaults.max_urls,
        },
    }
    // 手改坏了监听地址或端口时不阻断其它功能：按「未启用」的默认值回落。
    .normalize()
    .unwrap_or(defaults)
}

/// 把 `[fetch_api]` 写回 `config.toml`，保留其它段。
pub fn save_fetch_api_configuration(
    env: &ConfigEnvironment,
    configuration: &FetchApiConfig,
    config_path: Option<&Path>,
) -> Result<PathBuf, ConfigError> {
    let normalized = configuration.clone().normalize()?;
    let mut data = load_config_data(env, config_path)?;
    let mut section = Table::new();
    section.insert("enabled".to_string(), Value::Boolean(normalized.enabled));
    section.insert("host".to_string(), Value::String(normalized.host));
    section.insert("port".to_string(), Value::Integer(normalized.port));
    section.insert(
        "impersonate".to_string(),
        Value::String(normalized.impersonate),
    );
    section.insert("max_urls".to_string(), Value::Integer(normalized.max_urls));
    data.insert(FETCH_API_SECTION.to_string(), Value::Table(section));
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

#[cfg(test)]
mod tests {
    use super::*;

    fn environment(tag: &str) -> ConfigEnvironment {
        let root = std::env::temp_dir().join(format!("oc-fetch-api-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join(crate::core::runtime::USER_CONFIG_DIRNAME))
            .expect("建立临时配置目录");
        ConfigEnvironment::new(root.to_string_lossy().to_string(), "win32")
    }

    #[test]
    fn defaults_to_disabled_on_loopback() {
        let env = environment("defaults");
        let configuration = load_fetch_api_configuration(&env, None);
        assert!(!configuration.enabled);
        assert_eq!(configuration.host, "127.0.0.1");
        assert_eq!(configuration.port, DEFAULT_FETCH_API_PORT);
        assert_eq!(configuration.address(), "127.0.0.1:8768");
    }

    #[test]
    fn round_trips_and_keeps_other_sections() {
        let env = environment("roundtrip");
        let mut data = Table::new();
        data.insert("version".to_string(), Value::Integer(2));
        save_config_data(&env, &data, None).expect("写基础配置");

        let configuration = FetchApiConfig {
            enabled: true,
            host: "localhost".to_string(),
            port: 18_768,
            impersonate: "firefox".to_string(),
            max_urls: 5,
        };
        save_fetch_api_configuration(&env, &configuration, None).expect("写抓取接口段");
        let reloaded = load_fetch_api_configuration(&env, None);
        assert_eq!(reloaded, configuration);

        // 其它段（含 version）不能被覆盖掉。
        let raw = load_config_data(&env, None).expect("读回配置");
        assert_eq!(raw.get("version"), Some(&Value::Integer(2)));
    }

    #[test]
    fn rejects_non_loopback_and_bad_port() {
        assert!(FetchApiConfig {
            host: "0.0.0.0".to_string(),
            ..FetchApiConfig::default()
        }
        .normalize()
        .is_err());
        assert!(FetchApiConfig {
            port: 0,
            ..FetchApiConfig::default()
        }
        .normalize()
        .is_err());
        assert!(FetchApiConfig {
            max_urls: 21,
            ..FetchApiConfig::default()
        }
        .normalize()
        .is_err());

        // 手改坏的值在装载时按默认值回落，而不是让整份配置失败。
        let env = environment("bad");
        let mut data = Table::new();
        let mut section = Table::new();
        section.insert("host".to_string(), Value::String("0.0.0.0".to_string()));
        data.insert(FETCH_API_SECTION.to_string(), Value::Table(section));
        save_config_data(&env, &data, None).expect("写坏值");
        assert_eq!(
            load_fetch_api_configuration(&env, None),
            FetchApiConfig::default()
        );
    }
}
