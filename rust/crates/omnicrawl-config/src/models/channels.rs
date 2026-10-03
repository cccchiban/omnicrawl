//! 模型渠道配置的读取、校验与原子写回（对应 `omnicrawl/config/models/channels.py`）。
//!
//! 渠道是「`config.toml` 里的 profile + `models.toml` 里的自定义模型条目」的成对视图：
//! 读的时候按 profile 补齐 Provider 侧信息，写的时候两侧一起更新并保证 `models.toml`
//! 不落凭据；两次写盘任一步失败都会回滚已写入的文件。

use std::path::{Path, PathBuf};

use omnicrawl_protocol::{Protocol, Provider};

use crate::core::runtime::{
    atomic_write_text, dump_toml_text, load_config_data, load_raw_file, resolve_config_path,
    resolve_config_write_path, resolve_models_path, resolve_models_write_path, ConfigEnvironment,
};
use crate::error::ConfigError;
use crate::toml::{Table, Value};
use crate::value::python_str;

pub const PROVIDER_OPENAI: &str = "openai";
pub const PROVIDER_ANTHROPIC: &str = "anthropic";
pub const PROVIDER_GEMINI: &str = "gemini";

/// 模型条目里禁止出现的凭据键。
const FORBIDDEN_MODEL_KEYS: [&str; 4] = ["api_key", "token", "cookie", "authorization"];

/// 一个可独立选择的模型渠道。
#[derive(Debug, Clone, PartialEq)]
pub struct ChannelConfig {
    pub key: String,
    pub name: String,
    pub profile_id: String,
    pub provider: String,
    pub protocol: String,
    pub base_url: String,
    pub api_key: String,
    pub model_id: String,
    pub enabled: bool,
    pub api_key_env: String,
    pub user_agent: String,
}

impl ChannelConfig {
    pub fn provider_label(&self) -> String {
        provider_label(&self.provider)
    }

    pub fn protocol_label(&self) -> String {
        protocol_label(&self.protocol)
    }
}

/// 渠道集合及当前默认模型 key。
#[derive(Debug, Clone, PartialEq)]
pub struct ChannelConfiguration {
    pub channels: Vec<ChannelConfig>,
    pub default_key: String,
}

/// 可选 Provider 列表。
pub fn provider_options() -> [&'static str; 3] {
    [PROVIDER_OPENAI, PROVIDER_ANTHROPIC, PROVIDER_GEMINI]
}

/// Provider 支持的协议列表（未知 Provider 返回空）。
pub fn protocols_for_provider(provider: &str) -> Vec<&'static str> {
    provider_protocols(provider)
        .iter()
        .map(|protocol| protocol.as_str())
        .collect()
}

pub fn provider_label(provider: &str) -> String {
    match Provider::parse(provider) {
        Some(Provider::Openai) => "OpenAI".to_string(),
        Some(Provider::Anthropic) => "Anthropic".to_string(),
        Some(Provider::Gemini) => "Gemini".to_string(),
        None => provider.to_string(),
    }
}

pub fn protocol_label(protocol: &str) -> String {
    match Protocol::parse(protocol) {
        Some(Protocol::OpenaiChatCompletions) => "Chat Completions".to_string(),
        Some(Protocol::OpenaiResponses) => "Responses".to_string(),
        Some(Protocol::AnthropicMessages) => "Messages".to_string(),
        Some(Protocol::GeminiGenerateContent) => "Generate Content".to_string(),
        None => protocol.to_string(),
    }
}

/// 创建指定 Provider 的安全默认草稿，不包含凭据。
pub fn default_channel(provider: &str, key: Option<&str>) -> Result<ChannelConfig, ConfigError> {
    let protocols = provider_protocols(provider);
    if protocols.is_empty() {
        return Err(ConfigError::new(format!("不支持的请求方式：{provider}")));
    }
    let channel_key = match key {
        Some(text) if !text.is_empty() => text.to_string(),
        _ => format!("{provider}-main"),
    };
    Ok(ChannelConfig {
        name: format!("{} 主渠道", provider_label(provider)),
        profile_id: channel_key.clone(),
        provider: provider.to_string(),
        protocol: protocols[0].as_str().to_string(),
        base_url: provider_default_url(provider).to_string(),
        api_key: String::new(),
        api_key_env: provider_api_key_env(provider).to_string(),
        model_id: provider_default_model(provider).to_string(),
        key: channel_key,
        enabled: true,
        user_agent: String::new(),
    })
}

/// 把渠道名转为稳定 key，并在冲突时追加序号。
pub fn unique_channel_key(name: &str, existing: &[String]) -> String {
    let mut base = String::new();
    let mut pending_separator = false;
    for ch in name.trim().to_lowercase().chars() {
        if ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_' || ch == '-' {
            if pending_separator {
                base.push('-');
                pending_separator = false;
            }
            base.push(ch);
        } else {
            pending_separator = true;
        }
    }
    let mut base = base.trim_matches(['-', '_']).to_string();
    if base.is_empty() {
        base = "channel".to_string();
    }
    let first_is_alnum = base
        .chars()
        .next()
        .map(|ch| ch.is_alphanumeric())
        .unwrap_or(false);
    if !first_is_alnum {
        base = format!("channel-{base}");
    }
    let mut candidate = base.clone();
    let mut suffix = 2;
    while existing.iter().any(|item| item == &candidate) {
        candidate = format!("{base}-{suffix}");
        suffix += 1;
    }
    candidate
}

/// 从现有 profiles 和自定义模型目录构建渠道列表。
pub fn load_channel_configuration(
    env: &ConfigEnvironment,
    config_path: Option<&Path>,
    models_path: Option<&Path>,
) -> Result<ChannelConfiguration, ConfigError> {
    let resolved_config = resolve_config_path(env, config_path)?;
    let resolved_models = resolve_models_path(env, models_path)?;
    let config_data = load_config_data(env, Some(&resolved_config))?;
    let models_data = if resolved_models.exists() {
        load_raw_file(&resolved_models)?
    } else {
        Table::new()
    };

    let empty = Table::new();
    let llm = config_data
        .get("llm")
        .and_then(|value| value.as_table())
        .unwrap_or(&empty);
    let profiles = llm
        .get("profiles")
        .and_then(|value| value.as_table())
        .unwrap_or(&empty);
    let models = models_data
        .get("models")
        .and_then(|value| value.as_table())
        .unwrap_or(&empty);

    let mut channels: Vec<ChannelConfig> = Vec::new();
    for (key, raw_model) in models {
        let Some(raw_model) = raw_model.as_table() else {
            continue;
        };
        let profile_id = python_str(raw_model.get("profile")).trim().to_string();
        let raw_profile = match profiles.get(&profile_id).and_then(|value| value.as_table()) {
            Some(profile) if !profile_id.is_empty() => profile,
            _ => continue,
        };
        let provider = {
            let raw = python_str(raw_profile.get("provider"))
                .trim()
                .to_lowercase();
            if raw.is_empty() {
                PROVIDER_OPENAI.to_string()
            } else {
                raw
            }
        };
        let protocol = {
            let mut raw = python_str(raw_model.get("protocol")).trim().to_string();
            if raw.is_empty() {
                raw = python_str(raw_profile.get("default_protocol"))
                    .trim()
                    .to_string();
            }
            if raw.is_empty() {
                raw = provider_protocols(&provider)
                    .first()
                    .map(|protocol| protocol.as_str().to_string())
                    .unwrap_or_default();
            }
            raw
        };
        let api_key_env = {
            let raw = python_str(raw_profile.get("api_key_env"))
                .trim()
                .to_string();
            if raw.is_empty() {
                provider_api_key_env(&provider).to_string()
            } else {
                raw
            }
        };
        let name = {
            let raw = python_str(raw_model.get("display_name")).trim().to_string();
            if raw.is_empty() {
                key.clone()
            } else {
                raw
            }
        };
        channels.push(ChannelConfig {
            key: key.clone(),
            name,
            profile_id,
            provider,
            protocol,
            base_url: python_str(raw_profile.get("base_url")).trim().to_string(),
            api_key: python_str(raw_profile.get("api_key")).trim().to_string(),
            api_key_env,
            user_agent: python_str(raw_profile.get("user_agent")).trim().to_string(),
            model_id: python_str(raw_model.get("model_id")).trim().to_string(),
            enabled: flag_default_true(raw_model) && flag_default_true(raw_profile),
        });
    }

    let active = llm
        .get("active_model")
        .and_then(|value| value.as_table())
        .unwrap_or(&empty);
    let requested_default = python_str(active.get("key")).trim().to_string();
    let default_key = enabled_default(&channels, &requested_default);
    Ok(ChannelConfiguration {
        channels,
        default_key,
    })
}

/// 把渠道配置写回现有 TOML，同时保证 `models.toml` 不含凭据。
pub fn save_channel_configuration(
    env: &ConfigEnvironment,
    configuration: &ChannelConfiguration,
    config_path: Option<&Path>,
    models_path: Option<&Path>,
) -> Result<(PathBuf, PathBuf), ConfigError> {
    let channels = &configuration.channels;
    if channels.is_empty() {
        return Err(ConfigError::new("至少需要保留一个模型渠道。"));
    }
    validate_channels(channels)?;

    let source_config = resolve_config_path(env, config_path)?;
    let source_models = resolve_models_path(env, models_path)?;
    let resolved_config = resolve_config_write_path(env, config_path)?;
    let resolved_models = resolve_models_write_path(env, models_path)?;

    let mut config_data = load_config_data(env, Some(&source_config))?;
    let mut models_data = if source_models.exists() {
        load_raw_file(&source_models)?
    } else {
        Table::new()
    };

    let mut llm = match config_data.get("llm") {
        Some(Value::Table(table)) => table.clone(),
        _ => Table::new(),
    };
    let mut profiles = match llm.get("profiles") {
        Some(Value::Table(table)) => table.clone(),
        _ => Table::new(),
    };
    let mut raw_models = Table::new();
    let missing_models_section = match models_data.get("models") {
        Some(Value::Table(table)) => {
            raw_models = table.clone();
            false
        }
        _ => true,
    };
    if missing_models_section {
        let version = match models_data.get("version") {
            Some(Value::Integer(value)) => *value,
            _ => 1,
        };
        let mut rebuilt = Table::new();
        rebuilt.insert("version".to_string(), Value::Integer(version));
        rebuilt.insert("models".to_string(), Value::Table(Table::new()));
        models_data = rebuilt;
    }

    let previous_model_profiles: Vec<(String, String)> = raw_models
        .iter()
        .filter_map(|(key, value)| {
            value.as_table().map(|table| {
                (
                    key.clone(),
                    python_str(table.get("profile")).trim().to_string(),
                )
            })
        })
        .collect();
    let final_keys: Vec<&str> = channels.iter().map(|item| item.key.as_str()).collect();
    let final_profiles: Vec<&str> = channels
        .iter()
        .map(|item| item.profile_id.as_str())
        .collect();
    for (key, _) in &previous_model_profiles {
        if !final_keys.contains(&key.as_str()) {
            raw_models.remove(key);
        }
    }
    for (_, profile) in &previous_model_profiles {
        if !profile.is_empty() && !final_profiles.contains(&profile.as_str()) {
            profiles.remove(profile);
        }
    }

    for channel in channels {
        let mut profile = match profiles.get(&channel.profile_id) {
            Some(Value::Table(table)) => table.clone(),
            _ => Table::new(),
        };
        profile.insert(
            "provider".to_string(),
            Value::String(channel.provider.clone()),
        );
        profile.insert("enabled".to_string(), Value::Boolean(channel.enabled));
        profile.insert(
            "base_url".to_string(),
            Value::String(channel.base_url.clone()),
        );
        let api_key_env = if channel.api_key_env.is_empty() {
            provider_api_key_env(&channel.provider).to_string()
        } else {
            channel.api_key_env.clone()
        };
        profile.insert("api_key_env".to_string(), Value::String(api_key_env));
        profile.insert(
            "default_protocol".to_string(),
            Value::String(channel.protocol.clone()),
        );
        if channel.api_key.is_empty() {
            profile.remove("api_key");
        } else {
            profile.insert(
                "api_key".to_string(),
                Value::String(channel.api_key.clone()),
            );
        }
        let user_agent = channel.user_agent.trim();
        if user_agent.is_empty() {
            profile.remove("user_agent");
        } else {
            profile.insert(
                "user_agent".to_string(),
                Value::String(user_agent.to_string()),
            );
        }
        let mut discovery = match profile.get("discovery") {
            Some(Value::Table(table)) => table.clone(),
            _ => Table::new(),
        };
        if !discovery.contains_key("enabled") {
            discovery.insert("enabled".to_string(), Value::Boolean(true));
        }
        profile.insert("discovery".to_string(), Value::Table(discovery));
        profiles.insert(channel.profile_id.clone(), Value::Table(profile));

        let mut model = match raw_models.get(&channel.key) {
            Some(Value::Table(table)) => table.clone(),
            _ => Table::new(),
        };
        model.insert(
            "display_name".to_string(),
            Value::String(channel.name.clone()),
        );
        model.insert(
            "profile".to_string(),
            Value::String(channel.profile_id.clone()),
        );
        model.insert(
            "model_id".to_string(),
            Value::String(channel.model_id.clone()),
        );
        model.insert(
            "protocol".to_string(),
            Value::String(channel.protocol.clone()),
        );
        model.insert("enabled".to_string(), Value::Boolean(channel.enabled));
        for forbidden in FORBIDDEN_MODEL_KEYS {
            model.remove(forbidden);
        }
        raw_models.insert(channel.key.clone(), Value::Table(model));
    }

    let default_key = enabled_default(channels, &configuration.default_key);
    if default_key.is_empty() {
        return Err(ConfigError::new("至少需要启用一个模型渠道。"));
    }
    let mut active_model = Table::new();
    active_model.insert("source".to_string(), Value::String("custom".to_string()));
    active_model.insert("key".to_string(), Value::String(default_key));
    llm.insert("active_model".to_string(), Value::Table(active_model));
    llm.insert("profiles".to_string(), Value::Table(profiles));
    config_data.insert("llm".to_string(), Value::Table(llm));
    if !config_data.contains_key("version") {
        config_data.insert("version".to_string(), Value::Integer(2));
    }
    models_data.insert("models".to_string(), Value::Table(raw_models));
    if !models_data.contains_key("version") {
        models_data.insert("version".to_string(), Value::Integer(1));
    }

    let original_config = read_text_if_exists(&resolved_config)?;
    let original_models = read_text_if_exists(&resolved_models)?;
    let mut written: Vec<(PathBuf, Option<String>)> = Vec::new();
    let mut failure: Option<String> = None;
    match atomic_write_text(&resolved_config, &dump_toml_text(&config_data)) {
        Ok(()) => {
            written.push((resolved_config.clone(), original_config));
            if let Err(error) = atomic_write_text(&resolved_models, &dump_toml_text(&models_data)) {
                failure = Some(error.message().to_string());
            } else {
                written.push((resolved_models.clone(), original_models));
            }
        }
        Err(error) => failure = Some(error.message().to_string()),
    }
    if let Some(message) = failure {
        let mut rollback_errors: Vec<String> = Vec::new();
        for (path, original) in written.iter().rev() {
            let outcome = match original {
                None => std::fs::remove_file(path).map_err(|error| error.to_string()),
                Some(text) => {
                    atomic_write_text(path, text).map_err(|error| error.message().to_string())
                }
            };
            if let Err(error) = outcome {
                rollback_errors.push(format!("{}: {error}", path.display()));
            }
        }
        let detail = if rollback_errors.is_empty() {
            String::new()
        } else {
            format!("；回滚失败：{}", rollback_errors.join("；"))
        };
        return Err(ConfigError::new(format!(
            "模型渠道配置写入失败：{message}{detail}"
        )));
    }
    Ok((resolved_config, resolved_models))
}

/// 返回已启用但没有明文凭据的渠道名称。
pub fn missing_enabled_credentials(
    _env: &ConfigEnvironment,
    configuration: &ChannelConfiguration,
) -> Vec<String> {
    configuration
        .channels
        .iter()
        .filter(|channel| channel.enabled && channel.api_key.is_empty())
        .map(|channel| channel.name.clone())
        .collect()
}

/// 判断默认渠道是否具备可解析的明文 Key。
pub fn has_usable_channel(_env: &ConfigEnvironment, configuration: &ChannelConfiguration) -> bool {
    configuration
        .channels
        .iter()
        .find(|channel| channel.key == configuration.default_key && channel.enabled)
        .is_some_and(|channel| !channel.api_key.is_empty())
}

fn enabled_default(channels: &[ChannelConfig], requested: &str) -> String {
    for channel in channels {
        if channel.key == requested && channel.enabled {
            return requested.to_string();
        }
    }
    channels
        .iter()
        .find(|channel| channel.enabled)
        .map(|channel| channel.key.clone())
        .unwrap_or_default()
}

fn validate_channels(channels: &[ChannelConfig]) -> Result<(), ConfigError> {
    let mut keys: Vec<String> = Vec::new();
    let mut profiles: Vec<(&str, &ChannelConfig)> = Vec::new();
    let mut enabled = 0;
    for channel in channels {
        if !is_valid_channel_key(&channel.key) {
            return Err(ConfigError::new(format!("渠道 key 非法：{}", channel.key)));
        }
        if keys.contains(&channel.key) {
            return Err(ConfigError::new(format!("渠道 key 重复：{}", channel.key)));
        }
        keys.push(channel.key.clone());
        if channel.name.trim().is_empty() {
            return Err(ConfigError::new(format!("渠道 {} 缺少名称。", channel.key)));
        }
        let allowed = provider_protocols(&channel.provider);
        if allowed.is_empty() {
            return Err(ConfigError::new(format!(
                "渠道 {} 的请求方式不支持：{}",
                channel.name, channel.provider
            )));
        }
        match Protocol::parse(&channel.protocol) {
            Some(protocol) if allowed.contains(&protocol) => {}
            _ => {
                return Err(ConfigError::new(format!(
                    "渠道 {} 的协议与请求方式不匹配。",
                    channel.name
                )))
            }
        }
        let (scheme, netloc) = url_scheme_and_netloc(&channel.base_url);
        if (scheme != "http" && scheme != "https") || netloc.is_empty() {
            return Err(ConfigError::new(format!(
                "渠道 {} 的 Base URL 无效。",
                channel.name
            )));
        }
        if channel.model_id.trim().is_empty() {
            return Err(ConfigError::new(format!(
                "渠道 {} 缺少模型 ID。",
                channel.name
            )));
        }
        if channel.user_agent.contains('\r') || channel.user_agent.contains('\n') {
            return Err(ConfigError::new(format!(
                "渠道 {} 的 User-Agent 不能包含换行。",
                channel.name
            )));
        }
        if let Some((_, previous)) = profiles
            .iter()
            .find(|(profile_id, _)| *profile_id == channel.profile_id.as_str())
        {
            let same = previous.provider == channel.provider
                && previous.protocol == channel.protocol
                && previous.base_url == channel.base_url
                && previous.api_key == channel.api_key
                && previous.user_agent == channel.user_agent;
            if !same {
                return Err(ConfigError::new(format!(
                    "Profile {} 被多个不同渠道配置复用。",
                    channel.profile_id
                )));
            }
        }
        profiles.push((channel.profile_id.as_str(), channel));
        if channel.enabled {
            enabled += 1;
        }
    }
    if enabled == 0 {
        return Err(ConfigError::new("至少需要启用一个模型渠道。"));
    }
    Ok(())
}

/// `^[a-z0-9][a-z0-9_-]*$` 的等价匹配。
fn is_valid_channel_key(key: &str) -> bool {
    let mut chars = key.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first.is_ascii_lowercase() || first.is_ascii_digit()) {
        return false;
    }
    chars.all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_' || ch == '-')
}

/// `urllib.parse.urlparse` 的可用子集：返回 `(scheme, netloc)`。
fn url_scheme_and_netloc(url: &str) -> (String, String) {
    let Some(colon) = url.find(':') else {
        return (String::new(), String::new());
    };
    let scheme = &url[..colon];
    if scheme.is_empty()
        || !scheme
            .chars()
            .next()
            .map(|ch| ch.is_ascii_alphabetic())
            .unwrap_or(false)
        || !scheme
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '+' || ch == '-' || ch == '.')
    {
        return (String::new(), String::new());
    }
    let rest = url[colon + 1..].strip_prefix("//").unwrap_or("");
    let netloc = rest
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default()
        .to_string();
    (scheme.to_lowercase(), netloc)
}

fn provider_protocols(provider: &str) -> &'static [Protocol] {
    match Provider::parse(provider) {
        Some(Provider::Openai) => &[Protocol::OpenaiChatCompletions, Protocol::OpenaiResponses],
        Some(Provider::Anthropic) => &[Protocol::AnthropicMessages],
        Some(Provider::Gemini) => &[Protocol::GeminiGenerateContent],
        None => &[],
    }
}

fn provider_api_key_env(provider: &str) -> &'static str {
    match Provider::parse(provider) {
        Some(Provider::Anthropic) => "ANTHROPIC_API_KEY",
        Some(Provider::Gemini) => "GEMINI_API_KEY",
        _ => "OPENAI_API_KEY",
    }
}

fn provider_default_url(provider: &str) -> &'static str {
    match Provider::parse(provider) {
        Some(Provider::Anthropic) => "https://api.anthropic.com",
        Some(Provider::Gemini) => "https://generativelanguage.googleapis.com",
        _ => "https://api.openai.com/v1",
    }
}

fn provider_default_model(provider: &str) -> &'static str {
    match Provider::parse(provider) {
        Some(Provider::Anthropic) => "claude-sonnet-4-5",
        Some(Provider::Gemini) => "gemini-2.5-pro",
        _ => "gpt-5.2",
    }
}

/// Python `table.get(key, True) is not False`：只有显式 `false` 才算关闭。
fn flag_default_true(table: &Table) -> bool {
    match table.get("enabled") {
        Some(Value::Boolean(flag)) => *flag,
        _ => true,
    }
}

/// 读取文本；`utf-8-sig` 语义下先剥掉 BOM。
fn read_text_if_exists(path: &Path) -> Result<Option<String>, ConfigError> {
    if !path.exists() {
        return Ok(None);
    }
    let bytes = std::fs::read(path)
        .map_err(|error| ConfigError::new(format!("读取 {} 失败：{error}", path.display())))?;
    let bytes = bytes
        .strip_prefix(&[0xEF, 0xBB, 0xBF][..])
        .unwrap_or(&bytes);
    let text = String::from_utf8(bytes.to_vec())
        .map_err(|error| ConfigError::new(format!("读取 {} 失败：{error}", path.display())))?;
    Ok(Some(text))
}
