from __future__ import annotations

import io
import os
import re
import unittest
from contextlib import redirect_stdout
from unittest.mock import patch

from ai_voice_agent.terminal_ui import (
    MarkdownStreamState,
    StatusLine,
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

    def test_markdown_table_allows_blank_line_between_header_and_delimiter(self) -> None:
        ui = TerminalUI(TerminalCapabilities(ansi=False))
        state = MarkdownStreamState()
        output = io.StringIO()
        text = (
            "当前查询结果如下：\n\n"
            "| 项目 | 数据 |\n\n"
            "|---|---|\n"
            "| 天气 | Overcast，阴 / 阴天 |\n"
            "| 当前气温 | 23℃ |\n"
            "\n结论：数据已查询。"
        )

        with redirect_stdout(output):
            ui.print_ai_prefix()
            ui.write_markdown_delta(text, state)
            ui.flush_markdown(state)

        rendered = output.getvalue()
        self.assertIn("项目", rendered)
        self.assertIn("│ 数据", rendered)
        self.assertIn("─", rendered)
        self.assertIn("┼", rendered)
        self.assertIn("天气", rendered)
        self.assertIn("Overcast，阴 / 阴天", rendered)
        self.assertIn("当前气温", rendered)
        self.assertIn("23℃", rendered)
        self.assertIn("结论：数据已查询。", rendered)
        self.assertNotIn("|---|---|", rendered)

    def test_cjk_markdown_blocks_do_not_fall_back_to_raw_passthrough(self) -> None:
        ui = TerminalUI(TerminalCapabilities(ansi=True))
        state = MarkdownStreamState()
        output = io.StringIO()

        with redirect_stdout(output):
            ui.print_ai_prefix()
            ui.write_markdown_delta("## ", state)
            ui.write_markdown_delta("主要内容总结\n", state)
            ui.write_markdown_delta("### 1. 日本：数量下降\n", state)
            ui.write_markdown_delta("> 流浪汉问题并不只是贫困问题。", state)
            ui.flush_markdown(state)

        rendered = ANSI_PATTERN.sub("", output.getvalue())
        self.assertIn("^ 主要内容总结", rendered)
        self.assertIn("1. 日本：数量下降", rendered)
        self.assertIn("│ 流浪汉问题并不只是贫困问题。", rendered)
        self.assertNotIn("##", rendered)
        self.assertNotIn("###", rendered)
        self.assertNotIn("> 流浪汉", rendered)

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

    def test_status_can_render_gray_italic(self) -> None:
        ui = TerminalUI(TerminalCapabilities(ansi=True))
        output = io.StringIO()

        with redirect_stdout(output):
            ui.status("模型流式连接中断，正在重试 2/5", leading_blank=False, italic=True)

        rendered = output.getvalue()
        self.assertIn("\033[3;90m[模型流式连接中断，正在重试 2/5]\033[0m", rendered)

    def test_status_line_aligns_with_prompt_content_and_keeps_blank_spacing(self) -> None:
        ui = TerminalUI(TerminalCapabilities(ansi=True))
        status_line = StatusLine(ui)
        output = io.StringIO()

        with redirect_stdout(output):
            status_line.show("处理中  (-_-)...")
            status_line.show("处理中  (-_-)..")

        rendered = output.getvalue()
        self.assertTrue(rendered.startswith("\n\033[2K"))
        self.assertIn("\033[97m  处理中  (-_-)...\033[0m", rendered)
        self.assertIn("\r\033[2K\033[97m  处理中  (-_-)..\033[0m", rendered)

    def test_status_line_plain_mode_keeps_blank_spacing_and_indent(self) -> None:
        ui = TerminalUI(TerminalCapabilities(ansi=False))
        status_line = StatusLine(ui)
        output = io.StringIO()

        with redirect_stdout(output):
            status_line.show("处理中  (-_-)...")

        self.assertEqual(output.getvalue(), "\n  处理中  (-_-)...\n")

    def test_tool_call_start_shows_command_detail_and_running_marker(self) -> None:
        ui = TerminalUI(TerminalCapabilities(ansi=False))
        output = io.StringIO()

        with redirect_stdout(output):
            ui.print_tool_call_start(
                2,
                "run_command",
                {"command": "echo hello"},
                leading_blank=False,
            )

        rendered = output.getvalue()
        self.assertIn("* 步骤 2 — 请求 run_command", rendered)
        self.assertIn("  Ran echo hello", rendered)

    def test_tool_result_record_shows_exit_code_and_stdout_preview(self) -> None:
        ui = TerminalUI(TerminalCapabilities(ansi=False))
        output = io.StringIO()

        with redirect_stdout(output):
            ui.print_tool_result_record(
                True,
                "退出码：0\n\nstdout:\nhello\n\nstderr:\n",
                tool_name="run_command",
            )

        rendered = output.getvalue()
        self.assertIn("执行记录：成功（退出码 0）", rendered)
        self.assertIn("  └ hello", rendered)

    def test_consecutive_tool_steps_are_separated_by_blank_line(self) -> None:
        ui = TerminalUI(TerminalCapabilities(ansi=False))
        output = io.StringIO()

        with redirect_stdout(output):
            ui.print_tool_call_start(
                1,
                "run_command",
                {"command": "echo first"},
                leading_blank=False,
            )
            ui.print_tool_result_record(
                True,
                "退出码：0\n\nstdout:\nfirst\n\nstderr:\n",
                tool_name="run_command",
            )
            ui.print_tool_call_start(
                2,
                "run_command",
                {"command": "echo second"},
                leading_blank=False,
            )

        rendered = output.getvalue()
        self.assertIn("  └ first\n\n  * 步骤 2 — 请求 run_command", rendered)

    def test_tool_call_start_uses_ansi_color_and_blink(self) -> None:
        ui = TerminalUI(TerminalCapabilities(ansi=True))
        output = io.StringIO()

        with redirect_stdout(output):
            ui.print_tool_call_start(
                1,
                "run_command",
                {"command": "echo hi"},
                leading_blank=False,
            )

        rendered = output.getvalue()
        self.assertIn("\033[5m*\033[0m", rendered)
        self.assertIn("\033[94mrun_command\033[0m", rendered)
        self.assertIn("\033[97mecho hi\033[0m", rendered)

    def test_tool_result_refreshes_running_marker_when_ansi_enabled(self) -> None:
        ui = TerminalUI(TerminalCapabilities(ansi=True))
        output = io.StringIO()

        with redirect_stdout(output):
            state = ui.print_tool_call_start(
                1,
                "run_command",
                {"command": "echo hi"},
                leading_blank=False,
            )
            ui.print_tool_result_record(
                True,
                "退出码：0\n\nstdout:\nhi",
                tool_name="run_command",
                display_state=state,
            )

        rendered = output.getvalue()
        self.assertIn("\033[2A", rendered)
        self.assertIn("\033[32m✓\033[0m", rendered)
        self.assertIn("执行记录：", rendered)
        self.assertIn("\033[32m成功\033[0m", rendered)

    def test_prompt_status_line_contains_bright_model_and_tokens(self) -> None:
        ui = TerminalUI(TerminalCapabilities(ansi=True), model_label="gpt-5.5")
        ui.update_token_usage(123, 45, 67)

        line = ui.prompt_status_line()
        plain_line = ANSI_PATTERN.sub("", line)

        self.assertIn("gpt-5.5", line)
        self.assertIn("Input Token: 123 Cached: 67 Output Token: 45", plain_line)
        self.assertIn("\033[97m", line)


if __name__ == "__main__":
    unittest.main()
