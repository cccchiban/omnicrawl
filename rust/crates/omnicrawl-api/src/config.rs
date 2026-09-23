//! `omnicrawl/api/models.py::APIConfig` 与 `omnicrawl/api/app.py::load_api_config` 的移植。
//!
//! 校验顺序、默认值与错误文案都按 Python 侧原样搬运：令牌 → 回环地址 → 端口 →
//! 确认超时 → worker 上限 → CORS 通配符。对照数据集由
//! `rust/tools/gen_api_config_fixture.py` 从 Python 真实现生成。

use std::fmt;
use std::path::Path;

use omnicrawl_config::core::runtime::{get_section, load_config_data, ConfigEnvironment};
use omnicrawl_config::error::ConfigError;
use omnicrawl_config::toml::{Table, Value};

pub const API_PREFIX: &str = "/api/v1";

/// 多 worker 上限：每个 worker 都会建自己的 Agent 与隔离工作区，只用它拦住明显误配。
pub const MAX_API_WORKERS: usize = 32;

pub const DEFAULT_HOST: &str = "127.0.0.1";
pub const DEFAULT_PORT: i64 = 8765;
pub const DEFAULT_CONFIRMATION_TIMEOUT_SECONDS: f64 = 300.0;
pub const DEFAULT_WORKERS: i64 = 1;

/// 服务只接受回环地址。
pub const LOOPBACK_HOSTS: [&str; 3] = ["127.0.0.1", "localhost", "::1"];

pub const TOKEN_ENV: &str = "OMNICRAWL_API_TOKEN";
pub const HOST_ENV: &str = "OMNICRAWL_API_HOST";
pub const PORT_ENV: &str = "OMNICRAWL_API_PORT";
pub const WORKERS_ENV: &str = "OMNICRAWL_API_WORKERS";

/// 多 worker 子进程标记：监督进程给每个子进程设置它，子进程据此不再拉起下层。
pub const WORKER_CHILD_ENV: &str = "OMNICRAWL_API_WORKER";

const EMPTY_TOKEN: &str = "api.bearer_token 或 OMNICRAWL_API_TOKEN 不能为空。";
const NON_LOOPBACK_HOST: &str = "API 服务首版仅允许绑定回环地址。";
const INVALID_PORT_RANGE: &str = "api.port 必须是 1 到 65535 的整数。";
const INVALID_TIMEOUT_RANGE: &str = "api.confirmation_timeout_seconds 必须大于 0。";
const ORIGIN_WILDCARD: &str = "api.allowed_origins 不允许使用通配符 *。";
const PORT_NOT_INTEGER: &str = "api.port 必须是整数。";
const TIMEOUT_NOT_NUMBER: &str = "api.confirmation_timeout_seconds 必须是数字。";
const WORKERS_NOT_INTEGER: &str = "api.workers 必须是整数。";
const ORIGINS_NOT_STRING_LIST: &str = "api.allowed_origins 必须是字符串列表。";

/// 配置装载或校验失败；文案与 Python 侧异常消息逐字一致。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiConfigError {
    message: String,
}

impl ApiConfigError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for ApiConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ApiConfigError {}

impl From<ConfigError> for ApiConfigError {
    fn from(error: ConfigError) -> Self {
        Self::new(error.to_string())
    }
}

/// 本地 API 服务配置（对应 `APIConfig`）。
#[derive(Debug, Clone, PartialEq)]
pub struct ApiConfig {
    pub bearer_token: String,
    pub host: String,
    pub port: u16,
    /// 已去除空白项与空串的精确来源白名单；空列表表示不安装 CORS 中间件。
    pub allowed_origins: Vec<String>,
    pub confirmation_timeout_seconds: f64,
    /// `> 1` 时以多进程监听（监督进程 + SO_REUSEPORT），运行状态改由共享存储承载。
    /// 不支持 SO_REUSEPORT 的平台上退化为单进程（见 `app.rs`）。
    pub workers: usize,
}

impl ApiConfig {
    /// 按 Python `APIConfig.__post_init__` 的顺序校验并归一化。
    ///
    /// `port` / `workers` 收 `i64` 是为了能用同一条路径报出「超出范围」而不是类型错误。
    pub fn new(
        bearer_token: impl Into<String>,
        host: impl Into<String>,
        port: i64,
        allowed_origins: Vec<String>,
        confirmation_timeout_seconds: f64,
        workers: i64,
    ) -> Result<Self, ApiConfigError> {
        let bearer_token = bearer_token.into();
        let host = host.into();
        let token = bearer_token.trim();
        let host_text = host.trim().to_string();
        if token.is_empty() {
            return Err(ApiConfigError::new(EMPTY_TOKEN));
        }
        if !LOOPBACK_HOSTS.contains(&host_text.to_lowercase().as_str()) {
            return Err(ApiConfigError::new(NON_LOOPBACK_HOST));
        }
        if !(1..=65535).contains(&port) {
            return Err(ApiConfigError::new(INVALID_PORT_RANGE));
        }
        if confirmation_timeout_seconds <= 0.0 {
            return Err(ApiConfigError::new(INVALID_TIMEOUT_RANGE));
        }
        if !(1..=MAX_API_WORKERS as i64).contains(&workers) {
            return Err(ApiConfigError::new(format!(
                "api.workers 必须是 1 到 {MAX_API_WORKERS} 的整数。"
            )));
        }
        let origins: Vec<String> = allowed_origins
            .iter()
            .map(|origin| origin.trim().to_string())
            .filter(|origin| !origin.is_empty())
            .collect();
        if origins.iter().any(|origin| origin == "*") {
            return Err(ApiConfigError::new(ORIGIN_WILDCARD));
        }
        Ok(Self {
            bearer_token: token.to_string(),
            host: host_text,
            port: port as u16,
            allowed_origins: origins,
            confirmation_timeout_seconds,
            workers: workers as usize,
        })
    }
}

/// 读取 `config.toml` 的 `api` 段与 `OMNICRAWL_API_*` 环境变量，环境变量优先。
pub fn load_api_config(
    env: &ConfigEnvironment,
    config_path: Option<&Path>,
) -> Result<ApiConfig, ApiConfigError> {
    let data = load_config_data(env, config_path)?;
    let section = get_section(&data, "api")?;
    api_config_from_section(&section, env)
}

/// 已取到 `api` 段的装载路径：便于对照测试直接喂段表。
pub fn api_config_from_section(
    section: &Table,
    env: &ConfigEnvironment,
) -> Result<ApiConfig, ApiConfigError> {
    let env_token = env.get_trimmed(TOKEN_ENV);
    let bearer_token = if env_token.is_empty() {
        section_token(section, "bearer_token")
    } else {
        env_token
    };

    let env_host = env.get_trimmed(HOST_ENV);
    let host = if env_host.is_empty() {
        let from_section = section_host(section);
        if from_section.is_empty() {
            DEFAULT_HOST.to_string()
        } else {
            from_section
        }
    } else {
        env_host
    };

    let env_port = env.get_trimmed(PORT_ENV);
    let raw_port = if env_port.is_empty() {
        section
            .get("port")
            .cloned()
            .unwrap_or(Value::Integer(DEFAULT_PORT))
    } else {
        Value::String(env_port)
    };
    let port = python_int(&raw_port).ok_or_else(|| ApiConfigError::new(PORT_NOT_INTEGER))?;

    let origins = match section.get("allowed_origins") {
        None => Vec::new(),
        Some(Value::String(text)) => text
            .split(',')
            .map(|item| item.trim().to_string())
            .filter(|item| !item.is_empty())
            .collect(),
        Some(Value::Array(items)) if items.iter().all(|item| item.is_str()) => items
            .iter()
            .map(|item| item.as_str().unwrap_or_default().to_string())
            .collect(),
        Some(_) => return Err(ApiConfigError::new(ORIGINS_NOT_STRING_LIST)),
    };

    let timeout = match section.get("confirmation_timeout_seconds") {
        None => DEFAULT_CONFIRMATION_TIMEOUT_SECONDS,
        Some(value) => {
            python_float(value).ok_or_else(|| ApiConfigError::new(TIMEOUT_NOT_NUMBER))?
        }
    };

    let env_workers = env.get_trimmed(WORKERS_ENV);
    let raw_workers = if env_workers.is_empty() {
        section
            .get("workers")
            .cloned()
            .unwrap_or(Value::Integer(DEFAULT_WORKERS))
    } else {
        Value::String(env_workers)
    };
    let workers =
        python_int(&raw_workers).ok_or_else(|| ApiConfigError::new(WORKERS_NOT_INTEGER))?;

    ApiConfig::new(bearer_token, host, port, origins, timeout, workers)
}

/// 段里的令牌项：只有字符串算数，其余类型与 Python 一样按缺省处理。
fn section_token(section: &Table, key: &str) -> String {
    match section.get(key) {
        Some(Value::String(text)) => text.clone(),
        _ => String::new(),
    }
}

/// 段里的监听地址：Python 侧走 `str(host)`，非标量只可能落到同一条非回环文案。
fn section_host(section: &Table) -> String {
    match section.get("host") {
        None => String::new(),
        Some(value) => python_text(value),
    }
}

/// `str(...)` 的同义；容器不还原 Python 的 repr，它们都不是回环地址。
fn python_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Integer(number) => number.to_string(),
        Value::Float(number) => omnicrawl_config::toml::float_repr(*number),
        Value::Boolean(flag) => if *flag { "True" } else { "False" }.to_string(),
        Value::Datetime(moment) => moment.to_string(),
        Value::Array(_) | Value::Table(_) => "<非标量>".to_string(),
    }
}

/// `int(...)` 的同义：整数、布尔、可截断的浮点，以及严格的十进制字符串。
///
/// 布尔按 Python 的 `int(True) == 1` 处理；`"1_0"` 这类带下划线的字面量不支持（见 README）。
fn python_int(value: &Value) -> Option<i64> {
    match value {
        Value::Integer(number) => Some(*number),
        Value::Boolean(flag) => Some(i64::from(*flag)),
        Value::Float(number) if number.is_finite() => Some(*number as i64),
        Value::String(text) => text.trim().parse::<i64>().ok(),
        _ => None,
    }
}

/// `float(...)` 的同义：整数、布尔、浮点与数字字符串（含 `nan` / `inf`）。
fn python_float(value: &Value) -> Option<f64> {
    match value {
        Value::Integer(number) => Some(*number as f64),
        Value::Boolean(flag) => Some(if *flag { 1.0 } else { 0.0 }),
        Value::Float(number) => Some(*number),
        Value::String(text) => text.trim().parse::<f64>().ok(),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env() -> ConfigEnvironment {
        ConfigEnvironment::new("C:\\oc-api-home", "win32")
    }

    fn valid() -> ApiConfig {
        ApiConfig::new("token", "127.0.0.1", 8765, Vec::new(), 300.0, 1).expect("合法配置")
    }

    #[test]
    fn defaults_match_python() {
        let config = valid();
        assert_eq!(config.host, DEFAULT_HOST);
        assert_eq!(config.port, 8765);
        assert_eq!(config.confirmation_timeout_seconds, 300.0);
        assert_eq!(config.workers, 1);
        assert!(config.allowed_origins.is_empty());
    }

    #[test]
    fn validation_rejects_bad_values_in_python_order() {
        let cases: [(Result<ApiConfig, ApiConfigError>, &str); 6] = [
            (
                ApiConfig::new("  ", "0.0.0.0", 8765, vec!["*".into()], 300.0, 33),
                EMPTY_TOKEN,
            ),
            (
                ApiConfig::new("token", "0.0.0.0", 8765, vec!["*".into()], 300.0, 33),
                NON_LOOPBACK_HOST,
            ),
            (
                ApiConfig::new("token", "127.0.0.1", 0, vec!["*".into()], 300.0, 33),
                INVALID_PORT_RANGE,
            ),
            (
                ApiConfig::new("token", "127.0.0.1", 8765, vec!["*".into()], 0.0, 33),
                INVALID_TIMEOUT_RANGE,
            ),
            (
                ApiConfig::new("token", "127.0.0.1", 8765, vec!["*".into()], 300.0, 33),
                "api.workers 必须是 1 到 32 的整数。",
            ),
            (
                ApiConfig::new("token", "127.0.0.1", 8765, vec![" * ".into()], 300.0, 1),
                ORIGIN_WILDCARD,
            ),
        ];
        for (outcome, expected) in cases {
            assert_eq!(outcome.expect_err("应当被拒绝").message(), expected);
        }
    }

    #[test]
    fn load_prefers_environment_and_normalizes_origins() {
        let mut section = Table::new();
        section.insert("bearer_token".into(), Value::String("file-token".into()));
        section.insert("host".into(), Value::String(" LocalHost ".into()));
        section.insert(
            "allowed_origins".into(),
            Value::Array(vec![
                Value::String("  http://a  ".into()),
                Value::String("   ".into()),
                Value::String("http://b".into()),
            ]),
        );
        let launched = env().with_env_value(TOKEN_ENV, " env-token ");
        let config = api_config_from_section(&section, &launched).expect("应当装载成功");
        assert_eq!(config.bearer_token, "env-token");
        assert_eq!(config.host, "LocalHost");
        assert_eq!(config.allowed_origins, vec!["http://a", "http://b"]);
    }

    #[test]
    fn load_reports_type_errors_with_python_messages() {
        let mut section = Table::new();
        section.insert("bearer_token".into(), Value::String("token".into()));
        section.insert("port".into(), Value::String("9000.0".into()));
        assert_eq!(
            api_config_from_section(&section, &env())
                .expect_err("应当被拒绝")
                .message(),
            PORT_NOT_INTEGER
        );

        section.insert("port".into(), Value::Integer(8765));
        section.insert(
            "confirmation_timeout_seconds".into(),
            Value::String("soon".into()),
        );
        assert_eq!(
            api_config_from_section(&section, &env())
                .expect_err("应当被拒绝")
                .message(),
            TIMEOUT_NOT_NUMBER
        );

        section.remove("confirmation_timeout_seconds");
        section.insert("workers".into(), Value::String("many".into()));
        assert_eq!(
            api_config_from_section(&section, &env())
                .expect_err("应当被拒绝")
                .message(),
            WORKERS_NOT_INTEGER
        );

        section.insert("workers".into(), Value::Integer(1));
        section.insert(
            "allowed_origins".into(),
            Value::Array(vec![Value::Integer(1)]),
        );
        assert_eq!(
            api_config_from_section(&section, &env())
                .expect_err("应当被拒绝")
                .message(),
            ORIGINS_NOT_STRING_LIST
        );
    }
}
