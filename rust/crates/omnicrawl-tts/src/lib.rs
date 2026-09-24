//! TTS（文本转语音）：`omnicrawl/tts/` 的 Rust 等价实现，外加一条接口后端。
//!
//! 两条合成链路：
//!
//! * **接口后端**（[`api`]，发布默认）：OpenAI 兼容 `POST {base_url}/audio/speech`，
//!   只需要 HTTP 客户端——`ort` 的预编译库不全（musl 目标没有分发件），所以它是默认路径。
//! * **本地 ONNX 推理**（[`engine`] + `runtime`，`onnx` feature，默认关闭）：
//!   基于 MOSS-TTS-Nano（OpenMOSS）的 0.1B ONNX 模型，离线可用、支持参考音频克隆，
//!   需要 `ort` 与模型文件。开启方式：`cargo build -p omnicrawl-tts --features onnx`。
//!
//! 模块对映（本地推理侧是 `omnicrawl/tts/` 的等价实现；接口侧是 Rust 独有新增）：
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
//! | `runtime` | `omnicrawl/tts/onnx_runtime.py` 的推理核心（`onnx` feature） |
//! | [`engine`] | `omnicrawl/tts/engine.py`（`onnx` feature） |
//! | [`player`] | `omnicrawl/tts/player.py` |
//! | [`result`] / [`api`] | 无（Rust 侧新增） |

pub mod api;
pub mod audio;
pub mod config;
pub mod download;
pub mod normalize;
pub mod player;
pub mod result;
pub mod sampler;
pub mod tokenizer;
pub mod voices;

/// 本地 ONNX 推理链路（`onnx` feature）。
#[cfg(feature = "onnx")]
pub mod cli;
/// `omnicrawl-tts` 命令行入口只在带本地推理时有意义（下载/克隆/合成）。
#[cfg(feature = "onnx")]
pub mod engine;
#[cfg(feature = "onnx")]
pub mod runtime;

/// 未启用本地推理时，`engine` 换成同名同签名的占位实现（见模块文档）。
#[cfg(not(feature = "onnx"))]
#[path = "engine_stub.rs"]
pub mod engine;

mod paths;

/// 本构建是否带本地 ONNX 推理（宿主与设置面板据此隐藏/禁用相关入口）。
pub const fn local_engine_available() -> bool {
    cfg!(feature = "onnx")
}
