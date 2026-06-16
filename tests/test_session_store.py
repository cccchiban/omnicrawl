from __future__ import annotations

import json
import tempfile
import unittest
from datetime import datetime, timezone
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
            store.append_event(
                state.session_id,
                "tool_result",
                {"tool": "read_file", "ok": True, "output": "README"},
            )
            store.append_event(state.session_id, "assistant_message", {"content": "第一轮回答"})
            restored = store.load_session(state.session_id)

            self.assertEqual(
                restored.messages,
                [
                    {"role": "user", "content": "第一轮问题"},
                    {"role": "assistant", "content": "工具执行结果：read_file 成功\nREADME"},
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

    def test_compact_summary_restores_summary_and_recent_window(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            workspace = Path(temp_dir) / "workspace"
            workspace.mkdir()
            store = SessionStore(workspace / ".agent_sessions")
            state = store.start_session(workspace)

            store.append_event(state.session_id, "user_message", {"content": "第一轮问题"})
            store.append_event(state.session_id, "assistant_message", {"content": "第一轮回答"})
            store.append_event(state.session_id, "user_message", {"content": "第二轮问题"})
            store.append_event(state.session_id, "assistant_message", {"content": "第二轮回答"})
            store.append_event(
                state.session_id,
                "compact_summary",
                {
                    "content": "早期两轮已经压缩。",
                    "compacted_message_count": 2,
                    "remaining_message_count": 2,
                    "manual": False,
                },
            )
            store.append_event(state.session_id, "user_message", {"content": "第三轮问题"})
            restored = store.load_session(state.session_id)

        self.assertEqual(
            restored.messages,
            [
                {"role": "assistant", "content": "会话压缩摘要：\n早期两轮已经压缩。"},
                {"role": "user", "content": "第二轮问题"},
                {"role": "assistant", "content": "第二轮回答"},
                {"role": "user", "content": "第三轮问题"},
            ],
        )

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

    def test_rename_session_updates_index_and_writes_event(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            workspace = Path(temp_dir) / "workspace"
            workspace.mkdir()
            store = SessionStore(workspace / ".agent_sessions")
            state = store.start_session(workspace)

            renamed = store.rename_session(state.session_id, "  Qt 会话列表  ")
            events = [
                json.loads(line)
                for line in state.path.read_text(encoding="utf-8").splitlines()
                if line.strip()
            ]
            sessions = store.list_sessions(workspace_root=workspace)

        self.assertEqual(renamed.title, "Qt 会话列表")
        self.assertEqual(sessions[0].title, "Qt 会话列表")
        self.assertEqual(events[-1]["type"], "session_renamed")
        self.assertEqual(events[-1]["payload"]["title"], "Qt 会话列表")

    def test_export_session_markdown_writes_exports_file_and_event(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            workspace = Path(temp_dir) / "workspace"
            workspace.mkdir()
            store = SessionStore(workspace / ".agent_sessions")
            state = store.start_session(workspace)

            path = store.export_session_markdown(
                state.session_id,
                "# AI Voice Agent 对话记录\n",
                now=datetime(2026, 6, 16, 9, 30, 5, tzinfo=timezone.utc),
            )
            saved_text = path.read_text(encoding="utf-8")
            events = [
                json.loads(line)
                for line in state.path.read_text(encoding="utf-8").splitlines()
                if line.strip()
            ]

        self.assertEqual(path.parent.name, "exports")
        self.assertEqual(path.parent.parent.name, ".agent_sessions")
        self.assertIn(state.session_id, path.name)
        self.assertEqual(saved_text, "# AI Voice Agent 对话记录\n")
        self.assertEqual(events[-1]["type"], "session_exported")
        self.assertEqual(events[-1]["payload"]["format"], "markdown")
        self.assertTrue(events[-1]["payload"]["path"].startswith("exports/"))

    def test_archive_session_hides_from_default_list_and_resume_unarchives(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            workspace = Path(temp_dir) / "workspace"
            workspace.mkdir()
            store = SessionStore(workspace / ".agent_sessions")
            state = store.start_session(workspace)
            store.append_event(state.session_id, "user_message", {"content": "归档前问题"})
            store.append_event(state.session_id, "assistant_message", {"content": "归档前回答"})

            archived = store.archive_session(
                state.session_id,
                now=datetime(2026, 6, 16, 10, 0, tzinfo=timezone.utc),
            )
            default_sessions = store.list_sessions(workspace_root=workspace)
            archived_sessions = store.list_sessions(workspace_root=workspace, archived_only=True)
            archived_events = [
                json.loads(line)
                for line in archived.path.read_text(encoding="utf-8").splitlines()
                if line.strip()
            ]
            unarchived = store.unarchive_session(
                state.session_id,
                now=datetime(2026, 6, 16, 10, 5, tzinfo=timezone.utc),
            )
            default_after_resume = store.list_sessions(workspace_root=workspace)

        self.assertEqual(archived.path.parent.name, "archive")
        self.assertIsNotNone(archived.archived_at)
        self.assertFalse((workspace / ".agent_sessions" / "sessions" / f"{state.session_id}.jsonl").exists())
        self.assertEqual(default_sessions, [])
        self.assertEqual([entry.session_id for entry in archived_sessions], [state.session_id])
        self.assertEqual(archived_events[-1]["type"], "session_archived")
        self.assertEqual(unarchived.path.parent.name, "sessions")
        self.assertIsNone(unarchived.archived_at)
        self.assertEqual([entry.session_id for entry in default_after_resume], [state.session_id])
        self.assertEqual(unarchived.messages[-1]["content"], "归档前回答")

    def test_large_tool_result_writes_artifact_and_restores_summary_context(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            workspace = Path(temp_dir) / "workspace"
            workspace.mkdir()
            store = SessionStore(workspace / ".agent_sessions")
            state = store.start_session(workspace)
            large_output = "A" * 9000 + "api_key=secret-value" + "Z" * 9000

            store.append_event(
                state.session_id,
                "tool_result",
                {
                    "tool": "run_command",
                    "ok": True,
                    "output": large_output,
                    "model_output": "模型可见截断输出",
                },
            )
            events = [
                json.loads(line)
                for line in state.path.read_text(encoding="utf-8").splitlines()
                if line.strip()
            ]
            payload = events[-1]["payload"]
            artifact_path = workspace / ".agent_sessions" / payload["artifact_path"]
            artifact_exists = artifact_path.is_file()
            artifact_text = artifact_path.read_text(encoding="utf-8")
            restored = store.load_session(state.session_id)

        self.assertEqual(payload["storage"], "artifact")
        self.assertEqual(payload["output"], payload["output"].strip())
        self.assertIn("字符数", payload["output"])
        self.assertIn("output_preview", payload)
        self.assertIn("model_output", payload)
        self.assertTrue(artifact_exists)
        self.assertNotIn("secret-value", artifact_text)
        self.assertIn("api_key=***", artifact_text)
        self.assertEqual(
            restored.messages[-1]["content"],
            "工具执行结果：run_command 成功\n模型可见截断输出\n"
            f"完整输出 artifact：{payload['artifact_path']}",
        )

    def test_sensitive_tool_arguments_are_redacted_in_session_events(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            workspace = Path(temp_dir) / "workspace"
            workspace.mkdir()
            store = SessionStore(workspace / ".agent_sessions")
            state = store.start_session(workspace)

            store.append_event(
                state.session_id,
                "tool_call_requested",
                {
                    "tool": "demo",
                    "arguments": {
                        "api_key": "secret",
                        "nested": {"token": "abc"},
                        "text": "Authorization: Bearer abcdefghijklmnop",
                    },
                },
            )
            event = json.loads(state.path.read_text(encoding="utf-8").splitlines()[-1])
            restored = store.load_session(state.session_id)

        self.assertEqual(event["payload"]["arguments"]["api_key"], "***")
        self.assertEqual(event["payload"]["arguments"]["nested"]["token"], "***")
        self.assertIn("Bearer ***", event["payload"]["arguments"]["text"])
        self.assertIn('"api_key": "***"', restored.messages[-1]["content"])


if __name__ == "__main__":
    unittest.main()
