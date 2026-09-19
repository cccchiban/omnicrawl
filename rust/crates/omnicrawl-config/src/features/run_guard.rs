//! Agent 运行节奏护栏与自动续跑配置（对应 `omnicrawl/config/features/run_guard.py`）。

use std::path::{Path, PathBuf};

use crate::core::runtime::{get_section, load_config_data, save_config_data, ConfigEnvironment};
use crate::error::ConfigError;
use crate::toml::{Table, Value};

/// 单次模型调用的 reasoning 死循环检测参数。
#[derive(Debug, Clone, PartialEq)]
pub struct ReasoningGuardConfig {
    pub enabled: bool,
    pub window_chars: i64,
    pub substr_len: i64,
    pub repeat_ratio: f64,
    pub check_every: i64,
    pub max_blocks: i64,
    pub max_chars: i64,
    pub max_guard_retries: i64,
    /// provider-neutral 错误码白名单；`configured_retry_code` 另有文本匹配路径。
    pub auto_retry_errors: Vec<String>,
}

impl Default for ReasoningGuardConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            window_chars: 2_000,
            substr_len: 32,
            repeat_ratio: 0.7,
            check_every: 50,
            max_blocks: 10_000,
            max_chars: 500_000,
            max_guard_retries: 2,
            auto_retry_errors: vec!["SERVICE_UNAVAILABLE".to_string()],
        }
    }
}

impl ReasoningGuardConfig {
    /// Python `__post_init__`：区间校验 + 错误码去重与上限。
    pub fn validate(self) -> Result<Self, ConfigError> {
        require_int_range("window_chars", self.window_chars, 64, 1_000_000)?;
        require_int_range("substr_len", self.substr_len, 8, 128)?;
        require_float_range("repeat_ratio", self.repeat_ratio, 0.0, 1.0)?;
        require_int_range("check_every", self.check_every, 1, 100_000)?;
        require_int_range("max_blocks", self.max_blocks, 100, 10_000_000)?;
        require_int_range("max_chars", self.max_chars, 1_000, 100_000_000)?;
        require_int_range("max_guard_retries", self.max_guard_retries, 0, 10)?;
        let mut normalized: Vec<String> = Vec::new();
        for raw_code in &self.auto_retry_errors {
            let code = raw_code.trim();
            if code.is_empty() {
                return Err(ConfigError::new(
                    "run_guard.guard.auto_retry_errors 的每项必须是非空字符串。",
                ));
            }
            if !normalized.iter().any(|item| item == code) {
                normalized.push(code.to_string());
            }
        }
        if normalized.len() > 32 {
            return Err(ConfigError::new(
                "run_guard.guard.auto_retry_errors 最多允许 32 个错误码。",
            ));
        }
        Ok(Self {
            auto_retry_errors: normalized,
            ..self
        })
    }
}

/// Todo 未完成和 reasoning-only 停止时的自动续跑参数。
#[derive(Debug, Clone, PartialEq)]
pub struct ContinueConfig {
    pub enabled: bool,
    pub max_auto_followups: i64,
}

impl Default for ContinueConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_auto_followups: 3,
        }
    }
}

/// 运行节奏功能总配置，对应 `config.toml` 的 `[run_guard]` 段。
#[derive(Debug, Clone, PartialEq)]
pub struct RunGuardConfig {
    pub enabled: bool,
    pub guard: ReasoningGuardConfig,
    pub continuation: ContinueConfig,
}

impl Default for RunGuardConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            guard: ReasoningGuardConfig::default(),
            continuation: ContinueConfig::default(),
        }
    }
}

impl RunGuardConfig {
    /// 与 Python 的 dataclass 默认值同义（总开关默认开启）。
    pub fn new() -> Self {
        Self::default()
    }

    /// 校验三段自身的取值。
    pub fn validate(self) -> Result<Self, ConfigError> {
        let guard = self.guard.validate()?;
        if self.continuation.max_auto_followups < 1 || self.continuation.max_auto_followups > 20 {
            return Err(ConfigError::new(
                "run_guard.max_auto_followups 必须是 1 到 20 之间的整数。",
            ));
        }
        Ok(Self { guard, ..self })
    }
}

/// 保留其他配置段，只写回完整的 `run_guard` 配置。
pub fn save_run_guard_config(
    env: &ConfigEnvironment,
    config: &RunGuardConfig,
    config_path: Option<&Path>,
) -> Result<PathBuf, ConfigError> {
    let mut data = load_config_data(env, config_path)?;
    let mut guard = Table::new();
    guard.insert("enabled".to_string(), Value::Boolean(config.guard.enabled));
    guard.insert(
        "window_chars".to_string(),
        Value::Integer(config.guard.window_chars),
    );
    guard.insert(
        "substr_len".to_string(),
        Value::Integer(config.guard.substr_len),
    );
    guard.insert(
        "repeat_ratio".to_string(),
        Value::Float(config.guard.repeat_ratio),
    );
    guard.insert(
        "check_every".to_string(),
        Value::Integer(config.guard.check_every),
    );
    guard.insert(
        "max_blocks".to_string(),
        Value::Integer(config.guard.max_blocks),
    );
    guard.insert(
        "max_chars".to_string(),
        Value::Integer(config.guard.max_chars),
    );
    guard.insert(
        "max_guard_retries".to_string(),
        Value::Integer(config.guard.max_guard_retries),
    );
    guard.insert(
        "auto_retry_errors".to_string(),
        Value::Array(
            config
                .guard
                .auto_retry_errors
                .iter()
                .cloned()
                .map(Value::String)
                .collect(),
        ),
    );
    let mut continuation = Table::new();
    continuation.insert(
        "enabled".to_string(),
        Value::Boolean(config.continuation.enabled),
    );
    continuation.insert(
        "max_auto_followups".to_string(),
        Value::Integer(config.continuation.max_auto_followups),
    );
    let mut section = Table::new();
    section.insert("enabled".to_string(), Value::Boolean(config.enabled));
    section.insert("guard".to_string(), Value::Table(guard));
    section.insert("continue".to_string(), Value::Table(continuation));
    data.insert("run_guard".to_string(), Value::Table(section));
    save_config_data(env, &data, config_path)
}

/// 读取并严格校验 `config.toml` 的 `run_guard` 段。
pub fn load_run_guard_config(
    env: &ConfigEnvironment,
    config_path: Option<&Path>,
) -> Result<RunGuardConfig, ConfigError> {
    let data = load_config_data(env, config_path)?;
    let section = get_section(&data, "run_guard")?;
    let guard_section = nested_section(&section, "guard")?;
    let continue_section = nested_section(&section, "continue")?;
    reject_unknown_fields(&section, &["enabled", "guard", "continue"], "run_guard")?;
    reject_unknown_fields(
        &guard_section,
        &[
            "enabled",
            "window_chars",
            "substr_len",
            "repeat_ratio",
            "check_every",
            "max_blocks",
            "max_chars",
            "max_guard_retries",
            "auto_retry_errors",
        ],
        "run_guard.guard",
    )?;
    reject_unknown_fields(
        &continue_section,
        &["enabled", "max_auto_followups"],
        "run_guard.continue",
    )?;

    let defaults = RunGuardConfig::new();
    let guard_defaults = ReasoningGuardConfig::default();
    let continue_defaults = ContinueConfig::default();
    let config = RunGuardConfig {
        enabled: bool_field(&section, "enabled", defaults.enabled, "run_guard.enabled")?,
        guard: ReasoningGuardConfig {
            enabled: bool_field(
                &guard_section,
                "enabled",
                guard_defaults.enabled,
                "run_guard.guard.enabled",
            )?,
            window_chars: guard_int(
                &guard_section,
                "window_chars",
                guard_defaults.window_chars,
                64,
                1_000_000,
            )?,
            substr_len: guard_int(
                &guard_section,
                "substr_len",
                guard_defaults.substr_len,
                8,
                128,
            )?,
            repeat_ratio: guard_float(&guard_section, "repeat_ratio", guard_defaults.repeat_ratio)?,
            check_every: guard_int(
                &guard_section,
                "check_every",
                guard_defaults.check_every,
                1,
                100_000,
            )?,
            max_blocks: guard_int(
                &guard_section,
                "max_blocks",
                guard_defaults.max_blocks,
                100,
                10_000_000,
            )?,
            max_chars: guard_int(
                &guard_section,
                "max_chars",
                guard_defaults.max_chars,
                1_000,
                100_000_000,
            )?,
            max_guard_retries: guard_int(
                &guard_section,
                "max_guard_retries",
                guard_defaults.max_guard_retries,
                0,
                10,
            )?,
            auto_retry_errors: match guard_section.get("auto_retry_errors") {
                None => guard_defaults.auto_retry_errors.clone(),
                Some(Value::Array(items)) => {
                    let mut codes: Vec<String> = Vec::new();
                    for item in items {
                        match item {
                            Value::String(text) => codes.push(text.clone()),
                            _ => {
                                return Err(ConfigError::new(
                                    "run_guard.guard.auto_retry_errors 的每项必须是非空字符串。",
                                ))
                            }
                        }
                    }
                    codes
                }
                Some(_) => {
                    return Err(ConfigError::new(
                        "run_guard.guard.auto_retry_errors 必须是字符串数组。",
                    ))
                }
            },
        },
        continuation: ContinueConfig {
            enabled: bool_field(
                &continue_section,
                "enabled",
                continue_defaults.enabled,
                "run_guard.continue.enabled",
            )?,
            max_auto_followups: guard_int(
                &continue_section,
                "max_auto_followups",
                continue_defaults.max_auto_followups,
                1,
                20,
            )?,
        },
    };
    config.validate()
}

fn nested_section(section: &Table, name: &str) -> Result<Table, ConfigError> {
    match section.get(name) {
        None => Ok(Table::new()),
        Some(Value::String(text)) if text.is_empty() => Ok(Table::new()),
        Some(Value::Table(table)) => Ok(table.clone()),
        Some(_) => Err(ConfigError::new(format!(
            "配置项 run_guard.{name} 必须是对象。"
        ))),
    }
}

fn reject_unknown_fields(
    section: &Table,
    allowed: &[&str],
    prefix: &str,
) -> Result<(), ConfigError> {
    let mut unknown: Vec<&str> = section
        .keys()
        .map(|key| key.as_str())
        .filter(|key| !allowed.contains(key))
        .collect();
    unknown.sort();
    if !unknown.is_empty() {
        return Err(ConfigError::new(format!(
            "{prefix} 包含未知配置项：{}",
            unknown.join(", ")
        )));
    }
    Ok(())
}

fn require_int_range(
    name: &str,
    value: i64,
    minimum: i64,
    maximum: i64,
) -> Result<(), ConfigError> {
    if value < minimum || value > maximum {
        return Err(ConfigError::new(format!(
            "run_guard.{name} 必须是 {minimum} 到 {maximum} 之间的整数。"
        )));
    }
    Ok(())
}

fn require_float_range(
    name: &str,
    value: f64,
    minimum: f64,
    maximum: f64,
) -> Result<(), ConfigError> {
    if value < minimum || value > maximum {
        // 区间用 Python `repr` 写法（`0.0` / `1.0`），与 Python 文案逐字一致。
        return Err(ConfigError::new(format!(
            "run_guard.guard.{name} 必须满足 {} <= value <= {}。",
            crate::toml::float_repr(minimum),
            crate::toml::float_repr(maximum)
        )));
    }
    Ok(())
}

fn bool_field(
    section: &Table,
    name: &str,
    default: bool,
    label: &str,
) -> Result<bool, ConfigError> {
    match section.get(name) {
        None => Ok(default),
        Some(Value::Boolean(flag)) => Ok(*flag),
        Some(_) => Err(ConfigError::new(format!("{label} 必须是布尔值。"))),
    }
}

/// Python 把整数校验的文案统一成 `run_guard.<name>`（guard 段不重复前缀），
/// 类型错误与越界走同一条文案。
fn guard_int(
    section: &Table,
    name: &str,
    default: i64,
    minimum: i64,
    maximum: i64,
) -> Result<i64, ConfigError> {
    match section.get(name) {
        None => Ok(default),
        Some(Value::Integer(value)) => Ok(*value),
        Some(_) => Err(ConfigError::new(format!(
            "run_guard.{name} 必须是 {minimum} 到 {maximum} 之间的整数。"
        ))),
    }
}

fn guard_float(section: &Table, name: &str, default: f64) -> Result<f64, ConfigError> {
    match section.get(name) {
        None => Ok(default),
        Some(Value::Integer(value)) => Ok(*value as f64),
        Some(Value::Float(value)) => Ok(*value),
        Some(_) => Err(ConfigError::new(format!(
            "run_guard.guard.{name} 必须是数字。"
        ))),
    }
}
