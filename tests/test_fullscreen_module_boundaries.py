from __future__ import annotations

import importlib
import unittest
from pathlib import Path

from omnicrawl.ui import fullscreen


class FullscreenModuleBoundaryTests(unittest.TestCase):
    """锁定全屏 UI 的组件和纯 HUD 格式化边界。"""

    def test_fullscreen_support_modules_are_real_files(self) -> None:
        package_path = Path(fullscreen.__file__).resolve().parent

        for module_name in ("widgets", "hud"):
            with self.subTest(module=module_name):
                module = importlib.import_module(f"omnicrawl.ui.fullscreen.{module_name}")
                self.assertEqual(module.__name__, f"omnicrawl.ui.fullscreen.{module_name}")
                self.assertEqual(Path(module.__file__).resolve(), package_path / f"{module_name}.py")

    def test_package_keeps_widget_compatibility_exports(self) -> None:
        for export_name in (
            "ConfirmationScreen",
            "ReasoningDisclosure",
            "ToolDisclosure",
            "FullscreenStartup",
            "OmniCrawlApp",
            "run_fullscreen_tui",
        ):
            with self.subTest(export=export_name):
                self.assertTrue(hasattr(fullscreen, export_name))

    def test_hud_formatter_does_not_depend_on_textual(self) -> None:
        hud = importlib.import_module("omnicrawl.ui.fullscreen.hud")
        self.assertFalse(any(name == "textual" or name.startswith("textual.") for name in hud.__dict__))
