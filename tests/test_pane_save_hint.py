"""保存成功提示（flash_save_hint）：保存按钮左侧短暂显示“设置已保存”。"""

from __future__ import annotations

import tempfile
import unittest
from pathlib import Path

from textual.app import App, ComposeResult
from textual.widgets import Static

from omnicrawl.ui.fullscreen.screens.agent_workspace_settings import (
    AgentWorkspaceSettingsPane,
)
from omnicrawl.ui.fullscreen.screens.image_gen_settings import ImageGenSettingsPane
from omnicrawl.ui.fullscreen.screens.run_guard_settings import RunGuardSettingsPane
from omnicrawl.ui.fullscreen.screens.tts_settings import TTSSettingsPane


class _PaneHost(App):
    """直接把 Pane 挂到宿主，观察 hint 的显示/隐藏。"""

    def __init__(self, pane_factory) -> None:
        super().__init__()
        self._pane_factory = pane_factory
        self.pane = None

    def compose(self) -> ComposeResult:
        self.pane = self._pane_factory()
        yield self.pane


class PaneSaveHintTests(unittest.IsolatedAsyncioTestCase):
    async def test_flash_hint_visible_then_hidden(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            path = Path(temp_dir) / "config.toml"
            path.write_text(
                "[run_guard]\n"
                "enabled = true\n"
                "[run_guard.guard]\n"
                "enabled = true\n"
                "window_chars = 2000\n"
                "substr_len = 32\n"
                "repeat_ratio = 0.7\n"
                "check_every = 50\n"
                "max_blocks = 10000\n"
                "max_chars = 500000\n"
                "max_guard_retries = 2\n"
                "auto_retry_errors = []\n"
                "[run_guard.continue]\n"
                "enabled = true\n"
                "max_auto_followups = 3\n",
                encoding="utf-8",
            )

            def factory() -> RunGuardSettingsPane:
                return RunGuardSettingsPane(path)

            app = _PaneHost(factory)
            async with app.run_test(size=(110, 40)) as pilot:
                await pilot.pause()
                pane = app.pane
                hint = pane.query_one(".pane-save-hint", Static)
                # 初始隐藏
                self.assertFalse(hint.display)
                # 保存成功后显示绿色提示
                pane.action_save()
                await pilot.pause()
                self.assertTrue(hint.display)
                self.assertEqual(str(hint.content).strip(), "设置已保存")
                # 约 2 秒后自动隐藏
                await pilot.pause(2.5)
                self.assertFalse(hint.display)

    async def test_all_form_panes_compose_hint(self) -> None:
        """四个内嵌表单页均在保存按钮左侧提供提示占位。"""
        with tempfile.TemporaryDirectory() as temp_dir:
            path = Path(temp_dir) / "config.toml"

            factories = {
                "agent_workspace": lambda: AgentWorkspaceSettingsPane(path),
                "image_gen": lambda: ImageGenSettingsPane(path),
                "run_guard": lambda: RunGuardSettingsPane(path),
                "tts": lambda: TTSSettingsPane(path),
            }
            for name, factory in factories.items():
                with self.subTest(pane=name):
                    path.write_text("", encoding="utf-8")
                    app = _PaneHost(factory)
                    async with app.run_test(size=(110, 50)) as pilot:
                        await pilot.pause()
                        hints = list(app.pane.query(".pane-save-hint"))
                        self.assertEqual(len(hints), 1, f"{name} 缺少保存提示占位")


if __name__ == "__main__":
    unittest.main()
