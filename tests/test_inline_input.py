from __future__ import annotations

import unittest

from ai_voice_agent.inline_input import (
    _InlineInputHistoryBrowser,
    _append_inline_input_history,
)


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


if __name__ == "__main__":
    unittest.main()
