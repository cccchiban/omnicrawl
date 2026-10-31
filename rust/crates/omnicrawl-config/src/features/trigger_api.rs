//! 触发接口（`config.toml` 的 `[trigger_api]` 段）。
//!
//! 本地 API 的 `POST /api/v1/runs` 是交互式入口：占单活动生成名额、写当前会话。无人值守的
//! 外部触发（定时任务、传感器阈值、CI 失败钩子）不能走那条路——它会和用户抢名额，并把噪音
//! 写进用户的对话历史。本段给触发接口一份自己的配置：独立监听地址、独立并发与队列、独立的
//! 工具白名单与超时口径。
//!
//! 与 `[fetch_api]` 一样只接受回环地址：接口不做鉴权，「只有本机程序能访问」是唯一的准入控制。

use std::path::{Path, PathBuf};

use crate::core::runtime::{ConfigEnvironment, get_section, load_config_data, save_config_data};
use crate::error::ConfigError;
use crate::toml::{Table, Value};
use crate::value::{python_str, truthy};

/// `config.toml` 里的段名。
pub const TRIGGER_API_SECTION: &str = "trigger_api";

/// 默认监听端口：与本地 API（8765）、决策接口（8767）、抓取接口（8768）错开。
pub const DEFAULT_TRIGGER_API_PORT: i64 = 8769;

/// 只接受回环地址（无鉴权的本地通道）。
pub const TRIGGER_API_LOOPBACK_HOSTS: [&str; 3] = ["127.0.0.1", "localhost", "::1"];

/// 并发 slot 上限：每个 slot 一套内核进程与工具表，再多就不该开在这台机器上。
pub const MAX_TRIGGER_CONCURRENCY: i64 = 4;

/// 单次回合的超时范围（秒）。
pub const MIN_TRIGGER_TIMEOUT_SECONDS: i64 = 60;
pub const MAX_TRIGGER_TIMEOUT_SECONDS: i64 = 7200;

/// 会话模式：每次触发新会话 / 按 `session_key` 复用持久会话。
pub const SESSION_MODE_PER_TRIGGER: &str = "per_trigger";
pub const SESSION_MODE_KEYED: &str = "keyed";
pub const SESSION_MODES: [&str; 2] = [SESSION_MODE_PER_TRIGGER, SESSION_MODE_KEYED];

/// 无人值守默认工具白名单：只读 + 记忆/知识库 + 联网检索。
///
/// 与 `[mqtt]` 回路同一份口径与同一批工具——两者都是「外部事件驱动、没人在终端前」，
/// 因此默认都不给 shell、文件写入、桌面控制与子代理。要放开就显式写 `[trigger_api] tools`。
pub const DEFAULT_TRIGGER_TOOLS: [&str; 13] = [
    "read",
    "read_image",
    "list",
    "find",
    "grep",
    "memory_search",
    "memory_read",
    "memory_expand_related",
    "memory_write",
    "kb_search",
    "kb_read",
    "web_search",
    "fetcher",
];

/// 触发接口的运行期配置。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TriggerApiConfig {
    /// 是否随宿主启动常驻服务。
    pub enabled: bool,
    pub host: String,
    pub port: i64,
    /// 同时最多跑几个 loop。
    pub max_concurrency: i64,
    /// 排队上限；满了拒绝新触发（不静默丢弃）。
    pub queue_limit: i64,
    /// 单次触发的回合上限（秒）；请求可用 `timeout_seconds` 收窄。
    pub default_timeout_seconds: i64,
    /// `per_trigger` 或 `keyed`。
    pub session_mode: String,
    /// 请求未给工作区时的缺省工作区；空串表示用宿主当前工作区。
    pub default_workspace: String,
    /// 无人值守工具白名单（白名单语义，不是黑名单）。
    pub tools: Vec<String>,
    /// 追加到固定无人值守约束之后的部署专属提示。
    pub system_prompt_append: String,
    /// 触发记录在服务进程内的保留小时数。
    pub retention_hours: i64,
}

impl Default for TriggerApiConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            host: "127.0.0.1".to_string(),
            port: DEFAULT_TRIGGER_API_PORT,
            max_concurrency: 1,
            queue_limit: 32,
            default_timeout_seconds: 900,
            session_mode: SESSION_MODE_PER_TRIGGER.to_string(),
            default_workspace: String::new(),
            tools: DEFAULT_TRIGGER_TOOLS
                .iter()
                .map(|name| name.to_string())
                .collect(),
            system_prompt_append: String::new(),
            retention_hours: 168,
        }
    }
}

impl TriggerApiConfig {
    /// 取值域校验与归一化（错误文案带 `trigger_api` 前缀，与段名一致）。
    pub fn normalize(mut self) -> Result<Self, ConfigError> {
        let host = self.host.trim().to_string();
        if !TRIGGER_API_LOOPBACK_HOSTS.contains(&host.to_lowercase().as_str()) {
            return Err(ConfigError::new(
                "触发接口 trigger_api.host 仅允许回环地址（127.0.0.1 / localhost / ::1）。",
            ));
        }
        self.host = host;
        if !(1..=65535).contains(&self.port) {
            return Err(ConfigError::new(
                "触发接口 trigger_api.port 必须是 1 到 65535 的整数。",
            ));
        }
        if !(1..=MAX_TRIGGER_CONCURRENCY).contains(&self.max_concurrency) {
            return Err(ConfigError::new(format!(
                "触发接口 trigger_api.max_concurrency 必须是 1 到 {MAX_TRIGGER_CONCURRENCY} 的整数。"
            )));
        }
        if !(1..=1024).contains(&self.queue_limit) {
            return Err(ConfigError::new(
                "触发接口 trigger_api.queue_limit 必须是 1 到 1024 的整数。",
            ));
        }
        if !(MIN_TRIGGER_TIMEOUT_SECONDS..=MAX_TRIGGER_TIMEOUT_SECONDS)
            .contains(&self.default_timeout_seconds)
        {
            return Err(ConfigError::new(format!(
                "触发接口 trigger_api.default_timeout_seconds 必须是 \
                 {MIN_TRIGGER_TIMEOUT_SECONDS} 到 {MAX_TRIGGER_TIMEOUT_SECONDS} 的整数。"
            )));
        }
        let mode = self.session_mode.trim().to_lowercase();
        if !SESSION_MODES.contains(&mode.as_str()) {
            return Err(ConfigError::new(
                "触发接口 trigger_api.session_mode 只能是 per_trigger 或 keyed。",
            ));
        }
        self.session_mode = mode;
        self.default_workspace = self.default_workspace.trim().to_string();
        // 空白项与重复项都去掉：白名单是权限表，重复没有意义，空串会永远匹配不到工具。
        let mut tools: Vec<String> = Vec::new();
        for name in &self.tools {
            let name = name.trim().to_string();
            if !name.is_empty() && !tools.iter().any(|existing| existing == &name) {
                tools.push(name);
            }
        }
        self.tools = tools;
        self.system_prompt_append = self.system_prompt_append.trim().to_string();
        if !(1..=24 * 365).contains(&self.retention_hours) {
            return Err(ConfigError::new(
                "触发接口 trigger_api.retention_hours 必须是 1 到 8760 的整数。",
            ));
        }
        Ok(self)
    }

    /// 服务能否真正监听（开关打开）。
    pub fn usable(&self) -> bool {
        self.enabled
    }

    /// 监听地址文本。
    pub fn address(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}

/// 读取 `[trigger_api]` 段；缺段或读不出来时返回默认值（未启用）。
pub fn load_trigger_api_configuration(
    env: &ConfigEnvironment,
    config_path: Option<&Path>,
) -> TriggerApiConfig {
    let Ok(data) = load_config_data(env, config_path) else {
        return TriggerApiConfig::default();
    };
    let Ok(section) = get_section(&data, TRIGGER_API_SECTION) else {
        return TriggerApiConfig::default();
    };
    let defaults = TriggerApiConfig::default();
    TriggerApiConfig {
        enabled: section
            .get("enabled")
            .map(truthy)
            .unwrap_or(defaults.enabled),
        host: text_field(&section, "host", &defaults.host),
        port: integer_field(&section, "port", defaults.port),
        max_concurrency: integer_field(&section, "max_concurrency", defaults.max_concurrency),
        queue_limit: integer_field(&section, "queue_limit", defaults.queue_limit),
        default_timeout_seconds: integer_field(
            &section,
            "default_timeout_seconds",
            defaults.default_timeout_seconds,
        ),
        session_mode: text_field(&section, "session_mode", &defaults.session_mode),
        default_workspace: text_field(&section, "default_workspace", ""),
        // 显式写了 `tools` 就用它（空数组 = 一个都不给，是明确的决定）；
        // 没写时用默认白名单。
        tools: match section.get("tools") {
            Some(Value::Array(items)) => items
                .iter()
                .filter_map(|item| match item {
                    Value::String(text) => Some(text.clone()),
                    _ => None,
                })
                .collect(),
            Some(_) => defaults.tools.clone(),
            None => defaults.tools.clone(),
        },
        system_prompt_append: text_field(&section, "system_prompt_append", ""),
        retention_hours: integer_field(&section, "retention_hours", defaults.retention_hours),
    }
    // 手改坏了监听地址或端口时不阻断其它功能：按「未启用」的默认值回落。
    .normalize()
    .unwrap_or(defaults)
}

/// 把 `[trigger_api]` 写回 `config.toml`，保留其它段。
pub fn save_trigger_api_configuration(
    env: &ConfigEnvironment,
    configuration: &TriggerApiConfig,
    config_path: Option<&Path>,
) -> Result<PathBuf, ConfigError> {
    let normalized = configuration.clone().normalize()?;
    let mut data = load_config_data(env, config_path)?;
    let mut section = Table::new();
    section.insert("enabled".to_string(), Value::Boolean(normalized.enabled));
    section.insert("host".to_string(), Value::String(normalized.host));
    section.insert("port".to_string(), Value::Integer(normalized.port));
    section.insert(
        "max_concurrency".to_string(),
        Value::Integer(normalized.max_concurrency),
    );
    section.insert(
        "queue_limit".to_string(),
        Value::Integer(normalized.queue_limit),
    );
    section.insert(
        "default_timeout_seconds".to_string(),
        Value::Integer(normalized.default_timeout_seconds),
    );
    section.insert(
        "session_mode".to_string(),
        Value::String(normalized.session_mode),
    );
    section.insert(
        "default_workspace".to_string(),
        Value::String(normalized.default_workspace),
    );
    section.insert(
        "tools".to_string(),
        Value::Array(
            normalized
                .tools
                .iter()
                .map(|name| Value::String(name.clone()))
                .collect(),
        ),
    );
    section.insert(
        "system_prompt_append".to_string(),
        Value::String(normalized.system_prompt_append),
    );
    section.insert(
        "retention_hours".to_string(),
        Value::Integer(normalized.retention_hours),
    );
    data.insert(TRIGGER_API_SECTION.to_string(), Value::Table(section));
    save_config_data(env, &data, config_path)
}

/// Python `str(section.get(name) or fallback)`。
fn text_field(section: &Table, name: &str, fallback: &str) -> String {
    let text = python_str(section.get(name));
    if text.is_empty() {
        fallback.to_string()
    } else {
        text
    }
}

/// 段里的整数字段：整数与浮点都认（浮点截断），其余类型回落缺省值。
fn integer_field(section: &Table, name: &str, fallback: i64) -> i64 {
    match section.get(name) {
        Some(Value::Integer(value)) => *value,
        Some(Value::Float(value)) => *value as i64,
        _ => fallback,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn environment(tag: &str) -> ConfigEnvironment {
        let root =
            std::env::temp_dir().join(format!("oc-trigger-api-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join(crate::core::runtime::USER_CONFIG_DIRNAME))
            .expect("建立临时配置目录");
        ConfigEnvironment::new(root.to_string_lossy().to_string(), "win32")
    }

    #[test]
    fn defaults_to_disabled_on_loopback_with_readonly_tools() {
        let env = environment("defaults");
        let configuration = load_trigger_api_configuration(&env, None);
        assert!(!configuration.enabled);
        assert_eq!(configuration.host, "127.0.0.1");
        assert_eq!(configuration.port, DEFAULT_TRIGGER_API_PORT);
        assert_eq!(configuration.address(), "127.0.0.1:8769");
        assert_eq!(configuration.max_concurrency, 1);
        assert_eq!(configuration.session_mode, SESSION_MODE_PER_TRIGGER);
        assert_eq!(configuration.tools, DEFAULT_TRIGGER_TOOLS.to_vec());
        assert!(!configuration.usable());
    }

    #[test]
    fn round_trips_and_keeps_other_sections() {
        let env = environment("roundtrip");
        let mut data = Table::new();
        data.insert("version".to_string(), Value::Integer(2));
        save_config_data(&env, &data, None).expect("写基础配置");

        let configuration = TriggerApiConfig {
            enabled: true,
            host: "localhost".to_string(),
            port: 18_769,
            max_concurrency: 2,
            queue_limit: 8,
            default_timeout_seconds: 600,
            session_mode: SESSION_MODE_KEYED.to_string(),
            default_workspace: "D:/Proj".to_string(),
            tools: vec!["read".to_string(), "web_search".to_string()],
            system_prompt_append: "回复不超过 300 字".to_string(),
            retention_hours: 24,
        };
        save_trigger_api_configuration(&env, &configuration, None).expect("写触发接口段");
        assert_eq!(load_trigger_api_configuration(&env, None), configuration);

        let raw = load_config_data(&env, None).expect("读回配置");
        assert_eq!(raw.get("version"), Some(&Value::Integer(2)));
    }

    #[test]
    fn duplicate_and_blank_tool_names_are_dropped() {
        let configuration = TriggerApiConfig {
            tools: vec![
                " read ".to_string(),
                "read".to_string(),
                "  ".to_string(),
                "list".to_string(),
            ],
            ..TriggerApiConfig::default()
        };
        let normalized = configuration.normalize().expect("应当通过校验");
        assert_eq!(
            normalized.tools,
            vec!["read".to_string(), "list".to_string()]
        );
    }

    #[test]
    fn rejects_non_loopback_and_out_of_range_values() {
        let base = TriggerApiConfig::default();
        let cases: [TriggerApiConfig; 6] = [
            TriggerApiConfig {
                host: "0.0.0.0".to_string(),
                ..base.clone()
            },
            TriggerApiConfig {
                port: 0,
                ..base.clone()
            },
            TriggerApiConfig {
                max_concurrency: MAX_TRIGGER_CONCURRENCY + 1,
                ..base.clone()
            },
            TriggerApiConfig {
                queue_limit: 0,
                ..base.clone()
            },
            TriggerApiConfig {
                default_timeout_seconds: MIN_TRIGGER_TIMEOUT_SECONDS - 1,
                ..base.clone()
            },
            TriggerApiConfig {
                session_mode: "forever".to_string(),
                ..base.clone()
            },
        ];
        for configuration in cases {
            assert!(configuration.normalize().is_err(), "应当被拒绝");
        }
    }

    #[test]
    fn bad_values_fall_back_to_defaults_instead_of_failing() {
        let env = environment("bad");
        let mut data = Table::new();
        let mut section = Table::new();
        section.insert("host".to_string(), Value::String("0.0.0.0".to_string()));
        data.insert(TRIGGER_API_SECTION.to_string(), Value::Table(section));
        save_config_data(&env, &data, None).expect("写坏值");
        assert_eq!(
            load_trigger_api_configuration(&env, None),
            TriggerApiConfig::default()
        );
    }

    #[test]
    fn explicit_empty_tools_means_no_tools() {
        let env = environment("no-tools");
        let mut data = Table::new();
        let mut section = Table::new();
        section.insert("enabled".to_string(), Value::Boolean(true));
        section.insert("tools".to_string(), Value::Array(Vec::new()));
        data.insert(TRIGGER_API_SECTION.to_string(), Value::Table(section));
        save_config_data(&env, &data, None).expect("写配置");
        let configuration = load_trigger_api_configuration(&env, None);
        assert!(configuration.enabled);
        assert!(configuration.tools.is_empty());
    }
}
