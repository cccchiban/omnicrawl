//! 向量模型配置（`embedding_models.toml`）。
//!
//! 文件里放两组渠道，对应检索的两个阶段：
//! - **嵌入模型**（`[channels.*]`）：把文本映射成定长向量，负责从大池子里粗筛召回；
//! - **重排序模型**（`[rerank_channels.*]`）：对召回出的候选逐条打分，负责精排去噪。
//!
//! 两者都走云端 OpenAI 兼容接口（嵌入 `POST {base_url}/embeddings`，重排
//! `POST {base_url}/rerank`），不引入本地推理。
//!
//! 与 `models.toml` 分开是刻意的：它们不是对话模型，混进去会被对话侧的目录与
//! 能力解析当成候选模型；与 `decision_models.toml` 也分开，因为语义不同
//! （决策出答案，向量出坐标与名次）。本模块只做配置读写与校验，请求构造留给接入方。

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
/// 首次生成配置时使用的默认渠道 key。
pub const DEFAULT_EMBEDDING_CHANNEL_KEY: &str = "openai-embedding";

/// 默认重排序接口基地址（OpenAI 兼容口径，已含 `/v1`）。
pub const DEFAULT_RERANK_BASE_URL: &str = "https://api.jina.ai/v1";
/// 默认重排序模型。
pub const DEFAULT_RERANK_MODEL: &str = "jina-reranker-v2-base-multilingual";
/// 首次生成配置时使用的默认重排渠道 key。
pub const DEFAULT_RERANK_CHANNEL_KEY: &str = "jina-rerank";

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
    pub api_key: String,
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

    /// 解析凭据：只认配置里的明文 `api_key`。
    pub fn resolve_api_key(&self, _env: &ConfigEnvironment) -> String {
        self.api_key.trim().to_string()
    }
}

/// 一个重排序渠道：同一套服务地址 + 凭据 + 模型，但没有维度概念。
///
/// 重排接口一次只比一批候选，批大小由调用方按候选池决定，因此这里也没有 `batch_size`。
#[derive(Debug, Clone, PartialEq)]
pub struct RerankChannelConfig {
    pub key: String,
    pub name: String,
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub timeout_seconds: i64,
    pub enabled: bool,
}

impl Default for RerankChannelConfig {
    fn default() -> Self {
        Self {
            key: DEFAULT_RERANK_CHANNEL_KEY.to_string(),
            name: "Jina 重排序".to_string(),
            base_url: DEFAULT_RERANK_BASE_URL.to_string(),
            api_key: String::new(),
            model: DEFAULT_RERANK_MODEL.to_string(),
            timeout_seconds: 30,
            enabled: true,
        }
    }
}

impl RerankChannelConfig {
    /// 取值域校验与归一化（错误文案带渠道 key）。
    pub fn normalize(mut self) -> Result<Self, ConfigError> {
        self.key = self.key.trim().to_string();
        if self.key.is_empty() {
            return Err(ConfigError::new("重排序渠道的 key 不能为空。"));
        }
        self.name = self.name.trim().to_string();
        if self.name.is_empty() {
            self.name = self.key.clone();
        }
        self.base_url = self.base_url.trim().trim_end_matches('/').to_string();
        if self.base_url.is_empty() {
            return Err(ConfigError::new(format!(
                "重排序渠道 {} 的 base_url 不能为空。",
                self.key
            )));
        }
        self.model = self.model.trim().to_string();
        if self.model.is_empty() {
            return Err(ConfigError::new(format!(
                "重排序渠道 {} 的 model 不能为空。",
                self.key
            )));
        }
        if self.timeout_seconds <= 0 {
            self.timeout_seconds = 30;
        }
        Ok(self)
    }

    /// 解析凭据：只认配置里的明文 `api_key`。
    pub fn resolve_api_key(&self, _env: &ConfigEnvironment) -> String {
        self.api_key.trim().to_string()
    }
}

/// 向量渠道集合与默认渠道 key（嵌入与重排各一组）。
#[derive(Debug, Clone, PartialEq)]
pub struct EmbeddingModelConfiguration {
    pub channels: Vec<EmbeddingChannelConfig>,
    pub default_key: String,
    pub rerank_channels: Vec<RerankChannelConfig>,
    pub rerank_default_key: String,
}

impl Default for EmbeddingModelConfiguration {
    fn default() -> Self {
        Self {
            channels: vec![EmbeddingChannelConfig::default()],
            default_key: DEFAULT_EMBEDDING_CHANNEL_KEY.to_string(),
            rerank_channels: vec![RerankChannelConfig::default()],
            rerank_default_key: DEFAULT_RERANK_CHANNEL_KEY.to_string(),
        }
    }
}

impl EmbeddingModelConfiguration {
    /// 校验整份配置：两组各至少一条渠道、各自 key 唯一。
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.channels.is_empty() {
            return Err(ConfigError::new("至少需要保留一个嵌入模型渠道。"));
        }
        let mut seen: Vec<&str> = Vec::new();
        for channel in &self.channels {
            let key = channel.key.trim();
            if key.is_empty() {
                return Err(ConfigError::new("嵌入模型渠道的 key 不能为空。"));
            }
            if seen.contains(&key) {
                return Err(ConfigError::new(format!(
                    "嵌入模型渠道 key 重复：{key}。"
                )));
            }
            seen.push(key);
        }
        if self.rerank_channels.is_empty() {
            return Err(ConfigError::new("至少需要保留一个重排序模型渠道。"));
        }
        let mut seen: Vec<&str> = Vec::new();
        for channel in &self.rerank_channels {
            let key = channel.key.trim();
            if key.is_empty() {
                return Err(ConfigError::new("重排序模型渠道的 key 不能为空。"));
            }
            if seen.contains(&key) {
                return Err(ConfigError::new(format!(
                    "重排序模型渠道 key 重复：{key}。"
                )));
            }
            seen.push(key);
        }
        Ok(())
    }

    /// 当前生效的默认嵌入渠道：`default_key` 命中且启用时优先，否则取第一条启用的渠道。
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

    /// 当前生效的默认重排渠道，口径与 [`Self::active_channel`] 一致。
    pub fn active_rerank_channel(&self) -> Option<&RerankChannelConfig> {
        let key = self.rerank_default_key.trim();
        if !key.is_empty() {
            if let Some(channel) = self
                .rerank_channels
                .iter()
                .find(|channel| channel.key == key && channel.enabled)
            {
                return Some(channel);
            }
        }
        self.rerank_channels.iter().find(|channel| channel.enabled)
    }

    /// 把两组默认渠道折算到存在的 key 上（写盘与读盘统一口径）。
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
        let key = self.rerank_default_key.trim().to_string();
        let resolved = self
            .rerank_channels
            .iter()
            .find(|channel| channel.key == key && channel.enabled)
            .or_else(|| self.rerank_channels.iter().find(|channel| channel.enabled))
            .or_else(|| self.rerank_channels.first())
            .map(|channel| channel.key.clone())
            .unwrap_or_default();
        self.rerank_default_key = resolved;
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

fn parse_rerank_channel(key: &str, section: &Table) -> Result<RerankChannelConfig, ConfigError> {
    let text = |name: &str| python_str(section.get(name)).trim().to_string();
    let number = |name: &str, fallback: i64| -> i64 {
        match section.get(name) {
            Some(Value::Integer(value)) => *value,
            Some(Value::Float(value)) => *value as i64,
            Some(Value::String(value)) => value.trim().parse::<i64>().unwrap_or(fallback),
            _ => fallback,
        }
    };
    let base = RerankChannelConfig::default();
    RerankChannelConfig {
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
        model: match text("model") {
            value if value.is_empty() => base.model,
            value => value,
        },
        timeout_seconds: number("timeout_seconds", base.timeout_seconds),
        enabled: match section.get("enabled") {
            None => true,
            Some(value) => truthy(value),
        },
    }
    .normalize()
}

/// 取一个 `[<name>.*]` 形式的渠道表；缺失或空串都当作空表。
fn channel_table(
    data: &Table,
    name: &str,
) -> Result<Table, ConfigError> {
    match data.get(name) {
        None => Ok(Table::new()),
        Some(Value::String(text)) if text.is_empty() => Ok(Table::new()),
        Some(Value::Table(section)) => Ok(section.clone()),
        Some(_) => Err(ConfigError::new(format!(
            "配置段 embedding_models.{name} 必须是对象。"
        ))),
    }
}

/// 读取 `embedding_models.toml`；某一段缺失时该段返回一条默认渠道。
///
/// 两组渠道各自独立兜底：只配了嵌入渠道的旧文件照样能读，重排侧拿到默认渠道。
pub fn load_embedding_model_configuration(
    env: &ConfigEnvironment,
    embedding_models_path: Option<&Path>,
) -> Result<EmbeddingModelConfiguration, ConfigError> {
    let target = resolve_embedding_models_path(env, embedding_models_path)?;
    let data = load_config_data(env, Some(&target))?;
    let defaults = EmbeddingModelConfiguration::default();

    let channels = channel_table(&data, "channels")?;
    let mut parsed: Vec<EmbeddingChannelConfig> = Vec::new();
    for (key, value) in &channels {
        let Some(section) = value.as_table() else {
            return Err(ConfigError::new(format!("向量渠道 {key} 必须是对象。")));
        };
        parsed.push(parse_channel(key, section)?);
    }
    let rerank_channels = channel_table(&data, "rerank_channels")?;
    let mut parsed_rerank: Vec<RerankChannelConfig> = Vec::new();
    for (key, value) in &rerank_channels {
        let Some(section) = value.as_table() else {
            return Err(ConfigError::new(format!("重排序渠道 {key} 必须是对象。")));
        };
        parsed_rerank.push(parse_rerank_channel(key, section)?);
    }

    let configuration = EmbeddingModelConfiguration {
        channels: if parsed.is_empty() {
            defaults.channels
        } else {
            parsed
        },
        default_key: python_str(data.get("default_key")).trim().to_string(),
        rerank_channels: if parsed_rerank.is_empty() {
            defaults.rerank_channels
        } else {
            parsed_rerank
        },
        rerank_default_key: python_str(data.get("rerank_default_key"))
            .trim()
            .to_string(),
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
    let mut rerank_channels: Vec<RerankChannelConfig> = Vec::new();
    for channel in &configuration.rerank_channels {
        rerank_channels.push(channel.clone().normalize()?);
    }
    let normalized = EmbeddingModelConfiguration {
        channels,
        default_key: configuration.default_key.clone(),
        rerank_channels,
        rerank_default_key: configuration.rerank_default_key.clone(),
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
    data.insert(
        "rerank_default_key".to_string(),
        Value::String(normalized.rerank_default_key.clone()),
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
    let mut rerank_section = Table::new();
    for channel in &normalized.rerank_channels {
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
        entry.insert("model".to_string(), Value::String(channel.model.clone()));
        entry.insert(
            "timeout_seconds".to_string(),
            Value::Integer(channel.timeout_seconds),
        );
        entry.insert("enabled".to_string(), Value::Boolean(channel.enabled));
        rerank_section.insert(channel.key.clone(), Value::Table(entry));
    }
    data.insert(
        "rerank_channels".to_string(),
        Value::Table(rerank_section),
    );
    atomic_write_text(&target, &dump_toml_text(&data))?;
    Ok(target)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::runtime::USER_CONFIG_DIRNAME;

    fn temp_root(tag: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("oc-embedding-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(path.join(USER_CONFIG_DIRNAME)).expect("建立临时配置目录");
        path
    }

    fn env_for(root: &Path) -> ConfigEnvironment {
        ConfigEnvironment::new(root.to_string_lossy().to_string(), "win32")
    }

    #[test]
    fn missing_file_yields_one_default_channel_per_group() {
        let root = temp_root("empty");
        let configuration =
            load_embedding_model_configuration(&env_for(&root), None).expect("缺文件时用默认值");
        assert_eq!(configuration.channels.len(), 1);
        assert_eq!(configuration.channels[0].key, DEFAULT_EMBEDDING_CHANNEL_KEY);
        assert_eq!(configuration.rerank_channels.len(), 1);
        assert_eq!(
            configuration.rerank_channels[0].key,
            DEFAULT_RERANK_CHANNEL_KEY
        );
        assert_eq!(
            configuration
                .active_rerank_channel()
                .map(|channel| channel.key.as_str()),
            Some(DEFAULT_RERANK_CHANNEL_KEY)
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 只配了嵌入渠道的旧文件也要能读：重排侧拿默认一条，不报错。
    #[test]
    fn legacy_file_without_rerank_section_still_loads() {
        let root = temp_root("legacy");
        let path = root
            .join(USER_CONFIG_DIRNAME)
            .join(crate::core::runtime::DEFAULT_EMBEDDING_MODELS_FILENAME);
        std::fs::write(
            &path,
            r#"version = 1
default_key = "old-embedding"

[channels.old-embedding]
name = "旧向量"
base_url = "https://api.openai.com/v1"
api_key = "sk-old"
model = "text-embedding-3-small"
dimensions = 0
batch_size = 64
timeout_seconds = 30
enabled = true
"#,
        )
        .expect("写旧格式配置");
        let configuration =
            load_embedding_model_configuration(&env_for(&root), None).expect("旧文件应当能读");
        assert_eq!(configuration.default_key, "old-embedding");
        assert_eq!(configuration.channels[0].api_key, "sk-old");
        assert_eq!(
            configuration.rerank_channels.len(),
            1,
            "缺段时重排侧补一条默认渠道"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 两组渠道各自往返：写回后再读，两条都在，互不覆盖。
    #[test]
    fn both_groups_survive_a_round_trip() {
        let root = temp_root("round-trip");
        let environment = env_for(&root);
        let mut configuration = EmbeddingModelConfiguration::default();
        configuration.channels[0].api_key = "sk-embed".to_string();
        configuration.rerank_channels[0].api_key = "sk-rerank".to_string();
        configuration.rerank_channels[0].name = "精排".to_string();
        save_embedding_model_configuration(&environment, &configuration, None)
            .expect("写入应当成功");

        let reloaded =
            load_embedding_model_configuration(&environment, None).expect("回读应当成功");
        assert_eq!(reloaded.channels.len(), 1);
        assert_eq!(reloaded.channels[0].api_key, "sk-embed");
        assert_eq!(reloaded.rerank_channels.len(), 1);
        assert_eq!(reloaded.rerank_channels[0].api_key, "sk-rerank");
        assert_eq!(reloaded.rerank_channels[0].name, "精排");
        assert_eq!(reloaded.rerank_default_key, DEFAULT_RERANK_CHANNEL_KEY);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 任一组的 key 都不能为空，且两组各查各的。
    #[test]
    fn empty_rerank_group_is_rejected() {
        let configuration = EmbeddingModelConfiguration {
            rerank_channels: Vec::new(),
            ..EmbeddingModelConfiguration::default()
        };
        let error = configuration.validate().expect_err("重排组不能为空");
        assert!(error.message().contains("重排序模型渠道"), "{}", error.message());
    }
}

