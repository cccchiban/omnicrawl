"""跨进程 SubAgent 任务恢复：只恢复终态快照，中断任务标记失败且不重跑。"""

from __future__ import annotations

import tempfile
import threading
import time
import unittest
from datetime import datetime, timezone
from pathlib import Path
from unittest.mock import patch

from omnicrawl.agent.core import AgentConfig, LocalToolAgent
from omnicrawl.agent.subagents.recovery import (
    rebuild_task_snapshots_from_session_events,
)
from omnicrawl.agent.subagents.tasks import SubAgentTaskManager, SubAgentTaskSpec
from omnicrawl.config.llm import LLMConfig
from omnicrawl.config.subagents import SubAgentConfig
from omnicrawl.mcp.config import MCPConfig
from omnicrawl.state.session import SessionStore
from omnicrawl.state.session_models import SessionEvent
from omnicrawl.temp_workspace import AgentTempWorkspaceConfig


def _event(
    event_type: str,
    payload: dict,
    *,
    session_id: str = "20260716-120000-abcdef",
    created_at: datetime | None = None,
) -> SessionEvent:
    return SessionEvent.create(
        session_id=session_id,
        event_type=event_type,
        payload=payload,
        now=created_at or datetime(2026, 7, 16, 12, 0, 0, tzinfo=timezone.utc),
    )


class RebuildFromSessionEventsTest(unittest.TestCase):
    def test_terminal_tasks_are_restored_from_latest_event(self) -> None:
        events = [
            _event(
                "subagent_task_queued",
                {
                    "task_id": "task-a1b2c3d4e5f6",
                    "batch_id": "batch-111111111111",
                    "agent_type": "explore",
                    "description": "检查恢复",
                    "status": "queued",
                },
                created_at=datetime(2026, 7, 16, 12, 0, 0, tzinfo=timezone.utc),
            ),
            _event(
                "subagent_task_started",
                {
                    "task_id": "task-a1b2c3d4e5f6",
                    "batch_id": "batch-111111111111",
                    "agent_type": "explore",
                    "description": "检查恢复",
                    "status": "running",
                },
                created_at=datetime(2026, 7, 16, 12, 0, 1, tzinfo=timezone.utc),
            ),
            _event(
                "subagent_task_completed",
                {
                    "task_id": "task-a1b2c3d4e5f6",
                    "batch_id": "batch-111111111111",
                    "agent_type": "explore",
                    "description": "检查恢复",
                    "status": "completed",
                    "summary": "已定位边界",
                    "artifacts": [{"type": "subagent_result", "artifact_path": "x.json"}],
                    "usage": {"model_turns": 1},
                    "error": None,
                },
                created_at=datetime(2026, 7, 16, 12, 0, 5, tzinfo=timezone.utc),
            ),
        ]

        snapshots = rebuild_task_snapshots_from_session_events(
            events,
            owner_id="agent-1",
            session_id="20260716-120000-abcdef",
        )

        self.assertEqual(len(snapshots), 1)
        task = snapshots[0]
        self.assertEqual(task["task_id"], "task-a1b2c3d4e5f6")
        self.assertEqual(task["status"], "completed")
        self.assertEqual(task["result"]["summary"], "已定位边界")
        self.assertTrue(task["result"]["recovered"])
        self.assertEqual(task["result"]["artifacts"][0]["artifact_path"], "x.json")
        self.assertIsNone(task["error"])
        self.assertGreaterEqual(task["updated_at"], task["created_at"])

    def test_interrupted_non_terminal_tasks_become_failed(self) -> None:
        events = [
            _event(
                "subagent_task_queued",
                {
                    "task_id": "task-b1b2c3d4e5f6",
                    "batch_id": "batch-222222222222",
                    "agent_type": "plan",
                    "description": "进程中断",
                    "status": "queued",
                },
            ),
            _event(
                "subagent_task_started",
                {
                    "task_id": "task-b1b2c3d4e5f6",
                    "batch_id": "batch-222222222222",
                    "agent_type": "plan",
                    "description": "进程中断",
                    "status": "running",
                },
                created_at=datetime(2026, 7, 16, 12, 0, 2, tzinfo=timezone.utc),
            ),
            _event(
                "subagent_task_waiting_approval",
                {
                    "task_id": "task-c1b2c3d4e5f6",
                    "batch_id": "batch-333333333333",
                    "agent_type": "general-purpose",
                    "description": "等待审批中断",
                    "status": "waiting_approval",
                },
            ),
        ]

        snapshots = rebuild_task_snapshots_from_session_events(
            events,
            owner_id="agent-1",
            session_id="session-x",
        )
        by_id = {item["task_id"]: item for item in snapshots}

        self.assertEqual(by_id["task-b1b2c3d4e5f6"]["status"], "failed")
        self.assertEqual(
            by_id["task-b1b2c3d4e5f6"]["error"]["code"],
            "SUBAGENT_INTERRUPTED",
        )
        self.assertEqual(by_id["task-c1b2c3d4e5f6"]["status"], "failed")
        self.assertIn("进程重启", by_id["task-c1b2c3d4e5f6"]["error"]["message"])
        # 恢复路径不得重放 prompt 或凭据字段。
        for item in snapshots:
            serialized = str(item)
            self.assertNotIn("prompt", serialized.casefold())
            self.assertNotIn("api_key", serialized.casefold())

    def test_failed_and_cancelled_terminal_states_are_preserved(self) -> None:
        events = [
            _event(
                "subagent_task_failed",
                {
                    "task_id": "task-d1b2c3d4e5f6",
                    "batch_id": "batch-444444444444",
                    "agent_type": "explore",
                    "description": "失败任务",
                    "status": "failed",
                    "summary": "",
                    "error": {"code": "SUBAGENT_MODEL_ERROR", "message": "模型失败"},
                },
            ),
            _event(
                "subagent_task_cancelled",
                {
                    "task_id": "task-e1b2c3d4e5f6",
                    "batch_id": "batch-555555555555",
                    "agent_type": "explore",
                    "description": "取消任务",
                    "status": "cancelled",
                    "error": {"code": "SUBAGENT_CANCELLED", "message": "任务已取消。"},
                },
            ),
        ]
        snapshots = rebuild_task_snapshots_from_session_events(
            events,
            owner_id="agent-1",
            session_id="session-x",
        )
        by_id = {item["task_id"]: item for item in snapshots}
        self.assertEqual(by_id["task-d1b2c3d4e5f6"]["status"], "failed")
        self.assertEqual(
            by_id["task-d1b2c3d4e5f6"]["error"]["code"],
            "SUBAGENT_MODEL_ERROR",
        )
        self.assertEqual(by_id["task-e1b2c3d4e5f6"]["status"], "cancelled")

    def test_invalid_payloads_and_non_subagent_events_are_ignored(self) -> None:
        events = [
            _event("user_message", {"content": "hello"}),
            _event("subagent_batch_created", {"batch_id": "batch-1"}),
            _event("subagent_task_completed", {"description": "缺 task_id"}),
            _event(
                "subagent_task_completed",
                {
                    "task_id": "not-a-valid-task-id",
                    "description": "坏 id",
                    "status": "completed",
                },
            ),
        ]
        snapshots = rebuild_task_snapshots_from_session_events(
            events,
            owner_id="agent-1",
            session_id="session-x",
        )
        self.assertEqual(snapshots, [])


class TaskManagerImportRecoveredTest(unittest.TestCase):
    def setUp(self) -> None:
        self.manager = SubAgentTaskManager(retention_seconds=3600, max_workers=1)

    def tearDown(self) -> None:
        self.manager.close(owner_id="owner-a")

    def test_import_recovered_snapshots_are_listable_and_not_rerun(self) -> None:
        runner_calls: list[str] = []

        def runner(spec: SubAgentTaskSpec, _cancel: threading.Event) -> dict:
            runner_calls.append(spec.task_id)
            return {"status": "completed", "summary": "should-not-run"}

        imported = self.manager.import_recovered_snapshots(
            owner_id="owner-a",
            session_id="session-a",
            snapshots=[
                {
                    "task_id": "task-a1b2c3d4e5f6",
                    "batch_id": "batch-111111111111",
                    "description": "恢复任务",
                    "agent_type": "explore",
                    "status": "completed",
                    "result": {
                        "status": "completed",
                        "summary": "历史结果",
                        "recovered": True,
                    },
                    "error": None,
                    "created_at": time.time() - 10,
                    "updated_at": time.time() - 5,
                }
            ],
        )
        self.assertEqual(imported, 1)
        listed = self.manager.list(owner_id="owner-a", session_id="session-a")
        self.assertEqual(len(listed), 1)
        self.assertEqual(listed[0]["status"], "completed")
        self.assertEqual(listed[0]["result"]["summary"], "历史结果")
        self.assertEqual(runner_calls, [])
        # 恢复任务不得进入通知队列，避免模型上下文重复注入。
        self.assertEqual(
            self.manager.drain_notifications(
                owner_id="owner-a",
                session_id="session-a",
            ),
            [],
        )

    def test_import_does_not_overwrite_live_task(self) -> None:
        release = threading.Event()

        def runner(_spec, _cancel):
            release.wait(2)
            return {"status": "completed", "summary": "live"}

        self.manager.spawn(
            owner_id="owner-a",
            session_id="session-a",
            specs=(
                SubAgentTaskSpec(
                    "task-a1b2c3d4e5f6",
                    "live",
                    "explore",
                    "batch-live",
                ),
            ),
            runner=runner,
        )
        imported = self.manager.import_recovered_snapshots(
            owner_id="owner-a",
            session_id="session-a",
            snapshots=[
                {
                    "task_id": "task-a1b2c3d4e5f6",
                    "batch_id": "batch-old",
                    "description": "old",
                    "agent_type": "explore",
                    "status": "completed",
                    "result": {"status": "completed", "summary": "old"},
                    "error": None,
                    "created_at": 1.0,
                    "updated_at": 2.0,
                }
            ],
        )
        self.assertEqual(imported, 0)
        live = self.manager.get(
            "task-a1b2c3d4e5f6",
            owner_id="owner-a",
            session_id="session-a",
        )
        self.assertIn(live["status"], {"queued", "running", "completed"})
        release.set()
        self.manager.wait_for_idle(owner_id="owner-a", timeout=2)

    def test_import_rejects_non_terminal_and_cross_session_mismatch(self) -> None:
        imported = self.manager.import_recovered_snapshots(
            owner_id="owner-a",
            session_id="session-a",
            snapshots=[
                {
                    "task_id": "task-a1b2c3d4e5f6",
                    "batch_id": "batch-1",
                    "description": "running 不能导入",
                    "agent_type": "explore",
                    "status": "running",
                    "result": None,
                    "error": None,
                    "created_at": 1.0,
                    "updated_at": 2.0,
                }
            ],
        )
        self.assertEqual(imported, 0)
        self.assertEqual(
            self.manager.list(owner_id="owner-a", session_id="session-a"),
            [],
        )


class AgentSessionRecoveryIntegrationTest(unittest.TestCase):
    def test_agent_resume_imports_terminal_subagent_tasks(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            workspace = Path(temp_dir)
            store = SessionStore(workspace / ".agent_sessions")
            state = store.start_session(workspace)
            store.append_event(
                state.session_id,
                "subagent_task_completed",
                {
                    "task_id": "task-a1b2c3d4e5f6",
                    "batch_id": "batch-111111111111",
                    "agent_type": "explore",
                    "description": "恢复列表可见",
                    "status": "completed",
                    "summary": "跨进程摘要",
                    "artifacts": [],
                    "usage": {},
                    "error": None,
                },
            )
            store.append_event(
                state.session_id,
                "subagent_task_started",
                {
                    "task_id": "task-b1b2c3d4e5f6",
                    "batch_id": "batch-222222222222",
                    "agent_type": "plan",
                    "description": "中断任务",
                    "status": "running",
                },
            )

            config = AgentConfig(
                llm=LLMConfig(
                    api_key="test-key",
                    base_url="http://example.invalid",
                    model="test-model",
                ),
                workspace_root=workspace,
                session_enabled=True,
                session_directory=".agent_sessions",
                resume_session_id=state.session_id,
                skills_enabled=False,
                memory_enabled=False,
                mcp_config=MCPConfig(enabled=False),
                subagents=SubAgentConfig(enabled=True, allow_background=True),
                temp_workspace=AgentTempWorkspaceConfig(cleanup_enabled=False),
            )
            with patch("openai.OpenAI", return_value=object()):
                with patch.object(
                    LocalToolAgent,
                    "_load_system_prompt_template",
                    return_value="sys",
                ):
                    agent = LocalToolAgent(config=config)

            try:
                tasks = agent.list_subagent_tasks()
            finally:
                agent.close()

            by_id = {item["task_id"]: item for item in tasks}
            self.assertEqual(by_id["task-a1b2c3d4e5f6"]["status"], "completed")
            self.assertEqual(by_id["task-a1b2c3d4e5f6"]["result"]["summary"], "跨进程摘要")
            self.assertTrue(by_id["task-a1b2c3d4e5f6"]["result"]["recovered"])
            self.assertEqual(by_id["task-b1b2c3d4e5f6"]["status"], "failed")
            self.assertEqual(
                by_id["task-b1b2c3d4e5f6"]["error"]["code"],
                "SUBAGENT_INTERRUPTED",
            )


if __name__ == "__main__":
    unittest.main()
