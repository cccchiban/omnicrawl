//! 工具输出压缩配置（对应 `omnicrawl/config/features/tool_output_compression.py`）。
//!
//! 默认关闭：显式启用并选定压缩模型后才生效。

use std::path::{Path, PathBuf};

use crate::core::runtime::{get_section, load_config_data, save_config_data, ConfigEnvironment};
use crate::error::ConfigError;
use crate::toml::{Table, Value};
use crate::value::python_str;

pub const DEFAULT_MIN_CHARS: i64 = 1200;
pub const DEFAULT_MAX_INPUT_CHARS: i64 = 24_000;
pub const DEFAULT_MAX_OUTPUT_CHARS: i64 = 1_500;
pub const DEFAULT_TIMEOUT_SECONDS: i64 = 60;

/// 思考深度：关闭思考由 `thinking_enabled=false` 表达，因此这里不含 `none`。
pub const DEFAULT_THINKING_EFFORT: &str = "low";

/// 可选思考深度：按源码顺序过滤 [`VALID_REASONING_EFFORTS`] 的结果。
pub const THINKING_EFFORT_OPTIONS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];

/// `", ".join(THINKING_EFFORT_OPTIONS)` 的拼接结果。
const REASONING_EFFORT_ALLOWED: &str = "low, medium, high, xhigh, max";

/// 工具输出压缩的开关、模型选择与预算。
#[derive(Debug, Clone, PartialEq)]
pub struct ToolOutputCompressionConfig {
    pub enabled: bool,
    pub model_key: String,
    pub thinking_enabled: bool,
    pub reasoning_effort: String,
    pub min_chars: i64,
    pub max_input_chars: i64,
    pub max_output_chars: i64,
    pub timeout_seconds: i64,
}

impl Default for ToolOutputCompressionConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            model_key: String::new(),
            thinking_enabled: false,
            reasoning_effort: DEFAULT_THINKING_EFFORT.to_string(),
            min_chars: DEFAULT_MIN_CHARS,
            max_input_chars: DEFAULT_MAX_INPUT_CHARS,
            max_output_chars: DEFAULT_MAX_OUTPUT_CHARS,
            timeout_seconds: DEFAULT_TIMEOUT_SECONDS,
        }
    }
}

impl ToolOutputCompressionConfig {
    /// Python `__post_init__`：思考深度取值域与四个预算的正整数校验。
    pub fn validate(self) -> Result<Self, ConfigError> {
        if !THINKING_EFFORT_OPTIONS.contains(&self.reasoning_effort.as_str()) {
            return Err(ConfigError::new(format!(
                "tool_output_compression.reasoning_effort 仅支持 {REASONING_EFFORT_ALLOWED}，\
                 当前值：{}；关闭思考请设 thinking_enabled = false。",
                self.reasoning_effort
            )));
        }
        require_positive_int("min_chars", self.min_chars)?;
        require_positive_int("max_input_chars", self.max_input_chars)?;
        require_positive_int("max_output_chars", self.max_output_chars)?;
        require_positive_int("timeout_seconds", self.timeout_seconds)?;
        Ok(self)
    }

    /// 是否真正可用：显式启用且已选择压缩模型。
    pub fn active(&self) -> bool {
        self.enabled && !self.model_key.trim().is_empty()
    }
}

/// 读取 `config.toml` 的 `[tool_output_compression]` 段；缺失即默认关闭。
pub fn load_tool_output_compression_config(
    env: &ConfigEnvironment,
    config_path: Option<&Path>,
) -> Result<ToolOutputCompressionConfig, ConfigError> {
    let data = load_config_data(env, config_path)?;
    let section = get_section(&data, "tool_output_compression")?;
    parse_section(&section)
}

fn parse_section(section: &Table) -> Result<ToolOutputCompressionConfig, ConfigError> {
    let defaults = ToolOutputCompressionConfig::default();
    let reasoning_effort = {
        let raw = python_str(section.get("reasoning_effort"));
        let raw = raw.trim();
        if raw.is_empty() {
            DEFAULT_THINKING_EFFORT.to_string()
        } else {
            raw.to_string()
        }
    };
    ToolOutputCompressionConfig {
        enabled: bool_field(section, "enabled", false)?,
        model_key: python_str(section.get("model_key")).trim().to_string(),
        thinking_enabled: bool_field(section, "thinking_enabled", false)?,
        reasoning_effort,
        min_chars: int_field(section, "min_chars", defaults.min_chars)?,
        max_input_chars: int_field(section, "max_input_chars", defaults.max_input_chars)?,
        max_output_chars: int_field(section, "max_output_chars", defaults.max_output_chars)?,
        timeout_seconds: int_field(section, "timeout_seconds", defaults.timeout_seconds)?,
    }
    .validate()
}

/// 把工具输出压缩配置写回 `config.toml`（保留其他段）。
pub fn save_tool_output_compression_config(
    env: &ConfigEnvironment,
    config: &ToolOutputCompressionConfig,
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
        "thinking_enabled".to_string(),
        Value::Boolean(config.thinking_enabled),
    );
    section.insert(
        "reasoning_effort".to_string(),
        Value::String(config.reasoning_effort.clone()),
    );
    section.insert("min_chars".to_string(), Value::Integer(config.min_chars));
    section.insert(
        "max_input_chars".to_string(),
        Value::Integer(config.max_input_chars),
    );
    section.insert(
        "max_output_chars".to_string(),
        Value::Integer(config.max_output_chars),
    );
    section.insert(
        "timeout_seconds".to_string(),
        Value::Integer(config.timeout_seconds),
    );
    data.insert("tool_output_compression".to_string(), Value::Table(section));
    save_config_data(env, &data, config_path)
}

/// 关闭工具输出压缩：置 `enabled=False` 并清空 `model_key`。
pub fn clear_tool_output_compression_config(
    env: &ConfigEnvironment,
    config_path: Option<&Path>,
) -> Result<PathBuf, ConfigError> {
    save_tool_output_compression_config(
        env,
        &ToolOutputCompressionConfig {
            enabled: false,
            ..ToolOutputCompressionConfig::default()
        },
        config_path,
    )
}

fn require_positive_int(name: &str, value: i64) -> Result<(), ConfigError> {
    if value <= 0 {
        return Err(ConfigError::new(format!(
            "tool_output_compression.{name} 必须是正整数。"
        )));
    }
    Ok(())
}

fn bool_field(section: &Table, name: &str, default: bool) -> Result<bool, ConfigError> {
    match section.get(name) {
        None => Ok(default),
        Some(Value::Boolean(flag)) => Ok(*flag),
        Some(_) => Err(ConfigError::new(format!(
            "tool_output_compression.{name} 必须是布尔值。"
        ))),
    }
}

fn int_field(section: &Table, name: &str, default: i64) -> Result<i64, ConfigError> {
    match section.get(name) {
        None => Ok(default),
        Some(Value::Integer(value)) => Ok(*value),
        Some(_) => Err(ConfigError::new(format!(
            "tool_output_compression.{name} 必须是整数。"
        ))),
    }
}
