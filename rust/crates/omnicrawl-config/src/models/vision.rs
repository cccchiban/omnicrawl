//! 视觉模型代理配置的读取、校验与写回。
//!
//! 对应 `omnicrawl/config/models/vision.py`：视觉代理的开关与有序故障转移模型引用，
//! 以及「模型原生视觉」三态开关（模型覆盖 > 渠道覆盖 > 未配置）。

use std::path::{Path, PathBuf};

use omnicrawl_protocol::Protocol;

use crate::core::runtime::{
    get_section, load_config_data, resolve_models_path, save_config_data, ConfigEnvironment,
};
use crate::error::ConfigError;
use crate::models::llm::ActiveModelRef;
use crate::models::model_store::{load_model_store, save_model_store};
use crate::toml::{Table, Value};

/// 模型条目与渠道 Profile 共用的原生视觉字段名。
pub const NATIVE_VISION_FIELD: &str = "native_vision";

/// 视觉模型代理的开关和有序故障转移模型引用。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct VisionConfiguration {
    pub enabled: bool,
    pub models: Vec<ActiveModelRef>,
}

impl VisionConfiguration {
    /// 与 Python `__post_init__` 同义的校验：开关必须是布尔、引用不能重复。
    pub fn validate(self) -> Result<Self, ConfigError> {
        let mut seen: Vec<Vec<(String, String)>> = Vec::new();
        for (index, reference) in self.models.iter().enumerate() {
            let identity = ref_identity(reference);
            if seen.contains(&identity) {
                return Err(ConfigError::new(format!(
                    "vision.models[{index}] 与前面的模型重复。"
                )));
            }
            seen.push(identity);
        }
        Ok(self)
    }
}

/// 当前模型「模型原生视觉」的覆盖值与来源。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct NativeVisionSetting {
    pub value: Option<bool>,
    /// model | channel | 空串（未配置）
    pub scope: String,
    pub label: String,
}

impl NativeVisionSetting {
    /// 界面上的来源说明。
    pub fn scope_text(&self) -> String {
        if self.scope == "model" {
            return format!("模型设置（{}）", self.label);
        }
        if self.scope == "channel" {
            return format!("渠道设置（{}）", self.label);
        }
        "未配置（按模型能力）".to_string()
    }
}

/// 读取视觉代理配置；缺少 `vision` 段时保持关闭。
pub fn load_vision_configuration(
    env: &ConfigEnvironment,
    config_path: Option<&Path>,
) -> Result<VisionConfiguration, ConfigError> {
    let data = load_config_data(env, config_path)?;
    let raw_section = match data.get("vision") {
        None => Table::new(),
        Some(Value::String(text)) if text.is_empty() => Table::new(),
        Some(Value::Table(table)) => table.clone(),
        Some(_) => return Err(ConfigError::new("配置段 vision 必须是对象。")),
    };
    let enabled = match raw_section.get("enabled") {
        None => false,
        Some(Value::Boolean(flag)) => *flag,
        Some(_) => return Err(ConfigError::new("vision.enabled 必须是布尔值。")),
    };
    let models = parse_model_refs(raw_section.get("models"))?;
    VisionConfiguration { enabled, models }.validate()
}

/// 保留其他配置段，只更新完整的视觉代理配置。
pub fn save_vision_configuration(
    env: &ConfigEnvironment,
    configuration: &VisionConfiguration,
    config_path: Option<&Path>,
) -> Result<PathBuf, ConfigError> {
    let mut data = load_config_data(env, config_path)?;
    let mut section = Table::new();
    section.insert("enabled".to_string(), Value::Boolean(configuration.enabled));
    section.insert(
        "models".to_string(),
        Value::Array(
            configuration
                .models
                .iter()
                .map(|reference| Value::Table(reference.to_table()))
                .collect(),
        ),
    );
    data.insert("vision".to_string(), Value::Table(section));
    save_config_data(env, &data, config_path)
}

/// 把 TOML 取值解析为三态开关；缺失或非法一律视为未配置。
pub fn parse_native_vision(raw: Option<&Value>) -> Option<bool> {
    match raw {
        Some(Value::Boolean(flag)) => Some(*flag),
        _ => None,
    }
}

/// 按「模型覆盖 > 渠道覆盖 > 未配置」读取当前模型的模型原生视觉设置。
pub fn resolve_native_vision(
    env: &ConfigEnvironment,
    catalog_key: &str,
    profile_id: &str,
    config_path: Option<&Path>,
    models_path: Option<&Path>,
) -> Result<NativeVisionSetting, ConfigError> {
    if let Some(value) = read_model_native_vision(env, catalog_key, models_path)? {
        return Ok(NativeVisionSetting {
            value: Some(value),
            scope: "model".to_string(),
            label: catalog_key.to_string(),
        });
    }
    if let Some(value) = read_channel_native_vision(env, profile_id, config_path)? {
        return Ok(NativeVisionSetting {
            value: Some(value),
            scope: "channel".to_string(),
            label: profile_id.to_string(),
        });
    }
    let label = if catalog_key.is_empty() {
        profile_id.to_string()
    } else {
        catalog_key.to_string()
    };
    Ok(NativeVisionSetting {
        value: None,
        scope: String::new(),
        label,
    })
}

/// 把模型原生视觉开关写回模型或渠道 TOML；`None` 表示删除该键。
pub fn save_native_vision(
    env: &ConfigEnvironment,
    value: Option<bool>,
    scope: &str,
    catalog_key: &str,
    profile_id: &str,
    config_path: Option<&Path>,
    models_path: Option<&Path>,
) -> Result<PathBuf, ConfigError> {
    if scope == "model" {
        if catalog_key.trim().is_empty() {
            return Err(ConfigError::new("缺少模型 key，无法写入模型原生视觉。"));
        }
        return write_model_native_vision(env, catalog_key, value, models_path);
    }
    if scope == "channel" {
        if profile_id.trim().is_empty() {
            return Err(ConfigError::new("缺少渠道标识，无法写入模型原生视觉。"));
        }
        return write_channel_native_vision(env, profile_id, value, config_path);
    }
    Err(ConfigError::new(
        "模型原生视觉必须写入范围仅支持 model 或 channel。",
    ))
}

fn ref_identity(reference: &ActiveModelRef) -> Vec<(String, String)> {
    let mut items: Vec<(String, String)> = reference
        .to_table()
        .iter()
        .map(|(key, value)| {
            let text = match value {
                Value::String(text) => text.clone(),
                other => crate::value::python_repr(other),
            };
            (key.clone(), text)
        })
        .collect();
    items.sort();
    items
}

fn parse_model_refs(raw: Option<&Value>) -> Result<Vec<ActiveModelRef>, ConfigError> {
    let items = match raw {
        None => return Ok(Vec::new()),
        Some(Value::String(text)) if text.is_empty() => return Ok(Vec::new()),
        Some(Value::Array(items)) => items.clone(),
        Some(_) => return Err(ConfigError::new("配置项 vision.models 必须是列表。")),
    };

    let mut refs: Vec<ActiveModelRef> = Vec::new();
    let mut seen: Vec<Vec<(String, String)>> = Vec::new();
    for (index, item) in items.iter().enumerate() {
        let table = match item {
            Value::Table(table) => table,
            _ => {
                return Err(ConfigError::new(format!(
                    "配置项 vision.models[{index}] 必须是对象。"
                )))
            }
        };
        let source = crate::value::python_str(table.get("source"))
            .trim()
            .to_lowercase();
        let reference = if source == "custom" {
            let key = crate::value::python_str(table.get("key"))
                .trim()
                .to_string();
            if key.is_empty() {
                return Err(ConfigError::new(format!(
                    "配置项 vision.models[{index}] 缺少 key。"
                )));
            }
            ActiveModelRef {
                source: "custom".to_string(),
                key,
                ..ActiveModelRef::default()
            }
        } else if source == "detected" {
            let profile = crate::value::python_str(table.get("profile"))
                .trim()
                .to_string();
            let model_id = crate::value::python_str(table.get("model_id"))
                .trim()
                .to_string();
            let protocol = crate::value::python_str(table.get("protocol"))
                .trim()
                .to_string();
            if profile.is_empty() || model_id.is_empty() {
                return Err(ConfigError::new(format!(
                    "配置项 vision.models[{index}] 必须包含 profile 和 model_id。"
                )));
            }
            if !protocol.is_empty() && Protocol::parse(&protocol).is_none() {
                return Err(ConfigError::new(format!(
                    "配置项 vision.models[{index}].protocol 不支持：{protocol}"
                )));
            }
            ActiveModelRef {
                source: "detected".to_string(),
                key: String::new(),
                profile,
                model_id,
                protocol,
            }
        } else {
            return Err(ConfigError::new(format!(
                "配置项 vision.models[{index}].source 仅支持 custom 或 detected。"
            )));
        };
        let identity = ref_identity(&reference);
        if seen.contains(&identity) {
            return Err(ConfigError::new(format!(
                "配置项 vision.models[{index}] 与前面的模型重复。"
            )));
        }
        seen.push(identity);
        refs.push(reference);
    }
    Ok(refs)
}

fn read_model_native_vision(
    env: &ConfigEnvironment,
    catalog_key: &str,
    models_path: Option<&Path>,
) -> Result<Option<bool>, ConfigError> {
    let key = catalog_key.trim();
    if key.is_empty() {
        return Ok(None);
    }
    let store = load_native_vision_store(env, models_path)?;
    match store.by_key(key) {
        None => Ok(None),
        Some(record) => Ok(record.native_vision),
    }
}

fn read_channel_native_vision(
    env: &ConfigEnvironment,
    profile_id: &str,
    config_path: Option<&Path>,
) -> Result<Option<bool>, ConfigError> {
    let key = profile_id.trim();
    if key.is_empty() {
        return Ok(None);
    }
    let data = load_native_vision_data(env, config_path)?;
    match channel_profile(&data, key) {
        None => Ok(None),
        Some(profile) => Ok(parse_native_vision(profile.get(NATIVE_VISION_FIELD))),
    }
}

fn write_model_native_vision(
    env: &ConfigEnvironment,
    catalog_key: &str,
    value: Option<bool>,
    models_path: Option<&Path>,
) -> Result<PathBuf, ConfigError> {
    let key = catalog_key.trim();
    let mut store = load_native_vision_store(env, models_path)?;
    if store.by_key(key).is_none() {
        return Err(ConfigError::new(format!("models.toml 中没有模型 {key}。")));
    }
    for record in store.models.iter_mut() {
        if record.key == key {
            record.native_vision = value;
        }
    }
    save_model_store(env, &store, models_path)
}

fn write_channel_native_vision(
    env: &ConfigEnvironment,
    profile_id: &str,
    value: Option<bool>,
    config_path: Option<&Path>,
) -> Result<PathBuf, ConfigError> {
    let key = profile_id.trim();
    let mut data = load_native_vision_data(env, config_path)?;
    let mut llm = get_section(&data, "llm")?;
    let mut profiles = get_section(&llm, "profiles")?;
    let mut profile = match profiles.get(key) {
        Some(Value::Table(table)) => table.clone(),
        _ => {
            return Err(ConfigError::new(format!(
                "config.toml 的 llm.profiles 中没有渠道 {key}。"
            )))
        }
    };
    match value {
        None => {
            profile.remove(NATIVE_VISION_FIELD);
        }
        Some(flag) => {
            profile.insert(NATIVE_VISION_FIELD.to_string(), Value::Boolean(flag));
        }
    }
    profiles.insert(key.to_string(), Value::Table(profile));
    llm.insert("profiles".to_string(), Value::Table(profiles));
    data.insert("llm".to_string(), Value::Table(llm));
    save_config_data(env, &data, config_path)
}

fn load_native_vision_data(
    env: &ConfigEnvironment,
    config_path: Option<&Path>,
) -> Result<Table, ConfigError> {
    load_config_data(env, config_path)
}

fn load_native_vision_store(
    env: &ConfigEnvironment,
    models_path: Option<&Path>,
) -> Result<crate::models::model_store::ModelStore, ConfigError> {
    let path = resolve_models_path(env, models_path)?;
    load_model_store(env, Some(&path))
}

fn channel_profile(data: &Table, profile_id: &str) -> Option<Table> {
    let llm = match data.get("llm") {
        Some(Value::Table(table)) => table,
        _ => return None,
    };
    let profiles = match llm.get("profiles") {
        Some(Value::Table(table)) => table,
        _ => return None,
    };
    match profiles.get(profile_id) {
        Some(Value::Table(table)) => Some(table.clone()),
        _ => None,
    }
}
