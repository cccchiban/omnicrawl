# TTS 语音合成

OmniCrawl 的语音合成走**云服务**，两条通道按配置二选一：

| 通道 | `provider` | 接口 |
| --- | --- | --- |
| **OpenAI** | `openai` | `POST {base_url}/audio/speech` |
| **Gemini** | `gemini` | `POST {base_url}/models/{model}:generateContent` |

本机不再需要模型文件或 `espeak-ng` 一类原生依赖：只有 HTTP 与密钥。`tts.enabled` 是总开关，
`tts_cloud.provider` 决定这次合成走哪家。**朗读语言不用配**：两家都按输入文本自动判断，
Gemini 的请求里不下发 `languageCode`。

## 配置

三段各司其职：`[tts]` 是与后端无关的通用键，`[tts_cloud]` 是服务商路由，
`[tts_api]` / `[tts_gemini]` 是两家各自的接口参数。切换服务商**不会**丢掉另一家的配置。

```toml
[tts]
enabled = true            # 总开关：关闭时 tts_synthesize 不进工具表
auto_play = true          # 合成完成后自动播放
speak_replies = false     # 回合结束后朗读助手回复（不经工具面，因此不触发审批）
output_dir = ".omnicrawl/.agent_tmp/tts"   # 无 path 参数时的默认保存目录（相对工作区）

[tts_cloud]
provider = "openai"       # openai / gemini

# OpenAI 通道（provider = "openai" 时生效）
[tts_api]
base_url = "https://api.openai.com/v1"
api_key = ""              # 明文密钥；可在设置面板直接填写
model = "gpt-4o-mini-tts"
voice = "alloy"
instructions = ""         # 语气指令；只对 gpt-4o-mini-tts 一类模型生效
speed = 1                 # 0.25~4.0；1 时不下发该字段
timeout_seconds = 120

# Gemini 通道（provider = "gemini" 时生效）
[tts_gemini]
base_url = "https://generativelanguage.googleapis.com/v1beta"
api_key = ""              # 明文密钥；可在设置面板直接填写
model = "gemini-2.5-flash-preview-tts"
voice = "Kore"            # 预置音色名，如 Kore、Puck、Zephyr
instructions = ""         # 语气指令，作为正文前缀交给模型
timeout_seconds = 120
```

配置由 Rust 的 `omnicrawl-config`（`[tts]`、`[tts_cloud]`、`[tts_api]`、`[tts_gemini]`）读写，
也可在 TUI 设置面板（`/settings → TTS 语音合成`）中直接修改并写回；面板保存后立刻重建工具表，
即刻生效。**密钥在面板里直接填写**（界面只显示掩码，进输入态从空起步、留空表示不改动磁盘原值），
写回对应服务商段的 `api_key`。

## OpenAI 通道

- **接口**：`POST {base_url}/audio/speech`，请求体 `model` / `input` / `voice` / `response_format`，
  `speed` 与 `instructions` 为默认或空时不下发（避免 `tts-1` 这类不支持指令的模型报 400）。
- **固定 `response_format = "wav"`**：本地播放走 `winmm`（只认 WAV），且落盘前要解析出时长。
- **13 个内置音色**：`alloy`、`ash`、`ballad`、`coral`、`echo`、`fable`、`nova`、`onyx`、
  `sage`、`shimmer`、`verse`、`marin`、`cedar`（设置面板的下拉即取自这份清单）。
- **模型**：`gpt-4o-mini-tts`（支持语气指令）、`tts-1`、`tts-1-hd`。
- **任何 OpenAI 兼容服务**：把 `base_url` 指向它、按它的音色名填 `voice` 即可。

## Gemini 通道

- **接口**：`POST {base_url}/models/{model}:generateContent`，`generationConfig.responseModalities`
  固定为 `["AUDIO"]`，音色走 `speechConfig.voiceConfig.prebuiltVoiceConfig.voiceName`。
- **返回裸 PCM**：响应里的音频是 base64（`candidates[].content.parts[].inlineData.data`），
  解出来是 24kHz 单声道 16 位 PCM，落盘时由本项目补 WAV 头。
- **语气指令写在正文前**：官方接口没有独立的指令字段，`instructions` 以正文前缀下发。
- **朗读语言由模型自判**：请求里不下发 `languageCode`，中文回复会按中文读；想改写作风格用
  `instructions`（例如「用平静的语气」）。
- **音色**：预置音色名如 `Kore`、`Puck`、`Zephyr`（设置面板下拉取自官方语音列表）。
- **单次请求上限更紧**：文本字段 4000 字节，因此切块按字节算（比 OpenAI 的字符口径更保守）。

## 通用行为

- **长文本自动分块**：按句末标点切块（OpenAI 单块 ≤2000 字符，Gemini 按字节预算），
  逐块请求后沿时间轴拼接，最后只写一次 WAV。采样率不一致时明确报错，不静默重采样。
- **自动播放**：合成完成后默认本地播放（Windows `winmm` 的 `PlaySoundW`，macOS/Linux 回退
  `afplay`/`aplay`/`paplay`），可用配置关闭。
- **失败也是 JSON**：`ok=false` 加 `error`，错误文案带服务端响应原文（模型名不对、音色不存在、
  密钥无效、内容被安全策略拦下都能直接看出来）。

## 自动朗读回复

`speak_replies = true` 时，每轮结束后 TUI 直接把助手回复文本交给合成通道播放，
落盘在 `reply_<时间戳>_<序号>.wav`（与工具的 `tts_<时间戳>.wav` 分开命名）。

- **与 `auto_play` 的分工**：`auto_play` 只管「某次 `tts_synthesize` 工具合成完后要不要播」；
  朗读回复是另一条独立通路，由本开关控制，**不经工具面、不触发审批**。
- **同一时刻只播一轮**：合成要几秒，上一轮还在合成时落地的下一轮直接跳过，
  避免积压出与界面错位的语音。落盘失败只记一条诊断，不打断回合。

## Agent 工具：`tts_synthesize`

模型可通过 `tts_synthesize` 把文本合成为语音：

| 参数 | 说明 |
|---|---|
| `text` | 必填。要朗读的文本。 |
| `path` | 可选。WAV 保存路径；缺省写入配置 `output_dir`（时间戳命名，不覆盖）。 |

返回 JSON：`ok` / `audio_path` / `sample_rate` / `duration_seconds` / `provider` / `voice` /
`text_chunks`。**失败时同样返回 JSON（`ok=false` + `error`）**，模型可直接判断成败，避免因
结果不明重复调用。工具未启用（`tts.enabled=false`）时返回明确指引。

音色不由工具参数决定，取当前服务商的 `voice`；工具面只认 `text` 与 `path`，多传的参数不参与
合成。

工具注册与开关：目录定义 `omnicrawl-controllers` 的 `data/agent_tools.json`，执行体在
`omnicrawl-host` 的 `tools/tts.rs`；可用 config.toml `[tools]` 段或 TUI 工具开关单独禁用。

## 常见问题

- **报「未配置密钥」？** 在设置面板 TTS 页的 `API Key` 行填写，或直接在 config.toml 对应段的
  `api_key` 里写明文（`[tts_api]` 或 `[tts_gemini]`）。
- **为什么合成没有声音？** 检查 `tts.auto_play` 是否开启；Windows 播放走 `winmm`（只认 WAV），
  macOS/Linux 回退 `afplay`/`aplay`/`paplay`（未找到播放器时只记录一行提示，不影响合成结果）。
- **为什么开了自动播放，AI 回答还是不出声？** `auto_play` 不是「自动朗读回复」——它只在
  `tts_synthesize` 工具被调用后生效。要让每轮回复都被读出来，改用 `tts.speak_replies`
  （设置面板 TTS 页的「自动朗读回复」行）。
- **读错了语言？** 朗读语言不配置：两家都按输入文本自动判断。要调节奏或语气用 `instructions`。
- **音色不存在？** 两家的音色名不通用（OpenAI 是小写英文名、Gemini 是星名），
  切换 `provider` 后记着把 `voice` 也换成那家的候选。

## 实现位置

| 部分 | 位置 |
|---|---|
| 服务商路由与两家参数 | `omnicrawl-config/src/features/{tts,tts_cloud}.rs` |
| 合成入口（选路/分块/拼接/落盘） | `omnicrawl-tts/src/synthesis.rs` |
| OpenAI 通道 | `omnicrawl-tts/src/openai.rs` |
| Gemini 通道 | `omnicrawl-tts/src/gemini.rs` |
| 响应音频解码 | `omnicrawl-tts/src/codec.rs` |
| 长文本切块 | `omnicrawl-tts/src/text.rs` |
| 音频 I/O 与播放 | `omnicrawl-tts/src/{audio,player}.rs` |
| 工具执行体 | `omnicrawl-host/src/tools/tts.rs`（输出目录、自动播放；`speak_text` 供回合朗读） |
| 回合朗读触发 | `omnicrawl-tui/src/app.rs`（`TurnFinished` → `speak_reply`） |

历史：本地推理最初按 Python 侧的 MOSS-TTS-Nano 实现（自回归解码、采样器、codec、sentencepiece
分词与参考音频语音克隆），后整体改为 Kokoro-82M（一次前向出整段波形），再后来移除 OpenAI 兼容
的接口后端只留本地推理。之后反向调整：本地推理链路（含 `ort` / `espeak-ng` 依赖、音素化、
音色库、模型下载与 `onnx` feature）整体删除，TTS 只走云服务；随后又删掉凭据的环境变量通道
（`api_key_env`）与 Gemini 的朗读语言（`language_code`，改为按输入文本自判），密钥统一在
设置面板直接填写并写回 config.toml。
