#!/usr/bin/env python3
"""生成 `omnicrawl-tts` TTS 音频 I/O 与声线库的对照数据集。

期望值来自 Python 真实现 `omnicrawl/tts/audio.py`（WAV 读写、线性重采样、参考音频加载）与
`omnicrawl/tts/custom_voices.py`（音色名校验、自定义音色库读写、内置音色行）。

WAV 样本以 Base64 存进数据集（含 8/16/24/32 位与单/双声道），写入用例则记录 Python 写出的
完整文件字节。

用法（仓库根目录）：

    python rust/tools/gen_tts_io_fixture.py
    cd rust && cargo test -p omnicrawl-tts --test tts_io_parity
"""

from __future__ import annotations

import base64
import os
import json
import shutil
import sys
import tempfile
import wave
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
FIXTURE_PATH = ROOT / "rust/crates/omnicrawl-tts/tests/fixtures/tts_io_parity.json"

sys.path.insert(0, str(ROOT))

import numpy as np  # noqa: E402

from omnicrawl.tts.audio import (  # noqa: E402
    load_reference_audio,
    read_wav,
    resample_linear,
    write_wav,
)
from omnicrawl.tts import custom_voices  # noqa: E402
from omnicrawl.tts.custom_voices import validate_voice_name  # noqa: E402

if not Path(sys.modules["omnicrawl.tts.audio"].__file__).resolve().is_relative_to(ROOT):
    raise SystemExit("导入到的 omnicrawl 不在本仓库内，先确认运行目录")


def pack(width: int, values: list[int]) -> bytes:
    if width == 1:
        return bytes((int(value) + 128) & 0xFF for value in values)
    if width == 2:
        return b"".join(int(value).to_bytes(2, "little", signed=True) for value in values)
    if width == 3:
        return b"".join((int(value) & 0xFFFFFF).to_bytes(3, "little") for value in values)
    return b"".join(int(value).to_bytes(4, "little", signed=True) for value in values)


def make_wav(path: Path, channels: int, width: int, rate: int, frames: list[list[int]]) -> None:
    values = [sample for frame in frames for sample in frame]
    with wave.open(str(path), "wb") as handle:
        handle.setnchannels(channels)
        handle.setsampwidth(width)
        handle.setframerate(rate)
        handle.writeframes(pack(width, values))


def rounded(value) -> list:
    array = np.asarray(value, dtype=np.float64)
    return np.round(array, 6).tolist()


def main() -> int:
    workdir = Path(tempfile.mkdtemp(prefix="omnicrawl-tts-io-"))
    try:
        root = workdir.resolve()
        samples = {
            "pcm8-mono.wav": (1, 1, 8000, [[0], [128], [255], [64]]),
            "pcm16-mono.wav": (1, 2, 16000, [[0], [16384], [-16384], [32767], [-32768]]),
            "pcm24-mono.wav": (1, 3, 24000, [[0], [4194304], [-4194304], [8388607], [-8388608]]),
            "pcm32-mono.wav": (1, 4, 32000, [[0], [1073741824], [-2147483648], [2147483647]]),
            "pcm16-stereo.wav": (
                2,
                2,
                44100,
                [[100, -100], [200, -200], [300, -300], [400, -400]],
            ),
        }
        read_cases = []
        for name, (channels, width, rate, frames) in samples.items():
            path = root / name
            make_wav(path, channels, width, rate, frames)
            waveform, sample_rate = read_wav(path)
            read_cases.append(
                {
                    "name": name,
                    "base64": base64.b64encode(path.read_bytes()).decode("ascii"),
                    "sample_rate": sample_rate,
                    "expected": rounded(waveform),
                }
            )

        resample_cases = []
        for source_rate, target_rate in [(8000, 16000), (16000, 8000), (16000, 16000), (22050, 16000)]:
            source = np.array([[0.0, 1.0, 0.0, -1.0, 0.5, -0.5]], dtype=np.float32)
            resample_cases.append(
                {
                    "source_rate": source_rate,
                    "target_rate": target_rate,
                    "source": rounded(source),
                    "expected": rounded(resample_linear(source, source_rate, target_rate)),
                }
            )

        reference_cases = [
            {
                "name": "pcm16-mono.wav",
                "target_sample_rate": 8000,
                "target_channels": 2,
                "expected": rounded(
                    load_reference_audio(
                        root / "pcm16-mono.wav", target_sample_rate=8000, target_channels=2
                    ).reshape(2, -1)
                ),
            },
            {
                "name": "pcm16-stereo.wav",
                "target_sample_rate": 44100,
                "target_channels": 1,
                "expected": rounded(
                    load_reference_audio(
                        root / "pcm16-stereo.wav", target_sample_rate=44100, target_channels=1
                    ).reshape(1, -1)
                ),
            },
        ]

        write_cases = []
        for name, waveform, rate in [
            ("written-mono.wav", np.array([0.0, 0.5, -0.5, 1.0, -1.0], dtype=np.float32), 16000),
            (
                "written-stereo.wav",
                np.array([[0.25, -0.25], [0.75, -0.75]], dtype=np.float32),
                8000,
            ),
        ]:
            path = root / name
            write_wav(path, waveform, rate)
            write_cases.append(
                {
                    "name": name,
                    "sample_rate": rate,
                    "waveform": rounded(waveform.reshape(1, -1) if waveform.ndim == 1 else waveform),
                    "base64": base64.b64encode(path.read_bytes()).decode("ascii"),
                }
            )

        validate_cases = []
        for value in ["Fairy", "  我的音色 1  ", "a" * 40, "", "   ", "a" * 41, "bad/name", "dot.name"]:
            try:
                result = validate_voice_name(value)
                validate_cases.append({"input": value, "ok": True, "result": result})
            except ValueError as exc:
                validate_cases.append({"input": value, "ok": False, "error": str(exc)})

        # 自定义音色库：把 home 指到临时目录，跑一遍写入/覆盖/删除并记录最终 JSON 文本。
        home = root / "home"
        home.mkdir(parents=True, exist_ok=True)
        previous_profile = os.environ.get("USERPROFILE")
        previous_home = os.environ.get("HOME")
        os.environ["USERPROFILE"] = str(home)
        os.environ["HOME"] = str(home)
        library_cases = []
        try:
            entry = custom_voices.add_custom_voice(
                voice="Fairy",
                prompt_audio_codes=[[1, 2, 3], [4, 5]],
                display_name="CN 我的克隆音色",
                audio_file="fairy_ref.wav",
                source_audio_path="source/ref.wav",
            )
            library_cases.append({"op": "add", "entry": entry})
            custom_voices.add_custom_voice(
                voice="Fairy", prompt_audio_codes=[[9]], display_name="", audio_file="", source_audio_path=""
            )
            library_cases.append({"op": "overwrite", "names": custom_voices.list_custom_voice_names()})
            path = custom_voices.custom_voices_path()
            library_cases.append(
                {"op": "library_text", "text": path.read_text(encoding="utf-8")}
            )
            library_cases.append(
                {"op": "delete", "removed": custom_voices.delete_custom_voice("Fairy")}
            )
            library_cases.append(
                {"op": "delete-again", "removed": custom_voices.delete_custom_voice("Fairy")}
            )
            library_cases.append(
                {"op": "names-after-delete", "names": custom_voices.list_custom_voice_names()}
            )
        finally:
            if previous_profile is None:
                os.environ.pop("USERPROFILE", None)
            else:
                os.environ["USERPROFILE"] = previous_profile
            if previous_home is None:
                os.environ.pop("HOME", None)
            else:
                os.environ["HOME"] = previous_home

        data = {
            "source": "omnicrawl/tts/audio.py + omnicrawl/tts/custom_voices.py",
            "read": read_cases,
            "resample": resample_cases,
            "reference": reference_cases,
            "write": write_cases,
            "validate": validate_cases,
            "library": library_cases,
        }
    finally:
        shutil.rmtree(workdir, ignore_errors=True)

    FIXTURE_PATH.parent.mkdir(parents=True, exist_ok=True)
    FIXTURE_PATH.write_text(
        json.dumps(data, ensure_ascii=False, indent=2) + "\n", encoding="utf-8"
    )
    print(
        f"已写入 {FIXTURE_PATH}（read {len(read_cases)}、resample {len(resample_cases)}、"
        f"reference {len(reference_cases)}、write {len(write_cases)}、"
        f"validate {len(validate_cases)}、library {len(library_cases)}）"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
