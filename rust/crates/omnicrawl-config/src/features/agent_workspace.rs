//! 主 Agent 隔离工作区配置（对应 `omnicrawl/config/features/agent_workspace.py`）。
//!
//! `[agent_workspace]` 段控制主 Agent 是否在 git worktree 或本地目录副本中运行，
//! 以及退出时的回写与清理策略。

use std::path::{Path, PathBuf};

use crate::core::runtime::{load_config_data, save_config_data, ConfigEnvironment};
use crate::error::ConfigError;
use crate::toml::{Table, Value};

const CONFIG_SECTION: &str = "agent_workspace";

/// 主 Agent 隔离工作区配置。
#[derive(Debug, Clone, PartialEq)]
pub struct AgentWorkspaceConfig {
    pub enabled: bool,
    /// worktree | local
    pub mode: String,
    pub base_branch: String,
    pub base_ref: String,
    pub detached: bool,
    pub apply_on_exit: bool,
    /// auto | keep | never
    pub cleanup_on_exit: String,
    pub sync_uncommitted: bool,
    pub copy_dirs: Vec<String>,
    pub env_scripts: Vec<String>,
}

impl Default for AgentWorkspaceConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            mode: "worktree".to_string(),
            base_branch: String::new(),
            base_ref: "HEAD".to_string(),
            detached: true,
            apply_on_exit: true,
            cleanup_on_exit: "auto".to_string(),
            sync_uncommitted: true,
            copy_dirs: Vec::new(),
            env_scripts: Vec::new(),
        }
    }
}

impl AgentWorkspaceConfig {
    /// Python `__post_init__`：枚举取值与列表元素类型校验。
    pub fn normalize(self) -> Result<Self, ConfigError> {
        if self.mode != "worktree" && self.mode != "local" {
            return Err(ConfigError::new(format!(
                "agent_workspace.mode 必须是 worktree 或 local，当前值：{}",
                self.mode
            )));
        }
        if self.cleanup_on_exit != "auto"
            && self.cleanup_on_exit != "keep"
            && self.cleanup_on_exit != "never"
        {
            return Err(ConfigError::new(format!(
                "agent_workspace.cleanup_on_exit 必须是 auto/keep/never，当前值：{}",
                self.cleanup_on_exit
            )));
        }
        Ok(self)
    }
}

/// 读取 `[agent_workspace]` 段，缺省使用默认值（默认开启）。
pub fn load_agent_workspace_config(
    env: &ConfigEnvironment,
    config_path: Option<&Path>,
) -> Result<AgentWorkspaceConfig, ConfigError> {
    let data = load_config_data(env, config_path)?;
    let section = match data.get(CONFIG_SECTION) {
        None => return Ok(AgentWorkspaceConfig::default()),
        Some(Value::Table(section)) => section.clone(),
        Some(_) => {
            return Err(ConfigError::new(format!(
                "配置项 {CONFIG_SECTION} 必须是表（table）。"
            )))
        }
    };
    AgentWorkspaceConfig {
        enabled: bool_field(&section, "enabled", true)?,
        mode: string_field(&section, "mode", "worktree")?,
        base_branch: string_field(&section, "base_branch", "")?,
        base_ref: string_field(&section, "base_ref", "HEAD")?,
        detached: bool_field(&section, "detached", true)?,
        apply_on_exit: bool_field(&section, "apply_on_exit", true)?,
        cleanup_on_exit: string_field(&section, "cleanup_on_exit", "auto")?,
        sync_uncommitted: bool_field(&section, "sync_uncommitted", true)?,
        copy_dirs: string_list_field(&section, "copy_dirs")?,
        env_scripts: string_list_field(&section, "env_scripts")?,
    }
    .normalize()
}

/// 把隔离工作区配置写回 `config.toml`（保留其他段）。
pub fn save_agent_workspace_config(
    env: &ConfigEnvironment,
    configuration: &AgentWorkspaceConfig,
    config_path: Option<&Path>,
) -> Result<PathBuf, ConfigError> {
    let mut data = load_config_data(env, config_path)?;
    let mut section = Table::new();
    section.insert("enabled".to_string(), Value::Boolean(configuration.enabled));
    section.insert(
        "mode".to_string(),
        Value::String(configuration.mode.clone()),
    );
    section.insert(
        "base_branch".to_string(),
        Value::String(configuration.base_branch.clone()),
    );
    section.insert(
        "base_ref".to_string(),
        Value::String(configuration.base_ref.clone()),
    );
    section.insert(
        "detached".to_string(),
        Value::Boolean(configuration.detached),
    );
    section.insert(
        "apply_on_exit".to_string(),
        Value::Boolean(configuration.apply_on_exit),
    );
    section.insert(
        "cleanup_on_exit".to_string(),
        Value::String(configuration.cleanup_on_exit.clone()),
    );
    section.insert(
        "sync_uncommitted".to_string(),
        Value::Boolean(configuration.sync_uncommitted),
    );
    section.insert(
        "copy_dirs".to_string(),
        Value::Array(
            configuration
                .copy_dirs
                .iter()
                .map(|item| Value::String(item.clone()))
                .collect(),
        ),
    );
    section.insert(
        "env_scripts".to_string(),
        Value::Array(
            configuration
                .env_scripts
                .iter()
                .map(|item| Value::String(item.clone()))
                .collect(),
        ),
    );
    data.insert(CONFIG_SECTION.to_string(), Value::Table(section));
    save_config_data(env, &data, config_path)
}

fn bool_field(section: &Table, name: &str, default: bool) -> Result<bool, ConfigError> {
    match section.get(name) {
        None => Ok(default),
        Some(Value::Boolean(flag)) => Ok(*flag),
        Some(_) => Err(ConfigError::new(format!(
            "配置项 {CONFIG_SECTION}.{name} 必须是布尔值。"
        ))),
    }
}

fn string_field(section: &Table, name: &str, default: &str) -> Result<String, ConfigError> {
    match section.get(name) {
        None => Ok(default.to_string()),
        Some(Value::String(text)) => Ok(text.clone()),
        Some(_) => Err(ConfigError::new(format!(
            "配置项 {CONFIG_SECTION}.{name} 必须是字符串。"
        ))),
    }
}

fn string_list_field(section: &Table, name: &str) -> Result<Vec<String>, ConfigError> {
    match section.get(name) {
        None => Ok(Vec::new()),
        Some(Value::Array(items)) => {
            let mut list = Vec::with_capacity(items.len());
            for item in items {
                match item {
                    Value::String(text) => list.push(text.clone()),
                    _ => {
                        return Err(ConfigError::new(format!(
                            "配置项 {CONFIG_SECTION}.{name} 必须是字符串数组。"
                        )))
                    }
                }
            }
            Ok(list)
        }
        Some(_) => Err(ConfigError::new(format!(
            "配置项 {CONFIG_SECTION}.{name} 必须是字符串数组。"
        ))),
    }
}
