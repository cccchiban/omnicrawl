//! 上下文压缩与测量模式的运行配置（对应 `omnicrawl/config/features/context_compaction.py`）。

use std::path::Path;

use crate::core::runtime::{get_section, load_config_data, ConfigEnvironment};
use crate::error::ConfigError;
use crate::toml::{Table, Value};

/// 已删除的配置项：旧 `config.toml` 里残留时忽略，避免严格校验直接拒绝启动。
const RETIRED_FIELDS: [&str; 1] = ["minimum_turns_between_model_compactions"];

/// 上下文压缩配置。
///
/// `target_summary_tokens` 为 0 表示不设摘要预算上限；`trigger_context_percent` 为 `None`
/// 表示显式使用固定 Token 阈值。
#[derive(Debug, Clone, PartialEq)]
pub struct ContextCompactionConfig {
    pub trigger_context_percent: Option<i64>,
    pub trigger_context_tokens: i64,
    pub next_user_reserve_tokens: i64,
    pub emergency_context_ratio: f64,
    pub summary_profile: String,
    pub reasoning_effort: String,
    pub recent_turns: i64,
    pub target_summary_tokens: i64,
    pub preserve_exact_evidence: bool,
    pub archive_compacted_events: bool,
    pub auto_memory_recall: bool,
    pub allow_cross_provider: bool,
    pub failure_fallback: String,
}

impl Default for ContextCompactionConfig {
    fn default() -> Self {
        Self {
            trigger_context_percent: Some(80),
            trigger_context_tokens: 100_000,
            next_user_reserve_tokens: 10_240,
            emergency_context_ratio: 0.85,
            summary_profile: String::new(),
            reasoning_effort: "low".to_string(),
            recent_turns: 6,
            target_summary_tokens: 6_000,
            preserve_exact_evidence: true,
            archive_compacted_events: true,
            auto_memory_recall: true,
            allow_cross_provider: false,
            failure_fallback: "deterministic".to_string(),
        }
    }
}

impl ContextCompactionConfig {
    /// Python `__post_init__`：字段区间与类型校验。
    pub fn validate(self) -> Result<Self, ConfigError> {
        require_positive_int("trigger_context_tokens", self.trigger_context_tokens)?;
        require_positive_int("next_user_reserve_tokens", self.next_user_reserve_tokens)?;
        require_positive_int("recent_turns", self.recent_turns)?;
        // target_summary_tokens 允许 0（0 = 无摘要预算上限），其余必须为正整数。
        if self.target_summary_tokens < 0 {
            return Err(ConfigError::new(
                "context_compaction.target_summary_tokens 必须是非负整数（0 表示无预算限制）。",
            ));
        }
        if let Some(percent) = self.trigger_context_percent {
            require_positive_int("trigger_context_percent", percent)?;
        }
        require_ratio("emergency_context_ratio", self.emergency_context_ratio, 1.0)?;
        if self.reasoning_effort.trim().is_empty() {
            return Err(ConfigError::new(
                "context_compaction.reasoning_effort 必须是非空字符串。",
            ));
        }
        if self.failure_fallback != "deterministic" {
            return Err(ConfigError::new(
                "context_compaction.failure_fallback 当前仅支持 deterministic。",
            ));
        }
        Ok(self)
    }
}

/// 从 `config.toml` 读取并严格校验 `context_compaction` 段。
pub fn load_context_compaction_config(
    env: &ConfigEnvironment,
    config_path: Option<&Path>,
) -> Result<ContextCompactionConfig, ConfigError> {
    let data = load_config_data(env, config_path)?;
    let section = get_section(&data, "context_compaction")?;
    let defaults = ContextCompactionConfig::default();

    let config = ContextCompactionConfig {
        trigger_context_percent: optional_positive_int(
            &section,
            "trigger_context_percent",
            defaults.trigger_context_percent,
        )?,
        trigger_context_tokens: integer_field(
            &section,
            "trigger_context_tokens",
            defaults.trigger_context_tokens,
            "context_compaction.trigger_context_tokens 必须是正整数。",
        )?,
        next_user_reserve_tokens: integer_field(
            &section,
            "next_user_reserve_tokens",
            defaults.next_user_reserve_tokens,
            "context_compaction.next_user_reserve_tokens 必须是正整数。",
        )?,
        emergency_context_ratio: number_field(
            &section,
            "emergency_context_ratio",
            defaults.emergency_context_ratio,
        )?,
        summary_profile: string_field(&section, "summary_profile", &defaults.summary_profile)?,
        reasoning_effort: match section.get("reasoning_effort") {
            None => defaults.reasoning_effort.clone(),
            Some(Value::String(text)) => text.clone(),
            // Python 对非字符串同样报「非空字符串」这条文案。
            Some(_) => {
                return Err(ConfigError::new(
                    "context_compaction.reasoning_effort 必须是非空字符串。",
                ))
            }
        },
        recent_turns: integer_field(
            &section,
            "recent_turns",
            defaults.recent_turns,
            "context_compaction.recent_turns 必须是正整数。",
        )?,
        target_summary_tokens: integer_field(
            &section,
            "target_summary_tokens",
            defaults.target_summary_tokens,
            "context_compaction.target_summary_tokens 必须是非负整数（0 表示无预算限制）。",
        )?,
        preserve_exact_evidence: bool_field(
            &section,
            "preserve_exact_evidence",
            defaults.preserve_exact_evidence,
        )?,
        archive_compacted_events: bool_field(
            &section,
            "archive_compacted_events",
            defaults.archive_compacted_events,
        )?,
        auto_memory_recall: bool_field(
            &section,
            "auto_memory_recall",
            defaults.auto_memory_recall,
        )?,
        allow_cross_provider: bool_field(
            &section,
            "allow_cross_provider",
            defaults.allow_cross_provider,
        )?,
        failure_fallback: match section.get("failure_fallback") {
            None => defaults.failure_fallback.clone(),
            Some(Value::String(text)) => text.clone(),
            // 非字符串必然不等于 `deterministic`，落到同一条文案。
            Some(_) => {
                return Err(ConfigError::new(
                    "context_compaction.failure_fallback 当前仅支持 deterministic。",
                ))
            }
        },
    };
    reject_unknown_fields(&section, &config_field_names())?;
    config.validate()
}

fn config_field_names() -> Vec<&'static str> {
    vec![
        "trigger_context_percent",
        "trigger_context_tokens",
        "next_user_reserve_tokens",
        "emergency_context_ratio",
        "summary_profile",
        "reasoning_effort",
        "recent_turns",
        "target_summary_tokens",
        "preserve_exact_evidence",
        "archive_compacted_events",
        "auto_memory_recall",
        "allow_cross_provider",
        "failure_fallback",
    ]
}

fn reject_unknown_fields(section: &Table, allowed: &[&str]) -> Result<(), ConfigError> {
    let mut unknown: Vec<&str> = section
        .keys()
        .map(|key| key.as_str())
        .filter(|key| !allowed.contains(key) && !RETIRED_FIELDS.contains(key))
        .collect();
    unknown.sort();
    if !unknown.is_empty() {
        return Err(ConfigError::new(format!(
            "context_compaction 包含未知配置项：{}",
            unknown.join(", ")
        )));
    }
    Ok(())
}

fn require_positive_int(name: &str, value: i64) -> Result<(), ConfigError> {
    if value <= 0 {
        return Err(ConfigError::new(format!(
            "context_compaction.{name} 必须是正整数。"
        )));
    }
    Ok(())
}

fn require_ratio(name: &str, value: f64, upper: f64) -> Result<(), ConfigError> {
    if value <= 0.0 || value > upper || (upper == 1.0 && value >= 1.0) {
        let comparator = if upper == 1.0 {
            "< 1".to_string()
        } else {
            format!("<= {upper}")
        };
        return Err(ConfigError::new(format!(
            "context_compaction.{name} 必须满足 0 < value {comparator}。"
        )));
    }
    Ok(())
}

fn integer_field(
    section: &Table,
    name: &str,
    default: i64,
    message: &str,
) -> Result<i64, ConfigError> {
    match section.get(name) {
        None => Ok(default),
        Some(Value::Integer(value)) => Ok(*value),
        Some(_) => Err(ConfigError::new(message)),
    }
}

fn optional_positive_int(
    section: &Table,
    name: &str,
    default: Option<i64>,
) -> Result<Option<i64>, ConfigError> {
    match section.get(name) {
        None => Ok(default),
        Some(Value::Integer(value)) => Ok(Some(*value)),
        Some(_) => Err(ConfigError::new(format!(
            "context_compaction.{name} 必须是正整数。"
        ))),
    }
}

fn number_field(section: &Table, name: &str, default: f64) -> Result<f64, ConfigError> {
    match section.get(name) {
        None => Ok(default),
        Some(Value::Integer(value)) => Ok(*value as f64),
        Some(Value::Float(value)) => Ok(*value),
        Some(_) => Err(ConfigError::new(format!(
            "context_compaction.{name} 必须是数字。"
        ))),
    }
}

fn string_field(section: &Table, name: &str, default: &str) -> Result<String, ConfigError> {
    match section.get(name) {
        None => Ok(default.to_string()),
        Some(Value::String(text)) => Ok(text.clone()),
        Some(_) => Err(ConfigError::new(format!(
            "context_compaction.{name} 必须是字符串。"
        ))),
    }
}

fn bool_field(section: &Table, name: &str, default: bool) -> Result<bool, ConfigError> {
    match section.get(name) {
        None => Ok(default),
        Some(Value::Boolean(flag)) => Ok(*flag),
        Some(_) => Err(ConfigError::new(format!(
            "context_compaction.{name} 必须是布尔值。"
        ))),
    }
}
