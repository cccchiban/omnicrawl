//! 工作区运行配置（对应 `omnicrawl/config/core/workspace.py`）。
//!
//! 把当前工作区根目录持久化到 `[workspace] root`，供其它入口（TUI、API、连接器）
//! 重读，实现跨进程工作区同步。

use std::path::{Path, PathBuf};

use crate::core::runtime::{
    absolute_path, expand_user, get_section, load_config_data, save_config_data, ConfigEnvironment,
};
use crate::error::ConfigError;
use crate::toml::Value;

pub const WORKSPACE_SECTION: &str = "workspace";
pub const WORKSPACE_ROOT_KEY: &str = "root";

/// 读取 `[workspace] root`；未配置或值为空时返回 `None`。
pub fn load_workspace_root(
    env: &ConfigEnvironment,
    config_path: Option<&Path>,
) -> Result<Option<String>, ConfigError> {
    let data = load_config_data(env, config_path)?;
    let section = get_section(&data, WORKSPACE_SECTION)?;
    match section.get(WORKSPACE_ROOT_KEY) {
        None => Ok(None),
        Some(Value::String(text)) if !text.trim().is_empty() => Ok(Some(text.trim().to_string())),
        Some(_) => Ok(None),
    }
}

/// 把工作区根目录写回 `[workspace] root`，保留已有配置项。
///
/// 路径按 `expanduser` + `abspath` 归一化；目录不存在也允许写入，可切换性由调用方校验。
pub fn save_workspace_root(
    env: &ConfigEnvironment,
    path: &Path,
    config_path: Option<&Path>,
) -> Result<PathBuf, ConfigError> {
    let root = absolute_path(&expand_user(env, &path.to_string_lossy()));
    let mut data = load_config_data(env, config_path)?;
    let mut section = get_section(&data, WORKSPACE_SECTION)?;
    section.insert(
        WORKSPACE_ROOT_KEY.to_string(),
        Value::String(root.to_string_lossy().to_string()),
    );
    data.insert(WORKSPACE_SECTION.to_string(), Value::Table(section));
    save_config_data(env, &data, config_path)
}
