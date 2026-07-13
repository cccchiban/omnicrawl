from __future__ import annotations

import importlib
import unittest
from pathlib import Path

from omnicrawl.ui import fullscreen


class FullscreenModuleBoundaryTests(unittest.TestCase):
    """锁定全屏 UI 的组件和纯 HUD 格式化边界。"""

    def test_fullscreen_support_modules_are_real_files(self) -> None:
        package_path = Path(fullscreen.__file__).resolve().parent

        for module_name in ("widgets", "hud", "turns", "commands", "monitor", "tool_diff"):
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

    def test_non_visual_support_modules_do_not_depend_on_textual(self) -> None:
        for module_name in ("hud", "turns", "commands", "monitor", "tool_diff"):
            with self.subTest(module=module_name):
                module = importlib.import_module(f"omnicrawl.ui.fullscreen.{module_name}")
                self.assertFalse(
                    any(
                        name == "textual" or name.startswith("textual.")
                        for name in module.__dict__
                    )
                )

    def test_fullscreen_keeps_command_handler_patch_points(self) -> None:
        """命令抽离后，既有全屏模块级 monkeypatch 入口仍必须存在。"""

        for name in (
            "format_memory_clean_result",
            "format_mcp_status",
            "format_skills_list",
            "handle_approval_command",
            "handle_model_command",
            "handle_reasoning_command",
            "handle_session_command",
        ):
            with self.subTest(name=name):
                self.assertTrue(callable(getattr(fullscreen, name, None)))

    def test_package_keeps_explicit_widget_and_turn_exports(self) -> None:
        self.assertEqual(
            fullscreen.__all__,
            [
                "AgentTurnCallbacks",
                "AgentTurnController",
                "ConfirmationScreen",
                "FullscreenStartup",
                "ModelPickerResult",
                "ModelPickerScreen",
                "OmniCrawlApp",
                "ReasoningDisclosure",
                "ToolDisclosure",
                "run_fullscreen_tui",
            ],
        )
