from __future__ import annotations

import unittest

from ai_voice_agent.speech_to_text import SpeechToText


class SpeechToTextTest(unittest.TestCase):
    def test_normalize_recognized_text_merges_cjk_spaces_only(self) -> None:
        text = "目 前 我 的 浏 览 器 打 开 了 几 个 窗 口 Chrome LINUX DO D:\\下载"

        normalized = SpeechToText._normalize_recognized_text(text)

        self.assertEqual(normalized, "目前我的浏览器打开了几个窗口 Chrome LINUX DO D:\\下载")


if __name__ == "__main__":
    unittest.main()
