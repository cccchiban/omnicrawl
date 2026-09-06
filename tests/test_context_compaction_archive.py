"""L3：压缩事件二级归档、覆盖度指标与压缩后自动记忆检索。"""

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
    TokenUsageSample,
)
from omnicrawl.config.features.context_compaction import ContextCompactionConfig
from omnicrawl.memory import MemoryStore
from omnicrawl.session import SessionStore
from omnicrawl.state.session import SessionStoreError


def _build_agent(
    workspace: Path,
    *,
    memory_store: MemoryStore | None = None,
) -> tuple[LocalToolAgent, SessionStore, str]:
    agent = object.__new__(LocalToolAgent)
    agent.workspace_root = workspace
    agent.config = SimpleNamespace(
        max_history_turns=2,
        context_compaction=ContextCompactionConfig(recent_turns=2),
        llm=SimpleNamespace(context_window_tokens=128_000),
    )
    agent._history = [
        {"role": "user", "content": "第一轮问题"},
        {"role": "assistant", "content": "第一轮回答"},
        {"role": "user", "content": "第二轮问题"},
        {"role": "assistant", "content": "第二轮回答"},
    ]
    agent._skill_manager = None
    agent._active_skills = []
    agent._tools = {}
    agent._system_prompt_template = "system"
    agent._session_memory_store = memory_store
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
            message={"role": "assistant", "content": "第三轮回答"},
            content="第三轮回答",
        )

    agent._request_agent_reply = fake_request  # type: ignore[method-assign]
    return agent, store, state.session_id


def _summary_model(event_ids: list[str]) -> ContextCompactionService:
    def call_model(messages):
        payload = json.loads(str(messages[0]["content"]).split("输入：\n", 1)[1])
        source_ids = [item["event_id"] for item in payload["source_events"]]
        user_messages = [
            {"text": item["payload"]["content"], "source_event_ids": [item["event_id"]]}
            for item in payload["source_events"]
            if item["type"] == "user_message"
            and isinstance(item.get("payload", {}).get("content"), str)
            and item["payload"]["content"].strip()
        ]
        structured = {
            "objective": ["自动压缩归档与记忆检索测试"],
            "constraints": [
                {"text": "保留项目约束", "source_event_ids": [source_ids[0]]}
            ],
            "decisions": [],
            "completed": [],
            "current_state": ["归档已完成，等待恢复验证"],
            "open_issues": [],
            "artifacts": [],
            "exact_evidence": [],
            "user_messages": user_messages,
        }
        return SummaryModelResponse(
            content=json.dumps(structured, ensure_ascii=False),
            usage=TokenUsageSample(100, 20, 10),
            profile="test-summary",
            provider="test",
        )

    return ContextCompactionService(compactor=ModelSummaryCompactor(call_model))


class SessionStoreArchiveTest(unittest.TestCase):
    def test_archive_compacted_events_roundtrip(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            workspace = Path(temp_dir)
            store = SessionStore(workspace / ".agent_sessions")
            state = store.start_session(workspace)
            events = [
                {
                    "event_id": "e1",
                    "type": "tool_call_requested",
                    "payload": {"tool": "bash", "arguments": {"command": "pytest"}},
                },
                {
                    "event_id": "e2",
                    "type": "tool_result",
                    "payload": {"tool": "bash", "ok": True},
                },
            ]

            archive_id = store.archive_compacted_events(
                state.session_id,
                events,
                archive_id="compact-test-1",
            )
            restored = store.read_compacted_events(state.session_id)

        self.assertEqual(archive_id, "compact-test-1")
        self.assertEqual([event["event_id"] for event in restored], ["e1", "e2"])
        self.assertEqual(restored[0]["payload"]["tool"], "bash")

    def test_archive_id_is_generated_when_omitted(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            workspace = Path(temp_dir)
            store = SessionStore(workspace / ".agent_sessions")
            state = store.start_session(workspace)

            archive_id = store.archive_compacted_events(state.session_id, [{"event_id": "x"}])

        self.assertTrue(archive_id.startswith("compact-"))

    def test_archive_rejects_path_separators_in_archive_id(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            workspace = Path(temp_dir)
            store = SessionStore(workspace / ".agent_sessions")
            state = store.start_session(workspace)

            with self.assertRaises(SessionStoreError):
                store.archive_compacted_events(
                    state.session_id,
                    [{"event_id": "x"}],
                    archive_id="../escape",
                )

    def test_archive_rejects_unknown_session(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            workspace = Path(temp_dir)
            store = SessionStore(workspace / ".agent_sessions")

            with self.assertRaises(SessionStoreError):
                store.archive_compacted_events("20260101-000000-aaaaaa", [{"event_id": "x"}])

    def test_read_compacted_events_empty_for_unknown_session(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            workspace = Path(temp_dir)
            store = SessionStore(workspace / ".agent_sessions")

            self.assertEqual(store.read_compacted_events("20260101-000000-aaaaaa"), [])


class CompactionCoverageMetricsTest(unittest.TestCase):
    def test_coverage_metrics_and_compacted_ids_in_payload(self) -> None:
        def call_model(messages):
            payload = json.loads(str(messages[0]["content"]).split("输入：\n", 1)[1])
            event_ids = [item["event_id"] for item in payload["source_events"]]
            user_messages = [
                {"text": item["payload"]["content"], "source_event_ids": [item["event_id"]]}
                for item in payload["source_events"]
                if item["type"] == "user_message"
                and isinstance(item.get("payload", {}).get("content"), str)
                and item["payload"]["content"].strip()
            ]
            # 只显式引用第一个被压缩事件。
            structured = {
                "objective": ["覆盖度指标"],
                "constraints": [
                    {"text": "只引用一个", "source_event_ids": [event_ids[0]]}
                ],
                "decisions": [],
                "completed": [],
                "current_state": [],
                "open_issues": [],
                "artifacts": [],
                "modified_files": [
                    {
                        "path": "src/a.py",
                        "description": "新增实现",
                        "source_event_ids": [event_ids[0]],
                    }
                ],
                "exact_evidence": [],
                "user_messages": user_messages,
            }
            return SummaryModelResponse(content=json.dumps(structured, ensure_ascii=False))

        with tempfile.TemporaryDirectory() as temp_dir:
            agent, store, session_id = _build_agent(Path(temp_dir))
            agent.config.context_compaction = ContextCompactionConfig(
                recent_turns=2,
                trigger_context_tokens=1,
            )
            agent._context_compaction_service_instance = ContextCompactionService(
                compactor=ModelSummaryCompactor(call_model)
            )

            LocalToolAgent.run_stream(agent, "第三轮问题", lambda _delta: None)
            events = store.read_session_events(session_id)

        summary = next(event.payload for event in events if event.type == "compact_summary")
        self.assertIn("compacted_event_ids", summary)
        self.assertTrue(summary["compacted_event_ids"])
        coverage = summary["coverage"]
        self.assertGreater(coverage["compacted_event_count"], 0)
        self.assertGreaterEqual(coverage["covered_event_count"], 0)
        self.assertGreaterEqual(coverage["referenced_event_count"], 1)
        self.assertGreater(coverage["coverage_ratio"], 0)
        self.assertGreaterEqual(coverage["coverage_ratio"], coverage["referenced_event_count"] / coverage["compacted_event_count"] - 1e-9)
        self.assertGreaterEqual(coverage["retired_token_estimate"], 0)
        self.assertEqual(coverage["field_counts"]["modified_files"], 1)


class CompactionArchiveIntegrationTest(unittest.TestCase):
    def test_automatic_compaction_archives_events_and_reports_coverage(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            workspace = Path(temp_dir)
            agent, store, session_id = _build_agent(workspace)
            agent.config.context_compaction = ContextCompactionConfig(
                recent_turns=2,
                trigger_context_tokens=1,
            )
            source_ids = [
                event.event_id
                for event in store.read_session_events(session_id)
            ]
            agent._context_compaction_service_instance = _summary_model(source_ids)

            LocalToolAgent.run_stream(agent, "第三轮问题", lambda _delta: None)
            events = store.read_session_events(session_id)

            event_types = [event.type for event in events]
            self.assertIn("context_compaction_measurement", event_types)
            self.assertIn("compact_summary", event_types)
            summary = next(
                event.payload for event in events if event.type == "compact_summary"
            )
            self.assertIn("archive_id", summary)
            measurement = next(
                event.payload
                for event in events
                if event.type == "context_compaction_measurement"
            )
            self.assertIn("coverage", measurement)
            self.assertEqual(measurement["archive_id"], summary["archive_id"])
            self.assertGreaterEqual(measurement["archived_event_count"], 1)

            archived = store.read_compacted_events(session_id)
            self.assertTrue(archived)
            archived_ids = {event["event_id"] for event in archived}
            self.assertTrue(
                set(summary["compacted_event_ids"]).issubset(archived_ids),
                "被压缩事件必须能从二级归档精确恢复",
            )

    def test_auto_memory_recall_injects_hits_and_records_event(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            workspace = Path(temp_dir)
            memory = MemoryStore(workspace / "memory")
            agent, store, session_id = _build_agent(workspace, memory_store=memory)
            agent.config.context_compaction = ContextCompactionConfig(
                recent_turns=2,
                trigger_context_tokens=1,
                auto_memory_recall=True,
            )
            source_ids = [
                event.event_id
                for event in store.read_session_events(session_id)
            ]
            agent._context_compaction_service_instance = _summary_model(source_ids)

            LocalToolAgent.run_stream(agent, "第三轮问题", lambda _delta: None)
            events = store.read_session_events(session_id)

        self.assertTrue(
            any(event.type == "compaction_memory_recall" for event in events),
            "压缩后应记录记忆检索事件",
        )
        self.assertTrue(
            any(
                isinstance(message.get("content"), str)
                and message["content"].startswith("记忆检索（压缩后自动补强）")
                for message in agent._history
            ),
            "记忆命中应注入后续模型上下文",
        )

    def test_auto_memory_recall_can_be_disabled(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            workspace = Path(temp_dir)
            memory = MemoryStore(workspace / "memory")
            agent, store, session_id = _build_agent(workspace, memory_store=memory)
            agent.config.context_compaction = ContextCompactionConfig(
                recent_turns=2,
                trigger_context_tokens=1,
                auto_memory_recall=False,
            )
            source_ids = [
                event.event_id
                for event in store.read_session_events(session_id)
            ]
            agent._context_compaction_service_instance = _summary_model(source_ids)

            LocalToolAgent.run_stream(agent, "第三轮问题", lambda _delta: None)
            events = store.read_session_events(session_id)

        self.assertFalse(
            any(event.type == "compaction_memory_recall" for event in events)
        )
        self.assertFalse(
            any(
                isinstance(message.get("content"), str)
                and message["content"].startswith("记忆检索（压缩后自动补强）")
                for message in agent._history
            )
        )


if __name__ == "__main__":
    unittest.main()
