from __future__ import annotations

import threading
import time
import unittest
from types import SimpleNamespace
from unittest.mock import patch


class FullscreenTUITest(unittest.IsolatedAsyncioTestCase):
    async def test_fullscreen_layout_uses_single_line_hud_and_compact_composer(self) -> None:
        """界面应使用两行稳态 HUD、无侧栏和三行高的紧凑输入舱。"""

        from textual.widgets import Input, Static

        from omnicrawl.ui.fullscreen import FullscreenStartup, OmniCrawlApp
        from omnicrawl.ui.fullscreen.theme import (
            ACCENT_BLUE,
            TERMINAL_BACKGROUND,
            TERMINAL_FOREGROUND,
            THEME_NAME,
        )

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
            FullscreenStartup(True, "max", "人工确认", "D:/workspace", ".agent_tmp，每 24 小时自动清理"),
        )

        async with app.run_test(size=(120, 40)) as pilot:
            await pilot.pause()
            self.assertEqual(len(app.query("#sidebar")), 0)
            self.assertEqual(len(app.query("#header")), 0)
            self.assertEqual(len(app.query("#hint")), 0)
            self.assertEqual(app.query_one("#topbar").region.y, 0)
            self.assertEqual(app.query_one("#token-telemetry").region.y, 1)
            # 顶部只保留品牌与稳态上下文；运行态进入对话区且空闲时不存在。
            self.assertEqual(app.query_one("#brand").region.width, 18)
            self.assertEqual(len(app.query("#topbar > *")), 2)
            self.assertEqual(len(app.query(".runtime-status-message")), 0)
            context = app.query_one("#context-summary", Static).content
            self.assertIn("PRJ workspace", context.plain)
            self.assertNotIn("D:/workspace", context.plain)
            self.assertIn("MDL demo-model", context.plain)
            self.assertIn("THK MAX", context.plain)
            self.assertIn("APR MAN", context.plain)
            self.assertNotIn(".agent_tmp", context.plain)
            self.assertEqual(app.theme, THEME_NAME)
            self.assertTrue(app.current_theme.ansi)
            self.assertEqual(app.current_theme.background, TERMINAL_BACKGROUND)
            self.assertEqual(app.current_theme.foreground, TERMINAL_FOREGROUND)
            self.assertIn(ACCENT_BLUE, str(context.spans))
            token_widget = app.query_one("#token-telemetry", Static)
            telemetry = str(token_widget.content)
            self.assertIn("IN 0", telemetry)
            self.assertIn("CTX 0/128K", telemetry)
            # border-bottom 会占 1 行；内容区高度必须 > 0，否则终端上看不到 Token 行。
            self.assertEqual(token_widget.region.height, 2)
            self.assertGreater(token_widget.size.height, 0)
            rendered_token = "".join(segment.text for segment in token_widget.render_line(0))
            self.assertIn("IN", rendered_token)
            self.assertIn("CTX", rendered_token)
            self.assertEqual(
                token_widget.content_region.x,
                app.query_one("#context-summary").region.x,
            )
            self.assertEqual(app.query_one("#composer-wrap").region.height, 3)
            composer = app.query_one("#composer", Input)
            self.assertGreater(composer.region.height, 0)
            self.assertTrue(composer.has_focus)
            self.assertEqual(composer.styles.background.a, 0)

    def test_terminal_css_is_transparent_and_self_contained(self) -> None:
        import re

        from omnicrawl.ui.fullscreen import OmniCrawlApp
        from omnicrawl.ui.fullscreen.model_picker import ModelPickerScreen
        from omnicrawl.ui.fullscreen.settings import SettingsScreen
        from omnicrawl.ui.fullscreen.theme import TERMINAL_FOREGROUND, terminal_css
        from omnicrawl.ui.fullscreen.widgets import ConfirmationScreen

        self.assertEqual(
            terminal_css("$terminal-text-secondary|$terminal-text"),
            f"{TERMINAL_FOREGROUND}|{TERMINAL_FOREGROUND}",
        )
        for css in (
            OmniCrawlApp.CSS,
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

    async def test_ctrl_c_copies_selection_or_clears_unselected_input(self) -> None:
        """Ctrl+C 有输入选区时复制，无选区时只清空输入框。"""

        from textual.widgets import Input

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
            composer = app.query_one("#composer", Input)
            composer.value = "selected text"
            await pilot.press("ctrl+shift+a", "ctrl+c")
            await pilot.pause()

            self.assertEqual(copied, ["selected text"])
            self.assertEqual(composer.value, "selected text")
            self.assertFalse(app._cancel_requested.is_set())

            composer.cursor_position = len(composer.value)
            app.is_generating = True
            await pilot.press("ctrl+c")
            await pilot.pause()

            self.assertEqual(composer.value, "")
            self.assertFalse(app._cancel_requested.is_set())

    async def test_submissions_queue_and_run_in_fifo_order(self) -> None:
        """生成期间提交的多条消息应按 FIFO 顺序自动发送。"""

        from textual.widgets import Input

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
                on_delta(f"完成：{text}")
                return text

        agent = FakeAgent()
        app = OmniCrawlApp(
            agent,
            FullscreenStartup(True, "max", "人工确认", "D:/workspace", ".agent_tmp"),
        )

        async with app.run_test(size=(100, 32)) as pilot:
            composer = app.query_one("#composer", Input)
            composer.value = "first"
            await pilot.press("enter")
            await pilot.pause()
            self.assertTrue(first_started.wait(timeout=1))

            composer.value = "second"
            await pilot.press("enter")
            await pilot.pause()
            composer.value = "/skills"
            app._hide_command_menu()
            app.on_input_submitted(Input.Submitted(composer, "/skills"))
            await pilot.pause()

            self.assertEqual(agent.calls, ["first"])
            self.assertEqual(list(app._pending_inputs), ["second", "/skills"])

            release_first.set()
            for _ in range(80):
                if agent.calls == ["first", "second"] and not app._pending_inputs:
                    break
                await pilot.pause(0.05)

            self.assertEqual(agent.calls, ["first", "second"])
            self.assertFalse(app._pending_inputs)
            self.assertIn("完成：second", app.conversation_text)

    async def test_queued_modal_command_resumes_after_screen_closes(self) -> None:
        """排队的弹窗命令应暂停后续消息，并在弹窗关闭后恢复。"""

        from textual.widgets import Input

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
            composer = app.query_one("#composer", Input)
            composer.value = "first"
            await pilot.press("enter")
            await pilot.pause()
            self.assertTrue(first_started.wait(timeout=1))

            app._hide_command_menu()
            app.on_input_submitted(Input.Submitted(composer, "/settings"))
            app.on_input_submitted(Input.Submitted(composer, "after settings"))
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
            composer.value = "first"
            await pilot.press("enter")
            await pilot.pause()
            self.assertTrue(first_started.wait(timeout=1))

            composer.value = "after cancel"
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

    def test_subagent_events_render_only_safe_task_status(self) -> None:
        from omnicrawl.ui.fullscreen import OmniCrawlApp

        app = object.__new__(OmniCrawlApp)
        messages = []
        app._append_message = lambda role, text: messages.append((role, text))
        payload = {
            "task_id": "task-a1b2c3d4e5f6",
            "agent_type": "explore",
            "description": "检查 Session",
            "summary": "不应展示的完整结果",
            "prompt": "不应展示的任务 prompt",
        }

        app._handle_subagent_event("subagent.task.queued", payload)
        app._handle_subagent_event("subagent.task.running", payload)
        app._handle_subagent_event("subagent.task.started", payload)
        app._handle_subagent_event("subagent.task.waiting_approval", payload)
        app._handle_subagent_event("subagent.task.completed", payload)
        app._handle_subagent_event("subagent.task.failed", payload)
        app._handle_subagent_event("subagent.task.cancelled", payload)
        app._handle_subagent_event("subagent.task.approval_cancelled", payload)

        rendered = "\n".join(text for _role, text in messages)
        self.assertIn("子任务排队：explore · 检查 Session", rendered)
        self.assertIn("子任务运行中：explore · 检查 Session", rendered)
        self.assertIn("子任务等待审批：explore · 检查 Session", rendered)
        self.assertIn("子任务完成：explore · 检查 Session", rendered)
        self.assertIn("子任务失败：explore · 检查 Session", rendered)
        self.assertIn("子任务取消：explore · 检查 Session", rendered)
        self.assertNotIn("完整结果", rendered)
        self.assertNotIn("任务 prompt", rendered)

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
            config = SimpleNamespace(subagents=SimpleNamespace(enabled=False))

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
            self.assertEqual(len(rows), 8)
            self.assertTrue(
                all(str(row.content).strip() for row in rows),
                [repr(str(row.content)) for row in rows],
            )
            self.assertIn("模型：demo-model", str(rows[0].content))
            self.assertIn("上下文长度（K）：128K", str(rows[2].content))
            app.screen.query_one("#settings-dialog")
            await pilot.press("escape")
            await pilot.pause()
            self.assertEqual(len(app.screen.query("#settings-dialog")), 0)

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

            self.assertEqual(getattr(screen.focused, "id", None), "list-custom")
            await pilot.press("down")
            await pilot.pause()
            self.assertEqual(screen._index_custom, 1)
            await pilot.press("up")
            await pilot.pause()
            self.assertEqual(screen._index_custom, 0)

            await pilot.press("slash")
            await pilot.pause()
            self.assertEqual(
                getattr(screen.focused, "id", None), "model-picker-search"
            )
            await pilot.press("down")
            await pilot.pause()

            self.assertEqual(screen._index_custom, 1)
            self.assertEqual(getattr(screen.focused, "id", None), "list-custom")
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
            config = SimpleNamespace(subagents=SimpleNamespace(enabled=False))

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
                ):
                    worker = screen._apply_setting(key, value)
                    await worker.wait()

                self.assertEqual(agent.reasoning_effort, "high")
                self.assertEqual(agent.approval_mode, "auto")
                self.assertIsNotNone(agent._memory_store)
                self.assertTrue(agent._mcp_manager.enabled)
                self.assertTrue(agent._plugin_manager.enabled)
                self.assertTrue(agent.config.subagents.enabled)

                for key in ("memory", "mcp", "plugins", "subagents"):
                    worker = screen._apply_setting(key, False)
                    await worker.wait()

                self.assertIsNone(agent._memory_store)
                self.assertFalse(agent._mcp_manager.enabled)
                self.assertFalse(agent._plugin_manager.enabled)
                self.assertFalse(agent.config.subagents.enabled)
                self.assertEqual(
                    agent.feature_calls,
                    [
                        ("memory", True),
                        ("mcp", True),
                        ("plugins", True),
                        ("subagents", True),
                        ("memory", False),
                        ("mcp", False),
                        ("plugins", False),
                        ("subagents", False),
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
            await pilot.press("tab")
            self.assertEqual(screen.focused.id, "model-picker-search")

    async def test_custom_models_move_down_and_up(self) -> None:
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
            screen._index_custom = 0
            screen._focus_active_list()
            await pilot.press("down")
            self.assertEqual(screen._index_custom, 1)
            await pilot.press("up")
            self.assertEqual(screen._index_custom, 0)

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
            rendered = str(screen.query_one("#list-detected", Static).content)

            self.assertEqual(screen._index_detected, 51)
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
        """Token 回调应刷新缩写统计，并按配置上限生成彩色上下文进度条。"""

        from rich.text import Text
        from textual.widgets import Static

        from omnicrawl.ui.fullscreen import FullscreenStartup, OmniCrawlApp
        from omnicrawl.ui.fullscreen.theme import ACCENT_AMBER

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
            self.assertIn("IN 62.5K", telemetry.plain)
            self.assertIn("OUT 2.4K", telemetry.plain)
            self.assertIn("CA 50K", telemetry.plain)
            self.assertIn("CTX 62.5K/100K", telemetry.plain)
            self.assertIn("62%", telemetry.plain)
            self.assertIn(ACCENT_AMBER, str(telemetry.spans))

    async def test_context_summary_uses_windows_workspace_folder_name(self) -> None:
        """顶部项目名解析不应依赖测试进程当前运行的平台。"""

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
        self.assertTrue(context.plain.startswith("PRJ omnicrawl  ·  MDL "))
        self.assertNotIn(r"D:\projects", context.plain)

    async def test_slash_menu_filters_commands_and_completion_does_not_submit(self) -> None:
        """斜杠菜单应包含动态 Skill，最多八项，Enter/Tab 只补全不执行。"""

        from textual.widgets import Input, Static

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
            composer = app.query_one("#composer", Input)
            menu = app.query_one("#command-menu", Static)
            await pilot.press("/")
            await pilot.pause()
            self.assertTrue(menu.display)
            self.assertEqual(len(app._command_matches), 8)
            self.assertIn("/new", str(menu.content))

            composer.value = "/skill:ui"
            await pilot.pause()
            self.assertEqual([item["command"] for item in app._command_matches], ["/skill:ui-design"])
            await pilot.press("enter")
            await pilot.pause()
            self.assertEqual(composer.value, "/skill:ui-design ")
            self.assertEqual(submitted, [])
            self.assertFalse(menu.display)

            composer.value = "/mem"
            await pilot.pause()
            await pilot.press("tab")
            await pilot.pause()
            self.assertEqual(composer.value, "/memory:clean")
            self.assertEqual(submitted, [])

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
                target=lambda: decision.append(app._confirm_tool("read_file", {"path": "README.md"})),
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

    async def test_runtime_indicator_only_shows_while_active(self) -> None:
        """活动状态应闪烁显示，默认和完成状态必须隐藏且不占空间。"""

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
            self.assertEqual(len(app.query(".runtime-status-message")), 0)

            app._set_runtime_status("正在思考", "working")
            await pilot.pause()
            status = app.query_one(".runtime-status-message", Static)
            self.assertEqual(status.parent.id, "conversation")
            self.assertIn("正在思考", str(status.content))

            app._status_dot_visible = False
            app._render_status_indicator()
            self.assertEqual(str(status.content), "  正在思考")
            app._status_dot_visible = True
            app._render_status_indicator()
            self.assertEqual(str(status.content), "● 正在思考")

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
        tool_call = SimpleNamespace(name="read_file", arguments={"path": "README.md"})

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

        from textual.widgets import Input

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
                    # 留出一个可预测的协议检查点，让测试能够在回合运行期间
                    # 设置取消令牌，而非在提交前被 `_submit()` 清除。
                    time.sleep(0.25)
                    callbacks["cancel_check"]()
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
            composer = app.query_one("#composer", Input)
            for text, expected in (
                ("取消", "当前任务已取消。"),
                ("预期失败", "Agent 请求失败：配置无效"),
                ("未知失败", "界面任务异常：连接中断"),
            ):
                composer.value = text
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

            composer.value = "恢复"
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

    async def test_reasoning_sections_are_separate_expanded_and_clickable(self) -> None:
        """每次模型推理应独立成段、默认展开，并且可通过点击折叠正文。"""

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
            self.assertTrue(first.expanded)

            await pilot.pause()
            first_text = "".join(
                segment.text
                for line_number in range(first.size.height)
                for segment in first.render_line(line_number)
            )
            self.assertIn("先检查配置", first_text)

            await pilot.click(".reasoning-message")
            await pilot.pause()
            self.assertFalse(first.expanded)
            collapsed_text = "".join(segment.text for segment in first.render_line(0))
            self.assertNotIn("先检查配置", collapsed_text)
            self.assertEqual(first.reasoning_text, "先检查配置。")

            app._handle_tool_start(1, SimpleNamespace(name="read_file", arguments={}))
            app._append_reasoning_delta("再整理结果。")
            app._append_message("assistant", "配置已经整理完成。")
            self.assertEqual(len(app.query(ReasoningDisclosure)), 2)
            second = app.query(ReasoningDisclosure)[1]
            self.assertTrue(second.expanded)

            await pilot.pause()
            second_text = "".join(
                segment.text
                for line_number in range(second.size.height)
                for segment in second.render_line(line_number)
            )
            self.assertIn("再整理结果", second_text)
            messages = list(app.query(".message"))
            self.assertGreaterEqual(len(messages), 5)
            self.assertTrue(all(message.styles.margin.bottom == 1 for message in messages))

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

        from textual.widgets import Input, Static

        from omnicrawl.ui.fullscreen import FullscreenStartup, OmniCrawlApp

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
                tool_call = SimpleNamespace(name="read_file", arguments={"path": "README.md"})
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
            composer = app.query_one("#composer", Input)
            composer.value = "你好"
            await pilot.press("enter")
            await pilot.pause(0.8)

            self.assertFalse(app.is_generating)
            self.assertIn("回复：你好", app.conversation_text)
            self.assertIn("read_file", app.conversation_text)
            self.assertEqual(len(app.query(".tool-message")), 1)
            tool_record = app.query_one(".tool-message", Static)
            collapsed = str(tool_record.content)
            self.assertIn("⌁ read_file · 成功 ·", collapsed)
            self.assertNotIn("步骤", collapsed)
            self.assertNotIn("读取完成", collapsed)
            await pilot.click(".tool-message")
            await pilot.pause()
            expanded = str(tool_record.content)
            self.assertIn("参数：{'path': 'README.md'}", expanded)
            self.assertIn("读取完成", expanded)
            self.assertEqual(app.agent.seen_statuses, ["正在思考", "正在回复", "等待", "正在调用"])
            self.assertEqual(len(app.query(".runtime-status-message")), 0)

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
        tool_call = SimpleNamespace(name="read_file", arguments={"path": "README.md"})

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
            for index, name in enumerate(("read_file", "search_text"), start=1):
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
            self.assertIn("MDL demo-model", context.plain)
            self.assertIn("THK LOW", context.plain)
            self.assertIn("APR AUTO", context.plain)
            self.assertNotIn("完全自动批准", context.plain)

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
        first = SimpleNamespace(name="read_file", arguments={}, id="call_1")
        second = SimpleNamespace(name="search_text", arguments={}, id="call_2")

        async with app.run_test(size=(120, 40)) as pilot:
            app._handle_tool_start(1, first)
            app._handle_tool_start(2, second)
            app._handle_tool_result(second, SimpleNamespace(ok=False, output="search-failed"))
            app._handle_tool_result(first, SimpleNamespace(ok=True, output="read-ok"))
            await pilot.pause()

            records = list(app.query(ToolDisclosure))
            self.assertEqual(len(records), 2)
            self.assertIn("read_file · 成功", str(records[0].content))
            self.assertIn("search_text · 失败", str(records[1].content))
            records[0].on_click()
            records[1].on_click()
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
        tool_call = SimpleNamespace(name="read_file", arguments={"path": "README.md"})

        async with app.run_test(size=(120, 40)) as pilot:
            app._handle_tool_start(1, tool_call)
            app._handle_tool_result(tool_call, SimpleNamespace(ok=True, output="读取完成"))
            await pilot.pause()

            record = app.query_one(ToolDisclosure)
            rendered_text = "".join(segment.text for segment in record.render_line(0))
            self.assertIn("read_file", rendered_text)
            self.assertIn("成功", rendered_text)

            await pilot.click(".tool-message")
            await pilot.pause()
            expanded_text = "".join(
                segment.text
                for line_number in range(record.region.height)
                for segment in record.render_line(line_number)
            )
            self.assertIn("README.md", expanded_text)
            self.assertIn("读取完成", expanded_text)

    async def test_tool_disclosure_stays_collapsed_while_running_and_after_completion(self) -> None:
        """工具执行中和完成后均应默认折叠，标题展示状态及耗时而不展示步骤。"""

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
        tool_call = SimpleNamespace(name="read_file", arguments={"path": "README.md"})

        async with app.run_test(size=(120, 40)) as pilot:
            with patch("omnicrawl.ui.fullscreen.time.perf_counter", side_effect=[10.0, 10.126]):
                app._handle_tool_start(7, tool_call)
                record = app.query_one(ToolDisclosure)
                self.assertFalse(record.expanded)
                self.assertEqual(str(record.content), "▸ ⌁ read_file · 调用中 · 0.00s")

                app._handle_tool_result(tool_call, SimpleNamespace(ok=True, output="读取完成"))

            self.assertFalse(record.expanded)
            self.assertEqual(str(record.content), "▸ ⌁ read_file · 成功 · 0.13s")
            self.assertNotIn("步骤", str(record.content))
            self.assertNotIn("读取完成", str(record.content))

            await pilot.pause()
            await pilot.click(".tool-message")
            await pilot.pause()
            self.assertTrue(record.expanded)
            self.assertIn("读取完成", str(record.content))

    async def test_tool_result_truncates_large_output_for_rendering(self) -> None:
        """工具的大输出只能保留预览，且仅在展开调用记录后显示。"""

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
        result = SimpleNamespace(ok=True, output="x" * 20_000)

        async with app.run_test(size=(120, 40)) as pilot:
            app._handle_tool_start(1, tool_call)
            app._handle_tool_result(tool_call, result)
            await pilot.pause()

            record = app.query_one(ToolDisclosure)
            self.assertNotIn("界面展示已截断", str(record.content))
            await pilot.click(".tool-message")
            await pilot.pause()
            rendered = str(record.content)
            self.assertIn("界面展示已截断", rendered)
            self.assertLess(len(rendered), 5_000)

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

    async def test_model_command_runs_outside_event_loop(self) -> None:
        """模型列表检测较慢时，事件循环仍必须能够继续处理界面事件。"""

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

        def delayed_model_command(_agent, _text: str) -> str:
            time.sleep(0.25)
            return "模型列表已加载"

        async with app.run_test(size=(120, 40)) as pilot:
            with patch("omnicrawl.ui.fullscreen.handle_model_command", side_effect=delayed_model_command):
                started = time.monotonic()
                self.assertTrue(app._handle_command("/model demo-model"))
                elapsed = time.monotonic() - started
                self.assertLess(elapsed, 0.1)
                await pilot.pause(0.35)

            self.assertIn("模型列表已加载", app.conversation_text)
