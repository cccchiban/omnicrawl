from __future__ import annotations

import sys
import threading
import time
import unittest
from types import SimpleNamespace
from unittest.mock import call, patch


class FullscreenTUITest(unittest.IsolatedAsyncioTestCase):
    def test_should_disable_mouse_reporting_when_fullscreen_tui_exits(self) -> None:
        """全屏界面正常结束后必须兜底关闭终端鼠标报告协议。"""

        from io import StringIO

        from omnicrawl.ui.fullscreen import FullscreenStartup, run_fullscreen_tui

        output = StringIO()

        class FakeApp:
            def __init__(self, _agent, _startup) -> None:
                pass

            def run(self) -> None:
                output.write("app-stopped")

        with patch("omnicrawl.ui.fullscreen.OmniCrawlApp", FakeApp), patch(
            "omnicrawl.ui.fullscreen.sys.__stdout__",
            output,
        ):
            run_fullscreen_tui(object(), FullscreenStartup(False, "none", "", "", ""))

        self.assertTrue(
            output.getvalue().endswith(
                "\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1015l\x1b[?1006l"
            )
        )

    def test_should_disable_mouse_reporting_when_fullscreen_tui_raises(self) -> None:
        """全屏界面异常退出时也不能把鼠标报告状态泄漏给 PowerShell。"""

        from io import StringIO

        from omnicrawl.ui.fullscreen import FullscreenStartup, run_fullscreen_tui

        output = StringIO()

        class FailingApp:
            def __init__(self, _agent, _startup) -> None:
                pass

            def run(self) -> None:
                raise RuntimeError("tui failed")

        with patch("omnicrawl.ui.fullscreen.OmniCrawlApp", FailingApp), patch(
            "omnicrawl.ui.fullscreen.sys.__stdout__",
            output,
        ):
            with self.assertRaisesRegex(RuntimeError, "tui failed"):
                run_fullscreen_tui(
                    object(),
                    FullscreenStartup(False, "none", "", "", ""),
                )

        self.assertEqual(
            output.getvalue(),
            "\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1015l\x1b[?1006l",
        )

    def test_should_propagate_textual_fatal_return_code(self) -> None:
        """Textual 捕获定时器异常后返回的非零状态不能被误报为正常结束。"""

        from io import StringIO

        from omnicrawl.ui.fullscreen import FullscreenStartup, run_fullscreen_tui

        output = StringIO()

        class FatalApp:
            return_code = 1

            def __init__(self, _agent, _startup) -> None:
                pass

            def run(self) -> None:
                pass

        with patch("omnicrawl.ui.fullscreen.OmniCrawlApp", FatalApp), patch(
            "omnicrawl.ui.fullscreen.sys.__stdout__",
            output,
        ):
            exit_code = run_fullscreen_tui(
                object(),
                FullscreenStartup(False, "none", "", "", ""),
            )

        self.assertEqual(exit_code, 1)
        self.assertTrue(output.getvalue().endswith("\x1b[?1006l"))

    async def test_fullscreen_layout_uses_single_line_hud_and_compact_composer(self) -> None:
        """界面应使用两行稳态 HUD、无侧栏和五行高的多行输入舱。"""

        from rich.text import Text
        from textual.widgets import TextArea, Static

        from omnicrawl.ui.fullscreen import FullscreenStartup, OmniCrawlApp
        from omnicrawl.ui.fullscreen.theme import (
            TERMINAL_BACKGROUND,
            TERMINAL_FOREGROUND,
            TEXT_MUTED,
            TEXT_PRIMARY,
            THEME_NAME,
        )

        class FakeAgent:
            current_model = "demo-model"
            approval_mode = "manual"
            reasoning_effort = "max"
            workspace_root = "D:/workspace"
            current_session_id = "session-demo"
            skill_manager = None
            _mcp_manager = SimpleNamespace(
                config=SimpleNamespace(enabled=True, enabled_servers=["one", "two"])
            )

            def set_confirm_handler(self, _handler) -> None:
                pass

        app = OmniCrawlApp(
            FakeAgent(),
            FullscreenStartup(
                True,
                "max",
                "人工确认",
                "D:/workspace",
                ".agent_tmp，每 24 小时自动清理",
                current_version="0.1.1",
            ),
        )

        async with app.run_test(size=(140, 40)) as pilot:
            await pilot.pause()
            self.assertEqual(len(app.query("#sidebar")), 0)
            self.assertEqual(len(app.query("#header")), 0)
            self.assertEqual(len(app.query("#hint")), 0)
            self.assertEqual(app.query_one("#topbar").region.y, 0)
            self.assertEqual(app.query_one("#token-telemetry").region.y, 1)
            # 顶部由稳态上下文占满；运行态进入对话区且空闲时不存在。
            self.assertEqual(len(app.query("#brand")), 0)
            self.assertEqual(len(app.query("#topbar > *")), 2)
            self.assertEqual(len(app.query(".runtime-status-message")), 0)
            topbar = app.query_one("#topbar")
            context_widget = app.query_one("#context-summary", Static)
            self.assertEqual(context_widget.region.x, topbar.content_region.x)
            # 第一行仅项目绝对路径 + 行尾索引状态；第二行为遥测（左）+ 模型/状态（右），
            # 各组件内容紧排，行首无竖线，无行尾闭合元素。
            index_widget = app.query_one("#index-status", Static)
            self.assertEqual(index_widget.region.y, 0)
            status_widget = app.query_one("#status-summary", Static)
            self.assertEqual(status_widget.region.y, 1)
            self.assertLessEqual(status_widget.region.right, app.query_one("#telemetry-row").content_region.right)
            context = context_widget.content
            # 第一行显示项目绝对路径（灰色），不再取文件夹名。
            self.assertTrue(context.plain.startswith("D:/workspace"))
            self.assertIn("D:/workspace", context.plain)
            self.assertNotIn("demo-model", context.plain)
            self.assertNotIn("THK MAX", context.plain)
            self.assertNotIn(".agent_tmp", context.plain)
            # 模型、推理强度、审批模式、排队数与 MCP 数量在第二行右段，
            # 前置 ⁕ 分隔符（衔接遥测 t/s），以 QUE 段收束。
            status = status_widget.content
            self.assertIn("APR MAN", status.plain)
            self.assertIn("QUE 0", status.plain)
            self.assertIn("MCP 2", status.plain)
            self.assertTrue(status.plain.startswith("⁕ demo-model"))
            self.assertIn("THK MAX", status.plain)
            self.assertTrue(status.plain.endswith("QUE 0 " ))
            self.assertNotIn("·", context.plain)
            self.assertNotIn("·", status.plain)
            self.assertEqual(app.theme, THEME_NAME)
            self.assertTrue(app.current_theme.ansi)
            self.assertEqual(app.current_theme.background, TERMINAL_BACKGROUND)
            self.assertEqual(app.current_theme.foreground, TERMINAL_FOREGROUND)
            self.assertIn(TEXT_MUTED, str(context.spans))
            self.assertNotIn("ansi_blue", str(context.spans))
            self.assertNotIn("ansi_green", str(context.spans))
            self.assertNotIn("ansi_magenta", str(context.spans))
            token_widget = app.query_one("#token-telemetry", Static)
            telemetry = str(token_widget.content)
            # 遥测位于第二行最左：用量/总量 + 百分比起头（上下文限制 128K）。
            self.assertTrue(telemetry.startswith("0/128K 0%"))
            self.assertIn("0/128K 0%", telemetry)
            self.assertIn("↑0", telemetry)
            self.assertIn("↓0", telemetry)
            self.assertIn("†0", telemetry)
            self.assertIn("CH0%", telemetry)
            self.assertIn("-- t/s", telemetry)
            self.assertNotIn("MCP", telemetry)
            self.assertNotIn("·", telemetry)
            self.assertNotIn("omnicrawl", telemetry)
            # 第二行容器：模型/状态与遥测同排，单行内容 + 底边框。
            telemetry_row = app.query_one("#telemetry-row")
            self.assertEqual(telemetry_row.region.height, 2)
            self.assertEqual(token_widget.region.height, 1)
            self.assertGreater(token_widget.size.height, 0)
            # 索引状态空闲时为空文本，渲染后组件整体隐藏避免分隔符占位。
            app._render_search_index_status()
            self.assertFalse(index_widget.display)
            # 使用非空状态模拟真实索引加载，避免后台轮询在下一帧把手动显示复位。
            with patch.object(
                app,
                "_search_index_status_text",
                return_value=Text("⠋ 加载索引"),
            ):
                app._render_search_index_status()
                await pilot.pause()
            self.assertTrue(index_widget.display)
            self.assertIn("⁕", str(index_widget.content))
            self.assertIn("加载索引", str(index_widget.content))
            rendered_token = "".join(segment.text for segment in token_widget.render_line(0))
            self.assertIn("↑0", rendered_token)
            self.assertIn("0/128K", rendered_token)
            # 模型/状态段紧贴遥测右侧（遥测在最左）。
            self.assertEqual(status_widget.region.x, token_widget.region.right)
            self.assertEqual(app.query_one("#composer-wrap").region.height, 2)
            composer = app.query_one("#composer", TextArea)
            self.assertEqual(composer.region.height, 1)
            self.assertTrue(composer.has_focus)
            cursor_style = composer.get_component_rich_style("text-area--cursor")
            self.assertEqual(cursor_style.color.number, 0)  # ansi_black
            self.assertEqual(cursor_style.bgcolor.number, 7)  # ansi_white
            self.assertFalse(cursor_style.reverse)
            self.assertEqual(composer.styles.background.a, 0)

    async def test_hud_fits_within_content_when_terminal_is_compact(self) -> None:
        """较窄终端下顶部两行各段不得溢出内容区（第二行满数据约 86 cell）。"""

        from textual.widgets import Static

        from omnicrawl.ui.fullscreen import FullscreenStartup, OmniCrawlApp

        class FakeAgent:
            current_model = "demo-model"
            approval_mode = "manual"
            reasoning_effort = "max"
            workspace_root = "D:/workspace"
            current_session_id = "session-demo"
            skill_manager = None
            _mcp_manager = SimpleNamespace(
                config=SimpleNamespace(enabled=True, enabled_servers=[])
            )

            def set_confirm_handler(self, _handler) -> None:
                pass

        app = OmniCrawlApp(
            FakeAgent(),
            FullscreenStartup(
                True,
                "max",
                "人工确认",
                "D:/workspace",
                ".agent_tmp",
                current_version="0.1.1",
            ),
        )

        async with app.run_test(size=(100, 30)) as pilot:
            await pilot.pause(0.1)
            app._render_search_index_status()
            await pilot.pause(0.1)

            topbar = app.query_one("#topbar")
            telemetry_row = app.query_one("#telemetry-row")

            self.assertLessEqual(
                app.query_one("#context-summary", Static).region.right,
                topbar.content_region.right,
            )
            self.assertLessEqual(
                app.query_one("#status-summary", Static).region.right,
                telemetry_row.content_region.right,
            )

    async def test_composer_starts_on_one_row_and_grows_with_wrapped_text(self) -> None:
        """输入区初始一行，长文本和真实换行均自动扩展，最多保留五行。"""

        from textual.widgets import TextArea

        from omnicrawl.ui.fullscreen import FullscreenStartup, OmniCrawlApp

        class FakeAgent:
            current_model = "demo-model"
            current_session_id = "session-demo"
            skill_manager = None

            def set_confirm_handler(self, _handler) -> None:
                pass

        app = OmniCrawlApp(
            FakeAgent(),
            FullscreenStartup(True, "max", "人工确认", "D:/workspace", ".agent_tmp"),
        )
        submitted: list[str] = []
        app._submit = submitted.append  # type: ignore[method-assign]

        async with app.run_test(size=(30, 20)) as pilot:
            composer = app.query_one("#composer", TextArea)
            self.assertEqual(composer.region.height, 1)
            for shift_enter_key in ("shift+enter", "shift+\r", "shift+j"):
                composer.clear()
                await pilot.press("a", shift_enter_key, "b")
                await pilot.pause()
                self.assertEqual(composer.text, "a\nb")

            composer.text = "第一行\n第二行\n第三行"
            await pilot.pause()
            self.assertEqual(composer.region.height, 3)

            composer.text = "这是一个用于验证软折行不会出现横向滚动条的很长输入内容"
            await pilot.pause()
            self.assertTrue(composer.soft_wrap)
            self.assertFalse(composer.show_horizontal_scrollbar)
            self.assertGreater(composer.region.height, 1)

            composer.text = "\n".join(f"第 {index} 行" for index in range(8))
            await pilot.pause()
            self.assertEqual(composer.region.height, app.COMPOSER_MAX_ROWS)

            composer.clear()
            await pilot.pause()
            self.assertEqual(composer.region.height, 1)

            composer.text = "第一行\n第二行"
            await pilot.press("enter")
            await pilot.pause()
            self.assertEqual(submitted, ["第一行\n第二行"])
            self.assertEqual(composer.text, "")

    @unittest.skipUnless(sys.platform == "win32", "仅验证 Windows 终端按键协议")
    def test_windows_driver_uses_raw_console_input_for_modifier_keys(self) -> None:
        """Windows Driver 必须关闭 VT 输入，避免终端吞掉 Shift 修饰位。"""

        from textual.drivers import win32 as textual_win32
        from textual.drivers.windows_driver import WindowsDriver

        from omnicrawl.ui.fullscreen import (
            FullscreenStartup,
            OmniCrawlApp,
            OmniCrawlWindowsDriver,
            OmniCrawlWindowsEventMonitor,
        )

        class FakeAgent:
            current_model = "demo-model"
            current_session_id = "session-demo"
            skill_manager = None

            def set_confirm_handler(self, _handler) -> None:
                pass

        app = OmniCrawlApp(
            FakeAgent(),
            FullscreenStartup(True, "max", "人工确认", "D:/workspace", ".agent_tmp"),
        )
        self.assertIs(app.driver_class, OmniCrawlWindowsDriver)

        driver = object.__new__(OmniCrawlWindowsDriver)
        protocol_calls: list[str] = []
        driver.write = protocol_calls.append  # type: ignore[method-assign]
        driver.flush = lambda: protocol_calls.append("flush")  # type: ignore[method-assign]

        input_mode = 0x0001 | textual_win32.ENABLE_VIRTUAL_TERMINAL_INPUT
        observed_event_monitors: list[object] = []
        original_event_monitor = textual_win32.EventMonitor
        with patch.object(WindowsDriver, "start_application_mode") as base_start, patch.object(
            textual_win32,
            "get_console_mode",
            side_effect=[input_mode, 0x0001],
        ) as get_console_mode, patch.object(
            textual_win32,
            "set_console_mode",
            return_value=True,
        ) as set_console_mode:
            base_start.side_effect = lambda: observed_event_monitors.append(
                textual_win32.EventMonitor
            )
            driver.start_application_mode()

        base_start.assert_called_once_with()
        self.assertEqual(observed_event_monitors, [OmniCrawlWindowsEventMonitor])
        self.assertIs(textual_win32.EventMonitor, original_event_monitor)
        get_console_mode.assert_has_calls(
            [call(sys.__stdin__), call(sys.__stdout__)]
        )
        set_console_mode.assert_has_calls(
            [
                call(
                    sys.__stdin__,
                    (
                        input_mode
                        | textual_win32.ENABLE_MOUSE_INPUT
                        | textual_win32.ENABLE_WINDOW_INPUT
                        | textual_win32.ENABLE_EXTENDED_FLAGS
                    )
                    & ~(
                        textual_win32.ENABLE_QUICK_EDIT_MODE
                        | textual_win32.ENABLE_VIRTUAL_TERMINAL_INPUT
                    ),
                ),
                call(sys.__stdout__, 0x0001 | textual_win32.ENABLE_VIRTUAL_TERMINAL_PROCESSING),
            ]
        )
        self.assertEqual(
            protocol_calls,
            ["flush"],
        )

    def test_windows_driver_recovers_shift_enter_when_terminal_drops_modifier(self) -> None:
        """终端未提供 Kitty 修饰位时，Driver 仍应从 Windows 键态恢复 Shift。"""

        from textual import events
        from textual.drivers import win32

        from omnicrawl.ui.fullscreen import OmniCrawlWindowsEventMonitor

        record = win32.KEY_EVENT_RECORD()
        record.bKeyDown = True
        record.wVirtualKeyCode = 0x0D
        record.uChar.UnicodeChar = "\r"
        record.dwControlKeyState = OmniCrawlWindowsEventMonitor.WINDOWS_SHIFT_PRESSED

        event = OmniCrawlWindowsEventMonitor.key_event_to_textual(record)

        self.assertIsInstance(event, events.Key)
        self.assertEqual(event.key, "shift+enter")
        self.assertEqual(event.character, "\r")

        record.dwControlKeyState = 0
        self.assertIsNone(OmniCrawlWindowsEventMonitor.key_event_to_textual(record))

    @unittest.skipUnless(sys.platform == "win32", "仅验证 Windows 终端按键协议")
    def test_windows_event_monitor_maps_raw_navigation_keys(self) -> None:
        """原始控制台的导航键必须保留修饰状态并转换为 Textual 事件。"""

        from textual import events
        from textual.drivers import win32

        from omnicrawl.ui.fullscreen import OmniCrawlWindowsEventMonitor

        record = win32.KEY_EVENT_RECORD()
        record.bKeyDown = True
        record.wVirtualKeyCode = 0x25
        record.uChar.UnicodeChar = "\x00"
        record.dwControlKeyState = OmniCrawlWindowsEventMonitor.WINDOWS_SHIFT_PRESSED

        event = OmniCrawlWindowsEventMonitor.key_event_to_textual(record)

        self.assertIsInstance(event, events.Key)
        self.assertEqual(event.key, "shift+left")
        self.assertIsNone(event.character)

        record.dwControlKeyState = 0
        event = OmniCrawlWindowsEventMonitor.key_event_to_textual(record)
        self.assertIsInstance(event, events.Key)
        self.assertEqual(event.key, "left")
        self.assertIsNone(event.character)

        record.wVirtualKeyCode = 0x10
        self.assertIsNone(OmniCrawlWindowsEventMonitor.key_event_to_textual(record))

    @unittest.skipUnless(sys.platform == "win32", "仅验证 Windows 终端鼠标协议")
    def test_should_map_raw_mouse_clicks_when_windows_console_reports_buttons(self) -> None:
        """原始控制台鼠标按下和释放必须成为 Textual 鼠标事件。"""

        from textual import events
        from textual.drivers import win32

        from omnicrawl.ui.fullscreen import OmniCrawlWindowsEventMonitor

        record = win32.MOUSE_EVENT_RECORD()
        record.dwMousePosition.X = 4
        record.dwMousePosition.Y = 7
        record.dwButtonState = 0x0001
        record.dwControlKeyState = OmniCrawlWindowsEventMonitor.WINDOWS_SHIFT_PRESSED
        record.dwEventFlags = 0

        messages, button_state, position = OmniCrawlWindowsEventMonitor.mouse_events_from_raw(
            record,
            previous_button_state=0,
            previous_position=(0, 0),
        )

        self.assertEqual(button_state, 0x0001)
        self.assertEqual(position, (4, 7))
        self.assertEqual(len(messages), 1)
        self.assertIsInstance(messages[0], events.MouseDown)
        self.assertEqual(messages[0].button, 1)
        self.assertEqual((messages[0].screen_x, messages[0].screen_y), (4, 7))
        self.assertTrue(messages[0].shift)

        record.dwButtonState = 0
        messages, button_state, position = OmniCrawlWindowsEventMonitor.mouse_events_from_raw(
            record,
            previous_button_state=button_state,
            previous_position=position,
        )

        self.assertEqual(button_state, 0)
        self.assertEqual(len(messages), 1)
        self.assertIsInstance(messages[0], events.MouseUp)
        self.assertEqual(messages[0].button, 1)

    @unittest.skipUnless(sys.platform == "win32", "仅验证 Windows 终端鼠标协议")
    def test_should_map_raw_mouse_wheel_when_windows_console_reports_delta(self) -> None:
        """Windows 原始滚轮记录必须保留方向并进入 Textual 事件链。"""

        from textual import events
        from textual.drivers import win32

        from omnicrawl.ui.fullscreen import OmniCrawlWindowsEventMonitor

        record = win32.MOUSE_EVENT_RECORD()
        record.dwMousePosition.X = 8
        record.dwMousePosition.Y = 10
        record.dwButtonState = 120 << 16
        record.dwEventFlags = 0x0004

        messages, button_state, position = OmniCrawlWindowsEventMonitor.mouse_events_from_raw(
            record,
            previous_button_state=0,
            previous_position=(8, 10),
        )

        self.assertEqual(button_state, 0)
        self.assertEqual(position, (8, 10))
        self.assertEqual(len(messages), 1)
        self.assertIsInstance(messages[0], events.MouseScrollUp)

        record.dwButtonState = (-120 & 0xFFFF) << 16
        messages, _, _ = OmniCrawlWindowsEventMonitor.mouse_events_from_raw(
            record,
            previous_button_state=0,
            previous_position=position,
        )
        self.assertEqual(len(messages), 1)
        self.assertIsInstance(messages[0], events.MouseScrollDown)

    async def test_should_insert_newline_when_shift_enter_is_reported_as_lf_enter(self) -> None:
        """终端把 Shift+Enter 退化为 LF 形式的 Enter 时仍应插入换行。"""

        from textual import events
        from textual.widgets import TextArea

        from omnicrawl.ui.fullscreen import FullscreenStartup, OmniCrawlApp

        class FakeAgent:
            current_model = "demo-model"
            current_session_id = "session-demo"
            skill_manager = None

            def set_confirm_handler(self, _handler) -> None:
                pass

        app = OmniCrawlApp(
            FakeAgent(),
            FullscreenStartup(True, "max", "人工确认", "D:/workspace", ".agent_tmp"),
        )
        submitted: list[str] = []
        app._submit = submitted.append  # type: ignore[method-assign]

        async with app.run_test(size=(80, 24)) as pilot:
            composer = app.query_one("#composer", TextArea)
            composer.text = "第一行"
            composer.cursor_location = (0, len("第一行"))
            composer.on_key(events.Key("enter", "\n"))
            await pilot.pause()

            self.assertEqual(composer.text, "第一行\n")
            self.assertEqual(submitted, [])

            composer.on_key(events.Key("enter", "\r"))
            await pilot.pause()
            self.assertEqual(submitted, ["第一行"])
            self.assertEqual(composer.text, "")

    async def test_long_paste_is_compacted_in_composer_and_expanded_on_submit(self) -> None:
        """超过五行的粘贴内容只在输入框中缩略，提交仍发送原文。"""

        from textual import events
        from textual.widgets import TextArea

        from omnicrawl.ui.fullscreen import FullscreenStartup, OmniCrawlApp

        class FakeAgent:
            current_model = "demo-model"
            current_session_id = "session-demo"
            skill_manager = None

            def set_confirm_handler(self, _handler) -> None:
                pass

        app = OmniCrawlApp(
            FakeAgent(),
            FullscreenStartup(True, "max", "人工确认", "D:/workspace", ".agent_tmp"),
        )
        submitted: list[str] = []
        app._submit = submitted.append  # type: ignore[method-assign]

        async with app.run_test(size=(80, 24)) as pilot:
            composer = app.query_one("#composer", TextArea)
            short_paste = "\n".join(f"短粘贴 {index}" for index in range(5))
            composer.post_message(events.Paste(short_paste))
            await pilot.pause()

            self.assertEqual(composer.text, short_paste)

            composer.clear()
            long_paste = "\n".join(f"长粘贴 {index}" for index in range(6))
            composer.post_message(events.Paste(long_paste))
            await pilot.pause()

            self.assertEqual(composer.text, "[粘贴 #1 +6 行]")
            self.assertEqual(composer.region.height, 1)

            await pilot.press("enter")
            await pilot.pause()

            self.assertEqual(submitted, [long_paste])
            self.assertEqual(composer.text, "")

            app._clipboard = "\n".join(f"剪贴板粘贴 {index}" for index in range(8))
            composer.action_paste()
            await pilot.pause()

            self.assertEqual(composer.text, "[粘贴 #2 +8 行]")

    async def test_long_paste_is_expanded_when_queued_during_generation(self) -> None:
        """生成期间排队的长粘贴也必须保存原文，而不是保存占位符。"""

        from textual import events
        from textual.widgets import TextArea

        from omnicrawl.ui.fullscreen import FullscreenStartup, OmniCrawlApp

        class FakeAgent:
            current_model = "demo-model"
            current_session_id = "session-demo"
            skill_manager = None

            def set_confirm_handler(self, _handler) -> None:
                pass

        app = OmniCrawlApp(
            FakeAgent(),
            FullscreenStartup(True, "max", "人工确认", "D:/workspace", ".agent_tmp"),
        )

        async with app.run_test(size=(80, 24)) as pilot:
            composer = app.query_one("#composer", TextArea)
            app.is_generating = True
            long_paste = "\n".join(f"排队粘贴 {index}" for index in range(7))
            composer.post_message(events.Paste(long_paste))
            await pilot.pause()

            self.assertEqual(composer.text, "[粘贴 #1 +7 行]")

            await pilot.press("enter")
            await pilot.pause()

            self.assertEqual(list(app._pending_inputs), [long_paste])
            self.assertEqual(composer.text, "")

    def test_terminal_css_is_transparent_and_self_contained(self) -> None:
        import re

        from omnicrawl.ui.fullscreen import OmniCrawlApp
        from omnicrawl.ui.fullscreen.model_picker import ModelPickerScreen
        from omnicrawl.ui.fullscreen.settings import SettingsScreen
        from omnicrawl.ui.fullscreen.theme import (
            REASONING_BACKGROUND,
            REASONING_FOCUS_BACKGROUND,
            TERMINAL_FOREGROUND,
            TOOL_BACKGROUND,
            TOOL_FOCUS_BACKGROUND,
            TOOL_TEXT,
            USER_BACKGROUND,
            terminal_css,
        )
        from omnicrawl.ui.fullscreen.widgets import ConfirmationScreen

        self.assertEqual(
            terminal_css("$terminal-text-secondary|$terminal-text"),
            f"{TERMINAL_FOREGROUND}|{TERMINAL_FOREGROUND}",
        )
        app_backgrounds = set(
            re.findall(r"(?<!-)background:\s*([^;]+);", OmniCrawlApp.CSS)
        )
        self.assertIn(USER_BACKGROUND, app_backgrounds)
        self.assertIn(".assistant-message { color: ansi_default; }", OmniCrawlApp.CSS)
        self.assertIn(
            f".tool-message:focus {{ color: ansi_bright_black; "
            f"background: {TOOL_FOCUS_BACKGROUND}; }}",
            OmniCrawlApp.CSS,
        )
        self.assertIn(TOOL_BACKGROUND, app_backgrounds)
        self.assertEqual(TOOL_BACKGROUND, "rgba(0, 170, 90, 0.22)")
        self.assertIn(
            f".tool-message {{ color: ansi_bright_black; padding-left: 2; "
            f"background: {TOOL_BACKGROUND}; }}",
            OmniCrawlApp.CSS,
        )
        self.assertIn(REASONING_BACKGROUND, app_backgrounds)
        self.assertIn(REASONING_FOCUS_BACKGROUND, app_backgrounds)
        self.assertEqual(
            app_backgrounds
            - {
                USER_BACKGROUND,
                TOOL_BACKGROUND,
                TOOL_FOCUS_BACKGROUND,
                REASONING_BACKGROUND,
                REASONING_FOCUS_BACKGROUND,
                "ansi_white",
            },
            {"transparent"},
        )
        for css in (
            ModelPickerScreen.CSS,
            SettingsScreen.CSS,
            ConfirmationScreen.CSS,
        ):
            self.assertNotIn("$terminal-", css)
            backgrounds = re.findall(r"(?<!-)background:\s*([^;]+);", css)
            self.assertGreater(len(backgrounds), 0)
            self.assertEqual(set(backgrounds), {"transparent"})

    async def test_escape_cancels_active_turn(self) -> None:
        """Esc 应作为全屏工作台的默认任务取消键。"""

        from omnicrawl.ui.fullscreen import FullscreenStartup, OmniCrawlApp

        class FakeAgent:
            current_model = "demo-model"
            current_session_id = "session-demo"
            skill_manager = None

            def set_confirm_handler(self, _handler) -> None:
                pass

        app = OmniCrawlApp(
            FakeAgent(),
            FullscreenStartup(True, "max", "人工确认", "D:/workspace", ".agent_tmp"),
        )

        async with app.run_test(size=(100, 32)) as pilot:
            app.is_generating = True
            await pilot.press("escape")
            await pilot.pause()

            self.assertTrue(app._cancel_requested.is_set())

    async def test_should_keep_composer_focused_when_historical_reply_is_clicked(self) -> None:
        """点击历史回复只用于选择文本，不能夺走固定输入框焦点。"""

        from textual.widgets import TextArea

        from omnicrawl.ui.fullscreen import FullscreenStartup, OmniCrawlApp

        class FakeAgent:
            current_model = "demo-model"
            current_session_id = "session-demo"
            skill_manager = None

            def set_confirm_handler(self, _handler) -> None:
                pass

        app = OmniCrawlApp(
            FakeAgent(),
            FullscreenStartup(True, "max", "人工确认", "D:/workspace", ".agent_tmp"),
        )

        async with app.run_test(size=(100, 32)) as pilot:
            app._append_delta("历史 AI 回复")
            app._render_stream_markdown()
            await pilot.pause(0.2)
            reply = app._stream_message
            assert reply is not None

            await pilot.click(reply, offset=(2, 0))
            await pilot.pause()

            self.assertTrue(app.query_one("#composer", TextArea).has_focus)

    async def test_should_hide_horizontal_scrollbar_when_assistant_message_wraps(self) -> None:
        """AI 回复由 RichLog 渲染时只能换行，不能在消息底部生成横向滚动条。"""

        from omnicrawl.ui.fullscreen import FullscreenStartup, OmniCrawlApp

        class FakeAgent:
            current_model = "demo-model"
            current_session_id = "session-demo"
            skill_manager = None

            def set_confirm_handler(self, _handler) -> None:
                pass

        app = OmniCrawlApp(
            FakeAgent(),
            FullscreenStartup(True, "max", "人工确认", "D:/workspace", ".agent_tmp"),
        )

        async with app.run_test(size=(100, 32)) as pilot:
            app._append_delta(
                "执行清单：\n\n"
                "1. 读取项目 TUI 设计文档。\n"
                "2. 定位 TUI 代码、相关命令和会话入口。\n"
                "3. 阅读关键实现及必要的测试和配置。\n"
                "4. 汇总当前理解与可能的关键入口，然后等待。"
            )
            app._render_stream_markdown()
            await pilot.pause(0.2)

            reply = app._stream_message
            assert reply is not None
            self.assertEqual(reply.styles.overflow_x, "hidden")
            self.assertFalse(reply.show_horizontal_scrollbar)

    async def _wait_until(
        self,
        pilot,
        predicate,
        *,
        attempts: int = 40,
        interval: float = 0.05,
    ) -> bool:
        """轮询等待异步状态（Textual 消息/动画以事件循环节奏生效）。"""

        for _ in range(attempts):
            if predicate():
                return True
            await pilot.pause(interval)
        return predicate()

    async def test_should_drag_conversation_scrollbar_from_track_by_mouse(self) -> None:
        """按住滚动条任意位置（含轨道）都能拖动：按下即抓取并跳到对应位置。"""

        from textual import events
        from textual.scrollbar import ScrollBar

        from omnicrawl.ui.fullscreen import FullscreenStartup, OmniCrawlApp

        class FakeAgent:
            current_model = "demo-model"
            current_session_id = "session-demo"
            skill_manager = None

            def set_confirm_handler(self, _handler) -> None:
                pass

        app = OmniCrawlApp(
            FakeAgent(),
            FullscreenStartup(True, "max", "人工确认", "D:/workspace", ".agent_tmp"),
        )

        async with app.run_test(size=(100, 32)) as pilot:
            for i in range(40):
                app._append_delta(f"第 {i} 行内容足够长让消息区溢出显示滚动条" * 2 + "\n")
            app._render_stream_markdown()
            await pilot.pause(0.3)
            conversation = app.query_one("#conversation")
            scrollbar = conversation.vertical_scrollbar
            self.assertIsInstance(scrollbar, ScrollBar)
            self.assertGreater(conversation.max_scroll_y, 0)
            x = scrollbar.region.x + scrollbar.region.width // 2
            # 内容在顶部时，thumb 只占滚动条上部；点轨道下部 y=28（底部附近）。
            track_y = scrollbar.region.bottom - 2
            app.post_message(
                events.MouseDown(
                    None, x, track_y, 0, 0, 1, False, False, False,
                    screen_x=x, screen_y=track_y,
                )
            )
            await self._wait_until(
                pilot, lambda: scrollbar.grabbed is not None,
                attempts=60,
            )
            self.assertIsNotNone(
                scrollbar.grabbed,
                "按住轨道必须进入拖动状态（Textual 原生只抓 thumb）",
            )
            await self._wait_until(pilot, lambda: conversation.scroll_y > 0)
            jumped = conversation.scroll_y
            self.assertGreater(jumped, 0, "按住轨道应先跳到点击位置对应的比例")
            # 按住向上拖动两行：位置应随拖动回退。
            move_y = track_y - 2
            self.assertIsNotNone(
                scrollbar.grabbed,
                "拖动开始前 grab 必须保持（看门狗/恢复逻辑不得打断）："
                f"grabbed={scrollbar.grabbed} captured="
                f"{type(app.mouse_captured).__name__ if app.mouse_captured else None}",
            )
            app.post_message(
                events.MouseMove(
                    None, x, move_y, 0, move_y - track_y, 1, False, False, False,
                    screen_x=x, screen_y=move_y,
                )
            )
            await self._wait_until(
                pilot,
                lambda: conversation.scroll_y < jumped - 1,
                attempts=60,
            )
            app.post_message(
                events.MouseUp(
                    None, x, move_y, 0, move_y - track_y, 1, False, False, False,
                    screen_x=x, screen_y=move_y,
                )
            )
            await self._wait_until(pilot, lambda: scrollbar.grabbed is None)
            self.assertIsNone(scrollbar.grabbed, "MouseUp 后必须释放拖动")
            self.assertLess(
                conversation.scroll_y,
                jumped,
                "向上拖动后位置应回退到跳转点之前："
                f"scroll_y={conversation.scroll_y} jumped={jumped} "
                f"grabbed={scrollbar.grabbed} position={scrollbar.position} "
                f"captured={type(app.mouse_captured).__name__ if app.mouse_captured else None} "
                f"max={conversation.max_scroll_y}",
            )

    async def test_should_not_capture_scrollbar_when_secondary_button_is_pressed(self) -> None:
        """右键或中键点击消息区滚动条不能启动左键式拖动。"""

        from textual import events

        from omnicrawl.ui.fullscreen import FullscreenStartup, OmniCrawlApp

        class FakeAgent:
            current_model = "demo-model"
            current_session_id = "session-demo"
            skill_manager = None

            def set_confirm_handler(self, _handler) -> None:
                pass

        app = OmniCrawlApp(
            FakeAgent(),
            FullscreenStartup(True, "max", "人工确认", "D:/workspace", ".agent_tmp"),
        )

        async with app.run_test(size=(100, 32)) as pilot:
            for index in range(40):
                app._append_delta(f"第 {index} 行内容足够长让消息区溢出显示滚动条" * 2 + "\n")
            app._render_stream_markdown()
            await pilot.pause(0.3)
            conversation = app.query_one("#conversation")
            scrollbar = conversation.vertical_scrollbar
            x = scrollbar.region.x + scrollbar.region.width // 2
            y = scrollbar.region.bottom - 2
            initial_scroll_y = conversation.scroll_y

            for button in (2, 3):
                app.post_message(
                    events.MouseDown(
                        None, x, y, 0, 0, button, False, False, False,
                        screen_x=x, screen_y=y,
                    )
                )
                await pilot.pause()
                self.assertIsNone(scrollbar.grabbed)
                self.assertIsNone(app.mouse_captured)
                self.assertEqual(conversation.scroll_y, initial_scroll_y)

    async def test_should_reset_mouse_state_and_reporting_when_app_focus_changes(self) -> None:
        """失焦应立即清理交互状态，重新聚焦时应恢复键盘与鼠标协议。"""

        from textual import events
        from textual.widgets import TextArea

        from omnicrawl.ui.fullscreen import FullscreenStartup, OmniCrawlApp

        class FakeAgent:
            current_model = "demo-model"
            current_session_id = "session-demo"
            skill_manager = None

            def set_confirm_handler(self, _handler) -> None:
                pass

        app = OmniCrawlApp(
            FakeAgent(),
            FullscreenStartup(True, "max", "人工确认", "D:/workspace", ".agent_tmp"),
        )

        async with app.run_test(size=(100, 32)) as pilot:
            composer = app.query_one("#composer", TextArea)
            await pilot.mouse_down(composer, offset=(2, 0))
            app._driver._down_buttons.append(1)
            await pilot.pause()
            self.assertIs(app.mouse_captured, composer)

            app.post_message(events.AppBlur())
            await pilot.pause()

            self.assertIsNone(app.mouse_captured)
            self.assertIsNone(app.screen._mouse_down_offset)
            self.assertEqual(app._driver._down_buttons, [])

            mouse_support_enabled: list[bool] = []
            app._driver._enable_mouse_support = lambda: mouse_support_enabled.append(True)
            app.post_message(events.AppFocus())
            await pilot.pause()

            self.assertEqual(mouse_support_enabled, [True])
            self.assertTrue(composer.has_focus)

    @unittest.skipUnless(sys.platform == "win32", "仅验证 Windows 回焦协议")
    def test_should_rearm_windows_mouse_protocols_when_app_regains_focus(self) -> None:
        """Windows 回焦后必须重新启用鼠标报告，不能退回终端原生选择。"""

        from textual import events

        from omnicrawl.ui.fullscreen import (
            OmniCrawlWindowsDriver,
            TerminalHandlingMixin,
        )

        protocol_calls: list[str] = []
        driver = object.__new__(OmniCrawlWindowsDriver)
        driver._down_buttons = [1]
        driver._enable_mouse_support = lambda: protocol_calls.append("mouse")
        driver._enable_bracketed_paste = lambda: protocol_calls.append("paste")
        driver.write = protocol_calls.append
        driver.flush = lambda: protocol_calls.append("flush")

        class FakeScreen:
            _mouse_down_offset = object()
            _selecting = True

            def clear_selection(self) -> None:
                pass

        class FakeComposer:
            def focus(self) -> None:
                protocol_calls.append("focus")

        class Harness(TerminalHandlingMixin):
            def __init__(self) -> None:
                self._driver = driver
                self.screen = FakeScreen()
                self.screen_stack = [self.screen]
                self._interaction_watchdog_signature = object()
                self._interaction_watchdog_stable_ticks = 1

            def capture_mouse(self, _widget) -> None:
                pass

            def query_one(self, _selector, _widget_type):
                return FakeComposer()

        with patch(
            "omnicrawl.ui.fullscreen.terminal_handling._restore_windows_raw_input_mode_if_needed",
            return_value=True,
        ) as restore_raw_mode:
            Harness().on_app_focus(events.AppFocus())

        restore_raw_mode.assert_called_once_with()
        self.assertEqual(
            protocol_calls,
            [
                "mouse",
                "\x1b[?1004h",
                OmniCrawlWindowsDriver.KEYBOARD_PROTOCOL,
                "paste",
                "flush",
                "focus",
            ],
        )

    def test_should_restore_windows_vt_input_mode_when_console_mode_is_reset(self) -> None:
        """息屏后控制台退回普通输入模式时，应恢复 Textual 所需的 VT 输入。"""

        from omnicrawl.ui.fullscreen import _restore_windows_vt_input_mode_if_needed

        input_stream = object()
        output_stream = object()

        class FakeWin32:
            ENABLE_VIRTUAL_TERMINAL_INPUT = 0x0200
            ENABLE_VIRTUAL_TERMINAL_PROCESSING = 0x0004

            def __init__(self) -> None:
                self.set_calls: list[tuple[object, int]] = []

            def get_console_mode(self, stream: object) -> int:
                return 0x01F7 if stream is input_stream else 0x0001

            def set_console_mode(self, stream: object, mode: int) -> bool:
                self.set_calls.append((stream, mode))
                return True

        win32_api = FakeWin32()

        restored = _restore_windows_vt_input_mode_if_needed(
            platform_name="win32",
            input_stream=input_stream,
            output_stream=output_stream,
            win32_api=win32_api,
        )

        self.assertTrue(restored)
        self.assertEqual(
            win32_api.set_calls,
            [
                (input_stream, FakeWin32.ENABLE_VIRTUAL_TERMINAL_INPUT),
                (
                    output_stream,
                    0x0001 | FakeWin32.ENABLE_VIRTUAL_TERMINAL_PROCESSING,
                ),
            ],
        )

    async def test_should_rearm_terminal_protocols_when_vt_mode_is_lost_in_modal(self) -> None:
        """设置页打开期间也必须自愈，否则方向键会失效。"""

        from textual.screen import ModalScreen

        from omnicrawl.ui.fullscreen import FullscreenStartup, OmniCrawlApp

        class FakeAgent:
            current_model = "demo-model"
            current_session_id = "session-demo"
            skill_manager = None

            def set_confirm_handler(self, _handler) -> None:
                pass

        app = OmniCrawlApp(
            FakeAgent(),
            FullscreenStartup(True, "max", "人工确认", "D:/workspace", ".agent_tmp"),
        )

        async with app.run_test(size=(100, 32)) as pilot:
            app.push_screen(ModalScreen())
            await pilot.pause()

            protocol_calls: list[str] = []
            app._driver._enable_mouse_support = lambda: protocol_calls.append("mouse")
            app._driver._enable_bracketed_paste = lambda: protocol_calls.append("paste")
            app._driver.write = lambda data: protocol_calls.append(data)
            app._driver.flush = lambda: protocol_calls.append("flush")

            with patch.object(
                type(app._driver),
                "is_headless",
                new=property(lambda _driver: False),
            ), patch(
                "omnicrawl.ui.fullscreen.terminal_handling._restore_windows_vt_input_mode_if_needed",
                return_value=True,
            ):
                app._recover_stale_mouse_interaction()

            self.assertEqual(
                protocol_calls,
                ["mouse", "\x1b[?1004h", "\x1b[>25u", "paste", "flush"],
            )

    async def test_should_keep_running_when_terminal_recovery_raises(self) -> None:
        """终端自愈的瞬时异常不能通过定时器升级为整个 TUI 的致命退出。"""

        from omnicrawl.ui.fullscreen import FullscreenStartup, OmniCrawlApp

        class FakeAgent:
            current_model = "demo-model"
            current_session_id = "session-demo"
            skill_manager = None

            def set_confirm_handler(self, _handler) -> None:
                pass

        app = OmniCrawlApp(
            FakeAgent(),
            FullscreenStartup(True, "max", "人工确认", "D:/workspace", ".agent_tmp"),
        )

        async with app.run_test(size=(100, 32)) as pilot:
            with patch.object(
                type(app._driver),
                "is_headless",
                new=property(lambda _driver: False),
            ), patch(
                "omnicrawl.ui.fullscreen.terminal_handling._restore_windows_vt_input_mode_if_needed",
                side_effect=RuntimeError("console mode unavailable"),
            ):
                app._recover_stale_mouse_interaction()

            await pilot.pause()
            self.assertFalse(app._exit)
            self.assertIsNone(app._interaction_watchdog_signature)
            self.assertEqual(app._interaction_watchdog_stable_ticks, 0)

    async def test_ctrl_c_copies_selection_or_clears_unselected_input(self) -> None:
        """Ctrl+C 有输入选区时复制，无选区时只清空输入框。"""

        from textual.widgets import TextArea

        from omnicrawl.ui.fullscreen import FullscreenStartup, OmniCrawlApp

        class FakeAgent:
            current_model = "demo-model"
            current_session_id = "session-demo"
            skill_manager = None

            def set_confirm_handler(self, _handler) -> None:
                pass

        app = OmniCrawlApp(
            FakeAgent(),
            FullscreenStartup(True, "max", "人工确认", "D:/workspace", ".agent_tmp"),
        )
        copied: list[str] = []
        app.copy_to_clipboard = copied.append  # type: ignore[method-assign]

        async with app.run_test(size=(100, 32)) as pilot:
            composer = app.query_one("#composer", TextArea)
            composer.text = "selected text"
            await pilot.press("f7", "ctrl+c")
            await pilot.pause()

            self.assertEqual(copied, ["selected text"])
            self.assertEqual(composer.text, "selected text")
            self.assertFalse(app._cancel_requested.is_set())

            composer.move_cursor((0, len(composer.text)))
            app.is_generating = True
            await pilot.press("ctrl+c")
            await pilot.pause()

            self.assertEqual(composer.text, "")
            self.assertFalse(app._cancel_requested.is_set())

    async def test_ctrl_c_copies_selected_ai_reply(self) -> None:
        """AI 回复支持鼠标选区，并由 Ctrl+C 复制选中的渲染文本。"""

        from omnicrawl.ui.fullscreen import FullscreenStartup, OmniCrawlApp

        class FakeAgent:
            current_model = "demo-model"
            current_session_id = "session-demo"
            skill_manager = None

            def set_confirm_handler(self, _handler) -> None:
                pass

        app = OmniCrawlApp(
            FakeAgent(),
            FullscreenStartup(True, "max", "人工确认", "D:/workspace", ".agent_tmp"),
        )
        copied: list[str] = []
        app.copy_to_clipboard = copied.append  # type: ignore[method-assign]

        async with app.run_test(size=(100, 32)) as pilot:
            app._append_delta("可以复制的 AI 回复")
            app._render_stream_markdown()
            await pilot.pause(0.2)
            reply = app._stream_message
            assert reply is not None

            # 必须覆盖真实的拖动选择。此前只发送 MouseDown/MouseUp，实际没有
            # 行内偏移量时会退化为整条回复选择，掩盖 RichLog 无法精确选取的问题。
            # 消息上下各有 1 行 blank 边框空行，内容首行从 y=1 开始。
            from textual import events

            await pilot.mouse_down(reply, offset=(2, 1))
            await pilot._post_mouse_events(
                [events.MouseMove],
                reply,
                offset=(10, 1),
                button=1,
            )
            await pilot.mouse_up(reply, offset=(10, 1))
            await pilot.pause()
            self.assertEqual(app.screen.get_selected_text(), " 可以复制的")
            await pilot.press("ctrl+c")
            await pilot.pause()

            self.assertEqual(copied, [" 可以复制的"])

    async def test_pending_messages_are_drained_in_fifo_order(self) -> None:
        """生成期间提交的多条消息应按 FIFO 顺序自动发送。"""

        from textual.widgets import Static, TextArea

        from omnicrawl.ui.fullscreen import FullscreenStartup, OmniCrawlApp

        first_started = threading.Event()
        release_first = threading.Event()

        class FakeAgent:
            current_model = "demo-model"
            current_session_id = "session-demo"
            skill_manager = None

            def __init__(self) -> None:
                self.calls: list[str] = []

            def set_confirm_handler(self, _handler) -> None:
                pass

            def run_stream(self, text: str, on_delta, **callbacks) -> str:
                self.calls.append(text)
                if text == "first":
                    first_started.set()
                    release_first.wait(timeout=2)
                on_delta(f"完成：{text}")
                return text

        agent = FakeAgent()
        app = OmniCrawlApp(
            agent,
            FullscreenStartup(True, "max", "人工确认", "D:/workspace", ".agent_tmp"),
        )

        async with app.run_test(size=(100, 32)) as pilot:
            composer = app.query_one("#composer", TextArea)
            composer.text = "first"
            await pilot.press("enter")
            await pilot.pause()
            self.assertTrue(first_started.wait(timeout=2))

            composer.text = "second"
            await pilot.press("enter")
            await pilot.pause()
            composer.text = "/skills"
            app._hide_command_menu()
            app._submit_composer_text()
            await pilot.pause()

            self.assertEqual(agent.calls, ["first"])
            self.assertEqual(list(app._pending_inputs), ["second", "/skills"])
            self.assertIn(
                "QUE 2", app.query_one("#status-summary", Static).content.plain
            )

            release_first.set()
            for _ in range(80):
                if agent.calls == ["first", "second"] and not app._pending_inputs:
                    break
                await pilot.pause(0.05)

            self.assertEqual(agent.calls, ["first", "second"])
            self.assertFalse(app._pending_inputs)
            self.assertIn(
                "QUE 0", app.query_one("#status-summary", Static).content.plain
            )
            self.assertIn("完成：second", app.conversation_text)

    async def test_queued_modal_command_resumes_after_screen_closes(self) -> None:
        """排队的弹窗命令应暂停后续消息，并在弹窗关闭后恢复。"""

        from textual.widgets import TextArea

        from omnicrawl.ui.fullscreen import FullscreenStartup, OmniCrawlApp
        from omnicrawl.ui.fullscreen.settings import SettingsScreen

        first_started = threading.Event()
        release_first = threading.Event()

        class FakeAgent:
            current_model = "demo-model"
            current_session_id = "session-demo"
            skill_manager = None

            def __init__(self) -> None:
                self.calls: list[str] = []

            def set_confirm_handler(self, _handler) -> None:
                pass

            def run_stream(self, text: str, on_delta, **callbacks) -> str:
                self.calls.append(text)
                if text == "first":
                    first_started.set()
                    release_first.wait(timeout=1)
                on_delta(f"完成：{text}")
                return text

        agent = FakeAgent()
        app = OmniCrawlApp(
            agent,
            FullscreenStartup(True, "max", "人工确认", "D:/workspace", ".agent_tmp"),
        )

        async with app.run_test(size=(100, 32)) as pilot:
            composer = app.query_one("#composer", TextArea)
            composer.text = "first"
            await pilot.press("enter")
            await pilot.pause()
            self.assertTrue(first_started.wait(timeout=1))

            composer.text = "/settings"
            app._hide_command_menu()
            app._submit_composer_text()
            composer.text = "after settings"
            app._submit_composer_text()
            release_first.set()

            for _ in range(60):
                if isinstance(app.screen, SettingsScreen):
                    break
                await pilot.pause(0.05)

            self.assertIsInstance(app.screen, SettingsScreen)
            self.assertEqual(agent.calls, ["first"])
            self.assertEqual(list(app._pending_inputs), ["after settings"])

            await pilot.press("escape")
            for _ in range(60):
                if agent.calls == ["first", "after settings"] and not app.is_generating:
                    break
                await pilot.pause(0.05)

            self.assertEqual(agent.calls, ["first", "after settings"])
            self.assertFalse(app._pending_inputs)

    async def test_cancelled_turn_drains_pending_messages(self) -> None:
        """取消当前回合只影响当前消息，已排队消息仍应继续发送。"""

        from omnicrawl.ui.fullscreen import FullscreenStartup, OmniCrawlApp

        first_started = threading.Event()
        release_first = threading.Event()

        class FakeAgent:
            current_model = "demo-model"
            current_session_id = "session-demo"
            skill_manager = None

            def __init__(self) -> None:
                self.calls: list[str] = []

            def set_confirm_handler(self, _handler) -> None:
                pass

            def run_stream(self, text: str, on_delta, **callbacks) -> str:
                self.calls.append(text)
                if text == "first":
                    first_started.set()
                    release_first.wait(timeout=1)
                    callbacks["cancel_check"]()
                on_delta(f"完成：{text}")
                return text

        agent = FakeAgent()
        app = OmniCrawlApp(
            agent,
            FullscreenStartup(True, "max", "人工确认", "D:/workspace", ".agent_tmp"),
        )

        async with app.run_test(size=(100, 32)) as pilot:
            composer = app.query_one("#composer")
            composer.text = "first"
            await pilot.press("enter")
            await pilot.pause()
            self.assertTrue(first_started.wait(timeout=1))

            composer.text = "after cancel"
            await pilot.press("enter")
            await pilot.press("escape")
            release_first.set()

            for _ in range(80):
                if agent.calls == ["first", "after cancel"] and not app.is_generating:
                    break
                await pilot.pause(0.05)

            self.assertEqual(agent.calls, ["first", "after cancel"])
            self.assertFalse(app._pending_inputs)
            self.assertIn("完成：after cancel", app.conversation_text)

    async def test_confirmation_screen_and_buttons_are_transparent_at_runtime(self) -> None:
        from textual.app import App
        from textual.widgets import Button

        from omnicrawl.ui.fullscreen.theme import TERMINAL_THEME, THEME_NAME
        from omnicrawl.ui.fullscreen.widgets import ConfirmationScreen

        class ConfirmationHarness(App[None]):
            def __init__(self) -> None:
                super().__init__()
                self.register_theme(TERMINAL_THEME)
                self.theme = THEME_NAME

            def on_mount(self) -> None:
                self.push_screen(ConfirmationScreen("允许执行测试命令？"))

            def cancel_pending_turn(self) -> None:
                pass

        app = ConfirmationHarness()
        async with app.run_test(size=(100, 32)) as pilot:
            await pilot.pause()
            screen = app.screen
            self.assertEqual(screen.styles.background.a, 0)
            self.assertEqual(screen.query_one("#confirmation-dialog").styles.background.a, 0)
            approve = screen.query_one("#approve", Button)
            reject = screen.query_one("#reject", Button)
            self.assertEqual(approve.styles.background.a, 0)
            self.assertEqual(reject.styles.background.a, 0)

            approve.focus()
            await pilot.pause()
            self.assertEqual(approve.styles.background.a, 0)
            self.assertTrue(await pilot.hover("#approve", offset=(2, 1)))
            self.assertEqual(approve.styles.background.a, 0)

    async def test_tool_result_ui_keeps_raw_result_for_session_artifact_recovery(self) -> None:
        from omnicrawl.ui.fullscreen import FullscreenStartup, OmniCrawlApp

        class FakeAgent:
            current_model = "demo-model"
            current_session_id = "session-demo"
            skill_manager = None
            config = SimpleNamespace(max_tool_output_chars=10)

            def set_confirm_handler(self, _handler) -> None:
                pass

        app = OmniCrawlApp(
            FakeAgent(),
            FullscreenStartup(True, "max", "人工确认", "D:/workspace", ".agent_tmp"),
        )
        call = SimpleNamespace(name="bash", arguments={"command": "pytest"})
        result = SimpleNamespace(ok=True, output="0123456789ABCDEFGHIJ")

        async with app.run_test(size=(100, 20)) as pilot:
            app._handle_tool_result(call, result)
            await pilot.pause()
            record = app.query_one(".tool-message")
            # bash 默认展开：无需点击即可看到输出内容。
            self.assertFalse(record.has_class("collapsed"))
            self.assertIn("0123456789", str(record.content))
            self.assertIn("ABCDEFGHIJ", str(record.content))

    async def test_subagent_events_update_one_safe_progress_tree_in_place(self) -> None:
        from omnicrawl.ui.fullscreen import FullscreenStartup, OmniCrawlApp
        from omnicrawl.ui.fullscreen.widgets import SubAgentProgressTree

        class FakeAgent:
            current_model = "demo-model"
            current_session_id = "session-demo"
            skill_manager = None

            def set_confirm_handler(self, _handler) -> None:
                pass

        app = OmniCrawlApp(
            FakeAgent(),
            FullscreenStartup(True, "max", "人工确认", "D:/workspace", ".agent_tmp"),
        )
        first_payload = {
            "batch_id": "batch-a1b2c3d4e5f6",
            "task_id": "task-a1b2c3d4e5f6",
            "agent_type": "explore",
            "description": "检查 Session",
            "summary": "不应展示的完整结果",
            "prompt": "不应展示的任务 prompt",
        }
        second_payload = {
            "batch_id": "batch-a1b2c3d4e5f6",
            "task_id": "task-b1b2c3d4e5f6",
            "agent_type": "verify",
            "description": "运行回归测试",
        }

        async with app.run_test(size=(100, 32)) as pilot:
            app._handle_subagent_event("subagent.task.queued", first_payload)
            app._handle_subagent_event("subagent.task.running", first_payload)
            app._handle_subagent_event("subagent.task.started", first_payload)
            app._handle_subagent_event("subagent.task.queued", second_payload)
            app._handle_subagent_event("subagent.task.completed", first_payload)
            await pilot.pause()

            trees = list(app.query(SubAgentProgressTree))
            self.assertEqual(len(trees), 1)
            rendered = trees[0].render_text().plain
            self.assertIn("◇ 并行子任务  1/2 完成", rendered)
            self.assertIn("├─ ✓ 检查 Session  explore · 完成", rendered)
            self.assertIn("└─ ○ 运行回归测试  verify · 等待中", rendered)
            self.assertEqual(rendered.count("检查 Session"), 1)
            self.assertNotIn("完整结果", rendered)
            self.assertNotIn("任务 prompt", rendered)

            app._handle_subagent_event(
                "subagent.task.waiting_approval",
                second_payload,
            )
            await pilot.pause()
            self.assertIn("verify · 等待审批", trees[0].render_text().plain)

            app._handle_subagent_event("subagent.task.failed", second_payload)
            app._handle_subagent_event("subagent.task.running", second_payload)
            await pilot.pause()
            terminal_rendered = trees[0].render_text().plain
            self.assertIn("× 运行回归测试  verify · 失败", terminal_rendered)
            self.assertNotIn("运行回归测试  verify · 运行中", terminal_rendered)

    async def test_subagent_progress_tree_formats_terminal_elapsed_time(self) -> None:
        from textual.app import App, ComposeResult

        from omnicrawl.ui.fullscreen.widgets import SubAgentProgressTree

        class ProgressTreeHarness(App[None]):
            def compose(self) -> ComposeResult:
                yield SubAgentProgressTree("batch-demo")

        app = ProgressTreeHarness()
        async with app.run_test(size=(80, 12)) as pilot:
            tree = app.query_one(SubAgentProgressTree)
            tree.update_task(
                task_id="task-demo",
                agent_type="explore",
                description="检查 Agent 变更",
                status="queued",
                now=10.0,
            )
            tree.update_task(
                task_id="task-demo",
                agent_type="explore",
                description="检查 Agent 变更",
                status="running",
                now=12.0,
            )
            tree.update_task(
                task_id="task-demo",
                agent_type="explore",
                description="检查 Agent 变更",
                status="completed",
                now=77.0,
            )
            await pilot.pause()

            rendered = tree.render_text(now=90.0).plain
            self.assertIn("◇ 子任务进度  1/1 完成", rendered)
            self.assertIn(
                "└─ ✓ 检查 Agent 变更  explore · 完成 · 01:05",
                rendered,
            )

    async def test_tool_disclosure_elapsed_refreshes_only_while_running(self) -> None:
        from textual.app import App, ComposeResult

        from omnicrawl.ui.fullscreen.widgets import ToolDisclosure
        from omnicrawl.ui.tool_labels import format_duration

        class ToolHarness(App[None]):
            def compose(self) -> ComposeResult:
                yield ToolDisclosure(
                    "read",
                    {"path": "README.md"},
                    started_at=10.0,
                )

        app = ToolHarness()
        async with app.run_test(size=(80, 12)) as pilot:
            record = app.query_one(ToolDisclosure)
            record.refresh_elapsed(now=11.25)
            self.assertEqual(record.duration_seconds, 1.25)
            self.assertIn(format_duration(1.25), record.render().plain)

            record.finish(ok=True, output="读取完成", finished_at=12.0)
            record.refresh_elapsed(now=99.0)
            self.assertEqual(record.duration_seconds, 2.0)
            self.assertIn(format_duration(2.0), record.render().plain)
            await pilot.pause()

    def test_subagent_confirmation_hides_complete_prompts(self) -> None:
        from omnicrawl.commands.slash import format_tool_confirmation

        prompt = format_tool_confirmation(
            "subagent",
            {
                "action": "run",
                "tasks": [
                    {
                        "description": "检查 Session",
                        "prompt": "完整 prompt token=should-not-leak",
                        "subagent_type": "explore",
                    }
                ],
            },
        )

        self.assertIn("任务数：1", prompt)
        self.assertIn("检查 Session", prompt)
        self.assertNotIn("完整 prompt", prompt)
        self.assertNotIn("should-not-leak", prompt)

    def test_subagent_inner_confirmation_shows_safe_task_origin(self) -> None:
        from omnicrawl.commands.slash import format_tool_confirmation

        prompt = format_tool_confirmation(
            "powershell",
            {
                "command": "git commit -m demo",
                "_subagent_origin": {
                    "task_id": "task-a1b2c3d4e5f6",
                    "agent_label": "verify",
                    "description": "运行验证",
                },
            },
        )

        self.assertIn("来源：子任务 verify（task-a1b2c3d4e5f6），任务：运行验证", prompt)
        self.assertIn("命令：git commit -m demo", prompt)

    def test_subagent_tool_disclosure_hides_complete_prompts(self) -> None:
        from omnicrawl.ui.fullscreen import OmniCrawlApp

        arguments = OmniCrawlApp._public_tool_arguments(
            SimpleNamespace(
                name="subagent",
                arguments={
                    "action": "run",
                    "tasks": [
                        {
                            "subagent_type": "explore",
                            "prompt": "完整 prompt token=should-not-leak",
                        }
                    ],
                    "max_concurrency": 2,
                },
            )
        )

        self.assertEqual(arguments["task_count"], 1)
        self.assertEqual(arguments["max_concurrency"], 2)
        self.assertNotIn("tasks", arguments)
        self.assertNotIn("should-not-leak", str(arguments))

    def test_compact_hud_value_truncates_long_fields(self) -> None:
        from omnicrawl.ui.fullscreen.hud import compact_hud_value

        self.assertEqual(compact_hud_value("short", 24), "short")
        self.assertEqual(compact_hud_value("", 8), "-")
        truncated = compact_hud_value("deepseek-very-long-model-name-flash", 16)
        self.assertLessEqual(len(truncated), 16)
        self.assertIn("…", truncated)

    async def test_should_render_settings_rows_when_panel_opens(self) -> None:
        from omnicrawl.ui.fullscreen import FullscreenStartup, OmniCrawlApp

        class FakeAgent:
            current_model = "demo-model"
            approval_mode = "manual"
            reasoning_effort = "max"
            workspace_root = "D:/workspace"
            current_session_id = "session-demo"
            skill_manager = None
            _memory_store = None
            _mcp_manager = SimpleNamespace(enabled=False)
            _plugin_manager = SimpleNamespace(enabled=False)
            config = SimpleNamespace(
                subagents=SimpleNamespace(enabled=False),
                context_compaction=SimpleNamespace(enabled=False),
            )

            def set_confirm_handler(self, _handler) -> None:
                pass

        app = OmniCrawlApp(
            FakeAgent(),
            FullscreenStartup(True, "max", "人工确认", "D:/workspace", ".agent_tmp"),
        )

        async with app.run_test(size=(100, 32)) as pilot:
            app._handle_command("/settings")
            await pilot.pause()
            self.assertEqual(app.screen.query_one("#settings-title").content, "运行设置")
            rows = list(app.screen.query(".settings-row"))
            self.assertEqual(len(rows), 16)
            self.assertTrue(
                all(str(row.content).strip() for row in rows),
                [repr(str(row.content)) for row in rows],
            )
            self.assertIn("模型：demo-model", str(rows[0].content))
            self.assertIn("模型渠道：管理", str(rows[1].content))
            self.assertIn("视觉：已关闭", str(rows[2].content))
            self.assertIn("图像生成：已关闭", str(rows[3].content))
            self.assertIn("上下文长度（K）：128K", str(rows[5].content))
            self.assertTrue(
                any("上下文压缩：已关闭" in str(row.content) for row in rows)
            )
            self.assertTrue(
                any("文件名快速索引：已关闭" in str(row.content) for row in rows)
            )
            self.assertTrue(
                any("内容关键词索引：已关闭" in str(row.content) for row in rows)
            )
            self.assertTrue(
                any("工具开关：进入" in str(row.content) for row in rows)
            )
            settings_list = app.screen.query_one("#settings-list")
            compaction_row = app.screen.query_one(
                "#settings-row-context_compaction"
            )
            self.assertLessEqual(
                compaction_row.region.y + compaction_row.region.height,
                settings_list.region.y + settings_list.region.height,
            )
            app.screen.query_one("#settings-dialog")
            await pilot.press("escape")
            await pilot.pause()
            self.assertEqual(len(app.screen.query("#settings-dialog")), 0)

    async def test_should_scroll_selected_setting_into_view_when_height_is_small(self) -> None:
        from textual.app import App, ComposeResult
        from textual.widgets import Static

        from omnicrawl.ui.fullscreen.settings import SettingsScreen

        class FakeAgent:
            current_model = "demo-model"
            approval_mode = "manual"
            reasoning_effort = "none"
            context_window_tokens = 128_000
            _memory_store = None
            _mcp_manager = SimpleNamespace(enabled=False)
            _plugin_manager = SimpleNamespace(enabled=False)
            config = SimpleNamespace(
                subagents=SimpleNamespace(enabled=False),
                context_compaction=SimpleNamespace(enabled=False),
            )

        class SettingsApp(App):
            def compose(self) -> ComposeResult:
                yield Static("probe")

            def on_mount(self) -> None:
                self.push_screen(SettingsScreen(FakeAgent()))

        app = SettingsApp()
        async with app.run_test(size=(100, 24)) as pilot:
            await pilot.pause()
            screen = app.screen
            compaction_index = screen._row_keys.index("context_compaction")
            for _ in range(compaction_index):
                await pilot.press("down")
            await pilot.pause()

            self.assertEqual(
                screen._row_keys[screen._selected],
                "context_compaction",
            )
            settings_list = screen.query_one("#settings-list")
            compaction_row = screen.query_one("#settings-row-context_compaction")
            self.assertGreaterEqual(compaction_row.region.y, settings_list.region.y)
            self.assertLessEqual(
                compaction_row.region.y + compaction_row.region.height,
                settings_list.region.y + settings_list.region.height,
            )

            await pilot.press("up")
            await pilot.pause()
            self.assertEqual(screen._row_keys[screen._selected], "subagents")

    async def test_should_open_model_picker_from_settings_model_row(self) -> None:
        from textual.app import ComposeResult
        from textual.screen import ModalScreen
        from textual.widgets import Static

        from omnicrawl.ui.fullscreen import FullscreenStartup, OmniCrawlApp

        class FakeAgent:
            current_model = "demo-model"
            approval_mode = "manual"
            reasoning_effort = "max"
            workspace_root = "D:/workspace"
            current_session_id = "session-demo"
            skill_manager = None
            _memory_store = None
            _mcp_manager = SimpleNamespace(enabled=False)
            _plugin_manager = SimpleNamespace(enabled=False)
            config = SimpleNamespace(subagents=SimpleNamespace(enabled=False))

            def set_confirm_handler(self, _handler) -> None:
                pass

        class DummyModelPicker(ModalScreen):
            def __init__(self, _agent, *, refresh_on_open: bool = False) -> None:
                super().__init__()
                self.refresh_on_open = refresh_on_open

            def compose(self) -> ComposeResult:
                yield Static("模型选择器", id="dummy-model-picker")

        app = OmniCrawlApp(
            FakeAgent(),
            FullscreenStartup(True, "max", "人工确认", "D:/workspace", ".agent_tmp"),
        )

        async with app.run_test(size=(100, 32)) as pilot:
            with patch(
                "omnicrawl.ui.fullscreen.ModelPickerScreen",
                DummyModelPicker,
            ):
                app._open_settings()
                await pilot.pause()
                app.screen.action_confirm()
                await pilot.pause()

                self.assertIsInstance(app.screen, DummyModelPicker)
                app.screen.query_one("#dummy-model-picker")

    async def test_should_focus_model_list_and_leave_search_on_arrow_key(self) -> None:
        from textual.app import App, ComposeResult
        from textual.widgets import Static

        from omnicrawl.config.model_catalog import CatalogModel
        from omnicrawl.ui.fullscreen.model_picker import ModelPickerScreen

        class TestPickerScreen(ModelPickerScreen):
            def _load_catalog(self, *, refresh: bool) -> None:
                # 使用固定目录隔离网络发现，但保留真实 on_mount 和焦点逻辑。
                del refresh

        items = [
            CatalogModel(
                source="custom",
                key=f"model-{index}",
                profile_id="profile",
                provider="openai",
                protocol="openai_chat_completions",
                model_id=f"model-{index}",
                display_name=f"Model {index}",
            )
            for index in range(2)
        ]
        agent = SimpleNamespace(current_model="model-0")

        class PickerApp(App):
            def compose(self) -> ComposeResult:
                yield Static("probe")

            def on_mount(self) -> None:
                self.push_screen(TestPickerScreen(agent))

        app = PickerApp()
        async with app.run_test(size=(100, 32)) as pilot:
            await pilot.pause()
            screen = app.screen
            screen._apply_catalog(
                {"custom": items, "detected": [], "diagnostics": [], "error": ""}
            )

            self.assertEqual(getattr(screen.focused, "id", None), "list-channels")
            await pilot.press("down")
            await pilot.pause()
            self.assertEqual(screen._index_channels, 1)
            await pilot.press("up")
            await pilot.pause()
            self.assertEqual(screen._index_channels, 0)

            await pilot.press("slash")
            await pilot.pause()
            self.assertEqual(
                getattr(screen.focused, "id", None), "model-picker-search"
            )
            await pilot.press("down")
            await pilot.pause()

            self.assertEqual(screen._index_channels, 1)
            self.assertEqual(getattr(screen.focused, "id", None), "list-channels")
            self.assertEqual(agent.current_model, "model-0")

    async def test_should_apply_model_selection_immediately(self) -> None:
        from textual.app import App, ComposeResult
        from textual.widgets import Static

        from omnicrawl.config.model_catalog import CatalogModel
        from omnicrawl.ui.fullscreen.model_picker import ModelPickerScreen

        persisted: list[str] = []

        class FakeAgent:
            current_model = "old-model"

            def set_model(self, model: str, *, persist=None) -> None:
                if persist is not None:
                    persist()
                self.current_model = model

        class TestPickerScreen(ModelPickerScreen):
            def on_mount(self) -> None:
                pass

        agent = FakeAgent()
        item = CatalogModel(
            source="detected",
            key="migrated-openai/new-model",
            profile_id="migrated-openai",
            provider="openai",
            protocol="openai_chat_completions",
            model_id="new-model",
            display_name="new-model",
        )

        class PickerApp(App):
            def compose(self) -> ComposeResult:
                yield Static("probe")

            def on_mount(self) -> None:
                self.push_screen(
                    TestPickerScreen(
                        agent,
                        persist_selection=lambda selected: persisted.append(
                            selected.model_id
                        )
                        or "已保存",
                    )
                )

        app = PickerApp()
        async with app.run_test(size=(100, 32)) as pilot:
            await pilot.pause()
            screen = app.screen
            screen._perform_switch(item)
            for _ in range(20):
                await pilot.pause(0.05)
                if agent.current_model == "migrated-openai/new-model":
                    break

            self.assertEqual(agent.current_model, "migrated-openai/new-model")
            self.assertEqual(persisted, ["new-model"])

    async def test_should_refresh_hud_when_settings_close(self) -> None:
        from omnicrawl.ui.fullscreen import FullscreenStartup, OmniCrawlApp

        class FakeAgent:
            current_model = "demo-model"
            approval_mode = "manual"
            reasoning_effort = "max"
            workspace_root = "D:/workspace"
            current_session_id = "session-demo"
            skill_manager = None
            _memory_store = None
            _mcp_manager = SimpleNamespace(enabled=False)
            _plugin_manager = SimpleNamespace(enabled=False)
            config = SimpleNamespace(subagents=SimpleNamespace(enabled=False))

            def set_confirm_handler(self, _handler) -> None:
                pass

        app = OmniCrawlApp(
            FakeAgent(),
            FullscreenStartup(True, "max", "人工确认", "D:/workspace", ".agent_tmp"),
        )

        async with app.run_test(size=(100, 32)) as pilot:
            with patch.object(app, "_refresh_context_summary") as refresh:
                app._open_settings()
                await pilot.pause()
                app.screen.dismiss(None)
                await pilot.pause()

                refresh.assert_called_once()

    async def test_should_apply_every_non_model_setting_immediately(self) -> None:
        from textual.app import App, ComposeResult
        from textual.widgets import Static

        from omnicrawl.ui.fullscreen.settings import SettingsScreen

        class FakeAgent:
            current_model = "demo-model"
            reasoning_effort = "none"
            approval_mode = "manual"
            _memory_store = None
            _mcp_manager = SimpleNamespace(enabled=False)
            _plugin_manager = SimpleNamespace(enabled=False)
            config = SimpleNamespace(
                subagents=SimpleNamespace(enabled=False),
                context_compaction=SimpleNamespace(enabled=False),
            )

            def __init__(self) -> None:
                self.feature_calls: list[tuple[str, bool]] = []

            def set_reasoning_effort(self, value: str) -> str:
                self.reasoning_effort = value
                return value

            def set_approval_mode(self, value: str) -> None:
                self.approval_mode = value

            def set_memory_enabled(self, enabled: bool) -> None:
                self.feature_calls.append(("memory", enabled))
                self._memory_store = object() if enabled else None

            def set_mcp_enabled(self, enabled: bool) -> None:
                self.feature_calls.append(("mcp", enabled))
                self._mcp_manager = SimpleNamespace(enabled=enabled)

            def set_plugin_enabled(self, enabled: bool) -> None:
                self.feature_calls.append(("plugins", enabled))
                self._plugin_manager = SimpleNamespace(enabled=enabled)

            def set_subagents_enabled(self, enabled: bool) -> None:
                self.feature_calls.append(("subagents", enabled))
                self.config.subagents.enabled = enabled

            def set_context_compaction_enabled(self, enabled: bool) -> None:
                self.feature_calls.append(("context_compaction", enabled))
                self.config.context_compaction.enabled = enabled

        agent = FakeAgent()

        class SettingsApp(App):
            def compose(self) -> ComposeResult:
                yield Static("probe")

            def on_mount(self) -> None:
                self.push_screen(SettingsScreen(agent))

        app = SettingsApp()
        with patch(
            "omnicrawl.ui.fullscreen.settings.save_reasoning_effort",
            return_value="config.yaml",
        ), patch(
            "omnicrawl.ui.fullscreen.settings.save_approval_mode",
            return_value="config.yaml",
        ), patch(
            "omnicrawl.ui.fullscreen.settings.save_feature_enabled",
            return_value="config.yaml",
        ) as save_feature:
            async with app.run_test(size=(100, 32)) as pilot:
                await pilot.pause()
                screen = app.screen
                for key, value in (
                    ("reasoning", "high"),
                    ("approval", "auto"),
                    ("memory", True),
                    ("mcp", True),
                    ("plugins", True),
                    ("subagents", True),
                    ("context_compaction", True),
                ):
                    worker = screen._apply_setting(key, value)
                    await worker.wait()

                self.assertEqual(agent.reasoning_effort, "high")
                self.assertEqual(agent.approval_mode, "auto")
                self.assertIsNotNone(agent._memory_store)
                self.assertTrue(agent._mcp_manager.enabled)
                self.assertTrue(agent._plugin_manager.enabled)
                self.assertTrue(agent.config.subagents.enabled)
                self.assertTrue(agent.config.context_compaction.enabled)

                for key in (
                    "memory",
                    "mcp",
                    "plugins",
                    "subagents",
                    "context_compaction",
                ):
                    worker = screen._apply_setting(key, False)
                    await worker.wait()

                self.assertIsNone(agent._memory_store)
                self.assertFalse(agent._mcp_manager.enabled)
                self.assertFalse(agent._plugin_manager.enabled)
                self.assertFalse(agent.config.subagents.enabled)
                self.assertFalse(agent.config.context_compaction.enabled)
                self.assertEqual(
                    agent.feature_calls,
                    [
                        ("memory", True),
                        ("mcp", True),
                        ("plugins", True),
                        ("subagents", True),
                        ("context_compaction", True),
                        ("memory", False),
                        ("mcp", False),
                        ("plugins", False),
                        ("subagents", False),
                        ("context_compaction", False),
                    ],
                )
                self.assertEqual(
                    [call.args for call in save_feature.call_args_list],
                    agent.feature_calls,
                )
                self.assertIn("已保存", screen._status)

    async def test_model_picker_tab_focuses_search_from_active_list(self) -> None:
        from textual.app import App, ComposeResult
        from textual.widgets import Static
        from omnicrawl.config.model_catalog import CatalogModel
        from omnicrawl.ui.fullscreen.model_picker import ModelPickerScreen

        class TestPickerScreen(ModelPickerScreen):
            def on_mount(self) -> None:
                pass

        class PickerApp(App):
            def compose(self) -> ComposeResult:
                yield Static("probe")

            def on_mount(self) -> None:
                self.push_screen(TestPickerScreen(SimpleNamespace()))

        app = PickerApp()
        async with app.run_test(size=(100, 32)) as pilot:
            await pilot.pause()
            screen = app.screen
            screen._apply_catalog(
                {
                    "custom": [
                        CatalogModel(
                            source="custom",
                            key="demo",
                            profile_id="profile",
                            provider="openai",
                            protocol="openai_chat_completions",
                            model_id="demo-model",
                            display_name="Demo",
                        )
                    ],
                    "detected": [],
                    "diagnostics": [],
                    "error": "",
                }
            )
            screen._focus_active_list()
            # 焦点设置异步生效，先等一帧再按 Tab，避免按键落入未就绪状态。
            await pilot.pause()
            await pilot.press("tab")
            self.assertEqual(screen.focused.id, "model-picker-search")

    async def test_channels_move_down_and_up(self) -> None:
        from textual.app import App, ComposeResult
        from textual.widgets import Static
        from omnicrawl.config.model_catalog import CatalogModel
        from omnicrawl.ui.fullscreen.model_picker import ModelPickerScreen

        class TestPickerScreen(ModelPickerScreen):
            def on_mount(self) -> None:
                pass

        class PickerApp(App):
            def compose(self) -> ComposeResult:
                yield Static("probe")

            def on_mount(self) -> None:
                self.push_screen(TestPickerScreen(SimpleNamespace()))

        app = PickerApp()
        async with app.run_test(size=(100, 32)) as pilot:
            await pilot.pause()
            screen = app.screen
            screen._apply_catalog(
                {
                    "custom": [
                        CatalogModel(
                            source="custom",
                            key=f"demo-{index}",
                            profile_id="profile",
                            provider="openai",
                            protocol="openai_chat_completions",
                            model_id=f"demo-model-{index}",
                            display_name=f"Demo {index}",
                        )
                        for index in range(3)
                    ],
                    "detected": [],
                    "diagnostics": [],
                    "error": "",
                }
            )
            screen._active_column = 0
            screen._index_channels = 0
            screen._focus_active_list()
            await pilot.press("down")
            await self._wait_until(pilot, lambda: screen._index_channels == 1)
            self.assertEqual(screen._index_channels, 1)
            await pilot.press("up")
            await self._wait_until(pilot, lambda: screen._index_channels == 0)
            self.assertEqual(screen._index_channels, 0)

    async def test_channel_selection_filters_models_and_wraps(self) -> None:
        from textual.app import App, ComposeResult
        from textual.widgets import Static
        from omnicrawl.config.model_catalog import CatalogModel
        from omnicrawl.ui.fullscreen.model_picker import (
            ModelPickerScreen,
            _ChannelChoice,
        )

        class TestPickerScreen(ModelPickerScreen):
            def on_mount(self) -> None:
                pass

        class PickerApp(App):
            def compose(self) -> ComposeResult:
                yield Static("probe")

            def on_mount(self) -> None:
                self.push_screen(TestPickerScreen(SimpleNamespace(current_model="")))

        channels = [
            _ChannelChoice(
                key="openai-main",
                name="OpenAI 主渠道",
                profile_id="openai-main",
                provider="openai",
                protocol="openai_chat_completions",
                model_id="gpt-5.2",
            ),
            _ChannelChoice(
                key="anthropic-main",
                name="Anthropic 主渠道",
                profile_id="anthropic-main",
                provider="anthropic",
                protocol="anthropic_messages",
                model_id="claude-sonnet-4-5",
            ),
        ]
        detected = [
            CatalogModel(
                source="detected",
                key="openai-main/gpt-5.2",
                profile_id="openai-main",
                provider="openai",
                protocol="openai_chat_completions",
                model_id="gpt-5.2",
                display_name="gpt-5.2",
            ),
            CatalogModel(
                source="detected",
                key="anthropic-main/claude-sonnet-4-5",
                profile_id="anthropic-main",
                provider="anthropic",
                protocol="anthropic_messages",
                model_id="claude-sonnet-4-5",
                display_name="claude-sonnet-4-5",
            ),
        ]

        app = PickerApp()
        async with app.run_test(size=(100, 32)) as pilot:
            await pilot.pause()
            screen = app.screen
            screen._apply_catalog(
                {
                    "channels": channels,
                    "custom": [],
                    "detected": detected,
                    "diagnostics": [],
                    "error": "",
                }
            )
            models = screen.query_one("#list-models", Static).content.plain
            self.assertIn("gpt-5.2", models)
            self.assertNotIn("claude-sonnet-4-5", models)

            screen._focus_active_list()
            await pilot.press("down")
            await pilot.pause()
            self.assertEqual(screen._index_channels, 1)
            models = screen.query_one("#list-models", Static).content.plain
            self.assertIn("claude-sonnet-4-5", models)
            self.assertNotIn("gpt-5.2", models)

            await pilot.press("down")
            await pilot.pause()
            self.assertEqual(screen._index_channels, 0)

    async def test_model_list_shows_only_model_names(self) -> None:
        from omnicrawl.config.model_catalog import CatalogModel
        from omnicrawl.ui.fullscreen.model_picker import ModelPickerScreen

        screen = ModelPickerScreen(SimpleNamespace())
        item = CatalogModel(
            source="custom",
            key="demo",
            profile_id="profile",
            provider="openai",
            protocol="openai_chat_completions",
            model_id="demo-model",
            display_name="Demo Model",
        )

        rendered = screen._render_column_text([item], selected=0, current="")

        self.assertIn("Demo Model", rendered.plain)
        self.assertNotIn("tools", rendered.plain)
        self.assertNotIn("Chat", rendered.plain)
        self.assertNotIn("profile", rendered.plain)

    async def test_should_render_selected_model_when_detected_list_exceeds_window(self) -> None:
        from omnicrawl.config.model_catalog import CatalogModel
        from omnicrawl.ui.fullscreen.model_picker import ModelPickerScreen

        screen = ModelPickerScreen(SimpleNamespace())
        items = [
            CatalogModel(
                source="detected",
                key=f"migrated-openai/model-{index}",
                profile_id="migrated-openai",
                provider="openai",
                protocol="openai_chat_completions",
                model_id=f"model-{index}",
                display_name=f"model-{index}",
            )
            for index in range(52)
        ]

        rendered = screen._render_column_text(items, selected=51, current="")

        self.assertIn("model-51", rendered.plain)
        self.assertIn("前面", rendered.plain)

    async def test_should_show_current_model_when_picker_opens(self) -> None:
        from textual.app import App, ComposeResult
        from textual.widgets import Static

        from omnicrawl.config.model_catalog import CatalogModel
        from omnicrawl.ui.fullscreen.model_picker import ModelPickerScreen

        items = [
            CatalogModel(
                source="detected",
                key=f"migrated-openai/model-{index}",
                profile_id="migrated-openai",
                provider="openai",
                protocol="openai_chat_completions",
                model_id=f"model-{index}",
                display_name=f"model-{index}",
            )
            for index in range(52)
        ]
        agent = SimpleNamespace(current_model="model-51")

        class TestPickerScreen(ModelPickerScreen):
            def on_mount(self) -> None:
                pass

        class PickerApp(App):
            def compose(self) -> ComposeResult:
                yield Static("probe")

            def on_mount(self) -> None:
                self.push_screen(TestPickerScreen(agent))

        app = PickerApp()
        async with app.run_test(size=(100, 32)) as pilot:
            await pilot.pause()
            screen = app.screen
            screen._apply_catalog(
                {"custom": [], "detected": items, "diagnostics": [], "error": ""}
            )
            rendered = str(screen.query_one("#list-models", Static).content)

            self.assertEqual(screen._index_models, 51)
            self.assertIn("model-51", rendered)

    async def test_should_keep_current_model_selected_when_catalog_refreshes_with_filter(self) -> None:
        """刷新目录时，当前模型应以过滤后列表的下标继续保持选中。"""

        from textual.app import App, ComposeResult
        from textual.widgets import Static

        from omnicrawl.config.model_catalog import CatalogModel
        from omnicrawl.ui.fullscreen.model_picker import ModelPickerScreen

        def item(model_id: str) -> CatalogModel:
            return CatalogModel(
                source="custom",
                key=model_id,
                profile_id="profile",
                provider="openai",
                protocol="openai_chat_completions",
                model_id=model_id,
                display_name=model_id,
            )

        # 当前模型在原始目录中的下标为 1；搜索后它成为过滤列表的第 0 项。
        # 若仍使用原始下标，会错误选中第二个匹配项。
        items = [item("ignore"), item("current-match"), item("other-match")]
        agent = SimpleNamespace(current_model="current-match")

        class TestPickerScreen(ModelPickerScreen):
            def on_mount(self) -> None:
                pass

        class PickerApp(App):
            def compose(self) -> ComposeResult:
                yield Static("probe")

            def on_mount(self) -> None:
                self.push_screen(TestPickerScreen(agent))

        app = PickerApp()
        async with app.run_test(size=(100, 32)) as pilot:
            await pilot.pause()
            screen = app.screen
            screen._query = "match"
            screen._apply_catalog(
                {"custom": items, "detected": [], "diagnostics": [], "error": ""}
            )

            selected = screen._selected_item()
            self.assertIsNotNone(selected)
            self.assertEqual(selected.model_id, "current-match")

    async def test_token_telemetry_updates_counts_and_context_progress(self) -> None:
        """Token 回调应刷新缩写统计与默认前景色的上下文进度条。"""

        from rich.text import Text
        from textual.widgets import Static

        from omnicrawl.ui.fullscreen import FullscreenStartup, OmniCrawlApp
        from omnicrawl.ui.fullscreen.theme import TEXT_MUTED

        class FakeAgent:
            current_model = "demo-model"
            context_window_tokens = 100_000
            skill_manager = None

            def set_confirm_handler(self, _handler) -> None:
                pass

        app = OmniCrawlApp(
            FakeAgent(),
            FullscreenStartup(True, "max", "人工确认", "D:/workspace", ".agent_tmp"),
        )

        async with app.run_test(size=(120, 40)) as pilot:
            app._handle_token_usage(62_500, 2_400, 50_000)
            await pilot.pause()
            telemetry = app.query_one("#token-telemetry", Static).content
            self.assertIsInstance(telemetry, Text)
            self.assertIn("↑62.5K", telemetry.plain)
            self.assertIn("↓2.4K", telemetry.plain)
            self.assertIn("†50K", telemetry.plain)
            # 上下文占用为用量/总量 + 百分比（位于实时速率之前）；
            # 缓存率 CH = 缓存命中输入 ÷ 本次总输入 = 50K / 62.5K = 80%。
            self.assertIn("62%", telemetry.plain)
            self.assertIn("62.5K/100K", telemetry.plain)
            self.assertIn("CH80%", telemetry.plain)
            self.assertIn(TEXT_MUTED, str(telemetry.spans))
            self.assertIn("default", str(telemetry.spans))
            self.assertNotIn("blue", str(telemetry.spans))
            self.assertNotIn("magenta", str(telemetry.spans))
            self.assertNotIn("green", str(telemetry.spans))
            self.assertNotIn("yellow", str(telemetry.spans))
            self.assertNotIn("red", str(telemetry.spans))

    async def test_token_rate_shown_while_streaming_and_reset_after_turn(self) -> None:
        """流式生成期间顶部遥测显示实时 t/s，回合结束后归零为 --。"""

        import time

        from textual.widgets import Static

        from omnicrawl.ui.fullscreen import FullscreenStartup, OmniCrawlApp

        class FakeAgent:
            current_model = "demo-model"
            current_session_id = "session-demo"
            skill_manager = None

            def set_confirm_handler(self, _handler) -> None:
                pass

        app = OmniCrawlApp(
            FakeAgent(),
            FullscreenStartup(True, "max", "人工确认", "D:/workspace", ".agent_tmp"),
        )

        async with app.run_test(size=(120, 40)) as pilot:
            # 手工构造 1 秒前的采样点：400 ASCII 字符 ≈ 100 token。
            # 跨度 1.0s → 速率约 100.0 t/s（同一 tick 内误差 < 1%）。
            app._generation_samples.append((time.monotonic() - 1.0, 100.0))
            app._refresh_token_rate()
            await pilot.pause()
            telemetry = app.query_one("#token-telemetry", Static)
            self.assertIn("100.0 t/s", telemetry.content.plain)
            # MCP 数量已移入第一行右段状态卡片。
            self.assertIn(
                "MCP 0", app.query_one("#status-summary", Static).content.plain
            )

            app._finish_turn()
            await pilot.pause()
            self.assertEqual(app._tokens_per_second, 0.0)
            self.assertEqual(len(app._generation_samples), 0)
            self.assertIn("-- t/s", telemetry.content.plain)

    async def test_token_rate_accumulates_from_stream_deltas(self) -> None:
        """思考与正文流增量都会计入实时生成速率；估算值按字符加权。"""

        from textual.widgets import Static

        from omnicrawl.ui.fullscreen import FullscreenStartup, OmniCrawlApp

        class FakeAgent:
            current_model = "demo-model"
            current_session_id = "session-demo"
            skill_manager = None

            def set_confirm_handler(self, _handler) -> None:
                pass

        app = OmniCrawlApp(
            FakeAgent(),
            FullscreenStartup(True, "max", "人工确认", "D:/workspace", ".agent_tmp"),
        )

        async with app.run_test(size=(120, 40)) as pilot:
            self.assertEqual(app._tokens_per_second, 0.0)
            # 思考流增量：400 ASCII ≈ 100 token；正文流增量：100 CJK ≈ 100 token。
            app._append_reasoning_delta("A" * 400)
            app._append_delta("问题" * 50)
            self.assertEqual(
                round(app._estimate_generation_tokens("A" * 400)),
                100,
            )
            self.assertEqual(
                round(app._estimate_generation_tokens("问题" * 50)),
                100,
            )
            app._refresh_token_rate()
            await pilot.pause()
            self.assertGreater(app._tokens_per_second, 0.0)
            self.assertIn(
                "t/s",
                app.query_one("#token-telemetry", Static).content.plain,
            )
            self.assertGreaterEqual(len(app._generation_samples), 2)

    async def test_context_summary_shows_full_workspace_path_in_muted_style(self) -> None:
        """顶部显示项目绝对路径（灰色），不做平台相关的 basename 解析。"""

        from omnicrawl.ui.fullscreen import FullscreenStartup, OmniCrawlApp

        class FakeAgent:
            current_model = "demo-model"
            approval_mode = "manual"
            reasoning_effort = "max"
            workspace_root = r"D:\projects\omnicrawl"
            current_session_id = "session-demo"
            skill_manager = None

            def set_confirm_handler(self, _handler) -> None:
                pass

        app = OmniCrawlApp(
            FakeAgent(),
            FullscreenStartup(True, "max", "人工确认", "fallback", ".agent_tmp"),
        )

        context = app._context_summary_text()
        # 第一行显示完整绝对路径（灰色），模型名已移入第二行。
        self.assertTrue(context.plain.startswith(r"D:\projects\omnicrawl"))
        self.assertNotIn("demo-model", context.plain)
        status = app._status_summary_text()
        # 第二行右段：⁕ 分隔后模型名起头，接 THK 与状态字段。
        self.assertTrue(status.plain.startswith("⁕ demo-model"))
        self.assertIn("THK MAX", status.plain)

    async def test_should_keep_composer_visible_when_slash_description_spans_multiple_lines(self) -> None:
        """单个多行 Skill 描述不能把输入框挤出 composer 容器。"""

        from textual.widgets import TextArea

        from omnicrawl.ui.fullscreen import FullscreenStartup, OmniCrawlApp

        class FakeSkillManager:
            def list_all(self):
                return [
                    SimpleNamespace(
                        name="multiline-skill",
                        description="第一行介绍\n第二行介绍\n第三行介绍",
                    )
                ]

        class FakeAgent:
            current_model = "demo-model"
            approval_mode = "manual"
            reasoning_effort = "max"
            workspace_root = "D:/workspace"
            current_session_id = "session-demo"
            skill_manager = FakeSkillManager()

            def set_confirm_handler(self, _handler) -> None:
                pass

        app = OmniCrawlApp(
            FakeAgent(),
            FullscreenStartup(True, "max", "人工确认", "D:/workspace", ".agent_tmp"),
        )

        async with app.run_test(size=(50, 20)) as pilot:
            composer = app.query_one("#composer", TextArea)
            composer.text = "/skill:multiline"
            menu = app.query_one("#command-menu")
            menu_is_ready = await self._wait_until(
                pilot,
                lambda: menu.display and menu.region.height == 1,
            )

            composer_wrap = app.query_one("#composer-wrap")
            self.assertTrue(menu_is_ready)
            self.assertTrue(menu.display)
            self.assertEqual(menu.region.height, 1)
            self.assertIn("第一行介绍 第二行介绍 第三行介绍", str(menu.content))
            self.assertGreater(composer.region.height, 0)
            self.assertLessEqual(
                composer.region.bottom,
                composer_wrap.content_region.bottom,
                (composer.region, menu.region, composer_wrap.region, composer_wrap.content_region),
            )

    async def test_slash_menu_filters_commands_and_completion_does_not_submit(self) -> None:
        """斜杠菜单应保留全部候选，展示窗口最多八项，Enter/Tab 只补全不执行。"""

        from textual.widgets import Static, TextArea

        from omnicrawl.ui.fullscreen import FullscreenStartup, OmniCrawlApp

        class FakeSkillManager:
            def list_all(self):
                return [
                    SimpleNamespace(name=f"skill-{index}", description=f"动态技能 {index}")
                    for index in range(10)
                ] + [SimpleNamespace(name="ui-design", description="界面设计")]

        class FakeAgent:
            current_model = "demo-model"
            approval_mode = "manual"
            reasoning_effort = "max"
            workspace_root = "D:/workspace"
            current_session_id = "session-demo"
            skill_manager = FakeSkillManager()

            def set_confirm_handler(self, _handler) -> None:
                pass

        app = OmniCrawlApp(
            FakeAgent(),
            FullscreenStartup(True, "max", "人工确认", "D:/workspace", ".agent_tmp"),
        )
        submitted: list[str] = []
        app._submit = submitted.append  # type: ignore[method-assign]

        async with app.run_test(size=(120, 40)) as pilot:
            composer = app.query_one("#composer", TextArea)
            menu = app.query_one("#command-menu", Static)
            await pilot.press("/")
            await pilot.pause()
            self.assertTrue(menu.display)
            self.assertGreater(len(app._command_matches), 8)
            self.assertIn("/new", str(menu.content))
            self.assertNotIn(
                "/models",
                [item["command"] for item in app._command_matches],
            )
            self.assertIn(
                "/skill:ui-design",
                [item["command"] for item in app._command_matches],
            )
            for _ in range(len(app._command_matches) - 1):
                await pilot.press("down")
                await pilot.pause()
            self.assertEqual(
                app._command_matches[app._command_selection]["command"],
                "/skill:ui-design",
            )
            await pilot.press("enter")
            await pilot.pause()
            self.assertEqual(composer.text, "/skill:ui-design ")
            self.assertEqual(submitted, [])
            self.assertFalse(menu.display)

            composer.text = "/skill:ui"
            await pilot.pause()
            self.assertEqual([item["command"] for item in app._command_matches], ["/skill:ui-design"])
            await pilot.press("enter")
            await pilot.pause()
            self.assertEqual(composer.text, "/skill:ui-design ")
            self.assertEqual(submitted, [])
            self.assertFalse(menu.display)

            composer.text = "/mem"
            await pilot.pause()
            await pilot.press("tab")
            await pilot.pause()
            self.assertEqual(composer.text, "/memory:clean")
            self.assertEqual(submitted, [])

    async def test_should_submit_exact_slash_command_when_menu_already_matches(self) -> None:
        """输入已完整匹配的斜杠命令时，Enter 应提交而不是反复补全。"""

        from textual.widgets import Static, TextArea

        from omnicrawl.ui.fullscreen import FullscreenStartup, OmniCrawlApp
        from omnicrawl.ui.fullscreen.settings import SettingsScreen

        class FakeAgent:
            current_model = "demo-model"
            approval_mode = "manual"
            reasoning_effort = "max"
            workspace_root = "D:/workspace"
            current_session_id = "session-demo"
            skill_manager = None
            _memory_store = None
            _mcp_manager = SimpleNamespace(enabled=False)
            _plugin_manager = SimpleNamespace(enabled=False)
            config = SimpleNamespace(
                subagents=SimpleNamespace(enabled=False),
                context_compaction=SimpleNamespace(enabled=False),
            )

            def set_confirm_handler(self, _handler) -> None:
                pass

        app = OmniCrawlApp(
            FakeAgent(),
            FullscreenStartup(True, "max", "人工确认", "D:/workspace", ".agent_tmp"),
        )

        async with app.run_test(size=(120, 40)) as pilot:
            composer = app.query_one("#composer", TextArea)
            menu = app.query_one("#command-menu", Static)
            composer.text = "/settings"
            await pilot.pause()
            self.assertTrue(menu.display)
            self.assertEqual(
                [item["command"] for item in app._command_matches],
                ["/settings"],
            )

            await pilot.press("enter")
            await pilot.pause()

            self.assertIsInstance(app.screen, SettingsScreen)
            self.assertEqual(composer.text, "")
            self.assertFalse(menu.display)
            self.assertEqual(app.screen.query_one("#settings-title").content, "运行设置")

    async def test_confirm_tool_returns_when_cancelled_while_waiting_for_approval(self) -> None:
        """审批模态框等待期间取消必须解除后台线程，不能永久停在确认页。"""

        from omnicrawl.ui.fullscreen import FullscreenStartup, OmniCrawlApp

        class FakeAgent:
            current_model = "demo-model"
            current_session_id = "session-demo"
            skill_manager = None

            def set_confirm_handler(self, _handler) -> None:
                pass

        app = OmniCrawlApp(
            FakeAgent(),
            FullscreenStartup(True, "max", "人工确认", "D:/workspace", ".agent_tmp"),
        )
        decision: list[bool] = []

        async with app.run_test(size=(120, 40)) as pilot:
            worker = threading.Thread(
                target=lambda: decision.append(app._confirm_tool("read", {"path": "README.md"})),
                daemon=True,
            )
            worker.start()
            await pilot.pause()

            self.assertTrue(app.screen_stack[-1].is_modal)
            app.is_generating = True
            await pilot.press("escape")
            await pilot.pause()
            self.assertTrue(app._cancel_requested.is_set())

            for _ in range(40):
                if not worker.is_alive():
                    break
                await pilot.pause(0.05)
            worker.join(timeout=0.1)

            self.assertFalse(worker.is_alive())
            self.assertEqual(decision, [False])
            self.assertEqual(len(app.screen_stack), 1)

    async def test_conversation_scroll_is_preserved_during_active_updates(self) -> None:
        """查看历史时，流式回复、工具和状态刷新不能抢回底部。"""

        from omnicrawl.ui.fullscreen import FullscreenStartup, OmniCrawlApp
        from textual.containers import VerticalScroll

        class FakeAgent:
            current_model = "demo-model"
            current_session_id = "session-demo"
            skill_manager = None

            def set_confirm_handler(self, _handler) -> None:
                pass

        app = OmniCrawlApp(
            FakeAgent(),
            FullscreenStartup(True, "max", "人工确认", "D:/workspace", ".agent_tmp"),
        )
        tool_call = SimpleNamespace(name="read", arguments={"path": "README.md"})
        tool_result = SimpleNamespace(ok=True, output="读取完成")

        async with app.run_test(size=(100, 20)) as pilot:
            for index in range(30):
                app._append_message("status", f"历史记录 {index}")
            await pilot.pause()
            conversation = app.query_one("#conversation", VerticalScroll)
            self.assertGreater(conversation.max_scroll_y, 0)

            conversation.scroll_to(y=0, animate=False, immediate=True)
            await pilot.pause()
            self.assertEqual(conversation.scroll_y, 0)

            app._set_runtime_status("正在思考", "working")
            app._append_reasoning_delta("分析中")
            app._append_delta("回复中")
            app._render_stream_markdown()
            app._handle_tool_start(1, tool_call)
            app._handle_tool_result(tool_call, tool_result)
            app._tick_status_indicator()
            app._append_message("tool", "后台任务仍在执行")
            await pilot.pause()
            self.assertEqual(conversation.scroll_y, 0)

            conversation.scroll_end(animate=False)
            await pilot.pause()
            self.assertTrue(conversation.is_vertical_scroll_end)
            app._append_delta("继续输出")
            app._render_stream_markdown()
            await pilot.pause()
            self.assertTrue(conversation.is_vertical_scroll_end)

    async def test_should_render_all_braille_spinner_frames_before_restarting_when_runtime_is_active(
        self,
    ) -> None:
        """活动状态应轮换 Braille 动画帧，完成后必须隐藏且不占空间。"""

        from textual.widgets import Static

        from omnicrawl.ui.fullscreen import FullscreenStartup, OmniCrawlApp

        class FakeAgent:
            current_model = "demo-model"
            current_session_id = "session-demo"
            skill_manager = None

            def set_confirm_handler(self, _handler) -> None:
                pass

        app = OmniCrawlApp(
            FakeAgent(),
            FullscreenStartup(True, "max", "人工确认", "D:/workspace", ".agent_tmp"),
        )

        async with app.run_test(size=(100, 32)) as pilot:
            expected_spinner_frames = (
                "⠋",
                "⠙",
                "⠹",
                "⠸",
                "⠼",
                "⠴",
                "⠦",
                "⠧",
                "⠇",
                "⠏",
            )
            self.assertEqual(app.STATUS_SPINNER_FRAMES, expected_spinner_frames)
            self.assertEqual(len(app.query(".runtime-status-message")), 0)

            app._set_runtime_status("正在思考", "working")
            await pilot.pause()
            status = app.query_one(".runtime-status-message", Static)
            self.assertEqual(status.parent.id, "conversation")
            self.assertFalse(status.has_class("working"))
            self.assertIn(str(status.content)[0], app.STATUS_SPINNER_FRAMES)

            # 等待 UI 首次刷新时计时器可能已经推进一帧；重新设置状态后同步断言
            # 首帧，再手动调用 tick，避免测试依赖运行环境的调度时机。
            app._set_runtime_status("正在检查", "working")
            self.assertEqual(str(status.content), "⠋ 正在检查")
            app._tick_status_indicator()
            self.assertEqual(str(status.content), "⠙ 正在检查")
            # 思考分片会重复上报同一阶段，不能把已推进的帧重置为首帧。
            app._set_runtime_status("正在检查", "working")
            self.assertEqual(str(status.content), "⠙ 正在检查")
            for spinner_frame in (*expected_spinner_frames[2:], expected_spinner_frames[0]):
                app._tick_status_indicator()
                self.assertEqual(str(status.content), f"{spinner_frame} 正在检查")

            app._append_delta("开始回复。")
            await pilot.pause()
            status = app.query_one(".runtime-status-message", Static)
            self.assertIs(list(app.query("#conversation > *"))[-1], status)
            self.assertIn("正在回复", str(status.content))

            app._set_runtime_status("等待", "waiting")
            await pilot.pause()
            status = app.query_one(".runtime-status-message", Static)
            self.assertIs(list(app.query("#conversation > *"))[-1], status)

            app._set_runtime_status("完成", "complete")
            app._tick_status_indicator()
            await pilot.pause()
            self.assertEqual(len(app.query(".runtime-status-message")), 0)

    async def test_should_keep_runtime_status_after_latest_output_when_turn_is_active(self) -> None:
        """状态应作为消息流末项跟随思考、正文和工具记录，而非固定在输入框上方。"""

        from textual.widgets import Static

        from omnicrawl.ui.fullscreen import FullscreenStartup, OmniCrawlApp

        class FakeAgent:
            current_model = "demo-model"
            current_session_id = "session-demo"
            skill_manager = None

            def set_confirm_handler(self, _handler) -> None:
                pass

        app = OmniCrawlApp(
            FakeAgent(),
            FullscreenStartup(True, "max", "人工确认", "D:/workspace", ".agent_tmp"),
        )
        tool_call = SimpleNamespace(name="read", arguments={"path": "README.md"})

        async with app.run_test(size=(100, 32)) as pilot:
            app._set_runtime_status("正在思考", "working")
            await pilot.pause()
            status = app.query_one(".runtime-status-message", Static)

            app._append_reasoning_delta("先分析问题。")
            await pilot.pause()
            records = list(app.query("#conversation > *"))
            self.assertIs(records[-1], status)
            self.assertTrue(records[-2].has_class("reasoning-message"))
            self.assertIn("正在思考", str(status.content))

            app._append_delta("你好")
            app._render_stream_markdown()
            await pilot.pause()
            records = list(app.query("#conversation > *"))
            self.assertIs(records[-1], status)
            self.assertTrue(records[-2].has_class("assistant-message"))
            self.assertIn("正在回复", str(status.content))

            app._handle_tool_start(1, tool_call)
            await pilot.pause()
            records = list(app.query("#conversation > *"))
            self.assertIs(records[-1], status)
            self.assertTrue(records[-2].has_class("tool-message"))
            self.assertIn("正在调用", str(status.content))

            app._finish_turn()
            await pilot.pause()
            self.assertEqual(len(app.query(".runtime-status-message")), 0)

    async def test_should_preload_mcp_silently_when_app_mounts(self) -> None:
        """首屏后台发现 MCP 时应锁定输入，但不显示瞬时等待状态。"""

        from omnicrawl.ui.fullscreen import FullscreenStartup, OmniCrawlApp

        preload_started = threading.Event()
        preload_release = threading.Event()

        class FakeAgent:
            current_model = "demo-model"
            current_session_id = "session-demo"
            skill_manager = None

            def set_confirm_handler(self, _handler) -> None:
                pass

            def preload_mcp_tools(self) -> None:
                preload_started.set()
                preload_release.wait(timeout=1)

        app = OmniCrawlApp(
            FakeAgent(),
            FullscreenStartup(True, "max", "人工确认", "D:/workspace", ".agent_tmp"),
        )

        async with app.run_test(size=(100, 32)) as pilot:
            try:
                self.assertTrue(preload_started.wait(timeout=1))
                self.assertTrue(app.is_generating)
                self.assertEqual(len(app.query(".runtime-status-message")), 0)
            finally:
                preload_release.set()
            await pilot.pause(0.2)
            self.assertFalse(app.is_generating)
            self.assertEqual(len(app.query(".runtime-status-message")), 0)

    async def test_agent_turn_errors_and_cancellation_release_input_for_next_submission(self) -> None:
        """取消、预期异常和未知异常结束后，输入锁都必须解除且允许下一轮提交。"""

        from textual.widgets import TextArea

        from omnicrawl.agent import AgentError
        from omnicrawl.ui.fullscreen import FullscreenStartup, OmniCrawlApp

        class FakeAgent:
            current_model = "demo-model"
            current_session_id = "session-demo"
            skill_manager = None

            def __init__(self) -> None:
                self.calls: list[str] = []

            def set_confirm_handler(self, _handler) -> None:
                pass

            def run_stream(self, text: str, on_delta, **callbacks) -> str:
                self.calls.append(text)
                if text == "取消":
                    # 轮询协议检查点，避免单次 sleep 与 UI 取消信号的竞态：
                    # 测试先观察到本轮输入进入 Agent，再按 Esc 设置取消令牌。
                    deadline = time.time() + 2.0
                    while time.time() < deadline:
                        callbacks["cancel_check"]()
                        time.sleep(0.05)
                    raise AssertionError("取消令牌未在协议检查点生效")
                if text == "预期失败":
                    raise AgentError("配置无效")
                if text == "未知失败":
                    raise RuntimeError("连接中断")
                on_delta(f"恢复：{text}")
                return text

        agent = FakeAgent()
        app = OmniCrawlApp(
            agent,
            FullscreenStartup(True, "max", "人工确认", "D:/workspace", ".agent_tmp"),
        )

        async with app.run_test(size=(120, 40)) as pilot:
            composer = app.query_one("#composer", TextArea)
            for text, expected in (
                ("取消", "当前任务已取消。"),
                ("预期失败", "Agent 请求失败：配置无效"),
                ("未知失败", "界面任务异常：连接中断"),
            ):
                composer.text = text
                await pilot.press("enter")
                # Textual 的 worker 可能尚未开始执行；先观察本轮输入已被
                # Agent 接收，避免把上一轮 `is_generating=False` 误认为本轮完成。
                for _ in range(40):
                    if text in agent.calls:
                        break
                    await pilot.pause(0.05)
                self.assertIn(text, agent.calls)
                if text == "取消":
                    # 输入提交会清除上一回合的取消令牌；等待本回合启动后再模拟
                    # 用户按下 Esc，才能验证 Agent 的协议检查点是否中断。
                    await pilot.press("escape")
                for _ in range(40):
                    if not app.is_generating:
                        break
                    await pilot.pause(0.05)
                self.assertFalse(app.is_generating)
                self.assertIn(expected, app.conversation_text)

            composer.text = "恢复"
            await pilot.press("enter")
            for _ in range(40):
                if not app.is_generating and "恢复" in agent.calls:
                    break
                await pilot.pause(0.05)

        self.assertEqual(agent.calls, ["取消", "预期失败", "未知失败", "恢复"])
        self.assertIn("恢复：恢复", app.conversation_text)
        self.assertFalse(app.is_generating)

    async def test_mcp_preload_failure_releases_input_and_renders_error(self) -> None:
        """MCP 预热失败不能永久锁住输入，且必须沿用既有错误呈现。"""

        from omnicrawl.agent import AgentError
        from omnicrawl.ui.fullscreen import FullscreenStartup, OmniCrawlApp

        class FakeAgent:
            current_model = "demo-model"
            current_session_id = "session-demo"
            skill_manager = None

            def set_confirm_handler(self, _handler) -> None:
                pass

            def preload_mcp_tools(self) -> None:
                raise AgentError("Server 启动失败")

        app = OmniCrawlApp(
            FakeAgent(),
            FullscreenStartup(True, "max", "人工确认", "D:/workspace", ".agent_tmp"),
        )

        async with app.run_test(size=(120, 40)) as pilot:
            await pilot.pause(0.2)

        self.assertIn("MCP 能力加载失败：Server 启动失败", app.conversation_text)
        self.assertFalse(app.is_generating)


    async def test_reasoning_single_line_does_not_reserve_blank_rows_during_reply(self) -> None:
        """单行思考在回复流开始后仍应保持正文和自然高度。"""

        from omnicrawl.ui.fullscreen import FullscreenStartup, OmniCrawlApp, ReasoningDisclosure

        class FakeAgent:
            current_model = "demo-model"
            current_session_id = "session-demo"
            skill_manager = None

            def set_confirm_handler(self, _handler) -> None:
                pass

        app = OmniCrawlApp(
            FakeAgent(),
            FullscreenStartup(True, "max", "人工确认", "D:/workspace", ".agent_tmp"),
        )

        async with app.run_test(size=(100, 32)) as pilot:
            app._append_reasoning_delta("仅一行思考")
            await pilot.pause(0.1)
            widget = app.query_one(ReasoningDisclosure)

            self.assertLessEqual(widget.size.height, 3)
            self.assertEqual(widget.virtual_size.height, 2)
            self.assertIn(
                "仅一行思考",
                "".join(
                    segment.text
                    for line_number in range(widget.size.height)
                    for segment in widget.render_line(line_number)
                ),
            )

            for delta in ("回", "复", "内", "容"):
                app._append_delta(delta)
                await pilot.pause()

            self.assertLessEqual(widget.size.height, 3)
            self.assertEqual(widget.virtual_size.height, 2)
            rendered = "".join(
                segment.text
                for line_number in range(widget.size.height)
                for segment in widget.render_line(line_number)
            )
            self.assertIn("思考过程", rendered)
            self.assertIn("仅一行思考", rendered)

    async def test_reasoning_burst_is_coalesced_and_complete_after_flush(self) -> None:
        """突发分片必须合并渲染（不逐分片全量重绘）；flush_tail 后内容完整。"""

        from omnicrawl.ui.fullscreen import FullscreenStartup, OmniCrawlApp, ReasoningDisclosure

        class FakeAgent:
            current_model = "demo-model"
            current_session_id = "session-demo"
            skill_manager = None

            def set_confirm_handler(self, _handler) -> None:
                pass

        app = OmniCrawlApp(
            FakeAgent(),
            FullscreenStartup(True, "max", "人工确认", "D:/workspace", ".agent_tmp"),
        )

        async with app.run_test(size=(100, 32)) as pilot:
            app._append_reasoning_delta("开头")
            widget = app.query_one(ReasoningDisclosure)
            await pilot.pause()

            render_count = 0
            original_render_tail = widget._render_tail

            def counting_render_tail() -> None:
                nonlocal render_count
                render_count += 1
                original_render_tail()

            widget._render_tail = counting_render_tail

            # 紧贴循环内的 200 个分片属于同一突发，合并刷新必须把渲染次数
            # 压到远低于分片数（前缘渲染 + 至多一个延时合并）。
            for index in range(200):
                widget.append_delta(f"片段{index}")
            self.assertLess(render_count, 10)

            widget.flush_tail()
            await pilot.pause()
            self.assertEqual(
                widget.reasoning_text,
                "开头" + "".join(f"片段{index}" for index in range(200)),
            )
            rendered = "".join(
                segment.text
                for line_number in range(widget.size.height)
                for segment in widget.render_line(line_number)
            )
            self.assertIn("片段199", rendered)

            # 折叠功能已移除：点击后内容仍保持展开可见。
            await pilot.click(".reasoning-message")
            await pilot.pause()
            still_rendered = "".join(
                segment.text
                for line_number in range(widget.size.height)
                for segment in widget.render_line(line_number)
            )
            self.assertIn("片段199", still_rendered)
            self.assertFalse(widget.has_class("collapsed"))

    async def test_assistant_message_renders_latex_math(self) -> None:
        """模型回复中的 LaTeX 公式应转换为终端可读的 Unicode 数学文本。"""

        from omnicrawl.ui.fullscreen import (
            AssistantMessage,
            FullscreenStartup,
            OmniCrawlApp,
        )

        class FakeAgent:
            current_model = "demo-model"
            current_session_id = "session-demo"
            skill_manager = None

            def set_confirm_handler(self, _handler) -> None:
                pass

        app = OmniCrawlApp(
            FakeAgent(),
            FullscreenStartup(True, "max", "人工确认", "D:/workspace", ".agent_tmp"),
        )

        async with app.run_test(size=(100, 32)) as pilot:
            app._append_message(
                "assistant",
                "勾股定理 $a^2 + b^2 = c^2$，以及块级公式："
                "$$\\frac{1}{2} + \\alpha = 1$$",
            )
            await pilot.pause()
            message = app.query_one(AssistantMessage)
            rendered = "".join(
                segment.text
                for line_number in range(message.size.height)
                for segment in message.render_line(line_number)
            )
            self.assertIn("a² + b² = c²", rendered)
            self.assertIn("1/2", rendered)
            self.assertIn("α", rendered)
            self.assertNotIn("$a^2", rendered)
            self.assertNotIn("$\\frac", rendered)

    async def test_confirmation_screen_returns_explicit_approval(self) -> None:
        """人工审批必须通过全屏模态框返回明确结果。"""

        from omnicrawl.ui.fullscreen import ConfirmationScreen, FullscreenStartup, OmniCrawlApp

        class FakeAgent:
            current_model = "demo-model"
            approval_mode = "manual"
            reasoning_effort = "max"
            workspace_root = "D:/workspace"
            current_session_id = "session-demo"
            skill_manager = None

            def set_confirm_handler(self, _handler) -> None:
                pass

        app = OmniCrawlApp(
            FakeAgent(),
            FullscreenStartup(True, "max", "人工确认", "D:/workspace", ".agent_tmp"),
        )
        approved: list[bool | None] = []

        async with app.run_test(size=(120, 40)) as pilot:
            app.push_screen(ConfirmationScreen("允许读取 README.md？"), approved.append)
            await pilot.pause()
            await pilot.click("#approve")
            await pilot.pause()

        self.assertEqual(approved, [True])

    async def test_fullscreen_app_renders_stream_events_with_merged_tool_record(self) -> None:
        """全屏界面必须将同一工具的开始与结果聚合为一条可扫描记录。"""

        from textual.widgets import Static, TextArea

        from omnicrawl.ui.fullscreen import (
            FullscreenStartup,
            OmniCrawlApp,
            ToolDisclosure,
        )

        class FakeAgent:
            current_model = "demo-model"
            approval_mode = "manual"
            reasoning_effort = "max"
            workspace_root = "D:/workspace"
            current_session_id = "session-demo"
            skill_manager = SimpleNamespace(count=2)

            def set_confirm_handler(self, _handler) -> None:
                pass

            def run_stream(self, text, on_delta, **callbacks) -> str:
                callbacks["on_reasoning_delta"]("先分析问题。")
                self.seen_statuses = ["正在思考"]
                on_delta(f"回复：{text}")
                self.seen_statuses.append("正在回复")
                tool_call = SimpleNamespace(name="read", arguments={"path": "README.md"})
                callbacks["on_protocol_wait"]()
                self.seen_statuses.append("等待")
                callbacks["on_tool_start"](1, tool_call)
                self.seen_statuses.append("正在调用")
                callbacks["on_tool_result"](
                    tool_call,
                    SimpleNamespace(ok=True, output="读取完成"),
                )
                callbacks["on_token_usage"](12, 8, 3)
                return f"回复：{text}"

        app = OmniCrawlApp(
            FakeAgent(),
            FullscreenStartup(
                thinking_enabled=True,
                reasoning_effort="max",
                approval_label="人工确认",
                workspace_label="D:/workspace",
                temp_label=".agent_tmp",
            ),
        )

        async with app.run_test(size=(120, 40)) as pilot:
            composer = app.query_one("#composer", TextArea)
            composer.text = "你好"
            await pilot.press("enter")
            await pilot.pause(0.8)

            self.assertFalse(app.is_generating)
            self.assertIn("回复：你好", app.conversation_text)
            self.assertIn("read", app.conversation_text)
            self.assertEqual(len(app.query(".tool-message")), 1)
            tool_record = app.query_one(".tool-message", Static)
            content = str(tool_record.content)
            self.assertIn("R  README.md  ✓ 成功", content)
            self.assertIn("读取完成", content)
            # 除 write_file 外：正文为原始输出，不再展示参数包装。
            self.assertNotIn("参数：", content)
            self.assertEqual(app.agent.seen_statuses, ["正在思考", "正在回复", "等待", "正在调用"])
            self.assertEqual(len(app.query(".runtime-status-message")), 0)
            assistant_record = app.query_one(".assistant-message")
            self.assertEqual(assistant_record.styles.background.a, 0)

            replace_call = SimpleNamespace(
                name="replace_text",
                arguments={"path": "README.md", "old_text": "旧", "new_text": "新"},
            )
            app._handle_tool_start(2, replace_call)
            await pilot.pause()
            replace_record = app.query(ToolDisclosure)[-1]
            self.assertTrue(replace_record.has_class("tool-message"))
            # 工具调用与输出统一半透明淡绿背景。
            self.assertEqual(replace_record.styles.background.a, 0.22)

    async def test_stream_records_preserve_model_tool_model_visual_order(self) -> None:
        """工具边界后的推理和最终回复不得写回工具之前的旧回复组件。"""

        from omnicrawl.ui.fullscreen import (
            FullscreenStartup,
            OmniCrawlApp,
            ReasoningDisclosure,
            ToolDisclosure,
        )

        class FakeAgent:
            current_model = "demo-model"
            current_session_id = "session-demo"
            skill_manager = None

            def set_confirm_handler(self, _handler) -> None:
                pass

        app = OmniCrawlApp(
            FakeAgent(),
            FullscreenStartup(True, "max", "人工确认", "D:/workspace", ".agent_tmp"),
        )
        tool_call = SimpleNamespace(name="read", arguments={"path": "README.md"})

        async with app.run_test(size=(120, 40)) as pilot:
            app._append_reasoning_delta("第一轮思考")
            app._append_delta("调用前说明")
            app._handle_tool_start(1, tool_call)
            app._handle_tool_result(tool_call, SimpleNamespace(ok=True, output="读取完成"))
            app._append_reasoning_delta("第二轮思考")
            app._append_delta("最终回复")
            app._render_stream_markdown()
            await pilot.pause()

            records = list(app.query("#conversation > *"))
            self.assertEqual(
                [
                    "reasoning" if isinstance(record, ReasoningDisclosure)
                    else "tool" if isinstance(record, ToolDisclosure)
                    else "status" if record.has_class("runtime-status-message")
                    else "assistant"
                    for record in records
                ],
                ["reasoning", "assistant", "tool", "reasoning", "assistant", "status"],
            )
            first_reply = "".join(segment.text for segment in records[1].render_line(0))
            final_reply = "".join(segment.text for segment in records[4].render_line(0))
            self.assertIn("调用前说明", first_reply)
            self.assertNotIn("最终回复", first_reply)
            self.assertIn("最终回复", final_reply)

    async def test_stream_records_preserve_order_across_multiple_tools(self) -> None:
        """连续工具调用时，每个模型 pass 都应追加到上一工具记录之后。"""

        from omnicrawl.ui.fullscreen import FullscreenStartup, OmniCrawlApp, ToolDisclosure

        class FakeAgent:
            current_model = "demo-model"
            current_session_id = "session-demo"
            skill_manager = None

            def set_confirm_handler(self, _handler) -> None:
                pass

        app = OmniCrawlApp(
            FakeAgent(),
            FullscreenStartup(True, "max", "人工确认", "D:/workspace", ".agent_tmp"),
        )

        async with app.run_test(size=(120, 40)) as pilot:
            for index, name in enumerate(("read", "grep"), start=1):
                app._append_reasoning_delta(f"思考 {index}")
                call = SimpleNamespace(name=name, arguments={})
                app._handle_tool_start(index, call)
                app._handle_tool_result(call, SimpleNamespace(ok=True, output="完成"))
            app._append_reasoning_delta("总结思考")
            app._append_delta("最终答案")
            app._render_stream_markdown()
            await pilot.pause()

            records = list(app.query("#conversation > *"))
            self.assertEqual(
                [
                    "tool" if isinstance(record, ToolDisclosure)
                    else "status" if record.has_class("runtime-status-message")
                    else "message"
                    for record in records
                ],
                ["message", "tool", "message", "tool", "message", "message", "status"],
            )
            self.assertIn("最终答案", "".join(segment.text for segment in records[-2].render_line(0)))

    async def test_context_summary_refreshes_runtime_values_after_commands(self) -> None:
        """运行时切换模型、审批和推理强度后，顶部上下文条应展示当前实际状态。"""

        from textual.widgets import Static

        from omnicrawl.ui.fullscreen import FullscreenStartup, OmniCrawlApp

        class FakeAgent:
            current_model = "demo-model"
            approval_mode = "auto"
            reasoning_effort = "low"
            workspace_root = "D:/workspace"
            current_session_id = "session-updated"
            skill_manager = None

            def set_confirm_handler(self, _handler) -> None:
                pass

        app = OmniCrawlApp(
            FakeAgent(),
            FullscreenStartup(True, "max", "人工确认", "D:/workspace", ".agent_tmp"),
        )

        async with app.run_test(size=(120, 40)) as pilot:
            app._refresh_context_summary()
            await pilot.pause()

            context = app.query_one("#context-summary", Static).content
            # 第一行仅项目名；模型与推理强度已移入第二行左段。
            self.assertNotIn("demo-model", context.plain)
            self.assertNotIn("THK LOW", context.plain)
            self.assertNotIn("完全自动批准", context.plain)
            # 模型/推理强度/审批模式都在第二行左段。
            status = app.query_one("#status-summary", Static).content
            self.assertIn("demo-model", status.plain)
            self.assertIn("THK LOW", status.plain)
            self.assertIn("APR AUTO", status.plain)

    async def test_parallel_tool_results_update_their_matching_disclosures(self) -> None:
        """并发工具即使逆序完成，也必须更新各自的调用记录。"""

        from omnicrawl.ui.fullscreen import FullscreenStartup, OmniCrawlApp, ToolDisclosure

        class FakeAgent:
            current_model = "demo-model"
            current_session_id = "session-demo"
            skill_manager = None

            def set_confirm_handler(self, _handler) -> None:
                pass

        app = OmniCrawlApp(
            FakeAgent(),
            FullscreenStartup(True, "max", "人工确认", "D:/workspace", ".agent_tmp"),
        )
        first = SimpleNamespace(name="read", arguments={}, id="call_1")
        second = SimpleNamespace(name="grep", arguments={}, id="call_2")

        async with app.run_test(size=(120, 40)) as pilot:
            app._handle_tool_start(1, first)
            app._handle_tool_start(2, second)
            app._handle_tool_result(second, SimpleNamespace(ok=False, output="search-failed"))
            app._handle_tool_result(first, SimpleNamespace(ok=True, output="read-ok"))
            await pilot.pause()

            records = list(app.query(ToolDisclosure))
            self.assertEqual(len(records), 2)
            self.assertIn("R  (未指定文件)  ✓ 成功", str(records[0].content))
            self.assertIn("G  .  |  目标: (未指定)  ✗ 失败", str(records[1].content))
            # 鼠标交互已禁用：工具记录始终展开，正文无需点击即可见。
            self.assertIn("read-ok", str(records[0].content))
            self.assertIn("search-failed", str(records[1].content))

    async def test_tool_disclosure_title_is_visible_in_real_terminal_render(self) -> None:
        """折叠标题不能只存在于 content 属性中，必须实际绘制到终端单元格。"""

        from omnicrawl.ui.fullscreen import FullscreenStartup, OmniCrawlApp, ToolDisclosure

        class FakeAgent:
            current_model = "demo-model"
            current_session_id = "session-demo"
            skill_manager = None

            def set_confirm_handler(self, _handler) -> None:
                pass

        app = OmniCrawlApp(
            FakeAgent(),
            FullscreenStartup(True, "max", "人工确认", "D:/workspace", ".agent_tmp"),
        )
        tool_call = SimpleNamespace(name="read", arguments={"path": "README.md"})

        async with app.run_test(size=(120, 40)) as pilot:
            app._handle_tool_start(1, tool_call)
            app._handle_tool_result(tool_call, SimpleNamespace(ok=True, output="读取完成"))
            await pilot.pause()

            record = app.query_one(ToolDisclosure)
            rendered_text = "".join(segment.text for segment in record.render_line(0))
            self.assertIn("R  README.md", rendered_text)
            self.assertIn("成功", rendered_text)

            # 鼠标交互已禁用：正文始终展开渲染，无需点击。
            expanded_text = "".join(
                segment.text
                for line_number in range(record.region.height)
                for segment in record.render_line(line_number)
            )
            self.assertIn("README.md", expanded_text)
            self.assertIn("读取完成", expanded_text)

    async def test_write_file_disclosure_is_expanded_while_running_and_after_completion(self) -> None:
        """写入文件记录默认展开，调用中和完成后都显示文件内容预览。"""

        from omnicrawl.ui.fullscreen import FullscreenStartup, OmniCrawlApp, ToolDisclosure

        class FakeAgent:
            current_model = "demo-model"
            current_session_id = "session-demo"
            skill_manager = None

            def set_confirm_handler(self, _handler) -> None:
                pass

        app = OmniCrawlApp(
            FakeAgent(),
            FullscreenStartup(True, "max", "人工确认", "D:/workspace", ".agent_tmp"),
        )
        tool_call = SimpleNamespace(
            name="write_file",
            arguments={
                "path": "notes/demo.txt",
                "mode": "overwrite",
                "content": "line-one\nline-two\n",
            },
        )

        async with app.run_test(size=(120, 40)) as pilot:
            app._handle_tool_start(1, tool_call)
            record = app.query_one(ToolDisclosure)
            self.assertNotIn("collapsed", record.classes)
            self.assertIn("rewrite +2 lines", str(record.content))
            self.assertIn("line-one", str(record.content))

            app._handle_tool_result(
                tool_call,
                SimpleNamespace(ok=True, output="已写入 notes/demo.txt"),
            )
            await pilot.pause()

            self.assertNotIn("collapsed", record.classes)
            self.assertIn("line-two", str(record.content))
            self.assertIn("已写入 notes/demo.txt", str(record.content))

    async def test_replace_text_disclosure_is_expanded_while_running_and_after_completion(self) -> None:
        """替换文本记录默认展开：调用中显示标题摘要，完成后显示原始输出。"""

        from omnicrawl.ui.fullscreen import FullscreenStartup, OmniCrawlApp, ToolDisclosure

        class FakeAgent:
            current_model = "demo-model"
            current_session_id = "session-demo"
            skill_manager = None

            def set_confirm_handler(self, _handler) -> None:
                pass

        app = OmniCrawlApp(
            FakeAgent(),
            FullscreenStartup(True, "max", "人工确认", "D:/workspace", ".agent_tmp"),
        )
        tool_call = SimpleNamespace(
            name="replace_text",
            arguments={
                "path": "notes/demo.txt",
                "old_text": "line-one\nline-two\n",
                "new_text": "line-one\nline-three\n",
            },
        )

        async with app.run_test(size=(120, 40)) as pilot:
            app._handle_tool_start(1, tool_call)
            record = app.query_one(ToolDisclosure)
            self.assertNotIn("collapsed", record.classes)
            # 调用中无结果：正文为空，标题保留文件变更统计摘要。
            self.assertIn("M  notes/demo.txt", str(record.content))
            self.assertIn("+1 -1", str(record.content))
            self.assertNotIn("line-two", str(record.content))

            app._handle_tool_result(
                tool_call,
                SimpleNamespace(ok=True, output="已修改 notes/demo.txt，替换 1 处。"),
            )
            await pilot.pause()

            self.assertNotIn("collapsed", record.classes)
            # 除 write_file 外：正文为原始输出，不再展示 diff 预览。
            self.assertNotIn("line-three", str(record.content))
            self.assertIn("已修改 notes/demo.txt，替换 1 处。", str(record.content))

    async def test_tool_disclosure_stays_expanded_while_running_and_after_completion(self) -> None:
        """鼠标交互已禁用：所有工具记录执行中和完成后均保持展开、正文可见。"""

        from omnicrawl.ui.fullscreen import FullscreenStartup, OmniCrawlApp, ToolDisclosure

        class FakeAgent:
            current_model = "demo-model"
            current_session_id = "session-demo"
            skill_manager = None

            def set_confirm_handler(self, _handler) -> None:
                pass

        app = OmniCrawlApp(
            FakeAgent(),
            FullscreenStartup(True, "max", "人工确认", "D:/workspace", ".agent_tmp"),
        )
        tool_call = SimpleNamespace(name="read", arguments={"path": "README.md"})

        async with app.run_test(size=(120, 40)) as pilot:
            with patch("omnicrawl.ui.fullscreen.rendering.time.perf_counter", side_effect=[10.0, 10.126]):
                app._handle_tool_start(7, tool_call)
                record = app.query_one(ToolDisclosure)
                self.assertFalse(record.has_class("collapsed"))
                self.assertIn("R  README.md", str(record.content))

                app._handle_tool_result(tool_call, SimpleNamespace(ok=True, output="读取完成"))

            self.assertFalse(record.has_class("collapsed"))
            self.assertIn("R  README.md  ✓ 成功", str(record.content))
            self.assertIn("读取完成", str(record.content))


    async def test_fullscreen_app_appends_monitor_events_without_starting_agent_turn(self) -> None:
        """后台日志到达时应直接显示，不应触发模型新回合。"""

        from omnicrawl.ui.fullscreen import FullscreenStartup, OmniCrawlApp

        monitor_id = "monitor-demo"

        class FakeAgent:
            current_model = "demo-model"
            current_session_id = "session-demo"
            skill_manager = None

            def set_confirm_handler(self, _handler) -> None:
                pass

            def list_monitor_tasks(self):
                return [SimpleNamespace(monitor_id=monitor_id)]

            def poll_monitor_events(self, _monitor_id: str, *, cursor: int, max_events: int):
                del max_events
                events = () if cursor else (
                    SimpleNamespace(stream="stdout", text="monitor-ready", sequence=1),
                )
                return SimpleNamespace(
                    snapshot=SimpleNamespace(status="running"),
                    events=events,
                    next_cursor=1 if events else cursor,
                )

        app = OmniCrawlApp(
            FakeAgent(),
            FullscreenStartup(True, "max", "人工确认", "D:/workspace", ".agent_tmp"),
        )

        async with app.run_test(size=(120, 40)) as pilot:
            app._refresh_monitor_events()
            await pilot.pause()

            self.assertIn("monitor-ready", app.conversation_text)
            self.assertFalse(app.is_generating)

    async def test_streaming_markdown_batches_rapid_deltas(self) -> None:
        """连续分片到达时，不应逐片重建完整 Markdown。"""

        from omnicrawl.ui.fullscreen import FullscreenStartup, OmniCrawlApp

        class FakeAgent:
            current_model = "demo-model"
            current_session_id = "session-demo"
            skill_manager = None

            def set_confirm_handler(self, _handler) -> None:
                pass

        app = OmniCrawlApp(
            FakeAgent(),
            FullscreenStartup(True, "max", "人工确认", "D:/workspace", ".agent_tmp"),
        )

        async with app.run_test(size=(120, 40)) as pilot:
            with patch.object(app, "_render_stream_markdown", wraps=app._render_stream_markdown) as render:
                for _ in range(20):
                    app._append_delta("x")
                self.assertLessEqual(render.call_count, 1)
                await pilot.pause(0.2)
                self.assertEqual(render.call_count, 1)

    async def test_workspace_switch_runs_outside_event_loop_and_resets_monitor_cursors(self) -> None:
        """工作区切换涉及磁盘和进程重建，不能阻塞 Textual 主事件循环。"""

        from omnicrawl.ui.fullscreen import FullscreenStartup, OmniCrawlApp

        expected_max_events = 50

        class FakeAgent:
            current_model = "demo-model"
            current_session_id = "session-demo"
            skill_manager = None
            workspace_root = "D:/workspace"

            def __init__(self) -> None:
                self.monitor_poll_cursors: list[int] = []

            def set_confirm_handler(self, _handler) -> None:
                pass

            def list_monitor_tasks(self):
                return [SimpleNamespace(monitor_id="old-task")]

            def poll_monitor_events(self, _monitor_id: str, *, cursor: int, max_events: int):
                if max_events != expected_max_events:
                    raise AssertionError("Monitor 单次读取上限改变")
                self.monitor_poll_cursors.append(cursor)
                return SimpleNamespace(
                    snapshot=SimpleNamespace(status="running"),
                    events=(),
                    next_cursor=42,
                )

            def switch_workspace(self, workspace: str) -> None:
                time.sleep(0.25)
                self.workspace_root = workspace

        agent = FakeAgent()
        app = OmniCrawlApp(
            agent,
            FullscreenStartup(True, "max", "人工确认", "D:/workspace", ".agent_tmp"),
        )

        async with app.run_test(size=(120, 40)) as pilot:
            app._refresh_monitor_events()
            self.assertEqual(dict(app._monitor_state.cursors), {"old-task": 42})
            started = time.monotonic()
            self.assertTrue(app._handle_command("/workspace D:/next"))
            elapsed = time.monotonic() - started
            self.assertLess(elapsed, 0.1)
            self.assertEqual(dict(app._monitor_state.cursors), {})
            self.assertTrue(app._monitor_state.polling_suspended)
            self.assertTrue(app.is_generating)
            await pilot.pause(0.35)

        self.assertGreaterEqual(len(agent.monitor_poll_cursors), 1)
        self.assertTrue(all(cursor == 0 for cursor in agent.monitor_poll_cursors))
        self.assertIn("已切换工作区：D:/next", app.conversation_text)
        self.assertFalse(app.is_generating)
        self.assertFalse(app._monitor_state.polling_suspended)

    async def test_workspace_switch_failure_is_rendered_by_slow_command_worker(self) -> None:
        """工作区切换失败应解除输入锁，并在对话区显示可读错误。"""

        from omnicrawl.agent import AgentError
        from omnicrawl.ui.fullscreen import FullscreenStartup, OmniCrawlApp

        class FakeAgent:
            current_model = "demo-model"
            current_session_id = "session-demo"
            skill_manager = None
            workspace_root = "D:/workspace"

            def set_confirm_handler(self, _handler) -> None:
                pass

            def switch_workspace(self, _workspace: str) -> None:
                raise AgentError("目录不存在")

        app = OmniCrawlApp(
            FakeAgent(),
            FullscreenStartup(True, "max", "人工确认", "D:/workspace", ".agent_tmp"),
        )

        async with app.run_test(size=(120, 40)) as pilot:
            self.assertTrue(app._handle_command("/workspace D:/missing"))
            await pilot.pause(0.1)

        self.assertIn("命令执行失败：目录不存在", app.conversation_text)
        self.assertFalse(app.is_generating)

    async def test_fullscreen_uses_runtime_module_patch_for_command_dispatch(self) -> None:
        """命令分派抽离后，App 创建后的模块级 patch 仍必须被实际调用。"""

        from omnicrawl.ui.fullscreen import FullscreenStartup, OmniCrawlApp

        class FakeAgent:
            current_model = "demo-model"
            current_session_id = "session-demo"
            skill_manager = None

            def set_confirm_handler(self, _handler) -> None:
                pass

        app = OmniCrawlApp(
            FakeAgent(),
            FullscreenStartup(True, "max", "人工确认", "D:/workspace", ".agent_tmp"),
        )

        async with app.run_test(size=(120, 40)) as pilot:
            with patch(
                "omnicrawl.ui.fullscreen.handle_approval_command",
                return_value="已通过 patch 切换审批",
            ) as patched_handler:
                self.assertTrue(app._handle_command("/approval:auto"))
                await pilot.pause()

            patched_handler.assert_called_once_with(app.agent, "/approval:auto")
            self.assertIn("已通过 patch 切换审批", app.conversation_text)

    async def test_tools_expand_by_default_and_limit_body_to_five_lines(self) -> None:
        """除 write_file 外所有工具默认展开，展开正文最多五行并带折叠提示。"""

        from textual.app import App, ComposeResult

        from omnicrawl.ui.fullscreen.widgets import ToolDisclosure

        class ToolHarness(App[None]):
            def __init__(self, name: str, output: str) -> None:
                super().__init__()
                self._name = name
                self._output = output

            def compose(self) -> ComposeResult:
                yield ToolDisclosure(
                    self._name,
                    {"command": "echo test"},
                    started_at=1.0,
                )

        for name in ("bash", "powershell", "read", "grep"):
            with self.subTest(name=name):
                app = ToolHarness(name, "第一行\n第二行\n第三行\n第四行\n第五行\n第六行\n第七行")
                async with app.run_test(size=(100, 24)) as pilot:
                    record = app.query_one(ToolDisclosure)
                    # 鼠标交互已禁用：始终展开、无 collapsed class，正文可见。
                    self.assertFalse(record.has_class("collapsed"))

                    record.finish(ok=True, output=app._output, finished_at=2.0)
                    await pilot.pause()
                    rendered = record.render().plain
                    for expected in ("第一行", "第二行", "第三行", "第四行", "第五行"):
                        self.assertIn(expected, rendered)
                    self.assertNotIn("第六行", rendered)
                    self.assertNotIn("第七行", rendered)
                    self.assertIn("仅显示前五行", rendered)


    async def test_tools_with_short_output_are_not_truncated(self) -> None:
        """输出不超过五行时保持完整，不出现截断提示。"""

        from textual.app import App, ComposeResult

        from omnicrawl.ui.fullscreen.widgets import ToolDisclosure

        class ToolHarness(App[None]):
            def compose(self) -> ComposeResult:
                yield ToolDisclosure("bash", {"command": "echo test"}, started_at=1.0)

        app = ToolHarness()
        async with app.run_test(size=(100, 24)) as pilot:
            record = app.query_one(ToolDisclosure)
            record.finish(ok=True, output="只有两行\n完成", finished_at=2.0)
            await pilot.pause()
            rendered = record.render().plain
            self.assertIn("只有两行", rendered)
            self.assertIn("完成", rendered)
            self.assertNotIn("仅显示前五行", rendered)


    async def test_write_file_keeps_full_body_when_expanded(self) -> None:
        """write_file 是唯一豁免工具：默认展开且不受五行限制。"""

        from textual.app import App, ComposeResult

        from omnicrawl.ui.fullscreen.widgets import ToolDisclosure

        class ToolHarness(App[None]):
            def compose(self) -> ComposeResult:
                yield ToolDisclosure("write_file", {"path": "a.txt"}, started_at=1.0)

        app = ToolHarness()
        async with app.run_test(size=(100, 24)) as pilot:
            record = app.query_one(ToolDisclosure)
            self.assertFalse(record.has_class("collapsed"))
            record.finish(
                ok=True,
                output="第一行\n第二行\n第三行\n第四行\n第五行\n第六行\n第七行",
                finished_at=2.0,
            )
            await pilot.pause()
            rendered = record.render().plain
            self.assertIn("第六行", rendered)
            self.assertIn("第七行", rendered)
            self.assertNotIn("仅显示前五行", rendered)


    async def test_model_picker_esc_returns_to_settings_panel_without_cancel_tip(self) -> None:
        """ESC 离开模型选择器：返回设置面板（与其他选项页面一致），无取消提示。"""

        from textual.app import ComposeResult
        from textual.screen import ModalScreen
        from textual.widgets import Static

        from omnicrawl.ui.fullscreen import FullscreenStartup, OmniCrawlApp
        from omnicrawl.ui.fullscreen.settings import SettingsScreen

        class FakeAgent:
            current_model = "demo-model"
            current_session_id = "session-demo"
            skill_manager = None

            def set_confirm_handler(self, _handler) -> None:
                pass

        class DummyModelPicker(ModalScreen):
            def __init__(self, _agent, *, refresh_on_open: bool = False) -> None:
                super().__init__()
                self.refresh_on_open = refresh_on_open

            def compose(self) -> ComposeResult:
                yield Static("模型选择器", id="dummy-model-picker")

        app = OmniCrawlApp(
            FakeAgent(),
            FullscreenStartup(True, "max", "人工确认", "D:/workspace", ".agent_tmp"),
        )

        async with app.run_test(size=(100, 32)) as pilot:
            with patch(
                "omnicrawl.ui.fullscreen.ModelPickerScreen",
                DummyModelPicker,
            ):
                app._open_settings()
                await pilot.pause()
                app.screen.action_confirm()
                await pilot.pause()
                self.assertIsInstance(app.screen, DummyModelPicker)

                # ESC 取消：参考渠道/工具等选项页面，返回设置面板。
                app.screen.dismiss(None)
                await pilot.pause()
                self.assertIsInstance(app.screen, SettingsScreen, "取消后应回到设置面板")
                self.assertNotIn("已取消模型切换", app.conversation_text)


    async def test_model_picker_switch_returns_to_settings_panel_without_tip(self) -> None:
        """切换模型后：返回设置面板（与其他选项页面一致），无切换提示。"""

        from textual.app import ComposeResult
        from textual.screen import ModalScreen
        from textual.widgets import Static

        from omnicrawl.ui.fullscreen import (
            FullscreenStartup,
            ModelPickerResult,
            OmniCrawlApp,
        )
        from omnicrawl.ui.fullscreen.settings import SettingsScreen

        class FakeAgent:
            current_model = "old-model"
            current_session_id = "session-demo"
            skill_manager = None

            def set_confirm_handler(self, _handler) -> None:
                pass

        class DummyModelPicker(ModalScreen):
            def __init__(self, _agent, *, refresh_on_open: bool = False) -> None:
                super().__init__()
                self.refresh_on_open = refresh_on_open

            def compose(self) -> ComposeResult:
                yield Static("模型选择器", id="dummy-model-picker")

        app = OmniCrawlApp(
            FakeAgent(),
            FullscreenStartup(True, "max", "人工确认", "D:/workspace", ".agent_tmp"),
        )

        async with app.run_test(size=(100, 32)) as pilot:
            with patch(
                "omnicrawl.ui.fullscreen.ModelPickerScreen",
                DummyModelPicker,
            ):
                app._open_settings()
                await pilot.pause()
                app.screen.action_confirm()
                await pilot.pause()

                app.screen.dismiss(
                    ModelPickerResult(
                        model="new-model",
                        source="custom",
                        key="channel-1",
                        message="已切换为渠道 channel-1",
                    )
                )
                await pilot.pause()
                self.assertIsInstance(app.screen, SettingsScreen, "切换后应回到设置面板")
                self.assertNotIn("已切换为渠道", app.conversation_text)
                self.assertNotIn("当前模型已切换", app.conversation_text)


    async def test_model_picker_dialog_uses_white_border(self) -> None:
        """模型设置页面边框为白色，搜索框焦点左条为细线。"""

        from omnicrawl.ui.fullscreen.model_picker import ModelPickerScreen

        self.assertIn("border: solid white", ModelPickerScreen.CSS)
        self.assertIn("border-left: solid ansi_green", ModelPickerScreen.CSS)
        self.assertNotIn("border-left: thick ansi_green", ModelPickerScreen.CSS)


    async def test_command_menu_uses_thin_blue_border(self) -> None:
        """输入框输入 / 后的预选框左侧蓝条为细线。"""

        from omnicrawl.ui.fullscreen import FullscreenStartup, OmniCrawlApp

        class FakeAgent:
            current_model = "demo-model"
            current_session_id = "session-demo"
            skill_manager = None

            def set_confirm_handler(self, _handler) -> None:
                pass

        app = OmniCrawlApp(
            FakeAgent(),
            FullscreenStartup(True, "max", "人工确认", "D:/workspace", ".agent_tmp"),
        )
        # #command-menu 是唯一使用 solid ansi_blue 的样式块（细线）。
        self.assertIn("#command-menu", app.CSS)
        self.assertIn("border-left: solid ansi_blue", app.CSS)


    async def test_model_picker_loads_channels_before_models(self) -> None:
        """两阶段加载：左侧渠道先就绪并渲染，右侧模型随后补齐。

        阶段一完成回调只依赖本地渠道配置与 custom 模型；阶段二（网络
        发现）在渠道展示后才启动，失败也不能摧毁已展示的渠道。
        """

        from textual.app import App, ComposeResult
        from textual.widgets import Static

        from omnicrawl.config.channels import ChannelConfig
        from omnicrawl.config.model_catalog import CatalogModel
        from omnicrawl.ui.fullscreen.model_picker import ModelPickerScreen

        fetch_models_called: list[bool] = []

        class TestPickerScreen(ModelPickerScreen):
            def _load_catalog(self, *, refresh: bool) -> None:
                # 阻断自动两阶段，由测试手动驱动顺序。
                del refresh

            def _fetch_models(self, *, refresh: bool) -> None:
                # 模拟阶段二异步入口：记录调用，不触发真实网络发现。
                del refresh
                fetch_models_called.append(True)

        agent = SimpleNamespace(current_model="", config=SimpleNamespace(llm=None))
        channel = ChannelConfig(
            key="channel-1",
            name="渠道一",
            profile_id="profile-a",
            provider="openai",
            protocol="openai_chat_completions",
            base_url="",
            api_key="",
            model_id="",
        )
        custom = CatalogModel(
            source="custom",
            key="channel-1",
            profile_id="profile-a",
            provider="openai",
            protocol="openai_chat_completions",
            model_id="gpt-4o",
            display_name="渠道一",
        )
        detected = CatalogModel(
            source="detected",
            key="profile-a/gpt-4o",
            profile_id="profile-a",
            provider="openai",
            protocol="openai_chat_completions",
            model_id="gpt-4o",
            display_name="gpt-4o",
        )

        class PickerApp(App):
            def compose(self) -> ComposeResult:
                yield Static("probe")

            def on_mount(self) -> None:
                self.push_screen(TestPickerScreen(agent))

        app = PickerApp()
        async with app.run_test(size=(100, 32)) as pilot:
            await pilot.pause()
            screen = app.screen

            # 阶段一完成：左侧渠道已渲染，右侧模型尚未加载。
            screen._loading = True  # 模拟 _load_catalog 已进入加载态
            screen._apply_channels(
                {
                    "channels": [channel],
                    "custom": [custom],
                    "channel_error": "",
                    "error": "",
                }
            )
            await pilot.pause()
            self.assertEqual(len(screen._channels), 1)
            self.assertEqual(screen._detected, [])
            self.assertTrue(screen._loading, "阶段二进行中应保持加载态")
            self.assertIn(
                "渠道一",
                screen.query_one("#list-channels", Static).content.plain,
            )
            self.assertEqual(
                screen.query_one("#list-models", Static).content.plain,
                "（空）",
            )
            self.assertIn("正在发现可用模型", screen._status)
            self.assertEqual(fetch_models_called, [True], "渠道就绪后立即启动模型发现")

            # 阶段二完成：右侧模型渲染，渠道列保持完整。
            screen._apply_catalog(
                {"detected": [detected], "diagnostics": [], "error": ""}
            )
            await pilot.pause()
            self.assertFalse(screen._loading)
            self.assertEqual(screen._detected, [detected])
            self.assertIn(
                "gpt-4o",
                screen.query_one("#list-models", Static).content.plain,
            )
            self.assertEqual(len(screen._channels), 1)

            # 阶段二失败：保留已展示渠道，状态行说明原因。
            screen._apply_catalog({"detected": [], "diagnostics": [], "error": "超时"})
            await pilot.pause()
            self.assertFalse(screen._loading)
            self.assertEqual(len(screen._channels), 1)
            self.assertIn("模型发现失败：超时", screen._status)


    async def test_finish_turn_after_cancel_keeps_cancelled_terminal_state(self) -> None:
        """取消回合的终态不能被 finally 的通用完成逻辑覆盖为“完成”。

        worker 在异常分支后通过 finally 调用 ``_finish_turn``；若取消标志
        已置位，状态文本必须保留“已取消”，否则 UI 会误导用户认为任务
        正常完成。
        """

        from omnicrawl.ui.fullscreen import FullscreenStartup, OmniCrawlApp

        class FakeAgent:
            current_model = "demo-model"
            current_session_id = "session-demo"
            skill_manager = None

            def set_confirm_handler(self, _handler) -> None:
                pass

        app = OmniCrawlApp(
            FakeAgent(),
            FullscreenStartup(True, "max", "人工确认", "D:/workspace", ".agent_tmp"),
        )

        async with app.run_test(size=(120, 40)) as pilot:
            app._cancel_requested.set()
            app._finish_turn()
            await pilot.pause()
            self.assertEqual(app._runtime_status_text, "已取消")
            self.assertFalse(app.is_generating)

            # 未取消的普通回合仍然以“完成”收尾。
            app._cancel_requested.clear()
            app.is_generating = True
            app._finish_turn()
            await pilot.pause()
            self.assertEqual(app._runtime_status_text, "完成")


    async def test_should_scroll_conversation_with_navigation_keys_when_composer_has_focus(self) -> None:
        """输入框保持焦点时，四个导航键都应滚动消息区。"""

        from textual.widgets import TextArea
        from textual.containers import VerticalScroll

        from omnicrawl.ui.fullscreen import FullscreenStartup, OmniCrawlApp

        class FakeAgent:
            current_model = "demo-model"
            current_session_id = "session-demo"
            skill_manager = None

            def set_confirm_handler(self, _handler) -> None:
                pass

        app = OmniCrawlApp(
            FakeAgent(),
            FullscreenStartup(True, "max", "人工确认", "D:/workspace", ".agent_tmp"),
        )

        async with app.run_test(size=(100, 20)) as pilot:
            conversation = app.query_one("#conversation", VerticalScroll)
            composer = app.query_one("#composer", TextArea)
            for index in range(30):
                app._append_message("status", f"历史记录 {index}")
            await pilot.pause()
            conversation.scroll_end(animate=False)
            await pilot.pause()
            self.assertTrue(composer.has_focus)
            self.assertGreater(conversation.max_scroll_y, 0)
            bottom = conversation.max_scroll_y

            await pilot.press("up")
            await pilot.pause()
            self.assertLess(conversation.scroll_y, bottom)

            await pilot.press("down")
            await pilot.pause()
            self.assertEqual(conversation.scroll_y, bottom)

            await pilot.press("pageup")
            await pilot.pause()
            self.assertLess(conversation.scroll_y, bottom)

            await pilot.press("pagedown")
            await pilot.pause()
            self.assertEqual(conversation.scroll_y, bottom)


    async def test_should_scroll_conversation_with_mouse_wheel_at_message_area(self) -> None:
        """消息区收到真实滚轮消息时应沿指针方向滚动。"""

        from textual import events
        from textual.containers import VerticalScroll

        from omnicrawl.ui.fullscreen import FullscreenStartup, OmniCrawlApp

        class FakeAgent:
            current_model = "demo-model"
            current_session_id = "session-demo"
            skill_manager = None

            def set_confirm_handler(self, _handler) -> None:
                pass

        app = OmniCrawlApp(
            FakeAgent(),
            FullscreenStartup(True, "max", "人工确认", "D:/workspace", ".agent_tmp"),
        )

        async with app.run_test(size=(100, 20)) as pilot:
            conversation = app.query_one("#conversation", VerticalScroll)
            for index in range(30):
                app._append_message("status", f"历史记录 {index}")
            await pilot.pause()
            conversation.scroll_end(animate=False)
            await pilot.pause()
            conversation.scroll_to(y=0, animate=False, immediate=True)
            await pilot.pause()
            wheel = events.MouseScrollDown(
                None,
                conversation.region.x + 2,
                conversation.region.y + 1,
                0,
                0,
                0,
                False,
                False,
                False,
                screen_x=conversation.region.x + 2,
                screen_y=conversation.region.y + 1,
            )

            app.post_message(wheel)
            await pilot.pause()

            self.assertGreaterEqual(
                conversation.scroll_y,
                5,
                "单次滚轮刻度至少应滚动 5 行，避免长对话翻阅过慢",
            )

            position_after_down = conversation.scroll_y
            wheel_up = events.MouseScrollUp(
                None,
                conversation.region.x + 2,
                conversation.region.y + 1,
                0,
                0,
                0,
                False,
                False,
                False,
                screen_x=conversation.region.x + 2,
                screen_y=conversation.region.y + 1,
            )
            app.post_message(wheel_up)
            await pilot.pause()

            self.assertGreaterEqual(
                position_after_down - conversation.scroll_y,
                5,
                "向上滚动也应保持每刻度至少 5 行",
            )


    async def test_reasoning_click_keeps_content_expanded(self) -> None:
        """思考块点击后仍保持展开：折叠功能已移除，正文始终可见。"""

        from textual import events
        from textual.containers import VerticalScroll
        from omnicrawl.ui.fullscreen import FullscreenStartup, OmniCrawlApp
        from omnicrawl.ui.fullscreen.widgets import ReasoningDisclosure

        class FakeAgent:
            current_model = "demo-model"
            current_session_id = "session-demo"
            skill_manager = None

            def set_confirm_handler(self, _handler) -> None:
                pass

        app = OmniCrawlApp(
            FakeAgent(),
            FullscreenStartup(True, "max", "人工确认", "D:/workspace", ".agent_tmp"),
        )

        async with app.run_test(size=(100, 20)) as pilot:
            conversation = app.query_one("#conversation", VerticalScroll)
            app._append_reasoning_delta("思考内容")
            await pilot.pause()
            conversation.scroll_end(animate=False)
            await pilot.pause()
            reasoning = app.query_one(ReasoningDisclosure)
            x = reasoning.region.x + 2
            y = reasoning.region.y

            app.post_message(events.MouseDown(
                None, x, y, 0, 0, 1, False, False, False, screen_x=x, screen_y=y
            ))
            app.post_message(events.MouseUp(
                None, x, y, 0, 0, 1, False, False, False, screen_x=x, screen_y=y
            ))
            await pilot.pause()

            # 无折叠功能：点击后正文仍完整展示。
            rendered = "".join(
                segment.text
                for line_number in range(reasoning.size.height)
                for segment in reasoning.render_line(line_number)
            )
            self.assertIn("思考内容", rendered)
            self.assertEqual(reasoning.reasoning_text, "思考内容")
            self.assertFalse(reasoning.has_class("collapsed"))


    async def test_reasoning_sections_are_separate_and_always_expanded(self) -> None:
        """每次模型推理应独立成段，且始终展开（折叠功能已移除）。"""

        from omnicrawl.ui.fullscreen import FullscreenStartup, OmniCrawlApp, ReasoningDisclosure

        class FakeAgent:
            current_model = "demo-model"
            current_session_id = "session-demo"
            skill_manager = None

            def set_confirm_handler(self, _handler) -> None:
                pass

        app = OmniCrawlApp(
            FakeAgent(),
            FullscreenStartup(True, "max", "人工确认", "D:/workspace", ".agent_tmp"),
        )

        async with app.run_test(size=(100, 32)) as pilot:
            app._append_message("user", "检查并整理配置。")
            app._append_reasoning_delta("先检查配置。")
            first = app.query_one(ReasoningDisclosure)
            self.assertFalse(first.has_class("collapsed"))
            self.assertGreater(first.styles.background.a, 0)

            await pilot.pause()
            first_text = "".join(
                segment.text
                for line_number in range(first.size.height)
                for segment in first.render_line(line_number)
            )
            self.assertIn("先检查配置", first_text)
            self.assertNotIn("▾", first_text)
            self.assertNotIn("▸", first_text)

            await pilot.click(".reasoning-message")
            await pilot.pause()
            # 点击不再折叠：正文依然完整可见。
            still_text = "".join(
                segment.text
                for line_number in range(first.size.height)
                for segment in first.render_line(line_number)
            )
            self.assertIn("先检查配置", still_text)
            self.assertFalse(first.has_class("collapsed"))
            self.assertEqual(first.reasoning_text, "先检查配置。")

            app._handle_tool_start(1, SimpleNamespace(name="read", arguments={}))
            app._append_reasoning_delta("再整理结果。")
            app._append_message("assistant", "配置已经整理完成。")
            self.assertEqual(len(app.query(ReasoningDisclosure)), 2)
            second = app.query(ReasoningDisclosure)[1]
            self.assertFalse(second.has_class("collapsed"))

            await pilot.pause()
            second_text = "".join(
                segment.text
                for line_number in range(second.size.height)
                for segment in second.render_line(line_number)
            )
            self.assertIn("再整理结果", second_text)
            messages = list(app.query(".message"))
            self.assertGreaterEqual(len(messages), 5)
            self.assertEqual(messages[0].styles.margin.bottom, 1)
            self.assertTrue(all(message.styles.margin.bottom == 1 for message in messages[:-1]))
            self.assertEqual(messages[-1].styles.margin.bottom, 0)


    async def test_tool_result_large_output_limited_to_five_lines_when_expanded(self) -> None:
        """shell 工具的大输出在展开记录中只显示前五行，并带截断提示。"""

        from omnicrawl.ui.fullscreen import FullscreenStartup, OmniCrawlApp, ToolDisclosure

        class FakeAgent:
            current_model = "demo-model"
            current_session_id = "session-demo"
            skill_manager = None

            def set_confirm_handler(self, _handler) -> None:
                pass

        app = OmniCrawlApp(
            FakeAgent(),
            FullscreenStartup(True, "max", "人工确认", "D:/workspace", ".agent_tmp"),
        )
        tool_call = SimpleNamespace(name="powershell", arguments={"command": "Write-Output long"})
        result = SimpleNamespace(ok=True, output="\n".join(f"输出行{index}" for index in range(1, 9)))

        async with app.run_test(size=(120, 40)) as pilot:
            app._handle_tool_start(1, tool_call)
            app._handle_tool_result(tool_call, result)
            await pilot.pause()

            record = app.query_one(ToolDisclosure)
            # 鼠标交互已禁用：shell 工具始终展开，无需点击即可看到正文。
            self.assertFalse(record.has_class("collapsed"))
            rendered = str(record.content)
            for index in range(1, 6):
                self.assertIn(f"输出行{index}", rendered)
            self.assertNotIn("输出行6", rendered)
            self.assertNotIn("输出行7", rendered)
            self.assertNotIn("输出行8", rendered)
            self.assertIn("仅显示前五行", rendered)
