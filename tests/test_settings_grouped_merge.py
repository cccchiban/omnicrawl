"""设置页合并项测试：工具设置/子任务设置分节面板的路由、行内容与修改回调。"""

from __future__ import annotations

import unittest
from types import SimpleNamespace

from textual.app import App, ComposeResult
from textual.widgets import Static

from omnicrawl.ui.fullscreen.screens.settings import (
    SettingsScreen,
    _SETTING_ORDER,
    _SubagentsPane,
    _ToolsPane,
)


def _screen() -> SettingsScreen:
    agent = SimpleNamespace(
        current_model="demo",
        approval_mode="review",
        reasoning_effort="none",
        context_window_tokens=128_000,
        workspace_root="D:/workspace",
        current_session_id="s",
        skill_manager=None,
        _memory_store=None,
        _mcp_manager=SimpleNamespace(enabled=True),
        _plugin_manager=SimpleNamespace(enabled=False),
        _tools=SimpleNamespace(keys=lambda: ("read", "bash")),
        config=SimpleNamespace(
            context_compaction=SimpleNamespace(trigger_context_tokens=102_400),
            subagents=SimpleNamespace(
                enabled=True,
                max_concurrency=2,
                max_tasks_per_batch=4,
                default_timeout_seconds=3600.0,
                model_request_concurrency=2,
                verify_command_timeout_seconds=120,
                task_retention_minutes=60,
            ),
            show_thinking=True,
            llm=SimpleNamespace(model_source="legacy", catalog_key=""),
            vision=SimpleNamespace(enabled=False),
            image_gen=SimpleNamespace(enabled=False),
            disabled_tools=frozenset({"powershell"}),
        ),
    )
    screen = object.__new__(SettingsScreen)
    screen._agent = agent
    screen._advanced = False
    screen._row_keys = _SETTING_ORDER
    return screen


class SettingsGroupedMergeTests(unittest.TestCase):
    def test_merged_keys_replace_old_simple_keys(self) -> None:
        # 旧独立键不再作为一级设置项；新合并键在列。
        for old in ("approval", "mcp", "subagents_advanced"):
            self.assertNotIn(old, _SETTING_ORDER)
        for merged in ("tools", "subagents"):
            self.assertIn(merged, _SETTING_ORDER)

    def test_tools_row_label_and_subagents_row_label(self) -> None:
        labels = SettingsScreen._row_labels()
        self.assertEqual(labels["tools"], "工具设置")
        self.assertEqual(labels["subagents"], "子任务设置")
        # 旧独立标签移除
        self.assertNotIn("工具审批", labels.values())
        self.assertNotIn("MCP 工具", labels.values())
        self.assertNotIn("子任务功能", labels.values())

    def test_tools_pane_sections(self) -> None:
        pane = _screen()._build_pane("tools")
        self.assertIsInstance(pane, _ToolsPane)
        self.assertEqual(pane._rows[0], "approval")
        # 分节标题
        heads = [h for h, _ in pane.sections]
        self.assertEqual(heads, ["审批模式", "MCP 工具", "内置工具开关"])
        # MCP Server 管理行存在
        self.assertIn("mcp-servers", pane._rows)
        # 工具开关行数 = 全部 TOOL_SWITCH_KEYS
        from omnicrawl.config.features.tools import TOOL_SWITCH_KEYS
        tool_rows = [k for k in pane._rows if k.startswith("tool:")]
        self.assertEqual(len(tool_rows), len(TOOL_SWITCH_KEYS))

    def test_subagents_pane_sections(self) -> None:
        pane = _screen()._build_pane("subagents")
        self.assertIsInstance(pane, _SubagentsPane)
        self.assertEqual(pane._rows[0], "enabled")
        heads = [h for h, _ in pane.sections]
        self.assertEqual(heads, ["子任务功能", "高级参数"])
        # 高级参数行数 = SUBAGENT_ADVANCED_SETTING_KEYS
        from omnicrawl.config.features.subagents import SUBAGENT_ADVANCED_SETTING_KEYS
        self.assertEqual(
            len([k for k in pane._rows if k != "enabled"]),
            len(SUBAGENT_ADVANCED_SETTING_KEYS),
        )


class GroupedRowsPaneUnmountRaceTests(unittest.IsolatedAsyncioTestCase):
    """分节列表 pane 被切走（拆除）后，迟到的 refresh_pane 不得抛 NoMatches。

    回归：Textual 的 InvokeLater 会转发到 screen 空闲回调执行；on_mount 里
    call_after_refresh(refresh_pane) 排队后若 pane 已被 remove_children 拆除，
    迟到的 refresh 会在子节点清空后执行 query_one('#grouped-pane-row-0')，
    抛 NoMatches 使整个 TUI 崩溃。守卫应静默跳过该刷新。
    """

    async def test_late_refresh_after_unmount_is_skipped(self) -> None:
        app = _HostApp()
        async with app.run_test(size=(100, 40)) as pilot:
            pane = _screen()._build_pane("tools")
            self.assertIsInstance(pane, _ToolsPane)
            await app.mount(pane)
            await pilot.pause()
            self.assertTrue(pane._can_refresh())
            # 拆除 pane（模拟切换到其它设置项）
            area = pane.parent
            self.assertIsNotNone(area)
            area.remove_children()  # type: ignore[union-attr]
            # 拆除后守卫应立刻失效（_pruning 置位）
            self.assertFalse(pane._can_refresh())
            # 迟到刷新不得抛异常（修复前：NoMatches '#grouped-pane-row-0'）
            pane.refresh_pane()  # 应被守卫拦截
            await pilot.pause()
            # 完全拆除后仍应安全
            self.assertFalse(pane._can_refresh())
            pane.refresh_pane()

    async def test_refresh_while_mounted_updates_rows(self) -> None:
        app = _HostApp()
        async with app.run_test(size=(100, 40)) as pilot:
            pane = _screen()._build_pane("subagents")
            await app.mount(pane)
            await pilot.pause()
            # 正常挂载期守卫放行
            self.assertTrue(pane._can_refresh())
            pane.refresh_pane()
            first = pane.query_one("#grouped-pane-row-0", Static)
            self.assertIn("功能总开关", str(first.content))


class _HostApp(App):
    def compose(self) -> ComposeResult:
        yield Static("probe")


if __name__ == "__main__":
    unittest.main()
