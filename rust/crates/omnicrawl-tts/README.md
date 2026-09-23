# omnicrawl-tts

TTS 引擎：MOSS-TTS-Nano（OpenMOSS）的 ONNX 推理版本，不依赖 PyTorch。语义基准是
`omnicrawl/tts/`（`config` / `audio` / `normalize` / `custom_voices` / `download` /
`onnx_runtime` / `engine` / `player` / `cli`）。

内核要脱离宿主持有时，语音不能再由 Python 提供：谁提供分词、模型从哪来、8 个 ONNX
session 怎么串、波形怎么落盘，都必须由 Rust 自己回答。

## 模块对映

| crate 模块 | Python 模块 | 说明 |
|---|---|---|
| `config` | `tts/config.py` | `TtsConfig`、模型目录解析（`OMNICRAWL_TTS_MODEL_DIR` 覆盖） |
| `audio` | `tts/audio.py` | 8/16/24/32 位 PCM WAV 读写、线性重采样、参考音频声道转换 |
| `normalize` | `tts/normalize.py` | 稳健清洗管道（URL/路径/日期保护、噪声符号清理、标点收敛） |
| `voices` | `tts/custom_voices.py` | 音色名校验、`~/.omnicrawl/tts/custom_voices.json` 读写、manifest 内置音色 |
| `download` | `tts/download.py` | Hugging Face 仓库枚举与流式下载、模型目录发现/布局归一/就绪判断 |
| `tokenizer` | `sentencepiece` | 纯 Rust sentencepiece（BPE + nmt_nfkc + byte fallback） |
| `sampler` | `onnx_runtime.py` 采样段 | PCG64 随机数、softmax、top-k/top-p、重复惩罚 |
| `runtime` | `tts/onnx_runtime.py` | 8 个 ONNX session、prefill/decode、local 采样分支、codec 全量与流式解码 |
| `engine` | `tts/engine.py` | 文本分块、音色解析、参考音频编码、逐块合成与 WAV 写出 |
| `player` | `tts/player.py` | Windows `PlaySoundW`（同步/异步）、macOS/Linux 系统播放器回退 |
| `cli` | `tts/cli.py` | `omnicrawl-tts` 命令行入口 |

## 关键决策

- **推理**：`ort`（ONNX Runtime 绑定，`download-binaries` + `copy-dylibs`）。session 的输入
  输出名与形状全部由模型自带的 meta JSON 驱动，不硬编码层数。
- **设备**：本实现只做 CPU（`CPUExecutionProvider`）。`device = auto` 直接落 CPU，
  `device = cuda` 明确报错——CUDA 需要随包分发 `onnxruntime-gpu`，另行评估。
- **分词**：用纯 Rust 的 `sentencepiece-rs` 而不是官方 C++ 绑定：C++ 静态库自带 protobuf，
  与 ORT 携带的 protobuf 会在链接期大量重复定义（LNK2005），且需要 CMake/C++ 工具链。
- **随机数**：`fixed`（默认采样模式）用 PCG64 + splitmix64 播种，与 numpy 的
  `default_rng` 同族但播种不同，因此同一 seed 的采样序列与 Python **不逐位一致**；
  `greedy` 不使用随机数，可与 Python 逐帧对齐。
- **模型下载**：只走内置 HTTP 下载器（`ureq`），不再依赖 `huggingface_hub`。

## 验证

对照数据集由 Python 真实现生成：

```bash
python rust/tools/gen_tts_fixture.py          # 文本归一化
python rust/tools/gen_tts_io_fixture.py       # 音频 I/O 与声线库
python rust/tools/gen_tts_runtime_fixture.py  # greedy 生成帧（需要模型）

cd rust && cargo test -p omnicrawl-tts
```

`tests/tts_runtime_parity.rs` 需要 `~/.omnicrawl/tts/models` 就绪，缺模型时自动跳过。
命令行自查：

```bash
cargo run -p omnicrawl-tts -- --list-voices
cargo run -p omnicrawl-tts -- --warmup
cargo run -p omnicrawl-tts -- --text "你好，世界。" --no-play --sample-mode greedy --output out.wav
```

## 已知差异

- `enable_wetext`（WeTextProcessing 语义归一化，依赖 Python `pynini`）不实现：开启时降级为
  纯 Rust 清洗并提示。
- CUDA 推理未接入，见上。
- `fixed` 采样模式的随机序列与 Python 不同（功能等价，音频不同）。
