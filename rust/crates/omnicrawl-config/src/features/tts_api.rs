//! TTS 语音合成接口（OpenAI 兼容 `POST {base_url}/audio/speech`）配置。
//!
//! 与 `[tts]` 分成两段是刻意的：`[tts]` 的对映实现是 Python
//! `omnicrawl/config/features/tts.py`，它的读回结果与写回文本都被 parity 数据集
//! 逐字节钉住（见 `tests/features_extra_parity.rs` 的 `tts_save`），加字段就必须
//! 同时改 Python 侧。本段是 Rust 侧新增的合成后端（本地 ONNX 之外的另一种选择，
//! 形状对齐既有的 `[image_gen]`），Python 没有对应实现，因此自带 Rust-only 测试。

use std::path::{Path, PathBuf};

use crate::core::runtime::{load_config_data, save_config_data, ConfigEnvironment};
use crate::error::ConfigError;
use crate::toml::{Table, Value};
use crate::value::{python_str, truthy};

/// 默认接口地址：OpenAI 官方（任何兼容服务的地址都可以填这里）。
pub const DEFAULT_TTS_API_BASE_URL: &str = "https://api.openai.com/v1";
/// 默认模型：比 `tts-1` 更自然、支持后续扩展的语音指令。
pub const DEFAULT_TTS_API_MODEL: &str = "gpt-4o-mini-tts";
/// 默认音色：OpenAI 内置音色名。第三方兼容服务用自己的音色名覆盖即可。
pub const DEFAULT_TTS_API_VOICE: &str = "alloy";
/// 默认密钥环境变量名；沿用 OpenAI 约定，避免多配一份密钥。
pub const DEFAULT_TTS_API_KEY_ENV: &str = "OPENAI_API_KEY";

/// 允许的响应格式。只支持 `wav`：Windows 侧播放走 `winmm` 的 `PlaySoundW`
/// （只认 WAV），输出文件也按 WAV 解析时长/波形，其它格式要么引入解码器、
/// 要么只能落盘不能自检与播放。
const TTS_API_FORMATS: [&str; 1] = ["wav"];

/// 语音合成接口的开关与参数。
#[derive(Debug, Clone, PartialEq)]
pub struct TtsApiConfiguration {
    /// 是否走接口合成。`false` 时回落到本地 ONNX 推理（该构建未编译本地推理则报错）。
    pub enabled: bool,
    pub base_url: String,
    pub api_key: String,
    pub api_key_env: String,
    pub model: String,
    pub voice: String,
    pub response_format: String,
    /// 语速倍数，取值域与 OpenAI 一致（0.25~4.0）；1.0 时不发给服务端。
    pub speed: f64,
    pub timeout_seconds: i64,
}

impl Default for TtsApiConfiguration {
    fn default() -> Self {
        Self {
            // 默认走接口：本项目的发布构建默认不编译本地 ONNX（见 omnicrawl-tts 的
            // `onnx` feature），若不默认开启接口，用户打开 TTS 总开关后会直接失败。
            enabled: true,
            base_url: DEFAULT_TTS_API_BASE_URL.to_string(),
            api_key: String::new(),
            api_key_env: DEFAULT_TTS_API_KEY_ENV.to_string(),
            model: DEFAULT_TTS_API_MODEL.to_string(),
            voice: DEFAULT_TTS_API_VOICE.to_string(),
            response_format: "wav".to_string(),
            speed: 1.0,
            timeout_seconds: 120,
        }
    }
}

impl TtsApiConfiguration {
    /// 取值域校验与归一化（错误文案里用 `tts_api` 前缀，与段名一致）。
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
        let response_format = self.response_format.trim().to_lowercase();
        if !TTS_API_FORMATS.contains(&response_format.as_str()) {
            return Err(ConfigError::new(format!(
                "tts_api.response_format 仅支持 {}。",
                TTS_API_FORMATS.join("、")
            )));
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
        self.response_format = response_format;
        self.api_key = self.api_key.trim().to_string();
        let api_key_env = self.api_key_env.trim().to_string();
        self.api_key_env = if api_key_env.is_empty() {
            DEFAULT_TTS_API_KEY_ENV.to_string()
        } else {
            api_key_env
        };
        Ok(self)
    }

    /// 生效的 API Key：只认配置里的明文 `api_key`。
    pub fn resolve_api_key(&self, _env: &ConfigEnvironment) -> String {
        self.api_key.trim().to_string()
    }

    /// 合成接口的完整地址（`base_url` 末尾斜杠已在归一化时去掉）。
    pub fn speech_url(&self) -> String {
        format!("{}/audio/speech", self.base_url)
    }
}

/// 读取接口配置；缺少 `tts_api` 段时返回默认配置（默认开启接口合成）。
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
    let defaults = TtsApiConfiguration::default();
    TtsApiConfiguration {
        enabled: bool_field(&section, "enabled", defaults.enabled),
        base_url: text_field(&section, "base_url", DEFAULT_TTS_API_BASE_URL),
        api_key: text_field(&section, "api_key", ""),
        api_key_env: text_field(&section, "api_key_env", DEFAULT_TTS_API_KEY_ENV),
        model: text_field(&section, "model", DEFAULT_TTS_API_MODEL),
        voice: text_field(&section, "voice", DEFAULT_TTS_API_VOICE),
        response_format: text_field(&section, "response_format", "wav"),
        speed: float_field(&section, "speed", defaults.speed)?,
        timeout_seconds: int_field(&section, "timeout_seconds", defaults.timeout_seconds)?,
    }
    .normalize()
}

/// 保留其他配置段，只更新完整的接口配置。
pub fn save_tts_api_configuration(
    env: &ConfigEnvironment,
    configuration: &TtsApiConfiguration,
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
        "voice".to_string(),
        Value::String(configuration.voice.clone()),
    );
    section.insert(
        "response_format".to_string(),
        Value::String(configuration.response_format.clone()),
    );
    section.insert("speed".to_string(), Value::Float(configuration.speed));
    section.insert(
        "timeout_seconds".to_string(),
        Value::Integer(configuration.timeout_seconds),
    );
    data.insert("tts_api".to_string(), Value::Table(section));
    save_config_data(env, &data, config_path)
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

/// `speed` 是浮点字段：整数与浮点都接受，字符串按十进制解析（对齐前面几段的宽松口径）。
fn float_field(section: &Table, name: &str, default: f64) -> Result<f64, ConfigError> {
    let Some(value) = section.get(name) else {
        return Ok(default);
    };
    if !truthy(value) {
        return Ok(default);
    }
    match value {
        Value::Float(number) => Ok(*number),
        Value::Integer(number) => Ok(*number as f64),
        Value::Boolean(flag) => Ok(if *flag { 1.0 } else { 0.0 }),
        Value::String(text) => match text.trim().parse::<f64>() {
            Ok(number) => Ok(number),
            Err(_) => Err(float_error(text)),
        },
        _ => Err(float_error(&python_str(Some(value)))),
    }
}

fn numeric_error(text: &str) -> ConfigError {
    ConfigError::new(format!(
        "配置段 tts_api 数值字段无效：invalid literal for int() with base 10: '{text}'"
    ))
}

fn float_error(text: &str) -> ConfigError {
    ConfigError::new(format!(
        "配置段 tts_api.speed 无效：could not convert string to float: '{text}'"
    ))
}
