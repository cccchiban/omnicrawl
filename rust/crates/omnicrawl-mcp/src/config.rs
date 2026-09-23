//! MCP 配置：从配置文件的 `[mcp]` 段与环境变量读取（对应 `omnicrawl/mcp/config.py`）。
//!
//! 环境变量只覆盖全局开关与通用阈值，Server 列表仍放在配置文件里：把复杂命令、参数与
//! 环境变量拆到多个来源，会让「到底连了哪个 Server」失去单一真相。

use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;

use omnicrawl_config::core::runtime::{get_section, load_config_data, ConfigEnvironment};
use omnicrawl_config::toml::{Table, Value};

pub const MCP_TRANSPORT_STDIO: &str = "stdio";
pub const MCP_TRANSPORT_STREAMABLE_HTTP: &str = "streamable_http";
pub const VALID_MCP_TRANSPORTS: [&str; 2] = [MCP_TRANSPORT_STDIO, MCP_TRANSPORT_STREAMABLE_HTTP];

pub const MCP_RISK_TRUSTED: &str = "trusted";
pub const MCP_RISK_RESTRICTED: &str = "restricted";
pub const MCP_RISK_EXTERNAL: &str = "external";
pub const VALID_MCP_RISK_LEVELS: [&str; 3] =
    [MCP_RISK_TRUSTED, MCP_RISK_RESTRICTED, MCP_RISK_EXTERNAL];

/// 与 `omnicrawl/workspace/tools.py` 的命令超时同界。
pub const MAX_COMMAND_TIMEOUT_SECONDS: i64 = 360;

const BOOL_TRUE_VALUES: [&str; 7] = ["1", "true", "yes", "on", "enabled", "启用", "是"];
const BOOL_FALSE_VALUES: [&str; 7] = ["0", "false", "no", "off", "disabled", "禁用", "否"];

/// 字符串键值对（Server 的 `env` 与 `headers`）。按键排序，与 Python 的 dict 顺序无关。
pub type TextMap = BTreeMap<String, String>;

/// MCP 配置读取或校验失败。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpConfigError {
    message: String,
}

impl McpConfigError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for McpConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for McpConfigError {}

impl From<omnicrawl_config::ConfigError> for McpConfigError {
    fn from(error: omnicrawl_config::ConfigError) -> Self {
        Self::new(error.message().to_string())
    }
}

/// Host 侧 MCP 安全策略。
///
/// 这些配置只决定默认策略；实际工具调用仍会在 Host 侧按工具名称、Server 风险等级和
/// 审批模式再次判断，避免完全信任外部 Server 声明。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpPolicyConfig {
    pub require_confirmation_for_write: bool,
    pub require_confirmation_for_command: bool,
    pub allow_external_network_tools: bool,
    pub audit_log_enabled: bool,
}

impl Default for McpPolicyConfig {
    fn default() -> Self {
        Self {
            require_confirmation_for_write: true,
            require_confirmation_for_command: true,
            allow_external_network_tools: false,
            audit_log_enabled: true,
        }
    }
}

/// 单个 MCP Server 的连接配置。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpServerConfig {
    pub name: String,
    pub enabled: bool,
    pub transport: String,
    pub command: Option<String>,
    pub args: Vec<String>,
    pub url: Option<String>,
    pub env: TextMap,
    /// 仅由 Streamable HTTP 客户端使用；stdio Server 走本地进程边界，不下发这些请求头。
    pub headers: TextMap,
    pub timeout_seconds: i64,
    pub risk_level: String,
}

impl McpServerConfig {
    /// 单测与调用方构造用的最小 Server：名字 + 传输，其余取默认值。
    pub fn new(name: impl Into<String>, transport: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            enabled: true,
            transport: transport.into(),
            command: None,
            args: Vec::new(),
            url: None,
            env: TextMap::new(),
            headers: TextMap::new(),
            timeout_seconds: 30,
            risk_level: MCP_RISK_RESTRICTED.to_string(),
        }
    }
}

/// MCP 子系统总配置。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpConfig {
    pub enabled: bool,
    pub default_timeout_seconds: i64,
    /// 按配置出现顺序保存的 Server 列表（发现顺序、状态列表与诊断顺序都跟着它）。
    pub servers: Vec<(String, McpServerConfig)>,
    pub policy: McpPolicyConfig,
}

impl Default for McpConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            default_timeout_seconds: 30,
            servers: Vec::new(),
            policy: McpPolicyConfig::default(),
        }
    }
}

impl McpConfig {
    pub fn server(&self, name: &str) -> Option<&McpServerConfig> {
        self.servers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, server)| server)
    }

    pub fn enabled_servers(&self) -> Vec<&McpServerConfig> {
        self.servers
            .iter()
            .map(|(_, server)| server)
            .filter(|server| server.enabled)
            .collect()
    }

    /// 配置里声明的 Server 名（含禁用），用于初始化状态表。
    pub fn server_names(&self) -> Vec<String> {
        self.servers.iter().map(|(name, _)| name.clone()).collect()
    }
}

/// 从 `config.toml` 与环境变量读取 MCP 配置。
pub fn load_mcp_config(
    env: &ConfigEnvironment,
    config_path: Option<&Path>,
) -> Result<McpConfig, McpConfigError> {
    let data = load_config_data(env, config_path)?;
    let section = get_section(&data, "mcp")?;

    let mut enabled = read_bool_field(&section, "enabled", false, "mcp.enabled")?;
    if let Some(env_enabled) = read_bool_env(env, "MCP_ENABLED")? {
        enabled = env_enabled;
    }

    let mut default_timeout = read_int_field(
        &section,
        "default_timeout_seconds",
        30,
        1,
        MAX_COMMAND_TIMEOUT_SECONDS,
        "mcp.default_timeout_seconds",
    )?;
    default_timeout = read_int_env(
        env,
        "MCP_DEFAULT_TIMEOUT_SECONDS",
        default_timeout,
        1,
        MAX_COMMAND_TIMEOUT_SECONDS,
    )?;

    let policy = load_policy_config(&get_section(&section, "policy")?)?;
    let servers = load_server_configs(&section, default_timeout)?;

    Ok(McpConfig {
        enabled,
        default_timeout_seconds: default_timeout,
        servers,
        policy,
    })
}

fn load_policy_config(section: &Table) -> Result<McpPolicyConfig, McpConfigError> {
    Ok(McpPolicyConfig {
        require_confirmation_for_write: read_bool_field(
            section,
            "require_confirmation_for_write",
            true,
            "mcp.policy.require_confirmation_for_write",
        )?,
        require_confirmation_for_command: read_bool_field(
            section,
            "require_confirmation_for_command",
            true,
            "mcp.policy.require_confirmation_for_command",
        )?,
        allow_external_network_tools: read_bool_field(
            section,
            "allow_external_network_tools",
            false,
            "mcp.policy.allow_external_network_tools",
        )?,
        audit_log_enabled: read_bool_field(
            section,
            "audit_log_enabled",
            true,
            "mcp.policy.audit_log_enabled",
        )?,
    })
}

fn load_server_configs(
    section: &Table,
    default_timeout_seconds: i64,
) -> Result<Vec<(String, McpServerConfig)>, McpConfigError> {
    let raw_servers = section.get("servers");
    if raw_servers.is_none() || matches!(raw_servers, Some(Value::String(text)) if text.is_empty())
    {
        return Ok(Vec::new());
    }
    let Value::Table(raw_servers) = raw_servers.expect("已排除缺失分支") else {
        return Err(McpConfigError::new("配置项 mcp.servers 必须是对象。"));
    };

    let mut servers: Vec<(String, McpServerConfig)> = Vec::new();
    for (raw_name, raw_config) in raw_servers {
        let Value::Table(raw_config) = raw_config else {
            return Err(McpConfigError::new(format!(
                "配置项 mcp.servers.{raw_name} 必须是 JSON 对象。"
            )));
        };
        let server = load_server_config(raw_name, raw_config, default_timeout_seconds)?;
        // 与 Python 的 dict 赋值一致：同名重复时替换取值但保留首次出现的位置。
        match servers.iter_mut().find(|(name, _)| name == &server.name) {
            Some(entry) => entry.1 = server,
            None => servers.push((server.name.clone(), server)),
        }
    }
    Ok(servers)
}

fn load_server_config(
    name: &str,
    section: &Table,
    default_timeout_seconds: i64,
) -> Result<McpServerConfig, McpConfigError> {
    let normalized_name = name.trim().to_string();
    if !valid_server_name(&normalized_name) {
        return Err(McpConfigError::new(format!(
            "mcp.servers.{name} 名称只能包含小写字母、数字、下划线和连字符。"
        )));
    }

    let enabled = read_bool_field(
        section,
        "enabled",
        true,
        &format!("mcp.servers.{normalized_name}.enabled"),
    )?;
    let mut transport = read_text_field(
        section,
        "transport",
        MCP_TRANSPORT_STDIO,
        &format!("mcp.servers.{normalized_name}.transport"),
    )?;
    if transport == "streamable-http" {
        transport = MCP_TRANSPORT_STREAMABLE_HTTP.to_string();
    }
    if !VALID_MCP_TRANSPORTS.contains(&transport.as_str()) {
        return Err(McpConfigError::new(format!(
            "mcp.servers.{normalized_name}.transport 仅支持 {}，当前值：{transport}。",
            sorted_join(&VALID_MCP_TRANSPORTS)
        )));
    }

    let command = read_optional_text_field(
        section,
        "command",
        &format!("mcp.servers.{normalized_name}.command"),
    )?;
    let url = read_optional_text_field(
        section,
        "url",
        &format!("mcp.servers.{normalized_name}.url"),
    )?;
    let args = read_text_list_field(
        section,
        "args",
        &format!("mcp.servers.{normalized_name}.args"),
    )?;
    let env = read_text_map_field(
        section,
        "env",
        &format!("mcp.servers.{normalized_name}.env"),
    )?;
    let headers = read_text_map_field(
        section,
        "headers",
        &format!("mcp.servers.{normalized_name}.headers"),
    )?;
    let timeout_seconds = read_int_field(
        section,
        "timeout_seconds",
        default_timeout_seconds,
        1,
        MAX_COMMAND_TIMEOUT_SECONDS,
        &format!("mcp.servers.{normalized_name}.timeout_seconds"),
    )?;
    let risk_level = read_text_field(
        section,
        "risk_level",
        MCP_RISK_RESTRICTED,
        &format!("mcp.servers.{normalized_name}.risk_level"),
    )?;
    if !VALID_MCP_RISK_LEVELS.contains(&risk_level.as_str()) {
        return Err(McpConfigError::new(format!(
            "mcp.servers.{normalized_name}.risk_level 仅支持 {}，当前值：{risk_level}。",
            sorted_join(&VALID_MCP_RISK_LEVELS)
        )));
    }

    if enabled && transport == MCP_TRANSPORT_STDIO && command.is_none() {
        return Err(McpConfigError::new(format!(
            "mcp.servers.{normalized_name}.command 不能为空。"
        )));
    }
    if enabled && transport == MCP_TRANSPORT_STREAMABLE_HTTP {
        let Some(url_text) = url.as_deref() else {
            return Err(McpConfigError::new(format!(
                "mcp.servers.{normalized_name}.url 不能为空。"
            )));
        };
        validate_streamable_http_url(url_text, &normalized_name)?;
    }

    Ok(McpServerConfig {
        name: normalized_name,
        enabled,
        transport,
        command,
        args,
        url,
        env,
        headers,
        timeout_seconds,
        risk_level,
    })
}

/// Server 名只允许小写字母、数字、下划线与连字符（Python 侧同一正则）。
fn valid_server_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_' || ch == '-')
}

/// Streamable HTTP 的地址校验：只允许 http(s)，且明文 HTTP 只能指向本机。
fn validate_streamable_http_url(url: &str, server_name: &str) -> Result<(), McpConfigError> {
    let (scheme, netloc) = split_url(url);
    if !matches!(scheme, Some("http") | Some("https")) || netloc.is_empty() {
        return Err(McpConfigError::new(format!(
            "mcp.servers.{server_name}.url 必须是 http(s) URL。"
        )));
    }
    let hostname = hostname_of(netloc).to_lowercase();
    if scheme == Some("http") && !matches!(hostname.as_str(), "localhost" | "127.0.0.1" | "::1") {
        return Err(McpConfigError::new(format!(
            "mcp.servers.{server_name}.url 默认不允许明文公网 HTTP 地址：{url}"
        )));
    }
    Ok(())
}

/// `urlparse` 的可用子集：返回 `(scheme, netloc)`；没有 `://` 时 scheme 为 `None`。
fn split_url(url: &str) -> (Option<&str>, &str) {
    let Some(position) = url.find("://") else {
        return (None, "");
    };
    let scheme = &url[..position];
    let rest = &url[position + 3..];
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    (Some(scheme), &rest[..end])
}

/// 从 netloc 取主机名：去掉用户信息与端口，保留 IPv6 方括号内的地址。
fn hostname_of(netloc: &str) -> String {
    let after_user = match netloc.rsplit_once('@') {
        Some((_, host)) => host,
        None => netloc,
    };
    if let Some(rest) = after_user.strip_prefix('[') {
        return match rest.split_once(']') {
            Some((host, _)) => host.to_string(),
            None => rest.to_string(),
        };
    }
    match after_user.rsplit_once(':') {
        Some((host, _)) => host.to_string(),
        None => after_user.to_string(),
    }
}

fn read_bool_env(env: &ConfigEnvironment, name: &str) -> Result<Option<bool>, McpConfigError> {
    let Some(value) = env.get(name) else {
        return Ok(None);
    };
    if value.trim().is_empty() {
        return Ok(None);
    }
    Ok(Some(parse_bool_text(&value, name)?))
}

fn read_int_env(
    env: &ConfigEnvironment,
    name: &str,
    default: i64,
    min_value: i64,
    max_value: i64,
) -> Result<i64, McpConfigError> {
    let Some(value) = env.get(name) else {
        return Ok(default);
    };
    if value.trim().is_empty() {
        return Ok(default);
    }
    parse_int_text(&value, min_value, max_value, name)
}

fn read_bool_field(
    section: &Table,
    key: &str,
    default: bool,
    config_key: &str,
) -> Result<bool, McpConfigError> {
    match section.get(key) {
        None => Ok(default),
        Some(Value::Boolean(flag)) => Ok(*flag),
        Some(other) => match other {
            Value::String(text) => parse_bool_text(text, config_key),
            _ => Err(McpConfigError::new(format!(
                "配置项 {config_key} 必须是布尔值。"
            ))),
        },
    }
}

fn parse_bool_text(value: &str, config_key: &str) -> Result<bool, McpConfigError> {
    let normalized = value.trim().to_lowercase();
    if BOOL_TRUE_VALUES.contains(&normalized.as_str()) {
        return Ok(true);
    }
    if BOOL_FALSE_VALUES.contains(&normalized.as_str()) {
        return Ok(false);
    }
    Err(McpConfigError::new(format!(
        "配置项 {config_key} 必须是布尔值。"
    )))
}

fn read_int_field(
    section: &Table,
    key: &str,
    default: i64,
    min_value: i64,
    max_value: i64,
    config_key: &str,
) -> Result<i64, McpConfigError> {
    match section.get(key) {
        None => Ok(default),
        Some(Value::Integer(number)) => check_range(*number, min_value, max_value, config_key),
        Some(Value::Float(number)) => check_range(*number as i64, min_value, max_value, config_key),
        // Python 的 `int("12")` 接受带符号与下划线的十进制字面量；布尔值显式拒绝。
        Some(Value::String(text)) => parse_int_text(text, min_value, max_value, config_key),
        Some(_) => Err(McpConfigError::new(int_message(
            config_key, min_value, max_value, None,
        ))),
    }
}

fn parse_int_text(
    text: &str,
    min_value: i64,
    max_value: i64,
    config_key: &str,
) -> Result<i64, McpConfigError> {
    let trimmed = text.trim();
    let parsed = parse_python_int(trimmed);
    match parsed {
        Some(value) => check_range(value, min_value, max_value, config_key),
        None => Err(McpConfigError::new(int_message(
            config_key, min_value, max_value, None,
        ))),
    }
}

/// Python `int(text)` 的十进制子集：可选正负号，数字间允许单个下划线。
fn parse_python_int(text: &str) -> Option<i64> {
    let (negative, digits) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text.strip_prefix('+').unwrap_or(text)),
    };
    if digits.is_empty() {
        return None;
    }
    if digits.starts_with('_') || digits.ends_with('_') || digits.contains("__") {
        return None;
    }
    let normalized = digits.replace('_', "");
    if !normalized.chars().all(|ch| ch.is_ascii_digit()) {
        return None;
    }
    let value: i64 = normalized.parse().ok()?;
    Some(if negative { -value } else { value })
}

fn check_range(
    value: i64,
    min_value: i64,
    max_value: i64,
    config_key: &str,
) -> Result<i64, McpConfigError> {
    if value < min_value || value > max_value {
        return Err(McpConfigError::new(int_message(
            config_key,
            min_value,
            max_value,
            Some(value),
        )));
    }
    Ok(value)
}

fn int_message(config_key: &str, min_value: i64, max_value: i64, current: Option<i64>) -> String {
    match current {
        Some(value) => format!(
            "配置项 {config_key} 必须是 {min_value} 到 {max_value} 的整数，当前值：{value}。"
        ),
        None => format!("配置项 {config_key} 必须是 {min_value} 到 {max_value} 的整数。"),
    }
}

fn read_text_field(
    section: &Table,
    key: &str,
    default: &str,
    config_key: &str,
) -> Result<String, McpConfigError> {
    match section.get(key) {
        None => Ok(default.to_string()),
        Some(Value::String(text)) => {
            let stripped = text.trim();
            if stripped.is_empty() {
                Ok(default.to_string())
            } else {
                Ok(stripped.to_string())
            }
        }
        Some(_) => Err(McpConfigError::new(format!(
            "配置项 {config_key} 必须是字符串。"
        ))),
    }
}

fn read_optional_text_field(
    section: &Table,
    key: &str,
    config_key: &str,
) -> Result<Option<String>, McpConfigError> {
    match section.get(key) {
        None => Ok(None),
        Some(Value::String(text)) => {
            let stripped = text.trim();
            if stripped.is_empty() {
                Ok(None)
            } else {
                Ok(Some(stripped.to_string()))
            }
        }
        Some(_) => Err(McpConfigError::new(format!(
            "配置项 {config_key} 必须是字符串。"
        ))),
    }
}

fn read_text_list_field(
    section: &Table,
    key: &str,
    config_key: &str,
) -> Result<Vec<String>, McpConfigError> {
    let value = section.get(key);
    let items = match value {
        None => return Ok(Vec::new()),
        Some(Value::Array(items)) => items,
        Some(_) => {
            return Err(McpConfigError::new(format!(
                "配置项 {config_key} 必须是字符串列表。"
            )))
        }
    };
    let mut result = Vec::with_capacity(items.len());
    for (index, item) in items.iter().enumerate() {
        match item {
            Value::String(text) => result.push(text.clone()),
            _ => {
                return Err(McpConfigError::new(format!(
                    "配置项 {config_key}[{}] 必须是字符串。",
                    index + 1
                )))
            }
        }
    }
    Ok(result)
}

fn read_text_map_field(
    section: &Table,
    key: &str,
    config_key: &str,
) -> Result<TextMap, McpConfigError> {
    let value = section.get(key);
    let table = match value {
        None => return Ok(TextMap::new()),
        Some(Value::Table(table)) => table,
        Some(_) => {
            return Err(McpConfigError::new(format!(
                "配置项 {config_key} 必须是字符串键值对象。"
            )))
        }
    };
    let mut result = TextMap::new();
    for (raw_key, raw_value) in table {
        match raw_value {
            Value::String(text) => {
                result.insert(raw_key.clone(), text.clone());
            }
            _ => {
                return Err(McpConfigError::new(format!(
                    "配置项 {config_key} 必须是字符串键值对象。"
                )))
            }
        }
    }
    Ok(result)
}

fn sorted_join(values: &[&str]) -> String {
    let mut sorted: Vec<&str> = values.to_vec();
    sorted.sort_unstable();
    sorted.join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(home: &str) -> ConfigEnvironment {
        ConfigEnvironment::new(home, "linux")
    }

    #[test]
    fn missing_section_keeps_mcp_disabled() {
        let config = load_mcp_config(&env("/tmp"), None).expect("空配置应当可读");
        assert!(!config.enabled);
        assert_eq!(config.default_timeout_seconds, 30);
        assert!(config.servers.is_empty());
        assert_eq!(config.policy, McpPolicyConfig::default());
    }

    #[test]
    fn environment_overrides_global_switch() {
        let environment = env("/tmp").with_env_value("MCP_ENABLED", "是");
        let config = load_mcp_config(&environment, None).expect("环境变量应当可读");
        assert!(config.enabled);
    }

    #[test]
    fn server_name_and_transport_are_validated() {
        assert!(valid_server_name("files-system_1"));
        assert!(!valid_server_name("Files"));
        assert!(!valid_server_name(""));
    }

    #[test]
    fn http_url_requires_https_outside_loopback() {
        assert!(validate_streamable_http_url("http://localhost:8080/mcp", "a").is_ok());
        assert!(validate_streamable_http_url("http://[::1]:8080/mcp", "a").is_ok());
        let error = validate_streamable_http_url("http://example.com/mcp", "a").unwrap_err();
        assert!(error.message().contains("默认不允许明文公网 HTTP 地址"));
        assert!(validate_streamable_http_url("https://example.com/mcp", "a").is_ok());
    }
}
