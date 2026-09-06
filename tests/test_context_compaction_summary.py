from __future__ import annotations

import json
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import Mock, patch

from omnicrawl.agent.context_compaction import (
    CompactionBatch,
    ContextAssembler,
    ModelSummaryCompactor,
    RuntimeSummaryModelAdapter,
    SourceEvent,
    SummaryModelResponse,
    SummaryValidator,
    TokenUsageSample,
    load_summary_prompt,
    render_summary_markdown,
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
        "user_messages": [
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

    def test_unlimited_budget_is_communicated_to_model(self) -> None:
        prompts: list[dict] = []

        def call_model(messages):
            payload = json.loads(str(messages[0]["content"]).split("输入：\n", 1)[1])
            prompts.append(payload)
            return SummaryModelResponse(
                content=json.dumps(_structured()),
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
        # target=0 → 向模型传 null + budget_limited=false（完整性优先）。
        ModelSummaryCompactor(call_model).compact(batch, target_summary_tokens=0)
        self.assertIsNone(prompts[0]["target_summary_tokens"])
        self.assertFalse(prompts[0]["budget_limited"])

        # target>0 → 正常传预算并标记受限。
        ModelSummaryCompactor(call_model).compact(batch, target_summary_tokens=6_000)
        self.assertEqual(prompts[1]["target_summary_tokens"], 6_000)
        self.assertTrue(prompts[1]["budget_limited"])

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

    def test_unlimited_budget_never_rejects_for_length(self) -> None:
        # target=0 时即使摘要远超常规预算也不应被长度校验拒绝。
        structured = _structured()
        structured["objective"] = ["目标" + "内容" * 2_000]
        structured["current_state"] = ["状态" + "细节" * 2_000]
        structured["completed"] = [
            {"text": "完成" + "项" * 1_000, "source_event_ids": ["e2"]}
        ]

        result = SummaryValidator().validate(
            structured,
            source_events=self.events,
            target_summary_tokens=0,
        )

        self.assertTrue(result.valid, result.errors)

    def test_completeness_requires_modified_files_to_cover_successful_writes(self) -> None:
        events = (
            *self.events,
            SourceEvent(
                "e_w",
                "tool_call_requested",
                {
                    "tool": "write_file",
                    "arguments": {"path": "src/a.py"},
                    "tool_call_id": "c1",
                },
            ),
            SourceEvent(
                "e_wr",
                "tool_result",
                {"tool": "write_file", "tool_call_id": "c1", "ok": True},
            ),
        )
        # 成功写入文件但 modified_files 为空 → 完整性校验失败。
        structured = _structured()
        result = SummaryValidator().validate(
            structured,
            source_events=events,
            target_summary_tokens=6_000,
            completeness_events=events,
        )
        self.assertFalse(result.valid)
        self.assertTrue(any("modified_files 缺少" in error for error in result.errors))

        # 通过引用事件 ID 覆盖 → 通过。
        structured["modified_files"] = [
            {
                "path": "src/a.py",
                "description": "新增实现",
                "source_event_ids": ["e_w"],
            }
        ]
        result = SummaryValidator().validate(
            structured,
            source_events=events,
            target_summary_tokens=6_000,
            completeness_events=events,
        )
        self.assertTrue(result.valid, result.errors)

        # 通过宽松 path 匹配（相对/绝对差异）覆盖 → 通过。
        structured["modified_files"] = [
            {
                "path": "./src/a.py",
                "description": "新增实现",
                "source_event_ids": ["e1"],
            }
        ]
        result = SummaryValidator().validate(
            structured,
            source_events=events,
            target_summary_tokens=6_000,
            completeness_events=events,
        )
        self.assertTrue(result.valid, result.errors)

    def test_completeness_requires_failed_attempts_to_cover_failed_calls(self) -> None:
        events = (
            *self.events,
            SourceEvent(
                "e_f",
                "tool_call_requested",
                {
                    "tool": "bash",
                    "arguments": {"command": "pytest"},
                    "tool_call_id": "c2",
                },
            ),
            SourceEvent(
                "e_fr",
                "tool_result",
                {"tool": "bash", "tool_call_id": "c2", "ok": False},
            ),
        )
        # 失败调用但 failed_attempts 为空 → 完整性校验失败。
        structured = _structured()
        result = SummaryValidator().validate(
            structured,
            source_events=events,
            target_summary_tokens=6_000,
            completeness_events=events,
        )
        self.assertFalse(result.valid)
        self.assertTrue(any("failed_attempts 缺少" in error for error in result.errors))

        # 覆盖（引用调用事件或结果事件均可）→ 通过。
        structured["failed_attempts"] = [
            {"text": "pytest 失败，待修", "source_event_ids": ["e_f"]}
        ]
        result = SummaryValidator().validate(
            structured,
            source_events=events,
            target_summary_tokens=6_000,
            completeness_events=events,
        )
        self.assertTrue(result.valid, result.errors)

    def test_completeness_is_skipped_without_scope(self) -> None:
        # 未提供 completeness_events（旧调用方）时不做完整性校验。
        events = (
            *self.events,
            SourceEvent(
                "e_w",
                "tool_call_requested",
                {
                    "tool": "write_file",
                    "arguments": {"path": "src/a.py"},
                    "tool_call_id": "c1",
                },
            ),
            SourceEvent(
                "e_wr",
                "tool_result",
                {"tool": "write_file", "tool_call_id": "c1", "ok": True},
            ),
        )
        structured = _structured()
        result = SummaryValidator().validate(
            structured,
            source_events=events,
            target_summary_tokens=6_000,
        )
        self.assertTrue(result.valid, result.errors)

    def test_large_user_message_is_exempt_from_verbatim_coverage(self) -> None:
        # 超过切分阈值（60K 字符）的超大用户消息：摘要模型只收到分块，
        # 无法逐字保留，强校验对此豁免，避免必然失败后降级丢失全部信息。
        large_content = "超长指令" * 30_000  # 90000 字符
        events = (
            SourceEvent("e1", "user_message", {"content": large_content}),
            SourceEvent("e2", "assistant_message", {"content": "已处理"}),
        )
        structured = _structured()
        # exact_evidence 要求逐字匹配引用事件原文，超大消息原文无法逐字提供，
        # 移除该字段只验证 user_messages 豁免本身。
        structured["exact_evidence"] = []
        result = SummaryValidator().validate(
            structured,
            source_events=events,
            target_summary_tokens=6_000,
            completeness_events=events,
        )
        self.assertTrue(result.valid, result.errors)

    def test_user_messages_must_preserve_verbatim_text(self) -> None:
        # 原文逐字一致 → 通过。
        structured = _structured()
        result = SummaryValidator().validate(
            structured,
            source_events=self.events,
            target_summary_tokens=6_000,
        )
        self.assertTrue(result.valid, result.errors)

        # 改写/截断原文 → 拒绝。
        structured["user_messages"] = [
            {"text": "不要删除（概括）", "source_event_ids": ["e1"]}
        ]
        result = SummaryValidator().validate(
            structured,
            source_events=self.events,
            target_summary_tokens=6_000,
        )
        self.assertFalse(result.valid)
        self.assertTrue(any("不是用户消息原文" in error for error in result.errors))

        # 引用非用户消息事件 → 拒绝。
        structured["user_messages"] = [
            {"text": "已运行 pytest", "source_event_ids": ["e2"]}
        ]
        result = SummaryValidator().validate(
            structured,
            source_events=self.events,
            target_summary_tokens=6_000,
        )
        self.assertFalse(result.valid)
        self.assertTrue(any("非用户消息事件" in error for error in result.errors))

    def test_completeness_requires_user_messages_to_cover_all_user_messages(self) -> None:
        events = self.events  # e1 是 user_message。
        structured = _structured()
        structured["user_messages"] = []

        # 被压缩窗口内存在用户消息但 user_messages 为空 → 完整性校验失败。
        result = SummaryValidator().validate(
            structured,
            source_events=events,
            target_summary_tokens=6_000,
            completeness_events=events,
        )
        self.assertFalse(result.valid)
        self.assertTrue(any("user_messages 缺少" in error for error in result.errors))

        # 引用事件 ID 覆盖 → 通过。
        structured["user_messages"] = [
            {"text": "不要删除文件", "source_event_ids": ["e1"]}
        ]
        result = SummaryValidator().validate(
            structured,
            source_events=events,
            target_summary_tokens=6_000,
            completeness_events=events,
        )
        self.assertTrue(result.valid, result.errors)


class SummaryRenderingTest(unittest.TestCase):
    def test_render_summary_markdown_includes_process_and_negative_fields(self) -> None:
        structured = {
            "objective": ["目标"],
            "constraints": [],
            "decisions": [],
            "completed": [],
            "current_state": [],
            "open_issues": [],
            "artifacts": [],
            "read_files": [
                {
                    "path": "src/a.py",
                    "description": "确认接口签名",
                    "source_event_ids": ["e1"],
                }
            ],
            "modified_files": [
                {
                    "path": "src/b.py",
                    "description": "新增实现",
                    "source_event_ids": ["e2"],
                }
            ],
            "failed_attempts": [
                {"text": "pytest 失败，待修", "source_event_ids": ["e3"]}
            ],
            "excluded_approaches": [
                {"text": "放弃方案 A", "source_event_ids": ["e4"]}
            ],
            "exact_evidence": [],
        }

        markdown = render_summary_markdown(structured)

        self.assertIn("### 已读文件", markdown)
        self.assertIn("src/a.py：确认接口签名（来源：e1）", markdown)
        self.assertIn("### 修改文件", markdown)
        self.assertIn("src/b.py：新增实现（来源：e2）", markdown)
        self.assertIn("### 失败尝试", markdown)
        self.assertIn("pytest 失败，待修", markdown)
        self.assertIn("### 已排除方案", markdown)
        self.assertIn("放弃方案 A", markdown)

    def test_render_summary_markdown_includes_nine_part_fields(self) -> None:
        structured = {
            "objective": ["目标"],
            "key_concepts": [
                {"text": "上下文压缩采用事件溯源", "source_event_ids": ["e1"]}
            ],
            "constraints": [],
            "decisions": [],
            "completed": [],
            "current_state": [],
            "open_issues": [],
            "next_steps": [
                {"text": "继续实现 L2 关键词检索", "source_event_ids": ["e2"]}
            ],
            "artifacts": [],
            "read_files": [],
            "modified_files": [],
            "failed_attempts": [],
            "problem_solving_process": [
                {"text": "先复现再修复", "source_event_ids": ["e3"]}
            ],
            "excluded_approaches": [],
            "user_messages": [
                {"text": "开始实现L3", "source_event_ids": ["e4"]}
            ],
            "exact_evidence": [],
        }

        markdown = render_summary_markdown(structured)

        self.assertIn("### 关键技术概念", markdown)
        self.assertIn("上下文压缩采用事件溯源", markdown)
        self.assertIn("### 问题解决过程", markdown)
        self.assertIn("先复现再修复", markdown)
        self.assertIn("### 用户消息原文", markdown)
        self.assertIn("开始实现L3", markdown)
        self.assertIn("### 可能的下一步", markdown)
        self.assertIn("继续实现 L2 关键词检索", markdown)

    def test_assembler_projects_summary_then_recent_events(self) -> None:
        structured = {
            "objective": ["目标"],
            "constraints": [],
            "decisions": [],
            "completed": [],
            "current_state": [],
            "open_issues": [],
            "artifacts": [],
            "read_files": [],
            "modified_files": [],
            "failed_attempts": [],
            "excluded_approaches": [],
            "exact_evidence": [],
        }
        recent = (SourceEvent("r1", "user_message", {"content": "继续"}),)

        messages = ContextAssembler().assemble(structured, recent)

        self.assertTrue(str(messages[0]["content"]).startswith("会话压缩摘要："))
        self.assertEqual(messages[1], {"role": "user", "content": "继续"})

    def test_assembler_projects_summary_recent_events_then_final_reply_anchor(self) -> None:
        structured = {
            "objective": ["目标"],
            "constraints": [],
            "decisions": [],
            "completed": [],
            "current_state": [],
            "open_issues": [],
            "artifacts": [],
            "read_files": [],
            "modified_files": [],
            "failed_attempts": [],
            "excluded_approaches": [],
            "exact_evidence": [],
        }
        recent = (
            SourceEvent("r1", "user_message", {"content": "继续"}),
            SourceEvent("r2", "assistant_message", {"content": "最近回复"}),
        )

        messages = ContextAssembler().assemble(
            structured,
            recent,
            final_reply_event=recent[1],
        )

        self.assertTrue(str(messages[0]["content"]).startswith("会话压缩摘要："))
        self.assertEqual(messages[1], {"role": "user", "content": "继续"})
        self.assertEqual(messages[2], {"role": "assistant", "content": "最近回复"})

    def test_assembler_skips_empty_final_reply_anchor(self) -> None:
        structured = {
            "objective": ["目标"],
            "constraints": [],
            "decisions": [],
            "completed": [],
            "current_state": [],
            "open_issues": [],
            "artifacts": [],
            "read_files": [],
            "modified_files": [],
            "failed_attempts": [],
            "excluded_approaches": [],
            "exact_evidence": [],
        }

        messages = ContextAssembler().assemble(
            structured,
            (),
            final_reply_event=SourceEvent(
                "empty", "assistant_message", {"content": "  "}
            ),
        )

        self.assertEqual(len(messages), 1)


if __name__ == "__main__":
    unittest.main()
