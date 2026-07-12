from __future__ import annotations

import importlib
import unittest
from pathlib import Path

from omnicrawl.state import session as session_module


class SessionModuleBoundaryTests(unittest.TestCase):
    """确保会话子域使用真实模块，并保留原 Session API。"""

    MODULE_NAMES = (
        "session_models",
        "prompt_history",
        "session_projection",
        "session_artifacts",
    )

    def test_session_subdomains_are_real_modules(self) -> None:
        state_path = Path(session_module.__file__).resolve().parent

        for module_name in self.MODULE_NAMES:
            with self.subTest(module=module_name):
                module = importlib.import_module(f"omnicrawl.state.{module_name}")
                self.assertEqual(module.__name__, f"omnicrawl.state.{module_name}")
                self.assertEqual(
                    Path(module.__file__).resolve(),
                    state_path / f"{module_name}.py",
                )

    def test_session_module_keeps_compatibility_exports(self) -> None:
        expected_exports = (
            "COMPACT_SUMMARY_PREFIX",
            "PromptHistoryEntry",
            "PromptHistoryStore",
            "SessionEvent",
            "SessionIndexEntry",
            "SessionState",
            "SessionStore",
            "SessionStoreError",
        )

        for export_name in expected_exports:
            with self.subTest(export=export_name):
                self.assertTrue(hasattr(session_module, export_name))
