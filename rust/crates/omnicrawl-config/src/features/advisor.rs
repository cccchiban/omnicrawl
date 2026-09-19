//! Advisor（顾问策略）配置（对应 `omnicrawl/config/features/advisor.py`）。
//!
//! 默认关闭：只有显式启用、且选定了顾问模型时 [`AdvisorConfig::active`] 才为真。

use std::path::{Path, PathBuf};

use crate::core::runtime::{get_section, load_config_data, save_config_data, ConfigEnvironment};
use crate::error::ConfigError;
use crate::models::llm::VALID_REASONING_EFFORTS;
use crate::toml::{Table, Value};
use crate::value::python_str;

pub const DEFAULT_ADVISOR_EFFORT: &str = "high";

/// 可选推理档位：按源码顺序过滤 [`VALID_REASONING_EFFORTS`] 的结果。
pub const ADVISOR_EFFORT_OPTIONS: [&str; 6] = VALID_REASONING_EFFORTS;

/// `", ".join(sorted(VALID_REASONING_EFFORTS))` 的拼接结果。
const ADVISOR_EFFORT_ALLOWED: &str = "high, low, max, medium, none, xhigh";

/// Advisor 顾问策略开关与模型选择。
#[derive(Debug, Clone, PartialEq)]
pub struct AdvisorConfig {
    pub enabled: bool,
    pub model_key: String,
    pub effort: String,
    pub disabled_for_models: Vec<String>,
}

impl Default for AdvisorConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            model_key: String::new(),
            effort: DEFAULT_ADVISOR_EFFORT.to_string(),
            disabled_for_models: Vec::new(),
        }
    }
}

impl AdvisorConfig {
    /// Python `AdvisorConfig.__post_init__`：推理档位校验与黑名单归一化。
    pub fn normalize(mut self) -> Result<Self, ConfigError> {
        let effort = self.effort.trim();
        if !effort.is_empty() && !VALID_REASONING_EFFORTS.contains(&effort) {
            return Err(ConfigError::new(format!(
                "advisor.effort 仅支持 {ADVISOR_EFFORT_ALLOWED}，当前值：{effort}。"
            )));
        }
        self.disabled_for_models = self
            .disabled_for_models
            .iter()
            .map(|item| item.trim().to_string())
            .filter(|item| !item.is_empty())
            .collect();
        Ok(self)
    }

    /// 是否真正可用：显式启用且已选择顾问模型。
    pub fn active(&self) -> bool {
        self.enabled && !self.model_key.trim().is_empty()
    }

    /// 界面上展示的推理档位：空串回落到默认值。
    pub fn display_effort(&self) -> String {
        if self.effort.is_empty() {
            DEFAULT_ADVISOR_EFFORT.to_string()
        } else {
            self.effort.clone()
        }
    }
}

/// 读取 `config.toml` 的 `[advisor]` 段；缺失或为空时返回默认关闭配置。
pub fn load_advisor_config(
    env: &ConfigEnvironment,
    config_path: Option<&Path>,
) -> Result<AdvisorConfig, ConfigError> {
    let data = load_config_data(env, config_path)?;
    let section = get_section(&data, "advisor")?;
    parse_advisor_section(&section)
}

fn parse_advisor_section(section: &Table) -> Result<AdvisorConfig, ConfigError> {
    let enabled = bool_field(section, "enabled", false)?;
    let model_key = text_field(section, "model_key").trim().to_string();
    let effort = {
        let raw = text_field(section, "effort").trim().to_string();
        if raw.is_empty() {
            DEFAULT_ADVISOR_EFFORT.to_string()
        } else {
            raw
        }
    };
    let disabled_for_models = match section.get("disabled_for_models") {
        None => Vec::new(),
        Some(Value::String(text)) if text.is_empty() => Vec::new(),
        Some(Value::Array(items)) => items
            .iter()
            .map(python_text)
            .map(|item| item.trim().to_string())
            .filter(|item| !item.is_empty())
            .collect(),
        Some(_) => {
            return Err(ConfigError::new(
                "advisor.disabled_for_models 必须是字符串数组。",
            ))
        }
    };
    AdvisorConfig {
        enabled,
        model_key,
        effort,
        disabled_for_models,
    }
    .normalize()
}

/// 把 advisor 配置写回 `config.toml` 的 `[advisor]` 段（保留其他段）。
pub fn save_advisor_config(
    env: &ConfigEnvironment,
    config: &AdvisorConfig,
    config_path: Option<&Path>,
) -> Result<PathBuf, ConfigError> {
    let mut data = load_config_data(env, config_path)?;
    let mut section = Table::new();
    section.insert("enabled".to_string(), Value::Boolean(config.enabled));
    section.insert(
        "model_key".to_string(),
        Value::String(config.model_key.clone()),
    );
    section.insert(
        "effort".to_string(),
        Value::String(if config.effort.is_empty() {
            DEFAULT_ADVISOR_EFFORT.to_string()
        } else {
            config.effort.clone()
        }),
    );
    section.insert(
        "disabled_for_models".to_string(),
        Value::Array(
            config
                .disabled_for_models
                .iter()
                .map(|item| Value::String(item.clone()))
                .collect(),
        ),
    );
    data.insert("advisor".to_string(), Value::Table(section));
    save_config_data(env, &data, config_path)
}

/// 清除 advisor 选择：置 `enabled=False` 并清空 `model_key`。
pub fn clear_advisor_config(
    env: &ConfigEnvironment,
    config_path: Option<&Path>,
) -> Result<PathBuf, ConfigError> {
    save_advisor_config(env, &AdvisorConfig::default(), config_path)
}

fn bool_field(section: &Table, name: &str, default: bool) -> Result<bool, ConfigError> {
    match section.get(name) {
        None => Ok(default),
        Some(Value::Boolean(flag)) => Ok(*flag),
        Some(_) => Err(ConfigError::new(format!("advisor.{name} 必须是布尔值。"))),
    }
}

/// Python `str(section.get(name) or "")`。
fn text_field(section: &Table, name: &str) -> String {
    python_str(section.get(name))
}

/// Python `str(value)`：字符串原样，其余走 `repr`。
fn python_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => crate::value::python_repr(other),
    }
}
