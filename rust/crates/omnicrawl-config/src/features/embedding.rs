//! 向量模型配置（`embedding_models.toml`）。
//!
//! 向量模型只做一件事：把一段文本映射成一个定长浮点向量，供记忆检索做语义召回。
//! 全部走云端 OpenAI 兼容接口（`POST {base_url}/embeddings`），不引入本地推理。
//!
//! 与 `models.toml` 分开是刻意的：向量模型不是对话模型，混进去会被对话侧的目录与
//! 能力解析当成候选模型；与 `decision_models.toml` 也分开，因为两者语义不同
//! （决策出答案，向量出坐标）。本模块只做配置读写与校验，请求构造留给接入方。

use std::path::{Path, PathBuf};

use crate::core::runtime::{
    atomic_write_text, dump_toml_text, load_config_data, resolve_embedding_models_path,
    resolve_embedding_models_write_path, ConfigEnvironment,
};
use crate::error::ConfigError;
use crate::toml::{Table, Value};
use crate::value::{python_str, truthy};

/// 默认向量接口基地址（OpenAI 兼容口径，已含 `/v1`）。
pub const DEFAULT_EMBEDDING_BASE_URL: &str = "https://api.openai.com/v1";
/// 默认向量模型。
pub const DEFAULT_EMBEDDING_MODEL: &str = "text-embedding-3-small";
/// 默认凭据环境变量名。
pub const DEFAULT_EMBEDDING_API_KEY_ENV: &str = "OPENAI_API_KEY";
/// 首次生成配置时使用的默认渠道 key。
pub const DEFAULT_EMBEDDING_CHANNEL_KEY: &str = "openai-embedding";

/// 向量维度上限：超过这个值多半是配置写错（或把别的东西填进了维度）。
pub const MAX_EMBEDDING_DIMENSIONS: i64 = 8192;
/// 批量上限：一次请求最多带多少条文本（OpenAI 兼容接口的常见上限是 2048）。
pub const MAX_EMBEDDING_BATCH: i64 = 2048;

/// 一个向量渠道：一套服务地址 + 凭据 + 模型。
#[derive(Debug, Clone, PartialEq)]
pub struct EmbeddingChannelConfig {
    pub key: String,
    pub name: String,
    pub base_url: String,
    /// 内联凭据。为空时走 `api_key_env`。
    pub api_key: String,
    pub api_key_env: String,
    pub model: String,
    /// 期望维度；0 表示由服务端决定（不传 `dimensions`）。
    pub dimensions: i64,
    /// 单次请求最多携带的文本条数。
    pub batch_size: i64,
    pub timeout_seconds: i64,
    pub enabled: bool,
}

impl Default for EmbeddingChannelConfig {
    fn default() -> Self {
        Self {
            key: DEFAULT_EMBEDDING_CHANNEL_KEY.to_string(),
            name: "OpenAI 向量".to_string(),
            base_url: DEFAULT_EMBEDDING_BASE_URL.to_string(),
            api_key: String::new(),
            api_key_env: DEFAULT_EMBEDDING_API_KEY_ENV.to_string(),
            model: DEFAULT_EMBEDDING_MODEL.to_string(),
            dimensions: 0,
            batch_size: 64,
            timeout_seconds: 30,
            enabled: true,
        }
    }
}

impl EmbeddingChannelConfig {
    /// 取值域校验与归一化（错误文案带渠道 key，便于在界面上直接读）。
    pub fn normalize(mut self) -> Result<Self, ConfigError> {
        self.key = self.key.trim().to_string();
        if self.key.is_empty() {
            return Err(ConfigError::new("向量渠道的 key 不能为空。"));
        }
        self.name = self.name.trim().to_string();
        if self.name.is_empty() {
            self.name = self.key.clone();
        }
        self.base_url = self.base_url.trim().trim_end_matches('/').to_string();
        if self.base_url.is_empty() {
            return Err(ConfigError::new(format!(
                "向量渠道 {} 的 base_url 不能为空。",
                self.key
            )));
        }
        self.model = self.model.trim().to_string();
        if self.model.is_empty() {
            return Err(ConfigError::new(format!(
                "向量渠道 {} 的 model 不能为空。",
                self.key
            )));
        }
        self.api_key_env = self.api_key_env.trim().to_string();
        if self.dimensions < 0 {
            return Err(ConfigError::new(format!(
                "向量渠道 {} 的 dimensions 不能为负。",
                self.key
            )));
        }
        if self.dimensions > MAX_EMBEDDING_DIMENSIONS {
            return Err(ConfigError::new(format!(
                "向量渠道 {} 的 dimensions 超过上限 {MAX_EMBEDDING_DIMENSIONS}。",
                self.key
            )));
        }
        if self.batch_size <= 0 {
            self.batch_size = 1;
        }
        self.batch_size = self.batch_size.min(MAX_EMBEDDING_BATCH);
        if self.timeout_seconds <= 0 {
            self.timeout_seconds = 30;
        }
        Ok(self)
    }

    /// 解析凭据：内联优先，否则从环境变量取。
    pub fn resolve_api_key(&self, env: &ConfigEnvironment) -> String {
        let inline = self.api_key.trim();
        if !inline.is_empty() {
            return inline.to_string();
        }
        let name = self.api_key_env.trim();
        if name.is_empty() {
            return String::new();
        }
        env.get_trimmed(name)
    }
}

/// 向量渠道集合与默认渠道 key。
#[derive(Debug, Clone, PartialEq)]
pub struct EmbeddingModelConfiguration {
    pub channels: Vec<EmbeddingChannelConfig>,
    pub default_key: String,
}

impl Default for EmbeddingModelConfiguration {
    fn default() -> Self {
        Self {
            channels: vec![EmbeddingChannelConfig::default()],
            default_key: DEFAULT_EMBEDDING_CHANNEL_KEY.to_string(),
        }
    }
}

impl EmbeddingModelConfiguration {
    /// 校验整份配置：至少一条渠道、key 唯一。
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.channels.is_empty() {
            return Err(ConfigError::new("至少需要保留一个向量渠道。"));
        }
        let mut seen: Vec<&str> = Vec::new();
        for channel in &self.channels {
            let key = channel.key.trim();
            if key.is_empty() {
                return Err(ConfigError::new("向量渠道的 key 不能为空。"));
            }
            if seen.contains(&key) {
                return Err(ConfigError::new(format!("向量渠道 key 重复：{key}。")));
            }
            seen.push(key);
        }
        Ok(())
    }

    /// 当前生效的默认渠道：`default_key` 命中且启用时优先，否则取第一条启用的渠道。
    pub fn active_channel(&self) -> Option<&EmbeddingChannelConfig> {
        let key = self.default_key.trim();
        if !key.is_empty() {
            if let Some(channel) = self
                .channels
                .iter()
                .find(|channel| channel.key == key && channel.enabled)
            {
                return Some(channel);
            }
        }
        self.channels.iter().find(|channel| channel.enabled)
    }

    /// 把默认渠道折算到存在的 key 上（写盘与读盘统一口径）。
    fn normalize_default_key(mut self) -> Self {
        let key = self.default_key.trim().to_string();
        let resolved = self
            .channels
            .iter()
            .find(|channel| channel.key == key && channel.enabled)
            .or_else(|| self.channels.iter().find(|channel| channel.enabled))
            .or_else(|| self.channels.first())
            .map(|channel| channel.key.clone())
            .unwrap_or_default();
        self.default_key = resolved;
        self
    }
}

fn parse_channel(key: &str, section: &Table) -> Result<EmbeddingChannelConfig, ConfigError> {
    let text = |name: &str| python_str(section.get(name)).trim().to_string();
    let number = |name: &str, fallback: i64| -> i64 {
        match section.get(name) {
            Some(Value::Integer(value)) => *value,
            Some(Value::Float(value)) => *value as i64,
            Some(Value::String(value)) => value.trim().parse::<i64>().unwrap_or(fallback),
            _ => fallback,
        }
    };
    let base = EmbeddingChannelConfig::default();
    EmbeddingChannelConfig {
        key: key.trim().to_string(),
        name: match text("name") {
            value if value.is_empty() => key.trim().to_string(),
            value => value,
        },
        base_url: match text("base_url") {
            value if value.is_empty() => base.base_url,
            value => value,
        },
        api_key: text("api_key"),
        api_key_env: match text("api_key_env") {
            value if value.is_empty() => base.api_key_env,
            value => value,
        },
        model: match text("model") {
            value if value.is_empty() => base.model,
            value => value,
        },
        dimensions: number("dimensions", 0),
        batch_size: number("batch_size", base.batch_size),
        timeout_seconds: number("timeout_seconds", base.timeout_seconds),
        enabled: match section.get("enabled") {
            None => true,
            Some(value) => truthy(value),
        },
    }
    .normalize()
}

/// 读取 `embedding_models.toml`；文件或 `channels` 段缺失时返回一条默认渠道。
pub fn load_embedding_model_configuration(
    env: &ConfigEnvironment,
    embedding_models_path: Option<&Path>,
) -> Result<EmbeddingModelConfiguration, ConfigError> {
    let target = resolve_embedding_models_path(env, embedding_models_path)?;
    let data = load_config_data(env, Some(&target))?;
    let channels = match data.get("channels") {
        None => Table::new(),
        Some(Value::String(text)) if text.is_empty() => Table::new(),
        Some(Value::Table(section)) => section.clone(),
        Some(_) => {
            return Err(ConfigError::new(
                "配置段 embedding_models.channels 必须是对象。",
            ))
        }
    };
    if channels.is_empty() {
        return Ok(EmbeddingModelConfiguration::default());
    }
    let mut parsed: Vec<EmbeddingChannelConfig> = Vec::new();
    for (key, value) in &channels {
        let Some(section) = value.as_table() else {
            return Err(ConfigError::new(format!("向量渠道 {key} 必须是对象。")));
        };
        parsed.push(parse_channel(key, section)?);
    }
    let default_key = python_str(data.get("default_key")).trim().to_string();
    let configuration = EmbeddingModelConfiguration {
        channels: parsed,
        default_key,
    }
    .normalize_default_key();
    configuration.validate()?;
    Ok(configuration)
}

/// 把一份向量渠道配置写回 `embedding_models.toml`（保留其他段与 `version`）。
pub fn save_embedding_model_configuration(
    env: &ConfigEnvironment,
    configuration: &EmbeddingModelConfiguration,
    embedding_models_path: Option<&Path>,
) -> Result<PathBuf, ConfigError> {
    configuration.validate()?;
    let mut channels: Vec<EmbeddingChannelConfig> = Vec::new();
    for channel in &configuration.channels {
        channels.push(channel.clone().normalize()?);
    }
    let normalized = EmbeddingModelConfiguration {
        channels,
        default_key: configuration.default_key.clone(),
    }
    .normalize_default_key();
    normalized.validate()?;

    let source = resolve_embedding_models_path(env, embedding_models_path)?;
    let target = resolve_embedding_models_write_path(env, embedding_models_path)?;
    let mut data = if source.exists() {
        load_config_data(env, Some(&source))?
    } else {
        Table::new()
    };
    if !data.contains_key("version") {
        data.insert("version".to_string(), Value::Integer(1));
    }
    data.insert(
        "default_key".to_string(),
        Value::String(normalized.default_key.clone()),
    );
    let mut section = Table::new();
    for channel in &normalized.channels {
        let mut entry = Table::new();
        entry.insert("name".to_string(), Value::String(channel.name.clone()));
        entry.insert(
            "base_url".to_string(),
            Value::String(channel.base_url.clone()),
        );
        entry.insert(
            "api_key".to_string(),
            Value::String(channel.api_key.clone()),
        );
        entry.insert(
            "api_key_env".to_string(),
            Value::String(channel.api_key_env.clone()),
        );
        entry.insert("model".to_string(), Value::String(channel.model.clone()));
        entry.insert("dimensions".to_string(), Value::Integer(channel.dimensions));
        entry.insert("batch_size".to_string(), Value::Integer(channel.batch_size));
        entry.insert(
            "timeout_seconds".to_string(),
            Value::Integer(channel.timeout_seconds),
        );
        entry.insert("enabled".to_string(), Value::Boolean(channel.enabled));
        section.insert(channel.key.clone(), Value::Table(entry));
    }
    data.insert("channels".to_string(), Value::Table(section));
    atomic_write_text(&target, &dump_toml_text(&data))?;
    Ok(target)
}
