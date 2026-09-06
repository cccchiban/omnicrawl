from __future__ import annotations

import threading
import time
import unittest

from omnicrawl.agent.subagents.tasks import SubAgentTaskManager, SubAgentTaskSpec


class SubAgentTaskManagerTest(unittest.TestCase):
    def setUp(self) -> None:
        self.manager = SubAgentTaskManager(retention_seconds=60, max_workers=2)

    def tearDown(self) -> None:
        self.manager.close(owner_id="owner-a")

    def _spec(self, task_id: str, batch_id: str = "batch-1") -> SubAgentTaskSpec:
        return SubAgentTaskSpec(task_id, task_id, "explore", batch_id)

    def test_spawn_returns_immediately_and_reaches_terminal_states(self) -> None:
        started = threading.Event()
        release = threading.Event()

        def runner(spec, _cancel):
            started.set()
            release.wait(2)
            return {"status": "completed", "summary": f"safe {spec.task_id}"}

        accepted = self.manager.spawn(
            owner_id="owner-a", session_id="session-a", specs=(self._spec("task-a"),), runner=runner
        )
        self.assertEqual(accepted["batch_id"], "batch-1")
        self.assertIn(
            self.manager.get("task-a", owner_id="owner-a", session_id="session-a")["status"],
            {"queued", "running", "completed"},
        )
        self.assertTrue(started.wait(1))
        self.assertIn(self.manager.get("task-a", owner_id="owner-a")["status"], {"running", "completed"})
        release.set()
        for _ in range(100):
            if self.manager.get("task-a", owner_id="owner-a")["status"] == "completed":
                break
            time.sleep(0.01)
        self.assertEqual(self.manager.get("task-a", owner_id="owner-a")["status"], "completed")

    def test_input_order_and_owner_isolation(self) -> None:
        for index in range(2):
            self.manager.spawn(
                owner_id="owner-a", session_id="session-a", specs=(self._spec(f"task-{index}"),),
                runner=lambda spec, _cancel: {"status": "completed", "summary": spec.task_id},
            )
        self.assertEqual([item["task_id"] for item in self.manager.list(owner_id="owner-a")], ["task-0", "task-1"])
        self.assertEqual(self.manager.list(owner_id="owner-b"), [])
        self.assertIsNone(self.manager.get("task-0", owner_id="owner-b"))

    def test_cancel_unknown_and_terminal_are_stable(self) -> None:
        self.assertEqual(self.manager.cancel(owner_id="owner-a", task_id="missing")["code"], "SUBAGENT_NOT_FOUND")
        self.manager.spawn(
            owner_id="owner-a", session_id="session-a", specs=(self._spec("task-a"),),
            runner=lambda _spec, _cancel: {"status": "completed", "summary": "ok"},
        )
        time.sleep(0.05)
        result = self.manager.cancel(owner_id="owner-a", task_id="task-a")
        self.assertEqual(result["status"], "already_terminal")

    def test_notifications_are_exactly_once_and_observer_failure_isolated(self) -> None:
        observed = []
        self.manager.spawn(
            owner_id="owner-a", session_id="session-a", specs=(self._spec("task-a"),),
            runner=lambda _spec, _cancel: {"status": "completed", "summary": "token=***"},
            observer=lambda _name, payload: (observed.append(payload), (_ for _ in ()).throw(RuntimeError("closed")))[0],
        )
        for _ in range(100):
            if self.manager.get("task-a", owner_id="owner-a")["status"] == "completed":
                break
            time.sleep(0.01)
        events = self.manager.drain_notifications(
            owner_id="owner-a",
            session_id="session-a",
        )
        self.assertEqual(len(events), 1)
        self.assertEqual(events[0]["status"], "completed")
        self.assertEqual(
            self.manager.drain_notifications(
                owner_id="owner-a",
                session_id="session-a",
            ),
            [],
        )
        self.assertTrue(observed)
        self.assertNotIn("prompt", str(events))

    def test_expired_notification_is_not_returned_without_prior_list_or_get(self) -> None:
        manager = SubAgentTaskManager(retention_seconds=1, max_workers=1)
        try:
            manager.spawn(
                owner_id="owner-a",
                session_id="session-a",
                specs=(self._spec("task-expired"),),
                runner=lambda _spec, _cancel: {
                    "status": "completed",
                    "summary": "old",
                },
            )
            for _ in range(100):
                if manager.get("task-expired", owner_id="owner-a") is not None:
                    snapshot = manager.get("task-expired", owner_id="owner-a")
                    if snapshot and snapshot["status"] == "completed":
                        break
                time.sleep(0.01)
            with manager._lock:
                manager._tasks["task-expired"].updated_at = time.time() - 2
                for events in manager._notifications.values():
                    for event in events:
                        event["timestamp"] = time.time() - 2
            self.assertEqual(
                manager.drain_notifications(
                    owner_id="owner-a",
                    session_id="session-a",
                ),
                [],
            )
        finally:
            manager.close(owner_id="owner-a")

    def test_cleanup_removes_records_at_exact_ttl_boundary(self) -> None:
        """到期时间等于当前时间时也应立刻回收，避免清理线程零等待自旋。"""

        manager = SubAgentTaskManager(retention_seconds=1, max_workers=1)
        completed = threading.Event()
        try:
            manager.spawn(
                owner_id="owner-a",
                session_id="session-a",
                specs=(self._spec("task-boundary"),),
                runner=lambda _spec, _cancel: {"status": "completed", "summary": "done"},
                observer=lambda event_name, _payload: (
                    completed.set()
                    if event_name == "subagent.task.completed"
                    else None
                ),
            )
            self.assertTrue(completed.wait(1))
            with manager._lock:
                now = time.time()
                manager._tasks["task-boundary"].updated_at = now - manager.retention_seconds
                for events in manager._notifications.values():
                    for event in events:
                        event["timestamp"] = now - manager.retention_seconds
                manager._cleanup_expired_locked(now)
                self.assertNotIn("task-boundary", manager._tasks)
                self.assertNotIn(("owner-a", "session-a"), manager._notifications)
        finally:
            manager.close(owner_id="owner-a")

    def test_idle_callback_waits_for_running_worker(self) -> None:
        release = threading.Event()
        callback_called = threading.Event()
        self.manager.spawn(
            owner_id="owner-a",
            session_id="session-a",
            specs=(self._spec("task-idle"),),
            runner=lambda _spec, _cancel: (
                release.wait(2),
                {"status": "completed", "summary": "done"},
            )[1],
        )
        self.manager.call_when_idle(
            owner_id="owner-a",
            callback=callback_called.set,
        )
        self.assertFalse(callback_called.wait(0.05))
        release.set()
        self.assertTrue(callback_called.wait(1))

    def test_cancel_batch_marks_task_cancelled(self) -> None:
        release = threading.Event()
        self.manager.spawn(
            owner_id="owner-a", session_id="session-a", specs=(self._spec("task-a", "batch-x"),),
            runner=lambda _spec, cancel: (release.wait(2), {"status": "cancelled" if cancel.is_set() else "completed"})[1],
        )
        self.assertTrue(self.manager.cancel(owner_id="owner-a", batch_id="batch-x")["ok"])
        release.set()
        for _ in range(100):
            snapshot = self.manager.get("task-a", owner_id="owner-a")
            if snapshot and snapshot["status"] == "cancelled":
                break
            time.sleep(0.01)
        self.assertEqual(self.manager.get("task-a", owner_id="owner-a")["status"], "cancelled")

    def test_terminal_task_expires_without_followup_query(self) -> None:
        """TTL 到期后，清理不能依赖 list/get/drain 等后续控制面请求。"""

        manager = SubAgentTaskManager(retention_seconds=1, max_workers=1)
        completed = threading.Event()
        try:
            manager.spawn(
                owner_id="owner-a",
                session_id="session-a",
                specs=(self._spec("task-auto-expired"),),
                runner=lambda _spec, _cancel: {"status": "completed", "summary": "done"},
                observer=lambda event_name, _payload: (
                    completed.set()
                    if event_name == "subagent.task.completed"
                    else None
                ),
            )
            self.assertTrue(completed.wait(1))

            # 断言直接检查内部状态，避免 get/list/drain 自身的同步清理掩盖问题。
            deadline = time.monotonic() + 3
            while time.monotonic() < deadline:
                with manager._lock:
                    task_exists = "task-auto-expired" in manager._tasks
                    notifications_exist = ("owner-a", "session-a") in manager._notifications
                if not task_exists and not notifications_exist:
                    break
                time.sleep(0.02)

            self.assertFalse(task_exists)
            self.assertFalse(notifications_exist)
        finally:
            manager.close(owner_id="owner-a")

    def test_permanent_close_stops_ttl_cleanup_thread(self) -> None:
        """永久关闭必须唤醒无限等待的清理线程，避免 Agent 生命周期泄漏。"""

        manager = SubAgentTaskManager(retention_seconds=60, max_workers=1)
        cleanup_thread = manager._cleanup_thread
        self.assertTrue(cleanup_thread.is_alive())

        manager.close(owner_id="owner-a")

        self.assertFalse(cleanup_thread.is_alive())


if __name__ == "__main__":
    unittest.main()
