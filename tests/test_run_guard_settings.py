"""Run Guard 设置面板：打开、修改、保存、失败回滚与 settings 入口测试。"""

from __future__ import annotations

import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace

from textual.app import App, ComposeResult
from textual.widgets import Input, Select, Static

from omnicrawl.config.features.run_guard import (
    RunGuardConfig,
    load_run_guard_config,
)
from omnicrawl.ui.fullscreen.screens.run_guard_settings import RunGuardSettingsScreen
from omnicrawl.ui.fullscreen.screens.settings import SettingsScreen, _SETTING_ORDER


class _HostApp(App):
    def compose(self) -> ComposeResult:
        yield Static("probe")


def _config_path(temp_dir: str) -> Path:
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
        "auto_retry_errors = [\"SERVICE_UNAVAILABLE\"]\n"
        "[run_guard.continue]\n"
        "enabled = true\n"
        "max_auto_followups = 3\n",
        encoding="utf-8",
    )
    return path


class RunGuardSettingsScreenTests(unittest.IsolatedAsyncioTestCase):
    async def test_opens_with_current_values(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            path = _config_path(temp_dir)
            app = _HostApp()
            async with app.run_test(size=(110, 45)) as pilot:
                app.push_screen(RunGuardSettingsScreen(path))
                await pilot.pause()
                self.assertIsInstance(app.screen, RunGuardSettingsScreen)
                self.assertTrue(
                    app.screen.query_one("#run-guard-enabled", Select).value
                )
                self.assertTrue(
                    app.screen.query_one("#run-guard-guard-enabled", Select).value
                )
                self.assertEqual(
                    app.screen.query_one("#run-guard-window", Input).value,
                    "2000",
                )
                self.assertEqual(
                    app.screen.query_one("#run-guard-errors", Input).value,
                    "SERVICE_UNAVAILABLE",
                )
                self.assertEqual(
                    app.screen.query_one("#run-guard-followups", Input).value,
                    "3",
                )

    async def test_modify_and_save_persists_and_applies(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            path = _config_path(temp_dir)
            applied: list[RunGuardConfig] = []
            app = _HostApp()
            async with app.run_test(size=(110, 45)) as pilot:
                app.push_screen(
                    RunGuardSettingsScreen(
                        path,
                        apply_configuration=applied.append,
                    )
                )
                await pilot.pause()
                # 修改关键字段
                app.screen.query_one("#run-guard-window", Input).value = "1500"
                app.screen.query_one("#run-guard-followups", Input).value = "5"
                app.screen.query_one("#run-guard-errors", Input).value = (
                    "SERVICE_UNAVAILABLE, PI_AI_ERROR"
                )
                await pilot.press("ctrl+s")
                await pilot.pause()

            self.assertEqual(len(applied), 1)
            self.assertEqual(applied[0].guard.window_chars, 1500)
            self.assertEqual(applied[0].continuation.max_auto_followups, 5)
            self.assertEqual(
                applied[0].guard.auto_retry_errors,
                ("SERVICE_UNAVAILABLE", "PI_AI_ERROR"),
            )
            saved = load_run_guard_config(path)
            self.assertEqual(saved.guard.window_chars, 1500)
            self.assertEqual(saved.continuation.max_auto_followups, 5)
            self.assertEqual(
                saved.guard.auto_retry_errors,
                ("SERVICE_UNAVAILABLE", "PI_AI_ERROR"),
            )

    async def test_invalid_input_shows_error_and_stays(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            path = _config_path(temp_dir)
            app = _HostApp()
            async with app.run_test(size=(110, 45)) as pilot:
                app.push_screen(RunGuardSettingsScreen(path))
                await pilot.pause()
                app.screen.query_one("#run-guard-window", Input).value = "abc"
                await pilot.press("ctrl+s")
                await pilot.pause()
                self.assertIsInstance(app.screen, RunGuardSettingsScreen)
                self.assertIn(
                    "保存失败",
                    str(app.screen.query_one("#run-guard-status").content),
                )
                # 非法输入未破坏磁盘配置
            saved = load_run_guard_config(path)
            self.assertEqual(saved.guard.window_chars, 2000)

    async def test_apply_failure_restores_previous_config(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            path = _config_path(temp_dir)

            def failing_apply(_config):
                raise ValueError("应用配置失败")

            app = _HostApp()
            async with app.run_test(size=(110, 45)) as pilot:
                app.push_screen(
                    RunGuardSettingsScreen(
                        path,
                        apply_configuration=failing_apply,
                    )
                )
                await pilot.pause()
                app.screen.query_one("#run-guard-window", Input).value = "9999"
                await pilot.press("ctrl+s")
                await pilot.pause()
                self.assertIsInstance(app.screen, RunGuardSettingsScreen)
                self.assertIn(
                    "保存失败",
                    str(app.screen.query_one("#run-guard-status").content),
                )
            # 应用失败时磁盘配置回滚为旧值
            saved = load_run_guard_config(path)
            self.assertEqual(saved.guard.window_chars, 2000)


class SettingsScreenRunGuardEntryTests(unittest.TestCase):
    def test_run_guard_entry_is_an_open_subpage(self) -> None:
        """运行节奏行右侧内嵌复杂面板，不再跳转整屏。"""
        from omnicrawl.ui.fullscreen.screens.settings import SettingsScreen

        screen = object.__new__(SettingsScreen)
        screen._agent = SimpleNamespace(
            current_model="demo",
            approval_mode="manual",
            reasoning_effort="none",
            context_window_tokens=128_000,
            config=SimpleNamespace(
                run_guard=SimpleNamespace(enabled=True),
            ),
        )
        screen._advanced = False
        # 标签与行键仍暴露（左侧列表需要）。
        self.assertEqual(SettingsScreen._row_labels()["run_guard"], "持续运转")
        self.assertIn("run_guard", _SETTING_ORDER)
        # 路由方法能处理 run_guard（右侧内嵌复杂面板）。
        self.assertIsNotNone(getattr(screen, "_build_complex_pane", None))

    def test_settings_screen_exposes_run_guard_entry(self) -> None:
        screen = object.__new__(SettingsScreen)
        screen._agent = SimpleNamespace(
            current_model="demo",
            approval_mode="manual",
            reasoning_effort="none",
            context_window_tokens=128_000,
            config=SimpleNamespace(
                run_guard=SimpleNamespace(enabled=True),
            ),
        )
        screen._advanced = False
        screen._row_keys = ("model", "channels", "vision", "image_gen", "run_guard")
        self.assertEqual(SettingsScreen._row_labels()["run_guard"], "持续运转")
        self.assertIn("run_guard", screen._row_keys)


if __name__ == "__main__":
    unittest.main()
