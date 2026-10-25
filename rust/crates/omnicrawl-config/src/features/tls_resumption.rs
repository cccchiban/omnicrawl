//! TLS 会话恢复（PSK）的配置。
//!
//! 默认**关闭**：会话恢复改的是密钥调度，一旦 binder 或 `early_secret` 算错，症状可能是
//! 「连接建立成功但解出垃圾」而不立刻报错。因此默认走完整握手，由这里显式开启，
//! 并始终保留握手失败回退完整握手的路径。
//!
//! Python 侧没有对映实现，因此不进 parity 数据集。

use std::path::{Path, PathBuf};

use crate::core::runtime::{load_config_data, save_config_data, ConfigEnvironment};
use crate::error::ConfigError;
use crate::toml::{Table, Value};

/// `[tls_resumption]` 段的配置。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TlsResumptionConfiguration {
    /// 是否启用会话恢复。默认关闭。
    pub enabled: bool,
    /// 缓存条目上限：票据只对签发它的服务端有效，跨站点抓取会不断写入。
    pub cache_capacity: usize,
}

impl Default for TlsResumptionConfiguration {
    fn default() -> Self {
        Self {
            enabled: false,
            cache_capacity: DEFAULT_CACHE_CAPACITY,
        }
    }
}

/// 缓存上限的默认值与合理区间。
///
/// 一次抓取的站点数通常是个位到几十，256 足够覆盖；上限存在的意义是防止长时间运行的
/// 进程把票据攒成泄漏。
pub const DEFAULT_CACHE_CAPACITY: usize = 256;
const MIN_CACHE_CAPACITY: usize = 1;
const MAX_CACHE_CAPACITY: usize = 8192;

impl TlsResumptionConfiguration {
    /// 把容量夹到合理区间；`0` 视为「用默认值」而不是「不许缓存」——
    /// 后者会让「启用恢复」变成一句空话。
    pub fn normalize(mut self) -> Self {
        if self.cache_capacity == 0 {
            self.cache_capacity = DEFAULT_CACHE_CAPACITY;
        }
        self.cache_capacity = self
            .cache_capacity
            .clamp(MIN_CACHE_CAPACITY, MAX_CACHE_CAPACITY);
        self
    }
}

/// 读取 `[tls_resumption]`；缺段时返回默认（关闭）。
pub fn load_tls_resumption_configuration(
    env: &ConfigEnvironment,
    config_path: Option<&Path>,
) -> Result<TlsResumptionConfiguration, ConfigError> {
    let data = load_config_data(env, config_path)?;
    let section = match data.get("tls_resumption") {
        None => Table::new(),
        Some(Value::String(text)) if text.is_empty() => Table::new(),
        Some(Value::Table(section)) => section.clone(),
        Some(_) => return Err(ConfigError::new("配置段 tls_resumption 必须是对象。")),
    };
    let enabled = match section.get("enabled") {
        Some(Value::Boolean(value)) => *value,
        None => false,
        Some(_) => return Err(ConfigError::new("tls_resumption.enabled 必须是布尔值。")),
    };
    let cache_capacity = match section.get("cache_capacity") {
        // `0` 交给 `normalize` 换成默认值：与「0 不是『不许缓存』」的约定保持一致。
        Some(Value::Integer(value)) if *value >= 0 => *value as usize,
        None => DEFAULT_CACHE_CAPACITY,
        Some(_) => {
            return Err(ConfigError::new(
                "tls_resumption.cache_capacity 必须是非负整数。",
            ))
        }
    };
    Ok(TlsResumptionConfiguration {
        enabled,
        cache_capacity,
    }
    .normalize())
}

/// 保留其他配置段，只更新 `[tls_resumption]`。
pub fn save_tls_resumption_configuration(
    env: &ConfigEnvironment,
    configuration: &TlsResumptionConfiguration,
    config_path: Option<&Path>,
) -> Result<PathBuf, ConfigError> {
    let configuration = configuration.clone().normalize();
    let mut data = load_config_data(env, config_path)?;
    let mut section = Table::new();
    section.insert("enabled".to_string(), Value::Boolean(configuration.enabled));
    section.insert(
        "cache_capacity".to_string(),
        Value::Integer(configuration.cache_capacity as i64),
    );
    data.insert("tls_resumption".to_string(), Value::Table(section));
    save_config_data(env, &data, config_path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_is_disabled() {
        let configuration = TlsResumptionConfiguration::default();
        assert!(
            !configuration.enabled,
            "会话恢复默认必须关闭：它改的是密钥调度"
        );
        assert_eq!(configuration.cache_capacity, DEFAULT_CACHE_CAPACITY);
    }

    #[test]
    fn a_zero_capacity_falls_back_to_the_default_instead_of_disabling_the_cache() {
        let normalized = TlsResumptionConfiguration {
            enabled: true,
            cache_capacity: 0,
        }
        .normalize();
        assert_eq!(normalized.cache_capacity, DEFAULT_CACHE_CAPACITY);
    }

    #[test]
    fn the_capacity_is_clamped_to_a_sane_range() {
        assert_eq!(
            TlsResumptionConfiguration {
                enabled: true,
                cache_capacity: usize::MAX,
            }
            .normalize()
            .cache_capacity,
            MAX_CACHE_CAPACITY
        );
    }
}
