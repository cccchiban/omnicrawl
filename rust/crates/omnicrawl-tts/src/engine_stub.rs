//! 未编译本地推理时的 `engine` 占位实现。
//!
//! `omnicrawl-tts` 的 `onnx` feature 默认关闭（发布构建默认走接口合成，见 crate
//! 文档），此时 [`crate::engine`]（真实现）不存在。但宿主工具与 TUI 设置面板仍会
//! 引用 `TtsEngine`：这里给出**同名同签名**的最小面，全部返回明确错误，让调用方
//! 不必到处写 `cfg`，用户在界面上也能看到「为什么不能用本地推理」而不是链接错误。
//!
//! 新增消费方要调 `TtsEngine` 的其它方法时，要么在这里补一个同名桩，要么在调用点
//! 加 `#[cfg(feature = "onnx")]`——两者都比让发布构建编不出来好。

use std::path::Path;

use serde_json::Value;

use crate::config::TtsConfig;
pub use crate::result::TtsResult;

/// 未启用本地推理时的报错文案（宿主/界面直接展示）。
pub const LOCAL_ENGINE_UNAVAILABLE: &str =
    "本构建未启用本地 MOSS-TTS-Nano 推理（omnicrawl-tts 的 onnx feature）。\
     请改用 [tts_api] 接口合成，或用 `--features omnicrawl-tts/onnx` 重新构建。";

/// `crate::engine::TtsEngine` 的占位类型：构造即失败。
#[derive(Debug)]
pub struct TtsEngine {
    config: TtsConfig,
}

impl TtsEngine {
    /// 永远返回 [`LOCAL_ENGINE_UNAVAILABLE`]。
    pub fn new(_config: TtsConfig) -> Result<Self, String> {
        Err(LOCAL_ENGINE_UNAVAILABLE.to_string())
    }

    /// 与真实现同名，占位下不会走到（构造已失败），保留是为了调用方能编译。
    pub fn synthesize(
        &mut self,
        _text: &str,
        _voice: Option<&str>,
        _prompt_audio_path: Option<&Path>,
        _output_path: Option<&Path>,
        _sample_mode: Option<&str>,
        _do_sample: Option<bool>,
        _streaming: Option<bool>,
        _max_new_frames: Option<i64>,
        _voice_clone_max_text_tokens: Option<i64>,
        _seed: Option<i64>,
    ) -> Result<TtsResult, String> {
        Err(LOCAL_ENGINE_UNAVAILABLE.to_string())
    }

    /// 音色克隆同样只依赖本地推理，占位下直接报错。
    pub fn clone_voice(
        &mut self,
        _voice: &str,
        _reference_audio_path: &Path,
        _display_name: &str,
    ) -> Result<Value, String> {
        Err(LOCAL_ENGINE_UNAVAILABLE.to_string())
    }

    /// 与真实现一致：释放资源（占位下没有资源）。
    pub fn close(&mut self) {}

    /// 占位实现没有配置可读，返回默认值以免调用方 panic。
    pub fn list_available_voices(&self) -> Vec<Value> {
        Vec::new()
    }

    /// 占位实现没有配置可读，返回默认空配置。
    pub fn config_snapshot(&self) -> &TtsConfig {
        &self.config
    }
}
