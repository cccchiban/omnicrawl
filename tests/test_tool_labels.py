from __future__ import annotations

import unittest

from omnicrawl.ui.tool_labels import (
    format_duration,
    format_tool_status,
    tool_display,
)


class ToolLabelsTest(unittest.TestCase):
    def test_builtin_and_mcp_tools_use_original_names(self) -> None:
        """工具卡标题显示英文原名：内置工具显示注册名，MCP 工具保留完整
        命名空间原名，图标映射保持既有语义。"""

        self.assertEqual(tool_display("list").icon, "L")
        self.assertEqual(tool_display("find").icon, "F")
        self.assertEqual(tool_display("read").icon, "R")
        self.assertEqual(tool_display("bash").icon, "B")
        self.assertEqual(tool_display("trusted.bash").icon, "B")
        self.assertEqual(tool_display("powershell").icon, "P")
        self.assertEqual(tool_display("trusted.powershell").icon, "P")
        self.assertEqual(tool_display("read").name, "read")
        self.assertEqual(
            tool_display("trusted.read").name,
            "trusted.read",
        )
        self.assertEqual(
            tool_display("mcp_read_resource__local_project:project://README.md").name,
            "mcp_read_resource__local_project:project://README.md",
        )
        self.assertEqual(
            tool_display("mcp_get_prompt__local_project.code_review").name,
            "mcp_get_prompt__local_project.code_review",
        )

    def test_unknown_tool_keeps_internal_name_for_diagnostics(self) -> None:
        self.assertEqual(
            tool_display("custom.server.unknown_tool").name,
            "custom.server.unknown_tool",
        )

    def test_duration_and_status_are_adaptive(self) -> None:
        self.assertEqual(format_duration(0), "0ms")
        self.assertEqual(format_duration(0.126), "126ms")
        self.assertEqual(format_duration(1.26), "1.3s")
        self.assertEqual(format_duration(68), "1m 08s")
        self.assertEqual(format_tool_status("成功").icon, "✓")
        self.assertEqual(format_tool_status("调用中").icon, "…")


if __name__ == "__main__":
    unittest.main()
