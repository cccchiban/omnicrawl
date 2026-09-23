//! TTS（文本转语音）：`omnicrawl/tts/` 的 Rust 等价实现。
//!
//! 基于 MOSS-TTS-Nano（OpenMOSS）的 ONNX 推理版本：仅 0.1B 参数、不依赖 PyTorch，
//! 支持内置音色与参考音频语音克隆、长文本自动分块，输出 48kHz 立体声 WAV。
//!
//! 模块对映：
//!
//! | crate 模块 | Python 模块 |
//! |---|---|
//! | [`config`] | `omnicrawl/tts/config.py` |
//! | [`audio`] | `omnicrawl/tts/audio.py` |
//! | [`normalize`] | `omnicrawl/tts/normalize.py` |
//! | [`voices`] | `omnicrawl/tts/custom_voices.py` |
//! | [`download`] | `omnicrawl/tts/download.py` |
//! | [`tokenizer`] | `sentencepiece` 分词封装 |
//! | [`sampler`] | `omnicrawl/tts/onnx_runtime.py` 的采样与随机数 |
//! | [`runtime`] | `omnicrawl/tts/onnx_runtime.py` 的推理核心 |
//! | [`engine`] | `omnicrawl/tts/engine.py` |
//! | [`player`] | `omnicrawl/tts/player.py` |

pub mod audio;
pub mod cli;
pub mod config;
pub mod download;
pub mod engine;
pub mod normalize;
pub mod player;
pub mod runtime;
pub mod sampler;
pub mod tokenizer;
pub mod voices;

mod paths;
