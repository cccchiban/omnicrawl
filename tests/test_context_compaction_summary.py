from __future__ import annotations

import json
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import Mock, patch

from omnicrawl.agent.context_compaction import (
    CompactionBatch,
    ModelSummaryCompactor,
    RuntimeSummaryModelAdapter,
    SourceEvent,
    SummaryModelResponse,
    SummaryValidator,
    TokenUsageSample,
    load_summary_prompt,
)
from omnicrawl.llm import LLMConfig


def _structured() -> dict:
    return {
        "objective": ["实现上下文压缩"],
        "constraints": [
            {"text": "不要删除文件", "source_event_ids": ["e1"]}
        ],
        "decisions": [],
        "completed": [
            {"text": "已运行 pytest", "source_event_ids": ["e2"]}
        ],
        "current_state": ["实现中"],
        "open_issues": [],
        "artifacts": [],
        "exact_evidence": [
            {"text": "不要删除文件", "source_event_ids": ["e1"]}
        ],
    }


class RuntimeSummaryModelAdapterTest(unittest.TestCase):
    def test_should_return_summary_response_when_runtime_request_succeeds(self) -> None:
        parent_llm = LLMConfig(
            api_key="test",
            base_url="https://example.test/v1",
            model="main",
            provider="openai",
            profile_id="main",
        )
        manager = SimpleNamespace(bootstrap=Mock(return_value=object()), close=Mock())

        def request_reply(*args, **_kwargs):
            args[2](100, 20, 10)
            return SimpleNamespace(content="{}")

        protocol = SimpleNamespace(request_reply=request_reply)
        adapter = RuntimeSummaryModelAdapter(
            parent_llm=parent_llm,
            summary_profile="",
            reasoning_effort="low",
            allow_cross_provider=False,
            workspace_root=Path.cwd(),
        )

        with (
            patch(
                "omnicrawl.agent.context_compaction.summary.ModelRuntimeManager",
                return_value=manager,
            ),
            patch(
                "omnicrawl.agent.context_compaction.summary.llm_config_to_profile_and_descriptor",
                return_value=(object(), object()),
            ),
            patch(
                "omnicrawl.agent.context_compaction.summary.AgentLLMProtocol",
                return_value=protocol,
            ),
        ):
            response = adapter(({"role": "user", "content": "summarize"},))

        self.assertEqual(response.content, "{}")
        self.assertEqual(response.usage, TokenUsageSample(100, 20, 10))
        manager.close.assert_called_once_with()


class ModelSummaryCompactorTest(unittest.TestCase):
    def test_prompt_is_packaged_and_fenced_json_is_parsed(self) -> None:
        calls = []

        def call_model(messages):
            calls.append(messages)
            return SummaryModelResponse(
                content="```json\n" + json.dumps(_structured(), ensure_ascii=False) + "\n```",
                usage=TokenUsageSample(100, 20, 10),
                profile="cheap",
                provider="openai",
            )

        batch = CompactionBatch(
            events=(
                SourceEvent("e1", "user_message", {"content": "不要删除文件"}),
                SourceEvent("e2", "assistant_message", {"content": "已运行 pytest"}),
            ),
            recent_events=(),
        )
        result = ModelSummaryCompactor(call_model).compact(
            batch,
            target_summary_tokens=6_000,
        )

        self.assertIn("会话状态压缩器", load_summary_prompt())
        self.assertEqual(result.structured["objective"], ["实现上下文压缩"])
        self.assertEqual(result.usage.input_tokens, 100)
        self.assertEqual(len(calls), 1)

    def test_retries_once_after_malformed_json_response(self) -> None:
        prompts: list[str] = []

        def call_model(messages):
            prompts.append(str(messages[0]["content"]))
            content = "not-json" if len(prompts) == 1 else json.dumps(_structured())
            return SummaryModelResponse(
                content=content,
                usage=TokenUsageSample(10, 2, 1),
                profile="cheap",
                provider="openai",
            )

        batch = CompactionBatch(
            events=(
                SourceEvent("e1", "user_message", {"content": "不要删除文件"}),
                SourceEvent("e2", "assistant_message", {"content": "已运行 pytest"}),
            ),
            recent_events=(),
        )
        result = ModelSummaryCompactor(call_model).compact(
            batch,
            target_summary_tokens=6_000,
        )

        self.assertEqual(result.attempts, 2)
        self.assertEqual(result.usage.input_tokens, 20)
        self.assertEqual(len(prompts), 2)
        self.assertIn("response_error", prompts[1])

    def test_splits_oversized_event_and_merges_partial_summaries(self) -> None:
        operations: list[str] = []

        def call_model(messages):
            payload = json.loads(str(messages[0]["content"]).split("输入：\n", 1)[1])
            operations.append(payload["operation"])
            return SummaryModelResponse(
                content=json.dumps(_structured()),
                usage=TokenUsageSample(10, 2, 1),
                profile="cheap",
                provider="openai",
            )

        batch = CompactionBatch(
            events=(
                SourceEvent(
                    "e1",
                    "tool_result",
                    {"output": "x" * 3_000},
                ),
            ),
            recent_events=(),
        )
        result = ModelSummaryCompactor(
            call_model,
            max_input_tokens=500,
        ).compact(batch, target_summary_tokens=6_000)

        self.assertGreater(operations.count("extract_chunk"), 1)
        self.assertEqual(operations[-1], "merge_chunks")
        self.assertEqual(result.attempts, len(operations))
        self.assertEqual(result.usage.input_tokens, 10 * len(operations))


class SummaryValidatorTest(unittest.TestCase):
    def setUp(self) -> None:
        self.events = (
            SourceEvent("e1", "user_message", {"content": "不要删除文件"}),
            SourceEvent("e2", "assistant_message", {"content": "已运行 pytest"}),
        )

    def test_validates_schema_sources_and_exact_evidence(self) -> None:
        result = SummaryValidator().validate(
            _structured(),
            source_events=self.events,
            target_summary_tokens=6_000,
        )
        self.assertTrue(result.valid, result.errors)
        self.assertIsNotNone(result.normalized)

    def test_exact_evidence_preserves_multiline_source_text(self) -> None:
        structured = _structured()
        structured["exact_evidence"] = [
            {"text": "line one\nline two", "source_event_ids": ["e3"]}
        ]
        events = (
            *self.events,
            SourceEvent("e3", "tool_result", {"output": "line one\nline two"}),
        )

        result = SummaryValidator().validate(
            structured,
            source_events=events,
            target_summary_tokens=6_000,
        )

        self.assertTrue(result.valid, result.errors)

    def test_can_explicitly_drop_exact_evidence(self) -> None:
        structured = _structured()
        structured["exact_evidence"][0]["text"] = "not in source"

        result = SummaryValidator().validate(
            structured,
            source_events=self.events,
            target_summary_tokens=6_000,
            preserve_exact_evidence=False,
        )

        self.assertTrue(result.valid, result.errors)
        assert result.normalized is not None
        self.assertEqual(result.normalized["exact_evidence"], [])

    def test_rejects_unknown_source_event(self) -> None:
        structured = _structured()
        structured["constraints"][0]["source_event_ids"] = ["missing"]

        result = SummaryValidator().validate(
            structured,
            source_events=self.events,
            target_summary_tokens=6_000,
        )

        self.assertFalse(result.valid)
        self.assertTrue(any("不存在的事件" in error for error in result.errors))


if __name__ == "__main__":
    unittest.main()
