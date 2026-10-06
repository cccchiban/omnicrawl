//! 脱敏（Agent 网关消息脱敏）配置（对应 `omnicrawl/config/features/desensitization.py`）。
//!
//! 默认关闭：未启用时运行时不做任何包装（零成本）。字段与设计稿
//! `omnicrawl://docs/agent_gateway_desensitization_design.md` 的 `[desensitization]` 段一一对应。

use std::path::{Path, PathBuf};

use crate::core::runtime::{get_section, load_config_data, save_config_data, ConfigEnvironment};
use crate::error::ConfigError;
use crate::toml::{Table, Value};
use crate::value::python_str;

pub const DEFAULT_ENTROPY_MIN_LENGTH: i64 = 20;
pub const DEFAULT_ENTROPY_MIN_BITS: f64 = 3.5;

/// NER 兜底层的固定取值。
pub const NER_ENTITY_TYPES: [&str; 3] = ["PER", "ORG", "LOC"];
pub const NER_DEVICES: [&str; 3] = ["auto", "cpu", "cuda"];
pub const DEFAULT_NER_DEVICE: &str = "auto";
pub const DEFAULT_NER_MIN_ENTITY_CHARS: i64 = 2;
pub const DEFAULT_NER_CACHE_SIZE: i64 = 2048;

/// `{list(NER_ENTITY_TYPES)}` 的 Python `repr` 写法。
const NER_ENTITY_TYPES_REPR: &str = "['PER', 'ORG', 'LOC']";

/// 脱敏开关、失败策略与匹配参数。
#[derive(Debug, Clone, PartialEq)]
pub struct DesensitizationConfig {
    pub enabled: bool,
    pub fail_closed: bool,
    pub strict_restore: bool,
    pub extra_sensitive_keys: Vec<String>,
    pub exempt_keys: Vec<String>,
    pub entropy_enabled: bool,
    pub entropy_min_length: i64,
    pub entropy_min_bits: f64,
    pub entropy_pure_letters: bool,
    pub entropy_pure_digits: bool,
    pub detect_pem_private_key: bool,
    pub detect_db_connection_string: bool,
    pub detect_email: bool,
    pub detect_bank_card: bool,
    pub detect_internal_ip: bool,
    pub detect_external_ip: bool,
    pub detect_url: bool,
    pub detect_mac_address: bool,
    pub detect_license_plate: bool,
    pub gitleaks_enabled: bool,
    pub gitleaks_config_path: String,
    pub ner_enabled: bool,
    pub ner_model_path: String,
    pub ner_device: String,
    pub ner_entity_types: Vec<String>,
    pub ner_min_entity_chars: i64,
    pub ner_cache_size: i64,
}

impl Default for DesensitizationConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            fail_closed: true,
            strict_restore: false,
            extra_sensitive_keys: Vec::new(),
            exempt_keys: Vec::new(),
            entropy_enabled: true,
            entropy_min_length: DEFAULT_ENTROPY_MIN_LENGTH,
            entropy_min_bits: DEFAULT_ENTROPY_MIN_BITS,
            entropy_pure_letters: false,
            entropy_pure_digits: false,
            detect_pem_private_key: true,
            detect_db_connection_string: true,
            detect_email: false,
            detect_bank_card: true,
            detect_internal_ip: true,
            detect_external_ip: false,
            detect_url: false,
            detect_mac_address: true,
            detect_license_plate: true,
            gitleaks_enabled: true,
            gitleaks_config_path: String::new(),
            ner_enabled: false,
            ner_model_path: String::new(),
            ner_device: DEFAULT_NER_DEVICE.to_string(),
            ner_entity_types: NER_ENTITY_TYPES
                .iter()
                .map(|item| item.to_string())
                .collect(),
            ner_min_entity_chars: DEFAULT_NER_MIN_ENTITY_CHARS,
            ner_cache_size: DEFAULT_NER_CACHE_SIZE,
        }
    }
}

impl DesensitizationConfig {
    /// Python `__post_init__`：键名归一化、熵参数区间与 NER 字段校验。
    pub fn validate(mut self) -> Result<Self, ConfigError> {
        self.extra_sensitive_keys = normalized_keys(&self.extra_sensitive_keys);
        self.exempt_keys = normalized_keys(&self.exempt_keys);
        if self.entropy_min_length < 0 {
            return Err(ConfigError::new(
                "desensitization.entropy_min_length 必须是非负整数。",
            ));
        }
        if !(0.0..=8.0).contains(&self.entropy_min_bits) {
            return Err(ConfigError::new(
                "desensitization.entropy_min_bits 必须是 0–8 之间的数。",
            ));
        }
        self.gitleaks_config_path = self.gitleaks_config_path.trim().to_string();
        self.ner_model_path = self.ner_model_path.trim().to_string();
        let device = self.ner_device.trim().to_lowercase();
        if !NER_DEVICES.contains(&device.as_str()) {
            return Err(ConfigError::new(
                "desensitization.ner_device 必须是 auto / cpu / cuda 之一。",
            ));
        }
        self.ner_device = device;
        let entity_types = normalized_entity_types(&self.ner_entity_types);
        if entity_types.is_empty()
            || entity_types
                .iter()
                .any(|item| !NER_ENTITY_TYPES.contains(&item.as_str()))
        {
            return Err(ConfigError::new(format!(
                "desensitization.ner_entity_types 只能包含 {NER_ENTITY_TYPES_REPR}。"
            )));
        }
        self.ner_entity_types = entity_types;
        if self.ner_min_entity_chars < 1 {
            return Err(ConfigError::new(
                "desensitization.ner_min_entity_chars 必须是正整数。",
            ));
        }
        if self.ner_cache_size < 0 {
            return Err(ConfigError::new(
                "desensitization.ner_cache_size 必须是非负整数。",
            ));
        }
        Ok(self)
    }
}

/// 读取 `config.toml` 的 `[desensitization]` 段；缺失或为空时返回默认关闭配置。
pub fn load_desensitization_config(
    env: &ConfigEnvironment,
    config_path: Option<&Path>,
) -> Result<DesensitizationConfig, ConfigError> {
    let data = load_config_data(env, config_path)?;
    let section = get_section(&data, "desensitization")?;
    parse_desensitization_section(&section)
}

fn parse_desensitization_section(section: &Table) -> Result<DesensitizationConfig, ConfigError> {
    let defaults = DesensitizationConfig::default();
    DesensitizationConfig {
        enabled: bool_field(section, "enabled", false)?,
        fail_closed: bool_field(section, "fail_closed", true)?,
        strict_restore: bool_field(section, "strict_restore", false)?,
        extra_sensitive_keys: key_list_field(section, "extra_sensitive_keys")?,
        exempt_keys: key_list_field(section, "exempt_keys")?,
        entropy_enabled: bool_field(section, "entropy_enabled", true)?,
        entropy_min_length: int_field(section, "entropy_min_length", DEFAULT_ENTROPY_MIN_LENGTH)?,
        entropy_min_bits: float_field(section, "entropy_min_bits", DEFAULT_ENTROPY_MIN_BITS)?,
        entropy_pure_letters: bool_field(section, "entropy_pure_letters", false)?,
        entropy_pure_digits: bool_field(section, "entropy_pure_digits", false)?,
        detect_pem_private_key: bool_field(section, "detect_pem_private_key", true)?,
        detect_db_connection_string: bool_field(section, "detect_db_connection_string", true)?,
        detect_email: bool_field(section, "detect_email", false)?,
        detect_bank_card: bool_field(section, "detect_bank_card", true)?,
        detect_internal_ip: bool_field(section, "detect_internal_ip", true)?,
        detect_external_ip: bool_field(section, "detect_external_ip", false)?,
        detect_url: bool_field(section, "detect_url", false)?,
        detect_mac_address: bool_field(section, "detect_mac_address", true)?,
        detect_license_plate: bool_field(section, "detect_license_plate", true)?,
        gitleaks_enabled: bool_field(section, "gitleaks_enabled", true)?,
        gitleaks_config_path: str_field(section, "gitleaks_config_path", "")?,
        ner_enabled: bool_field(section, "ner_enabled", false)?,
        ner_model_path: str_field(section, "ner_model_path", "")?,
        ner_device: device_field(section, "ner_device", DEFAULT_NER_DEVICE)?,
        ner_entity_types: entity_types_field(section, "ner_entity_types")?,
        ner_min_entity_chars: int_field(
            section,
            "ner_min_entity_chars",
            DEFAULT_NER_MIN_ENTITY_CHARS,
        )?,
        ner_cache_size: int_field(section, "ner_cache_size", defaults.ner_cache_size)?,
    }
    .validate()
}

/// 把脱敏配置写回 `config.toml` 的 `[desensitization]` 段（保留其他段）。
pub fn save_desensitization_config(
    env: &ConfigEnvironment,
    config: &DesensitizationConfig,
    config_path: Option<&Path>,
) -> Result<PathBuf, ConfigError> {
    let mut data = load_config_data(env, config_path)?;
    let mut section = Table::new();
    put_bool(&mut section, "enabled", config.enabled);
    put_bool(&mut section, "fail_closed", config.fail_closed);
    put_bool(&mut section, "strict_restore", config.strict_restore);
    section.insert(
        "extra_sensitive_keys".to_string(),
        string_array(&config.extra_sensitive_keys),
    );
    section.insert("exempt_keys".to_string(), string_array(&config.exempt_keys));
    put_bool(&mut section, "entropy_enabled", config.entropy_enabled);
    section.insert(
        "entropy_min_length".to_string(),
        Value::Integer(config.entropy_min_length),
    );
    section.insert(
        "entropy_min_bits".to_string(),
        Value::Float(config.entropy_min_bits),
    );
    put_bool(
        &mut section,
        "entropy_pure_letters",
        config.entropy_pure_letters,
    );
    put_bool(
        &mut section,
        "entropy_pure_digits",
        config.entropy_pure_digits,
    );
    put_bool(
        &mut section,
        "detect_pem_private_key",
        config.detect_pem_private_key,
    );
    put_bool(
        &mut section,
        "detect_db_connection_string",
        config.detect_db_connection_string,
    );
    put_bool(&mut section, "detect_email", config.detect_email);
    put_bool(&mut section, "detect_bank_card", config.detect_bank_card);
    put_bool(
        &mut section,
        "detect_internal_ip",
        config.detect_internal_ip,
    );
    put_bool(
        &mut section,
        "detect_external_ip",
        config.detect_external_ip,
    );
    put_bool(&mut section, "detect_url", config.detect_url);
    put_bool(
        &mut section,
        "detect_mac_address",
        config.detect_mac_address,
    );
    put_bool(
        &mut section,
        "detect_license_plate",
        config.detect_license_plate,
    );
    put_bool(&mut section, "gitleaks_enabled", config.gitleaks_enabled);
    section.insert(
        "gitleaks_config_path".to_string(),
        Value::String(config.gitleaks_config_path.clone()),
    );
    put_bool(&mut section, "ner_enabled", config.ner_enabled);
    section.insert(
        "ner_model_path".to_string(),
        Value::String(config.ner_model_path.clone()),
    );
    section.insert(
        "ner_device".to_string(),
        Value::String(config.ner_device.clone()),
    );
    section.insert(
        "ner_entity_types".to_string(),
        string_array(&config.ner_entity_types),
    );
    section.insert(
        "ner_min_entity_chars".to_string(),
        Value::Integer(config.ner_min_entity_chars),
    );
    section.insert(
        "ner_cache_size".to_string(),
        Value::Integer(config.ner_cache_size),
    );
    data.insert("desensitization".to_string(), Value::Table(section));
    save_config_data(env, &data, config_path)
}

fn string_array(items: &[String]) -> Value {
    Value::Array(
        items
            .iter()
            .map(|item| Value::String(item.clone()))
            .collect(),
    )
}

fn put_bool(section: &mut Table, key: &str, value: bool) {
    section.insert(key.to_string(), Value::Boolean(value));
}

fn normalized_keys(items: &[String]) -> Vec<String> {
    items
        .iter()
        .map(|item| item.trim().to_string())
        .filter(|item| !item.is_empty())
        .collect()
}

fn normalized_entity_types(items: &[String]) -> Vec<String> {
    items
        .iter()
        .map(|item| item.trim().to_uppercase())
        .filter(|item| !item.is_empty())
        .collect()
}

fn bool_field(section: &Table, name: &str, default: bool) -> Result<bool, ConfigError> {
    match section.get(name) {
        None => Ok(default),
        Some(Value::Boolean(flag)) => Ok(*flag),
        Some(_) => Err(ConfigError::new(format!(
            "desensitization.{name} 必须是布尔值。"
        ))),
    }
}

fn key_list_field(section: &Table, name: &str) -> Result<Vec<String>, ConfigError> {
    match section.get(name) {
        None => Ok(Vec::new()),
        Some(Value::String(text)) if text.is_empty() => Ok(Vec::new()),
        Some(Value::Array(items)) => Ok(normalized_keys(
            &items
                .iter()
                .map(|item| python_str(Some(item)))
                .collect::<Vec<String>>(),
        )),
        Some(_) => Err(ConfigError::new(format!(
            "desensitization.{name} 必须是字符串数组。"
        ))),
    }
}

fn str_field(section: &Table, name: &str, default: &str) -> Result<String, ConfigError> {
    match section.get(name) {
        None => Ok(default.to_string()),
        Some(Value::String(text)) => Ok(text.trim().to_string()),
        Some(_) => Err(ConfigError::new(format!(
            "desensitization.{name} 必须是字符串路径。"
        ))),
    }
}

fn int_field(section: &Table, name: &str, default: i64) -> Result<i64, ConfigError> {
    match section.get(name) {
        None => Ok(default),
        Some(Value::Integer(value)) => Ok(*value),
        Some(_) => Err(ConfigError::new(format!(
            "desensitization.{name} 必须是非负整数。"
        ))),
    }
}

fn float_field(section: &Table, name: &str, default: f64) -> Result<f64, ConfigError> {
    match section.get(name) {
        None => Ok(default),
        Some(Value::Integer(value)) => Ok(*value as f64),
        Some(Value::Float(value)) => Ok(*value),
        Some(_) => Err(ConfigError::new(format!(
            "desensitization.{name} 必须是 0–8 之间的数。"
        ))),
    }
}

fn device_field(section: &Table, name: &str, default: &str) -> Result<String, ConfigError> {
    match section.get(name) {
        None => Ok(default.to_string()),
        Some(Value::String(text)) => {
            let device = text.trim().to_lowercase();
            if !NER_DEVICES.contains(&device.as_str()) {
                return Err(ConfigError::new(format!(
                    "desensitization.{name} 必须是 auto / cpu / cuda 之一。"
                )));
            }
            Ok(device)
        }
        Some(_) => Err(ConfigError::new(format!(
            "desensitization.{name} 必须是 auto / cpu / cuda 之一。"
        ))),
    }
}

fn entity_types_field(section: &Table, name: &str) -> Result<Vec<String>, ConfigError> {
    match section.get(name) {
        None => Ok(NER_ENTITY_TYPES
            .iter()
            .map(|item| item.to_string())
            .collect()),
        Some(Value::String(text)) if text.is_empty() => Ok(NER_ENTITY_TYPES
            .iter()
            .map(|item| item.to_string())
            .collect()),
        Some(Value::Array(items)) => Ok(normalized_entity_types(
            &items
                .iter()
                .map(|item| python_str(Some(item)))
                .collect::<Vec<String>>(),
        )),
        Some(_) => Err(ConfigError::new(format!(
            "desensitization.{name} 必须是字符串数组。"
        ))),
    }
}
