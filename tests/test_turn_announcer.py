"""TurnSpeechAnnouncer（主 TUI 回合自动朗读）单元测试。

不依赖真实 TTS 模型：注入替身引擎工厂与播放函数，验证段落切分、
并发合成（≤ max_concurrency）、按序播放、回合结束 flush 与取消丢弃语义。
"""

from __future__ import annotations

import threading
import time
import unittest
from pathlib import Path
from types import SimpleNamespace
from typing import Any

from omnicrawl.ui.fullscreen.turn.announcer import (
    TurnSpeechAnnouncer,
    _find_cut_index,
)


def _fake_tts_config(*, enabled: bool = True) -> SimpleNamespace:
    """返回带 models_ready 所需字段的最小 TTS 配置替身。"""
    return SimpleNamespace(
        enabled=enabled,
        model_dir="",
        thread_count=4,
        device="auto",
        voice="Junhao",
        resolved_model_dir=lambda: Path("/fake/models"),
    )


class _FakeEngine:
    """替身引擎：把文本写为音频文件并记录调用。"""

    instances: list["_FakeEngine"] = []

    def __init__(self, config) -> None:
        self.config = config
        self.closed = False
        self.synthesized: list[str] = []
        type(self).instances.append(self)

    def synthesize(self, text: str, *, voice=None, output_path=None):
        self.synthesized.append(text)
        if output_path is not None:
            path = Path(output_path)
        else:
            path = Path(self.config.output_dir) / f"audio_{len(self.synthesized)}.wav"
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text, encoding="utf-8")
        return SimpleNamespace(audio_path=path)

    def close(self) -> None:
        self.closed = True


class _FakeEngineFactory:
    """返回可追踪实例的工厂。"""

    def __init__(self) -> None:
        self.created: list[_FakeEngine] = []

    def __call__(self, config):
        engine = _FakeEngine(config)
        self.created.append(engine)
        return engine


class CutIndexTests(unittest.TestCase):
    def test_short_buffer_no_cut(self) -> None:
        self.assertIsNone(_find_cut_index("这是一句较短的话"))

    def test_cuts_at_blank_line(self) -> None:
        buffer = "第一段内容\n\n第二段开始"
        self.assertEqual(_find_cut_index(buffer), 5)

    def test_cuts_at_sentence_end_over_min_length(self) -> None:
        buffer = "这是足够长的一句话需要超过最短长度才会触发切分。后面还有内容"
        cut = _find_cut_index(buffer)
        self.assertIsNotNone(cut)
        self.assertEqual(buffer[cut - 1], "。")

    def test_hard_cut_on_very_long_no_punctuation(self) -> None:
        buffer = "无" * 500
        cut = _find_cut_index(buffer)
        self.assertIsNotNone(cut)
        self.assertLessEqual(cut, 240)

    def test_single_newline_is_not_paragraph_boundary(self) -> None:
        buffer = "这是第一行\n第二行内容也够长了但没有句号"
        cut = _find_cut_index(buffer)
        self.assertIsNone(cut)


class AnnouncerTestBase(unittest.TestCase):
    def setUp(self) -> None:
        self.factory = _FakeEngineFactory()
        self.played: list[str] = []
        self.play_lock = threading.Lock()

    def make_announcer(self, config=None, max_concurrency: int | None = None) -> TurnSpeechAnnouncer:
        self._config = config if config is not None else _fake_tts_config()

        def player(path: str) -> None:
            text = Path(path).read_text(encoding="utf-8")
            with self.play_lock:
                self.played.append(text)

        kwargs: dict[str, Any] = {
            "engine_factory": self.factory,
            "player": player,
        }
        # 不传时让构造器走代码默认值，避免测试与生产默认分叉。
        if max_concurrency is not None:
            kwargs["max_concurrency"] = max_concurrency
        announcer = TurnSpeechAnnouncer(
            lambda: self._config,
            **kwargs,
        )
        return announcer

    def wait_until(self, predicate, timeout: float = 5.0) -> None:
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if predicate():
                return
            time.sleep(0.01)
        self.fail("等待条件超时")

    def tearDown(self) -> None:
        announcer = getattr(self, "_announcer", None)
        if announcer is not None:
            announcer.close()


# 构造足够触发即时切分的长句（≥ _MIN_SENTENCE_CHARS 后遇句号即切）。
_LONG_SENTENCE_1 = "这是第一段足够长的内容，只有超过最短长度阈值之后遇到句号才会被切分朗读。"
_LONG_SENTENCE_2 = "这是第二段足够长的内容，同样需要超过最短长度阈值之后遇到句号才会切分朗读。"
_SHORT_TAIL = "这是末尾残留的短句，由回合结束时统一提交。"


class AnnouncerFlowTests(AnnouncerTestBase):
    def test_streaming_text_is_synthesized_and_played_in_order(self) -> None:
        self._announcer = self.make_announcer()
        self._announcer.on_turn_start()
        # 长句达到切分阈值：遇到句号即提交合成。
        self._announcer.feed_text(_LONG_SENTENCE_1)
        self.wait_until(lambda: len(self.factory.created) >= 1, timeout=3)
        # 残留短句不切段，flush 时提交为最后一段。
        self._announcer.feed_text(_SHORT_TAIL)
        self._announcer.flush_turn(cancelled=False)
        self.wait_until(lambda: len(self.played) >= 2, timeout=5)
        joined = "".join(self.played)
        self.assertIn(_LONG_SENTENCE_1, joined)
        self.assertIn(_SHORT_TAIL, joined)

    def test_disabled_tts_drops_text(self) -> None:
        self._announcer = self.make_announcer(config=_fake_tts_config(enabled=False))
        self._announcer.on_turn_start()
        self._announcer.feed_text(_LONG_SENTENCE_1 + _LONG_SENTENCE_2)
        self._announcer.flush_turn(cancelled=False)
        time.sleep(0.3)
        self.assertEqual(self.factory.created, [])
        self.assertEqual(self.played, [])

    def test_cancelled_turn_discards_pending(self) -> None:
        self._announcer = self.make_announcer()
        self._announcer.on_turn_start()
        self._announcer.feed_text(_LONG_SENTENCE_1)
        self.wait_until(lambda: len(self.played) >= 1, timeout=3)
        # 取消：已提交但尚未播放的后续段落应被丢弃。
        self._announcer.feed_text(_LONG_SENTENCE_2)
        self._announcer.flush_turn(cancelled=True)
        time.sleep(0.4)
        with self.play_lock:
            texts = list(self.played)
        self.assertTrue(any(_LONG_SENTENCE_1 in text for text in texts))
        self.assertFalse(any(_LONG_SENTENCE_2 in text for text in texts))

    def test_concurrency_limited_by_max_workers(self) -> None:
        self._announcer = self.make_announcer(max_concurrency=2)
        self._announcer.on_turn_start()
        for _index in range(6):
            self._announcer.feed_text(_LONG_SENTENCE_1)
        # 全部段落最终被播放。
        self.wait_until(lambda: len(self.played) >= 6, timeout=5)
        # 并发引擎数不超过 max_concurrency。
        self.assertLessEqual(len(self.factory.created), 2)

    def test_default_concurrency_is_single_engine(self) -> None:
        # 语音会话高内存根因：每个合成 worker 独立加载整套 TTS 权重。
        # 默认必须串行（单引擎），避免多套权重同时驻留内存拖垮 TUI。
        self._announcer = self.make_announcer()
        self._announcer.on_turn_start()
        for _index in range(6):
            self._announcer.feed_text(_LONG_SENTENCE_1)
        # 全部段落最终被播放。
        self.wait_until(lambda: len(self.played) >= 6, timeout=5)
        # 默认并发=1：无论多少段落，只创建 1 个引擎实例。
        self.assertEqual(len(self.factory.created), 1)

    def test_flush_without_active_turn_is_noop(self) -> None:
        self._announcer = self.make_announcer()
        # 未 on_turn_start：flush 不应提交残留（慢命令等非朗读场景）。
        self._announcer.feed_text(_LONG_SENTENCE_1)
        self._announcer.flush_turn(cancelled=False)
        time.sleep(0.3)
        self.assertEqual(self.played, [])

    def test_cancel_of_new_turn_keeps_previous_flushed_turn(self) -> None:
        self._announcer = self.make_announcer()
        # 回合 A：正常结束，段落进入播放队列。
        self._announcer.on_turn_start()
        self._announcer.feed_text(_LONG_SENTENCE_1)
        self._announcer.flush_turn(cancelled=False)
        # 回合 B：开始后立即取消，不应影响回合 A 已提交的内容播放。
        self._announcer.on_turn_start()
        self._announcer.feed_text(_LONG_SENTENCE_2)
        self._announcer.flush_turn(cancelled=True)
        self.wait_until(lambda: len(self.played) >= 1, timeout=5)
        # 回合 B 的内容不得被播放；回合 A 的内容应完整播完。
        self.wait_until(
            lambda: any(_LONG_SENTENCE_1 in text for text in list(self.played)),
            timeout=5,
        )
        time.sleep(0.4)
        with self.play_lock:
            texts = list(self.played)
        self.assertFalse(any(_LONG_SENTENCE_2 in text for text in texts))

    def test_history_sets_are_pruned_after_play_cursor_advances(self) -> None:
        """播放游标推进后，旧的 _skipped/_discarded 记录必须被清理（防长会话内存增长）。"""
        self._announcer = self.make_announcer()
        # 手动构造“已播放到 seq=10”的状态：跳过 3 段、丢弃区间 [0,2] 与 [8,12]。
        self._announcer._next_play = 10
        self._announcer._next_alloc = 20
        self._announcer._skipped = {1, 5, 15}
        self._announcer._discarded = [(0, 2), (8, 12)]
        with self._announcer._wake:
            self._announcer._prune_history_locked()
        # seq < 10 的历史全部移除；跨过游标的区间被压缩到从游标开始。
        self.assertEqual(self._announcer._skipped, {15})
        self.assertEqual(self._announcer._discarded, [(10, 12)])
        # 游标之前的内容不应再被视为已丢弃/跳过（否则播放线程会卡死）。
        self.assertFalse(self._announcer._is_discarded(5))
        self.assertFalse(self._announcer._is_discarded(9))
        self.assertTrue(self._announcer._is_discarded(11))


if __name__ == "__main__":
    unittest.main()
