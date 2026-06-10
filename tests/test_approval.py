from __future__ import annotations

import json
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from ai_voice_agent.agent import LocalToolAgent, ToolDefinition, ToolCall
from ai_voice_agent.approval import (
    APPROVAL_MODE_AUTO,
    APPROVAL_MODE_MANUAL,
    APPROVAL_MODE_REVIEW,
    load_approval_mode,
    normalize_approval_mode,
    save_approval_mode,
)
from ai_voice_agent.slash_commands import handle_approval_command


class ApprovalConfigTest(unittest.TestCase):
    def test_load_approval_mode_defaults_to_manual(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            config_path = Path(temp_dir) / "config.json"

            self.assertEqual(load_approval_mode(config_path), APPROVAL_MODE_MANUAL)

    def test_load_approval_mode_supports_aliases_and_legacy_flags(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            config_path = Path(temp_dir) / "config.json"
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
            config_path = Path(temp_dir) / "config.json"
            config_path.write_text(
                json.dumps({"llm": {"model": "demo"}, "voice": {"speech_to_text_enabled": True}}),
                encoding="utf-8",
            )

            save_approval_mode(APPROVAL_MODE_REVIEW, config_path)
            data = json.loads(config_path.read_text(encoding="utf-8"))

            self.assertEqual(data["approval"]["mode"], APPROVAL_MODE_REVIEW)
            self.assertEqual(data["llm"]["model"], "demo")
            self.assertTrue(data["voice"]["speech_to_text_enabled"])

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
            "ai_voice_agent.slash_commands.save_approval_mode",
            return_value=Path("config.json"),
        ) as save_mode:
            message = handle_approval_command(agent, "/approval:auto")

        self.assertEqual(agent.approval_mode, APPROVAL_MODE_AUTO)
        save_mode.assert_called_once_with(APPROVAL_MODE_AUTO)
        self.assertIn("完全自动批准", message or "")

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

    def test_parse_tool_call_accepts_missing_close_tag(self) -> None:
        agent = object.__new__(LocalToolAgent)

        tool_call = LocalToolAgent._parse_tool_call(
            agent,
            '<tool>{"name":"bb-browser.browser.evaluate","arguments":{"tab":"9404","script":"document.title"}}',
        )

        self.assertIsNotNone(tool_call)
        assert tool_call is not None
        self.assertEqual(tool_call.name, "bb-browser.browser.evaluate")
        self.assertEqual(tool_call.arguments["tab"], "9404")

    def test_parse_tool_call_keeps_plain_text_as_final_answer(self) -> None:
        agent = object.__new__(LocalToolAgent)

        tool_call = LocalToolAgent._parse_tool_call(
            agent,
            '我会说明一下 <tool>{"name":"read_file","arguments":{"path":"main.py"}}',
        )

        self.assertIsNone(tool_call)

    def test_parse_tool_call_still_accepts_raw_json_payload(self) -> None:
        agent = object.__new__(LocalToolAgent)

        tool_call = LocalToolAgent._parse_tool_call(
            agent,
            '{"name":"read_file","arguments":{"path":"main.py"}}',
        )

        self.assertIsNotNone(tool_call)
        assert tool_call is not None
        self.assertEqual(tool_call.name, "read_file")
        self.assertEqual(tool_call.arguments["path"], "main.py")

    def test_parse_tool_call_extracts_first_tool_from_chained_output(self) -> None:
        agent = object.__new__(LocalToolAgent)

        tool_call = LocalToolAgent._parse_tool_call(
            agent,
            (
                '  <tool>{"name":"bb-browser.browser.open",'
                '"arguments":{"url":"https://chiban.fyi/"}}</tool>'
                '<tool>{"name":"bb-browser.browser.status","arguments":{}}</tool>'
            ),
        )

        self.assertIsNotNone(tool_call)
        assert tool_call is not None
        self.assertEqual(tool_call.name, "bb-browser.browser.open")
        self.assertEqual(tool_call.arguments["url"], "https://chiban.fyi/")

    def test_parse_tool_call_accepts_newline_open_tag_and_trailing_tool(self) -> None:
        agent = object.__new__(LocalToolAgent)

        tool_call = LocalToolAgent._parse_tool_call(
            agent,
            (
                '<tool\n>{"name":"readfile","arguments":'
                '{"path":"README.md","startline":1,"maxlines":20}}'
                '<tool>{"name":"runcommand","arguments":{"command":"echo nope"}}</tool>'
            ),
        )

        self.assertIsNotNone(tool_call)
        assert tool_call is not None
        self.assertEqual(tool_call.name, "read_file")
        self.assertEqual(tool_call.arguments["startline"], 1)

    def test_parse_tool_call_repairs_missing_outer_brace_before_close_tag(self) -> None:
        agent = object.__new__(LocalToolAgent)

        tool_call = LocalToolAgent._parse_tool_call(
            agent,
            (
                '<tool>{"name":"runcommand","arguments":'
                '{"command":"echo hi","timeoutseconds":30}</tool>'
                '<tool>{"name":"read_file","arguments":{"path":"README.md"}}</tool>'
            ),
        )

        self.assertIsNotNone(tool_call)
        assert tool_call is not None
        self.assertEqual(tool_call.name, "run_command")
        self.assertEqual(tool_call.arguments["command"], "echo hi")
        self.assertEqual(tool_call.arguments["timeoutseconds"], 30)

    def test_normalize_tool_call_accepts_common_tool_and_argument_aliases(self) -> None:
        agent = object.__new__(LocalToolAgent)
        agent._tools = {
            "read_file": ToolDefinition(
                name="read_file",
                description="读取文件。",
                argument_schema='{"path":"main.py","start_line":1,"max_lines":200}',
                requires_confirmation=True,
                run=lambda _arguments: None,  # type: ignore[arg-type,return-value]
            ),
            "bb-browser.browser.tab_list": ToolDefinition(
                name="bb-browser.browser.tab_list",
                description="列出标签页。",
                argument_schema='{}',
                requires_confirmation=False,
                run=lambda _arguments: None,  # type: ignore[arg-type,return-value]
            ),
        }

        read_call = LocalToolAgent._normalize_tool_call(
            agent,
            ToolCall(
                name="readfile",
                arguments={"path": "README.md", "startline": 2, "maxlines": 30},
            ),
        )
        tab_call = LocalToolAgent._normalize_tool_call(
            agent,
            ToolCall(name="bb-browser.browser.tablist", arguments={}),
        )

        self.assertEqual(read_call.name, "read_file")
        self.assertEqual(read_call.arguments["start_line"], 2)
        self.assertEqual(read_call.arguments["max_lines"], 30)
        self.assertEqual(tab_call.name, "bb-browser.browser.tab_list")

    def test_review_mode_skips_non_delete_tool_calls(self) -> None:
        tool = ToolDefinition(
            name="run_command",
            description="以工作区为当前目录执行任意本地 command、脚本或 shell 片段。",
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

    def test_review_mode_detects_delete_commands(self) -> None:
        tool = ToolDefinition(
            name="run_command",
            description="以工作区为当前目录执行任意本地命令、脚本或 shell 片段。",
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

    def test_review_mode_detects_cmd_and_script_delete_commands(self) -> None:
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

    def test_review_mode_detects_delete_like_mcp_tools(self) -> None:
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

    def test_review_mode_detects_camel_case_delete_like_mcp_tools(self) -> None:
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

    def test_review_mode_skips_generic_mcp_description_without_delete_arguments(self) -> None:
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


if __name__ == "__main__":
    unittest.main()
