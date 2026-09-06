"""设置页简单项 → SelectPane（单个下拉选项框）路由与交互测试。"""

from __future__ import annotations

import unittest
from types import SimpleNamespace

from textual.app import App, ComposeResult
from textual.widgets import Select, Static

from omnicrawl.ui.fullscreen.screens.panes import SelectPane, SettingsPane
from omnicrawl.ui.fullscreen.screens.settings import (
    SettingsScreen,
    _SETTING_ORDER,
    _FEATURES,
)


class _HostApp(App):
    def compose(self) -> ComposeResult:
        yield Static("probe")


def _keyboard_agent() -> SimpleNamespace:
    """挂载 SettingsScreen 所需的较完整 fake agent（不触发保存）。"""
    return SimpleNamespace(
        current_model="demo",
        approval_mode="manual",
        reasoning_effort="none",
        context_window_tokens=128_000,
        workspace_root="D:/workspace",
        current_session_id="s",
        skill_manager=None,
        _memory_store=None,
        _mcp_manager=SimpleNamespace(enabled=False),
        _plugin_manager=SimpleNamespace(enabled=False),
        config=SimpleNamespace(
            context_compaction=SimpleNamespace(trigger_context_tokens=102_400),
            subagents=SimpleNamespace(enabled=False),
            show_thinking=True,
            llm=SimpleNamespace(model_source="legacy", catalog_key=""),
            vision=SimpleNamespace(enabled=False),
            image_gen=SimpleNamespace(enabled=False),
            disabled_tools=frozenset(),
        ),
        _tools={},
    )


def _agent() -> SimpleNamespace:
    """构造 SettingsScreen 简单项路由所需的最小 agent。"""
    return SimpleNamespace(
        current_model="demo",
        approval_mode="manual",
        reasoning_effort="none",
        context_window_tokens=128_000,
        _memory_store=None,
        _mcp_manager=SimpleNamespace(enabled=False),
        _plugin_manager=SimpleNamespace(enabled=False),
        config=SimpleNamespace(
            context_compaction=SimpleNamespace(trigger_context_tokens=102_400),
            subagents=SimpleNamespace(enabled=False),
            show_thinking=True,
            llm=SimpleNamespace(model_source="legacy", catalog_key=""),
        ),
    )


def _screen(agent: SimpleNamespace | None = None) -> SettingsScreen:
    screen = object.__new__(SettingsScreen)
    screen._agent = agent if agent is not None else _agent()
    screen._advanced = False
    screen._row_keys = _SETTING_ORDER
    return screen


class SettingsSimpleSelectRouteTests(unittest.TestCase):
    """简单单值设置项右侧应挂载 SelectPane（单个下拉选项框）。"""

    def test_simple_keys_build_select_pane(self) -> None:
        screen = _screen()
        for key in (
            "context",
            "reasoning",
            "context_compaction_threshold",
            "show_thinking",
            *(item[0] for item in _FEATURES),
        ):
            with self.subTest(key=key):
                pane = screen._build_pane(key)
                self.assertIsInstance(pane, SelectPane)

    def test_context_select_options_and_current(self) -> None:
        screen = _screen()
        pane = screen._build_pane("context")
        self.assertIsInstance(pane, SelectPane)
        select = pane._options
        self.assertEqual(select[0], ("32K", 32_000))
        # 当前 128K 应选中
        self.assertEqual(pane._current, 128_000)

    def test_tools_builds_grouped_pane(self) -> None:
        from omnicrawl.ui.fullscreen.screens.settings import _ToolsPane

        screen = _screen()
        pane = screen._build_pane("tools")
        self.assertIsInstance(pane, _ToolsPane)

    def test_subagents_builds_grouped_pane(self) -> None:
        from omnicrawl.ui.fullscreen.screens.settings import _SubagentsPane

        screen = _screen()
        pane = screen._build_pane("subagents")
        self.assertIsInstance(pane, _SubagentsPane)

    def test_simple_items_in_settings_order(self) -> None:
        for key in ("context", "reasoning", "context_compaction_threshold", "show_thinking"):
            self.assertIn(key, _SETTING_ORDER)


class SelectPaneInteractionTests(unittest.IsolatedAsyncioTestCase):
    """SelectPane：单个选项框选择即保存、失败回滚、Esc 返回。"""

    async def test_select_change_applies_immediately(self) -> None:
        applied: list[tuple[str, object]] = []

        def applier(value: object) -> str:
            applied.append(("approval", value))
            return f"审批模式已设为 {value}。"

        pane = SelectPane(
            [("手动审批", "manual"), ("人工复核", "review"), ("自动审批", "auto")],
            "manual",
            applier,
        )
        back: list[bool] = []
        pane.bind_pane_events(on_back=lambda: back.append(True))

        app = _HostApp()
        async with app.run_test(size=(80, 24)) as pilot:
            await app.mount(pane)
            await pilot.pause()
            select = pane.query_one("#select-pane-control", Select)
            self.assertEqual(select.value, "manual")
            select.value = "review"
            await pilot.pause()
            self.assertEqual(applied, [("approval", "review")])
            status = pane.query_one("#select-pane-status", Static)
            self.assertIn("review", str(status.content))

    async def test_select_failure_rolls_back_value(self) -> None:
        def applier(value: object) -> str:
            return f"设置未完成：保存失败 {value}"

        pane = SelectPane(
            [("开启", True), ("关闭", False)],
            True,
            applier,
        )
        app = _HostApp()
        async with app.run_test(size=(80, 24)) as pilot:
            await app.mount(pane)
            await pilot.pause()
            select = pane.query_one("#select-pane-control", Select)
            select.value = False
            await pilot.pause()
            self.assertEqual(select.value, True)
            status = pane.query_one("#select-pane-status", Static)
            self.assertIn("设置未完成", str(status.content))

    async def test_escape_requests_back(self) -> None:
        pane = SelectPane(
            [("32K", 32_000), ("128K", 128_000)],
            128_000,
            lambda value: "ok",
        )
        back: list[bool] = []
        pane.bind_pane_events(on_back=lambda: back.append(True))
        app = _HostApp()
        async with app.run_test(size=(80, 24)) as pilot:
            await app.mount(pane)
            await pilot.pause()
            pane.focus()
            await pilot.press("escape")
            await pilot.pause()
            self.assertEqual(back, [True])


class SettingsScreenSelectPaneTests(unittest.IsolatedAsyncioTestCase):
    """设置页右侧简单项实际渲染为单个 Select 下拉选项框。"""

    async def test_context_row_shows_single_select(self) -> None:
        from omnicrawl.ui.fullscreen.screens.settings import SettingsScreen

        app = _HostApp()
        async with app.run_test(size=(110, 40)) as pilot:
            app.push_screen(SettingsScreen(_keyboard_agent(), advanced=False))
            await pilot.pause()
            await pilot.pause()
            # 行序：model/context/... 下移 1 行到“上下文长度”。
            await pilot.press("down")
            await pilot.pause()
            await pilot.pause()
            screen = app.screen
            area = screen.query_one("#settings-right-area")
            pane_types = [type(w).__name__ for w in area.children]
            self.assertIn("SelectPane", pane_types)
            select = screen.query_one("#select-pane-control", Select)
            self.assertEqual(select.value, 128_000)
            # 展开选项应含候选窗口值（Select 内部 OptionList 选项）。
            self.assertIn("64K", str(select._options))


if __name__ == "__main__":
    unittest.main()
