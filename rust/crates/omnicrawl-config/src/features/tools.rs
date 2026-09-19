//! 内置工具开关配置（对应 `omnicrawl/config/features/tools.py`）。
//!
//! 默认除 `powershell` 关闭外全部启用；开关只影响 Agent 工具表的注册。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::core::runtime::{get_section, load_config_data, save_config_data, ConfigEnvironment};
use crate::error::ConfigError;
use crate::toml::Value;

/// 可开关的内置工具（含条件注册工具；未注册条件下配置开关不会报错）。
pub const TOOL_SWITCH_KEYS: [&str; 26] = [
    "list",
    "find",
    "read",
    "read_image",
    "grep",
    "web_search",
    "fetcher",
    "image_gen",
    "tts_synthesize",
    "Edit_file",
    "write_file",
    "bash",
    "powershell",
    "monitor",
    "recall_session_evidence",
    "windows_window",
    "windows_control",
    "windows_input",
    "windows_clipboard",
    "windows_screenshot",
    "subagent",
    "update_todos",
    "memory_search",
    "memory_read",
    "memory_expand_related",
    "memory_write",
];

/// 开关名 → 界面文案。
pub const TOOL_SWITCH_LABELS: [(&str, &str); 26] = [
    ("list", "列出目录内容"),
    ("find", "按名称或路径查找文件"),
    ("read", "读取文件内容"),
    ("read_image", "读取图片"),
    ("grep", "在文件中搜索文本（grep）"),
    ("web_search", "web_search（Bing/DuckDuckGo/雅虎）"),
    ("fetcher", "fetcher（浏览器指纹）"),
    ("image_gen", "图像生成（Image API）"),
    ("tts_synthesize", "TTS 语音合成（MOSS-TTS-Nano）"),
    ("Edit_file", "替换文件中的文本"),
    ("write_file", "写入文件"),
    ("bash", "执行 Bash 命令"),
    ("powershell", "执行 PowerShell 命令"),
    ("monitor", "管理后台命令"),
    ("recall_session_evidence", "会话证据恢复"),
    ("windows_window", "操作 Windows 窗口"),
    ("windows_control", "操作 Windows UI 控件"),
    ("windows_input", "模拟 Windows 鼠标键盘"),
    ("windows_clipboard", "操作 Windows 剪贴板"),
    ("windows_screenshot", "截取 Windows 桌面"),
    ("subagent", "分发受限子任务"),
    ("update_todos", "维护自动执行清单"),
    ("memory_search", "搜索长期记忆（scope 选作用域）"),
    ("memory_read", "读取长期记忆"),
    ("memory_expand_related", "展开相关长期记忆"),
    ("memory_write", "写入长期记忆（scope 选作用域）"),
];

/// 默认开关：除 `powershell` 外全部启用。
pub fn default_tool_switches() -> BTreeMap<&'static str, bool> {
    let mut switches: BTreeMap<&'static str, bool> = TOOL_SWITCH_KEYS
        .into_iter()
        .map(|name| (name, true))
        .collect();
    switches.insert("powershell", false);
    switches
}

/// 旧版按作用域拆分的记忆工具开关名 → 统一开关名。
pub fn legacy_tool_switch_alias(name: &str) -> Option<String> {
    for prefix in ["project_", "session_", "user_"] {
        let Some(rest) = name.strip_prefix(prefix) else {
            continue;
        };
        for action in ["search", "read", "expand_related", "write"] {
            if rest == format!("memory_{action}") {
                return Some(format!("memory_{action}"));
            }
        }
    }
    None
}

/// 校验工具名是否为可开关的内置工具，返回规范化后的开关名。
pub fn validate_tool_switch_name(name: &str) -> Result<String, ConfigError> {
    let trimmed = name.trim();
    let normalized = legacy_tool_switch_alias(trimmed).unwrap_or_else(|| trimmed.to_string());
    if !TOOL_SWITCH_KEYS.contains(&normalized.as_str()) {
        return Err(ConfigError::new(format!(
            "tools 配置不支持工具：{normalized}。可用：{}。",
            TOOL_SWITCH_KEYS.join(", ")
        )));
    }
    Ok(normalized)
}

/// 读取 `tools` 段并与默认开关合并，返回完整工具开关表。
pub fn load_tool_switches(
    env: &ConfigEnvironment,
    config_path: Option<&Path>,
) -> Result<BTreeMap<String, bool>, ConfigError> {
    let data = load_config_data(env, config_path)?;
    let section = get_section(&data, "tools")?;
    let mut switches: BTreeMap<String, bool> = default_tool_switches()
        .into_iter()
        .map(|(name, enabled)| (name.to_string(), enabled))
        .collect();
    for (raw_name, value) in &section {
        let name = validate_tool_switch_name(raw_name)?;
        let enabled = match value {
            Value::Boolean(flag) => *flag,
            _ => {
                return Err(ConfigError::new(format!(
                    "配置项 tools.{name} 必须是布尔值。"
                )))
            }
        };
        switches.insert(name, enabled);
    }
    Ok(switches)
}

/// 当前禁用的内置工具名集合。
pub fn load_disabled_tools(
    env: &ConfigEnvironment,
    config_path: Option<&Path>,
) -> Result<Vec<String>, ConfigError> {
    let switches = load_tool_switches(env, config_path)?;
    Ok(switches
        .into_iter()
        .filter(|(_, enabled)| !enabled)
        .map(|(name, _)| name)
        .collect())
}

/// 批量更新多个工具开关并原子写回（单次读写周期）。
///
/// 先校验全部名称与取值，再一次性写盘；任何一项非法都不会产生部分写入。
/// 切片而不是映射：写回顺序要跟调用方给的一致（Python 侧就是 `dict.update` 的入参顺序）。
pub fn save_tool_switches(
    env: &ConfigEnvironment,
    switches: &[(String, bool)],
    config_path: Option<&Path>,
) -> Result<PathBuf, ConfigError> {
    let mut validated: Vec<(String, bool)> = Vec::new();
    for (raw_name, enabled) in switches {
        let name = validate_tool_switch_name(raw_name)?;
        validated.push((name, *enabled));
    }
    let mut data = load_config_data(env, config_path)?;
    let mut section = get_section(&data, "tools")?;
    for (name, enabled) in validated {
        section.insert(name, Value::Boolean(enabled));
    }
    data.insert("tools".to_string(), Value::Table(section));
    save_config_data(env, &data, config_path)
}

/// 保留其他配置段，只更新单个工具开关并原子写回。
pub fn save_tool_switch(
    env: &ConfigEnvironment,
    name: &str,
    enabled: bool,
    config_path: Option<&Path>,
) -> Result<PathBuf, ConfigError> {
    let switches = vec![(name.to_string(), enabled)];
    save_tool_switches(env, &switches, config_path)
}
