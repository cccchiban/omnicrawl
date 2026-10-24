//! Computer Use 配置（`config.toml` 的 `[computer_use]` 段）。
//!
//! 默认关闭。本段与 `[subagents]` 完全独立：`subagents.enabled` 的语义是「允许主代理
//! 派发子任务」，`computer_use.enabled` 的语义是「允许主代理操作桌面」，关掉前者不该
//! 连坐关掉后者。本功能由专用子代理执行，那个子代理不进 `subagent` 的角色枚举。

use std::path::{Path, PathBuf};

use crate::core::runtime::{get_section, load_config_data, save_config_data, ConfigEnvironment};
use crate::error::ConfigError;
use crate::toml::{Table, Value};
use crate::value::python_str;

/// 单次任务的默认时间预算（秒）：按时间收敛，不按步数收敛。
pub const DEFAULT_TIMEOUT_SECONDS: f64 = 600.0;
/// 时间预算的取值范围。
pub const MIN_TIMEOUT_SECONDS: f64 = 10.0;
pub const MAX_TIMEOUT_SECONDS: f64 = 3_600.0;
/// 截图长边的默认上限：再大只会放大 token，UI 文字已足够辨认。
pub const DEFAULT_SCREENSHOT_MAX_DIMENSION: i64 = 1_024;
pub const MIN_SCREENSHOT_MAX_DIMENSION: i64 = 320;
pub const MAX_SCREENSHOT_MAX_DIMENSION: i64 = 4_096;
/// 思考强度的默认值：桌面任务靠看画面、点控件，深推理换不回转得动的操作。
pub const DEFAULT_THINKING_EFFORT: &str = "low";
/// 思考强度候选（桌面操作里「关掉思考」没有意义，因此不含 none/disabled）。
pub const THINKING_EFFORT_OPTIONS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];

/// Computer Use 开关与运行期参数。
#[derive(Debug, Clone, PartialEq)]
pub struct ComputerUseConfig {
    pub enabled: bool,
    /// 专用子代理的模型；空串或 `inherit` 表示继承主模型。
    pub model: String,
    /// 单次 `computer_use` 调用的时间预算（秒）。
    pub timeout_seconds: f64,
    /// 截图的 `max_dimension`：同时决定换算比例的上限。
    pub screenshot_max_dimension: i64,
    /// 专用子代理的思考强度。
    pub thinking_effort: String,
}

impl Default for ComputerUseConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            model: String::new(),
            timeout_seconds: DEFAULT_TIMEOUT_SECONDS,
            screenshot_max_dimension: DEFAULT_SCREENSHOT_MAX_DIMENSION,
            thinking_effort: DEFAULT_THINKING_EFFORT.to_string(),
        }
    }
}

impl ComputerUseConfig {
    /// 归一化：模型名去空白，`inherit` 收敛为空串（表示不覆盖）。
    pub fn normalize(mut self) -> Result<Self, ConfigError> {
        self.model = self.model.trim().to_string();
        if self.model.eq_ignore_ascii_case("inherit") {
            self.model.clear();
        }
        if !(MIN_TIMEOUT_SECONDS..=MAX_TIMEOUT_SECONDS).contains(&self.timeout_seconds) {
            return Err(ConfigError::new(format!(
                "computer_use.timeout_seconds 必须在 {MIN_TIMEOUT_SECONDS} 到 {MAX_TIMEOUT_SECONDS} 秒之间，当前值：{}。",
                self.timeout_seconds
            )));
        }
        if !(MIN_SCREENSHOT_MAX_DIMENSION..=MAX_SCREENSHOT_MAX_DIMENSION)
            .contains(&self.screenshot_max_dimension)
        {
            return Err(ConfigError::new(format!(
                "computer_use.screenshot_max_dimension 必须在 {MIN_SCREENSHOT_MAX_DIMENSION} 到 {MAX_SCREENSHOT_MAX_DIMENSION} 之间，当前值：{}。",
                self.screenshot_max_dimension
            )));
        }
        let effort = self.thinking_effort.trim().to_lowercase();
        self.thinking_effort = if effort.is_empty() {
            DEFAULT_THINKING_EFFORT.to_string()
        } else if THINKING_EFFORT_OPTIONS.contains(&effort.as_str()) {
            effort
        } else {
            return Err(ConfigError::new(format!(
                "computer_use.thinking_effort 仅支持 {}，当前值：{}。",
                THINKING_EFFORT_OPTIONS.join(", "),
                self.thinking_effort
            )));
        };
        Ok(self)
    }

    /// 是否真正启用：只看本段开关，与 `[subagents] enabled` 无关。
    pub fn active(&self) -> bool {
        self.enabled
    }

    /// 实际生效的模型：空串表示继承主模型。
    pub fn effective_model(&self) -> Option<&str> {
        if self.model.trim().is_empty() {
            None
        } else {
            Some(self.model.as_str())
        }
    }
}

/// 读取 `[computer_use]` 段；缺失时返回默认关闭配置。
pub fn load_computer_use_config(
    env: &ConfigEnvironment,
    config_path: Option<&Path>,
) -> Result<ComputerUseConfig, ConfigError> {
    let data = load_config_data(env, config_path)?;
    let section = get_section(&data, "computer_use")?;
    parse_section(&section)
}

fn parse_section(section: &Table) -> Result<ComputerUseConfig, ConfigError> {
    let timeout_seconds = match section.get("timeout_seconds") {
        None => DEFAULT_TIMEOUT_SECONDS,
        Some(Value::Float(value)) => *value,
        Some(Value::Integer(value)) => *value as f64,
        Some(_) => {
            return Err(ConfigError::new(
                "computer_use.timeout_seconds 必须是数字。",
            ))
        }
    };
    let screenshot_max_dimension = match section.get("screenshot_max_dimension") {
        None => DEFAULT_SCREENSHOT_MAX_DIMENSION,
        Some(Value::Integer(value)) => *value,
        Some(_) => {
            return Err(ConfigError::new(
                "computer_use.screenshot_max_dimension 必须是整数。",
            ))
        }
    };
    ComputerUseConfig {
        enabled: match section.get("enabled") {
            None => false,
            Some(Value::Boolean(flag)) => *flag,
            Some(_) => return Err(ConfigError::new("computer_use.enabled 必须是布尔值。")),
        },
        model: python_str(section.get("model")).trim().to_string(),
        timeout_seconds,
        screenshot_max_dimension,
        thinking_effort: match section.get("thinking_effort") {
            None => DEFAULT_THINKING_EFFORT.to_string(),
            Some(Value::String(value)) => value.clone(),
            Some(_) => {
                return Err(ConfigError::new(
                    "computer_use.thinking_effort 必须是字符串。",
                ))
            }
        },
    }
    .normalize()
}

/// 把配置写回 `config.toml` 的 `[computer_use]` 段（保留其他段）。
pub fn save_computer_use_config(
    env: &ConfigEnvironment,
    config: &ComputerUseConfig,
    config_path: Option<&Path>,
) -> Result<PathBuf, ConfigError> {
    let config = config.clone().normalize()?;
    let mut data = load_config_data(env, config_path)?;
    let mut section = Table::new();
    section.insert("enabled".to_string(), Value::Boolean(config.enabled));
    section.insert("model".to_string(), Value::String(config.model.clone()));
    section.insert(
        "timeout_seconds".to_string(),
        Value::Float(config.timeout_seconds),
    );
    section.insert(
        "screenshot_max_dimension".to_string(),
        Value::Integer(config.screenshot_max_dimension),
    );
    section.insert(
        "thinking_effort".to_string(),
        Value::String(config.thinking_effort.clone()),
    );
    data.insert("computer_use".to_string(), Value::Table(section));
    save_config_data(env, &data, config_path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn section(pairs: &[(&str, Value)]) -> Table {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_string(), value.clone()))
            .collect()
    }

    #[test]
    fn defaults_are_disabled_with_bounded_budget() {
        let config = parse_section(&Table::new()).expect("空段应当可解析");
        assert!(!config.enabled);
        assert_eq!(config.effective_model(), None);
        assert!(!config.active());
        assert_eq!(config.timeout_seconds, DEFAULT_TIMEOUT_SECONDS);
        assert_eq!(
            config.screenshot_max_dimension,
            DEFAULT_SCREENSHOT_MAX_DIMENSION
        );
        assert_eq!(config.thinking_effort, DEFAULT_THINKING_EFFORT);
    }

    #[test]
    fn thinking_effort_rejects_values_outside_the_candidates() {
        assert_eq!(
            parse_section(&section(&[(
                "thinking_effort",
                Value::String(" HIGH ".into())
            )]))
            .expect("大小写与空白应当被归一")
            .thinking_effort,
            "high"
        );
        assert_eq!(
            parse_section(&section(&[(
                "thinking_effort",
                Value::String(String::new())
            )]))
            .expect("空串应当回落到默认值")
            .thinking_effort,
            DEFAULT_THINKING_EFFORT
        );
        let error = parse_section(&section(&[(
            "thinking_effort",
            Value::String("none".into()),
        )]))
        .expect_err("不支持的档位应当被拒绝");
        assert!(error.message().contains("thinking_effort"), "{error}");
    }

    #[test]
    fn inherit_is_treated_as_no_override() {
        let config = parse_section(&section(&[("model", Value::String(" inherit ".into()))]))
            .expect("inherit 应当被接受");
        assert_eq!(config.model, "");
        assert_eq!(config.effective_model(), None);

        let config = parse_section(&section(&[("model", Value::String("gpt-4o".into()))]))
            .expect("具体模型名应当被保留");
        assert_eq!(config.effective_model(), Some("gpt-4o"));
    }

    #[test]
    fn out_of_range_budgets_are_rejected() {
        let error = parse_section(&section(&[("timeout_seconds", Value::Float(0.5))]))
            .expect_err("过小的时间预算应当被拒绝");
        assert!(error.message().contains("timeout_seconds"), "{error}");

        let error = parse_section(&section(&[("screenshot_max_dimension", Value::Integer(1))]))
            .expect_err("过小的截图上限应当被拒绝");
        assert!(
            error.message().contains("screenshot_max_dimension"),
            "{error}"
        );
    }

    #[test]
    fn enabled_is_independent_from_subagents() {
        let config = parse_section(&section(&[("enabled", Value::Boolean(true))]))
            .expect("布尔开关应当被接受");
        assert!(config.active());
    }
}
