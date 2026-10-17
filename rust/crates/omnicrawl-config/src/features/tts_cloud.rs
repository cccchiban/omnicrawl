//! 云端 TTS 的配置：服务商路由（`[tts_cloud]`）与各服务商的接口参数。
//!
//! 三段分开是因为职责不同：`[tts]` 只有与后端无关的通用键，`[tts_cloud]` 决定这次合成
//! 走哪家（与 `[web_search].engine` 同款的口径），`[tts_api]` / `[tts_gemini]` 各自持有
//! 一家的地址、凭据、模型与音色。切换服务商不会丢掉另一家的配置。
//!
//! 凭据只认段里的明文 `api_key`（在设置面板直接填写并写回 config.toml），不再有读环境变量
//! 的通道；朗读语言也不再配置：两家都按输入文本自动判断，Gemini 不下发 `languageCode`。
//!
//! 这三段都是 Rust 侧新增，Python 没有对映实现，因此不进 parity 数据集，
//! 自带 `tests/tts_cloud_config.rs`。

use std::path::{Path, PathBuf};

use crate::core::runtime::{load_config_data, save_config_data, ConfigEnvironment};
use crate::error::ConfigError;
use crate::toml::{Table, Value};
use crate::value::{python_str, truthy};

/// 可选服务商：OpenAI 的 `audio/speech` 与 Gemini 的 TTS 模型。
pub const TTS_PROVIDERS: [&str; 2] = ["openai", "gemini"];

/// 默认服务商。
pub const DEFAULT_TTS_PROVIDER: &str = "openai";

pub const DEFAULT_TTS_API_BASE_URL: &str = "https://api.openai.com/v1";
/// 比 `tts-1` 更自然，且支持用 `instructions` 控制语气。
pub const DEFAULT_TTS_API_MODEL: &str = "gpt-4o-mini-tts";
pub const DEFAULT_TTS_API_VOICE: &str = "alloy";

pub const DEFAULT_TTS_GEMINI_BASE_URL: &str = "https://generativelanguage.googleapis.com/v1beta";
pub const DEFAULT_TTS_GEMINI_MODEL: &str = "gemini-2.5-flash-preview-tts";
pub const DEFAULT_TTS_GEMINI_VOICE: &str = "Kore";

const DEFAULT_TIMEOUT_SECONDS: i64 = 120;

/// 服务商路由。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TtsCloudConfiguration {
    pub provider: String,
}

impl Default for TtsCloudConfiguration {
    fn default() -> Self {
        Self {
            provider: DEFAULT_TTS_PROVIDER.to_string(),
        }
    }
}

impl TtsCloudConfiguration {
    pub fn normalize(mut self) -> Result<Self, ConfigError> {
        let provider = self.provider.trim().to_lowercase();
        let provider = if provider.is_empty() {
            DEFAULT_TTS_PROVIDER.to_string()
        } else {
            provider
        };
        if !TTS_PROVIDERS.contains(&provider.as_str()) {
            return Err(ConfigError::new(format!(
                "tts_cloud.provider 仅支持 {}。",
                TTS_PROVIDERS.join("、")
            )));
        }
        self.provider = provider;
        Ok(self)
    }
}

/// OpenAI 兼容的 `POST {base_url}/audio/speech` 参数。
#[derive(Debug, Clone, PartialEq)]
pub struct TtsApiConfiguration {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub voice: String,
    /// 语气指令；只对 `gpt-4o-mini-tts` 一类模型生效，留空则不下发。
    pub instructions: String,
    /// 语速倍数（0.25~4.0）；1.0 时不下发。
    pub speed: f64,
    pub timeout_seconds: i64,
}

impl Default for TtsApiConfiguration {
    fn default() -> Self {
        Self {
            base_url: DEFAULT_TTS_API_BASE_URL.to_string(),
            api_key: String::new(),
            model: DEFAULT_TTS_API_MODEL.to_string(),
            voice: DEFAULT_TTS_API_VOICE.to_string(),
            instructions: String::new(),
            speed: 1.0,
            timeout_seconds: DEFAULT_TIMEOUT_SECONDS,
        }
    }
}

impl TtsApiConfiguration {
    pub fn normalize(mut self) -> Result<Self, ConfigError> {
        let base_url = self.base_url.trim().trim_end_matches('/').to_string();
        if base_url.is_empty() {
            return Err(ConfigError::new("tts_api.base_url 不能为空。"));
        }
        if !base_url.starts_with("http://") && !base_url.starts_with("https://") {
            return Err(ConfigError::new(
                "tts_api.base_url 必须以 http:// 或 https:// 开头。",
            ));
        }
        let model = self.model.trim().to_string();
        if model.is_empty() {
            return Err(ConfigError::new("tts_api.model 不能为空。"));
        }
        let voice = self.voice.trim().to_string();
        if voice.is_empty() {
            return Err(ConfigError::new("tts_api.voice 不能为空。"));
        }
        if !self.speed.is_finite() || !(0.25..=4.0).contains(&self.speed) {
            return Err(ConfigError::new(
                "tts_api.speed 必须是 0.25~4.0 之间的数值。",
            ));
        }
        if !(1..=600).contains(&self.timeout_seconds) {
            return Err(ConfigError::new(
                "tts_api.timeout_seconds 必须是 1~600 的整数。",
            ));
        }
        self.base_url = base_url;
        self.model = model;
        self.voice = voice;
        self.instructions = self.instructions.trim().to_string();
        self.api_key = self.api_key.trim().to_string();
        Ok(self)
    }

    pub fn speech_url(&self) -> String {
        format!("{}/audio/speech", self.base_url)
    }
}

/// Gemini 的 `generateContent`（TTS 模型）参数。
#[derive(Debug, Clone, PartialEq)]
pub struct GeminiTtsConfiguration {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    /// 预置音色名（如 `Kore`、`Puck`）。
    pub voice: String,
    /// 语气指令，作为文本前缀交给模型（Gemini 没有独立的指令字段）。
    pub instructions: String,
    pub timeout_seconds: i64,
}

impl Default for GeminiTtsConfiguration {
    fn default() -> Self {
        Self {
            base_url: DEFAULT_TTS_GEMINI_BASE_URL.to_string(),
            api_key: String::new(),
            model: DEFAULT_TTS_GEMINI_MODEL.to_string(),
            voice: DEFAULT_TTS_GEMINI_VOICE.to_string(),
            instructions: String::new(),
            timeout_seconds: DEFAULT_TIMEOUT_SECONDS,
        }
    }
}

impl GeminiTtsConfiguration {
    pub fn normalize(mut self) -> Result<Self, ConfigError> {
        let base_url = self.base_url.trim().trim_end_matches('/').to_string();
        if base_url.is_empty() {
            return Err(ConfigError::new("tts_gemini.base_url 不能为空。"));
        }
        if !base_url.starts_with("http://") && !base_url.starts_with("https://") {
            return Err(ConfigError::new(
                "tts_gemini.base_url 必须以 http:// 或 https:// 开头。",
            ));
        }
        let model = self.model.trim().to_string();
        if model.is_empty() {
            return Err(ConfigError::new("tts_gemini.model 不能为空。"));
        }
        let voice = self.voice.trim().to_string();
        if voice.is_empty() {
            return Err(ConfigError::new("tts_gemini.voice 不能为空。"));
        }
        if !(1..=600).contains(&self.timeout_seconds) {
            return Err(ConfigError::new(
                "tts_gemini.timeout_seconds 必须是 1~600 的整数。",
            ));
        }
        self.base_url = base_url;
        self.model = model;
        self.voice = voice;
        self.instructions = self.instructions.trim().to_string();
        self.api_key = self.api_key.trim().to_string();
        Ok(self)
    }

    /// 合成地址：`{base_url}/models/{model}:generateContent`。
    pub fn generate_url(&self) -> String {
        format!("{}/models/{}:generateContent", self.base_url, self.model)
    }
}

/// 读取服务商路由；缺少 `tts_cloud` 段时返回默认服务商。
pub fn load_tts_cloud_configuration(
    env: &ConfigEnvironment,
    config_path: Option<&Path>,
) -> Result<TtsCloudConfiguration, ConfigError> {
    let data = load_config_data(env, config_path)?;
    let section = match data.get("tts_cloud") {
        None => Table::new(),
        Some(Value::String(text)) if text.is_empty() => Table::new(),
        Some(Value::Table(section)) => section.clone(),
        Some(_) => return Err(ConfigError::new("配置段 tts_cloud 必须是对象。")),
    };
    TtsCloudConfiguration {
        provider: text_field(&section, "provider", DEFAULT_TTS_PROVIDER),
    }
    .normalize()
}

/// 保留其他配置段，只更新服务商路由。
pub fn save_tts_cloud_configuration(
    env: &ConfigEnvironment,
    configuration: &TtsCloudConfiguration,
    config_path: Option<&Path>,
) -> Result<PathBuf, ConfigError> {
    let configuration = configuration.clone().normalize()?;
    let mut data = load_config_data(env, config_path)?;
    let mut section = Table::new();
    section.insert(
        "provider".to_string(),
        Value::String(configuration.provider.clone()),
    );
    data.insert("tts_cloud".to_string(), Value::Table(section));
    save_config_data(env, &data, config_path)
}

/// 读取 OpenAI 通道配置。
pub fn load_tts_api_configuration(
    env: &ConfigEnvironment,
    config_path: Option<&Path>,
) -> Result<TtsApiConfiguration, ConfigError> {
    let data = load_config_data(env, config_path)?;
    let section = match data.get("tts_api") {
        None => Table::new(),
        Some(Value::String(text)) if text.is_empty() => Table::new(),
        Some(Value::Table(section)) => section.clone(),
        Some(_) => return Err(ConfigError::new("配置段 tts_api 必须是对象。")),
    };
    TtsApiConfiguration {
        base_url: text_field(&section, "base_url", DEFAULT_TTS_API_BASE_URL),
        api_key: text_field(&section, "api_key", ""),
        model: text_field(&section, "model", DEFAULT_TTS_API_MODEL),
        voice: text_field(&section, "voice", DEFAULT_TTS_API_VOICE),
        instructions: text_field(&section, "instructions", ""),
        speed: float_field(&section, "speed", 1.0, "tts_api")?,
        timeout_seconds: int_field(
            &section,
            "timeout_seconds",
            DEFAULT_TIMEOUT_SECONDS,
            "tts_api",
        )?,
    }
    .normalize()
}

/// 保留其他配置段，只更新 OpenAI 通道配置。
pub fn save_tts_api_configuration(
    env: &ConfigEnvironment,
    configuration: &TtsApiConfiguration,
    config_path: Option<&Path>,
) -> Result<PathBuf, ConfigError> {
    let configuration = configuration.clone().normalize()?;
    let mut data = load_config_data(env, config_path)?;
    let mut section = Table::new();
    section.insert(
        "base_url".to_string(),
        Value::String(configuration.base_url.clone()),
    );
    section.insert(
        "api_key".to_string(),
        Value::String(configuration.api_key.clone()),
    );
    section.insert(
        "model".to_string(),
        Value::String(configuration.model.clone()),
    );
    section.insert(
        "voice".to_string(),
        Value::String(configuration.voice.clone()),
    );
    section.insert(
        "instructions".to_string(),
        Value::String(configuration.instructions.clone()),
    );
    section.insert("speed".to_string(), Value::Float(configuration.speed));
    section.insert(
        "timeout_seconds".to_string(),
        Value::Integer(configuration.timeout_seconds),
    );
    data.insert("tts_api".to_string(), Value::Table(section));
    save_config_data(env, &data, config_path)
}

/// 读取 Gemini 通道配置。
pub fn load_tts_gemini_configuration(
    env: &ConfigEnvironment,
    config_path: Option<&Path>,
) -> Result<GeminiTtsConfiguration, ConfigError> {
    let data = load_config_data(env, config_path)?;
    let section = match data.get("tts_gemini") {
        None => Table::new(),
        Some(Value::String(text)) if text.is_empty() => Table::new(),
        Some(Value::Table(section)) => section.clone(),
        Some(_) => return Err(ConfigError::new("配置段 tts_gemini 必须是对象。")),
    };
    GeminiTtsConfiguration {
        base_url: text_field(&section, "base_url", DEFAULT_TTS_GEMINI_BASE_URL),
        api_key: text_field(&section, "api_key", ""),
        model: text_field(&section, "model", DEFAULT_TTS_GEMINI_MODEL),
        voice: text_field(&section, "voice", DEFAULT_TTS_GEMINI_VOICE),
        instructions: text_field(&section, "instructions", ""),
        timeout_seconds: int_field(
            &section,
            "timeout_seconds",
            DEFAULT_TIMEOUT_SECONDS,
            "tts_gemini",
        )?,
    }
    .normalize()
}

/// 保留其他配置段，只更新 Gemini 通道配置。
pub fn save_tts_gemini_configuration(
    env: &ConfigEnvironment,
    configuration: &GeminiTtsConfiguration,
    config_path: Option<&Path>,
) -> Result<PathBuf, ConfigError> {
    let configuration = configuration.clone().normalize()?;
    let mut data = load_config_data(env, config_path)?;
    let mut section = Table::new();
    section.insert(
        "base_url".to_string(),
        Value::String(configuration.base_url.clone()),
    );
    section.insert(
        "api_key".to_string(),
        Value::String(configuration.api_key.clone()),
    );
    section.insert(
        "model".to_string(),
        Value::String(configuration.model.clone()),
    );
    section.insert(
        "voice".to_string(),
        Value::String(configuration.voice.clone()),
    );
    section.insert(
        "instructions".to_string(),
        Value::String(configuration.instructions.clone()),
    );
    section.insert(
        "timeout_seconds".to_string(),
        Value::Integer(configuration.timeout_seconds),
    );
    data.insert("tts_gemini".to_string(), Value::Table(section));
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

/// Python `int(section.get(name) or default)`。
fn int_field(
    section: &Table,
    name: &str,
    default: i64,
    prefix: &str,
) -> Result<i64, ConfigError> {
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
            Err(_) => Err(ConfigError::new(format!(
                "配置段 {prefix} 数值字段无效：invalid literal for int() with base 10: '{text}'"
            ))),
        },
        _ => Err(ConfigError::new(format!(
            "配置段 {prefix} 数值字段无效：invalid literal for int() with base 10: '{}'",
            python_str(Some(value))
        ))),
    }
}

/// 浮点字段：整数与浮点都接受，字符串按十进制解析。
fn float_field(
    section: &Table,
    name: &str,
    default: f64,
    prefix: &str,
) -> Result<f64, ConfigError> {
    let Some(value) = section.get(name) else {
        return Ok(default);
    };
    if !truthy(value) {
        return Ok(default);
    }
    let parsed = match value {
        Value::Float(number) => Some(*number),
        Value::Integer(number) => Some(*number as f64),
        Value::Boolean(flag) => Some(if *flag { 1.0 } else { 0.0 }),
        Value::String(text) => text.trim().parse::<f64>().ok(),
        _ => None,
    };
    parsed.ok_or_else(|| {
        ConfigError::new(format!(
            "配置段 {prefix}.{name} 无效：could not convert string to float: '{}'",
            python_str(Some(value))
        ))
    })
}
