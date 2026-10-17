//! TTS（文本转语音）：云服务合成单链路。
//!
//! 两条云通道，由 `omnicrawl-config` 的 `[tts_cloud].provider` 选择：
//!
//! * **OpenAI**（[`openai`]）：`POST {base_url}/audio/speech`，返回 WAV/MP3/PCM 等字节。
//! * **Gemini**（[`gemini`]）：`POST {base_url}/models/{model}:generateContent`，
//!   返回 base64 的音频（默认裸 PCM，24kHz 单声道）。
//!
//! 两家都不需要本机模型或系统原生依赖，因此没有 feature 开关：本 crate 只做 HTTP 与音频 I/O。
//!
//! | crate 模块 | 职责 |
//! |---|---|
//! | [`openai`] | OpenAI `audio/speech` 请求与响应解析 |
//! | [`gemini`] | Gemini TTS `generateContent` 请求与响应解析 |
//! | [`synthesis`] | 两条通道的公共入口：选路、分块、拼接与落盘 |
//! | [`text`] | 长文本按句切块（两家都有单次请求的长度上限） |
//! | [`codec`] | 云服务返回的音频字节解码（WAV / 裸 PCM） |
//! | [`audio`] | PCM WAV 读写 |
//! | [`player`] | 合成后的本地播放 |
//! | [`result`] | 合成结果类型 |
//!
//! 本 crate 的历史：最初是本地 ONNX 推理（MOSS-TTS-Nano，后换 Kokoro-82M，含音素化、音色库、
//! 模型下载）；随后移除接口后端只留本地推理；现在整体转向云服务，本地推理链路（推理引擎、
//! 音素化、音色库、模型下载与 `ort`/`espeakng-sys` 依赖）连同 `onnx` feature 一并删除。

pub mod audio;
pub mod codec;
pub mod gemini;
pub mod openai;
pub mod player;
pub mod result;
pub mod synthesis;
pub mod text;

mod paths;
