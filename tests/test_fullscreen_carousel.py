"""底部轮播三页（遥测/工作区/留言）与留言随机读取的回归测试。"""

from __future__ import annotations

import random
import unittest

from rich.text import Text

from omnicrawl.ui.fullscreen.status.hud import (
    load_carousel_message_lines,
)
from omnicrawl.ui.fullscreen.status.indicators import StatusMixin


class _CarouselStub(StatusMixin):
    """仅暴露轮转/页面文本所需状态，不触碰 Textual DOM。"""

    def __init__(self) -> None:
        self._carousel_page = "telemetry"
        self._carousel_rand = random.Random(20260905)
        self._carousel_settled_text = None
        self._carousel_message_line: str | None = None
        self._carousel_anim_interval = None

    def set_timer(self, delay: float, callback) -> None:
        """测试桩：不真正调度定时器。"""

    def set_interval(self, interval: float, callback) -> None:
        """测试桩：不真正调度定时器。"""


class CarouselMessageFileTest(unittest.TestCase):
    """包内留言文件必须可读且含全部候选行。"""

    def test_load_carousel_message_lines_returns_expected_lines(self) -> None:
        lines = load_carousel_message_lines()
        self.assertEqual(len(lines), 8)
        self.assertIn("回眸一笑百媚生，六宫粉黛无颜色", lines)
        self.assertIn("我分不清！妈，我真的分不清啊！", lines)
        self.assertTrue(all(line.strip() == line for line in lines))


class CarouselRotationTest(unittest.TestCase):
    """遥测 10s → 工作区 10s → 留言 10s 循环。"""

    def test_pages_rotate_in_fixed_order(self) -> None:
        stub = _CarouselStub()
        sequence = []
        for _ in range(6):
            sequence.append(stub._carousel_page)
            stub._carousel_page = stub._carousel_next_page()
        self.assertEqual(
            sequence,
            [
                "telemetry",
                "workspace",
                "message",
                "telemetry",
                "workspace",
                "message",
            ],
        )

    def test_each_page_holds_ten_seconds(self) -> None:
        stub = _CarouselStub()
        for page in ("telemetry", "workspace", "message"):
            stub._carousel_page = page
            self.assertEqual(stub._carousel_page_duration(), 10)

    def test_message_page_picks_one_random_line(self) -> None:
        stub = _CarouselStub()
        stub._carousel_page = "message"
        text = stub._carousel_build_page_text("message")
        lines = load_carousel_message_lines()
        self.assertIn(text.plain, lines)

    def test_message_page_keeps_line_stable_during_stay(self) -> None:
        stub = _CarouselStub()
        stub._carousel_page = "message"
        first = stub._carousel_build_page_text("message").plain
        for _ in range(5):
            self.assertEqual(
                stub._carousel_build_page_text("message").plain,
                first,
            )

    def test_each_message_entry_picks_a_fresh_line(self) -> None:
        """再次切入留言页时应换新句，而不是永远复用首次抽取的那句。"""
        stub = _CarouselStub()
        stub._carousel_settled_text = Text("seed")
        picked: set[str] = set()
        # 切入 message 页 8 次（候选行共 8 条）；由于每轮都重新抽取，
        # 若始终只复用同一句，不可能出现 3 句以上。
        for _ in range(8):
            stub._carousel_switch_to("message", animate=False)
            picked.add(stub._carousel_message_text().plain)
            stub._carousel_page = "telemetry"  # 模拟停留结束切回，不构建旧页
        self.assertGreaterEqual(len(picked), 3)


if __name__ == "__main__":
    unittest.main()
