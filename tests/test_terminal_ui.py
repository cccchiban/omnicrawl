from __future__ import annotations

import io
import os
import re
import types
import unittest
from contextlib import redirect_stdout
from unittest.mock import patch

from omnicrawl.ui.terminal import (
    InputBar,
    MarkdownStreamState,
    StatusLine,
    TerminalCapabilities,
    TerminalUI,
    WaitingIndicator,
    _contains_complex_display_width,
    _display_width,
    _split_display_rows,
)


ANSI_PATTERN = re.compile(r"\033\[[0-9;]*m")


class _FakeMsvcrt:
    def __init__(self, chars: list[str]) -> None:
        self._chars = chars

    def kbhit(self) -> bool:
        return bool(self._chars)

    def getwch(self) -> str:
        if not self._chars:
            raise AssertionError("测试输入已耗尽。")
        return self._chars.pop(0)


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
            "omnicrawl.ui.tui._core.shutil.get_terminal_size",
            return_value=os.terminal_size((72, 30)),
        ):
            expected_rows = _split_display_rows(text, 72 - ui.prompt_width() - 1)
            output = io.StringIO()
            with redirect_stdout(output):
                ui.inline_turn_base(text)

        rendered = output.getvalue()
        self.assertIn(f"\033[{len(expected_rows)}A", rendered)
        # 用户前缀 ▸ (U+25B8) 在输出中
        self.assertIn("▸", rendered)
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
            "omnicrawl.ui.tui._markdown_renderer.shutil.get_terminal_size",
            return_value=os.terminal_size((34, 24)),
        ):
            with redirect_stdout(output):
                ui.print_ai_prefix()
                ui.write_markdown_delta(text, state)
                ui.flush_markdown(state)

        rendered = ANSI_PATTERN.sub("", output.getvalue())
        lines = rendered.splitlines()

        self.assertGreaterEqual(len(lines), 2)
        # AI 前缀 ◆ 后面跟内容
        self.assertTrue(lines[0].startswith("◆ "), f"Expected '◆ ' prefix, got: {repr(lines[0][:5])}")
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
        self.assertIn("◆ 第一段。\n  第二段。", rendered)

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
        # 标题有 █ 色条前缀，引用块用 ║ 标记
        self.assertIn("█ 主要内容总结", rendered)
        self.assertIn("1. 日本：数量下降", rendered)
        self.assertIn("║ 流浪汉问题并不只是贫困问题。", rendered)
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

    def test_long_styled_markdown_preview_uses_total_span_width(self) -> None:
        ui = TerminalUI(TerminalCapabilities(ansi=True))
        state = MarkdownStreamState()
        output = io.StringIO()

        with patch(
            "omnicrawl.ui.tui._markdown_renderer.shutil.get_terminal_size",
            return_value=os.terminal_size((24, 24)),
        ):
            with redirect_stdout(output):
                ui.write_markdown_delta(
                    "see `abcdefghijklmnopqrstuvwxyz0123456789`",
                    state,
                )

        self.assertTrue(state.passthrough_line)
        self.assertFalse(state.preview_visible)
        rendered = ANSI_PATTERN.sub("", output.getvalue())
        self.assertIn("see abcdefghijklmnopq\n  rstuvwxyz0123456789", rendered)

    def test_prompt_yes_no_redraws_and_collapses_current_option_block(self) -> None:
        ui = TerminalUI(TerminalCapabilities(ansi=True))
        keys = ["\xe0", "P", "\r"]
        fake_msvcrt = types.SimpleNamespace(getwch=lambda: keys.pop(0))
        output = io.StringIO()

        with patch.dict("sys.modules", {"msvcrt": fake_msvcrt}):
            with redirect_stdout(output):
                self.assertFalse(ui.prompt_yes_no("确认执行？", confirmed_label="已取消"))

        rendered = output.getvalue()
        plain_rendered = ANSI_PATTERN.sub("", rendered)
        # 选项区 5 行：分隔线 + 选项 + 底框 + 空行 + 提示
        self.assertIn("\033[5A", rendered)
        # 卡片行(前导空行+顶部框线+1内容行=3) + 选项区5行 = 8
        self.assertIn("\033[8A\033[J", rendered)
        self.assertIn("❯ No", plain_rendered)

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
            ui.status("模型请求中断，正在重试 2/5", leading_blank=False, italic=True)

        rendered = output.getvalue()
        # 新配色路由使用真彩色或16色序列包裹内容
        self.assertIn("模型请求中断，正在重试 2/5", rendered)
        self.assertIn("\033[3m", rendered)  # ITALIC

    def test_status_line_aligns_with_prompt_content_and_keeps_blank_spacing(self) -> None:
        ui = TerminalUI(TerminalCapabilities(ansi=True))
        status_line = StatusLine(ui)
        output = io.StringIO()

        with redirect_stdout(output):
            status_line.show("处理中  (-_-)...")
            status_line.show("处理中  (-_-)..")

        rendered = output.getvalue()
        self.assertTrue(rendered.startswith("\n\033[2K"))
        self.assertIn("处理中  (-_-)...", rendered)
        self.assertIn("处理中  (-_-)..", rendered)

    def test_status_line_plain_mode_keeps_blank_spacing_and_indent(self) -> None:
        ui = TerminalUI(TerminalCapabilities(ansi=False))
        status_line = StatusLine(ui)
        output = io.StringIO()

        with redirect_stdout(output):
            status_line.show("处理中  (-_-)...")

        self.assertEqual(output.getvalue(), "\n  处理中  (-_-)...\n")

    def test_waiting_indicator_does_not_submit_unconfirmed_pre_input(self) -> None:
        ui = TerminalUI(TerminalCapabilities(ansi=True))
        status_line = StatusLine(ui)
        waiting = WaitingIndicator(status_line)
        fake_msvcrt = _FakeMsvcrt(["下", "一", "句"])
        output = io.StringIO()

        with patch("omnicrawl.ui.tui._spinner.os.name", "nt"):
            with patch.dict("sys.modules", {"msvcrt": fake_msvcrt}):
                with redirect_stdout(output):
                    waiting._poll_pre_input()
                    waiting._render_status("处理中")
                    submitted = waiting.stop()

        rendered = output.getvalue()
        self.assertEqual(waiting.pre_input, "下一句")
        self.assertEqual(submitted, "")
        self.assertIn("\033[1A\r\033[2K", rendered)

    def test_waiting_indicator_returns_only_enter_submitted_pre_input(self) -> None:
        ui = TerminalUI(TerminalCapabilities(ansi=True))
        status_line = StatusLine(ui)
        waiting = WaitingIndicator(status_line)
        fake_msvcrt = _FakeMsvcrt(["n", "e", "x", "t", "\r"])
        output = io.StringIO()

        with patch("omnicrawl.ui.tui._spinner.os.name", "nt"):
            with patch.dict("sys.modules", {"msvcrt": fake_msvcrt}):
                with redirect_stdout(output):
                    waiting._poll_pre_input()
                    waiting._render_status("处理中")
                    submitted = waiting.stop()

        self.assertEqual(submitted, "next")
        self.assertEqual(waiting.pre_input, "next")

    def test_waiting_indicator_plain_mode_uses_status_line_clear(self) -> None:
        ui = TerminalUI(TerminalCapabilities(ansi=False))
        status_line = StatusLine(ui)
        waiting = WaitingIndicator(status_line)
        output = io.StringIO()

        with redirect_stdout(output):
            waiting._render_status("处理中")
            submitted = waiting.stop()

        self.assertEqual(submitted, "")
        self.assertEqual(output.getvalue(), "\n  处理中\n")
        self.assertNotIn("\033[2K", output.getvalue())

    def test_waiting_indicator_clears_dynamic_line_count_without_token_line(self) -> None:
        ui = TerminalUI(TerminalCapabilities(ansi=True))
        status_line = StatusLine(ui)
        waiting = WaitingIndicator(status_line)
        output = io.StringIO()

        with patch("omnicrawl.ui.tui._spinner.os.name", "nt"):
            with redirect_stdout(output):
                waiting._render_status("处理中")
                submitted = waiting.stop()

        self.assertEqual(submitted, "")
        self.assertEqual(output.getvalue().count("\033[1A"), 1)

    def test_waiting_indicator_clears_dynamic_line_count_with_token_line(self) -> None:
        ui = TerminalUI(TerminalCapabilities(ansi=True), model_label="gpt-5.5")
        ui.update_token_usage(1, 2, 3)
        status_line = StatusLine(ui)
        waiting = WaitingIndicator(status_line)
        output = io.StringIO()

        with patch("omnicrawl.ui.tui._spinner.os.name", "nt"):
            with redirect_stdout(output):
                waiting._render_status("处理中")
                submitted = waiting.stop()

        self.assertEqual(submitted, "")
        self.assertEqual(output.getvalue().count("\033[1A"), 2)

    def test_input_bar_push_up_clears_existing_bar_before_output(self) -> None:
        ui = TerminalUI(TerminalCapabilities(ansi=True), model_label="gpt-5.5")
        input_bar = InputBar(ui)
        output = io.StringIO()

        with patch("omnicrawl.ui.tui._spinner.os.name", "nt"):
            with redirect_stdout(output):
                input_bar.show()
                input_bar.push_up()
                print("AI 输出", end="")
                input_bar.pop_down()

        rendered = output.getvalue()
        ai_index = rendered.index("AI 输出")
        clear_index = rendered.index("\r\033[2K")
        self.assertLess(clear_index, ai_index)
        self.assertIn("AI 输出\n", rendered)

    def test_input_bar_clear_removes_all_visible_lines(self) -> None:
        ui = TerminalUI(TerminalCapabilities(ansi=True), model_label="gpt-5.5")
        input_bar = InputBar(ui)
        output = io.StringIO()

        with patch("omnicrawl.ui.tui._spinner.os.name", "nt"):
            with redirect_stdout(output):
                input_bar.show()
                submitted = input_bar.clear()

        self.assertEqual(submitted, "")
        self.assertEqual(output.getvalue().count("\r\033[2K"), 2)

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
        # 新格式：╭─ 步骤 N · tool_name
        self.assertIn("步骤 2", rendered)
        self.assertIn("run_command", rendered)
        self.assertIn("echo hello", rendered)

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
        # 新格式：退出码以 · 退出码 N 展示
        self.assertIn("退出码 0", rendered)
        self.assertIn("hello", rendered)

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
        # 连续工具之间有空行分隔
        self.assertIn("first", rendered)
        self.assertIn("步骤 2", rendered)
        self.assertIn("run_command", rendered)

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
        # 新格式使用 ◌ 闪烁标记 + 颜色路由
        self.assertIn("◌", rendered)
        self.assertIn("run_command", rendered)
        self.assertIn("echo hi", rendered)
        self.assertIn("\033[5m", rendered)  # BLINK

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
        self.assertIn("✓", rendered)

    def test_prompt_status_line_contains_model_and_tokens(self) -> None:
        ui = TerminalUI(TerminalCapabilities(ansi=True), model_label="gpt-5.5")
        ui.update_token_usage(123, 45, 67)

        line = ui.prompt_status_line()
        plain_line = ANSI_PATTERN.sub("", line)

        self.assertIn("gpt-5.5", line)
        # 新格式：in:123 cache:67 out:45
        self.assertIn("in:123", plain_line)
        self.assertIn("cache:67", plain_line)
        self.assertIn("out:45", plain_line)


if __name__ == "__main__":
    unittest.main()
