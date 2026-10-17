//! TTS 总开关与通用参数的读写。
//!
//! 只保留与合成后端无关的键：开关、播放行为与输出目录。服务商选择在 `[tts_cloud]`，
//! 各服务商的接口参数在 `[tts_api]`（OpenAI）与 `[tts_gemini]`（Gemini）。
//!
//! 本地 Kokoro 时代的 `voice` / `model_dir` / `device` / `thread_count` 随本地推理一起删除：
//! 音色现在由服务商决定，因此放在各服务商自己的段里。

use std::path::{Path, PathBuf};

use crate::core::runtime::{load_config_data, save_config_data, ConfigEnvironment};
use crate::error::ConfigError;
use crate::toml::{Table, Value};
use crate::value::{python_str, truthy};

/// 默认输出目录（相对工作区）。
pub const DEFAULT_TTS_OUTPUT_DIR: &str = ".omnicrawl/.agent_tmp/tts";

/// TTS 开关与通用参数。
#[derive(Debug, Clone, PartialEq)]
pub struct TtsConfiguration {
    pub enabled: bool,
    pub auto_play: bool,
    /// 回合结束后朗读助手回复（不经过 `tts_synthesize` 工具，因此也不走审批）。
    pub speak_replies: bool,
    pub output_dir: String,
}

impl Default for TtsConfiguration {
    fn default() -> Self {
        Self {
            enabled: false,
            auto_play: true,
            speak_replies: false,
            output_dir: DEFAULT_TTS_OUTPUT_DIR.to_string(),
        }
    }
}

impl TtsConfiguration {
    /// 归一化并校验取值域。
    pub fn normalize(mut self) -> Result<Self, ConfigError> {
        let output_dir = self.output_dir.trim().to_string();
        if output_dir.is_empty() {
            return Err(ConfigError::new("tts.output_dir 不能为空。"));
        }
        self.output_dir = output_dir;
        Ok(self)
    }
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
    TtsConfiguration {
        enabled: bool_field(&section, "enabled", false),
        auto_play: bool_field(&section, "auto_play", true),
        speak_replies: bool_field(&section, "speak_replies", false),
        output_dir: text_field(&section, "output_dir", DEFAULT_TTS_OUTPUT_DIR),
    }
    .normalize()
}

/// 保留其他配置段，只更新整份 TTS 配置。
///
/// 写盘前先归一化：读取器（[`load_tts_configuration`]）要求 `output_dir` 非空，
/// 写出空目录会让整段配置在下次读取时失效。
pub fn save_tts_configuration(
    env: &ConfigEnvironment,
    configuration: &TtsConfiguration,
    config_path: Option<&Path>,
) -> Result<PathBuf, ConfigError> {
    let configuration = configuration.clone().normalize()?;
    let mut data = load_config_data(env, config_path)?;
    let mut section = Table::new();
    section.insert("enabled".to_string(), Value::Boolean(configuration.enabled));
    section.insert(
        "auto_play".to_string(),
        Value::Boolean(configuration.auto_play),
    );
    section.insert(
        "speak_replies".to_string(),
        Value::Boolean(configuration.speak_replies),
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
