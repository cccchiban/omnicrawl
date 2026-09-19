//! 工具审批模式配置（对应 `omnicrawl/config/features/approval.py`）。
//!
//! 别名与解析顺序是 Python 侧的唯一实现；控制器域里同名函数应改为再导出本模块。

use std::path::{Path, PathBuf};

use crate::core::runtime::{get_section, load_config_data, save_config_data, ConfigEnvironment};
use crate::error::ConfigError;
use crate::toml::Value;

pub const APPROVAL_MODE_MANUAL: &str = "manual";
pub const APPROVAL_MODE_AUTO: &str = "auto";
pub const APPROVAL_MODE_REVIEW: &str = "review";

/// `sorted({manual, auto, review})` 的拼接结果。
const APPROVAL_MODE_ALLOWED: &str = "auto, manual, review";

const APPROVAL_MODE_ALIASES: [(&str, &str); 11] = [
    ("ask", APPROVAL_MODE_MANUAL),
    ("confirm", APPROVAL_MODE_MANUAL),
    ("manual", APPROVAL_MODE_MANUAL),
    ("off", APPROVAL_MODE_MANUAL),
    ("auto", APPROVAL_MODE_AUTO),
    ("auto_approve", APPROVAL_MODE_AUTO),
    ("approve", APPROVAL_MODE_AUTO),
    ("always", APPROVAL_MODE_AUTO),
    ("review", APPROVAL_MODE_REVIEW),
    ("auto_review", APPROVAL_MODE_REVIEW),
    ("reviewed", APPROVAL_MODE_REVIEW),
];

/// 把配置或命令里的审批模式规范化为内部枚举字符串。
pub fn normalize_approval_mode(value: &str) -> Result<&'static str, ConfigError> {
    let normalized = value.trim().to_lowercase().replace('-', "_");
    if let Some((_, mode)) = APPROVAL_MODE_ALIASES
        .iter()
        .find(|(alias, _)| *alias == normalized)
    {
        return Ok(mode);
    }
    Err(ConfigError::new(format!(
        "approval.mode 仅支持 {APPROVAL_MODE_ALLOWED}，当前值：{value}。"
    )))
}

/// 审批模式的中文显示名。
pub fn approval_mode_label(mode: &str) -> String {
    match mode {
        APPROVAL_MODE_MANUAL => "人工确认".to_string(),
        APPROVAL_MODE_AUTO => "完全自动批准".to_string(),
        APPROVAL_MODE_REVIEW => "自动审查".to_string(),
        other => other.to_string(),
    }
}

/// 从 `config.toml` 读取工具审批模式，默认自动审查。
pub fn load_approval_mode(
    env: &ConfigEnvironment,
    config_path: Option<&Path>,
) -> Result<String, ConfigError> {
    let data = load_config_data(env, config_path)?;
    let section = get_section(&data, "approval")?;
    if let Some(value) = section.get("mode") {
        let text = match value {
            Value::String(text) => text.clone(),
            _ => return Err(ConfigError::new("配置项 approval.mode 必须是字符串。")),
        };
        return Ok(normalize_approval_mode(&text)?.to_string());
    }
    if matches!(section.get("auto_review"), Some(Value::Boolean(true))) {
        return Ok(APPROVAL_MODE_REVIEW.to_string());
    }
    if matches!(section.get("auto_approve"), Some(Value::Boolean(true))) {
        return Ok(APPROVAL_MODE_AUTO.to_string());
    }
    Ok(APPROVAL_MODE_REVIEW.to_string())
}

/// 把工具审批模式写回 `config.toml`，保留已有配置项。
pub fn save_approval_mode(
    env: &ConfigEnvironment,
    mode: &str,
    config_path: Option<&Path>,
) -> Result<PathBuf, ConfigError> {
    let normalized = normalize_approval_mode(mode)?;
    let mut data = load_config_data(env, config_path)?;
    let mut section = get_section(&data, "approval")?;
    section.insert("mode".to_string(), Value::String(normalized.to_string()));
    data.insert("approval".to_string(), Value::Table(section));
    save_config_data(env, &data, config_path)
}

/// 自动审查使用的独立模型；空串表示沿用主对话模型。
pub fn load_approval_review_model(
    env: &ConfigEnvironment,
    config_path: Option<&Path>,
) -> Result<String, ConfigError> {
    let data = load_config_data(env, config_path)?;
    let section = get_section(&data, "approval")?;
    match section.get("review_model") {
        None => Ok(String::new()),
        Some(Value::String(text)) => Ok(text.trim().to_string()),
        Some(_) => Err(ConfigError::new(
            "配置项 approval.review_model 必须是字符串。",
        )),
    }
}
