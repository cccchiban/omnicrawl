//! 交互式设置使用的通用功能开关读写（对应 `omnicrawl/config/core/settings.py`）。
//!
//! 这些函数都遵循同一条约定：只改目标键，其余段与键原样保留后原子写回。

use std::path::{Path, PathBuf};

use crate::core::runtime::{
    get_section, load_config_data, resolve_subagents_path, resolve_subagents_write_path,
    save_config_data, ConfigEnvironment,
};
use crate::error::ConfigError;
use crate::features::subagents::{validate_subagent_advanced_setting, AdvancedValue};
use crate::models::llm_multi::is_multi_model_section;
use crate::models::model_store::{load_model_store, save_model_store, ModelStore};
use crate::toml::{Table, Value};

/// MCP 单个服务端的可写回视图。
#[derive(Debug, Clone, PartialEq)]
pub struct McpServerData {
    pub enabled: bool,
    pub transport: String,
    pub command: String,
    pub args: Vec<String>,
    pub url: String,
    pub env: Table,
    pub headers: Table,
    pub timeout_seconds: i64,
    pub risk_level: String,
}

/// MCP 审批策略。
#[derive(Debug, Clone, PartialEq)]
pub struct McpPolicyData {
    pub require_confirmation_for_write: bool,
    pub require_confirmation_for_command: bool,
    pub allow_external_network_tools: bool,
    pub audit_log_enabled: bool,
}

/// MCP 配置的可写回视图；服务端顺序即写回顺序。
#[derive(Debug, Clone, PartialEq)]
pub struct McpConfigData {
    pub enabled: bool,
    pub default_timeout_seconds: i64,
    pub servers: Vec<(String, McpServerData)>,
    pub policy: McpPolicyData,
}

/// 保留其他配置段，只更新完整的 MCP 配置并原子写回。
pub fn save_mcp_config(
    env: &ConfigEnvironment,
    config: &McpConfigData,
    config_path: Option<&Path>,
) -> Result<PathBuf, ConfigError> {
    let mut data = load_config_data(env, config_path)?;
    data.insert(
        "mcp".to_string(),
        Value::Table(serialize_mcp_config(config)),
    );
    save_config_data(env, &data, config_path)
}

fn serialize_mcp_config(config: &McpConfigData) -> Table {
    let mut servers = Table::new();
    for (name, server) in &config.servers {
        let mut item = Table::new();
        item.insert("enabled".to_string(), Value::Boolean(server.enabled));
        item.insert(
            "transport".to_string(),
            Value::String(server.transport.clone()),
        );
        item.insert("command".to_string(), Value::String(server.command.clone()));
        item.insert(
            "args".to_string(),
            Value::Array(
                server
                    .args
                    .iter()
                    .map(|item| Value::String(item.clone()))
                    .collect(),
            ),
        );
        item.insert("url".to_string(), Value::String(server.url.clone()));
        item.insert("env".to_string(), Value::Table(server.env.clone()));
        item.insert("headers".to_string(), Value::Table(server.headers.clone()));
        item.insert(
            "timeout_seconds".to_string(),
            Value::Integer(server.timeout_seconds),
        );
        item.insert(
            "risk_level".to_string(),
            Value::String(server.risk_level.clone()),
        );
        servers.insert(name.clone(), Value::Table(item));
    }
    let mut policy = Table::new();
    policy.insert(
        "require_confirmation_for_write".to_string(),
        Value::Boolean(config.policy.require_confirmation_for_write),
    );
    policy.insert(
        "require_confirmation_for_command".to_string(),
        Value::Boolean(config.policy.require_confirmation_for_command),
    );
    policy.insert(
        "allow_external_network_tools".to_string(),
        Value::Boolean(config.policy.allow_external_network_tools),
    );
    policy.insert(
        "audit_log_enabled".to_string(),
        Value::Boolean(config.policy.audit_log_enabled),
    );
    let mut out = Table::new();
    out.insert("enabled".to_string(), Value::Boolean(config.enabled));
    out.insert(
        "default_timeout_seconds".to_string(),
        Value::Integer(config.default_timeout_seconds),
    );
    out.insert("servers".to_string(), Value::Table(servers));
    out.insert("policy".to_string(), Value::Table(policy));
    out
}

/// 读取 `<section>.enabled`，缺省时保持既有默认行为。
///
/// `subagents` 段已迁到独立文件：该段的开关只从子代理设置文件读取。
pub fn load_feature_enabled(
    env: &ConfigEnvironment,
    section_name: &str,
    default: bool,
    config_path: Option<&Path>,
    subagents_path: Option<&Path>,
) -> Result<bool, ConfigError> {
    let data = if section_name == "subagents" {
        let target = resolve_subagents_path(env, subagents_path)?;
        load_config_data(env, Some(&target))?
    } else {
        load_config_data(env, config_path)?
    };
    let section = get_section(&data, section_name)?;
    match section.get("enabled") {
        None => Ok(default),
        Some(Value::Boolean(flag)) => Ok(*flag),
        Some(_) => Err(ConfigError::new(format!(
            "配置项 {section_name}.enabled 必须是布尔值。"
        ))),
    }
}

/// 持久化当前模型的上下文窗口，单位由调用方换算成 Token。
pub fn save_context_window_tokens(
    env: &ConfigEnvironment,
    tokens: i64,
    model_source: &str,
    catalog_key: &str,
    config_path: Option<&Path>,
    models_path: Option<&Path>,
) -> Result<PathBuf, ConfigError> {
    if tokens <= 0 {
        return Err(ConfigError::new("上下文长度必须是正整数 Token。"));
    }
    if model_source == "custom" {
        let key = catalog_key.trim();
        if key.is_empty() {
            return Err(ConfigError::new(
                "当前自定义模型缺少 catalog key，无法保存上下文长度。",
            ));
        }
        let store = load_model_store(env, models_path)?;
        let current = store
            .by_key(key)
            .ok_or_else(|| ConfigError::new(format!("models.toml 中不存在当前模型：{key}。")))?;
        let mut updated = current.clone();
        updated.context_window_tokens = tokens;
        updated.capabilities.context_window_tokens = tokens;
        let models = store
            .models
            .iter()
            .map(|item| {
                if item.key == key {
                    updated.clone()
                } else {
                    item.clone()
                }
            })
            .collect();
        let next = ModelStore {
            version: store.version,
            models,
            path: store.path.clone(),
        };
        return save_model_store(env, &next, models_path);
    }

    let mut data = load_config_data(env, config_path)?;
    let mut section = get_section(&data, "llm")?;
    if is_multi_model_section(&section) {
        let mut defaults = match section.get("defaults") {
            Some(Value::Table(table)) => table.clone(),
            _ => Table::new(),
        };
        defaults.insert("context_window_tokens".to_string(), Value::Integer(tokens));
        section.insert("defaults".to_string(), Value::Table(defaults));
    } else {
        section.insert("context_window_tokens".to_string(), Value::Integer(tokens));
    }
    data.insert("llm".to_string(), Value::Table(section));
    save_config_data(env, &data, config_path)
}

/// 按当前上下文窗口的百分比换算触发阈值并写回。
pub fn save_context_compaction_trigger_percent(
    env: &ConfigEnvironment,
    percent: i64,
    context_window_tokens: i64,
    config_path: Option<&Path>,
) -> Result<PathBuf, ConfigError> {
    if percent <= 0 {
        return Err(ConfigError::new("上下文压缩阈值百分比必须是正整数。"));
    }
    if context_window_tokens <= 0 {
        return Err(ConfigError::new("上下文长度必须是正整数 Token。"));
    }
    let tokens = std::cmp::max(1, context_window_tokens * percent / 100);
    let mut data = load_config_data(env, config_path)?;
    let mut section = get_section(&data, "context_compaction")?;
    section.insert("trigger_context_tokens".to_string(), Value::Integer(tokens));
    section.insert(
        "trigger_context_percent".to_string(),
        Value::Integer(percent),
    );
    data.insert("context_compaction".to_string(), Value::Table(section));
    save_config_data(env, &data, config_path)
}

/// 保留 `subagents` 其他配置，只更新面板允许的资源参数。
pub fn save_subagent_setting(
    env: &ConfigEnvironment,
    name: &str,
    value: &Value,
    subagents_path: Option<&Path>,
) -> Result<PathBuf, ConfigError> {
    let normalized = validate_subagent_advanced_setting(name, value)?;
    let target = resolve_subagents_write_path(env, subagents_path)?;
    let mut data = load_config_data(env, Some(&target))?;
    let mut section = get_section(&data, "subagents")?;
    section.insert(name.to_string(), advanced_value(normalized));
    data.insert("subagents".to_string(), Value::Table(section));
    save_config_data(env, &data, Some(&target))
}

fn advanced_value(value: AdvancedValue) -> Value {
    match value {
        AdvancedValue::Int(number) => Value::Integer(number),
        AdvancedValue::Number(number) => Value::Float(number),
    }
}

/// 保留功能段其余字段，只更新 `enabled` 并原子写回。
pub fn save_feature_enabled(
    env: &ConfigEnvironment,
    section_name: &str,
    enabled: bool,
    config_path: Option<&Path>,
    subagents_path: Option<&Path>,
) -> Result<PathBuf, ConfigError> {
    if section_name == "subagents" {
        let target = resolve_subagents_write_path(env, subagents_path)?;
        let mut data = load_config_data(env, Some(&target))?;
        let mut section = get_section(&data, "subagents")?;
        section.insert("enabled".to_string(), Value::Boolean(enabled));
        data.insert("subagents".to_string(), Value::Table(section));
        return save_config_data(env, &data, Some(&target));
    }
    let mut data = load_config_data(env, config_path)?;
    let mut section = get_section(&data, section_name)?;
    section.insert("enabled".to_string(), Value::Boolean(enabled));
    data.insert(section_name.to_string(), Value::Table(section));
    save_config_data(env, &data, config_path)
}

/// 读取 `ui.show_thinking`，缺省时默认开启。
pub fn load_show_thinking(
    env: &ConfigEnvironment,
    config_path: Option<&Path>,
) -> Result<bool, ConfigError> {
    let data = load_config_data(env, config_path)?;
    let section = get_section(&data, "ui")?;
    match section.get("show_thinking") {
        None => Ok(true),
        Some(Value::Boolean(flag)) => Ok(*flag),
        Some(_) => Err(ConfigError::new("配置项 ui.show_thinking 必须是布尔值。")),
    }
}

/// 保留 `ui` 段其余字段，只更新 `show_thinking` 并原子写回。
pub fn save_show_thinking(
    env: &ConfigEnvironment,
    enabled: bool,
    config_path: Option<&Path>,
) -> Result<PathBuf, ConfigError> {
    let mut data = load_config_data(env, config_path)?;
    let mut section = get_section(&data, "ui")?;
    section.insert("show_thinking".to_string(), Value::Boolean(enabled));
    data.insert("ui".to_string(), Value::Table(section));
    save_config_data(env, &data, config_path)
}
