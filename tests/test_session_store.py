from __future__ import annotations

import json
import tempfile
import threading
import unittest
from datetime import datetime, timezone
from pathlib import Path

from omnicrawl.session import SessionStore, SessionStoreError


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
                display="检查 API 会话列表",
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
        self.assertEqual([entry.display for entry in results], ["检查 API 会话列表", "帮我实现会话历史"])
        self.assertTrue(all(entry.project == str(workspace.resolve()) for entry in results))

    def test_list_sessions_filters_by_project_path(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            workspace = Path(temp_dir) / "workspace"
            other_workspace = Path(temp_dir) / "other"
            workspace.mkdir()
            other_workspace.mkdir()
            store = SessionStore(workspace / ".agent_sessions")
            state = store.start_session(workspace)
            other_state = store.start_session(other_workspace)

            workspace_sessions = store.list_sessions(project_path=workspace)
            other_sessions = store.list_sessions(project_path=str(other_workspace))
            project_paths = store.list_project_paths()

        self.assertEqual([entry.session_id for entry in workspace_sessions], [state.session_id])
        self.assertEqual([entry.session_id for entry in other_sessions], [other_state.session_id])
        self.assertEqual(
            set(project_paths),
            {str(workspace.resolve()), str(other_workspace.resolve())},
        )

    def test_rename_session_updates_index_and_writes_event(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            workspace = Path(temp_dir) / "workspace"
            workspace.mkdir()
            store = SessionStore(workspace / ".agent_sessions")
            state = store.start_session(workspace)

            renamed = store.rename_session(state.session_id, "  API 会话列表  ")
            events = [
                json.loads(line)
                for line in state.path.read_text(encoding="utf-8").splitlines()
                if line.strip()
            ]
            sessions = store.list_sessions(workspace_root=workspace)

        self.assertEqual(renamed.title, "API 会话列表")
        self.assertEqual(sessions[0].title, "API 会话列表")
        self.assertEqual(events[-1]["type"], "session_renamed")
        self.assertEqual(events[-1]["payload"]["title"], "API 会话列表")

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

    def test_html_artifact_redacts_sensitive_content_before_persisting(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            workspace = Path(temp_dir) / "workspace"
            workspace.mkdir()
            store = SessionStore(workspace / ".agent_sessions")
            state = store.start_session(workspace)
            html = (
                '<section data-api-key="attribute-secret">api_key=text-secret</section>'
                '<script>const config = {"token": "script-secret"}; '
                'const auth = "Bearer abcdefghijklmnop";</script>'
            )

            store.append_event(
                state.session_id,
                "tool_result",
                {
                    "tool": "display_html",
                    "ok": True,
                    "ui_artifact": {"type": "html", "title": "预览", "html": html},
                },
            )
            event = json.loads(state.path.read_text(encoding="utf-8").splitlines()[-1])
            artifact = event["payload"]["ui_artifact"]
            artifact_text = store.read_artifact_text(state.session_id, artifact["artifact_path"])

        self.assertNotIn("attribute-secret", artifact_text)
        self.assertNotIn("text-secret", artifact_text)
        self.assertNotIn("script-secret", artifact_text)
        self.assertNotIn("abcdefghijklmnop", artifact_text)
        self.assertIn("***", artifact_text)
        self.assertTrue(artifact["redacted"])

    def test_prompt_history_redacts_sensitive_display_and_nested_pasted_contents(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            workspace = Path(temp_dir) / "workspace"
            workspace.mkdir()
            store = SessionStore(workspace / ".agent_sessions")
            state = store.start_session(workspace)

            entry = store.append_prompt_history(
                display="请使用 api_key=display-secret 和 Bearer abcdefghijklmnop",
                workspace_root=workspace,
                session_id=state.session_id,
                pasted_contents={
                    "api_key": "top-secret",
                    "nested": {"access_token": "nested-secret"},
                    "items": [{"password": "password-secret"}],
                    "keyboard": "ordinary-value",
                },
            )
            saved_text = (workspace / ".agent_sessions" / "history.jsonl").read_text(encoding="utf-8")

        self.assertIsNotNone(entry)
        self.assertNotIn("display-secret", saved_text)
        self.assertNotIn("abcdefghijklmnop", saved_text)
        self.assertNotIn("top-secret", saved_text)
        self.assertNotIn("nested-secret", saved_text)
        self.assertNotIn("password-secret", saved_text)
        self.assertEqual(entry.pasted_contents["keyboard"], "ordinary-value")

    def test_sensitive_key_matching_keeps_unrelated_key_words(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            workspace = Path(temp_dir) / "workspace"
            workspace.mkdir()
            store = SessionStore(workspace / ".agent_sessions")
            state = store.start_session(workspace)

            store.append_event(
                state.session_id,
                "tool_call_requested",
                {
                    "arguments": {
                        "keyboard": "keep-keyboard",
                        "monkey": "keep-monkey",
                        "key_count": 7,
                        "api_key": "hide-api-key",
                        "access_token": "hide-token",
                    }
                },
            )
            event = json.loads(state.path.read_text(encoding="utf-8").splitlines()[-1])
            arguments = event["payload"]["arguments"]

        self.assertEqual(arguments["keyboard"], "keep-keyboard")
        self.assertEqual(arguments["monkey"], "keep-monkey")
        self.assertEqual(arguments["key_count"], 7)
        self.assertEqual(arguments["api_key"], "***")
        self.assertEqual(arguments["access_token"], "***")

    def test_concurrent_event_and_prompt_history_writes_keep_all_records(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            workspace = Path(temp_dir) / "workspace"
            workspace.mkdir()
            store = SessionStore(workspace / ".agent_sessions")
            state = store.start_session(workspace)
            writer_count = 12
            start = threading.Barrier(writer_count)
            errors: list[BaseException] = []

            def append_record(index: int) -> None:
                try:
                    start.wait()
                    store.append_event(
                        state.session_id,
                        "user_message",
                        {"content": f"并发消息 {index}"},
                    )
                    store.append_prompt_history(
                        display=f"并发提示 {index}",
                        workspace_root=workspace,
                        session_id=state.session_id,
                    )
                except BaseException as exc:  # pragma: no cover - asserted below
                    errors.append(exc)

            threads = [threading.Thread(target=append_record, args=(index,)) for index in range(writer_count)]
            for thread in threads:
                thread.start()
            for thread in threads:
                thread.join(timeout=5)

            events = [json.loads(line) for line in state.path.read_text(encoding="utf-8").splitlines() if line]
            index_data = json.loads((workspace / ".agent_sessions" / "index.json").read_text(encoding="utf-8"))
            history = [json.loads(line) for line in (workspace / ".agent_sessions" / "history.jsonl").read_text(encoding="utf-8").splitlines() if line]

        self.assertFalse(errors)
        self.assertTrue(all(not thread.is_alive() for thread in threads))
        self.assertEqual(len(events), writer_count + 1)
        self.assertEqual(index_data["sessions"][0]["event_count"], writer_count + 1)
        self.assertEqual(index_data["sessions"][0]["message_count"], writer_count)
        self.assertEqual(len(history), writer_count)

    def test_archive_session_serializes_file_move_with_concurrent_event_append(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            workspace = Path(temp_dir) / "workspace"
            workspace.mkdir()
            store = SessionStore(workspace / ".agent_sessions")
            state = store.start_session(workspace)
            moved = threading.Event()
            continue_archive = threading.Event()
            original_move = store._move_session_file

            def pause_after_move(entry, destination: str) -> None:
                original_move(entry, destination)
                moved.set()
                self.assertTrue(continue_archive.wait(timeout=5))

            store._move_session_file = pause_after_move  # type: ignore[method-assign]
            archive_thread = threading.Thread(target=store.archive_session, args=(state.session_id,))
            archive_thread.start()
            self.assertTrue(moved.wait(timeout=5))

            append_finished = threading.Event()

            def append_event() -> None:
                store.append_event(state.session_id, "user_message", {"content": "归档中的消息"})
                append_finished.set()

            append_thread = threading.Thread(target=append_event)
            append_thread.start()
            self.assertFalse(append_finished.wait(timeout=0.1))
            continue_archive.set()
            archive_thread.join(timeout=5)
            append_thread.join(timeout=5)
            restored = store.load_session(state.session_id)

        self.assertFalse(archive_thread.is_alive())
        self.assertFalse(append_thread.is_alive())
        self.assertEqual(restored.messages[-1], {"role": "user", "content": "归档中的消息"})

    def test_unarchive_session_serializes_file_move_with_concurrent_event_append(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            workspace = Path(temp_dir) / "workspace"
            workspace.mkdir()
            store = SessionStore(workspace / ".agent_sessions")
            state = store.start_session(workspace)
            store.archive_session(state.session_id)
            moved = threading.Event()
            continue_unarchive = threading.Event()
            original_move = store._move_session_file

            def pause_after_move(entry, destination: str) -> None:
                original_move(entry, destination)
                moved.set()
                self.assertTrue(continue_unarchive.wait(timeout=5))

            store._move_session_file = pause_after_move  # type: ignore[method-assign]
            unarchive_thread = threading.Thread(target=store.unarchive_session, args=(state.session_id,))
            unarchive_thread.start()
            self.assertTrue(moved.wait(timeout=5))

            append_finished = threading.Event()

            def append_event() -> None:
                store.append_event(state.session_id, "user_message", {"content": "恢复中的消息"})
                append_finished.set()

            append_thread = threading.Thread(target=append_event)
            append_thread.start()
            self.assertFalse(append_finished.wait(timeout=0.1))
            continue_unarchive.set()
            unarchive_thread.join(timeout=5)
            append_thread.join(timeout=5)
            restored = store.load_session(state.session_id)

        self.assertFalse(unarchive_thread.is_alive())
        self.assertFalse(append_thread.is_alive())
        self.assertEqual(restored.messages[-1], {"role": "user", "content": "恢复中的消息"})

    def test_read_artifact_text_rejects_artifact_from_another_session(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            workspace = Path(temp_dir) / "workspace"
            workspace.mkdir()
            store = SessionStore(workspace / ".agent_sessions")
            current_state = store.start_session(workspace)
            other_state = store.start_session(workspace)
            other_artifact_dir = workspace / ".agent_sessions" / "artifacts" / other_state.session_id
            other_artifact_dir.mkdir(parents=True)
            other_artifact_path = other_artifact_dir / "html_preview_other.html"
            other_artifact_path.write_text("<h1>其他会话</h1>", encoding="utf-8")

            with self.assertRaisesRegex(SessionStoreError, "artifact 路径必须位于当前会话目录"):
                store.read_artifact_text(
                    current_state.session_id,
                    f"artifacts/{other_state.session_id}/html_preview_other.html",
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
