from __future__ import annotations

import json
import threading
import time
import unittest
from types import SimpleNamespace
from typing import Any

from omnicrawl.agent import LocalToolAgent, ToolDefinition, ToolResult
from omnicrawl.agent.subagents.approval import (
    ApprovalBroker,
    SubAgentApprovalOrigin,
    SubAgentApprovalScope,
    activate_subagent_approval_scope,
    current_subagent_approval_scope,
    subagent_approval_risk_summary,
)


def _tool(name: str, *, confirmation: bool = False) -> ToolDefinition:
    return ToolDefinition(
        name=name,
        description=name,
        argument_schema='{"command": "example"}',
        requires_confirmation=confirmation,
        run=lambda _arguments: ToolResult(ok=True, output="ok"),
    )


def _origin(task_id: str = "task-a1b2c3d4e5f6") -> SubAgentApprovalOrigin:
    return SubAgentApprovalOrigin(
        batch_id="batch-a1b2c3d4e5f6",
        task_id=task_id,
        agent_label="verify",
        description="验证当前工作区",
    )


def _wait_until(predicate, *, timeout: float = 1.0) -> bool:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if predicate():
            return True
        time.sleep(0.01)
    return bool(predicate())


class SubAgentApprovalPolicyTest(unittest.TestCase):
    def test_only_delete_and_mutating_git_operations_need_subagent_confirmation(self) -> None:
        shell = _tool("powershell")
        write = ToolDefinition(
            name="write_file",
            description="写入文件。",
            argument_schema='{"path":"note.txt","content":"text"}',
            requires_confirmation=True,
            run=lambda _arguments: ToolResult(ok=True, output="ok"),
        )

        self.assertEqual(subagent_approval_risk_summary(write, {"path": "note.txt"}), "")
        restricted_external = _tool("restricted.mutate_resource", confirmation=True)
        custom_mcp_external = _tool("news_server.publish", confirmation=True)
        self.assertEqual(
            subagent_approval_risk_summary(restricted_external, {"action": "update"}),
            "受限外部操作",
        )
        self.assertEqual(
            subagent_approval_risk_summary(custom_mcp_external, {"action": "publish"}),
            "受限外部操作",
        )
        self.assertEqual(
            subagent_approval_risk_summary(shell, {"command": "python -m unittest"}),
            "",
        )
        self.assertEqual(
            subagent_approval_risk_summary(shell, {"command": "Remove-Item -Recurse cache"}),
            "删除操作",
        )
        self.assertEqual(
            subagent_approval_risk_summary(shell, {"command": "git rm stale.py"}),
            "删除操作",
        )

        for command in ("git status --short", "git diff", "git log -1", "git show HEAD"):
            with self.subTest(command=command):
                self.assertEqual(
                    subagent_approval_risk_summary(shell, {"command": command}),
                    "",
                )

        for command in (
            "git add README.md",
            "git commit -m test",
            "git switch feature/demo",
            "git reset --hard HEAD~1",
            "git push origin HEAD",
            "git unusual-future-command",
        ):
            with self.subTest(command=command):
                self.assertEqual(
                    subagent_approval_risk_summary(shell, {"command": command}),
                    "Git 变更操作",
                )


class ApprovalBrokerTest(unittest.TestCase):
    def _request(
        self,
        broker: ApprovalBroker,
        *,
        task_id: str,
        cancel_check=None,
    ) -> bool:
        return broker.request(
            origin=_origin(task_id),
            tool_name="powershell",
            public_arguments={"command": "git commit -m safe"},
            risk_summary="Git 变更操作",
            cancel_check=cancel_check,
        )

    def test_requests_are_serialized_in_fifo_order(self) -> None:
        first_visible = threading.Event()
        release_first = threading.Event()
        shown: list[str] = []
        results: dict[str, bool] = {}

        def approve(request) -> bool:
            shown.append(request.origin.task_id)
            if request.origin.task_id == "task-first":
                first_visible.set()
                release_first.wait(timeout=1)
            return True

        broker = ApprovalBroker(approve=approve)
        first = threading.Thread(
            target=lambda: results.setdefault(
                "first",
                self._request(broker, task_id="task-first"),
            )
        )
        second = threading.Thread(
            target=lambda: results.setdefault(
                "second",
                self._request(broker, task_id="task-second"),
            )
        )
        first.start()
        self.assertTrue(first_visible.wait(timeout=1))
        second.start()
        self.assertTrue(_wait_until(lambda: broker.pending_count == 2))
        self.assertEqual(shown, ["task-first"])

        release_first.set()
        first.join(timeout=1)
        second.join(timeout=1)

        self.assertFalse(first.is_alive())
        self.assertFalse(second.is_alive())
        self.assertEqual(shown, ["task-first", "task-second"])
        self.assertEqual(results, {"first": True, "second": True})

    def test_task_cancellation_rejects_queued_request_without_showing_it(self) -> None:
        first_visible = threading.Event()
        release_first = threading.Event()
        shown: list[str] = []
        results: dict[str, bool] = {}

        def approve(request) -> bool:
            shown.append(request.origin.task_id)
            if request.origin.task_id == "task-first":
                first_visible.set()
                release_first.wait(timeout=1)
            return True

        broker = ApprovalBroker(approve=approve)
        first = threading.Thread(
            target=lambda: results.setdefault(
                "first",
                self._request(broker, task_id="task-first"),
            )
        )
        second = threading.Thread(
            target=lambda: results.setdefault(
                "second",
                self._request(broker, task_id="task-second"),
            )
        )
        first.start()
        self.assertTrue(first_visible.wait(timeout=1))
        second.start()
        self.assertTrue(_wait_until(lambda: broker.pending_count == 2))

        broker.cancel_task("task-second")
        second.join(timeout=1)
        self.assertFalse(second.is_alive())
        self.assertFalse(results["second"])
        self.assertEqual(shown, ["task-first"])

        release_first.set()
        first.join(timeout=1)
        self.assertTrue(results["first"])
        self.assertTrue(_wait_until(lambda: broker.pending_count == 0))

    def test_active_cancellation_invalidates_late_approval(self) -> None:
        visible = threading.Event()
        release = threading.Event()
        results: list[bool] = []

        def approve(_request) -> bool:
            visible.set()
            release.wait(timeout=1)
            return True

        broker = ApprovalBroker(approve=approve)
        worker = threading.Thread(
            target=lambda: results.append(self._request(broker, task_id="task-active"))
        )
        worker.start()
        self.assertTrue(visible.wait(timeout=1))

        broker.cancel_task("task-active")
        worker.join(timeout=1)
        self.assertFalse(worker.is_alive())
        self.assertEqual(results, [False])

        release.set()
        self.assertTrue(_wait_until(lambda: broker.pending_count == 0))

    def test_cancellation_emits_safe_event_for_remote_control_plane(self) -> None:
        visible = threading.Event()
        release = threading.Event()
        events: list[tuple[str, dict[str, Any]]] = []
        result: list[bool] = []

        def approve(_request) -> bool:
            visible.set()
            release.wait(timeout=1)
            return True

        broker = ApprovalBroker(
            approve=approve,
            event_sink=lambda name, payload: events.append((name, payload)),
        )
        worker = threading.Thread(
            target=lambda: result.append(self._request(broker, task_id="task-cancel-event"))
        )
        worker.start()
        self.assertTrue(visible.wait(timeout=1))

        broker.cancel_task("task-cancel-event")
        worker.join(timeout=1)
        release.set()

        self.assertFalse(worker.is_alive())
        self.assertEqual(result, [False])
        cancelled = [
            payload
            for name, payload in events
            if name == "subagent.task.approval_cancelled"
        ]
        self.assertEqual(len(cancelled), 1)
        self.assertEqual(cancelled[0]["task_id"], "task-cancel-event")
        self.assertEqual(cancelled[0]["status"], "cancelled")
        self.assertNotIn("完整任务", json.dumps(cancelled, ensure_ascii=False))

    def test_close_releases_an_active_late_handler_slot(self) -> None:
        visible = threading.Event()
        release = threading.Event()
        results: list[bool] = []

        def approve(_request) -> bool:
            visible.set()
            release.wait(timeout=1)
            return True

        broker = ApprovalBroker(approve=approve)
        worker = threading.Thread(
            target=lambda: results.append(self._request(broker, task_id="task-close"))
        )
        worker.start()
        self.assertTrue(visible.wait(timeout=1))

        broker.close()
        worker.join(timeout=1)
        self.assertFalse(worker.is_alive())
        self.assertEqual(results, [False])

        release.set()
        self.assertTrue(_wait_until(lambda: broker.pending_count == 0))

    def test_waiting_event_is_source_attributed_and_redacted(self) -> None:
        events: list[tuple[str, dict[str, Any]]] = []
        broker = ApprovalBroker(
            approve=lambda _request: True,
            event_sink=lambda name, payload: events.append((name, payload)),
        )

        approved = broker.request(
            origin=_origin(),
            tool_name="powershell",
            public_arguments={"command": "git commit -m demo", "api_key": "very-secret"},
            risk_summary="Git 变更操作",
            cancel_check=None,
        )

        self.assertTrue(approved)
        waiting = next(payload for name, payload in events if name == "subagent.task.waiting_approval")
        self.assertEqual(waiting["task_id"], "task-a1b2c3d4e5f6")
        self.assertEqual(waiting["batch_id"], "batch-a1b2c3d4e5f6")
        self.assertEqual(waiting["agent_label"], "verify")
        self.assertEqual(waiting["risk_summary"], "Git 变更操作")
        self.assertNotIn("very-secret", json.dumps(waiting, ensure_ascii=False))

    def test_scope_is_thread_local_and_restored(self) -> None:
        broker = ApprovalBroker(approve=lambda _request: True)
        scope = SubAgentApprovalScope(broker=broker, origin=_origin())

        self.assertIsNone(current_subagent_approval_scope())
        with activate_subagent_approval_scope(scope):
            self.assertIs(current_subagent_approval_scope(), scope)
        self.assertIsNone(current_subagent_approval_scope())


class LocalToolAgentSubAgentApprovalTest(unittest.TestCase):
    def _agent(self, *, deny_approval_guard: bool = False) -> LocalToolAgent:
        agent = object.__new__(LocalToolAgent)
        agent.config = SimpleNamespace(approval_mode="manual")

        def dispatch(hook: str, payload: dict[str, Any], **_kwargs):
            if deny_approval_guard and hook == "tool.approval.before":
                return None
            return dict(payload)

        agent._dispatch_plugin_hook = dispatch
        return agent

    def test_subagent_policy_skips_normal_write_but_brokers_git_mutation(self) -> None:
        confirmations: list[str] = []
        broker = ApprovalBroker(
            approve=lambda request: confirmations.append(request.risk_summary) or True
        )
        scope = SubAgentApprovalScope(broker=broker, origin=_origin())
        agent = self._agent()
        write = ToolDefinition(
            name="write_file",
            description="写入文件。",
            argument_schema='{"path":"note.txt","content":"text"}',
            requires_confirmation=True,
            run=lambda _arguments: ToolResult(ok=True, output="ok"),
        )
        shell = _tool("powershell")

        self.assertIsNone(
            agent._approve_tool_for_batch(
                write,
                {"path": "note.txt", "content": "hello"},
                persist_session_events=False,
                subagent_approval_scope=scope,
            )
        )
        self.assertEqual(confirmations, [])

        self.assertIsNone(
            agent._approve_tool_for_batch(
                shell,
                {"command": "git status --short"},
                persist_session_events=False,
                subagent_approval_scope=scope,
            )
        )
        self.assertEqual(confirmations, [])

        self.assertIsNone(
            agent._approve_tool_for_batch(
                shell,
                {"command": "git commit -m safe"},
                persist_session_events=False,
                subagent_approval_scope=scope,
            )
        )
        self.assertEqual(confirmations, ["Git 变更操作"])

    def test_plugin_guard_can_deny_but_cannot_auto_approve_subagent_risk(self) -> None:
        confirmations: list[str] = []
        broker = ApprovalBroker(
            approve=lambda request: confirmations.append(request.tool_name) or True
        )
        scope = SubAgentApprovalScope(broker=broker, origin=_origin())
        denied_agent = self._agent(deny_approval_guard=True)

        denied = denied_agent._approve_tool_for_batch(
            _tool("powershell"),
            {"command": "Remove-Item cache"},
            persist_session_events=False,
            subagent_approval_scope=scope,
        )

        self.assertIsNotNone(denied)
        self.assertFalse(denied.ok)
        self.assertEqual(confirmations, [])


if __name__ == "__main__":
    unittest.main()
