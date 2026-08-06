from __future__ import annotations

import json
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace

from omnicrawl.agent import AgentModelReply, LocalToolAgent
from omnicrawl.agent.context_compaction import (
    ContextCompactionService,
    ModelSummaryCompactor,
    SummaryModelResponse,
)
from omnicrawl.config.context_compaction import ContextCompactionConfig
from omnicrawl.memory import MemoryRecord, MemoryStore, MemoryWriteRequest
from omnicrawl.session import SessionStore


class RecordingMemoryStore:
    def __init__(self, *, fail: bool = False) -> None:
        self.requests: list[MemoryWriteRequest] = []
        self.fail = fail

    def write(self, memories: list[MemoryWriteRequest]) -> list[MemoryRecord]:
        if self.fail:
            raise RuntimeError("memory write failed")
        self.requests.extend(memories)
        return []


def _build_agent(
    workspace: Path,
    *,
    memory_store: RecordingMemoryStore,
) -> tuple[LocalToolAgent, SessionStore, str]:
    agent = object.__new__(LocalToolAgent)
    agent.workspace_root = workspace
    agent.config = SimpleNamespace(
        max_history_turns=2,
        max_tool_output_chars=6_000,
        context_compaction=ContextCompactionConfig(enabled=True, recent_turns=2),
        llm=SimpleNamespace(context_window_tokens=128_000),
    )
    agent._history = [
        {
            "role": "user",
            "content": "为项目实现压缩记忆，保留项目约束，API Key=demo-token",
        },
        {"role": "assistant", "content": "已开始实现"},
        {"role": "user", "content": "保留项目约束，不删除配置"},
        {"role": "assistant", "content": "已记录约束"},
    ]
    agent._skill_manager = None
    agent._active_skills = []
    agent._tools = {}
    agent._system_prompt_template = "system"
    agent._memory_store = memory_store
    store = SessionStore(workspace / ".agent_sessions")
    state = store.start_session(workspace)
    for message in agent._history:
        event_type = "user_message" if message["role"] == "user" else "assistant_message"
        store.append_event(state.session_id, event_type, {"content": message["content"]})
    agent._session_store = store
    agent._session_state = store.load_session(state.session_id)

    def fake_request(
        _messages,
        _on_delta,
        on_token_usage,
        _on_protocol_wait,
        _on_retry_status,
        on_stream_rollback=None,
    ):
        on_token_usage(1_000, 100, 250)
        return AgentModelReply(
            message={"role": "assistant", "content": "继续完成"},
            content="继续完成",
        )

    agent._request_agent_reply = fake_request  # type: ignore[method-assign]
    return agent, store, state.session_id


class ContextCompactionMemoryTest(unittest.TestCase):
    def _summary_service(self) -> ContextCompactionService:
        def call_model(messages):
            prompt = str(messages[0]["content"])
            payload = json.loads(prompt.split("输入：\n", 1)[1])
            event_ids = [item["event_id"] for item in payload["source_events"]]
            structured = {
                "objective": ["为项目实现压缩记忆，API Key=demo-token"],
                "constraints": [
                    {"text": "保留项目约束", "source_event_ids": [event_ids[0]]}
                ],
                "decisions": [
                    {
                        "text": "使用长期记忆保存稳定项目上下文",
                        "source_event_ids": [event_ids[0]],
                    }
                ],
                "completed": [
                    {"text": "已开始实现", "source_event_ids": [event_ids[1]]}
                ],
                "current_state": ["自动压缩已完成"],
                "open_issues": [
                    {"text": "等待回归验证", "source_event_ids": [event_ids[1]]}
                ],
                "artifacts": [
                    {"text": "omnicrawl/agent/core.py", "source_event_ids": [event_ids[0]]}
                ],
                "exact_evidence": [],
            }
            return SummaryModelResponse(content=json.dumps(structured, ensure_ascii=False))

        return ContextCompactionService(compactor=ModelSummaryCompactor(call_model))

    def test_automatic_model_compaction_writes_project_and_task_memories(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            memory = RecordingMemoryStore()
            agent, _store, _session_id = _build_agent(Path(temp_dir), memory_store=memory)
            agent.config.context_compaction = ContextCompactionConfig(
                enabled=True,
                recent_turns=2,
                trigger_context_tokens=1,
            )
            agent._context_compaction_service_instance = self._summary_service()

            LocalToolAgent.run_stream(agent, "继续处理", lambda _delta: None)

        self.assertEqual(len(memory.requests), 2)
        by_directory = {request.storage_directory: request.content for request in memory.requests}
        self.assertIn("project-context/general", by_directory)
        self.assertIn("task-history/general", by_directory)
        self.assertIn("保留项目约束", by_directory["project-context/general"])
        self.assertIn("API Key=demo-token", by_directory["project-context/general"])
        self.assertIn("omnicrawl/agent/core.py", by_directory["project-context/general"])
        self.assertIn("等待回归验证", by_directory["task-history/general"])
        self.assertNotIn("自动压缩已完成", by_directory["task-history/general"])

    def test_manual_model_compaction_writes_the_same_memory_categories(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            memory = RecordingMemoryStore()
            agent, _store, _session_id = _build_agent(Path(temp_dir), memory_store=memory)
            agent._context_compaction_service_instance = self._summary_service()

            LocalToolAgent.compact_conversation_model(agent)

        self.assertEqual(
            {request.storage_directory for request in memory.requests},
            {"project-context/general", "task-history/general"},
        )

    def test_deterministic_compaction_writes_user_and_task_information(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            memory = RecordingMemoryStore()
            agent, _store, _session_id = _build_agent(Path(temp_dir), memory_store=memory)

            agent.compact_conversation()

        self.assertEqual(len(memory.requests), 2)
        contents = "\n".join(request.content for request in memory.requests)
        self.assertIn("为项目实现压缩记忆", contents)
        self.assertIn("保留项目约束", contents)
        self.assertIn("API Key=demo-token", contents)

    def test_compaction_memory_can_be_retrieved_by_a_later_session(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            workspace = Path(temp_dir)
            agent, _store, _session_id = _build_agent(
                workspace,
                memory_store=RecordingMemoryStore(),
            )
            memory_root = workspace / "memory"
            agent._memory_store = MemoryStore(memory_root)

            agent.compact_conversation()

            restored = MemoryStore(memory_root)
            matches = restored.search("压缩记忆", max_results=5)
            records = restored.read([item.id for item in matches])

        self.assertTrue(matches)
        self.assertTrue(any("为项目实现压缩记忆" in item.content for item in records))

    def test_memory_write_failure_does_not_break_compaction(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            agent, _store, _session_id = _build_agent(
                Path(temp_dir), memory_store=RecordingMemoryStore(fail=True)
            )

            with self.assertLogs("omnicrawl.agent.core", level="WARNING"):
                summary = agent.compact_conversation()

        self.assertIn("会话压缩摘要", summary)


if __name__ == "__main__":
    unittest.main()
