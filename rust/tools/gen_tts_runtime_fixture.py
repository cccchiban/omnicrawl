#!/usr/bin/env python3
"""生成 `omnicrawl-tts` 推理链路的对照数据集（greedy 模式，逐 token 可复现）。

期望值来自 Python 真实现 `omnicrawl/tts/`：同一模型、同一文本下 greedy 采样不使用
随机数，因此 Rust 侧应当逐帧一致。数据集只记录文本块与生成的 audio token 帧序列
（模型权重不进仓库），音频波形不落盘。

前置条件：`~/.omnicrawl/tts/models` 已存在模型（首次使用会下载约 763MB）。

用法（仓库根目录）：

    python rust/tools/gen_tts_runtime_fixture.py
    cd rust && cargo test -p omnicrawl-tts --test tts_runtime_parity
"""

from __future__ import annotations

import json
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
FIXTURE_PATH = ROOT / "rust/crates/omnicrawl-tts/tests/fixtures/tts_runtime_parity.json"

sys.path.insert(0, str(ROOT))

from omnicrawl.tts import TTSConfig, TtsEngine  # noqa: E402
from omnicrawl.tts import onnx_runtime  # noqa: E402
from omnicrawl.tts.download import models_ready  # noqa: E402

CASES = [
    {"text": "你好，世界。", "voice": "Junhao"},
    {"text": "hello world", "voice": "Junhao"},
]


def main() -> int:
    if not models_ready():
        print("模型未就绪，请先运行一次 TTS（会自动下载到 ~/.omnicrawl/tts/models）。")
        return 1

    captured: dict[str, list[list[int]]] = {}
    original = onnx_runtime.OrtCpuRuntime.generate_audio_frames

    def patched(self, request_rows, on_frame=None):  # noqa: ANN001 - 与真实现同签名
        frames = original(self, request_rows, on_frame=on_frame)
        captured.setdefault("frames", []).extend(frames)
        return frames

    onnx_runtime.OrtCpuRuntime.generate_audio_frames = patched  # type: ignore[method-assign]

    cases = []
    for case in CASES:
        captured.clear()
        engine = TtsEngine(
            TTSConfig(
                device="cpu",
                sample_mode="greedy",
                do_sample=False,
                streaming=True,
                output_dir=str(ROOT / ".omnicrawl/.agent_tmp/files/tts-fixture"),
            )
        )
        result = engine.synthesize(
            case["text"],
            voice=case["voice"],
            output_path=str(ROOT / ".omnicrawl/.agent_tmp/files/tts-fixture/out.wav"),
        )
        cases.append(
            {
                "text": case["text"],
                "voice": case["voice"],
                "sample_mode": "greedy",
                "do_sample": False,
                "streaming": True,
                "text_chunks": list(result.text_chunks),
                "audio_token_frames": int(result.audio_token_frames),
                "duration_seconds": round(float(result.duration_seconds), 6),
                "generated_frames": captured.get("frames", []),
            }
        )

    data = {
        "source": "omnicrawl/tts/onnx_runtime.py + omnicrawl/tts/engine.py（greedy）",
        "cases": cases,
    }
    FIXTURE_PATH.parent.mkdir(parents=True, exist_ok=True)
    FIXTURE_PATH.write_text(json.dumps(data, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    print(f"已写入 {FIXTURE_PATH}（{len(cases)} 例）")
    for case in cases:
        print(f"  {case['text']!r}: frames={len(case['generated_frames'])} chunks={case['text_chunks']}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
