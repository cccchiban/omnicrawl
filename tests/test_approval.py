from __future__ import annotations

import json
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from ai_voice_agent.agent import LocalToolAgent
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


if __name__ == "__main__":
    unittest.main()
