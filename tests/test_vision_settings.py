from __future__ import annotations

import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace

from textual.app import App, ComposeResult
from textual.widgets import Static

from omnicrawl.config.llm import ActiveModelRef
from omnicrawl.config.vision import VisionConfiguration
from omnicrawl.ui.fullscreen.settings import SettingsScreen
from omnicrawl.ui.fullscreen.vision_settings import VisionSettingsScreen


class _HostApp(App):
    def compose(self) -> ComposeResult:
        yield Static("probe")


class VisionSettingsScreenTests(unittest.IsolatedAsyncioTestCase):
    async def test_can_toggle_and_save_ordered_vision_configuration(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            path = Path(temp_dir) / "config.toml"
            path.write_text(
                "[vision]\nenabled = false\nmodels = [\n    {source = \"custom\", key = \"vision-first\"},\n    {source = \"custom\", key = \"vision-second\"},\n]\n",
                encoding="utf-8",
            )
            applied: list[VisionConfiguration] = []
            agent = SimpleNamespace()
            app = _HostApp()
            async with app.run_test(size=(110, 35)) as pilot:
                app.push_screen(
                    VisionSettingsScreen(agent, path, apply_configuration=applied.append)
                )
                await pilot.pause()
                self.assertIn("已停用", str(app.screen.query_one("#vision-settings-enabled").content))
                self.assertEqual(len(app.screen.query(".vision-model-row")), 2)

                await pilot.press("space")
                await pilot.press("ctrl+s")
                await pilot.pause()

            self.assertEqual(len(applied), 1)
            self.assertTrue(applied[0].enabled)
            self.assertEqual(
                [ref.key for ref in applied[0].models],
                ["vision-first", "vision-second"],
            )

    async def test_cannot_enable_without_a_fallback_model(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            path = Path(temp_dir) / "config.toml"
            path.write_text("[vision]\nenabled = false\nmodels = []\n", encoding="utf-8")
            app = _HostApp()
            async with app.run_test(size=(110, 35)) as pilot:
                app.push_screen(VisionSettingsScreen(SimpleNamespace(), path))
                await pilot.pause()
                await pilot.press("space")
                await pilot.press("ctrl+s")
                await pilot.pause()
                self.assertIsInstance(app.screen, VisionSettingsScreen)
                self.assertIn("至少添加一个", app.screen._status)


class SettingsScreenVisionEntryTests(unittest.TestCase):
    def test_settings_screen_exposes_vision_entry(self) -> None:
        screen = object.__new__(SettingsScreen)
        screen._agent = SimpleNamespace(
            current_model="demo",
            config=SimpleNamespace(
                vision=VisionConfiguration(
                    enabled=True,
                    models=(ActiveModelRef(source="custom", key="vision-model"),),
                )
            ),
        )
        screen._advanced = False
        screen._row_keys = ("model", "channels", "vision")

        self.assertEqual(SettingsScreen._row_labels()["vision"], "视觉")
        self.assertEqual(screen._current_row_values()["vision"], "已开启")


if __name__ == "__main__":
    unittest.main()
