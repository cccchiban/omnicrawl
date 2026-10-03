//! TTS（MOSS-TTS-Nano ONNX）配置的读取、校验与写回（对应 `omnicrawl/config/features/tts.py`）。

use std::path::{Path, PathBuf};

use crate::core::runtime::{
    absolute_path, expand_user, load_config_data, save_config_data, ConfigEnvironment,
};
use crate::error::ConfigError;
use crate::toml::{Table, Value};
use crate::value::{python_str, truthy};

pub const DEFAULT_TTS_VOICE: &str = "Junhao";
pub const DEFAULT_TTS_OUTPUT_DIR: &str = ".omnicrawl/.agent_tmp/tts";

const TTS_THREAD_COUNT_OPTIONS: [i64; 4] = [1, 2, 4, 8];

/// TTS 开关与参数。
#[derive(Debug, Clone, PartialEq)]
pub struct TtsConfiguration {
    pub enabled: bool,
    pub model_dir: String,
    pub voice: String,
    pub auto_play: bool,
    pub thread_count: i64,
    pub device: String,
    pub streaming: bool,
    pub output_dir: String,
}

impl Default for TtsConfiguration {
    fn default() -> Self {
        Self {
            enabled: false,
            model_dir: String::new(),
            voice: DEFAULT_TTS_VOICE.to_string(),
            auto_play: true,
            thread_count: 4,
            device: "auto".to_string(),
            streaming: true,
            output_dir: DEFAULT_TTS_OUTPUT_DIR.to_string(),
        }
    }
}

impl TtsConfiguration {
    /// Python `TTSConfiguration.__post_init__`：归一化并校验取值域。
    pub fn normalize(mut self) -> Result<Self, ConfigError> {
        let device = self.device.trim().to_lowercase();
        if device != "auto" && device != "cpu" && device != "cuda" {
            return Err(ConfigError::new("tts.device 必须是 auto/cpu/cuda 之一。"));
        }
        let voice = self.voice.trim().to_string();
        if voice.is_empty() {
            return Err(ConfigError::new("tts.voice 不能为空。"));
        }
        if !TTS_THREAD_COUNT_OPTIONS.contains(&self.thread_count) {
            return Err(ConfigError::new("tts.thread_count 必须是 1/2/4/8。"));
        }
        let output_dir = self.output_dir.trim().to_string();
        if output_dir.is_empty() {
            return Err(ConfigError::new("tts.output_dir 不能为空。"));
        }
        self.device = device;
        self.voice = voice;
        self.model_dir = self.model_dir.trim().to_string();
        self.output_dir = output_dir;
        Ok(self)
    }

    /// 解析模型目录：优先配置值，其次默认目录。
    pub fn resolved_model_dir(&self, env: &ConfigEnvironment) -> PathBuf {
        if self.model_dir.is_empty() {
            return default_tts_model_dir(env);
        }
        absolute_path(&expand_user(env, &self.model_dir))
    }
}

/// 默认模型目录：`~/.omnicrawl/tts/models`。
pub fn default_tts_model_dir(env: &ConfigEnvironment) -> PathBuf {
    env.home().join(".omnicrawl").join("tts").join("models")
}

/// 读取 TTS 配置；缺少 `tts` 段时返回关闭的默认配置。
pub fn load_tts_configuration(
    env: &ConfigEnvironment,
    config_path: Option<&Path>,
) -> Result<TtsConfiguration, ConfigError> {
    let data = load_config_data(env, config_path)?;
    let section = match data.get("tts") {
        None => Table::new(),
        Some(Value::String(text)) if text.is_empty() => Table::new(),
        Some(Value::Table(section)) => section.clone(),
        Some(_) => return Err(ConfigError::new("配置段 tts 必须是对象。")),
    };
    let defaults = TtsConfiguration::default();
    TtsConfiguration {
        enabled: bool_field(&section, "enabled", false),
        model_dir: text_field(&section, "model_dir", ""),
        voice: text_field(&section, "voice", DEFAULT_TTS_VOICE),
        auto_play: bool_field(&section, "auto_play", true),
        thread_count: int_field(&section, "thread_count", defaults.thread_count)?,
        device: text_field(&section, "device", "auto"),
        streaming: bool_field(&section, "streaming", true),
        output_dir: text_field(&section, "output_dir", DEFAULT_TTS_OUTPUT_DIR),
    }
    .normalize()
}

/// 保留其他配置段，只更新完整的 TTS 配置。
pub fn save_tts_configuration(
    env: &ConfigEnvironment,
    configuration: &TtsConfiguration,
    config_path: Option<&Path>,
) -> Result<PathBuf, ConfigError> {
    let mut data = load_config_data(env, config_path)?;
    let mut section = Table::new();
    section.insert("enabled".to_string(), Value::Boolean(configuration.enabled));
    section.insert(
        "model_dir".to_string(),
        Value::String(configuration.model_dir.clone()),
    );
    section.insert(
        "voice".to_string(),
        Value::String(configuration.voice.clone()),
    );
    section.insert(
        "auto_play".to_string(),
        Value::Boolean(configuration.auto_play),
    );
    section.insert(
        "thread_count".to_string(),
        Value::Integer(configuration.thread_count),
    );
    section.insert(
        "device".to_string(),
        Value::String(configuration.device.clone()),
    );
    section.insert(
        "streaming".to_string(),
        Value::Boolean(configuration.streaming),
    );
    section.insert(
        "output_dir".to_string(),
        Value::String(configuration.output_dir.clone()),
    );
    data.insert("tts".to_string(), Value::Table(section));
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

/// Python `int(section.get(name) or default)`：falsy 落默认值，字符串按十进制解析。
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
        "配置段 tts 数值字段无效：invalid literal for int() with base 10: '{text}'"
    ))
}
