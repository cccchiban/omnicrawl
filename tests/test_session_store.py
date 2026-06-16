from __future__ import annotations

import json
import tempfile
import unittest
from pathlib import Path

from ai_voice_agent.session import SessionStore, SessionStoreError


class SessionStoreTest(unittest.TestCase):
    def test_start_session_creates_index_and_jsonl_transcript(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            workspace = Path(temp_dir) / "workspace"
            workspace.mkdir()
            store = SessionStore(workspace / ".agent_sessions")

            state = store.start_session(workspace)

            self.assertRegex(state.session_id, r"^\d{8}-\d{6}-[a-f0-9]{6}$")
            self.assertTrue((workspace / ".agent_sessions" / "index.json").is_file())
            self.assertTrue(state.path.is_file())
            lines = state.path.read_text(encoding="utf-8").splitlines()
            self.assertEqual(len(lines), 1)
            event = json.loads(lines[0])
            self.assertEqual(event["type"], "session_started")
            self.assertEqual(event["session_id"], state.session_id)

    def test_append_event_updates_index_and_restore_messages(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            workspace = Path(temp_dir) / "workspace"
            workspace.mkdir()
            store = SessionStore(workspace / ".agent_sessions")
            state = store.start_session(workspace)

            store.append_event(state.session_id, "user_message", {"content": "第一轮问题"})
            store.append_event(state.session_id, "tool_result", {"tool": "read_file", "output": "README"})
            store.append_event(state.session_id, "assistant_message", {"content": "第一轮回答"})
            restored = store.load_session(state.session_id)

            self.assertEqual(
                restored.messages,
                [
                    {"role": "user", "content": "第一轮问题"},
                    {"role": "assistant", "content": "第一轮回答"},
                ],
            )
            sessions = store.list_sessions(workspace_root=workspace)
            self.assertEqual(len(sessions), 1)
            self.assertEqual(sessions[0].message_count, 2)
            self.assertEqual(sessions[0].last_event_type, "assistant_message")
            self.assertEqual(sessions[0].title, "第一轮问题")

    def test_rejects_invalid_session_id(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            workspace = Path(temp_dir) / "workspace"
            workspace.mkdir()
            store = SessionStore(workspace / ".agent_sessions")
            store.start_session(workspace)

            with self.assertRaisesRegex(SessionStoreError, "session_id 格式无效"):
                store.load_session("../bad")

    def test_prompt_history_appends_searches_and_deduplicates_by_project(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            workspace = Path(temp_dir) / "workspace"
            other_workspace = Path(temp_dir) / "other"
            workspace.mkdir()
            other_workspace.mkdir()
            store = SessionStore(workspace / ".agent_sessions")
            state = store.start_session(workspace)
            other_state = store.start_session(other_workspace)

            store.append_prompt_history(
                display="帮我实现会话历史",
                workspace_root=workspace,
                session_id=state.session_id,
            )
            store.append_prompt_history(
                display="帮我实现会话历史",
                workspace_root=workspace,
                session_id=state.session_id,
            )
            store.append_prompt_history(
                display="检查 Qt 会话列表",
                workspace_root=workspace,
                session_id=state.session_id,
            )
            store.append_prompt_history(
                display="其他项目提示",
                workspace_root=other_workspace,
                session_id=other_state.session_id,
            )

            history_path = workspace / ".agent_sessions" / "history.jsonl"
            all_lines = [line for line in history_path.read_text(encoding="utf-8").splitlines() if line]
            results = store.search_prompt_history(workspace_root=workspace, query="会话")

        self.assertEqual(len(all_lines), 4)
        self.assertEqual([entry.display for entry in results], ["检查 Qt 会话列表", "帮我实现会话历史"])
        self.assertTrue(all(entry.project == str(workspace.resolve()) for entry in results))


if __name__ == "__main__":
    unittest.main()
