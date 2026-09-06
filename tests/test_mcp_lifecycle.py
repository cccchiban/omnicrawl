from __future__ import annotations

import json
import tempfile
import unittest
from pathlib import Path

from omnicrawl.agent import LocalToolAgent
from omnicrawl.session import SessionStore


class AgentLifecycleTest(unittest.TestCase):
    def test_agent_close_closes_mcp_manager(self) -> None:
        class FakeMCPManager:
            closed = False

            def close(self) -> None:
                self.closed = True

        class FakeTempWorkspace:
            closed = False

            def close(self) -> None:
                self.closed = True

        agent = object.__new__(LocalToolAgent)
        manager = FakeMCPManager()
        temp_workspace = FakeTempWorkspace()
        agent._mcp_manager = manager
        agent._temp_workspace = temp_workspace

        LocalToolAgent.close(agent)

        self.assertTrue(manager.closed)
        self.assertTrue(temp_workspace.closed)

    def test_agent_close_records_session_closed_once(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            workspace = Path(temp_dir)
            store = SessionStore(workspace / ".agent_sessions")
            state = store.start_session(workspace)
            store.append_event(state.session_id, "user_message", {"content": "关闭前问题"})
            store.append_event(state.session_id, "assistant_message", {"content": "关闭前回答"})
            state = store.load_session(state.session_id)

            class FakeMCPManager:
                def close(self) -> None:
                    pass

            class FakeTempWorkspace:
                def close(self) -> None:
                    pass

            agent = object.__new__(LocalToolAgent)
            agent._session_store = store
            agent._session_state = state
            agent._mcp_manager = FakeMCPManager()
            agent._temp_workspace = FakeTempWorkspace()
            agent._closed = False

            LocalToolAgent.close(agent)
            LocalToolAgent.close(agent)

            events = [
                json.loads(line)
                for line in state.path.read_text(encoding="utf-8").splitlines()
                if line.strip()
            ]

        self.assertEqual(
            [event["type"] for event in events],
            ["session_started", "user_message", "assistant_message", "session_closed"],
        )

    def test_agent_close_keeps_interrupted_session_as_last_event(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            workspace = Path(temp_dir)
            store = SessionStore(workspace / ".agent_sessions")
            state = store.start_session(workspace)
            store.append_event(
                state.session_id,
                "session_interrupted",
                {"user_text": "未完成", "reason": "模型请求失败"},
            )

            agent = object.__new__(LocalToolAgent)
            agent._session_store = store
            agent._session_state = store.load_session(state.session_id)
            agent._mcp_manager = None
            agent._temp_workspace = None
            agent._closed = False

            LocalToolAgent.close(agent)

            events = [
                json.loads(line)
                for line in state.path.read_text(encoding="utf-8").splitlines()
                if line.strip()
            ]

        self.assertEqual(events[-1]["type"], "session_interrupted")

    def test_agent_close_still_closes_resources_when_session_event_fails(self) -> None:
        class FakeSessionStore:
            def append_event(self, *_args, **_kwargs):
                raise RuntimeError("写入失败")

        class FakeMCPManager:
            closed = False

            def close(self) -> None:
                self.closed = True

        class FakeTempWorkspace:
            closed = False

            def close(self) -> None:
                self.closed = True

        agent = object.__new__(LocalToolAgent)
        manager = FakeMCPManager()
        temp_workspace = FakeTempWorkspace()
        agent._session_store = FakeSessionStore()
        agent._session_state = type("State", (), {"session_id": "20260616-201530-a1b2c3", "last_event_type": ""})()
        agent._mcp_manager = manager
        agent._temp_workspace = temp_workspace
        agent._closed = False

        with self.assertRaisesRegex(RuntimeError, "写入失败"):
            LocalToolAgent.close(agent)

        self.assertTrue(manager.closed)
        self.assertTrue(temp_workspace.closed)


if __name__ == "__main__":
    unittest.main()
