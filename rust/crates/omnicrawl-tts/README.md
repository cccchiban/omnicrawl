# omnicrawl-tts

TTS 合成客户端：把文本交给云服务的语音接口，拿回音频并落盘。两条通道——

| 通道 | 接口 | 返回 |
|---|---|---|
| OpenAI | `POST {base_url}/audio/speech` | WAV（带 RIFF 头） |
| Gemini | `POST {base_url}/models/{model}:generateContent` | base64 包裹的裸 PCM（24kHz 单声道） |

两家都只需要 HTTP + 一个密钥，因此本 crate 没有 feature 开关、没有原生依赖、没有模型下载。

## 模块

| 模块 | 职责 |
|---|---|
| `openai` | OpenAI `audio/speech` 的请求体与响应处理 |
| `gemini` | Gemini TTS 的 `generateContent` 请求体、响应解析与 base64 解码 |
| `synthesis` | 两条通道的公共入口：选路、分块、波形拼接与落盘（`TtsBackend`） |
| `text` | 长文本按句切块（两家都有单次请求的长度上限） |
| `codec` | 响应音频的解码：WAV 容器与裸 PCM 16 位 |
| `audio` | 8/16/24/32 位 PCM WAV 读写（含内存字节解码） |
| `player` | Windows `PlaySoundW`（同步/异步）、macOS/Linux 系统播放器回退；只返回结果、不写 stderr |
| `result` | 合成结果 `TtsResult` |

## 关键决策

- **统一走 WAV 落盘**：播放走 `winmm` 的 `PlaySoundW`（只认 WAV），且本地要能按 WAV 解析出
  时长与波形。Gemini 回的是裸 PCM，因此就地补一个 WAV 头再落盘（采样率取官方口径 24kHz）。
- **只接受 WAV，不引入解码器**：OpenAI 侧固定请求 `response_format = "wav"`；其它格式
  （mp3/opus/aac/flac）要么得自带解码器，要么只能落盘不能自检与播放。
- **长文本自己分块**：两家都有单次请求的长度上限（OpenAI 4096 字符、Gemini 4000 字节），
  按句末标点贪心切块，再把各段的波形拼起来（同一模型同一采样率，直接拼接即可）。
  采样率不一致时明确报错，不静默重采样。
- **Gemini 的语气指令写在正文前**：官方接口没有独立的指令字段，语气用自然语言提示表达，
  因此 `instructions` 以正文前缀下发（OpenAI 侧走独立的 `instructions` 字段）。
- **错误文案带服务端原文**：4xx/5xx 的响应体里通常写着真正的原因（模型名不对、音色不存在、
  密钥无效、内容被安全策略拦下），截断后放进错误信息里便于排障。
- **只有一个合成入口**：`synthesis::synthesize` 收 `TtsBackend`，宿主只管装配通道，
  工具面的返回值也因此在两条通路上完全一致。

## 历史

最初是本地 ONNX 推理：MOSS-TTS-Nano（自回归解码、采样器、codec、sentencepiece 分词与参考音频
语音克隆），后换 Kokoro-82M（一次前向出整段波形，因此采样器/分词器/codec 与克隆一并删除），
再后来移除 OpenAI 兼容的接口后端只保留本地推理。现在整体转向云服务，本地推理链路
（推理引擎、espeak-ng 音素化、音色库、模型下载，以及 `ort` / `espeakng-sys` 依赖与 `onnx`
feature）全部删除。

## 验证

```bash
cargo test -p omnicrawl-tts
```

纯离线单测：切块边界、PCM/WAV 解码、请求体（默认参数不下发）、URL 拼接与缺密钥报错。
真实合成需要密钥，不在单测范围内。
