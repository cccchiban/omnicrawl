//! 图像生成（OpenAI 兼容 Image API）配置（对应 `omnicrawl/config/features/image_gen.py`）。

use std::path::{Path, PathBuf};

use crate::core::runtime::{load_config_data, save_config_data, ConfigEnvironment};
use crate::error::ConfigError;
use crate::toml::{Table, Value};
use crate::value::{python_str, truthy};

pub const DEFAULT_IMAGE_GEN_BASE_URL: &str = "https://api.openai.com/v1";
pub const DEFAULT_IMAGE_GEN_MODEL: &str = "gpt-image-2";
pub const DEFAULT_IMAGE_GEN_API_KEY_ENV: &str = "OPENAI_API_KEY";

const IMAGE_GEN_QUALITIES: [&str; 4] = ["auto", "low", "medium", "high"];
const IMAGE_GEN_FORMATS: [&str; 3] = ["png", "jpeg", "webp"];

/// 图像生成服务的开关与接口参数。
#[derive(Debug, Clone, PartialEq)]
pub struct ImageGenConfiguration {
    pub enabled: bool,
    pub base_url: String,
    pub api_key: String,
    pub api_key_env: String,
    pub model: String,
    pub size: String,
    pub quality: String,
    pub output_format: String,
    pub n: i64,
    pub timeout_seconds: i64,
}

impl Default for ImageGenConfiguration {
    fn default() -> Self {
        Self {
            enabled: false,
            base_url: DEFAULT_IMAGE_GEN_BASE_URL.to_string(),
            api_key: String::new(),
            api_key_env: DEFAULT_IMAGE_GEN_API_KEY_ENV.to_string(),
            model: DEFAULT_IMAGE_GEN_MODEL.to_string(),
            size: "auto".to_string(),
            quality: "auto".to_string(),
            output_format: "png".to_string(),
            n: 1,
            timeout_seconds: 120,
        }
    }
}

impl ImageGenConfiguration {
    /// Python `ImageGenConfiguration.__post_init__`：取值域校验与归一化。
    pub fn normalize(mut self) -> Result<Self, ConfigError> {
        let base_url = self.base_url.trim().trim_end_matches('/').to_string();
        if base_url.is_empty() {
            return Err(ConfigError::new("image_gen.base_url 不能为空。"));
        }
        if !base_url.starts_with("http://") && !base_url.starts_with("https://") {
            return Err(ConfigError::new(
                "image_gen.base_url 必须以 http:// 或 https:// 开头。",
            ));
        }
        let model = self.model.trim().to_string();
        if model.is_empty() {
            return Err(ConfigError::new("image_gen.model 不能为空。"));
        }
        if self.size != "auto" && !is_size_shape(&self.size) {
            return Err(ConfigError::new(
                "image_gen.size 必须是 auto 或 宽x高 形式（例如 1024x1024、1536x1024）。",
            ));
        }
        if !IMAGE_GEN_QUALITIES.contains(&self.quality.as_str()) {
            return Err(ConfigError::new(format!(
                "image_gen.quality 仅支持 {}。",
                IMAGE_GEN_QUALITIES.join("、")
            )));
        }
        if !IMAGE_GEN_FORMATS.contains(&self.output_format.as_str()) {
            return Err(ConfigError::new(format!(
                "image_gen.output_format 仅支持 {}。",
                IMAGE_GEN_FORMATS.join("、")
            )));
        }
        if !(1..=10).contains(&self.n) {
            return Err(ConfigError::new("image_gen.n 必须是 1~10 的整数。"));
        }
        if !(1..=600).contains(&self.timeout_seconds) {
            return Err(ConfigError::new(
                "image_gen.timeout_seconds 必须是 1~600 的整数。",
            ));
        }
        self.base_url = base_url;
        self.model = model;
        self.api_key = self.api_key.trim().to_string();
        let api_key_env = self.api_key_env.trim().to_string();
        self.api_key_env = if api_key_env.is_empty() {
            DEFAULT_IMAGE_GEN_API_KEY_ENV.to_string()
        } else {
            api_key_env
        };
        Ok(self)
    }

    /// 优先使用配置里的 `api_key`，否则按 `api_key_env` 读取环境变量。
    pub fn resolve_api_key(&self, env: &ConfigEnvironment) -> String {
        if !self.api_key.is_empty() {
            return self.api_key.clone();
        }
        env.get(&self.api_key_env).unwrap_or_default()
    }
}

/// 读取图像生成配置；缺少 `image_gen` 段时返回关闭的默认配置。
pub fn load_image_gen_configuration(
    env: &ConfigEnvironment,
    config_path: Option<&Path>,
) -> Result<ImageGenConfiguration, ConfigError> {
    let data = load_config_data(env, config_path)?;
    let section = match data.get("image_gen") {
        None => Table::new(),
        Some(Value::String(text)) if text.is_empty() => Table::new(),
        Some(Value::Table(section)) => section.clone(),
        Some(_) => return Err(ConfigError::new("配置段 image_gen 必须是对象。")),
    };
    let defaults = ImageGenConfiguration::default();
    ImageGenConfiguration {
        enabled: bool_field(&section, "enabled", false),
        base_url: text_field(&section, "base_url", DEFAULT_IMAGE_GEN_BASE_URL),
        api_key: text_field(&section, "api_key", ""),
        api_key_env: text_field(&section, "api_key_env", DEFAULT_IMAGE_GEN_API_KEY_ENV),
        model: text_field(&section, "model", DEFAULT_IMAGE_GEN_MODEL),
        size: text_field(&section, "size", "auto"),
        quality: text_field(&section, "quality", "auto"),
        output_format: text_field(&section, "output_format", "png"),
        n: int_field(&section, "n", defaults.n)?,
        timeout_seconds: int_field(&section, "timeout_seconds", defaults.timeout_seconds)?,
    }
    .normalize()
}

/// 保留其他配置段，只更新完整的图像生成配置。
pub fn save_image_gen_configuration(
    env: &ConfigEnvironment,
    configuration: &ImageGenConfiguration,
    config_path: Option<&Path>,
) -> Result<PathBuf, ConfigError> {
    let mut data = load_config_data(env, config_path)?;
    let mut section = Table::new();
    section.insert("enabled".to_string(), Value::Boolean(configuration.enabled));
    section.insert(
        "base_url".to_string(),
        Value::String(configuration.base_url.clone()),
    );
    section.insert(
        "api_key".to_string(),
        Value::String(configuration.api_key.clone()),
    );
    section.insert(
        "api_key_env".to_string(),
        Value::String(configuration.api_key_env.clone()),
    );
    section.insert(
        "model".to_string(),
        Value::String(configuration.model.clone()),
    );
    section.insert(
        "size".to_string(),
        Value::String(configuration.size.clone()),
    );
    section.insert(
        "quality".to_string(),
        Value::String(configuration.quality.clone()),
    );
    section.insert(
        "output_format".to_string(),
        Value::String(configuration.output_format.clone()),
    );
    section.insert("n".to_string(), Value::Integer(configuration.n));
    section.insert(
        "timeout_seconds".to_string(),
        Value::Integer(configuration.timeout_seconds),
    );
    data.insert("image_gen".to_string(), Value::Table(section));
    save_config_data(env, &data, config_path)
}

/// `^\d{2,4}x\d{2,4}$` 的等价匹配（ASCII 数字）。
fn is_size_shape(text: &str) -> bool {
    let Some((width, height)) = text.split_once('x') else {
        return false;
    };
    is_digit_run(width) && is_digit_run(height)
}

fn is_digit_run(text: &str) -> bool {
    let count = text.chars().count();
    (2..=4).contains(&count) && text.chars().all(|ch| ch.is_ascii_digit())
}

fn bool_field(section: &Table, name: &str, default: bool) -> bool {
    match section.get(name) {
        None => default,
        Some(value) => truthy(value),
    }
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

/// Python `int(section.get(name) or default)`。
fn int_field(section: &Table, name: &str, default: i64) -> Result<i64, ConfigError> {
    let Some(value) = section.get(name) else {
        return Ok(default);
    };
    if !truthy(value) {
        return Ok(default);
    }
    match value {
        Value::Integer(number) => Ok(*number),
        Value::Boolean(flag) => Ok(i64::from(*flag)),
        Value::Float(number) => Ok(*number as i64),
        Value::String(text) => match text.trim().parse::<i64>() {
            Ok(number) => Ok(number),
            Err(_) => Err(numeric_error(text)),
        },
        _ => Err(numeric_error(&python_str(Some(value)))),
    }
}

fn numeric_error(text: &str) -> ConfigError {
    ConfigError::new(format!(
        "配置段 image_gen 数值字段无效：invalid literal for int() with base 10: '{text}'"
    ))
}
