//! 记忆功能配置（`config.toml` 的 `[memory]` 段）。
//!
//! 目前只有一项：记忆遗忘时间。`enabled` 仍由功能开关那条既有链路读写
//! （`core::settings::save_feature_enabled`），这里不重复实现，避免两处都能改同一个键。

use std::path::{Path, PathBuf};

use crate::core::runtime::{get_section, load_config_data, save_config_data, ConfigEnvironment};
use crate::error::ConfigError;
use crate::toml::Value;

/// 可选遗忘时间（天）：一天、十天、一月、一年、两年、三年。
///
/// 写死成一个封闭集合而不是任意整数：界面上是有限档位，配置里出现别的值只可能是手改错了，
/// 这时回落默认值而不是报错——一个数字不该让整个记忆功能起不来。
pub const FORGET_AFTER_DAYS_CHOICES: [i64; 6] = [1, 10, 30, 365, 730, 1095];

/// 默认遗忘时间：一年。
///
/// 取一年而不是「一个月」：默认值不该让用户悄悄丢掉攒下来的东西。真要短就自己选。
pub const DEFAULT_FORGET_AFTER_DAYS: i64 = 365;

/// 遗忘时间的界面文案。
pub fn forget_after_label(days: i64) -> String {
    match days {
        1 => "一天".to_string(),
        10 => "十天".to_string(),
        30 => "一月".to_string(),
        365 => "一年".to_string(),
        730 => "两年".to_string(),
        1095 => "三年".to_string(),
        other => format!("{other} 天"),
    }
}

/// 把任意值归一到最近的可选值：手改成 `7` 就落到「十天」。
pub fn normalize_forget_after_days(days: i64) -> i64 {
    FORGET_AFTER_DAYS_CHOICES
        .into_iter()
        .min_by_key(|choice| (choice - days).abs())
        .unwrap_or(DEFAULT_FORGET_AFTER_DAYS)
}

/// `[memory]` 段的记忆功能配置。
#[derive(Debug, Clone, PartialEq)]
pub struct MemoryConfig {
    /// 多久没被召回就遗忘（天）。
    pub forget_after_days: i64,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            forget_after_days: DEFAULT_FORGET_AFTER_DAYS,
        }
    }
}

impl MemoryConfig {
    /// 校验并归一：不在可选集合里的值落到最近的可选值，而不是报错。
    pub fn validate(self) -> Result<Self, ConfigError> {
        if self.forget_after_days <= 0 {
            return Err(ConfigError::new(
                "memory.forget_after_days 必须大于 0。",
            ));
        }
        Ok(Self {
            forget_after_days: normalize_forget_after_days(self.forget_after_days),
            ..self
        })
    }

    /// 遗忘判定：`age_days` 超过这个天数即过期。
    ///
    /// 保留原有的 `7 + touch_count` 加成：被反复召回的条目更难被忘掉，这是既有行为，
    /// 不在这次改动里推翻。
    pub fn is_expired(&self, age_days: f64, touch_count: u32) -> bool {
        age_days >= self.forget_after_days as f64 + touch_count as f64
    }
}

/// 读取 `[memory]` 段；缺项回落默认值，类型不对返回中文错误。
pub fn load_memory_config(
    env: &ConfigEnvironment,
    config_path: Option<&Path>,
) -> Result<MemoryConfig, ConfigError> {
    let data = load_config_data(env, config_path)?;
    let section = get_section(&data, "memory")?;
    let days = match section.get("forget_after_days") {
        None => DEFAULT_FORGET_AFTER_DAYS,
        Some(Value::Integer(value)) => *value,
        Some(_) => {
            return Err(ConfigError::new(
                "memory.forget_after_days 必须是整数（天）。",
            ))
        }
    };
    MemoryConfig {
        forget_after_days: days,
    }
    .validate()
}

/// 写入遗忘时间，返回落盘路径。
pub fn save_forget_after_days(
    env: &ConfigEnvironment,
    days: i64,
    config_path: Option<&Path>,
) -> Result<PathBuf, ConfigError> {
    let config = MemoryConfig {
        forget_after_days: days,
    }
    .validate()?;
    let mut data = load_config_data(env, config_path)?;
    let mut section = get_section(&data, "memory")?;
    section.insert(
        "forget_after_days".to_string(),
        Value::Integer(config.forget_after_days),
    );
    data.insert("memory".to_string(), Value::Table(section));
    save_config_data(env, &data, config_path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_choice_is_labelled() {
        assert_eq!(forget_after_label(1), "一天");
        assert_eq!(forget_after_label(10), "十天");
        assert_eq!(forget_after_label(30), "一月");
        assert_eq!(forget_after_label(365), "一年");
        assert_eq!(forget_after_label(730), "两年");
        assert_eq!(forget_after_label(1095), "三年");
    }

    #[test]
    fn stray_values_snap_to_the_nearest_choice() {
        assert_eq!(normalize_forget_after_days(7), 10);
        assert_eq!(normalize_forget_after_days(2), 1);
        assert_eq!(normalize_forget_after_days(500), 365);
        assert_eq!(normalize_forget_after_days(45), 30);
    }

    #[test]
    fn the_default_is_a_year() {
        assert_eq!(MemoryConfig::default().forget_after_days, 365);
    }

    #[test]
    fn a_non_positive_window_is_rejected() {
        assert!(MemoryConfig {
            forget_after_days: 0
        }
        .validate()
        .is_err());
        assert!(MemoryConfig {
            forget_after_days: -3
        }
        .validate()
        .is_err());
    }

    #[test]
    fn recalled_entries_live_longer() {
        let config = MemoryConfig {
            forget_after_days: 10,
        };
        // 十天没动过就过期。
        assert!(config.is_expired(10.0, 0));
        assert!(!config.is_expired(9.9, 0));
        // 被召回三次的还能再撑三天。
        assert!(!config.is_expired(12.0, 3));
        assert!(config.is_expired(13.0, 3));
    }
}
