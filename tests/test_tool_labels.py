from __future__ import annotations

import unittest

from omnicrawl.ui.tool_labels import (
    format_duration,
    format_tool_status,
    tool_display,
)


class ToolLabelsTest(unittest.TestCase):
    def test_builtin_and_mcp_tools_use_friendly_labels(self) -> None:
        self.assertEqual(tool_display("list_files").icon, "L")
        self.assertEqual(tool_display("find_files").icon, "F")
        self.assertEqual(tool_display("read_file").icon, "R")
        self.assertEqual(tool_display("bash").icon, "B")
        self.assertEqual(tool_display("trusted.bash").icon, "B")
        self.assertEqual(tool_display("powershell").icon, "P")
        self.assertEqual(tool_display("trusted.powershell").icon, "P")
        self.assertEqual(tool_display("read_file").name, "读取文件")
        self.assertEqual(
            tool_display("trusted.read_file").name,
            "读取文件",
        )
        self.assertEqual(
            tool_display("mcp_read_resource__local_project:project://README.md").name,
            "读取资源",
        )
        self.assertEqual(
            tool_display("mcp_get_prompt__local_project.code_review").name,
            "获取提示词",
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
