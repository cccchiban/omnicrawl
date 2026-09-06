from __future__ import annotations

import importlib
import unittest
from pathlib import Path

import omnicrawl.mcp as mcp_package


class MCPModuleBoundaryTests(unittest.TestCase):
    """确保 MCP 子系统使用真实模块，而不是指向包入口的动态别名。"""

    MODULE_NAMES = ("registry", "config", "security", "audit", "client")

    def test_mcp_submodules_are_real_files(self) -> None:
        package_path = Path(mcp_package.__file__).resolve().parent

        for module_name in self.MODULE_NAMES:
            with self.subTest(module=module_name):
                module = importlib.import_module(f"omnicrawl.mcp.{module_name}")
                self.assertIsNot(module, mcp_package)
                self.assertEqual(module.__name__, f"omnicrawl.mcp.{module_name}")
                self.assertEqual(
                    Path(module.__file__).resolve(),
                    package_path / f"{module_name}.py",
                )

    def test_package_keeps_public_exports(self) -> None:
        for export_name in mcp_package.__all__:
            with self.subTest(export=export_name):
                self.assertTrue(hasattr(mcp_package, export_name))
