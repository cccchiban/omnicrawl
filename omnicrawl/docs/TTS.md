# TTS 语音合成（MOSS-TTS-Nano）

OmniCrawl 的 TTS 功能基于 [OpenMOSS/MOSS-TTS-Nano](https://github.com/OpenMOSS/MOSS-TTS-Nano) 的 **ONNX CPU 推理版本**：仅 0.1B 参数、无需 GPU 与 PyTorch，支持 20 种语言、内置音色与参考音频语音克隆。实现位于 `omnicrawl/tts/`。

## 能力概览

- **纯 CPU 推理**：`onnxruntime` + `sentencepiece` + numpy，无 torch 依赖；48kHz 立体声输出。
- **18 个内置音色**：Junhao/Zhiming/Weiguo/Xiaoyu/Yuewen/Lingyu/Trump/Ava/...（中英文混音色）。
- **语音克隆**：传入参考音频（`prompt_audio`）即可克隆该音色，优先于内置音色。
- **长文本自动分块**：按 token 预算切句/切块，块间插入自然停顿。
- **自动播放**：合成完成后默认本地播放（`winsound` 异步，不阻塞 Agent 线程），可用配置关闭。
- **模型自动下载**：首次启用时自动下载约 763MB（TTS 673MB + Codec 91MB），支持断点续传。

## 配置（config.toml `[tts]` 段）

```toml
[tts]
enabled = true
model_dir = ""            # 留空使用 ~/.omnicrawl/tts/models（可用环境变量 OMNICRAWL_TTS_MODEL_DIR 覆盖）
voice = "Junhao"          # 内置音色名
auto_play = true          # 合成完成后自动播放
thread_count = 4          # onnxruntime CPU 推理线程数（1/2/4/8）
streaming = false         # codec 流式解码（低首字节延迟）
output_dir = ".omnicrawl/.agent_tmp/tts"   # 无 path 参数时的默认保存目录（相对工作区）
```

配置读写由 `omnicrawl/config/features/tts.py` 提供；也可在 TUI 设置面板（`/settings → TTS 语音合成`）中直接修改并写回。

## Agent 工具：`tts_synthesize`

模型可通过 `tts_synthesize` 工具把文本合成为语音：

| 参数 | 说明 |
|---|---|
| `text` | 必填。要朗读的文本。 |
| `prompt_audio` | 可选。参考音频路径，提供时按音色克隆。 |
| `path` | 可选。WAV 保存路径；缺省写入配置 `output_dir`（时间戳命名，不覆盖）。 |

音色**固定使用配置 `tts.voice`**（在 /settings → TTS 中设置），工具面不暴露
`voice` 参数——模型不能指定音色，避免传入不存在的音色名导致合成失败。

返回 JSON：`ok`（是否成功）/ `audio_path` / `sample_rate`（48000）/ `duration_seconds` /
`voice` / `text_chunks`。**失败时同样返回 JSON（`ok=false` + `error`）**，模型可从返回
内容直接判断成败，避免因结果不明而重复调用。工具未启用（`tts.enabled=false`）时返回
明确指引。引擎按 `model_dir`+`thread_count` 缓存复用，多次调用不会重复加载模型。

工具注册与开关：`omnicrawl/agent/toolkit/tools.py`（定义）、`omnicrawl/agent/controllers/tools/building.py`（绑定）、`omnicrawl/agent/controllers/tools/implementations.py`（实现）。`tts_synthesize` 可经 config.toml `[tools]` 段或 TUI 工具开关单独禁用。

## CLI

```bash
python -m omnicrawl.tts --text "欢迎使用语音合成。" --voice Junhao --output out.wav
python -m omnicrawl.tts --text "你好" --prompt-audio ref.wav       # 语音克隆
python -m omnicrawl.tts --list-voices                               # 列出内置音色
python -m omnicrawl.tts --warmup                                    # 预热模型后退出
python -m omnicrawl.tts --text "..." --no-play                      # 不自动播放
```

## Python API

```python
from omnicrawl.tts import TtsEngine, TTSConfig

engine = TtsEngine()   # 首次使用自动下载模型
result = engine.synthesize("欢迎使用 MOSS-TTS-Nano。", voice="Junhao", output_path="out.wav")
print(result.audio_path, result.duration_seconds)
```

## 模块结构

| 文件 | 职责 |
|---|---|
| `omnicrawl/tts/engine.py` | `TtsEngine`：模型解析/下载、音色解析、文本分块、合成 |
| `omnicrawl/tts/onnx_runtime.py` | ONNX 推理核心（移植官方 `ort_cpu_runtime.py`，Apache-2.0，已做 Python 3.9 兼容） |
| `omnicrawl/tts/normalize.py` | 纯 Python 文本归一化（稳健清洗）+ WeTextProcessing 可选封装 |
| `omnicrawl/tts/audio.py` | wave 读写 + numpy 重采样（替代 torchaudio） |
| `omnicrawl/tts/download.py` | urllib 模型下载器（无需 huggingface_hub） |
| `omnicrawl/tts/player.py` | 生成音频自动播放（winsound/afplay/aplay） |
| `omnicrawl/tts/cli.py` | `python -m omnicrawl.tts` 命令行入口 |
| `omnicrawl/config/features/tts.py` | `[tts]` 配置读取/校验/写回 |

## 常见问题

- **模型在哪里下载？** 默认 `~/.omnicrawl/tts/models`；`model_dir` 留空时首次调用 `TtsEngine` 或点击 TUI 设置面板的"下载模型"按钮自动下载。网络受限时可用 `OMNICRAWL_TTS_MODEL_DIR` 指定已手动放置的模型目录。
- **为什么合成没有声音？** 检查 `tts.auto_play` 是否开启；Windows 播放依赖 `winsound`（WAV 格式），macOS/Linux 回退 `afplay`/`aplay`/`paplay`。
- **WeTextProcessing 语义归一化怎么开？** `enable_wetext` 需要额外安装 `pynini` + `WeTextProcessing`（Windows 上安装较麻烦）；未安装时自动降级为纯 Python 稳健清洗，绝大多数场景足够。
- **CPU 慢？** 调高 `thread_count`（最多 8）或把 `max_new_frames` 调低（会截断长音频）；实测 4 线程实时率约 0.73x（合成速度快于音频时长）。
