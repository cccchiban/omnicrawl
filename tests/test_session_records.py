from __future__ import annotations

import json
import tempfile
import unittest
from pathlib import Path

from omnicrawl.session import SessionStore
from omnicrawl.state import session_records


class SessionRecordsTest(unittest.TestCase):
    def _start_store(self, temp_dir: str) -> tuple[Path, SessionStore, str, Path]:
        workspace = Path(temp_dir) / "workspace"
        workspace.mkdir()
        store = SessionStore(workspace / ".agent_sessions")
        state = store.start_session(workspace)
        store.append_event(state.session_id, "user_message", {"content": "正常消息"})
        store.append_event(state.session_id, "assistant_message", {"content": "正常回复"})
        return workspace, store, state.session_id, state.path

    def test_current_version_events_read_without_diagnostics(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            _workspace, store, session_id, _path = self._start_store(temp_dir)
            result = store.read_session_events_with_diagnostics(session_id)

        self.assertEqual(len(result.events), 3)
        self.assertEqual(result.diagnostics, ())
        self.assertFalse(result.has_errors)

    def test_legacy_v0_and_unversioned_events_migrate_in_memory(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            _workspace, store, session_id, path = self._start_store(temp_dir)
            lines = path.read_text(encoding="utf-8").splitlines()
            # 在现有转录中插入 v0 与无 version 的遗留事件，模拟历史 fixture。
            v0_event = {
                "version": 0,
                "session_id": session_id,
                "id": "c" * 24,
                "parent_id": None,
                "type": "user_message",
                "created_at": "2026-07-12T16:00:00+00:00",
                "payload": {"content": "旧版 v0 消息"},
            }
            unversioned = {
                "session_id": session_id,
                "event_id": "d" * 24,
                "parent_id": None,
                "type": "assistant_message",
                "created_at": "2026-07-12T16:01:00+00:00",
                "payload": {"content": "无 version 遗留回复"},
            }
            path.write_text(
                "\n".join(lines + [json.dumps(v0_event), json.dumps(unversioned)]) + "\n",
                encoding="utf-8",
            )

            result = store.read_session_events_with_diagnostics(session_id)
            disk_text = path.read_text(encoding="utf-8")
            restored = store.load_session(session_id)

        self.assertEqual(len(result.events), 5)
        migration_codes = [item.code for item in result.diagnostics]
        self.assertEqual(migration_codes.count(session_records.DIAG_LEGACY_MIGRATED), 2)
        self.assertTrue(all(event.version == 1 for event in result.events))
        self.assertIn("旧版 v0 消息", [msg["content"] for msg in restored.messages])
        self.assertIn("无 version 遗留回复", [msg["content"] for msg in restored.messages])
        # 磁盘原始转录不得被隐式改写。
        self.assertIn('"version": 0', disk_text)
        self.assertNotIn('"version": 0', json.dumps(result.events[-2].to_dict()))

    def test_unsupported_version_is_diagnosed_not_silently_dropped(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            _workspace, store, session_id, path = self._start_store(temp_dir)
            future = {
                "version": 99,
                "session_id": session_id,
                "event_id": "e" * 24,
                "parent_id": None,
                "type": "user_message",
                "created_at": "2026-07-12T17:00:00+00:00",
                "payload": {"content": "未来版本"},
            }
            path.write_text(
                path.read_text(encoding="utf-8") + json.dumps(future) + "\n",
                encoding="utf-8",
            )

            result = store.read_session_events_with_diagnostics(session_id)

        self.assertEqual(len(result.events), 3)
        self.assertTrue(result.has_errors)
        unsupported = [item for item in result.diagnostics if item.code == session_records.DIAG_UNSUPPORTED_VERSION]
        self.assertEqual(len(unsupported), 1)
        self.assertEqual(unsupported[0].line_no, 4)
        self.assertEqual(unsupported[0].severity, session_records.SEVERITY_ERROR)
        self.assertIn("99", unsupported[0].message)

    def test_middle_corruption_and_trailing_incomplete_are_distinguished(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            _workspace, store, session_id, path = self._start_store(temp_dir)
            good_lines = path.read_text(encoding="utf-8").splitlines()
            # 中间损坏 + 尾部半行（无结尾换行）。
            content = (
                good_lines[0]
                + "\n"
                + "{not-json, api_key=TOP_SECRET\n"
                + good_lines[1]
                + "\n"
                + good_lines[2]
                + "\n"
                + "{\"version\":1,\"session_id\":"
            )
            path.write_text(content, encoding="utf-8")

            result = store.read_session_events_with_diagnostics(session_id)
            codes = {item.code: item for item in result.diagnostics}

        self.assertEqual(len(result.events), 3)
        self.assertIn(session_records.DIAG_INVALID_JSON, codes)
        self.assertIn(session_records.DIAG_TRAILING_INCOMPLETE, codes)
        self.assertEqual(codes[session_records.DIAG_INVALID_JSON].severity, session_records.SEVERITY_ERROR)
        self.assertEqual(codes[session_records.DIAG_TRAILING_INCOMPLETE].severity, session_records.SEVERITY_WARNING)
        self.assertIsNotNone(codes[session_records.DIAG_INVALID_JSON].line_no)
        # 诊断 snippet 必须脱敏，避免日志/API 二次泄露。
        self.assertNotIn("TOP_SECRET", json.dumps(codes[session_records.DIAG_INVALID_JSON].to_dict()))

    def test_prompt_history_diagnostics_for_corrupt_and_trailing_lines(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            workspace, store, session_id, _path = self._start_store(temp_dir)
            store.append_prompt_history(
                display="正常提示",
                workspace_root=workspace,
                session_id=session_id,
            )
            history_path = workspace / ".agent_sessions" / "history.jsonl"
            history_path.write_text(
                history_path.read_text(encoding="utf-8")
                + "{bad prompt api_key=PROMPT_SECRET\n"
                + "{\"display\":\"半行",
                encoding="utf-8",
            )

            diagnostics = store.read_prompt_history_diagnostics()
            search_results = store.search_prompt_history(workspace_root=workspace)

        codes = {item.code for item in diagnostics}
        self.assertIn(session_records.DIAG_INVALID_JSON, codes)
        self.assertIn(session_records.DIAG_TRAILING_INCOMPLETE, codes)
        self.assertEqual([entry.display for entry in search_results], ["正常提示"])
        payload = json.dumps([item.to_dict() for item in diagnostics])
        self.assertNotIn("PROMPT_SECRET", payload)

    def test_index_write_includes_schema_version_and_reads_legacy_without_it(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            workspace, store, session_id, _path = self._start_store(temp_dir)
            index_path = workspace / ".agent_sessions" / "index.json"
            data = json.loads(index_path.read_text(encoding="utf-8"))
            self.assertEqual(data["schema_version"], session_records.SESSION_INDEX_SCHEMA_VERSION)

            # 模拟旧 index：去掉 schema_version 后仍应可读。
            del data["schema_version"]
            index_path.write_text(json.dumps(data, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
            sessions = store.list_sessions(workspace_root=workspace)
            self.assertEqual(sessions[0].session_id, session_id)

            # 再次写入会补上 schema_version。
            store.rename_session(session_id, "补版本后的标题")
            rewritten = json.loads(index_path.read_text(encoding="utf-8"))
            self.assertEqual(rewritten["schema_version"], 1)
            self.assertEqual(rewritten["sessions"][0]["title"], "补版本后的标题")


class SessionRecordsMigrationUnitTest(unittest.TestCase):
    def test_migrate_event_dict_rejects_unknown_future_version(self) -> None:
        with self.assertRaisesRegex(Exception, "暂不支持的会话事件版本：42"):
            session_records.migrate_event_dict(
                {
                    "version": 42,
                    "session_id": "20260712-120000-abcdef",
                    "event_id": "a" * 24,
                    "type": "user_message",
                    "created_at": "2026-07-12T12:00:00+00:00",
                    "payload": {},
                }
            )


if __name__ == "__main__":
    unittest.main()
