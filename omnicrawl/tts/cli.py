"""命令行入口：``python -m omnicrawl.tts``。

示例::

    python -m omnicrawl.tts --text "欢迎使用 MOSS-TTS-Nano。" --voice Junhao
    python -m omnicrawl.tts --text "你好" --prompt-audio ref.wav --output out.wav
    python -m omnicrawl.tts --list-voices
"""

from __future__ import annotations

import argparse
import logging
import sys
from pathlib import Path
from typing import Sequence

from .config import TTSConfig
from .engine import TtsEngine


def parse_args(argv: Sequence[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(prog="omnicrawl.tts", description="MOSS-TTS-Nano ONNX 语音合成")
    parser.add_argument("--text", help="要合成的文本。")
    parser.add_argument("--text-file", help="UTF-8 文本文件路径（与 --text 二选一）。")
    parser.add_argument("--voice", default=None, help="内置音色名（未提供参考音频时使用）。")
    parser.add_argument(
        "--prompt-audio",
        "--reference-audio",
        dest="prompt_audio",
        default=None,
        help="语音克隆参考音频路径（提供时覆盖 --voice）。",
    )
    parser.add_argument("--output", default=None, help="输出 WAV 路径。")
    parser.add_argument("--model-dir", default=None, help="模型目录；缺省时自动下载到默认目录。")
    parser.add_argument("--output-dir", default="generated_audio", help="输出目录（未指定 --output 时）。")
    parser.add_argument("--cpu-threads", type=int, default=4, help="onnxruntime CPU intra-op 线程数。")
    parser.add_argument(
        "--device",
        choices=("auto", "cpu", "cuda"),
        default="auto",
        help="推理设备：auto 优先 CUDA、不可用时回退 CPU；cuda 不可用时直接报错。",
    )
    parser.add_argument("--sample-mode", choices=("greedy", "fixed", "full"), default="fixed")
    parser.add_argument("--do-sample", type=int, choices=[0, 1], default=1, help="是否采样（0 时强制 greedy）。")
    parser.add_argument("--streaming", action="store_true", help="使用 codec 流式解码。")
    parser.add_argument("--no-play", action="store_true", help="合成后不自动播放。")
    parser.add_argument("--max-new-frames", type=int, default=375)
    parser.add_argument("--voice-clone-max-text-tokens", type=int, default=75)
    parser.add_argument("--seed", type=int, default=None)
    parser.add_argument("--enable-wetext", action="store_true", help="启用 WeTextProcessing 语义归一化（需 pynini）。")
    parser.add_argument("--warmup", action="store_true", help="预热模型后退出（不合成）。")
    parser.add_argument("--list-voices", action="store_true", help="列出内置音色后退出。")
    parser.add_argument("-v", "--verbose", action="store_true", help="输出调试日志。")
    return parser.parse_args(argv)


def main(argv: Sequence[str] | None = None) -> int:
    args = parse_args(argv)
    logging.basicConfig(
        format="%(asctime)s %(levelname)s %(name)s: %(message)s",
        level=logging.DEBUG if args.verbose else logging.INFO,
    )

    config = TTSConfig(
        model_dir=args.model_dir,
        thread_count=args.cpu_threads,
        device=args.device,
        sample_mode=args.sample_mode,
        do_sample=bool(args.do_sample),
        max_new_frames=args.max_new_frames,
        voice=args.voice or "Junhao",
        prompt_audio_path=args.prompt_audio,
        output_dir=args.output_dir,
        streaming=args.streaming,
        voice_clone_max_text_tokens=args.voice_clone_max_text_tokens,
        enable_wetext=args.enable_wetext,
        seed=args.seed,
    )

    with TtsEngine(config) as engine:
        if args.list_voices:
            print("内置音色：")
            for voice_row in engine.list_builtin_voices():
                print(f"  - {voice_row['voice']}")
            return 0
        if args.warmup:
            engine.warmup()
            print("预热完成。")
            return 0

        if args.text is not None:
            text = args.text
        elif args.text_file:
            text = Path(args.text_file).read_text(encoding="utf-8")
        else:
            print("错误：必须提供 --text 或 --text-file。", file=sys.stderr)
            return 2

        result = engine.synthesize(
            text,
            voice=config.voice,
            prompt_audio_path=config.prompt_audio_path,
            output_path=args.output,
        )
        if not args.no_play:
            from .player import play_wav

            # 同步播放：等播完再退出，否则进程退出会终止异步播放导致无声。
            play_wav(result.audio_path, blocking=True)
        print(
            f"合成完成：{result.audio_path}  "
            f"采样率={result.sample_rate}Hz 时长={result.duration_seconds:.2f}s "
            f"帧数={result.audio_token_frames} 分块={len(result.text_chunks)} "
            f"模式={result.sample_mode}"
        )
        return 0


if __name__ == "__main__":
    raise SystemExit(main())
