//! TTS 引擎配置：`omnicrawl/tts/config.py` 的等价实现。

use std::path::PathBuf;

use crate::paths::resolve_lenient;

/// 默认模型目录：`~/.omnicrawl/tts/models`。
pub fn default_model_dir() -> PathBuf {
    home_directory()
        .join(".omnicrawl")
        .join("tts")
        .join("models")
}

/// 解析模型目录：未显式给出时回退默认目录。
pub fn resolve_model_dir(model_dir: Option<&str>) -> PathBuf {
    match model_dir.map(str::trim).filter(|value| !value.is_empty()) {
        None => default_model_dir(),
        Some(value) => resolve_lenient(&PathBuf::from(expand_user(value))),
    }
}

fn expand_user(path: &str) -> String {
    if path == "~" {
        return home_directory().to_string_lossy().to_string();
    }
    match path.strip_prefix("~/").or_else(|| path.strip_prefix("~\\")) {
        Some(rest) => home_directory().join(rest).to_string_lossy().to_string(),
        None => path.to_string(),
    }
}

fn home_directory() -> PathBuf {
    for name in ["USERPROFILE", "HOME"] {
        if let Ok(value) = std::env::var(name) {
            if !value.trim().is_empty() {
                return PathBuf::from(value);
            }
        }
    }
    PathBuf::from(".")
}

/// MOSS-TTS-Nano ONNX 推理配置：字段与 Python `TTSConfig` 一一对应。
#[derive(Debug, Clone, PartialEq)]
pub struct TtsConfig {
    /// 模型目录；None 时用 [`default_model_dir`]。
    pub model_dir: Option<PathBuf>,
    /// ONNX Runtime 线程数（intra-op）。
    pub thread_count: i64,
    /// 设备：`auto` 优先 CUDA、不可用回退 CPU；显式 `cuda` 不回退。
    pub device: Option<String>,
    /// 兼容旧 API 的 provider 参数；`device` 设置后优先。
    pub execution_provider: String,
    /// 采样模式：greedy / fixed / full。
    pub sample_mode: String,
    pub do_sample: bool,
    pub max_new_frames: i64,
    /// 内置音色名（未提供参考音频时使用）。
    pub voice: String,
    /// 语音克隆参考音频路径（提供时覆盖 voice）。
    pub prompt_audio_path: Option<PathBuf>,
    /// 输出音频目录（合成未指定路径时使用）。
    pub output_dir: PathBuf,
    /// 是否用 codec 流式解码。
    pub streaming: bool,
    /// 长文本按 token 预算分块。
    pub voice_clone_max_text_tokens: i64,
    /// WeTextProcessing 文本归一化（需要 pynini，Windows 上默认关闭）。
    pub enable_wetext: bool,
    /// 纯 Python 稳健文本清洗（此处为 Rust 实现）。
    pub enable_normalize_tts_text: bool,
    pub text_temperature: f64,
    pub text_top_p: f64,
    pub text_top_k: i64,
    pub audio_temperature: f64,
    pub audio_top_p: f64,
    pub audio_top_k: i64,
    pub audio_repetition_penalty: f64,
    pub seed: Option<i64>,
}

impl Default for TtsConfig {
    fn default() -> Self {
        Self {
            model_dir: None,
            thread_count: 4,
            device: Some("auto".to_string()),
            execution_provider: "cpu".to_string(),
            sample_mode: "fixed".to_string(),
            do_sample: true,
            max_new_frames: 375,
            voice: "Junhao".to_string(),
            prompt_audio_path: None,
            output_dir: PathBuf::from("generated_audio"),
            streaming: true,
            voice_clone_max_text_tokens: 75,
            enable_wetext: false,
            enable_normalize_tts_text: true,
            text_temperature: 1.0,
            text_top_p: 1.0,
            text_top_k: 50,
            audio_temperature: 0.8,
            audio_top_p: 0.95,
            audio_top_k: 25,
            audio_repetition_penalty: 1.2,
            seed: None,
        }
    }
}

impl TtsConfig {
    pub fn resolved_model_dir(&self) -> PathBuf {
        resolve_model_dir(self.model_dir.as_deref().and_then(|path| path.to_str()))
    }

    /// 写入 manifest `generation_defaults` 的采样参数覆盖（键序与 Python 一致）。
    pub fn generation_overrides(&self) -> serde_json::Value {
        serde_json::json!({
            "text_temperature": self.text_temperature,
            "text_top_p": self.text_top_p,
            "text_top_k": self.text_top_k,
            "audio_temperature": self.audio_temperature,
            "audio_top_p": self.audio_top_p,
            "audio_top_k": self.audio_top_k,
            "audio_repetition_penalty": self.audio_repetition_penalty,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_python_field_values() {
        let config = TtsConfig::default();
        assert_eq!(config.thread_count, 4);
        assert_eq!(config.voice, "Junhao");
        assert_eq!(config.max_new_frames, 375);
        assert_eq!(config.output_dir, PathBuf::from("generated_audio"));
        assert!(config.streaming);
        assert!(config.do_sample);
        assert!(!config.enable_wetext);
        assert!(config.enable_normalize_tts_text);
        assert_eq!(config.seed, None);

        let overrides = config.generation_overrides();
        assert_eq!(overrides["audio_repetition_penalty"], 1.2);
        assert_eq!(overrides["text_top_k"], 50);
    }

    #[test]
    fn model_directory_resolution_honours_overrides() {
        let explicit = resolve_model_dir(Some("~/tts-models"));
        assert!(!explicit.to_string_lossy().contains('~'));
        assert!(explicit.to_string_lossy().ends_with("tts-models"));
        assert!(default_model_dir().to_string_lossy().contains(".omnicrawl"));
    }
}
