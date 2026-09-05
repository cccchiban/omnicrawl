"""MOSS-TTS-Nano ONNX 语音合成引擎。

无 PyTorch 依赖：ONNX Runtime CPU/CUDA 推理 + sentencepiece 分词 + 纯 Python 音频/文本处理。
高层流程（对齐官方 `OnnxTtsRuntime`）：
  文本归一化 → 解析音色（内置音色 / 参考音频语音克隆）→ 按 token 预算分块 →
  逐块自回归生成音频帧 → codec 解码成波形 → 拼接写出 WAV。
"""

from __future__ import annotations

import logging
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

import numpy as np
import sentencepiece as spm

from .audio import load_reference_audio, write_wav
from .config import TTSConfig
# 模型仓库/目录管理与下载已下沉到 download.py（仅标准库依赖），引擎只负责推理。
# 这里再导出供既有 ``from omnicrawl.tts.engine import ...`` 调用方兼容。
from .download import (
    ensure_model_dir,
)
from .normalize import WeTextNormalizer, prepare_tts_request_texts
from .onnx_runtime import (
    OrtCpuRuntime,
    _normalize_sample_mode,
    _resolve_stream_decode_frame_budget,
    EXECUTION_PROVIDER_CPU,
    SAMPLE_MODE_GREEDY,
)

LOGGER = logging.getLogger(__name__)

DEFAULT_VOICE_CLONE_INTER_CHUNK_PAUSE_SHORT_SECONDS = 0.40
DEFAULT_VOICE_CLONE_INTER_CHUNK_PAUSE_LONG_SECONDS = 0.24
SENTENCE_END_PUNCTUATION = set(".!?。！？；;")
CLAUSE_SPLIT_PUNCTUATION = set("，,、；;：:")
CLOSING_PUNCTUATION = set("\"'”’)]}）】》」』")


@dataclass
class TtsResult:
    """一次合成的结果。"""

    audio_path: Path
    sample_rate: int
    # 波形 float32，形状 [channels, samples]。
    waveform: np.ndarray
    duration_seconds: float = field(default=0.0)
    # 生成的音频 token 帧数。
    audio_token_frames: int = field(default=0)
    text_chunks: list[str] = field(default_factory=list)
    sample_mode: str = "fixed"
    voice: str = ""

    def __post_init__(self) -> None:
        if not self.duration_seconds and self.sample_rate and self.waveform.size:
            self.duration_seconds = float(self.waveform.shape[-1]) / float(self.sample_rate)


# ---------------------------------------------------------------------------
# 文本分句 / 分块工具（vendored from onnx_tts_runtime.py）
# ---------------------------------------------------------------------------


def _contains_cjk(text: str) -> bool:
    for character in str(text or ""):
        if (
            "\u4e00" <= character <= "\u9fff"
            or "\u3400" <= character <= "\u4dbf"
            or "\u3040" <= character <= "\u30ff"
            or "\uac00" <= character <= "\ud7af"
        ):
            return True
    return False


def _prepare_text_for_sentence_chunking(text: str) -> str:
    normalized_text = str(text or "").strip()
    if not normalized_text:
        raise ValueError("待合成的文本不能为空。")
    normalized_text = normalized_text.replace("\r", " ").replace("\n", " ")
    while "  " in normalized_text:
        normalized_text = normalized_text.replace("  ", " ")
    if _contains_cjk(normalized_text):
        if normalized_text[-1] not in SENTENCE_END_PUNCTUATION:
            normalized_text += "。"
        return normalized_text
    if normalized_text[:1].islower():
        normalized_text = normalized_text[:1].upper() + normalized_text[1:]
    if normalized_text[-1].isalnum():
        normalized_text += "."
    if len([item for item in normalized_text.split() if item]) < 5:
        normalized_text = f"        {normalized_text}"
    return normalized_text


def _split_text_by_punctuation(text: str, punctuation: set[str]) -> list[str]:
    sentences: list[str] = []
    current_chars: list[str] = []
    index = 0
    normalized_text = str(text or "")
    while index < len(normalized_text):
        character = normalized_text[index]
        current_chars.append(character)
        if character in punctuation:
            lookahead = index + 1
            while lookahead < len(normalized_text) and normalized_text[lookahead] in CLOSING_PUNCTUATION:
                current_chars.append(normalized_text[lookahead])
                lookahead += 1
            sentence = "".join(current_chars).strip()
            if sentence:
                sentences.append(sentence)
            current_chars.clear()
            while lookahead < len(normalized_text) and normalized_text[lookahead].isspace():
                lookahead += 1
            index = lookahead
            continue
        index += 1
    tail = "".join(current_chars).strip()
    if tail:
        sentences.append(tail)
    return sentences


def _join_sentence_parts(left: str, right: str) -> str:
    if not left:
        return right
    if not right:
        return left
    if _contains_cjk(left) or _contains_cjk(right):
        return left + right
    return f"{left} {right}"


def _merge_audio_channels(channel_arrays: list[np.ndarray]) -> np.ndarray:
    if not channel_arrays:
        return np.zeros((0, 1), dtype=np.float32)
    if len(channel_arrays) == 1:
        return np.asarray(channel_arrays[0], dtype=np.float32).reshape(-1, 1)
    min_length = min(int(channel.shape[0]) for channel in channel_arrays)
    trimmed = [np.asarray(channel[:min_length], dtype=np.float32) for channel in channel_arrays]
    return np.stack(trimmed, axis=1)


def _concat_waveforms(waveforms: list[np.ndarray]) -> np.ndarray:
    if not waveforms:
        return np.zeros((0, 1), dtype=np.float32)
    non_empty = [waveform for waveform in waveforms if waveform.size > 0]
    if not non_empty:
        channel_count = int(waveforms[0].shape[1]) if waveforms[0].ndim == 2 and waveforms[0].shape[1] > 0 else 1
        return np.zeros((0, channel_count), dtype=np.float32)
    return np.concatenate(non_empty, axis=0)


class TtsEngine:
    """MOSS-TTS-Nano ONNX CPU 语音合成引擎。

    用法：:

        engine = TtsEngine()          # 首次使用自动下载模型
        result = engine.synthesize("欢迎使用 MOSS-TTS-Nano。")
        print(result.audio_path)      # generated_audio/moss_tts_nano_output.wav
    """

    def __init__(self, config: TTSConfig | None = None, **kwargs: Any) -> None:
        if config is None:
            config = TTSConfig(**kwargs)
        elif kwargs:
            for key, value in kwargs.items():
                if not hasattr(config, key):
                    raise TypeError(f"未知的 TTS 配置项：{key}")
                setattr(config, key, value)
        self.config = config

        resolved_model_dir = ensure_model_dir(config.model_dir)
        self._runtime = OrtCpuRuntime(
            model_dir=resolved_model_dir,
            thread_count=config.thread_count,
            max_new_frames=config.max_new_frames,
            do_sample=config.do_sample,
            sample_mode=config.sample_mode,
            execution_provider=(
                config.execution_provider
                if config.device == "auto" and config.execution_provider != EXECUTION_PROVIDER_CPU
                else (config.device or config.execution_provider)
            ),
        )
        self.model_dir = resolved_model_dir
        self.manifest = self._runtime.manifest
        self.execution_provider = self._runtime.execution_provider

        # 采样参数覆盖。
        self.manifest["generation_defaults"].update(config.generation_overrides())

        # sentencepiece 分词器。
        tokenizer_relative_path = str(self.manifest["model_files"].get("tokenizer_model", "tokenizer.model"))
        tokenizer_path = self._runtime.resolve_manifest_relative_path(tokenizer_relative_path)
        self.sp_model = spm.SentencePieceProcessor(model_file=str(tokenizer_path))

        # 输出目录。
        self.output_dir = Path(config.output_dir).expanduser().resolve()
        self.output_dir.mkdir(parents=True, exist_ok=True)

        self._wetext_normalizer: WeTextNormalizer | None = None
        self._seed: int | None = config.seed

    # ------------------------------------------------------------------
    # 基础能力
    # ------------------------------------------------------------------

    @property
    def sample_rate(self) -> int:
        return int(self._runtime.codec_meta["codec_config"]["sample_rate"])

    @property
    def channels(self) -> int:
        return int(self._runtime.codec_meta["codec_config"]["channels"])

    def list_builtin_voices(self) -> list[dict[str, Any]]:
        return list(self._runtime.list_builtin_voices())

    def list_text_samples(self) -> list[dict[str, Any]]:
        return list(self._runtime.list_text_samples())

    def warmup(self) -> None:
        """预热所有 ONNX session（prefill/decode/codec），降低首次合成延迟。"""
        self._runtime.warmup()

    def close(self) -> None:
        """释放会话引用；引擎实例可被垃圾回收。"""
        self._runtime.sessions.clear()

    def __enter__(self) -> "TtsEngine":
        return self

    def __exit__(self, *exc_info: Any) -> None:
        self.close()

    # ------------------------------------------------------------------
    # 分词 / 文本分块
    # ------------------------------------------------------------------

    def encode_text(self, text: str) -> list[int]:
        return [int(token_id) for token_id in self.sp_model.encode(str(text or ""), out_type=int)]

    def count_text_tokens(self, text: str) -> int:
        return len(self.encode_text(text))

    def split_text_by_token_budget(self, text: str, max_tokens: int) -> list[str]:
        remaining_text = str(text or "").strip()
        if not remaining_text:
            return []
        pieces: list[str] = []
        preferred_boundary_chars = set(CLAUSE_SPLIT_PUNCTUATION) | set(SENTENCE_END_PUNCTUATION) | {" "}
        while remaining_text:
            if self.count_text_tokens(remaining_text) <= max_tokens:
                pieces.append(remaining_text)
                break
            low = 1
            high = len(remaining_text)
            best_prefix_length = 1
            while low <= high:
                middle = (low + high) // 2
                candidate = remaining_text[:middle].strip()
                if not candidate:
                    low = middle + 1
                    continue
                if self.count_text_tokens(candidate) <= max_tokens:
                    best_prefix_length = middle
                    low = middle + 1
                else:
                    high = middle - 1
            cut_index = best_prefix_length
            prefix = remaining_text[:best_prefix_length]
            preferred_index = -1
            scan_min = max(-1, len(prefix) - 25)
            for scan_index in range(len(prefix) - 1, scan_min, -1):
                if prefix[scan_index] in preferred_boundary_chars:
                    preferred_index = scan_index + 1
                    break
            if preferred_index > 0:
                cut_index = preferred_index
            piece = remaining_text[:cut_index].strip()
            if not piece:
                piece = remaining_text[:best_prefix_length].strip()
                cut_index = best_prefix_length
            pieces.append(piece)
            remaining_text = remaining_text[cut_index:].strip()
        return pieces

    def split_voice_clone_text(self, text: str, max_tokens: int = 75) -> list[str]:
        normalized_text = str(text or "").strip()
        if not normalized_text:
            return []
        safe_max_tokens = max(1, int(max_tokens))
        prepared_text = _prepare_text_for_sentence_chunking(normalized_text)
        sentence_candidates = _split_text_by_punctuation(prepared_text, SENTENCE_END_PUNCTUATION) or [
            prepared_text.strip()
        ]
        sentence_slices: list[tuple[int, str]] = []
        for sentence_text in sentence_candidates:
            normalized_sentence = sentence_text.strip()
            if not normalized_sentence:
                continue
            sentence_token_count = self.count_text_tokens(normalized_sentence)
            if sentence_token_count <= safe_max_tokens:
                sentence_slices.append((sentence_token_count, normalized_sentence))
                continue
            clause_candidates = _split_text_by_punctuation(normalized_sentence, CLAUSE_SPLIT_PUNCTUATION)
            if len(clause_candidates) <= 1:
                clause_candidates = [normalized_sentence]
            for clause_text in clause_candidates:
                normalized_clause = clause_text.strip()
                if not normalized_clause:
                    continue
                clause_token_count = self.count_text_tokens(normalized_clause)
                if clause_token_count <= safe_max_tokens:
                    sentence_slices.append((clause_token_count, normalized_clause))
                    continue
                for piece in self.split_text_by_token_budget(normalized_clause, safe_max_tokens):
                    normalized_piece = piece.strip()
                    if normalized_piece:
                        sentence_slices.append((self.count_text_tokens(normalized_piece), normalized_piece))
        chunks: list[str] = []
        current_chunk = ""
        current_chunk_token_count = 0
        for sentence_token_count, sentence_text in sentence_slices:
            if not current_chunk:
                current_chunk = sentence_text
                current_chunk_token_count = sentence_token_count
                continue
            if current_chunk_token_count + sentence_token_count > safe_max_tokens:
                chunks.append(current_chunk.strip())
                current_chunk = sentence_text
                current_chunk_token_count = sentence_token_count
            else:
                current_chunk = _join_sentence_parts(current_chunk, sentence_text)
                current_chunk_token_count = self.count_text_tokens(current_chunk)
        if current_chunk:
            chunks.append(current_chunk.strip())
        return chunks if len(chunks) > 1 else [normalized_text]

    def estimate_voice_clone_inter_chunk_pause_seconds(self, text_chunk: str) -> float:
        word_count = len([item for item in str(text_chunk or "").strip().split() if item])
        return (
            DEFAULT_VOICE_CLONE_INTER_CHUNK_PAUSE_SHORT_SECONDS
            if word_count <= 4
            else DEFAULT_VOICE_CLONE_INTER_CHUNK_PAUSE_LONG_SECONDS
        )

    # ------------------------------------------------------------------
    # 音色解析
    # ------------------------------------------------------------------

    def _wetext(self) -> WeTextNormalizer | None:
        if not self.config.enable_wetext:
            return None
        if self._wetext_normalizer is None:
            self._wetext_normalizer = WeTextNormalizer()
        if not self._wetext_normalizer.available:
            LOGGER.warning(
                "enable_wetext=True 但 WeTextProcessing 未安装（需要 pynini），"
                "本次合成将降级为纯 Python 稳健清洗。"
            )
            return None
        return self._wetext_normalizer

    def encode_reference_audio(self, reference_audio_path: str | Path) -> list[list[int]]:
        """把参考音频编码成 prompt audio codes（语音克隆音色）。"""
        waveform = load_reference_audio(
            reference_audio_path,
            target_sample_rate=self.sample_rate,
            target_channels=self.channels,
        )
        waveform_length = int(waveform.shape[-1])
        outputs = self._runtime.sessions["codec_encode"].run(
            None,
            {
                "waveform": waveform,
                "input_lengths": np.asarray([waveform_length], dtype=np.int32),
            },
        )
        output_names = [output.name for output in self._runtime.sessions["codec_encode"].get_outputs()]
        named_outputs = dict(zip(output_names, outputs))
        audio_codes = np.asarray(named_outputs["audio_codes"], dtype=np.int32)
        audio_code_lengths = np.asarray(named_outputs["audio_code_lengths"], dtype=np.int32)
        code_length = int(audio_code_lengths.reshape(-1)[0])
        num_quantizers = int(self._runtime.codec_meta["codec_config"]["num_quantizers"])
        prompt_audio_codes: list[list[int]] = []
        for frame_index in range(code_length):
            prompt_audio_codes.append(
                [int(audio_codes[0, frame_index, quantizer_index]) for quantizer_index in range(num_quantizers)]
            )
        return prompt_audio_codes

    def resolve_prompt_audio_codes(
        self,
        *,
        voice: str | None,
        prompt_audio_path: str | Path | None,
    ) -> list[list[int]]:
        if prompt_audio_path:
            return self.encode_reference_audio(prompt_audio_path)
        resolved_voice = str(voice or self.list_builtin_voices()[0]["voice"])
        voice_row = next(
            (item for item in self.list_builtin_voices() if item["voice"] == resolved_voice),
            None,
        )
        if voice_row is None:
            raise ValueError(f"内置音色不存在：{resolved_voice}。可用：{', '.join(v['voice'] for v in self.list_builtin_voices())}")
        return list(voice_row["prompt_audio_codes"])

    # ------------------------------------------------------------------
    # 合成
    # ------------------------------------------------------------------

    def decode_full_audio_safe(self, generated_frames: list[list[int]]) -> np.ndarray:
        """整段 codec 解码，失败时回退到增量流式解码。"""
        try:
            channel_arrays, _audio_length = self._runtime.decode_full_audio(generated_frames)
            return _merge_audio_channels(channel_arrays)
        except Exception as exc:
            LOGGER.warning("full codec decode failed, falling back to incremental decode: %s", exc)
            self._runtime.codec_streaming_session.reset()
            merged_by_channel: list[list[np.ndarray]] = [
                [] for _ in range(self.channels)
            ]
            try:
                for start_index in range(0, len(generated_frames), 8):
                    frame_chunk = generated_frames[start_index : start_index + 8]
                    decoded = self._runtime.codec_streaming_session.run_frames(frame_chunk)
                    if decoded is None:
                        continue
                    audio, audio_length = decoded
                    if audio_length <= 0:
                        continue
                    for channel_index, channel in enumerate(audio[0, :, :audio_length]):
                        merged_by_channel[channel_index].append(np.asarray(channel, dtype=np.float32))
            finally:
                self._runtime.codec_streaming_session.reset()
            return _merge_audio_channels(
                [np.concatenate(chunks) if chunks else np.zeros((0,), dtype=np.float32) for chunks in merged_by_channel]
            )

    def synthesize_single_chunk(
        self,
        *,
        text: str,
        prompt_audio_codes: list[list[int]],
        streaming: bool,
    ) -> dict[str, Any]:
        """合成单个文本块（自回归生成帧 + codec 解码成波形）。"""
        text_token_ids = self.encode_text(text)
        request_rows = self._runtime.build_voice_clone_request_rows(prompt_audio_codes, text_token_ids)
        if not streaming:
            generated_frames = self._runtime.generate_audio_frames(request_rows)
            waveform = self.decode_full_audio_safe(generated_frames)
            return {
                "text": text,
                "text_token_ids": text_token_ids,
                "generated_frames": generated_frames,
                "waveform": waveform,
            }

        pending_decode_frames: list[list[int]] = []
        emitted_chunks: list[np.ndarray] = []
        emitted_samples_total = 0
        first_audio_emitted_at_perf: float | None = None
        self._runtime.codec_streaming_session.reset()

        def decode_pending_frames(force: bool) -> None:
            nonlocal emitted_samples_total, first_audio_emitted_at_perf
            pending_count = len(pending_decode_frames)
            if pending_count <= 0:
                return
            sample_rate = self.sample_rate
            decode_budget = _resolve_stream_decode_frame_budget(
                emitted_samples_total,
                sample_rate,
                first_audio_emitted_at_perf,
            )
            if not force and pending_count < max(1, decode_budget):
                return
            frame_budget = pending_count if force else min(pending_count, max(1, decode_budget))
            frame_chunk = pending_decode_frames[:frame_budget]
            del pending_decode_frames[:frame_budget]
            decoded = self._runtime.codec_streaming_session.run_frames(frame_chunk)
            if decoded is None:
                return
            audio, audio_length = decoded
            if audio_length <= 0:
                return
            if first_audio_emitted_at_perf is None:
                first_audio_emitted_at_perf = time.perf_counter()
            emitted_samples_total += audio_length
            emitted_chunks.append(
                _merge_audio_channels(
                    [audio[0, channel_index, :audio_length] for channel_index in range(audio.shape[1])]
                )
            )

        def on_frame(_generated_frames: list[list[int]], _step_index: int, frame: list[int]) -> None:
            pending_decode_frames.append(list(frame))
            decode_pending_frames(False)

        try:
            generated_frames = self._runtime.generate_audio_frames(request_rows, on_frame=on_frame)
            decode_pending_frames(True)
        finally:
            self._runtime.codec_streaming_session.reset()
        waveform = _concat_waveforms(emitted_chunks)
        return {
            "text": text,
            "text_token_ids": text_token_ids,
            "generated_frames": generated_frames,
            "waveform": waveform,
        }

    def synthesize(
        self,
        text: str,
        *,
        voice: str | None = None,
        prompt_audio_path: str | Path | None = None,
        output_path: str | Path | None = None,
        sample_mode: str | None = None,
        do_sample: bool | None = None,
        streaming: bool | None = None,
        max_new_frames: int | None = None,
        voice_clone_max_text_tokens: int | None = None,
        seed: int | None = None,
    ) -> TtsResult:
        """把文本合成为语音并写出 WAV，返回 `TtsResult`。

        参数缺省时使用 `TTSConfig` 的对应值；显式传参仅覆盖本次调用。
        """
        config = self.config
        if max_new_frames is not None:
            self.manifest["generation_defaults"]["max_new_frames"] = int(max_new_frames)
        effective_do_sample = config.do_sample if do_sample is None else bool(do_sample)
        effective_sample_mode = _normalize_sample_mode(
            sample_mode if sample_mode is not None else config.sample_mode,
            effective_do_sample,
        )
        self.manifest["generation_defaults"]["sample_mode"] = effective_sample_mode
        self.manifest["generation_defaults"]["do_sample"] = effective_sample_mode != SAMPLE_MODE_GREEDY
        if seed is not None or self._seed is not None:
            self._runtime.rng = np.random.default_rng(int(seed if seed is not None else self._seed))

        effective_voice = voice if voice is not None else config.voice
        effective_prompt_audio = prompt_audio_path if prompt_audio_path is not None else config.prompt_audio_path
        effective_wetext = self._wetext() is not None

        prepared_texts = prepare_tts_request_texts(
            text=text,
            voice=str(effective_voice or ""),
            enable_wetext=effective_wetext,
            enable_normalize_tts_text=config.enable_normalize_tts_text,
            wetext_normalizer=self._wetext() if effective_wetext else None,
        )
        prepared_text = str(prepared_texts["text"])
        LOGGER.info(
            "text normalization method=%s language=%s text_chars=%d",
            prepared_texts["normalization_method"],
            prepared_texts["text_normalization_language"] or "n/a",
            len(prepared_text),
        )

        prompt_audio_codes = self.resolve_prompt_audio_codes(
            voice=effective_voice,
            prompt_audio_path=effective_prompt_audio,
        )
        max_tokens = (
            int(voice_clone_max_text_tokens)
            if voice_clone_max_text_tokens is not None
            else int(config.voice_clone_max_text_tokens)
        )
        text_chunks = self.split_voice_clone_text(prepared_text, max_tokens=max_tokens)
        effective_streaming = config.streaming if streaming is None else bool(streaming)

        all_waveforms: list[np.ndarray] = []
        all_generated_frames: list[list[int]] = []
        sample_rate = self.sample_rate
        channels = self.channels
        for chunk_index, chunk_text in enumerate(text_chunks):
            chunk_result = self.synthesize_single_chunk(
                text=chunk_text,
                prompt_audio_codes=prompt_audio_codes,
                streaming=effective_streaming,
            )
            all_waveforms.append(np.asarray(chunk_result["waveform"], dtype=np.float32))
            all_generated_frames.extend(chunk_result["generated_frames"])
            if chunk_index < len(text_chunks) - 1:
                pause_seconds = self.estimate_voice_clone_inter_chunk_pause_seconds(chunk_text)
                pause_samples = max(0, int(round(sample_rate * pause_seconds)))
                if pause_samples > 0:
                    all_waveforms.append(np.zeros((pause_samples, channels), dtype=np.float32))
        waveform = _concat_waveforms(all_waveforms)
        # 移植代码内部用交错布局 [samples, channels]；对外统一为 [channels, samples]。
        waveform = np.ascontiguousarray(waveform.T) if waveform.ndim == 2 else waveform

        resolved_output_path = (
            Path(output_path).expanduser().resolve()
            if output_path
            else (self.output_dir / "moss_tts_nano_output.wav").resolve()
        )
        audio_path = write_wav(resolved_output_path, waveform, sample_rate)
        return TtsResult(
            audio_path=audio_path,
            sample_rate=sample_rate,
            waveform=waveform,
            audio_token_frames=len(all_generated_frames),
            text_chunks=text_chunks,
            sample_mode=effective_sample_mode,
            voice=str(effective_voice or ""),
        )
