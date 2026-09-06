from __future__ import annotations

import json
import tempfile
import unittest
from datetime import datetime, timezone
from pathlib import Path

from omnicrawl.session import (
    SessionConsistencyReport,
    SessionStore,
    SessionStoreError,
)
from omnicrawl.state import session_consistency


class SessionConsistencyTest(unittest.TestCase):
    def _prepare_store(self, temp_dir: str) -> tuple[Path, SessionStore, str]:
        workspace = Path(temp_dir) / "workspace"
        workspace.mkdir()
        store = SessionStore(workspace / ".agent_sessions")
        state = store.start_session(workspace, title="初始标题")
        store.append_event(state.session_id, "user_message", {"content": "第一轮问题"})
        store.append_event(state.session_id, "assistant_message", {"content": "第一轮回答"})
        store.rename_session(state.session_id, "自定义标题")
        return workspace, store, state.session_id

    def test_check_consistency_ok_for_healthy_store(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            workspace, store, session_id = self._prepare_store(temp_dir)

            report = store.check_consistency()

        self.assertIsInstance(report, SessionConsistencyReport)
        self.assertTrue(report.ok)
        self.assertEqual(report.issues, ())
        self.assertEqual(report.scanned_index_entries, 1)
        self.assertEqual(report.scanned_transcripts, 1)
        self.assertEqual(report.proposed_entries[0].session_id, session_id)
        self.assertEqual(report.proposed_entries[0].title, "自定义标题")
        self.assertEqual(report.proposed_entries[0].workspace_root, str(workspace.resolve()))

    def test_check_detects_event_count_drift_and_rebuild_repairs_index(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            workspace, store, session_id = self._prepare_store(temp_dir)
            index_path = workspace / ".agent_sessions" / "index.json"
            data = json.loads(index_path.read_text(encoding="utf-8"))
            data["sessions"][0]["event_count"] = 1
            data["sessions"][0]["message_count"] = 0
            data["sessions"][0]["title"] = "错误标题"
            data["sessions"][0]["last_event_type"] = "session_started"
            index_path.write_text(json.dumps(data, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")

            preview = store.rebuild_index(apply=False)
            codes = {issue.code for issue in preview.issues}
            self.assertIn(session_consistency.ISSUE_EVENT_COUNT_MISMATCH, codes)
            self.assertIn(session_consistency.ISSUE_MESSAGE_COUNT_MISMATCH, codes)
            self.assertIn(session_consistency.ISSUE_TITLE_MISMATCH, codes)
            self.assertIn(session_consistency.ISSUE_LAST_EVENT_MISMATCH, codes)
            self.assertFalse(preview.applied)

            # 预览不得改写索引。
            stale = json.loads(index_path.read_text(encoding="utf-8"))
            self.assertEqual(stale["sessions"][0]["event_count"], 1)

            applied = store.rebuild_index(
                apply=True,
                now=datetime(2026, 7, 12, 12, 0, tzinfo=timezone.utc),
            )
            restored = json.loads(index_path.read_text(encoding="utf-8"))
            entry = restored["sessions"][0]
            backups = list((workspace / ".agent_sessions").glob("index.json.bak.*"))

        self.assertTrue(applied.applied)
        self.assertIsNotNone(applied.backup_path)
        self.assertEqual(len(backups), 1)
        self.assertEqual(entry["event_count"], 4)
        self.assertEqual(entry["message_count"], 2)
        self.assertEqual(entry["title"], "自定义标题")
        self.assertEqual(entry["last_event_type"], "session_renamed")
        self.assertEqual(entry["session_id"], session_id)

    def test_rebuild_adds_orphan_transcript_and_drops_missing_index_entry(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            workspace, store, known_id = self._prepare_store(temp_dir)
            root = workspace / ".agent_sessions"

            # 手工写入一份未登记的转录，模拟 index 更新失败前进程崩溃。
            orphan_id = "20260712-120000-abcdef"
            orphan_path = root / "sessions" / f"{orphan_id}.jsonl"
            orphan_events = [
                {
                    "version": 1,
                    "session_id": orphan_id,
                    "event_id": "a" * 24,
                    "parent_id": None,
                    "type": "session_started",
                    "created_at": "2026-07-12T12:00:00+00:00",
                    "payload": {
                        "workspace_root": str(workspace.resolve()),
                        "title": "孤立会话",
                    },
                },
                {
                    "version": 1,
                    "session_id": orphan_id,
                    "event_id": "b" * 24,
                    "parent_id": None,
                    "type": "user_message",
                    "created_at": "2026-07-12T12:01:00+00:00",
                    "payload": {"content": "孤立消息"},
                },
            ]
            orphan_path.write_text(
                "\n".join(json.dumps(item, ensure_ascii=False) for item in orphan_events) + "\n",
                encoding="utf-8",
            )

            # 再写一个指向不存在文件的索引条目。
            index_path = root / "index.json"
            data = json.loads(index_path.read_text(encoding="utf-8"))
            data["sessions"].append(
                {
                    "session_id": "20260712-130000-ffffff",
                    "title": "幽灵会话",
                    "workspace_root": str(workspace.resolve()),
                    "path": "sessions/20260712-130000-ffffff.jsonl",
                    "created_at": "2026-07-12T13:00:00+00:00",
                    "updated_at": "2026-07-12T13:00:00+00:00",
                    "event_count": 1,
                    "message_count": 0,
                    "last_event_type": "session_started",
                    "archived_at": None,
                }
            )
            index_path.write_text(json.dumps(data, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")

            report = store.check_consistency()
            codes = {issue.code for issue in report.issues}
            self.assertIn(session_consistency.ISSUE_ORPHAN_TRANSCRIPT, codes)
            self.assertIn(session_consistency.ISSUE_MISSING_TRANSCRIPT, codes)

            applied = store.rebuild_index(apply=True)
            sessions = store.list_sessions(workspace_root=workspace, limit=20, include_archived=True)
            session_ids = {entry.session_id for entry in sessions}

        self.assertTrue(applied.applied)
        self.assertIn(known_id, session_ids)
        self.assertIn(orphan_id, session_ids)
        self.assertNotIn("20260712-130000-ffffff", session_ids)
        orphan_entry = next(entry for entry in sessions if entry.session_id == orphan_id)
        self.assertEqual(orphan_entry.title, "孤立消息")
        self.assertEqual(orphan_entry.event_count, 2)
        self.assertEqual(orphan_entry.message_count, 1)

    def test_rebuild_repairs_archive_path_mismatch_without_moving_files(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            workspace, store, session_id = self._prepare_store(temp_dir)
            archived = store.archive_session(
                session_id,
                now=datetime(2026, 7, 12, 14, 0, tzinfo=timezone.utc),
            )
            self.assertEqual(archived.path.parent.name, "archive")

            # 模拟索引路径未同步到 archive/ 的崩溃窗口。
            index_path = workspace / ".agent_sessions" / "index.json"
            data = json.loads(index_path.read_text(encoding="utf-8"))
            data["sessions"][0]["path"] = f"sessions/{session_id}.jsonl"
            data["sessions"][0]["archived_at"] = None
            index_path.write_text(json.dumps(data, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")

            report = store.check_consistency()
            codes = {issue.code for issue in report.issues}
            self.assertIn(session_consistency.ISSUE_PATH_MISMATCH, codes)
            self.assertIn(session_consistency.ISSUE_ARCHIVED_MISMATCH, codes)

            store.rebuild_index(apply=True)
            restored = store.load_session(session_id)
            active_path = workspace / ".agent_sessions" / "sessions" / f"{session_id}.jsonl"
            archive_path = workspace / ".agent_sessions" / "archive" / f"{session_id}.jsonl"

            self.assertTrue(archive_path.is_file())
            self.assertFalse(active_path.exists())
            self.assertEqual(restored.path, archive_path.resolve())
            self.assertIsNotNone(restored.archived_at)
            self.assertEqual(restored.messages[-1]["content"], "第一轮回答")

    def test_orphan_artifact_is_reported_but_not_deleted(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            workspace, store, _session_id = self._prepare_store(temp_dir)
            orphan_dir = workspace / ".agent_sessions" / "artifacts" / "20260712-150000-aaaaaa"
            orphan_dir.mkdir(parents=True)
            marker = orphan_dir / "keep.html"
            marker.write_text("<p>保留</p>", encoding="utf-8")

            report = store.check_consistency()
            codes = {issue.code for issue in report.issues}
            store.rebuild_index(apply=True)

            self.assertIn(session_consistency.ISSUE_ORPHAN_ARTIFACT, codes)
            self.assertTrue(marker.is_file())
            self.assertEqual(marker.read_text(encoding="utf-8"), "<p>保留</p>")

    def test_rebuild_is_idempotent_for_healthy_store(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            _workspace, store, _session_id = self._prepare_store(temp_dir)
            first = store.rebuild_index(apply=True)
            second = store.check_consistency()

        self.assertTrue(first.applied)
        self.assertTrue(second.ok)
        self.assertEqual(second.issues, ())


class SessionConsistencyHelpersTest(unittest.TestCase):
    def test_build_index_entry_from_events_rejects_empty_stream(self) -> None:
        with self.assertRaisesRegex(SessionStoreError, "空转录"):
            session_consistency.build_index_entry_from_events(
                session_id="20260712-120000-abcdef",
                relative_path="sessions/20260712-120000-abcdef.jsonl",
                events=[],
            )


if __name__ == "__main__":
    unittest.main()
