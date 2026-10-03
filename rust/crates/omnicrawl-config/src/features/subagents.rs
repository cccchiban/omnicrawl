//! SubAgent 运行配置（对应 `omnicrawl/config/features/subagents.py`）。
//!
//! 默认关闭；配置项只读 `subagents.toml` 的 `subagents` 段，不回退读 `config.toml`。

use std::collections::BTreeMap;
use std::path::Path;

use toml::Value;

use crate::core::runtime::{
    get_section, load_config_data, resolve_subagents_path, ConfigEnvironment,
};
use crate::error::ConfigError;

/// 设置面板允许调整的 SubAgent 资源参数（顺序与 Python 的规则表一致）。
pub const SUBAGENT_ADVANCED_SETTING_KEYS: [&str; 6] = [
    "max_concurrency",
    "max_tasks_per_batch",
    "default_timeout_seconds",
    "model_request_concurrency",
    "verify_command_timeout_seconds",
    "task_retention_minutes",
];

/// 设置面板校验结果：整型项与数字项分开保留。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AdvancedValue {
    Int(i64),
    Number(f64),
}

/// SubAgent 全局边界。
#[derive(Debug, Clone, PartialEq)]
pub struct SubAgentConfig {
    pub enabled: bool,
    pub max_depth: i64,
    pub max_concurrency: i64,
    pub max_tasks_per_batch: i64,
    pub default_timeout_seconds: f64,
    pub model_request_concurrency: i64,
    pub allow_background: bool,
    pub allow_fork: bool,
    pub allow_shared_workspace_writes: bool,
    pub allow_worktree: bool,
    pub allow_standard_agent: bool,
    pub enable_verify_agent: bool,
    pub verify_command_timeout_seconds: i64,
    pub task_retention_minutes: i64,
    pub result_summary_chars: i64,
    /// 按子代理角色名单独指定模型；空字符串或 `inherit` 表示不覆盖。
    pub model_overrides: BTreeMap<String, String>,
}

impl Default for SubAgentConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_depth: 1,
            max_concurrency: 2,
            max_tasks_per_batch: 4,
            default_timeout_seconds: 3600.0,
            model_request_concurrency: 2,
            allow_background: false,
            allow_fork: false,
            allow_shared_workspace_writes: false,
            allow_worktree: false,
            allow_standard_agent: false,
            enable_verify_agent: false,
            verify_command_timeout_seconds: 120,
            task_retention_minutes: 60,
            result_summary_chars: 6000,
            model_overrides: BTreeMap::new(),
        }
    }
}

/// 校验设置面板允许调整的 SubAgent 资源参数。
pub fn validate_subagent_advanced_setting(
    name: &str,
    value: &Value,
) -> Result<AdvancedValue, ConfigError> {
    let (value_type, minimum, maximum) = advanced_rule(name)?;
    if value_type == "int" {
        let Some(integer) = value.as_integer() else {
            return Err(ConfigError::new(format!(
                "配置项 subagents.{name} 必须是整数。"
            )));
        };
        if (integer as f64) < minimum || (integer as f64) > maximum {
            return Err(ConfigError::new(format!(
                "配置项 subagents.{name} 必须在 {} 到 {} 之间，当前值：{integer}。",
                format_g(minimum),
                format_g(maximum)
            )));
        }
        return Ok(AdvancedValue::Int(integer));
    }
    let Some(number) = value
        .as_float()
        .or_else(|| value.as_integer().map(|integer| integer as f64))
    else {
        return Err(ConfigError::new(format!(
            "配置项 subagents.{name} 必须是数字。"
        )));
    };
    if number < minimum || number > maximum {
        return Err(ConfigError::new(format!(
            "配置项 subagents.{name} 必须在 {} 到 {} 之间，当前值：{}。",
            format_g(minimum),
            format_g(maximum),
            format_g(number)
        )));
    }
    Ok(AdvancedValue::Number(number))
}

fn advanced_rule(name: &str) -> Result<(&'static str, f64, f64), ConfigError> {
    let rule = match name {
        "max_concurrency" => ("int", 1.0, 4.0),
        "max_tasks_per_batch" => ("int", 1.0, 4.0),
        "default_timeout_seconds" => ("number", 1.0, 3600.0),
        "model_request_concurrency" => ("int", 1.0, 4.0),
        "verify_command_timeout_seconds" => ("int", 1.0, 360.0),
        "task_retention_minutes" => ("int", 1.0, 10_080.0),
        _ => {
            return Err(ConfigError::new(format!(
                "设置面板不支持配置项 subagents.{name}。"
            )))
        }
    };
    Ok(rule)
}

/// 读取独立 `subagents.toml` 的 `subagents` 段。
pub fn load_subagent_config(
    env: &ConfigEnvironment,
    subagents_path: Option<&Path>,
) -> Result<SubAgentConfig, ConfigError> {
    let target = resolve_subagents_path(env, subagents_path)?;
    let data = load_config_data(env, Some(&target))?;
    let section = get_section(&data, "subagents")?;

    let config = SubAgentConfig {
        enabled: bool_field(&section, "enabled", false)?,
        max_depth: int_field(&section, "max_depth", 1, 1, 1)?,
        max_concurrency: int_field(&section, "max_concurrency", 2, 1, 4)?,
        max_tasks_per_batch: int_field(&section, "max_tasks_per_batch", 4, 1, 4)?,
        default_timeout_seconds: number_field(
            &section,
            "default_timeout_seconds",
            3600.0,
            1.0,
            3600.0,
        )?,
        model_request_concurrency: int_field(&section, "model_request_concurrency", 2, 1, 4)?,
        allow_background: bool_field(&section, "allow_background", false)?,
        allow_fork: bool_field(&section, "allow_fork", false)?,
        allow_shared_workspace_writes: bool_field(
            &section,
            "allow_shared_workspace_writes",
            false,
        )?,
        allow_worktree: bool_field(&section, "allow_worktree", false)?,
        allow_standard_agent: bool_field(&section, "allow_standard_agent", false)?,
        enable_verify_agent: bool_field(&section, "enable_verify_agent", false)?,
        verify_command_timeout_seconds: int_field(
            &section,
            "verify_command_timeout_seconds",
            120,
            1,
            360,
        )?,
        task_retention_minutes: int_field(&section, "task_retention_minutes", 60, 1, 10_080)?,
        result_summary_chars: int_field(&section, "result_summary_chars", 6000, 100, 50_000)?,
        model_overrides: parse_model_overrides(&section)?,
    };

    Ok(config)
}

fn parse_model_overrides(section: &toml::Table) -> Result<BTreeMap<String, String>, ConfigError> {
    let Some(raw_models) = section.get("models") else {
        return Ok(BTreeMap::new());
    };
    if raw_models.is_str() && raw_models.as_str().unwrap_or_default().is_empty() {
        return Ok(BTreeMap::new());
    }
    let Some(entries) = raw_models.as_table() else {
        return Err(ConfigError::new("配置项 subagents.models 必须是对象。"));
    };
    let mut overrides = BTreeMap::new();
    for (role_name, raw_entry) in entries {
        let role = role_name.trim().to_lowercase();
        if role.is_empty() {
            continue;
        }
        let Some(entry) = raw_entry.as_table() else {
            return Err(ConfigError::new(format!(
                "配置项 subagents.models.{role_name} 必须是对象。"
            )));
        };
        let model = match entry.get("model") {
            Some(value) if !toml_truthy(value) => String::new(),
            Some(value) => toml_text(value),
            None => String::new(),
        }
        .trim()
        .to_string();
        if model.is_empty() || model.to_lowercase() == "inherit" {
            continue;
        }
        overrides.insert(role, model);
    }
    Ok(overrides)
}

fn bool_field(section: &toml::Table, name: &str, default: bool) -> Result<bool, ConfigError> {
    match section.get(name) {
        None => Ok(default),
        Some(Value::Boolean(value)) => Ok(*value),
        Some(_) => Err(ConfigError::new(format!(
            "配置项 subagents.{name} 必须是布尔值。"
        ))),
    }
}

fn int_field(
    section: &toml::Table,
    name: &str,
    default: i64,
    minimum: i64,
    maximum: i64,
) -> Result<i64, ConfigError> {
    let value = match section.get(name) {
        None => default,
        Some(Value::Integer(value)) => *value,
        Some(_) => {
            return Err(ConfigError::new(format!(
                "配置项 subagents.{name} 必须是整数。"
            )))
        }
    };
    if value < minimum || value > maximum {
        return Err(ConfigError::new(format!(
            "配置项 subagents.{name} 必须在 {minimum} 到 {maximum} 之间，当前值：{value}。"
        )));
    }
    Ok(value)
}

fn number_field(
    section: &toml::Table,
    name: &str,
    default: f64,
    minimum: f64,
    maximum: f64,
) -> Result<f64, ConfigError> {
    let value = match section.get(name) {
        None => default,
        Some(Value::Float(value)) => *value,
        Some(Value::Integer(value)) => *value as f64,
        Some(_) => {
            return Err(ConfigError::new(format!(
                "配置项 subagents.{name} 必须是数字。"
            )))
        }
    };
    if value < minimum || value > maximum {
        return Err(ConfigError::new(format!(
            "配置项 subagents.{name} 必须在 {} 到 {} 之间，当前值：{}。",
            format_g(minimum),
            format_g(maximum),
            format_g(value)
        )));
    }
    Ok(value)
}

/// Python `%g` 的等价格式化：整数值去掉小数点，其余按最短表示。
fn format_g(value: f64) -> String {
    if value.fract() == 0.0 && value.abs() < 1_000_000.0 {
        return format!("{}", value as i64);
    }
    format!("{value}")
}

/// Python 真值：`false`、`0`、`0.0`、空串都是假。
fn toml_truthy(value: &Value) -> bool {
    match value {
        Value::Boolean(inner) => *inner,
        Value::Integer(inner) => *inner != 0,
        Value::Float(inner) => *inner != 0.0,
        Value::String(inner) => !inner.is_empty(),
        Value::Array(inner) => !inner.is_empty(),
        Value::Table(inner) => !inner.is_empty(),
        Value::Datetime(_) => true,
    }
}

/// Python `str(value)` 在 TOML 标量上的等价写法。
fn toml_text(value: &Value) -> String {
    match value {
        Value::Boolean(inner) => {
            if *inner {
                "True".to_string()
            } else {
                "False".to_string()
            }
        }
        Value::Integer(inner) => inner.to_string(),
        Value::Float(inner) => format!("{inner}"),
        Value::String(inner) => inner.clone(),
        other => other.to_string(),
    }
}
