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
    read_line_autocomplete,
)
from omnicrawl.ui.terminal import TerminalCapabilities, TerminalUI


class _FakeMsvcrt:
    def __init__(self, chars: list[str]) -> None:
        self._chars = chars

    def kbhit(self) -> bool:
        return bool(self._chars)

    def getwch(self) -> str:
        if not self._chars:
            raise AssertionError("测试输入已耗尽。")
        return self._chars.pop(0)


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
