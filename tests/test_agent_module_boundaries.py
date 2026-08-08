from __future__ import annotations

import importlib
import unittest
from pathlib import Path

import omnicrawl.agent as agent_package


class AgentModuleBoundaryTests(unittest.TestCase):
    """确保 Agent 子系统使用真实模块，而不是指向包入口的动态别名。"""

    MODULE_NAMES = (
        "types",
        "environment",
        "history",
        "tools",
        "host_tools",
        "image_tools",
        "vision_proxy",
        "execution",
        "approval_policy",
        "windows_desktop",
        "llm_protocol",
        "memory_tools",
        "prompt_context",
        "session_facade",
        "core",
    )

    def test_agent_submodules_are_real_files(self) -> None:
        package_path = Path(agent_package.__file__).resolve().parent

        for module_name in self.MODULE_NAMES:
            with self.subTest(module=module_name):
                module = importlib.import_module(f"omnicrawl.agent.{module_name}")
                self.assertIsNot(module, agent_package)
                self.assertEqual(module.__name__, f"omnicrawl.agent.{module_name}")
                self.assertEqual(
                    Path(module.__file__).resolve(),
                    package_path / f"{module_name}.py",
                )

    def test_subagent_modules_are_real_internal_files(self) -> None:
        package_path = Path(agent_package.__file__).resolve().parent / "subagents"
        for module_name in ("definitions", "coordinator"):
            with self.subTest(module=module_name):
                module = importlib.import_module(f"omnicrawl.agent.subagents.{module_name}")
                self.assertEqual(
                    Path(module.__file__).resolve(),
                    package_path / f"{module_name}.py",
                )

    def test_package_keeps_public_and_compatibility_exports(self) -> None:
        expected_exports = (
            "AgentConfig",
            "AgentError",
            "AgentLLMProtocol",
            "AgentModelReply",
            "LocalToolAgent",
            "ToolCall",
            "ToolDefinition",
            "ToolResult",
        )

        for export_name in expected_exports:
            with self.subTest(export=export_name):
                self.assertTrue(hasattr(agent_package, export_name))
