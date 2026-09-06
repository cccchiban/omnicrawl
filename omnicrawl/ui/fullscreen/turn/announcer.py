"""主 TUI 回合回复的自动朗读器（程序侧播报，不依赖模型调用 TTS 工具）。

模型流式正文（不含推理与工具调用）由 UI 逐片喂入 ``feed_text``；本模块按
段落切分后交给最多 ``max_concurrency`` 个合成线程合成（每线程独立持有
自己的 TtsEngine，避免共享 ONNX session 的并发读写），再由单一播放线程
严格按段落序号顺序播放，保证先输出先朗读、不互相打断。

内存注意：每个合成 worker 会独立加载整套 MOSS-TTS ONNX 权重（实测一套约
+1.8GB RSS / +2.2GB commit），并发数即内存倍数。默认 ``max_concurrency=1``
串行合成：实测多段连续合成无累积增长，单 worker 足够维持先输出先朗读，
同时把常驻内存压到单引擎水平，避免语音会话把内存/页面文件占满拖垮 TUI。

段落语义：以空行分隔的块为自然段落；块内单换行不算段落边界。为避免单段
无限增长，超过阈值后在最近的句末标点处切段；找不到标点则在阈值处硬切。
回合结束时由 UI 调用 ``flush_turn`` 把残留文本提交为最后一段。

本模块刻意不依赖 Textual/Agent，便于独立单元测试：引擎工厂与播放函数均可
注入替身。TTS 未启用或模型未就绪时静默丢弃文本，绝不触发模型下载。
"""

from __future__ import annotations

import logging
import shutil
import tempfile
import threading
import uuid
from collections import deque
from pathlib import Path
from typing import Any, Callable

LOGGER = logging.getLogger(__name__)

_SENTENCE_END = "。！？!?；;"
# 超过该长度后遇到句末标点即切段（保证实时性，不攒成长段）。
_MIN_SENTENCE_CHARS = 30
# 无标点可切时的硬切阈值。
_MAX_PARAGRAPH_CHARS = 240
# 硬切时句末标点回溯窗口：只在窗口内找标点，避免 O(n²) 扫描。
_CUT_LOOKBACK_CHARS = 60


def _find_cut_index(buffer: str) -> int | None:
    """返回 buffer 中第一个可切段边界下标；无边界返回 None。

    边界优先级：空行 > （长度 ≥ _MIN_SENTENCE_CHARS 后的）句末标点 >
    超长硬切。短于下限的句子继续缓冲等待后续内容，避免把半句切碎；
    超过下限后遇到句末标点即切段，让模型边输出边朗读。返回下标指向
    段落最后一个字符之后（不含其后的换行/空格）。保证返回下标 >= 1，
    避免零宽切分死循环。
    """
    if not buffer:
        return None
    # 空行边界：直接切在段落结束处。
    blank_index = buffer.find("\n\n")
    if blank_index > 0:
        return blank_index
    if len(buffer) >= _MIN_SENTENCE_CHARS:
        scan_end = min(len(buffer), _MAX_PARAGRAPH_CHARS)
        for index in range(scan_end - 1, -1, -1):
            if buffer[index] in _SENTENCE_END:
                cut = index + 1
                # 吞掉紧跟的换行/空格，避免下段以空白开头。
                while cut < len(buffer) and buffer[cut] in " \n\r\t":
                    cut += 1
                return cut if cut > 0 else None
    if len(buffer) >= _MAX_PARAGRAPH_CHARS:
        # 无标点超长兜底：在窗口内找标点，找不到则硬切。
        window = buffer[-_CUT_LOOKBACK_CHARS:]
        for index in range(len(window) - 1, -1, -1):
            if window[index] in _SENTENCE_END:
                return len(buffer) - len(window) + index + 1
        return _MAX_PARAGRAPH_CHARS
    return None


class TurnSpeechAnnouncer:
    """流式段落朗读调度器：受限并发合成（≤ max_concurrency）+ 顺序播放。

    线程模型：
    - UI 主线程调用 ``feed_text``/``flush_turn`` 提交段落（分配递增序号）；
    - 合成线程池每线程持有独立 TtsEngine，从待处理队列取段合成到临时目录；
    - 播放线程严格按序号推进：只播放当前最小序号对应的已合成文件。

    每个合成 worker 独立加载整套 TTS 模型权重（约 2GB 级内存），因此默认
    并发为 1：单引擎串行合成即可满足边输出边朗读，同时避免多套权重同时
    驻留内存拖垮 TUI（语音会话高内存/卡顿主因）。测试可通过显式传
    ``max_concurrency`` 提高并发验证上限语义。

    段落可能因取消/回滚/合成失败而不产生音频：播放线程通过
    ``_discarded``（被取消回合的序号闭区间集合）与 ``_skipped``（个别
    合成失败序号）跳过空洞，绝不永久等待。回滚/取消只影响其对应回合
    （从回合起始序号开始），更早回合已提交内容不受影响。
    """

    def __init__(
        self,
        config_provider: Callable[[], Any],
        *,
        engine_factory: Callable[[Any], Any] | None = None,
        player: Callable[[str], None] | None = None,
        max_concurrency: int = 1,
    ) -> None:
        self._config_provider = config_provider
        self._engine_factory = engine_factory
        self._player = player or self._default_player
        self._max_concurrency = max(1, int(max_concurrency))
        self._wake = threading.Condition()
        # 待合成队列与已合成结果，按分配序号有序推进。
        self._pending: deque[tuple[int, str]] = deque()
        self._ready: dict[int, str] = {}
        # 合成失败序号（不产生音频）。
        self._skipped: set[int] = set()
        # 被丢弃回合的序号闭区间 [start, end]。
        self._discarded: list[tuple[int, int]] = []
        self._next_alloc = 0
        self._next_play = 0
        # 当前回合的起始序号；None 表示无活动回合。
        self._turn_base: int | None = None
        self._buffer = ""
        self._closed = False
        self._started = False
        self._worker_threads: list[threading.Thread] = []
        self._player_thread: threading.Thread | None = None
        self._tmp_dir: str | None = None
        self._worker_local = threading.local()

    # ------------------------------------------------------------------
    # 生命周期
    # ------------------------------------------------------------------

    def _ensure_started(self) -> None:
        if self._started or self._closed:
            return
        self._started = True
        self._tmp_dir = tempfile.mkdtemp(prefix="omnicrawl-tts-announce-")
        for _index in range(self._max_concurrency):
            thread = threading.Thread(
                target=self._synthesize_loop,
                name=f"tts-announce-synth-{_index}",
                daemon=True,
            )
            thread.start()
            self._worker_threads.append(thread)
        self._player_thread = threading.Thread(
            target=self._play_loop,
            name="tts-announce-player",
            daemon=True,
        )
        self._player_thread.start()

    def close(self) -> None:
        """停止合成/播放线程并释放引擎与临时文件；幂等。"""
        with self._wake:
            if self._closed:
                return
            self._closed = True
            self._pending.clear()
            self._wake.notify_all()
        for thread in self._worker_threads:
            thread.join(timeout=5)
        if self._player_thread is not None:
            self._player_thread.join(timeout=5)
        engine = getattr(self._worker_local, "engine", None)
        if engine is not None:
            try:
                engine.close()
            except Exception:  # noqa: BLE001 - 关闭失败不阻塞退出
                pass
        self._worker_local = threading.local()
        tmp_dir = self._tmp_dir
        self._tmp_dir = None
        if tmp_dir:
            shutil.rmtree(tmp_dir, ignore_errors=True)

    # ------------------------------------------------------------------
    # 回合协议（UI 主线程调用）
    # ------------------------------------------------------------------

    def on_turn_start(self) -> None:
        """回合开始：记录本回合的队列起点，供回滚/取消丢弃未播段落。"""
        with self._wake:
            self._turn_base = self._next_alloc
            self._buffer = ""

    def feed_text(self, text: str) -> None:
        """追加流式正文；切出完整段落后提交合成。

        仅活跃回合（``on_turn_start`` 之后、``flush_turn`` 之前）的文本会被
        朗读；无活跃回合（慢命令、子代理流程等）时直接丢弃，避免误播。
        """
        if not text or not self._enabled():
            return
        with self._wake:
            if self._closed or self._turn_base is None:
                return
            self._buffer = (self._buffer + text).lstrip("\n")
            submitted = False
            while True:
                cut = _find_cut_index(self._buffer)
                if cut is None or cut <= 0:
                    break
                paragraph = self._buffer[:cut].strip()
                self._buffer = self._buffer[cut:].lstrip("\n")
                if paragraph:
                    if not submitted:
                        # 首个段落才启动后台线程（避免空缓冲空转）。
                        self._ensure_started()
                        submitted = True
                    self._pending.append((self._next_alloc, paragraph))
                    self._next_alloc += 1
            if submitted:
                self._wake.notify_all()

    def flush_turn(self, *, cancelled: bool) -> None:
        """回合结束：活跃回合提交残留段；取消/无回合时丢弃缓冲。"""
        if not self._enabled():
            return
        with self._wake:
            if self._closed:
                return
            active = self._turn_base is not None
            paragraph = self._buffer.strip() if active else ""
            self._buffer = ""
            if cancelled or not active:
                if active:
                    self._discard_active_turn_locked()
                self._turn_base = None
                self._wake.notify_all()
                return
            # 只有确有残留段时才启动后台线程（纯工具回合不空转）。
            if paragraph:
                self._ensure_started()
                self._pending.append((self._next_alloc, paragraph))
                self._next_alloc += 1
            self._turn_base = None
            self._wake.notify_all()

    def rollback_turn(self) -> None:
        """模型流中断重试：丢弃本回合已提交但尚未播放的内容。"""
        with self._wake:
            if self._closed or self._turn_base is None:
                return
            self._discard_active_turn_locked()
            self._buffer = ""
            self._wake.notify_all()

    def cancel_all(self) -> None:
        """清空全部未播段落（App 关闭前兜底）。"""
        with self._wake:
            if self._closed:
                return
            if self._next_alloc > self._next_play:
                self._discarded.append((self._next_play, self._next_alloc - 1))
            self._turn_base = None
            self._buffer = ""
            self._wake.notify_all()

    # ------------------------------------------------------------------
    # 内部实现
    # ------------------------------------------------------------------

    def _enabled(self) -> bool:
        """按当前 TTS 配置判断自动朗读是否可用；绝不触发模型下载。"""
        try:
            config = self._config_provider()
        except Exception:  # noqa: BLE001 - 配置异常按停用处理
            return False
        if config is None or not bool(getattr(config, "enabled", False)):
            return False
        # 注入替身工厂（测试/扩展）时跳过模型就绪检查：替身不需要真实模型。
        if self._engine_factory is not None:
            return True
        try:
            from omnicrawl.tts import models_ready

            model_dir = getattr(config, "resolved_model_dir", None)
            resolved = model_dir() if callable(model_dir) else None
            return bool(models_ready(resolved))
        except Exception:  # noqa: BLE001
            return False

    def _discard_active_turn_locked(self) -> None:
        """把当前活跃回合（起始序号为 _turn_base）的全部段落标记为丢弃。

        更早回合（序号 < _turn_base）已 flush 提交的内容不受影响，仍按序
        播完；正在合成的段落完成后由合成线程检查区间丢弃音频。
        """
        base = self._turn_base
        if base is None:
            return
        last = self._next_alloc - 1
        if last >= base:
            self._discarded.append((base, last))
            # 待合成队列中本回合段落直接移除。
            self._pending = deque(
                (seq, text) for seq, text in self._pending if seq < base
            )
        self._wake.notify_all()

    def _is_discarded(self, seq: int) -> bool:
        return any(start <= seq <= end for start, end in self._discarded)

    def _prune_history_locked(self) -> None:
        """清理播放游标之前的丢弃/失败记录，防止长会话无界增长。

        ``_skipped`` 与 ``_discarded`` 中的区间只对 ≥ ``_next_play`` 的序号
        有意义；播放游标推进后，旧记录不再被查询，长期运行（数小时语音
        会话、数千段落）会累积成可观内存。每次播放线程跳过/播完一段后
        惰性压缩：保留跨过当前游标的区间，其余移除。
        """
        cursor = self._next_play
        kept: list[tuple[int, int]] = []
        for start, end in self._discarded:
            if end >= cursor:
                kept.append((max(start, cursor), end))
        self._discarded = kept
        self._skipped = {seq for seq in self._skipped if seq >= cursor}

    def _synthesize_loop(self) -> None:
        while True:
            with self._wake:
                while not self._pending and not self._closed:
                    self._wake.wait()
                if self._closed:
                    break
                seq, text = self._pending.popleft()
            if self._is_discarded(seq):
                continue
            try:
                path = self._synthesize(text)
            except Exception as exc:  # noqa: BLE001 - 单段失败丢弃，不中断后续
                LOGGER.warning("自动朗读段落合成失败（seq=%d）：%s", seq, exc)
                with self._wake:
                    self._skipped.add(seq)
                    self._wake.notify_all()
                continue
            with self._wake:
                if self._closed:
                    try:
                        Path(path).unlink(missing_ok=True)
                    except Exception:  # noqa: BLE001
                        pass
                    break
                if self._is_discarded(seq):
                    try:
                        Path(path).unlink(missing_ok=True)
                    except Exception:  # noqa: BLE001
                        pass
                    continue
                self._ready[seq] = str(path)
                self._wake.notify_all()
        # 线程退出前释放本线程持有的引擎会话。
        engine = getattr(self._worker_local, "engine", None)
        if engine is not None:
            try:
                engine.close()
            except Exception:  # noqa: BLE001
                pass

    def _play_loop(self) -> None:
        while True:
            with self._wake:
                while True:
                    if self._closed:
                        return
                    if self._next_play in self._ready:
                        path = self._ready.pop(self._next_play)
                        self._next_play += 1
                        self._prune_history_locked()
                        break
                    if self._is_discarded(self._next_play) or (
                        self._next_play in self._skipped
                    ):
                        # 被丢弃/失败的段落：清理可能残留的音频后跳过。
                        leftover = self._ready.pop(self._next_play, None)
                        if leftover is not None:
                            try:
                                Path(leftover).unlink(missing_ok=True)
                            except Exception:  # noqa: BLE001
                                pass
                        self._next_play += 1
                        self._prune_history_locked()
                        continue
                    self._wake.wait()
            try:
                self._player(path)
            except Exception as exc:  # noqa: BLE001 - 播放失败不中断后续
                LOGGER.warning("自动朗读播放失败：%s", exc)
            finally:
                try:
                    Path(path).unlink(missing_ok=True)
                except Exception:  # noqa: BLE001
                    pass

    def _engine(self) -> Any:
        """返回当前线程持有的 TtsEngine；配置签名变化时重建。"""
        config = self._config_provider()
        signature = None
        if config is not None:
            signature = (
                str(getattr(config, "model_dir", "") or ""),
                int(getattr(config, "thread_count", 4) or 4),
                str(getattr(config, "device", "auto") or "auto"),
                str(getattr(config, "voice", "Junhao") or "Junhao"),
            )
        engine = getattr(self._worker_local, "engine", None)
        if engine is not None and self._worker_local.signature == signature:
            return engine
        if engine is not None:
            try:
                engine.close()
            except Exception:  # noqa: BLE001
                pass
        new_engine = self._build_engine(config)
        self._worker_local.engine = new_engine
        self._worker_local.signature = signature
        self._worker_local.voice = (
            str(getattr(config, "voice", "Junhao") or "Junhao")
            if config is not None
            else "Junhao"
        )
        return new_engine

    def _build_engine(self, config: Any) -> Any:
        """构造合成引擎；注入工厂时优先使用（便于测试替身）。"""
        from omnicrawl.tts import TTSConfig

        voice = str(getattr(config, "voice", "Junhao") or "Junhao")
        model_dir = str(getattr(config, "model_dir", "") or "").strip() or None
        thread_count = int(getattr(config, "thread_count", 4) or 4)
        device = str(getattr(config, "device", "auto") or "auto")
        engine_config = TTSConfig(
            model_dir=model_dir,
            thread_count=thread_count,
            device=device,
            voice=voice,
            output_dir=self._tmp_dir,
        )
        if self._engine_factory is not None:
            return self._engine_factory(engine_config)
        from omnicrawl.tts import TtsEngine

        return TtsEngine(engine_config)

    def _synthesize(self, text: str) -> str:
        engine = self._engine()
        # 并发 worker 若让引擎自选路径会全部写同一固定文件名互相覆盖；
        # 显式生成唯一输出路径隔离每次合成结果。
        output_path = Path(self._tmp_dir) / f"seg_{uuid.uuid4().hex}.wav"
        result = engine.synthesize(
            text,
            voice=getattr(self._worker_local, "voice", None),
            output_path=output_path,
        )
        return str(result.audio_path)

    @staticmethod
    def _default_player(path: str) -> None:
        from omnicrawl.tts.player import play_wav

        play_wav(path, blocking=True)


__all__ = ["TurnSpeechAnnouncer", "_find_cut_index"]
