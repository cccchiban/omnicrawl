# TTS 语音合成（MOSS-TTS-Nano）

OmniCrawl 的 TTS 功能基于 [OpenMOSS/MOSS-TTS-Nano](https://github.com/OpenMOSS/MOSS-TTS-Nano) 的 **ONNX 推理版本**：仅 0.1B 参数、无需 PyTorch，支持 CPU/CUDA 设备选择、20 种语言、内置音色与参考音频语音克隆。实现位于 `omnicrawl/tts/`。

## 能力概览

- **CPU/CUDA 推理**：`onnxruntime` + `sentencepiece` + numpy，无 torch 依赖；CPU 使用 `onnxruntime`，CUDA 使用 `onnxruntime-gpu`；48kHz 立体声输出。
- **18 个内置音色 + 用户自定义克隆音色**：Junhao/Zhiming/Weiguo/Xiaoyu/Yuewen/Lingyu/Trump/Ava/...（中英文混音色）；克隆音色存于 `~/.omnicrawl/tts/custom_voices.json`，由 `/settings → TTS 语音合成` 面板的"语音克隆"功能生成，与内置音色一起在下拉与 `tts_synthesize` 中可用。
- **语音克隆**：传入参考音频（`prompt_audio`）即可克隆该音色，优先于内置音色；也可在设置面板输入名称 + 选择参考 .wav，把克隆结果保存为一条"自定义音色"长期使用（不依赖原始音频路径，随库跨会话可用）。
- **删除自定义音色**：设置面板"语音克隆"区下方提供"选择要删除的音色"下拉与红色"删除音色"按钮；仅自定义（克隆）音色可删，内置音色不可删。删除后需保存设置才会从可用音色下拉移除；若被删音色正是当前配置的 voice，会自动回退到首个可用内置音色。
- **长文本自动分块**：按 token 预算切句/切块，块间插入自然停顿。
- **自动播放**：合成完成后默认本地播放（`winsound` 异步，不阻塞 Agent 线程），可用配置关闭。
- **模型自动下载**：首次启用时自动下载约 763MB（TTS 673MB + Codec 91MB），支持断点续传。
- **GPU 运行时管理**：打开 `/settings → TTS 语音合成` 时自动检查 `onnxruntime-gpu`，并用实际 MOSS ONNX session 验证 CUDA；点击按钮后自动安装与当前 Python/驱动兼容的 `onnxruntime-gpu`、CUDA 12 和 cuDNN 9 Python 运行时依赖。此功能不下载或安装 NVIDIA 显卡驱动或 CUDA Toolkit；安装完成后必须完全重启 OmniCrawl。

## 配置（config.toml `[tts]` 段）

```toml
[tts]
enabled = true
model_dir = ""            # 留空使用 ~/.omnicrawl/tts/models（可用环境变量 OMNICRAWL_TTS_MODEL_DIR 覆盖）
voice = "Junhao"          # 内置音色名
auto_play = true          # 合成完成后自动播放
thread_count = 4          # onnxruntime CPU 推理线程数（1/2/4/8）
device = "auto"           # 推理设备：auto / cpu / cuda
streaming = true          # codec 流式解码（低首字节延迟；默认开启，显著降低显存占用，
                          #   避免 codec 全量解码在低显存（如 4GB）设备上 OOM）
output_dir = ".omnicrawl/.agent_tmp/tts"   # 无 path 参数时的默认保存目录（相对工作区）
```

配置读写由 `omnicrawl/config/features/tts.py` 提供；也可在 TUI 设置面板（`/settings → TTS 语音合成`）中直接修改并写回。`device = "cpu"` 始终使用 CPU；`device = "cuda"` 要求真实 session 使用 `CUDAExecutionProvider`，不可用时直接报错；`device = "auto"` 优先尝试 CUDA，但存在两类自动回退 CPU 的情形：① CUDA provider 或会话初始化不可用；② 首张 NVIDIA 显卡**显存 < 6GB**（`onnx_runtime.py::_CUDA_AUTO_MIN_VRAM_MIB`）。原因：MOSS-TTS 引擎一次初始化约 8 个 CUDA session（约占 2.1GB 显存），长文本自回归推理峰值还会再涨约 1.8GB——4GB 级小卡上长文本必 OOM，且 OOM 时 ONNX Runtime 会把带 ANSI 色码的 C++ 错误直写 stderr 覆盖全屏 TUI。CUDA 需要安装与本机 CUDA/cuDNN 兼容的 `onnxruntime-gpu`，不要与 CPU 版 `onnxruntime` 同时保留。

## 主 TUI 自动播报

启用 TTS（`tts.enabled=true`）且模型就绪后，主 TUI（全屏对话界面）中 Agent
的文字回复会被**程序自动朗读**，模型无需感知或调用任何 TTS 工具：

- 模型流式正文（不含推理/思考与工具调用内容）按段落实时切分、后台合成并
  按顺序播放；回合结束时残留文本统一提交为最后一段，取消回合则丢弃未播内容。
- 段落合成默认单引擎串行（`TurnSpeechAnnouncer` 默认 `max_concurrency=1`）：
  每个 worker 会独立加载整套模型权重（约 2GB 级内存），串行合成即可满足
  边输出边朗读，同时避免多套权重同时驻留内存导致高内存占用与 TUI 卡顿；
  播放严格按输出顺序串行，不互相打断。
- 仅主 TUI 生效；Telegram/飞书等远程入口不自动朗读。
- 实现位于 `omnicrawl/ui/fullscreen/turn/announcer.py`（`TurnSpeechAnnouncer`），
  由 `omnicrawl/ui/fullscreen/app/core.py` 惰性装配、App 退出时统一释放。

`tts_synthesize` 工具仍保留注册，可用于手动朗读指定文本（如读文件内容、
长代码注释等），但不再是模型回复的必经步骤。

## Agent 工具：`tts_synthesize`

模型可通过 `tts_synthesize` 工具把指定文本合成为语音：

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
明确指引。引擎按 `model_dir`+`thread_count`+`device` 缓存复用，多次调用不会重复加载模型。

工具注册与开关：`omnicrawl/agent/toolkit/tools.py`（定义）、`omnicrawl/agent/controllers/tools/building.py`（绑定）、`omnicrawl/agent/controllers/tools/implementations.py`（实现）。`tts_synthesize` 可经 config.toml `[tools]` 段或 TUI 工具开关单独禁用。

## CLI

```bash
python -m omnicrawl.tts --text "欢迎使用语音合成。" --voice Junhao --output out.wav
python -m omnicrawl.tts --text "你好" --prompt-audio ref.wav       # 语音克隆
python -m omnicrawl.tts --list-voices                               # 列出内置音色
python -m omnicrawl.tts --warmup                                    # 预热模型后退出
python -m omnicrawl.tts --text "..." --no-play                      # 不自动播放
python -m omnicrawl.tts --device cpu --text "..." --no-play       # CPU 推理
python -m omnicrawl.tts --device cuda --text "..." --no-play      # CUDA 推理（不可用时报错）
python -m omnicrawl.tts --device auto --text "..." --no-play      # 自动优先 CUDA，失败回退 CPU
```

## Python API

```python
from omnicrawl.tts import TtsEngine, TTSConfig

engine = TtsEngine(device="auto")  # auto 优先 CUDA，不可用时回退 CPU
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

## Rust 实现（`omnicrawl-tts`）

`rust/crates/omnicrawl-tts/` 是同一引擎的 Rust 版本，用于内核脱离 Python 宿主：模块与
Python 侧一一对映（配置、音频 I/O、文本归一化、自定义音色库、模型下载、分词、采样、
ONNX 推理、合成编排、播放、CLI），自带对照数据集（文本归一化、音频 I/O 与声线库、
greedy 模式生成帧与 Python 逐帧一致）。

已知差异：

- **仅 CPU**：`device=cuda` 在 Rust 侧明确报错（CUDA 需要随包分发 `onnxruntime-gpu`）；
  `device=auto` 直接落 CPU。
- **无 WeTextProcessing**：`enable_wetext` 依赖 Python `pynini`，Rust 侧不支持——开启时
  降级为纯清洗并提示。
- **`fixed` 采样模式的随机序列不同**：Rust 用 PCG64 + 自己的播种，`seed` 可复现但数值与
  Python 不一致；`greedy` 不使用随机数，与 Python 完全一致。
- 命令行入口为 `omnicrawl-tts`，参数与 `python -m omnicrawl.tts` 对齐。

## 常见问题

- **模型在哪里下载？** 默认 `~/.omnicrawl/tts/models`；`model_dir` 留空时首次调用 `TtsEngine` 或点击 TUI 设置面板的"下载模型"按钮自动下载。网络受限时可用 `OMNICRAWL_TTS_MODEL_DIR` 指定已手动放置的模型目录。
- **为什么合成没有声音？** 检查 `tts.auto_play` 是否开启；Windows 播放依赖 `winsound`（WAV 格式），macOS/Linux 回退 `afplay`/`aplay`/`paplay`。
- **WeTextProcessing 语义归一化怎么开？** `enable_wetext` 需要额外安装 `pynini` + `WeTextProcessing`（Windows 上安装较麻烦）；未安装时自动降级为纯 Python 稳健清洗，绝大多数场景足够。
- **CPU 慢？** 调高 `thread_count`（最多 8）或把 `max_new_frames` 调低（会截断长音频）；也可以在设置面板选择 CUDA。若状态显示 provider 存在但实际会话仍是 CPU，通常是 CUDA/cuDNN DLL 缺失或版本不兼容；点击 GPU 按钮会安装 Python 运行时 DLL，但不会安装显卡驱动或 CUDA Toolkit。
- **TUI 语音会话中屏幕被大片红色 `[E:onnxruntime:...]`/`CUDA error ... out of memory` 覆盖？** 这是 4GB 级小显存卡上 `device=auto` 走 CUDA 长文本推理 OOM 时，ONNX Runtime 把 C++ 层错误（带 ANSI 色码）直写 stderr 造成的。已做三层防护：① `device=auto` 检测到显存 < 6GB 时初始化即回退 CPU（`onnx_runtime.py`）；② ORT 默认日志级别提升到 FATAL（`silence_ort_logging`），抑制 provider 加载失败与推理期 ERROR 红字；③ TUI 运行期 Python 日志落盘 `~/.OmniCrawl/logs/tui.log`（`ui/fullscreen/app/runner.py`），不再经 lastResort 刷 stderr。若仍需 CUDA 加速，请使用显存 ≥ 6GB 的显卡或缩短单段文本（`voice_clone_max_text_tokens`/段落切分会控制单次推理长度）。
