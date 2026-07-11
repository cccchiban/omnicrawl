from __future__ import annotations

import io
import os
import types
import unittest
from contextlib import redirect_stdout
from unittest.mock import patch

from omnicrawl.ui.inline_input import (
    _InlineInputHistoryBrowser,
    _append_inline_input_history,
    _delete_display_unit_after,
    _delete_display_unit_before,
    _next_display_unit_offset,
    _previous_display_unit_offset,
    read_line_autocomplete,
)
from omnicrawl.ui.terminal import (
    TerminalCapabilities,
    TerminalUI,
    _combine_surrogate_pair,
)


class _FakeMsvcrt:
    def __init__(self, chars: list[str]) -> None:
        self._chars = chars

    def kbhit(self) -> bool:
        return bool(self._chars)

    def getwch(self) -> str:
        if not self._chars:
            raise AssertionError("测试输入已耗尽。")
        return self._chars.pop(0)


class _DelayedSurrogateMsvcrt(_FakeMsvcrt):
    """模拟 getwch 先交付高代理、下一次阻塞读取才交付低代理的 Windows 输入。"""

    def __init__(self, chars: list[str]) -> None:
        super().__init__(chars)
        self._delay_low_surrogate = False

    def kbhit(self) -> bool:
        if self._delay_low_surrogate:
            self._delay_low_surrogate = False
            return False
        return super().kbhit()

    def getwch(self) -> str:
        char = super().getwch()
        if 0xD800 <= ord(char) <= 0xDBFF:
            self._delay_low_surrogate = True
        return char


class InlineInputHistoryTest(unittest.TestCase):
    def test_history_browser_walks_previous_and_restores_draft(self) -> None:
        browser = _InlineInputHistoryBrowser(["first", "second"])

        self.assertEqual(browser.previous("draft"), "second")
        self.assertEqual(browser.previous("second"), "first")
        self.assertEqual(browser.previous("first"), "first")
        self.assertEqual(browser.next(), "second")
        self.assertEqual(browser.next(), "draft")
        self.assertFalse(browser.is_browsing)

    def test_append_history_skips_noise_and_limits_size(self) -> None:
        history: list[str] = []

        _append_inline_input_history(history, "")
        _append_inline_input_history(history, "   ")
        _append_inline_input_history(history, "first", limit=2)
        _append_inline_input_history(history, "first", limit=2)
        _append_inline_input_history(history, "second", limit=2)
        _append_inline_input_history(history, "third", limit=2)

        self.assertEqual(history, ["second", "third"])


class InlineInputEditTest(unittest.TestCase):
    def test_read_line_autocomplete_renders_initial_draft(self) -> None:
        fake = _FakeMsvcrt(["\r"])
        ui = TerminalUI(TerminalCapabilities(ansi=True))
        output = io.StringIO()

        with patch.dict("sys.modules", {"msvcrt": fake}):
            with patch(
                "omnicrawl.ui.inline_input.shutil.get_terminal_size",
                return_value=os.terminal_size((80, 24)),
            ):
                with redirect_stdout(output):
                    result = read_line_autocomplete(
                        ui.prompt(), [], ui, history=[], initial_text="保留草稿"
                    )

        self.assertEqual(result, "保留草稿")
        self.assertIn("保留草稿", output.getvalue())

    def test_read_line_autocomplete_combines_surrogate_pairs_before_deleting(self) -> None:
        fake = _FakeMsvcrt(["\ud83d", "\udc4d", "\ud83c", "\udffd", "\b", "\r"])
        ui = TerminalUI(TerminalCapabilities(ansi=True))

        with patch.dict("sys.modules", {"msvcrt": fake}):
            with patch(
                "omnicrawl.ui.inline_input.shutil.get_terminal_size",
                return_value=os.terminal_size((80, 24)),
            ):
                with redirect_stdout(io.StringIO()):
                    result = read_line_autocomplete(ui.prompt(), [], ui, history=[])

        self.assertEqual(result, "")

    def test_read_line_autocomplete_keeps_surrogate_pair_across_delayed_input(self) -> None:
        fake = _DelayedSurrogateMsvcrt(["\ud83d", "\udc4d", "\ud83c", "\udffd", "\b", "\r"])
        ui = TerminalUI(TerminalCapabilities(ansi=True))

        with patch.dict("sys.modules", {"msvcrt": fake}):
            with patch(
                "omnicrawl.ui.inline_input.shutil.get_terminal_size",
                return_value=os.terminal_size((80, 24)),
            ):
                with redirect_stdout(io.StringIO()):
                    result = read_line_autocomplete(ui.prompt(), [], ui, history=[])

        self.assertEqual(result, "")

    def test_display_unit_navigation_and_delete_preserve_extended_graphemes(self) -> None:
        family = "👨‍👩‍👧‍👦"
        china_flag = "🇨🇳"
        thumbs_up = "👍🏽"
        text = f"a{family}{china_flag}{thumbs_up}b"

        family_end = 1 + len(family)
        flag_end = family_end + len(china_flag)
        thumbs_end = flag_end + len(thumbs_up)
        self.assertEqual(_next_display_unit_offset(text, 1), family_end)
        self.assertEqual(_next_display_unit_offset(text, family_end), flag_end)
        self.assertEqual(_previous_display_unit_offset(text, thumbs_end), flag_end)
        self.assertEqual(_previous_display_unit_offset(text, flag_end), family_end)
        self.assertEqual(_delete_display_unit_before(text, thumbs_end), f"a{family}{china_flag}b")
        self.assertEqual(_delete_display_unit_after(text, family_end), f"a{family}{thumbs_up}b")
        # 即使异常位置落到组合字符内部，也必须删除整个可见单元。
        self.assertEqual(_delete_display_unit_before(f"a{thumbs_up}b", 2), "ab")
        self.assertEqual(_delete_display_unit_after(f"a{thumbs_up}b", 2), "ab")

    def test_inline_surrogate_pair_combines_before_editing(self) -> None:
        self.assertEqual(_combine_surrogate_pair("\ud83d", "\udc4d"), "👍")
        self.assertEqual(_combine_surrogate_pair("\ud83c", "\udffd"), "🏽")
        self.assertIsNone(_combine_surrogate_pair("a", "b"))

    def test_del_character_from_terminal_behaves_like_backspace(self) -> None:
        fake = _FakeMsvcrt(["a", "b", "c", "\x7f", "\r"])
        fake_module = types.SimpleNamespace(kbhit=fake.kbhit, getwch=fake.getwch)
        ui = TerminalUI(TerminalCapabilities(ansi=True))

        with patch.dict("sys.modules", {"msvcrt": fake_module}):
            with patch(
                "omnicrawl.ui.inline_input.shutil.get_terminal_size",
                return_value=os.terminal_size((80, 24)),
            ):
                output = io.StringIO()
                with redirect_stdout(output):
                    result = read_line_autocomplete(ui.prompt(), [], ui, history=[])

        self.assertEqual(result, "ab")
        self.assertIn("\033[K", output.getvalue())

    def test_delete_key_escape_sequence_deletes_current_character(self) -> None:
        fake = _FakeMsvcrt(["a", "b", "c", "\x1b", "[", "D", "\x1b", "[", "3", "~", "\r"])
        fake_module = types.SimpleNamespace(kbhit=fake.kbhit, getwch=fake.getwch)
        ui = TerminalUI(TerminalCapabilities(ansi=True))

        with patch.dict("sys.modules", {"msvcrt": fake_module}):
            with patch(
                "omnicrawl.ui.inline_input.shutil.get_terminal_size",
                return_value=os.terminal_size((80, 24)),
            ):
                output = io.StringIO()
                with redirect_stdout(output):
                    result = read_line_autocomplete(ui.prompt(), [], ui, history=[])

        self.assertEqual(result, "ab")
        self.assertIn("\033[K", output.getvalue())


if __name__ == "__main__":
    unittest.main()
