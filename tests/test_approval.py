from __future__ import annotations

import json
import tempfile
import unittest
from pathlib import Path

import yaml
from unittest.mock import patch

from omnicrawl.agent import LocalToolAgent, ToolDefinition, ToolCall
from omnicrawl.agent.approval_policy import is_shell_command_tool_call
from omnicrawl.agent.tools import normalize_tool_call
from omnicrawl.approval import (
    APPROVAL_MODE_AUTO,
    APPROVAL_MODE_MANUAL,
    APPROVAL_MODE_REVIEW,
    load_approval_mode,
    normalize_approval_mode,
    save_approval_mode,
)
from omnicrawl.slash_commands import handle_approval_command, handle_reasoning_command


class ApprovalConfigTest(unittest.TestCase):
    def test_load_approval_mode_defaults_to_manual(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            config_path = Path(temp_dir) / "config.yaml"

            self.assertEqual(load_approval_mode(config_path), APPROVAL_MODE_MANUAL)

    def test_load_approval_mode_supports_aliases_and_legacy_flags(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            config_path = Path(temp_dir) / "config.yaml"
            config_path.write_text(
                json.dumps({"approval": {"mode": "auto-review"}}),
                encoding="utf-8",
            )

            self.assertEqual(load_approval_mode(config_path), APPROVAL_MODE_REVIEW)

            config_path.write_text(
                json.dumps({"approval": {"auto_approve": True}}),
                encoding="utf-8",
            )
            self.assertEqual(load_approval_mode(config_path), APPROVAL_MODE_AUTO)

    def test_save_approval_mode_preserves_existing_config(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            config_path = Path(temp_dir) / "config.yaml"
            config_path.write_text(
                json.dumps({"llm": {"model": "demo"}, "agent_temp": {"enabled": True}}),
                encoding="utf-8",
            )

            save_approval_mode(APPROVAL_MODE_REVIEW, config_path)
            data = yaml.safe_load(config_path.read_text(encoding="utf-8"))

            self.assertEqual(data["approval"]["mode"], APPROVAL_MODE_REVIEW)
            self.assertEqual(data["llm"]["model"], "demo")
            self.assertTrue(data["agent_temp"]["enabled"])

    def test_normalize_approval_mode_rejects_unknown_value(self) -> None:
        with self.assertRaisesRegex(RuntimeError, "approval.mode"):
            normalize_approval_mode("launch-everything")


class ApprovalCommandTest(unittest.TestCase):
    def test_handle_approval_command_updates_agent_and_config(self) -> None:
        class FakeAgent:
            approval_mode = APPROVAL_MODE_MANUAL

            def set_approval_mode(self, mode: str) -> None:
                self.approval_mode = mode

        agent = FakeAgent()
        with patch(
            "omnicrawl.slash_commands.save_approval_mode",
            return_value=Path("config.yaml"),
        ) as save_mode:
            message = handle_approval_command(agent, "/approval:auto")

        self.assertEqual(agent.approval_mode, APPROVAL_MODE_AUTO)
        save_mode.assert_called_once_with(APPROVAL_MODE_AUTO)
        self.assertIn("完全自动批准", message or "")

    def test_handle_reasoning_command_updates_agent_and_config(self) -> None:
        class FakeAgent:
            reasoning_effort = "none"

            def set_reasoning_effort(self, effort: str) -> str:
                self.reasoning_effort = effort
                return effort

        agent = FakeAgent()
        with patch(
            "omnicrawl.slash_commands.save_reasoning_effort",
            return_value=Path("config.yaml"),
        ) as save_effort:
            message = handle_reasoning_command(agent, "/reasoning high")

        self.assertEqual(agent.reasoning_effort, "high")
        save_effort.assert_called_once_with("high")
        self.assertIn("推理强度已切换为 high", message)

    def test_parse_tool_review_response_accepts_embedded_json(self) -> None:
        approved, reason = LocalToolAgent._parse_tool_review_response(
            '结论：{"approve": true, "reason": "只读搜索"}'
        )

        self.assertTrue(approved)
        self.assertEqual(reason, "只读搜索")

    def test_parse_tool_review_response_rejects_invalid_json(self) -> None:
        approved, reason = LocalToolAgent._parse_tool_review_response("approve")

        self.assertFalse(approved)
        self.assertIn("不是 JSON", reason)

    def test_normalize_tool_call_accepts_common_tool_and_argument_aliases(self) -> None:
        tools = {
            "read_file": ToolDefinition(
                name="read_file",
                description="读取文件。",
                argument_schema='{"path":"main.py","start_line":1,"max_lines":200}',
                requires_confirmation=True,
                run=lambda _arguments: None,  # type: ignore[arg-type,return-value]
            ),
        }

        read_call = normalize_tool_call(
            ToolCall(
                name="readfile",
                arguments={"path": "README.md", "startline": 2, "maxlines": 30},
            ),
            tools,
        )

        self.assertEqual(read_call.name, "read_file")
        self.assertEqual(read_call.arguments["start_line"], 2)
        self.assertEqual(read_call.arguments["max_lines"], 30)

    def test_delete_intent_skips_non_delete_tool_calls(self) -> None:
        tool = ToolDefinition(
            name="powershell",
            description="使用 PowerShell 在工作区执行命令。",
            argument_schema='{"command": "python -m unittest discover"}',
            requires_confirmation=True,
            run=lambda _arguments: None,  # type: ignore[arg-type,return-value]
        )

        self.assertFalse(
            LocalToolAgent._is_delete_behavior_tool_call(
                tool,
                {"command": "python -m unittest discover"},
            )
        )
        self.assertFalse(
            LocalToolAgent._is_delete_behavior_tool_call(
                tool,
                {"command": "npm run clean:build"},
            )
        )

    def test_delete_intent_detects_delete_commands(self) -> None:
        tool = ToolDefinition(
            name="powershell",
            description="使用 PowerShell 在工作区执行命令。",
            argument_schema="{}",
            requires_confirmation=True,
            run=lambda _arguments: None,  # type: ignore[arg-type,return-value]
        )

        self.assertTrue(
            LocalToolAgent._is_delete_behavior_tool_call(
                tool,
                {"command": "Remove-Item -Recurse logs"},
            )
        )
        self.assertTrue(
            LocalToolAgent._is_delete_behavior_tool_call(
                tool,
                {"command": "git rm stale.py"},
            )
        )
        self.assertTrue(
            LocalToolAgent._is_delete_behavior_tool_call(
                tool,
                {"command": "git clean -fd"},
            )
        )
        self.assertTrue(
            LocalToolAgent._is_delete_behavior_tool_call(
                tool,
                {"command": "find . -name '*.tmp' -delete"},
            )
        )

    def test_delete_intent_detects_cmd_and_script_delete_commands(self) -> None:
        tool = ToolDefinition(
            name="demo.shell",
            description="执行 shell 命令。",
            argument_schema='{"cmd": "python -m unittest", "script": "echo ok"}',
            requires_confirmation=True,
            run=lambda _arguments: None,  # type: ignore[arg-type,return-value]
        )

        self.assertTrue(
            LocalToolAgent._is_delete_behavior_tool_call(
                tool,
                {"cmd": "rm -rf logs"},
            )
        )
        self.assertTrue(
            LocalToolAgent._is_delete_behavior_tool_call(
                tool,
                {"request": {"script": "Remove-Item -Recurse logs"}},
            )
        )

    def test_delete_intent_detects_delete_like_mcp_tools(self) -> None:
        tool = ToolDefinition(
            name="demo.file_operation",
            description="Delete a file in the workspace.",
            argument_schema="{}",
            requires_confirmation=True,
            run=lambda _arguments: None,  # type: ignore[arg-type,return-value]
        )

        self.assertTrue(
            LocalToolAgent._is_delete_behavior_tool_call(
                tool,
                {"path": "old.txt"},
            )
        )

    def test_delete_intent_detects_camel_case_delete_like_mcp_tools(self) -> None:
        tool = ToolDefinition(
            name="demo.fileOperation",
            description="removeFile in the workspace.",
            argument_schema="{}",
            requires_confirmation=True,
            run=lambda _arguments: None,  # type: ignore[arg-type,return-value]
        )

        self.assertTrue(
            LocalToolAgent._is_delete_behavior_tool_call(
                tool,
                {"path": "old.txt"},
            )
        )

    def test_delete_intent_skips_generic_mcp_description_without_delete_arguments(self) -> None:
        tool = ToolDefinition(
            name="demo.file_operation",
            description="Create, update, read, or delete files in the workspace.",
            argument_schema="{}",
            requires_confirmation=True,
            run=lambda _arguments: None,  # type: ignore[arg-type,return-value]
        )

        self.assertFalse(
            LocalToolAgent._is_delete_behavior_tool_call(
                tool,
                {"path": "note.txt", "content": "delete 这个词只是正文"},
            )
        )


    def test_auto_review_reviews_bash_and_powershell_commands(self) -> None:
        bash_tool = ToolDefinition(
            name="bash",
            description="使用 Git Bash 执行命令。",
            argument_schema='{"command": "pytest"}',
            requires_confirmation=True,
            run=lambda _arguments: None,  # type: ignore[arg-type,return-value]
        )
        powershell_tool = ToolDefinition(
            name="powershell",
            description="使用 PowerShell 执行命令。",
            argument_schema='{"command": "Get-ChildItem"}',
            requires_confirmation=True,
            run=lambda _arguments: None,  # type: ignore[arg-type,return-value]
        )

        # 无论命令内容是否与删除相关，bash/powershell 命令都要进入自动审查。
        self.assertTrue(is_shell_command_tool_call(bash_tool, {"command": "pytest"}))
        self.assertTrue(
            is_shell_command_tool_call(powershell_tool, {"command": "Get-ChildItem"})
        )

    def test_auto_review_skips_non_shell_tools(self) -> None:
        read_tool = ToolDefinition(
            name="read_file",
            description="读取文件。",
            argument_schema='{"path": "main.py"}',
            requires_confirmation=False,
            run=lambda _arguments: None,  # type: ignore[arg-type,return-value]
        )
        replace_tool = ToolDefinition(
            name="replace_text",
            description="替换文本。",
            argument_schema='{"path": "main.py", "old_text": "a", "new_text": "b"}',
            requires_confirmation=True,
            run=lambda _arguments: None,  # type: ignore[arg-type,return-value]
        )

        self.assertFalse(is_shell_command_tool_call(read_tool, {"path": "main.py"}))
        self.assertFalse(
            is_shell_command_tool_call(
                replace_tool,
                {"path": "main.py", "old_text": "a", "new_text": "b"},
            )
        )

    def test_auto_review_accepts_shell_tool_aliases(self) -> None:
        alias_tool = ToolDefinition(
            name="bashcommand",
            description="执行 shell 命令。",
            argument_schema='{"command": "pwd"}',
            requires_confirmation=True,
            run=lambda _arguments: None,  # type: ignore[arg-type,return-value]
        )

        self.assertTrue(is_shell_command_tool_call(alias_tool, {"command": "pwd"}))


if __name__ == "__main__":
    unittest.main()
