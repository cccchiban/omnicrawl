from __future__ import annotations

import unittest
from types import SimpleNamespace

from omnicrawl.agent.core import AgentError, LocalToolAgent


class FakeOutcome:
    def __init__(self, payload=None, denied=False, reason=""):
        self.payload = payload or {}
        self.denied = denied
        self.deny_reason = reason


class FakePluginManager:
    def __init__(self):
        self.calls = []
        self.denied_hooks = set()
        self.payload_overrides = {}

    def begin_turn(self):
        self.calls.append(("begin_turn",))

    def end_turn(self):
        self.calls.append(("end_turn",))

    def dispatch(self, hook, payload=None, **kwargs):
        self.calls.append((hook, dict(payload or {}), kwargs))
        if hook in self.denied_hooks:
            return FakeOutcome(denied=True, reason=f"deny {hook}")
        if hook in self.payload_overrides:
            return FakeOutcome(payload=self.payload_overrides[hook])
        return FakeOutcome(payload=dict(payload or {}))


class AgentHooksTest(unittest.TestCase):
    def test_dispatch_helper_deny(self) -> None:
        agent = object.__new__(LocalToolAgent)
        manager = FakePluginManager()
        manager.denied_hooks.add("turn.start")
        agent._plugin_manager = manager
        agent.config = SimpleNamespace(resume_session_id="")
        # current_session_id 是 property；直接绕过，dispatch 传入显式 session_id。
        result = LocalToolAgent._dispatch_plugin_hook(
            agent,
            "turn.start",
            {"userText": "x"},
            session_id="",
        )
        self.assertIsNone(result)

    def test_dispatch_helper_transform(self) -> None:
        agent = object.__new__(LocalToolAgent)
        manager = FakePluginManager()
        manager.payload_overrides["turn.start"] = {"userText": "patched", "tags": ["a"]}
        agent._plugin_manager = manager
        agent._session_state = SimpleNamespace(session_id="s1")
        result = LocalToolAgent._dispatch_plugin_hook(agent, "turn.start", {"userText": "x", "tags": []})
        self.assertEqual(result["userText"], "patched")

    def test_no_manager_passthrough(self) -> None:
        agent = object.__new__(LocalToolAgent)
        agent._plugin_manager = None
        result = LocalToolAgent._dispatch_plugin_hook(agent, "turn.start", {"userText": "x"})
        self.assertEqual(result["userText"], "x")

    def test_guard_hook_infrastructure_failure_is_fail_closed(self) -> None:
        agent = object.__new__(LocalToolAgent)

        class BrokenManager:
            def dispatch(self, *_args, **_kwargs):
                raise RuntimeError("boom")

        agent._plugin_manager = BrokenManager()
        self.assertIsNone(
            LocalToolAgent._dispatch_plugin_hook(
                agent,
                "tool.approval.before",
                {"tool": "bash"},
                session_id="",
            )
        )
        # notify/observe 类钩子仍 fail-open，避免非关键路径拖垮主流程。
        self.assertEqual(
            LocalToolAgent._dispatch_plugin_hook(
                agent,
                "tool.execute.after",
                {"tool": "bash"},
                session_id="",
            )["tool"],
            "bash",
        )

    def test_run_stream_ends_plugin_turn_when_prompt_history_write_fails(self) -> None:
        agent = object.__new__(LocalToolAgent)
        manager = FakePluginManager()
        agent._plugin_manager = manager
        agent.config = SimpleNamespace(resume_session_id="", llm=None)
        agent._history = []
        agent._pending_user_text = None
        agent._ensure_mcp_tools_ready = lambda _status: None
        agent._apply_skill_command = lambda text, _status: text
        agent._resolve_continue_request = lambda text: text
        agent._append_prompt_history = lambda _text: (_ for _ in ()).throw(
            RuntimeError("disk failure")
        )
        session_events = []
        agent._append_session_event = lambda event, payload: session_events.append((event, payload))
        agent._is_turn_cancel_exception = lambda _exc: False

        with self.assertRaisesRegex(RuntimeError, "disk failure"):
            LocalToolAgent.run_stream(agent, "hello", lambda _delta: None)

        calls = [item[0] for item in manager.calls]
        self.assertEqual(calls.count("begin_turn"), 1)
        self.assertEqual(calls.count("turn.start"), 1)
        self.assertEqual(calls.count("turn.error"), 1)
        self.assertEqual(calls.count("end_turn"), 1)
        self.assertEqual(session_events[0][0], "session_interrupted")


if __name__ == "__main__":
    unittest.main()
