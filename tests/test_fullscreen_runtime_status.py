"""状态行 [ ESC ] 中断提示的鼠标悬停与点击交互回归。

锁定 2026-09 需求：运行状态（正在思考/回复/调用…）行尾的 ``[ ESC ]``
提示在鼠标悬停时变为淡蓝色（`ansi_bright_blue`），离开恢复灰色粗体；
点击它等价于按下键盘 ESC（触发宿主 ``action_cancel_or_focus``）。
状态文本高频重绘不得打断悬停态，悬停/点击热区只属于 [ ESC ] 本身。
"""

from __future__ import annotations

import unittest

from rich.text import Text
from textual.app import App, ComposeResult
from textual.containers import VerticalScroll
from textual.widgets import Static

from omnicrawl.ui.fullscreen.rendering.widgets import RuntimeStatus
from omnicrawl.ui.fullscreen.terminal.theme import terminal_css

_HOVER_BLUE = "Color(0, 0, 255, ansi=12)"


class _HostApp(App[None]):
    """最小宿主：挂载真实 App 级 CSS 变量与单个 RuntimeStatus。"""

    CSS = terminal_css("""
    #conversation { height: 1fr; padding: 0 1; }
    .message {
        margin: 0 0 1 0;
        padding: 0 1;
        background: transparent;
        border: none;
    }
    .message.runtime-status-message {
        color: $terminal-text-muted;
        text-style: bold;
        border-top: none;
        border-bottom: none;
        margin-bottom: 0;
    }
    """)

    def compose(self) -> ComposeResult:
        with VerticalScroll(id="conversation", can_focus=False):
            yield RuntimeStatus()


async def _fill_label(app: _HostApp, pilot) -> None:
    """先填入状态文本并等待布局稳定，避免空 label 阶段 hover 位置漂移。"""

    label = app.query_one("#runtime-status-label", Static)
    label.update(Text("⠋ 正在思考"))
    await pilot.pause()
    await pilot.pause()


class RuntimeStatusHoverTests(unittest.IsolatedAsyncioTestCase):
    async def test_default_esc_hint_is_dim_bold(self) -> None:
        """[ ESC ] 平时为灰色粗体（dim + bold），可读但不抢注意力。"""

        app = _HostApp()
        async with app.run_test(size=(100, 12)) as pilot:
            await pilot.pause()
            await _fill_label(app, pilot)
            hint = app.query_one("#runtime-status-esc-hint", Static)
            self.assertFalse(hint.has_pseudo_class("hover"))
            style = str(hint.styles.text_style)
            self.assertIn("dim", style)
            self.assertIn("bold", style)

    async def test_hover_turns_blue_bold_and_leave_restores_dim(self) -> None:
        """悬停 [ ESC ] 变淡蓝加粗；移回状态文本后恢复灰粗体。"""

        app = _HostApp()
        async with app.run_test(size=(100, 12)) as pilot:
            await pilot.pause()
            await _fill_label(app, pilot)
            hint = app.query_one("#runtime-status-esc-hint", Static)
            # 悬停 [ ESC ]：淡蓝 + 去灰（bold 保留）
            await pilot.hover("#runtime-status-esc-hint")
            await pilot.pause()
            self.assertTrue(hint.has_pseudo_class("hover"))
            self.assertEqual(str(hint.styles.color), _HOVER_BLUE)
            style = str(hint.styles.text_style)
            self.assertIn("bold", style)
            self.assertNotIn("dim", style)
            # 移回状态文本：恢复灰粗体
            await pilot.hover("#runtime-status-label")
            await pilot.pause()
            self.assertFalse(hint.has_pseudo_class("hover"))
            self.assertEqual(str(hint.styles.color), "Color(255, 255, 255)")
            style = str(hint.styles.text_style)
            self.assertIn("bold", style)
            self.assertIn("dim", style)

    async def test_streaming_updates_do_not_clear_hover(self) -> None:
        """状态文本高频重绘期间，悬停态必须保持（不闪烁回灰）。"""

        app = _HostApp()
        async with app.run_test(size=(100, 12)) as pilot:
            await pilot.pause()
            await _fill_label(app, pilot)
            label = app.query_one("#runtime-status-label", Static)
            hint = app.query_one("#runtime-status-esc-hint", Static)
            await pilot.hover("#runtime-status-esc-hint")
            await pilot.pause()
            self.assertTrue(hint.has_pseudo_class("hover"))
            for index, frame in enumerate("⠋⠙⠹⠸⠼⠴"):
                label.update(Text(f"{frame} 正在思考"))
                await pilot.pause(0.01)
                self.assertTrue(
                    hint.has_pseudo_class("hover"),
                    f"第 {index} 帧更新后悬停态丢失",
                )
                self.assertEqual(str(hint.styles.color), _HOVER_BLUE)

    async def test_hover_zone_is_esc_hint_only(self) -> None:
        """悬停热区只属于 [ ESC ]：状态文本区域不触发变蓝。"""

        app = _HostApp()
        async with app.run_test(size=(100, 12)) as pilot:
            await pilot.pause()
            await _fill_label(app, pilot)
            hint = app.query_one("#runtime-status-esc-hint", Static)
            await pilot.hover("#runtime-status-label")
            await pilot.pause()
            self.assertFalse(hint.has_pseudo_class("hover"))
            self.assertEqual(str(hint.styles.color), "Color(255, 255, 255)")

    async def test_click_esc_hint_invokes_cancel_action(self) -> None:
        """点击 [ ESC ] 等价键盘 ESC：调用宿主 action_cancel_or_focus。"""

        calls: list[str] = []
        app = _HostApp()

        def spy_action() -> None:
            calls.append("action_cancel_or_focus")

        # 动态附加宿主 action（真实 App 由 TurnExecutionMixin 提供）。
        app.action_cancel_or_focus = spy_action  # type: ignore[attr-defined]
        async with app.run_test(size=(100, 12)) as pilot:
            await pilot.pause()
            await _fill_label(app, pilot)
            await pilot.click("#runtime-status-esc-hint")
            await pilot.pause()
            self.assertEqual(calls, ["action_cancel_or_focus"])

    async def test_double_click_does_not_cancel_twice(self) -> None:
        """双击只取消一次：chain==2 的原生选词手势不重复触发取消。"""

        calls: list[str] = []
        app = _HostApp()

        def spy_action() -> None:
            calls.append("action_cancel_or_focus")

        app.action_cancel_or_focus = spy_action  # type: ignore[attr-defined]
        async with app.run_test(size=(100, 12)) as pilot:
            await pilot.pause()
            await _fill_label(app, pilot)
            await pilot.click("#runtime-status-esc-hint", times=2)
            await pilot.pause()
            self.assertEqual(calls, ["action_cancel_or_focus"])


if __name__ == "__main__":
    unittest.main()
