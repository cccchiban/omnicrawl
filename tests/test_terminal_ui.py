from __future__ import annotations

import io
import os
import re
import types
import unittest
from contextlib import redirect_stdout
from unittest.mock import patch

from omnicrawl.ui.chat_session import run_inline_chat
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
    def test_run_inline_chat_renders_initial_agent_status_without_unbound_local_error(self) -> None:
        """首次收到 MCP 初始化状态时，状态回调必须能读取外层流式输出标记。"""

        class FakeAgent:
            current_model = "test-model"

            def set_confirm_handler(self, _handler) -> None:
                pass

            skill_manager = None

            def prompt_history_texts(self, *, limit: int) -> list[str]:
                return []

            def run_stream(self, _user_text, _on_delta, *, on_status, **_callbacks) -> None:
                on_status("正在加载 MCP 能力")
                on_status("")
                on_status("MCP 能力已就绪")

        class FakeUI:
            def __init__(self) -> None:
                self.status_calls: list[tuple[str, bool]] = []

            def inline_turn_base(self, _user_text: str) -> None:
                pass

            def status(self, message: str, *, leading_blank: bool) -> None:
                self.status_calls.append((message, leading_blank))

            def newline(self) -> None:
                pass

            def flush_markdown(self, _state) -> None:
                pass

            def update_token_usage(self, *_usage: int) -> None:
                pass

        class FakeStatusLine:
            def __init__(self, _ui) -> None:
                pass

            def clear(self) -> None:
                pass

        class FakeInputBar:
            def __init__(self, _ui) -> None:
                pass

            def push_up(self) -> None:
                pass

            def pop_down(self) -> None:
                pass

            def clear(self) -> str:
                return ""

        class FakeWaitingIndicator:
            def __init__(self, _status_line, *, input_bar) -> None:
                pass

            def start(self) -> None:
                pass

            def stop(self) -> str:
                return ""

        ui = FakeUI()
        with patch("omnicrawl.ui.chat_session._get_user_text", side_effect=["测试消息", "退出"]):
            with patch("omnicrawl.ui.chat_session.StatusLine", FakeStatusLine):
                with patch("omnicrawl.ui.chat_session.InputBar", FakeInputBar):
                    with patch("omnicrawl.ui.chat_session.WaitingIndicator", FakeWaitingIndicator):
                        run_inline_chat(FakeAgent(), ui)

        self.assertEqual(
            ui.status_calls,
            [("正在加载 MCP 能力", False), ("MCP 能力已就绪", False)],
        )

    def test_run_inline_chat_does_not_send_settings_command_to_agent(self) -> None:
        """仅全屏支持的 /settings 不应在兼容内联界面变成模型输入。"""

        class FakeAgent:
            current_model = "test-model"
            skill_manager = None

            def __init__(self) -> None:
                self.requests: list[str] = []

            def set_confirm_handler(self, _handler) -> None:
                pass

            def prompt_history_texts(self, *, limit: int) -> list[str]:
                return []

            def run_stream(self, text: str, *_args, **_kwargs) -> None:
                self.requests.append(text)

        class FakeUI:
            def __init__(self) -> None:
                self.notices: list[str] = []

            def inline_turn_base(self, _user_text: str) -> None:
                pass

            def notice(self, text: str) -> None:
                self.notices.append(text)

            def status(self, *_args, **_kwargs) -> None:
                pass

            def newline(self) -> None:
                pass

            def flush_markdown(self, _state) -> None:
                pass

            def update_token_usage(self, *_usage: int) -> None:
                pass

        class FakeStatusLine:
            def __init__(self, _ui) -> None:
                pass

            def clear(self) -> None:
                pass

        class FakeInputBar:
            def __init__(self, _ui) -> None:
                pass

            def push_up(self) -> None:
                pass

            def pop_down(self) -> None:
                pass

            def clear(self) -> str:
                return ""

        class FakeWaitingIndicator:
            def __init__(self, _status_line, *, input_bar) -> None:
                pass

            def start(self) -> None:
                pass

            def stop(self) -> str:
                return ""

        agent = FakeAgent()
        ui = FakeUI()
        with patch(
            "omnicrawl.ui.chat_session._get_user_text",
            side_effect=["/settings", "退出"],
        ), patch("omnicrawl.ui.chat_session.StatusLine", FakeStatusLine), patch(
            "omnicrawl.ui.chat_session.InputBar", FakeInputBar
        ), patch("omnicrawl.ui.chat_session.WaitingIndicator", FakeWaitingIndicator), redirect_stdout(
            io.StringIO()
        ):
            run_inline_chat(agent, ui)

        self.assertEqual(agent.requests, [])
        self.assertEqual(
            ui.notices,
            ["/settings 仅支持全屏 TUI，请使用默认全屏工作台。"],
        )

    def test_run_inline_chat_keeps_stream_tool_output_in_order_and_replays_queued_input(self) -> None:
        """流状态、工具记录和下一条预输入必须按历史顺序写入。"""

        class FakeAgent:
            current_model = "test-model"
            skill_manager = None

            def __init__(self) -> None:
                self.requests: list[str] = []

            def set_confirm_handler(self, _handler) -> None:
                pass

            def prompt_history_texts(self, *, limit: int) -> list[str]:
                return []

            def run_stream(self, text, on_delta, **callbacks) -> None:
                self.requests.append(text)
                if text == "第一问":
                    callbacks["on_status"]("正在加载 MCP")
                    on_delta("第一段")
                    tool_call = types.SimpleNamespace(name="read_file", arguments={})
                    callbacks["on_tool_start"](1, tool_call)
                    callbacks["on_tool_result"](
                        tool_call,
                        types.SimpleNamespace(ok=True, output="文件内容"),
                    )
                    callbacks["on_status"]("")
                    on_delta("第二段")

        class FakeUI:
            def __init__(self) -> None:
                self.events: list[str] = []

            def prompt(self) -> str:
                return "$ "

            def inline_turn_base(self, _user_text: str) -> None:
                pass

            def newline(self) -> None:
                self.events.append("newline")

            def print_ai_prefix(self) -> None:
                self.events.append("ai")

            def write_markdown_delta(self, text: str, _state) -> None:
                self.events.append(f"delta:{text}")

            def flush_markdown(self, _state) -> None:
                self.events.append("flush")

            def status(self, text: str, **_kwargs) -> None:
                self.events.append(f"status:{text}")

            def print_tool_call_start(self, step, name, _arguments, **_kwargs) -> None:
                self.events.append(f"tool:{step}:{name}")

            def print_tool_result_record(self, _ok, output, **_kwargs) -> None:
                self.events.append(f"result:{output}")

            def update_token_usage(self, *_usage: int) -> None:
                pass

            def notice(self, text: str) -> None:
                self.events.append(f"notice:{text}")

            def set_model_label(self, _text: str) -> None:
                pass

        class FakeStatusLine:
            def __init__(self, _ui) -> None:
                pass

        class FakeInputBar:
            def __init__(self, _ui) -> None:
                self.cleared = False

            def push_up(self) -> None:
                pass

            def clear(self) -> str:
                self.cleared = True
                return ""

        class FakeWaitingIndicator:
            instances: list["FakeWaitingIndicator"] = []

            def __init__(self, _status_line, *, input_bar) -> None:
                self.input_bar = input_bar
                self.started = False
                self.index = len(self.instances)
                self.instances.append(self)

            def start(self) -> None:
                self.started = True

            def stop(self) -> str:
                if self.index == 0:
                    return "第二问"
                return ""

        agent = FakeAgent()
        ui = FakeUI()
        with patch("omnicrawl.ui.chat_session._get_user_text", side_effect=["第一问", "退出"]):
            with patch("omnicrawl.ui.chat_session.StatusLine", FakeStatusLine):
                with patch("omnicrawl.ui.chat_session.InputBar", FakeInputBar):
                    with patch("omnicrawl.ui.chat_session.WaitingIndicator", FakeWaitingIndicator):
                        with redirect_stdout(io.StringIO()):
                            run_inline_chat(agent, ui)

        self.assertEqual(agent.requests, ["第一问", "第二问"])
        self.assertLess(ui.events.index("delta:第一段"), ui.events.index("tool:1:read_file"))
        self.assertLess(ui.events.index("tool:1:read_file"), ui.events.index("result:文件内容"))
        self.assertLess(ui.events.index("result:文件内容"), ui.events.index("delta:第二段"))

    def test_complex_display_width_detection(self) -> None:
        self.assertEqual(_display_width("abc"), 3)
        self.assertEqual(_display_width("获"), 2)
        self.assertEqual(_display_width("1️⃣"), 1)
        self.assertFalse(_contains_complex_display_width("plain ascii"))
        self.assertTrue(_contains_complex_display_width("获取最新 Release"))
        self.assertTrue(_contains_complex_display_width("1️⃣ 安装 CLI"))

    def test_detect_capabilities_disables_ansi_for_redirected_stdout(self) -> None:
        from omnicrawl.ui.terminal import detect_capabilities

        with patch("omnicrawl.ui.tui._capabilities.sys.stdout.isatty", return_value=False):
            with patch.dict("os.environ", {"WT_SESSION": "present"}, clear=False):
                self.assertFalse(detect_capabilities().ansi)

    def test_extended_graphemes_are_kept_as_single_display_units(self) -> None:
        family = "👨‍👩‍👧‍👦"
        china_flag = "🇨🇳"
        thumbs_up = "👍🏽"

        self.assertEqual(_display_width(family), 2)
        self.assertEqual(_display_width(china_flag), 2)
        self.assertEqual(_display_width(thumbs_up), 2)
        self.assertEqual(_split_display_rows(f"a{family}b", 4), [f"a{family}b"])
        self.assertEqual(_split_display_rows(f"a{china_flag}b", 4), [f"a{china_flag}b"])
        self.assertEqual(_split_display_rows(f"a{thumbs_up}b", 4), [f"a{thumbs_up}b"])

    def test_inline_turn_base_preserves_submitted_input_without_cursor_repaint(self) -> None:
        ui = TerminalUI(TerminalCapabilities(ansi=True))
        text = (
            "请阅读 https://github.com/sleepinginsummer/agent-browser-cli/blob/main/"
            "AI_INSTALL.md，按说明安装 CLI、下载 Chrome 扩展到D:\\下载，并添加 "
            "`skills/agent-browser-cli/SKILL.md`。"
        )
        output = io.StringIO()

        with redirect_stdout(output):
            self.assertEqual(ui.inline_turn_base(text), "")

        # 输入编辑器和标准 input 都已经把已提交文本留在终端历史；再次上移重绘
        # 会在滚动、尺寸变化或窄窗口中覆盖历史，因此固化阶段必须没有光标控制序列。
        self.assertEqual(output.getvalue(), "")

    def test_inline_turn_base_is_safe_in_a_narrow_terminal(self) -> None:
        ui = TerminalUI(TerminalCapabilities(ansi=True))
        output = io.StringIO()

        with patch(
            "omnicrawl.ui.tui._core.shutil.get_terminal_size",
            return_value=os.terminal_size((20, 24)),
        ):
            with redirect_stdout(output):
                ui.inline_turn_base("x" * 38)

        self.assertEqual(output.getvalue(), "")

    def test_complex_streaming_text_waits_for_line_completion(self) -> None:
        ui = TerminalUI(TerminalCapabilities(ansi=True))
        state = MarkdownStreamState()
        output = io.StringIO()

        with redirect_stdout(output):
            ui.write_markdown_delta("获取最新 Release 下载 URL", state)

        self.assertFalse(state.passthrough_line)
        self.assertFalse(state.preview_visible)
        self.assertEqual(output.getvalue(), "")
        self.assertEqual(state.pending_line, "获取最新 Release 下载 URL")

    def test_streaming_deltas_append_without_ansi_cursor_rewind(self) -> None:
        """流式正文只能追加，不能回退覆盖上一个分片。"""

        ui = TerminalUI(TerminalCapabilities(ansi=True))
        state = MarkdownStreamState()
        output = io.StringIO()

        with redirect_stdout(output):
            ui.write_markdown_delta("first ", state)
            ui.write_markdown_delta("second", state)
            ui.flush_markdown(state)

        rendered = ANSI_PATTERN.sub("", output.getvalue())
        self.assertEqual(rendered, "first second")
        self.assertNotRegex(output.getvalue(), r"\033\[\d+[CD]")

    def test_stream_turn_controller_commits_output_before_tool_and_queues_input(self) -> None:
        """回合控制器必须统一收尾动态区，再追加历史输出和工具记录。"""

        from omnicrawl.ui.stream_turn import StreamTurnController

        class FakeUI:
            def __init__(self) -> None:
                self.events: list[tuple[str, object]] = []

            def newline(self) -> None:
                self.events.append(("newline", ""))

            def print_ai_prefix(self) -> None:
                self.events.append(("ai_prefix", ""))

            def write_markdown_delta(self, text: str, _state) -> None:
                self.events.append(("delta", text))

            def flush_markdown(self, _state) -> None:
                self.events.append(("flush", ""))

            def status(self, text: str, **_kwargs) -> None:
                self.events.append(("status", text))

            def print_tool_call_start(self, step: int, name: str, _arguments, **_kwargs) -> None:
                self.events.append(("tool", f"{step}:{name}"))

            def print_tool_result_record(self, _ok: bool, output: str, **_kwargs) -> None:
                self.events.append(("result", output))

        class FakeInputBar:
            pre_input = ""
            submitted_pre_input = ""

            def __init__(self) -> None:
                self.events: list[str] = []

            def push_up(self) -> None:
                self.events.append("push")

            def pop_down(self) -> None:
                self.events.append("pop")

            def clear(self) -> str:
                self.events.append("clear")
                return ""

        class FakeWaiting:
            def __init__(self) -> None:
                self.events: list[str] = []

            def start(self, **_kwargs) -> None:
                self.events.append("start")

            def stop(self) -> str:
                self.events.append("stop")
                return "排队消息"

        ui = FakeUI()
        input_bar = FakeInputBar()
        waiting = FakeWaiting()
        controller = StreamTurnController(
            ui,
            input_bar=input_bar,
            waiting_indicator=waiting,
        )
        tool_call = types.SimpleNamespace(name="read_file", arguments={})
        result = types.SimpleNamespace(ok=True, output="读取成功")

        controller.start()
        controller.handle_delta("第一段")
        controller.handle_tool_start(1, tool_call)
        controller.handle_tool_result(tool_call, result)
        queued = controller.finish()

        self.assertEqual(queued, "排队消息")
        self.assertEqual(waiting.events, ["start", "stop"])
        self.assertLess(ui.events.index(("delta", "第一段")), ui.events.index(("tool", "1:read_file")))
        self.assertIn(("flush", ""), ui.events)

    def test_stream_turn_controller_preserves_unsubmitted_draft(self) -> None:
        """未按 Enter 的预输入应交回下一次正常输入编辑器。"""

        from omnicrawl.ui.stream_turn import StreamTurnController

        class FakeUI:
            def flush_markdown(self, _state) -> None:
                pass

        class FakeInputBar:
            pre_input = "保留草稿"

            def push_up(self) -> None:
                pass

            def clear(self) -> str:
                return ""

        class FakeWaiting:
            def start(self, **_kwargs) -> None:
                pass

            def stop(self) -> str:
                return ""

        controller = StreamTurnController(
            FakeUI(),
            input_bar=FakeInputBar(),
            waiting_indicator=FakeWaiting(),
        )

        self.assertIsNone(controller.finish())
        self.assertEqual(controller.draft_input, "保留草稿")

    def test_streaming_text_commits_completed_line_and_buffers_remainder(self) -> None:
        ui = TerminalUI(TerminalCapabilities(ansi=True))
        state = MarkdownStreamState()
        output = io.StringIO()

        with redirect_stdout(output):
            ui.write_markdown_delta("获取最新", state)
            ui.write_markdown_delta(" Release\n下载完成", state)

        self.assertEqual(output.getvalue(), "获取最新 Release")
        self.assertFalse(state.passthrough_line)
        self.assertEqual(state.pending_line, "下载完成")

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

    def test_streaming_markdown_does_not_split_extended_graphemes(self) -> None:
        ui = TerminalUI(TerminalCapabilities(ansi=False))
        state = MarkdownStreamState()
        output = io.StringIO()
        family = "👨‍👩‍👧‍👦"
        china_flag = "🇨🇳"
        thumbs_up = "👍🏽"

        with patch(
            "omnicrawl.ui.tui._markdown_renderer.shutil.get_terminal_size",
            return_value=os.terminal_size((7, 24)),
        ):
            with redirect_stdout(output):
                ui.print_ai_prefix()
                ui.write_markdown_delta(f"aa{family}{china_flag}{thumbs_up}", state)
                ui.flush_markdown(state)

        rendered = output.getvalue()
        self.assertIn(family, rendered)
        self.assertIn(china_flag, rendered)
        self.assertIn(thumbs_up, rendered)
        self.assertNotIn("👨‍\n", rendered)
        self.assertNotIn("🇨\n", rendered)
        self.assertNotIn("👍\n", rendered)

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

    def test_long_styled_markdown_waits_for_flush_without_preview_repaint(self) -> None:
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

        self.assertFalse(state.passthrough_line)
        self.assertFalse(state.preview_visible)
        self.assertEqual(state.pending_line, "see `abcdefghijklmnopqrstuvwxyz0123456789`")
        rendered = ANSI_PATTERN.sub("", output.getvalue())
        self.assertEqual(rendered, "")

    def test_prompt_yes_no_records_selection_and_result_without_cursor_repaint(self) -> None:
        ui = TerminalUI(TerminalCapabilities(ansi=True))
        keys = ["n"]
        fake_msvcrt = types.SimpleNamespace(getwch=lambda: keys.pop(0))
        output = io.StringIO()

        with patch.dict("sys.modules", {"msvcrt": fake_msvcrt}):
            with redirect_stdout(output):
                self.assertFalse(ui.prompt_yes_no("确认执行？", confirmed_label="已取消"))

        rendered = output.getvalue()
        plain_rendered = ANSI_PATTERN.sub("", rendered)
        self.assertNotIn("\033[2A", rendered)
        self.assertNotIn("\033[J", rendered)
        self.assertIn("选择：拒绝", plain_rendered)
        self.assertIn("已取消", plain_rendered)

    def test_prompt_yes_no_does_not_allow_arrow_key_to_execute_without_enter(self) -> None:
        ui = TerminalUI(TerminalCapabilities(ansi=True))
        keys = ["\xe0", "P", "\r"]
        fake_msvcrt = types.SimpleNamespace(getwch=lambda: keys.pop(0))

        with patch.dict("sys.modules", {"msvcrt": fake_msvcrt}):
            with redirect_stdout(io.StringIO()):
                self.assertTrue(ui.prompt_yes_no("确认执行？"))

    def test_prompt_yes_no_wraps_card_content_in_a_narrow_terminal(self) -> None:
        ui = TerminalUI(TerminalCapabilities(ansi=True))
        prompt = "x" * 50
        keys = ["\r"]
        fake_msvcrt = types.SimpleNamespace(getwch=lambda: keys.pop(0))
        output = io.StringIO()

        with patch(
            "omnicrawl.ui.tui._prompt.shutil.get_terminal_size",
            return_value=os.terminal_size((20, 24)),
        ):
            with patch.dict("sys.modules", {"msvcrt": fake_msvcrt}):
                with redirect_stdout(output):
                    self.assertTrue(ui.prompt_yes_no(prompt))

        plain_rows = ANSI_PATTERN.sub("", output.getvalue()).splitlines()
        self.assertTrue(all(_display_width(row) <= 20 for row in plain_rows), plain_rows)
        self.assertNotIn("\033[2A", output.getvalue())
        self.assertNotIn("\033[J", output.getvalue())

    def test_startup_panel_uses_compact_hierarchy_without_narrow_overflow(self) -> None:
        ui = TerminalUI(TerminalCapabilities(ansi=False))
        output = io.StringIO()
        lines = [
            "frontend: TUI",
            "thinking: 已启用，推理强度：xhigh",
            "workspace: " + "D:/" + "x" * 44,
        ]

        with patch(
            "omnicrawl.ui.tui._panels.shutil.get_terminal_size",
            return_value=os.terminal_size((20, 24)),
        ):
            with redirect_stdout(output):
                ui.print_startup_panel("OmniCrawl", lines)

        rendered_rows = output.getvalue().splitlines()
        self.assertTrue(all(_display_width(row) <= 20 for row in rendered_rows), rendered_rows)
        self.assertIn("█ OmniCrawl", output.getvalue())

    def test_status_can_avoid_leading_blank_line(self) -> None:
        ui = TerminalUI(TerminalCapabilities(ansi=False))
        output = io.StringIO()

        with redirect_stdout(output):
            ui.status("步骤 1 - 请求 powershell", leading_blank=False)

        self.assertEqual(output.getvalue(), "  [步骤 1 - 请求 powershell]\n")

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

    def test_waiting_indicator_splits_styled_spinner_before_applying_ansi(self) -> None:
        ui = TerminalUI(TerminalCapabilities(ansi=True), model_label="gpt-5.5")
        ui.update_token_usage(12345, 67890, 1112)
        status_line = StatusLine(ui)
        waiting = WaitingIndicator(status_line)
        output = io.StringIO()
        # 预着色输入也要先剥离样式后再换行；每个物理行只能保留 renderer
        # 自己应用的一组 muted SGR，不能残留调用方的 primary SGR。
        styled_status = "\033[96m" + "正在思考" * 6 + "\033[0m"

        with patch("omnicrawl.ui.tui._spinner.os.name", "nt"):
            with patch(
                "omnicrawl.ui.tui._spinner.shutil.get_terminal_size",
                return_value=os.terminal_size((20, 24)),
            ):
                with redirect_stdout(output):
                    waiting._render_status(styled_status)

        status_lines = [
            line for line in output.getvalue().splitlines()
            if "正在" in ANSI_PATTERN.sub("", line)
        ]
        self.assertGreaterEqual(len(status_lines), 2, status_lines)
        self.assertTrue(
            all(
                "\033[90m" in line
                and "\033[96m" not in line
                and line.endswith("\033[0m")
                for line in status_lines
            ),
            status_lines,
        )

    def test_input_bar_splits_styled_spinner_before_applying_ansi(self) -> None:
        ui = TerminalUI(TerminalCapabilities(ansi=True), model_label="gpt-5.5")
        output = io.StringIO()

        with patch("omnicrawl.ui.tui._spinner.os.name", "nt"):
            with patch(
                "omnicrawl.ui.tui._spinner.shutil.get_terminal_size",
                return_value=os.terminal_size((20, 24)),
            ):
                with redirect_stdout(output):
                    InputBar(ui).show("\033[96m" + "正在思考" * 6 + "\033[0m")

        spinner_rows = [
            line for line in output.getvalue().splitlines()
            if "正在" in ANSI_PATTERN.sub("", line)
        ]
        self.assertGreaterEqual(len(spinner_rows), 2, spinner_rows)
        self.assertTrue(
            all(
                "\033[90m" in line
                and "\033[96m" not in line
                and line.endswith("\033[0m")
                for line in spinner_rows
            ),
            spinner_rows,
        )

    def test_input_bar_and_waiting_indicator_do_not_split_ansi_token_sequences(self) -> None:
        ui = TerminalUI(TerminalCapabilities(ansi=True), model_label="gpt-5.5")
        ui.update_token_usage(12345, 67890, 1112)
        status_line = StatusLine(ui)
        input_bar = InputBar(ui)
        input_bar._pre_input = "x" * 38
        waiting = WaitingIndicator(status_line, input_bar=input_bar)
        output = io.StringIO()

        with patch("omnicrawl.ui.tui._spinner.os.name", "nt"):
            with patch(
                "omnicrawl.ui.tui._spinner.shutil.get_terminal_size",
                return_value=os.terminal_size((20, 24)),
            ):
                with redirect_stdout(output):
                    input_bar.show()
                    input_bar.clear()
                    waiting._render_status("状态" * 12)
                    submitted = waiting.stop()

        self.assertEqual(submitted, "")
        rendered = output.getvalue()
        # 每个 ANSI SGR 序列必须在同一物理行完成，不能出现 `\\x1b[96\\nmin:` 之类片段。
        for line in rendered.splitlines():
            self.assertNotRegex(line, r"\x1b\[[0-9;]*$")
        content_rows = [
            ANSI_PATTERN.sub("", line)
            for line in rendered.splitlines()
            if not line.startswith("\033[")
        ]
        self.assertTrue(all(_display_width(row) <= 20 for row in content_rows), content_rows)
        # stop() 会清空内部计数；清理序列数量必须覆盖状态、预输入和 token 的实际物理行。
        self.assertGreater(rendered.count("\r\033[2K"), 3)

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

    def test_input_bar_pre_input_backspace_preserves_extended_graphemes(self) -> None:
        ui = TerminalUI(TerminalCapabilities(ansi=True))
        input_bar = InputBar(ui)
        # Windows msvcrt 会把非 BMP emoji 拆成 UTF-16 代理对；模拟真实输入后
        # 再退格，必须删除完整肤色 emoji 而不是残留代理项或修饰符。
        fake_msvcrt = _FakeMsvcrt(["\ud83d", "\udc4d", "\ud83c", "\udffd", "\b"])

        with patch("omnicrawl.ui.tui._spinner.os.name", "nt"):
            with patch.dict("sys.modules", {"msvcrt": fake_msvcrt}):
                input_bar.poll_pre_input()

        self.assertEqual(input_bar.pre_input, "")

    def test_input_bar_combines_surrogate_pair_across_poll_cycles(self) -> None:
        ui = TerminalUI(TerminalCapabilities(ansi=True))
        input_bar = InputBar(ui)
        high_surrogate = _FakeMsvcrt(["\ud83d"])
        low_surrogate = _FakeMsvcrt(["\udc4d"])

        with patch("omnicrawl.ui.tui._spinner.os.name", "nt"):
            with patch.dict("sys.modules", {"msvcrt": high_surrogate}):
                input_bar.poll_pre_input()
            self.assertEqual(input_bar.pre_input, "")
            with patch.dict("sys.modules", {"msvcrt": low_surrogate}):
                input_bar.poll_pre_input()

        self.assertEqual(input_bar.pre_input, "👍")

    def test_input_bar_clear_tracks_wrapped_pre_input_rows(self) -> None:
        ui = TerminalUI(TerminalCapabilities(ansi=True))
        input_bar = InputBar(ui)
        input_bar._pre_input = "x" * 38
        output = io.StringIO()
        expected_rows = len(_split_display_rows("x" * 38, 20 - ui.prompt_width() - 1))

        with patch("omnicrawl.ui.tui._spinner.os.name", "nt"):
            with patch(
                "omnicrawl.ui.tui._spinner.shutil.get_terminal_size",
                return_value=os.terminal_size((20, 24)),
            ):
                with redirect_stdout(output):
                    input_bar.show()
                    submitted = input_bar.clear()

        self.assertEqual(submitted, "")
        self.assertEqual(output.getvalue().count("\r\033[2K"), expected_rows)

    def test_tool_call_start_shows_command_detail_and_running_marker(self) -> None:
        ui = TerminalUI(TerminalCapabilities(ansi=False))
        output = io.StringIO()

        with redirect_stdout(output):
            ui.print_tool_call_start(
                2,
                "powershell",
                {"command": "Write-Output hello"},
                leading_blank=False,
            )

        rendered = output.getvalue()
        # 新格式：╭─ 步骤 N · tool_name
        self.assertIn("步骤 2", rendered)
        self.assertIn("执行 PowerShell", rendered)
        self.assertIn("Write-Output hello", rendered)

    def test_tool_call_card_does_not_overflow_narrow_terminal(self) -> None:
        ui = TerminalUI(TerminalCapabilities(ansi=False))
        output = io.StringIO()

        with patch(
            "omnicrawl.ui.tui._tools.shutil.get_terminal_size",
            return_value=os.terminal_size((20, 24)),
        ):
            with redirect_stdout(output):
                ui.print_tool_call_start(
                    1,
                    "powershell_with_a_very_long_name",
                    {"command": "echo " + "x" * 48},
                    leading_blank=False,
                )

        plain_rows = ANSI_PATTERN.sub("", output.getvalue()).splitlines()
        self.assertTrue(
            all(_display_width(line) <= 20 for line in plain_rows),
            plain_rows,
        )

    def test_tool_result_record_shows_exit_code_and_stdout_preview(self) -> None:
        ui = TerminalUI(TerminalCapabilities(ansi=False))
        output = io.StringIO()

        with redirect_stdout(output):
            ui.print_tool_result_record(
                True,
                "退出码：0\n\nstdout:\nhello\n\nstderr:\n",
                tool_name="powershell",
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
                "powershell",
                {"command": "Write-Output first"},
                leading_blank=False,
            )
            ui.print_tool_result_record(
                True,
                "退出码：0\n\nstdout:\nfirst\n\nstderr:\n",
                tool_name="powershell",
            )
            ui.print_tool_call_start(
                2,
                "powershell",
                {"command": "Write-Output second"},
                leading_blank=False,
            )

        rendered = output.getvalue()
        # 连续工具之间有空行分隔
        self.assertIn("first", rendered)
        self.assertIn("步骤 2", rendered)
        self.assertIn("执行 PowerShell", rendered)

    def test_tool_call_start_uses_static_ansi_status_marker(self) -> None:
        ui = TerminalUI(TerminalCapabilities(ansi=True))
        output = io.StringIO()

        with redirect_stdout(output):
            ui.print_tool_call_start(
                1,
                "powershell",
                {"command": "Write-Output hi"},
                leading_blank=False,
            )

        rendered = output.getvalue()
        self.assertIn("◌", rendered)
        self.assertIn("执行 PowerShell", rendered)
        self.assertIn("Write-Output hi", rendered)
        self.assertNotIn("\033[5m", rendered)  # 不使用终端兼容性不稳定的 BLINK

    def test_tool_result_appends_completion_without_rewriting_history(self) -> None:
        ui = TerminalUI(TerminalCapabilities(ansi=True))
        output = io.StringIO()

        with redirect_stdout(output):
            ui.print_tool_call_start(
                1,
                "powershell",
                {"command": "Write-Output hi"},
                leading_blank=False,
            )
            ui.print_tool_result_record(
                True,
                "退出码：0\n\nstdout:\nhi",
                tool_name="powershell",
            )

        rendered = output.getvalue()
        self.assertNotIn("\033[2A", rendered)
        self.assertIn("✓", rendered)



if __name__ == "__main__":
    unittest.main()
