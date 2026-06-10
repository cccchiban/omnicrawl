from __future__ import annotations

import io
import os
import re
import unittest
from contextlib import redirect_stdout
from unittest.mock import patch

from ai_voice_agent.terminal_ui import (
    MarkdownStreamState,
    TerminalCapabilities,
    TerminalUI,
    _contains_complex_display_width,
    _display_width,
    _split_display_rows,
)


ANSI_PATTERN = re.compile(r"\033\[[0-9;]*m")


class TerminalUITest(unittest.TestCase):
    def test_complex_display_width_detection(self) -> None:
        self.assertEqual(_display_width("abc"), 3)
        self.assertEqual(_display_width("获"), 2)
        self.assertEqual(_display_width("1️⃣"), 1)
        self.assertFalse(_contains_complex_display_width("plain ascii"))
        self.assertTrue(_contains_complex_display_width("获取最新 Release"))
        self.assertTrue(_contains_complex_display_width("1️⃣ 安装 CLI"))

    def test_inline_turn_base_repaints_wrapped_input_once(self) -> None:
        ui = TerminalUI(TerminalCapabilities(ansi=True))
        text = (
            "请阅读 https://github.com/sleepinginsummer/agent-browser-cli/blob/main/"
            "AI_INSTALL.md，按说明安装 CLI、下载 Chrome 扩展到D:\\下载，并添加 "
            "`skills/agent-browser-cli/SKILL.md`。"
        )

        with patch(
            "ai_voice_agent.terminal_ui.shutil.get_terminal_size",
            return_value=os.terminal_size((72, 30)),
        ):
            expected_rows = _split_display_rows(text, 72 - ui.prompt_width() - 1)
            output = io.StringIO()
            with redirect_stdout(output):
                ui.inline_turn_base(text)

        rendered = output.getvalue()
        self.assertIn(f"\033[{len(expected_rows)}A", rendered)
        self.assertEqual(rendered.count("> "), 1)
        self.assertIn("agent-browser-cli", rendered)
        self.assertIn("SKILL.md", rendered)

    def test_complex_streaming_text_uses_passthrough(self) -> None:
        ui = TerminalUI(TerminalCapabilities(ansi=True))
        state = MarkdownStreamState()
        output = io.StringIO()

        with redirect_stdout(output):
            ui.write_markdown_delta("获取最新 Release 下载 URL", state)

        self.assertTrue(state.passthrough_line)
        self.assertFalse(state.preview_visible)
        self.assertEqual(output.getvalue(), "获取最新 Release 下载 URL")

    def test_passthrough_keeps_remainder_after_newline(self) -> None:
        ui = TerminalUI(TerminalCapabilities(ansi=True))
        state = MarkdownStreamState()
        output = io.StringIO()

        with redirect_stdout(output):
            ui.write_markdown_delta("获取最新", state)
            ui.write_markdown_delta(" Release\n下载完成", state)

        self.assertIn("获取最新 Release\n  下载完成", output.getvalue())
        self.assertTrue(state.passthrough_line)
        self.assertEqual(state.pending_line, "")

    def test_passthrough_wraps_long_ai_lines_with_continuation_indent(self) -> None:
        ui = TerminalUI(TerminalCapabilities(ansi=True))
        state = MarkdownStreamState()
        output = io.StringIO()
        text = "这是一个很长的中文回答，用来验证终端手动换行后，所有续行都和正文起点对齐。"

        with patch(
            "ai_voice_agent.terminal_ui.shutil.get_terminal_size",
            return_value=os.terminal_size((34, 24)),
        ):
            with redirect_stdout(output):
                ui.print_ai_prefix()
                ui.write_markdown_delta(text, state)
                ui.flush_markdown(state)

        rendered = ANSI_PATTERN.sub("", output.getvalue())
        lines = rendered.splitlines()

        self.assertGreaterEqual(len(lines), 2)
        self.assertTrue(lines[0].startswith("^ "))
        self.assertTrue(all(line.startswith("  ") for line in lines[1:]))

    def test_markdown_blank_lines_do_not_create_extra_empty_rows(self) -> None:
        ui = TerminalUI(TerminalCapabilities(ansi=True))
        state = MarkdownStreamState()
        output = io.StringIO()

        with redirect_stdout(output):
            ui.print_ai_prefix()
            ui.write_markdown_delta("第一段。\n\n第二段。", state)
            ui.flush_markdown(state)

        rendered = ANSI_PATTERN.sub("", output.getvalue())

        self.assertNotIn("\n\n", rendered)
        self.assertIn("^ 第一段。\n  第二段。", rendered)

    def test_split_bold_marker_is_not_printed_raw(self) -> None:
        ui = TerminalUI(TerminalCapabilities(ansi=True))
        state = MarkdownStreamState()
        output = io.StringIO()

        with redirect_stdout(output):
            ui.write_markdown_delta("*", state)
            ui.write_markdown_delta("*1 个浏览器窗口**", state)
            ui.flush_markdown(state)

        rendered = output.getvalue()
        self.assertNotIn("**", rendered)
        self.assertIn("1 个浏览器窗口", rendered)

    def test_status_can_avoid_leading_blank_line(self) -> None:
        ui = TerminalUI(TerminalCapabilities(ansi=False))
        output = io.StringIO()

        with redirect_stdout(output):
            ui.status("步骤 1 - 请求 run_command", leading_blank=False)

        self.assertEqual(output.getvalue(), "  [步骤 1 - 请求 run_command]\n")

    def test_prompt_status_line_contains_bright_model_and_tokens(self) -> None:
        ui = TerminalUI(TerminalCapabilities(ansi=True), model_label="gpt-5.5")
        ui.update_token_usage(123, 45)

        line = ui.prompt_status_line()
        plain_line = ANSI_PATTERN.sub("", line)

        self.assertIn("gpt-5.5", line)
        self.assertIn("Input Token: 123 Output Token: 45", plain_line)
        self.assertIn("\033[97m", line)


if __name__ == "__main__":
    unittest.main()
