# TTS 语音合成

OmniCrawl 的语音合成有**两条后端**，由配置决定用哪条：

| 后端 | 配置 | 依赖 | 适用 |
| --- | --- | --- | --- |
| **接口合成**（默认） | `[tts_api]` 段 | 只要 HTTP（OpenAI 兼容 `POST {base_url}/audio/speech`） | 所有平台，含静态链接的 musl 构建 |
| **本地推理**（可选） | `[tts]` 段 | `omnicrawl-tts` 的 `onnx` feature（MOSS-TTS-Nano 0.1B ONNX） | 离线使用、参考音频克隆 |

发布构建**默认不带本地推理**（`ort` 的 ONNX Runtime 预编译库不覆盖全部目标，尤其是 musl），
因此 `tts_synthesize` 默认调接口；要离线或语音克隆，需要自己构建带
`--features omnicrawl-tts/onnx` 的宿主。`tts.enabled` 是两条后端共用的总开关。

## 接口合成（默认）

```toml
[tts_api]
enabled = true            # false 时回落到本地推理（该构建没编译本地推理则明确报错）
base_url = "https://api.openai.com/v1"   # 任何 OpenAI 兼容服务都可以填
api_key = ""              # 明文密钥；留空则读 api_key_env
api_key_env = "OPENAI_API_KEY"
model = "gpt-4o-mini-tts"
voice = "alloy"          # 服务端音色名
response_format = "wav"  # 只支持 wav（Windows 播放走 winmm，且要按 WAV 解析时长）
speed = 1.0              # 语速 0.25~4.0，1.0 时不发给服务端
timeout_seconds = 120
```

- 音色固定取 `tts_api.voice`：工具面不暴露 `voice` 参数，模型不能指定音色。
- **不支持参考音频（`prompt_audio`）**：换服务端支持的音色 id，或改用带本地推理的构建。
- 长文本按句末标点分块（每块 ≤2000 字符），逐块请求后拼接波形，只写一次 WAV。
- 错误信息会带上服务端返回体（模型名不对、音色不存在、余额不足等都在里面），便于排障。

## 本地推理（可选，MOSS-TTS-Nano）

以下细节仅在构建开启了 `omnicrawl-tts/onnx` 时适用，基于
[OpenMOSS/MOSS-TTS-Nano](https://github.com/OpenMOSS/MOSS-TTS-Nano) 的 **ONNX 推理版本**：
仅 0.1B 参数、无需 PyTorch，支持 20 种语言、内置音色与参考音频语音克隆。

- **CPU 推理**：`ort` + 纯 Rust sentencepiece，48kHz 立体声输出。`device=cuda` 明确报错
  （CUDA 需要随包分发 `onnxruntime-gpu`，另行评估）；`device=auto` 直接落 CPU。
- **18 个内置音色 + 用户自定义克隆音色**：Junhao/Zhiming/Weiguo/Xiaoyu/Yuewen/Lingyu/Trump/Ava/...
  （中英文混音色）；克隆音色存于 `~/.omnicrawl/tts/custom_voices.json`，由设置面板
  （`/settings → TTS 语音合成`）的「语音克隆」区生成，与内置音色一起在下拉与 `tts_synthesize` 中可用。
- **语音克隆**：传入参考音频（`prompt_audio`）即可克隆该音色，优先于内置音色；也可在设置面板
  输入名称 + 选择参考 `.wav`，把克隆结果保存为一条长期可用的「自定义音色」（不依赖原始音频路径）。
- **删除自定义音色**：设置面板的「删除自定义音色」行只列克隆音色；保存设置后才会从可用音色下拉移除。
- **长文本自动分块**：按 token 预算切句/切块，块间插入自然停顿。
- **自动播放**：合成完成后默认本地播放（Windows `winmm` 的 `PlaySoundW`，macOS/Linux 回退
  `afplay`/`aplay`/`paplay`），可用配置关闭。
- **模型下载**：首次使用时自动下载约 763MB（TTS 673MB + Codec 91MB），支持断点续传；
  下载与推理解耦，未编译本地推理时面板仍可下载模型。

```toml
[tts]
enabled = true
model_dir = ""            # 留空使用 ~/.omnicrawl/tts/models（可用环境变量 OMNICRAWL_TTS_MODEL_DIR 覆盖）
voice = "Junhao"          # 内置音色名
auto_play = true          # 合成完成后自动播放
thread_count = 4          # CPU 推理线程数（1/2/4/8）
device = "auto"           # 推理设备：auto / cpu / cuda（Rust 实现只有 CPU，见上）
streaming = true          # codec 流式解码（低首字节延迟、显存占用低）
output_dir = ".omnicrawl/.agent_tmp/tts"   # 无 path 参数时的默认保存目录（相对工作区）
```

配置由 Rust 的 `omnicrawl-config`（`[tts]` 与 `[tts_api]` 两段）读写，也可在 TUI 设置面板
（`/settings → TTS 语音合成`）中直接修改并写回；面板保存后立刻重建工具表，切换后端即时生效。

## Agent 工具：`tts_synthesize`

模型可通过 `tts_synthesize` 把文本合成为语音：

| 参数 | 说明 |
|---|---|
| `text` | 必填。要朗读的文本。 |
| `prompt_audio` | 可选。参考音频路径，仅在本地推理后端生效（接口后端会明确报错）。 |
| `path` | 可选。WAV 保存路径；缺省写入配置 `output_dir`（时间戳命名，不覆盖）。 |

返回 JSON：`ok` / `audio_path` / `sample_rate` / `duration_seconds` / `voice` / `text_chunks`。
**失败时同样返回 JSON（`ok=false` + `error`）**，模型可直接判断成败，避免因结果不明重复调用。
工具未启用（`tts.enabled=false`）时返回明确指引。本地后端还会按
`model_dir`+`thread_count`+`device` 缓存引擎，多次调用不重复加载模型。

工具注册与开关：目录定义 `omnicrawl-controllers` 的 `data/agent_tools.json`，执行体在
`omnicrawl-host` 的 `tools/tts.rs`；可用 config.toml `[tools]` 段或 TUI 工具开关单独禁用。

## CLI（本地推理）

```bash
cargo run -p omnicrawl-tts --features onnx -- --text "欢迎使用语音合成。" --voice Junhao --output out.wav
cargo run -p omnicrawl-tts --features onnx -- --text "你好" --prompt-audio ref.wav   # 语音克隆
cargo run -p omnicrawl-tts --features onnx -- --list-voices                         # 列出内置音色
cargo run -p omnicrawl-tts --features onnx -- --warmup                              # 预热模型后退出
```

## 常见问题

- **模型在哪里下载？** 默认 `~/.omnicrawl/tts/models`；`model_dir` 留空时首次调用或点击
  设置面板的「下载 ONNX 模型」按钮自动下载。网络受限时可用 `OMNICRAWL_TTS_MODEL_DIR`
  指定已手动放置的模型目录。
- **为什么合成没有声音？** 检查 `tts.auto_play` 是否开启；Windows 播放走 `winmm`（只认 WAV），
  macOS/Linux 回退 `afplay`/`aplay`/`paplay`（未找到播放器时只记录一行提示，不影响合成结果）。
- **接口合成报错怎么办？** 先看错误信息里的服务端返回体：`401` 多为密钥（`api_key` 或
  `api_key_env` 指向的环境变量），`404` 多为 `base_url` 少写或多写了 `/v1`，
  `400` 多为模型名或音色名不被该服务支持。
- **「本构建未启用本地 MOSS-TTS-Nano 推理」是什么意思？** 发布构建默认不带本地推理；
  让 `[tts_api].enabled = true` 走接口，或用 `--features omnicrawl-tts/onnx` 重新构建宿主。
- **CPU 慢？** 调高 `thread_count`（最多 8）或调低 `max_new_frames`（会截断长音频）。
- **`enable_wetext` 语义归一化怎么开？** Rust 侧不支持（依赖 Python `pynini`）：开启时
  降级为纯清洗并提示，绝大多数场景足够。
- **`fixed` 采样模式与 Python 数值不一致？** Rust 用 PCG64 + 自己的播种，同一 `seed` 可复现，
  但序列与 Python 不同；`greedy` 不使用随机数，与 Python 逐帧一致。

## 实现位置与 Python 基线

| 部分 | 位置 |
|---|---|
| 接口合成 | `omnicrawl-tts/src/api.rs`（请求分块、WAV 拼接、错误透传） |
| 合成结果类型 | `omnicrawl-tts/src/result.rs`（两条后端共用） |
| 本地推理 | `omnicrawl-tts/src/{runtime,engine,sampler,tokenizer}.rs`（`onnx` feature） |
| 音频 I/O 与归一化 | `omnicrawl-tts/src/{audio,normalize}.rs` |
| 音色库与模型下载 | `omnicrawl-tts/src/{voices,download}.rs` |
| 播放 | `omnicrawl-tts/src/player.rs` |
| 工具执行体 | `omnicrawl-host/src/tools/tts.rs`（后端选择、输出目录、自动播放） |
| 配置 | `omnicrawl-config/src/features/{tts,tts_api}.rs` |

`omnicrawl/tts/`（Python）**不再是产品路径**，只作为 parity 基准：Rust 侧的文本归一化、
音频 I/O、声线库与 greedy 生成帧都对着它生成的对照数据集逐项比对（见
`rust/crates/omnicrawl-tts/README.md`）。因此本文件在 `omnicrawl/docs/` 与
`rust/assets/docs/` 两份保持逐字节一致（由 `omnicrawl-mcp` 的对照测试钉住）。
