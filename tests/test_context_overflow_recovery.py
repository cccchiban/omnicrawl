from __future__ import annotations

import json
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace

from omnicrawl.agent import AgentError, AgentModelReply, LocalToolAgent
from omnicrawl.agent.llm_protocol import AgentProtocolError
from omnicrawl.agent.context_compaction import (
    ContextCompactionService,
    ModelSummaryCompactor,
    SummaryModelResponse,
    TokenUsageSample,
)
from omnicrawl.config.context_compaction import ContextCompactionConfig
from omnicrawl.llm.errors import ModelError, ModelErrorCode
from omnicrawl.session import SessionStore


RECOVERY_PROMPT = "请依据上方的结构化工作摘要继续完成当前任务。"


def _build_agent(workspace: Path) -> tuple[LocalToolAgent, SessionStore, str]:
    agent = object.__new__(LocalToolAgent)
    agent.workspace_root = workspace
    agent.config = SimpleNamespace(
        max_history_turns=2,
        max_tool_output_chars=6_000,
        context_compaction=ContextCompactionConfig(enabled=True, recent_turns=2),
        llm=SimpleNamespace(context_window_tokens=128_000),
    )
    agent._history = []
    agent._skill_manager = None
    agent._active_skills = []
    agent._tools = {}
    agent._system_prompt_template = "system"
    store = SessionStore(workspace / ".agent_sessions")
    state = store.start_session(workspace)
    agent._session_store = store
    agent._session_state = state

    def call_summary_model(messages):
        prompt = str(messages[0]["content"])
        payload = json.loads(prompt.split("输入：\n", 1)[1])
        event_ids = [item["event_id"] for item in payload["source_events"]]
        reference_ids = event_ids or [
            ref
            for item in payload.get("partial_summaries", [])
            if isinstance(item, dict)
            for field in ("constraints", "decisions", "completed", "open_issues", "artifacts", "exact_evidence")
            for entry in item.get(field, [])
            if isinstance(entry, dict)
            for ref in entry.get("source_event_ids", [])
            if isinstance(ref, str)
        ]
        structured = {
            "objective": ["完成当前长任务"],
            "constraints": [],
            "decisions": [],
            "completed": [],
            "current_state": ["主模型请求因上下文过长而需要从摘要继续"],
            "open_issues": [],
            "artifacts": [],
            "exact_evidence": [],
        }
        if reference_ids:
            structured["current_state"] = ["主模型请求因上下文过长而需要从摘要继续"]
        return SummaryModelResponse(
            content=json.dumps(structured, ensure_ascii=False),
            usage=TokenUsageSample(100, 20, 10),
            profile="test-summary",
            provider="test",
        )

    agent._context_compaction_service_instance = ContextCompactionService(
        compactor=ModelSummaryCompactor(call_summary_model)
    )
    return agent, store, state.session_id


class ContextOverflowRecoveryTest(unittest.TestCase):
    def test_should_compact_and_resume_same_turn_when_first_request_exceeds_context(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            agent, store, session_id = _build_agent(Path(temp_dir))
            requests: list[list[dict[str, str]]] = []
            statuses: list[str] = []

            def fake_request(
                messages,
                _on_delta,
                _on_token_usage,
                _on_protocol_wait,
                _on_retry_status,
            ):
                requests.append(list(messages))
                if len(requests) == 1:
                    raise AgentError("maximum context length exceeded")
                self.assertTrue(
                    any(
                        message["content"].startswith("会话压缩摘要：")
                        for message in messages
                    )
                )
                self.assertEqual(messages[-1], {"role": "user", "content": RECOVERY_PROMPT})
                return AgentModelReply(
                    message={"role": "assistant", "content": "已从压缩上下文继续完成。"},
                    content="已从压缩上下文继续完成。",
                )

            agent._request_agent_reply = fake_request  # type: ignore[method-assign]

            result = LocalToolAgent.run_stream(
                agent,
                "请处理" + "非常长的输入" * 20_000,
                lambda _delta: None,
                on_status=statuses.append,
            )
            events = store.read_session_events(session_id)
            restored = store.load_session(session_id)

        self.assertEqual(result, "已从压缩上下文继续完成。")
        self.assertEqual(len(requests), 2)
        self.assertTrue(any("上下文" in status and "压缩" in status for status in statuses))
        self.assertEqual(
            [event.type for event in events[:6]],
            [
                "session_started",
                "user_message",
                "compact_summary",
                "context_overflow_recovery",
                "user_message",
                "assistant_message",
            ],
        )
        self.assertEqual(events[-1].type, "context_compaction_measurement")
        compact = next(event.payload for event in events if event.type == "compact_summary")
        self.assertEqual(compact["decision_reason"], "context_overflow_recovery")
        self.assertTrue(compact["single_large_turn"])
        self.assertFalse(any(event.type == "session_interrupted" for event in events))
        self.assertEqual(restored.messages, agent._history)
        self.assertEqual(agent._history[-2], {"role": "user", "content": RECOVERY_PROMPT})

    def test_should_compact_and_resume_when_protocol_wraps_context_length_model_error(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            agent, _store, _session_id = _build_agent(Path(temp_dir))
            calls = 0

            def fake_request(
                _messages,
                _on_delta,
                _on_token_usage,
                _on_protocol_wait,
                _on_retry_status,
            ):
                nonlocal calls
                calls += 1
                if calls == 1:
                    model_error = ModelError(
                        code=ModelErrorCode.CONTEXT_LENGTH_EXCEEDED,
                        message="模型服务拒绝请求：输入上下文超过该模型的容量上限。",
                        status_code=422,
                    )
                    protocol_error = AgentProtocolError("request failed")
                    protocol_error.__cause__ = model_error
                    raise AgentError("Agent 请求失败") from protocol_error
                return AgentModelReply(
                    message={"role": "assistant", "content": "已恢复。"},
                    content="已恢复。",
                )

            agent._request_agent_reply = fake_request  # type: ignore[method-assign]

            result = LocalToolAgent.run_stream(agent, "很长的任务", lambda _delta: None)

        self.assertEqual(result, "已恢复。")
        self.assertEqual(calls, 2)

    def test_should_not_recover_rate_limit_with_token_marker(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            agent, _store, _session_id = _build_agent(Path(temp_dir))
            error = ModelError(
                code=ModelErrorCode.RATE_LIMITED,
                message="模型服务限流：token limit exceeded for this minute。",
                status_code=429,
            )

            should_recover = agent._can_recover_context_overflow(
                error,
                visible_output_seen=False,
            )

        self.assertFalse(should_recover)

    def test_should_not_retry_after_context_error_streams_visible_text(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            agent, store, session_id = _build_agent(Path(temp_dir))
            calls = 0
            deltas: list[str] = []

            def fake_request(
                _messages,
                on_delta,
                _on_token_usage,
                _on_protocol_wait,
                _on_retry_status,
            ):
                nonlocal calls
                calls += 1
                on_delta("已经输出的片段")
                raise AgentError("maximum context length exceeded")

            agent._request_agent_reply = fake_request  # type: ignore[method-assign]

            with self.assertRaisesRegex(AgentError, "maximum context length exceeded"):
                LocalToolAgent.run_stream(agent, "超长任务", deltas.append)
            events = store.read_session_events(session_id)

        self.assertEqual(calls, 1)
        self.assertEqual(deltas, ["已经输出的片段"])
        self.assertNotIn("compact_summary", [event.type for event in events])
        self.assertIn("session_interrupted", [event.type for event in events])


if __name__ == "__main__":
    unittest.main()
