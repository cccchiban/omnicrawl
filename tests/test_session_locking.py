from __future__ import annotations

import json
import multiprocessing
import os
import sys
import tempfile
import threading
import time
import unittest
from pathlib import Path

from omnicrawl.session import DurableWritePolicy, SessionStore, SessionStoreError
from omnicrawl.state import session_locking


def _worker_append_events(root: str, session_id: str, start_index: int, count: int, result_queue) -> None:
    """多进程 worker：对同一会话根追加事件。"""

    try:
        store = SessionStore(
            Path(root),
            durable=DurableWritePolicy(fsync=False, lock_timeout_seconds=10.0, lock_poll_seconds=0.02),
        )
        for offset in range(count):
            store.append_event(
                session_id,
                "user_message",
                {"content": f"p{os.getpid()}-{start_index + offset}"},
            )
        result_queue.put(("ok", os.getpid(), count))
    except BaseException as exc:  # pragma: no cover - 由主进程断言
        result_queue.put(("error", os.getpid(), repr(exc)))


class SessionLockingUnitTest(unittest.TestCase):
    def test_process_file_lock_is_reentrant_in_same_process(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            lock = session_locking.ProcessFileLock(
                Path(temp_dir) / ".session_store.lock",
                timeout_seconds=2.0,
                poll_seconds=0.02,
            )
            with lock:
                with lock:
                    self.assertEqual(lock._depth, 2)
                self.assertEqual(lock._depth, 1)
            self.assertEqual(lock._depth, 0)

    def test_process_file_lock_timeout_when_held_by_another_thread(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            lock_path = Path(temp_dir) / ".session_store.lock"
            holder = session_locking.ProcessFileLock(lock_path, timeout_seconds=1.0, poll_seconds=0.02)
            waiter = session_locking.ProcessFileLock(lock_path, timeout_seconds=0.2, poll_seconds=0.02)
            holder.acquire()
            try:
                with self.assertRaisesRegex(SessionStoreError, "获取会话存储写锁超时"):
                    waiter.acquire()
            finally:
                holder.release()

    def test_atomic_write_text_and_append_text_line_roundtrip(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            target = root / "index.json"
            session_locking.atomic_write_text(target, '{"ok": true}\n', fsync=True)
            self.assertEqual(json.loads(target.read_text(encoding="utf-8"))["ok"], True)

            history = root / "history.jsonl"
            session_locking.append_text_line(history, '{"a":1}', fsync=True)
            session_locking.append_text_line(history, '{"a":2}', fsync=True)
            lines = [line for line in history.read_text(encoding="utf-8").splitlines() if line]
            self.assertEqual(len(lines), 2)


class SessionStoreCrossProcessTest(unittest.TestCase):
    def test_multiprocess_appends_keep_event_and_index_counts(self) -> None:
        # Windows 下 multiprocessing 默认 spawn，需在 if __name__ 场景可用；
        # unittest 直接调用函数目标，spawn 兼容。
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir) / ".agent_sessions"
            workspace = Path(temp_dir) / "workspace"
            workspace.mkdir()
            store = SessionStore(
                root,
                durable=DurableWritePolicy(fsync=False, lock_timeout_seconds=10.0),
            )
            state = store.start_session(workspace)
            session_id = state.session_id

            process_count = 3
            events_per_process = 8
            ctx = multiprocessing.get_context("spawn")
            result_queue = ctx.Queue()
            processes = [
                ctx.Process(
                    target=_worker_append_events,
                    args=(str(root), session_id, index * events_per_process, events_per_process, result_queue),
                )
                for index in range(process_count)
            ]
            for process in processes:
                process.start()
            for process in processes:
                process.join(timeout=30)
                self.assertFalse(process.is_alive())
                self.assertEqual(process.exitcode, 0)

            results = [result_queue.get(timeout=2) for _ in processes]
            self.assertTrue(all(item[0] == "ok" for item in results), results)

            events = [
                json.loads(line)
                for line in state.path.read_text(encoding="utf-8").splitlines()
                if line.strip()
            ]
            index_data = json.loads((root / "index.json").read_text(encoding="utf-8"))
            expected = 1 + process_count * events_per_process
            self.assertEqual(len(events), expected)
            self.assertEqual(index_data["sessions"][0]["event_count"], expected)
            self.assertEqual(index_data["sessions"][0]["message_count"], process_count * events_per_process)
            self.assertTrue((root / session_locking.LOCK_FILE_NAME).exists())

    def test_nested_start_session_does_not_deadlock_with_reentrant_locks(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            workspace = Path(temp_dir) / "workspace"
            workspace.mkdir()
            store = SessionStore(
                workspace / ".agent_sessions",
                durable=DurableWritePolicy(fsync=False, lock_timeout_seconds=3.0),
            )
            # start_session 内部会再调用 append_event；若锁不可重入会卡住。
            finished = threading.Event()

            def worker() -> None:
                store.start_session(workspace)
                finished.set()

            thread = threading.Thread(target=worker)
            thread.start()
            thread.join(timeout=5)
            self.assertTrue(finished.is_set())
            self.assertFalse(thread.is_alive())


if __name__ == "__main__":
    unittest.main()
