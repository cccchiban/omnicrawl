from __future__ import annotations

import json
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch

from omnicrawl.agent import AgentModelReply, LocalToolAgent
from omnicrawl.agent.context_compaction import (
    ContextCompactionService,
    ModelSummaryCompactor,
    RuntimeSummaryModelAdapter,
    SummaryGenerationError,
    SummaryModelResponse,
    TokenUsageSample,
)
from omnicrawl.config.context_compaction import ContextCompactionConfig
from omnicrawl.llm import LLMConfig
from omnicrawl.session import SessionStore
from omnicrawl.slash_commands import build_slash_commands, handle_session_command


def _build_agent(workspace: Path, *, enabled: bool) -> tuple[LocalToolAgent, SessionStore, str]:
    agent = object.__new__(LocalToolAgent)
    agent.workspace_root = workspace
    agent.config = SimpleNamespace(
        max_history_turns=2,
        max_tool_output_chars=6_000,
        context_compaction=ContextCompactionConfig(enabled=enabled, recent_turns=2),
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


class ContextCompactionIntegrationTest(unittest.TestCase):
    def test_enabled_mode_only_measures_below_threshold(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            agent, store, session_id = _build_agent(Path(temp_dir), enabled=True)

            LocalToolAgent.run_stream(agent, "第三轮问题", lambda _delta: None)
            events = store.read_session_events(session_id)

        event_types = [event.type for event in events]
        self.assertIn("context_compaction_measurement", event_types)
        self.assertNotIn("compact_summary", event_types)
        measurement = next(
            event.payload for event in events if event.type == "context_compaction_measurement"
        )
        self.assertEqual(measurement["schema_version"], 1)
        self.assertEqual(measurement["usage"]["input_tokens"], 1_000)
        self.assertEqual(measurement["usage"]["cached_input_tokens"], 250)
        self.assertEqual(measurement["cache_hit_ratio"], 0.25)
        self.assertEqual(len(agent._history), 6)

    def test_enabled_mode_automatically_compacts_at_threshold(self) -> None:
        def call_model(messages):
            prompt = str(messages[0]["content"])
            payload = json.loads(prompt.split("输入：\n", 1)[1])
            event_ids = [item["event_id"] for item in payload["source_events"]]
            structured = {
                "objective": ["继续当前会话任务"],
                "constraints": [
                    {"text": "保留历史", "source_event_ids": [event_ids[0]]}
                ],
                "decisions": [],
                "completed": [],
                "current_state": ["自动压缩已完成"],
                "open_issues": [],
                "artifacts": [],
                "exact_evidence": [],
            }
            return SummaryModelResponse(
                content=json.dumps(structured, ensure_ascii=False),
                usage=TokenUsageSample(100, 20, 10),
                profile="test-summary",
                provider="test",
            )

        with tempfile.TemporaryDirectory() as temp_dir:
            agent, store, session_id = _build_agent(Path(temp_dir), enabled=True)
            agent.config.context_compaction = ContextCompactionConfig(
                enabled=True,
                recent_turns=2,
                trigger_context_tokens=1,
            )
            agent._context_compaction_service_instance = ContextCompactionService(
                compactor=ModelSummaryCompactor(call_model)
            )

            LocalToolAgent.run_stream(agent, "第三轮问题", lambda _delta: None)
            events = store.read_session_events(session_id)
            restored = store.load_session(session_id)
            expected_history = list(agent._history)
            agent.resume_session(session_id)
            resumed_history = list(agent._history)

        event_types = [event.type for event in events]
        self.assertLess(
            event_types.index("context_compaction_measurement"),
            event_types.index("compact_summary"),
        )
        summary = next(event.payload for event in events if event.type == "compact_summary")
        self.assertEqual(summary["schema_version"], 2)
        self.assertTrue(summary["model_generated"])
        self.assertTrue(agent._history[0]["content"].startswith("会话压缩摘要："))
        self.assertEqual(len(expected_history), 5)
        self.assertEqual(restored.messages, expected_history)
        self.assertEqual(resumed_history, expected_history)
        self.assertTrue(resumed_history[0]["content"].startswith("会话压缩摘要："))

    def test_automatic_compaction_waits_four_complete_turns_before_retry(self) -> None:
        calls = 0

        def call_model(messages):
            nonlocal calls
            calls += 1
            prompt = str(messages[0]["content"])
            payload = json.loads(prompt.split("输入：\n", 1)[1])
            source_ids = [item["event_id"] for item in payload["source_events"]]
            structured = {
                "objective": ["验证自动压缩冷却"],
                "constraints": [],
                "decisions": [],
                "completed": [],
                "current_state": ["继续运行"],
                "open_issues": [],
                "artifacts": [],
                "exact_evidence": [],
            }
            self.assertTrue(source_ids)
            return SummaryModelResponse(content=json.dumps(structured))

        with tempfile.TemporaryDirectory() as temp_dir:
            agent, store, session_id = _build_agent(Path(temp_dir), enabled=True)
            agent.config.context_compaction = ContextCompactionConfig(
                enabled=True,
                recent_turns=2,
                trigger_context_tokens=1,
                minimum_turns_between_model_compactions=4,
            )
            agent._context_compaction_service_instance = ContextCompactionService(
                compactor=ModelSummaryCompactor(call_model)
            )

            LocalToolAgent.run_stream(agent, "触发首次压缩", lambda _delta: None)
            self.assertEqual(calls, 1)
            for index in range(3):
                LocalToolAgent.run_stream(agent, f"冷却回合 {index}", lambda _delta: None)
            self.assertEqual(calls, 1)
            LocalToolAgent.run_stream(agent, "第四个完整回合", lambda _delta: None)
            events = store.read_session_events(session_id)

        model_summaries = [
            event
            for event in events
            if event.type == "compact_summary"
            and event.payload.get("model_generated") is True
        ]
        self.assertEqual(calls, 2)
        self.assertEqual(len(model_summaries), 2)

    def test_model_failure_falls_back_to_deterministic_compaction(self) -> None:
        def invalid_model(_messages):
            return SummaryModelResponse(content="not-json")

        with tempfile.TemporaryDirectory() as temp_dir:
            agent, store, session_id = _build_agent(Path(temp_dir), enabled=True)
            agent.config.context_compaction = ContextCompactionConfig(
                enabled=True,
                recent_turns=2,
                trigger_context_tokens=1,
            )
            agent._context_compaction_service_instance = ContextCompactionService(
                compactor=ModelSummaryCompactor(invalid_model)
            )

            LocalToolAgent.run_stream(agent, "第三轮问题", lambda _delta: None)
            events = store.read_session_events(session_id)

        event_types = [event.type for event in events]
        self.assertIn("context_compaction_failed", event_types)
        self.assertIn("compact_summary", event_types)
        summary = next(event.payload for event in events if event.type == "compact_summary")
        self.assertNotIn("schema_version", summary)
        self.assertTrue(agent._history[0]["content"].startswith("会话压缩摘要："))

    def test_invalid_structured_summary_falls_back_after_validation_retry(self) -> None:
        prompts: list[str] = []

        def invalid_sources(messages):
            prompt = str(messages[0]["content"])
            prompts.append(prompt)
            structured = {
                "objective": ["无效来源"],
                "constraints": [
                    {"text": "错误引用", "source_event_ids": ["missing-event"]}
                ],
                "decisions": [],
                "completed": [],
                "current_state": [],
                "open_issues": [],
                "artifacts": [],
                "exact_evidence": [],
            }
            return SummaryModelResponse(content=json.dumps(structured))

        with tempfile.TemporaryDirectory() as temp_dir:
            agent, store, session_id = _build_agent(Path(temp_dir), enabled=True)
            agent.config.context_compaction = ContextCompactionConfig(
                enabled=True,
                recent_turns=2,
                trigger_context_tokens=1,
            )
            agent._context_compaction_service_instance = ContextCompactionService(
                compactor=ModelSummaryCompactor(invalid_sources)
            )

            LocalToolAgent.run_stream(agent, "触发校验失败", lambda _delta: None)
            events = store.read_session_events(session_id)

        self.assertEqual(len(prompts), 2)
        self.assertIn("validation_feedback", prompts[1])
        failed = next(
            event for event in events if event.type == "context_compaction_failed"
        )
        self.assertIn("结构化摘要校验失败", failed.payload["reason"])
        fallback = next(event for event in events if event.type == "compact_summary")
        self.assertNotIn("schema_version", fallback.payload)

    def test_manual_model_compaction_keeps_last_complete_turn(self) -> None:
        def call_model(messages):
            prompt = str(messages[0]["content"])
            payload = json.loads(prompt.split("输入：\n", 1)[1])
            event_id = payload["source_events"][0]["event_id"]
            structured = {
                "objective": ["手动模型压缩"],
                "constraints": [
                    {"text": "保留完整转录", "source_event_ids": [event_id]}
                ],
                "decisions": [],
                "completed": [],
                "current_state": ["已完成"],
                "open_issues": [],
                "artifacts": [],
                "exact_evidence": [],
            }
            return SummaryModelResponse(content=json.dumps(structured, ensure_ascii=False))

        with tempfile.TemporaryDirectory() as temp_dir:
            agent, store, session_id = _build_agent(Path(temp_dir), enabled=True)
            agent._context_compaction_service_instance = ContextCompactionService(
                compactor=ModelSummaryCompactor(call_model)
            )

            summary_text = agent.compact_conversation_model()
            events = store.read_session_events(session_id)

        self.assertIn("结构化工作摘要", summary_text)
        compact = next(event.payload for event in events if event.type == "compact_summary")
        self.assertTrue(compact["manual"])
        self.assertEqual(len(agent._history), 3)

    def test_cross_provider_summary_requires_explicit_permission(self) -> None:
        parent = LLMConfig(
            api_key="test",
            base_url="https://example.test/v1",
            model="main",
            provider="openai",
            profile_id="main",
        )
        selected = LLMConfig(
            api_key="test",
            base_url="https://example.test/v1",
            model="cheap",
            provider="anthropic",
            profile_id="other",
        )
        adapter = RuntimeSummaryModelAdapter(
            parent_llm=parent,
            summary_profile="other/cheap",
            reasoning_effort="low",
            allow_cross_provider=False,
            workspace_root=Path.cwd(),
        )

        with patch(
            "omnicrawl.agent.context_compaction.summary.apply_model_selection",
            return_value=selected,
        ):
            with self.assertRaises(SummaryGenerationError):
                adapter.resolve_model_config()

    def test_plain_compact_command_remains_deterministic_when_feature_is_enabled(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            agent, store, session_id = _build_agent(Path(temp_dir), enabled=True)

            message = handle_session_command(agent, "/compact")
            events = store.read_session_events(session_id)

        self.assertIn("已压缩当前会话", message or "")
        compact = next(event.payload for event in events if event.type == "compact_summary")
        self.assertNotIn("schema_version", compact)
        self.assertNotIn("model_generated", compact)

    def test_model_compact_command_is_listed_and_refuses_disabled_feature(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            agent, _store, _session_id = _build_agent(Path(temp_dir), enabled=False)

            message = handle_session_command(agent, "/compact --model")

        self.assertIn("模型压缩功能已关闭", message or "")
        self.assertIn("/compact --model", build_slash_commands(agent))

    def test_disabled_mode_preserves_deterministic_compaction(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            agent, store, session_id = _build_agent(Path(temp_dir), enabled=False)

            LocalToolAgent.run_stream(agent, "第三轮问题", lambda _delta: None)
            events = store.read_session_events(session_id)

        event_types = [event.type for event in events]
        self.assertNotIn("context_compaction_measurement", event_types)
        self.assertIn("compact_summary", event_types)
        self.assertTrue(agent._history[0]["content"].startswith("会话压缩摘要："))

    def test_service_failure_does_not_break_completed_turn(self) -> None:
        class FailingService:
            def after_complete_turn(self, **_kwargs):
                raise RuntimeError("measurement failed")

        with tempfile.TemporaryDirectory() as temp_dir:
            agent, store, session_id = _build_agent(Path(temp_dir), enabled=True)
            agent.config.context_compaction = ContextCompactionConfig(
                enabled=True,
                recent_turns=2,
                trigger_context_tokens=1,
            )
            agent._context_compaction_service_instance = FailingService()

            with self.assertLogs("omnicrawl.agent.core", level="WARNING"):
                LocalToolAgent.run_stream(agent, "第三轮问题", lambda _delta: None)
            events = store.read_session_events(session_id)

        event_types = [event.type for event in events]
        self.assertNotIn("context_compaction_measurement", event_types)
        self.assertNotIn("compact_summary", event_types)
        self.assertEqual(len(agent._history), 6)


if __name__ == "__main__":
    unittest.main()
