//! `models.toml` 读取、校验与写回。
//!
//! 对应 `omnicrawl/config/models/model_store.py`。凭据字段（`api_key` / `token` / `cookie` /
//! `authorization`）在模型条目里一律拒绝；`api_key_env` 才是允许的写法。

use std::path::{Path, PathBuf};

use omnicrawl_llm::ModelCapabilities;
use omnicrawl_protocol::{ModelIdentity, Protocol, Provider};

use crate::core::runtime::{
    atomic_write_text, dump_toml_text, load_raw_file, resolve_models_path,
    resolve_models_write_path, ConfigEnvironment,
};
use crate::error::ConfigError;
use crate::toml::{Table, Value};
use crate::value::{json_object_to_table, python_str, truthy};

/// 模型条目里禁止出现的凭据字段。
pub const FORBIDDEN_CREDENTIAL_FIELDS: [&str; 4] = ["api_key", "token", "cookie", "authorization"];

/// 写回能力时始终保留的键（与 Python 侧同一集合）。
const CAPABILITY_ALWAYS_WRITTEN: [&str; 5] = [
    "streaming",
    "tools",
    "reasoning",
    "vision",
    "parallel_tool_calls",
];

/// 自定义模型条目。
#[derive(Debug, Clone, PartialEq)]
pub struct CustomModelRecord {
    pub key: String,
    pub display_name: String,
    pub profile: String,
    pub model_id: String,
    pub protocol: Protocol,
    pub enabled: bool,
    pub aliases: Vec<String>,
    pub description: String,
    pub tags: Vec<String>,
    pub context_window_tokens: i64,
    pub max_output_tokens: i64,
    pub temperature: Option<f64>,
    pub native_vision: Option<bool>,
    pub capabilities: ModelCapabilities,
    pub provider_options: Table,
    pub sort_order: i64,
}

impl CustomModelRecord {
    /// 模型条目 → 完整模型描述（`ModelDescriptor.to_descriptor`）。
    pub fn to_descriptor(&self, provider: Provider) -> ModelDescriptor {
        let mut identity = ModelIdentity::new(
            self.profile.clone(),
            provider,
            self.protocol,
            self.model_id.clone(),
        );
        identity.catalog_key = self.key.clone();
        ModelDescriptor {
            identity,
            display_name: if self.display_name.is_empty() {
                self.key.clone()
            } else {
                self.display_name.clone()
            },
            capabilities: self.capabilities,
            context_window_tokens: self.context_window_tokens,
            max_output_tokens: self.max_output_tokens,
            temperature: self.temperature,
            aliases: self.aliases.clone(),
            description: self.description.clone(),
            tags: self.tags.clone(),
            provider_options: self.provider_options.clone(),
            source: "custom".to_string(),
            sort_order: self.sort_order,
            enabled: self.enabled,
        }
    }
}

/// 可构建 Runtime 的完整模型描述（对应 Python `registry.ModelDescriptor`）。
#[derive(Debug, Clone, PartialEq)]
pub struct ModelDescriptor {
    pub identity: ModelIdentity,
    pub display_name: String,
    pub capabilities: ModelCapabilities,
    pub context_window_tokens: i64,
    pub max_output_tokens: i64,
    pub temperature: Option<f64>,
    pub aliases: Vec<String>,
    pub description: String,
    pub tags: Vec<String>,
    pub provider_options: Table,
    pub source: String,
    pub sort_order: i64,
    pub enabled: bool,
}

/// `models.toml` 的内存视图。
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ModelStore {
    pub version: i64,
    pub models: Vec<CustomModelRecord>,
    pub path: Option<PathBuf>,
}

impl ModelStore {
    /// 按 key 查条目。
    pub fn by_key(&self, key: &str) -> Option<&CustomModelRecord> {
        self.models.iter().find(|item| item.key == key)
    }

    /// 先按 key、再按别名解析；别名撞车时报错而不是随便挑一个。
    pub fn resolve_alias(&self, token: &str) -> Result<Option<&CustomModelRecord>, ConfigError> {
        let needle = token.trim();
        if needle.is_empty() {
            return Ok(None);
        }
        if let Some(record) = self.by_key(needle) {
            return Ok(Some(record));
        }
        let matches: Vec<&CustomModelRecord> = self
            .models
            .iter()
            .filter(|item| item.aliases.iter().any(|alias| alias == needle))
            .collect();
        if matches.len() == 1 {
            return Ok(Some(matches[0]));
        }
        if matches.len() > 1 {
            let keys: Vec<&str> = matches.iter().map(|item| item.key.as_str()).collect();
            return Err(ConfigError::new(format!(
                "别名 {needle} 匹配到多个模型：{}",
                keys.join(", ")
            )));
        }
        Ok(None)
    }
}

/// 读取模型目录；文件不存在时返回只有版本号的空目录。
pub fn load_model_store(
    env: &ConfigEnvironment,
    models_path: Option<&Path>,
) -> Result<ModelStore, ConfigError> {
    let path = resolve_models_path(env, models_path)?;
    if !path.exists() {
        return Ok(ModelStore {
            version: 1,
            models: Vec::new(),
            path: Some(path),
        });
    }
    let data = load_raw_file(&path)?;
    parse_model_store(&data, Some(&path))
}

/// 解析模型目录数据。
pub fn parse_model_store(data: &Table, path: Option<&Path>) -> Result<ModelStore, ConfigError> {
    let version = match data.get("version") {
        None => 1,
        Some(Value::Integer(value)) if *value >= 1 => *value,
        Some(_) => return Err(ConfigError::new("models.toml 的 version 必须是正整数。")),
    };
    let raw_models = match data.get("models") {
        None => Table::new(),
        Some(Value::String(text)) if text.is_empty() => Table::new(),
        Some(Value::Table(table)) => table.clone(),
        Some(_) => return Err(ConfigError::new("models.toml 的 models 必须是对象。")),
    };

    let mut records: Vec<CustomModelRecord> = Vec::new();
    let mut alias_owners: Vec<(String, String)> = Vec::new();
    for (key, raw) in &raw_models {
        let record = parse_model_record(key, raw)?;
        if let Value::Table(table) = raw {
            for forbidden in FORBIDDEN_CREDENTIAL_FIELDS {
                if table.contains_key(forbidden) {
                    return Err(ConfigError::new(format!(
                        "models.toml 模型 {key} 不允许包含凭据字段 {forbidden}。"
                    )));
                }
            }
        }
        for alias in &record.aliases {
            if let Some((_, owner)) = alias_owners.iter().find(|(name, _)| name == alias) {
                if owner != &record.key {
                    return Err(ConfigError::new(format!(
                        "models.toml 别名冲突：{alias} 同时属于 {owner} 与 {}。",
                        record.key
                    )));
                }
            }
            alias_owners.push((alias.clone(), record.key.clone()));
        }
        records.push(record);
    }

    // Python 用稳定排序：同序时保持文件里的出现顺序。
    records.sort_by(|left, right| {
        left.sort_order
            .cmp(&right.sort_order)
            .then_with(|| {
                left.display_name
                    .to_lowercase()
                    .cmp(&right.display_name.to_lowercase())
            })
            .then_with(|| left.key.cmp(&right.key))
    });

    Ok(ModelStore {
        version,
        models: records,
        path: path.map(|item| item.to_path_buf()),
    })
}

/// 目录的 TOML 负载（写回与对照共用）。
pub fn store_payload(store: &ModelStore) -> Table {
    let mut models = Table::new();
    for record in &store.models {
        models.insert(record.key.clone(), Value::Table(record_payload(record)));
    }
    let mut payload = Table::new();
    payload.insert("version".to_string(), Value::Integer(store.version));
    payload.insert("models".to_string(), Value::Table(models));
    payload
}

/// 把模型目录写回 `models.toml`。
pub fn save_model_store(
    env: &ConfigEnvironment,
    store: &ModelStore,
    models_path: Option<&Path>,
) -> Result<PathBuf, ConfigError> {
    let path = resolve_models_write_path(env, models_path)?;
    atomic_write_text(&path, &dump_toml_text(&store_payload(store)))?;
    Ok(path)
}

fn record_payload(record: &CustomModelRecord) -> Table {
    let mut item = Table::new();
    item.insert(
        "display_name".to_string(),
        Value::String(record.display_name.clone()),
    );
    item.insert("profile".to_string(), Value::String(record.profile.clone()));
    item.insert(
        "model_id".to_string(),
        Value::String(record.model_id.clone()),
    );
    item.insert(
        "protocol".to_string(),
        Value::String(record.protocol.as_str().to_string()),
    );
    item.insert("enabled".to_string(), Value::Boolean(record.enabled));
    if !record.aliases.is_empty() {
        item.insert(
            "aliases".to_string(),
            Value::Array(record.aliases.iter().cloned().map(Value::String).collect()),
        );
    }
    if !record.description.is_empty() {
        item.insert(
            "description".to_string(),
            Value::String(record.description.clone()),
        );
    }
    if !record.tags.is_empty() {
        item.insert(
            "tags".to_string(),
            Value::Array(record.tags.iter().cloned().map(Value::String).collect()),
        );
    }
    if record.context_window_tokens > 0 {
        item.insert(
            "context_window_tokens".to_string(),
            Value::Integer(record.context_window_tokens),
        );
    }
    if record.max_output_tokens > 0 {
        item.insert(
            "max_output_tokens".to_string(),
            Value::Integer(record.max_output_tokens),
        );
    }
    if let Some(temperature) = record.temperature {
        item.insert("temperature".to_string(), Value::Float(temperature));
    }
    if let Some(flag) = record.native_vision {
        item.insert("native_vision".to_string(), Value::Boolean(flag));
    }
    let caps = json_object_to_table(&serde_json::Value::Object(record.capabilities.to_map()));
    // 只写「有意义的非默认能力」，其余保持文件简洁。
    let meaningful: Vec<String> = caps
        .iter()
        .filter(|(key, value)| capability_is_meaningful(key, value))
        .map(|(key, _)| key.clone())
        .collect();
    if !meaningful.is_empty() {
        let mut written = Table::new();
        for (key, value) in &caps {
            if meaningful.contains(key) || CAPABILITY_ALWAYS_WRITTEN.contains(&key.as_str()) {
                written.insert(key.clone(), value.clone());
            }
        }
        item.insert("capabilities".to_string(), Value::Table(written));
    }
    if !record.provider_options.is_empty() {
        item.insert(
            "provider_options".to_string(),
            Value::Table(record.provider_options.clone()),
        );
    }
    if record.sort_order != 0 {
        item.insert("sort_order".to_string(), Value::Integer(record.sort_order));
    }
    item
}

/// Python：`v not in (False, 0, "", None) or k in {"streaming","tools"} and v is True`。
fn capability_is_meaningful(_key: &str, value: &Value) -> bool {
    truthy(value)
}

/// 模型 key：只允许小写字母、数字、`-` 与 `_`，且不能包含 `/`。
pub fn is_valid_model_key(key: &str) -> bool {
    if key.contains('/') {
        return false;
    }
    let mut chars = key.chars();
    match chars.next() {
        Some(first) if first.is_ascii_lowercase() || first.is_ascii_digit() => {}
        _ => return false,
    }
    chars.all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_' || ch == '-')
}

fn parse_model_record(key: &str, raw: &Value) -> Result<CustomModelRecord, ConfigError> {
    if !is_valid_model_key(key) {
        return Err(ConfigError::new(format!(
            "模型 key 非法：{key}。仅允许小写字母、数字、'-' 与 '_'，且不能包含 '/'。"
        )));
    }
    let table = match raw {
        Value::Table(table) => table,
        _ => return Err(ConfigError::new(format!("模型 {key} 必须是对象。"))),
    };

    let profile = python_str(table.get("profile")).trim().to_string();
    let model_id = python_str(table.get("model_id")).trim().to_string();
    let protocol_text = python_str(table.get("protocol")).trim().to_string();
    if profile.is_empty() {
        return Err(ConfigError::new(format!("模型 {key} 缺少 profile。")));
    }
    if model_id.is_empty() {
        return Err(ConfigError::new(format!("模型 {key} 缺少 model_id。")));
    }
    let protocol = Protocol::parse(&protocol_text).ok_or_else(|| {
        ConfigError::new(format!("模型 {key} 的 protocol 不支持：{protocol_text}"))
    })?;

    let display_name = {
        let raw = python_str(table.get("display_name"));
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            key.to_string()
        } else {
            trimmed.to_string()
        }
    };

    let enabled = match table.get("enabled") {
        None => true,
        Some(Value::Boolean(flag)) => *flag,
        Some(_) => {
            return Err(ConfigError::new(format!(
                "模型 {key} 的 enabled 必须是布尔值。"
            )))
        }
    };

    let aliases = string_list(table.get("aliases"), &format!("模型 {key} 的 aliases"))?;
    let tags = string_list(table.get("tags"), &format!("模型 {key} 的 tags"))?;
    let description = python_str(table.get("description")).trim().to_string();

    let context_window_tokens = positive_int(
        table.get("context_window_tokens"),
        &format!("模型 {key}.context_window_tokens"),
    )?;
    let mut max_output_tokens = positive_int(
        table.get("max_output_tokens"),
        &format!("模型 {key}.max_output_tokens"),
    )?;
    let mut temperature =
        optional_temperature(table.get("temperature"), &format!("模型 {key}.temperature"))?;

    let sort_order = match table.get("sort_order") {
        None => 0,
        Some(Value::Integer(value)) => *value,
        Some(_) => {
            return Err(ConfigError::new(format!(
                "模型 {key}.sort_order 必须是整数。"
            )))
        }
    };

    let mut capabilities = ModelCapabilities::from_mapping(
        &table
            .get("capabilities")
            .map(crate::value::toml_to_json)
            .unwrap_or(serde_json::Value::Null),
    );
    // 顶层 max_output_tokens 优先，其次 capabilities 中的值。
    if max_output_tokens <= 0 && capabilities.max_output_tokens > 0 {
        max_output_tokens = capabilities.max_output_tokens;
    }
    if context_window_tokens > 0 && capabilities.context_window_tokens <= 0 {
        let merged_max = if max_output_tokens > 0 {
            max_output_tokens
        } else {
            capabilities.max_output_tokens
        };
        capabilities = ModelCapabilities {
            context_window_tokens,
            max_output_tokens: merged_max,
            ..capabilities
        };
    } else if max_output_tokens > 0 && capabilities.max_output_tokens != max_output_tokens {
        capabilities = ModelCapabilities {
            context_window_tokens: if capabilities.context_window_tokens > 0 {
                capabilities.context_window_tokens
            } else {
                context_window_tokens
            },
            max_output_tokens,
            ..capabilities
        };
    }

    let native_vision = match table.get("native_vision") {
        None => None,
        Some(Value::Boolean(flag)) => Some(*flag),
        Some(_) => {
            return Err(ConfigError::new(format!(
                "模型 {key}.native_vision 必须是布尔值。"
            )))
        }
    };

    let provider_options = match table.get("provider_options") {
        None => Table::new(),
        Some(Value::Table(options)) => options.clone(),
        Some(_) => {
            return Err(ConfigError::new(format!(
                "模型 {key}.provider_options 必须是对象。"
            )))
        }
    };

    // 允许把 temperature 写在 provider_options 中，但顶层字段优先。
    if temperature.is_none() {
        if let Some(value) = provider_options.get("temperature") {
            temperature = optional_temperature(
                Some(value),
                &format!("模型 {key}.provider_options.temperature"),
            )?;
        }
    }

    Ok(CustomModelRecord {
        key: key.to_string(),
        display_name,
        profile,
        model_id,
        protocol,
        enabled,
        aliases,
        description,
        tags,
        context_window_tokens,
        max_output_tokens,
        temperature,
        native_vision,
        capabilities,
        provider_options,
        sort_order,
    })
}

/// Python `_positive_int`：缺省与空串算 0，布尔与负数报错。
fn positive_int(value: Option<&Value>, label: &str) -> Result<i64, ConfigError> {
    let Some(value) = value else {
        return Ok(0);
    };
    if let Value::String(text) = value {
        if text.is_empty() {
            return Ok(0);
        }
    }
    match value {
        Value::Integer(number) if *number >= 0 => Ok(*number),
        _ => Err(ConfigError::new(format!("{label} 必须是非负整数。"))),
    }
}

/// Python `_optional_temperature`：空值表示厂商默认。
fn optional_temperature(value: Option<&Value>, label: &str) -> Result<Option<f64>, ConfigError> {
    let Some(value) = value else {
        return Ok(None);
    };
    match value {
        Value::String(text) if text.is_empty() => Ok(None),
        Value::Integer(number) => Ok(Some(*number as f64)),
        Value::Float(number) => Ok(Some(*number)),
        _ => Err(ConfigError::new(format!("{label} 必须是数字。"))),
    }
}

fn string_list(value: Option<&Value>, label: &str) -> Result<Vec<String>, ConfigError> {
    let items = match value {
        None => return Ok(Vec::new()),
        Some(Value::String(text)) if text.is_empty() => return Ok(Vec::new()),
        Some(Value::Array(items)) => items.clone(),
        Some(_) => return Err(ConfigError::new(format!("{label} 必须是列表。"))),
    };
    let mut result: Vec<String> = Vec::new();
    for item in &items {
        let text = python_str(Some(item));
        let text = text.trim();
        if !text.is_empty() {
            result.push(text.to_string());
        }
    }
    Ok(result)
}

/// 从 protocol 前缀推断 provider（对应 Python `provider_from_protocol`）。
pub fn provider_from_protocol(protocol: &str) -> Result<Provider, ConfigError> {
    if protocol.starts_with("openai_") {
        return Ok(Provider::Openai);
    }
    if protocol.starts_with("anthropic_") {
        return Ok(Provider::Anthropic);
    }
    if protocol.starts_with("gemini_") {
        return Ok(Provider::Gemini);
    }
    Err(ConfigError::new(format!(
        "无法从 protocol 推断 provider：{protocol}"
    )))
}
