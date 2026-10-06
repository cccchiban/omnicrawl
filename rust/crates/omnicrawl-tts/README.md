# omnicrawl-tts

TTS 引擎，两条合成链路：

- **接口合成**（`api`，发布默认）：OpenAI 兼容 `POST {base_url}/audio/speech`，只要 HTTP 客户端，
  适用于所有平台（含 musl 静态构建）。
- **本地 ONNX 推理**（`engine` + `runtime`，`onnx` feature，默认关闭）：MOSS-TTS-Nano（OpenMOSS）
  的 0.1B ONNX 推理版本，不依赖 PyTorch，支持参考音频克隆。语义基准是 `omnicrawl/tts/`
  （`config` / `audio` / `normalize` / `custom_voices` / `download` / `onnx_runtime` /
  `engine` / `player` / `cli`）。

内核要脱离宿主持有时，语音不能再由 Python 提供：谁提供分词、模型从哪来、8 个 ONNX
session 怎么串、波形怎么落盘，都必须由 Rust 自己回答。

为什么本地推理默认不编译：`ort` 的预编译库不覆盖全部目标（musl 没有分发件），而发布要能
编出 musl 载荷（见 `rust/docs/python-free-build.md` 第 7 节）；接口合成没有这个限制。

## 模块对映

| crate 模块 | Python 模块 | 说明 |
|---|---|---|
| `api` | 无（Rust 侧新增） | OpenAI 兼容音频接口：请求分块、WAV 拼接、错误透传 |
| `result` | 无（Rust 侧新增） | 两条链路共用的合成结果 `TtsResult` |
| `config` | `tts/config.py` | `TtsConfig`、模型目录解析（`OMNICRAWL_TTS_MODEL_DIR` 覆盖） |
| `audio` | `tts/audio.py` | 8/16/24/32 位 PCM WAV 读写（含内存字节解码）、线性重采样、参考音频声道转换 |
| `normalize` | `tts/normalize.py` | 稳健清洗管道（URL/路径/日期保护、噪声符号清理、标点收敛） |
| `voices` | `tts/custom_voices.py` | 音色名校验、`~/.omnicrawl/tts/custom_voices.json` 读写、manifest 内置音色 |
| `download` | `tts/download.py` | Hugging Face 仓库枚举与流式下载、模型目录发现/布局归一/就绪判断 |
| `tokenizer` | `sentencepiece` | 纯 Rust sentencepiece（BPE + nmt_nfkc + byte fallback） |
| `sampler` | `onnx_runtime.py` 采样段 | PCG64 随机数、softmax、top-k/top-p、重复惩罚 |
| `runtime` | `tts/onnx_runtime.py` | 8 个 ONNX session、prefill/decode、local 采样分支、codec 全量与流式解码（`onnx`） |
| `engine` | `tts/engine.py` | 文本分块、音色解析、参考音频编码、逐块合成与 WAV 写出（`onnx`） |
| `player` | `tts/player.py` | Windows `PlaySoundW`（同步/异步）、macOS/Linux 系统播放器回退 |
| `cli` | `tts/cli.py` | `omnicrawl-tts` 命令行入口（需 `onnx`，否则不构建该 bin） |

## 关键决策

- **后端默认**：接口合成（`[tts_api]` 段），因为发布构建不带 `onnx`；两条链路都产出同构的
 `TtsResult`（波形 + 采样率 + 时长），宿主工具只需选后端。
- **只吃 WAV**：接口的 `response_format` 只允许 `wav`。Windows 播放走 `winmm` 的
 `PlaySoundW`（只认 WAV），Rust 侧也按 WAV 解析时长/波形；其它格式要么引入解码器，
 要么只能落盘不能自检。
- **长文本自己分块**：接口有 `input` 长度上限，按句末标点贪心切块（≤2000 字符/块），
 逐块请求后拼接波形，只写一次文件。
- **推理**（`onnx`）：`ort`（ONNX Runtime 绑定，`download-binaries` + `copy-dylibs`）。session 的输入
  输出名与形状全部由模型自带的 meta JSON 驱动，不硬编码层数。
- **设备**：本实现只做 CPU（`CPUExecutionProvider`）。`device = auto` 直接落 CPU，
  `device = cuda` 明确报错——CUDA 需要随包分发 `onnxruntime-gpu`，另行评估。
- **分词**：用纯 Rust 的 `sentencepiece-rs` 而不是官方 C++ 绑定：C++ 静态库自带 protobuf，
  与 ORT 携带的 protobuf 会在链接期大量重复定义（LNK2005），且需要 CMake/C++ 工具链。
- **随机数**：`fixed`（默认采样模式）用 PCG64 + splitmix64 播种，与 numpy 的
  `default_rng` 同族但播种不同，因此同一 seed 的采样序列与 Python **不逐位一致**；
  `greedy` 不使用随机数，可与 Python 逐帧对齐。
- **模型下载**：只走内置 HTTP 下载器（`ureq`），不再依赖 `huggingface_hub`；下载与推理解耦，
  没编译推理时设置面板仍可下载模型。

## 验证

对照数据集是冻结的契约，随仓库提交：

```bash

cd rust && cargo test -p omnicrawl-tts                     # 默认：不含推理链路
cd rust && cargo test -p omnicrawl-tts --features onnx     # 含 runtime 逐帧对照
```

`tests/tts_runtime_parity.rs` 需要 `~/.omnicrawl/tts/models` 就绪，缺模型时自动跳过；
它整个文件跟着 `onnx` feature（未开启时该套件为空）。命令行自查（需 `onnx`）：

```bash
cargo run -p omnicrawl-tts --features onnx -- --list-voices
cargo run -p omnicrawl-tts --features onnx -- --warmup
cargo run -p omnicrawl-tts --features onnx -- --text "你好，世界。" --no-play --sample-mode greedy --output out.wav
```

## 已知差异

- 接口合成不做语音克隆：`prompt_audio` 只在本地推理下生效，接口后端会明确报错
  （需要克隆音色时用服务端自己的音色 id，填 `[tts_api].voice`）。
- `enable_wetext`（WeTextProcessing 语义归一化，依赖 Python `pynini`）不实现：开启时降级为
  纯 Rust 清洗并提示。
- CUDA 推理未接入，见上。
- `fixed` 采样模式的随机序列与 Python 不同（功能等价，音频不同）。
