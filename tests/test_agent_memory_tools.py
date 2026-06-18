from __future__ import annotations

import json
import unittest
from datetime import datetime, timezone
from typing import Any

from ai_voice_agent.agent_memory_tools import (
    memory_expand_related_result,
    memory_read_result,
    memory_search_result,
    memory_write_result,
)
from ai_voice_agent.memory import MemoryRecord, MemorySearchResult, MemoryWriteRequest


class FakeMemoryStore:
    def __init__(self) -> None:
        self.search_calls: list[dict[str, Any]] = []
        self.read_calls: list[list[str]] = []
        self.expand_calls: list[dict[str, Any]] = []
        self.write_calls: list[list[MemoryWriteRequest]] = []

    def search(
        self,
        *,
        query: str,
        candidate_directories: list[str] | None,
        max_results: int,
    ) -> list[MemorySearchResult]:
        self.search_calls.append(
            {
                "query": query,
                "candidate_directories": candidate_directories,
                "max_results": max_results,
            }
        )
        return [
            MemorySearchResult(
                id="20260618-120000",
                summary="摘要",
                storage_directory="project-context/general",
                related_directories=["project-context/general"],
                timestamp=datetime(2026, 6, 18, tzinfo=timezone.utc),
            )
        ]

    def read(self, memory_ids: list[str]) -> list[MemoryRecord]:
        self.read_calls.append(memory_ids)
        return [
            MemoryRecord(
                id=memory_ids[0],
                timestamp=datetime(2026, 6, 18, tzinfo=timezone.utc),
                related_directories=["project-context/general"],
                content="完整记忆",
            )
        ]

    def expand_related(
        self,
        memory_ids: list[str],
        *,
        max_depth: int,
        max_results: int,
    ) -> list[MemorySearchResult]:
        self.expand_calls.append(
            {
                "memory_ids": memory_ids,
                "max_depth": max_depth,
                "max_results": max_results,
            }
        )
        return []

    def write(self, memories: list[MemoryWriteRequest]) -> list[MemoryRecord]:
        self.write_calls.append(memories)
        return [
            MemoryRecord(
                id="20260618-120001",
                timestamp=datetime(2026, 6, 18, tzinfo=timezone.utc),
                related_directories=memories[0].related_directories,
                content=memories[0].content,
            )
        ]


class AgentMemoryToolsTest(unittest.TestCase):
    def test_memory_search_validates_required_query_and_reason(self) -> None:
        store = FakeMemoryStore()

        missing_query = memory_search_result(store, {"reason": "需要上下文"})
        missing_reason = memory_search_result(store, {"query": "项目约束"})

        self.assertFalse(missing_query.ok)
        self.assertEqual(missing_query.output, "query 不能为空。")
        self.assertFalse(missing_reason.ok)
        self.assertEqual(missing_reason.output, "reason 不能为空。")
        self.assertEqual(store.search_calls, [])

    def test_memory_search_passes_normalized_arguments_and_json_result(self) -> None:
        store = FakeMemoryStore()

        result = memory_search_result(
            store,
            {
                "query": " 项目约束 ",
                "reason": "恢复上下文",
                "candidate_directories": [" project-context/general ", "", 42],
                "max_results": 99,
            },
        )

        self.assertTrue(result.ok)
        self.assertEqual(
            store.search_calls,
            [
                {
                    "query": "项目约束",
                    "candidate_directories": ["project-context/general"],
                    "max_results": 20,
                }
            ],
        )
        self.assertEqual(json.loads(result.output)[0]["id"], "20260618-120000")

    def test_memory_read_and_expand_require_memory_ids(self) -> None:
        store = FakeMemoryStore()

        read_result = memory_read_result(store, {"memory_ids": []})
        expand_result = memory_expand_related_result(store, {"memory_ids": [None, ""]})

        self.assertFalse(read_result.ok)
        self.assertEqual(read_result.output, "memory_ids 不能为空。")
        self.assertFalse(expand_result.ok)
        self.assertEqual(expand_result.output, "memory_ids 不能为空。")

    def test_memory_write_validates_and_builds_requests(self) -> None:
        store = FakeMemoryStore()

        result = memory_write_result(
            store,
            {
                "memories": [
                    {
                        "content": " 关键决策 ",
                        "related_directories": ["task-history/general"],
                        "storage_directory": "task-history/general",
                        "source_event": None,
                    }
                ]
            },
        )

        self.assertTrue(result.ok)
        self.assertEqual(store.write_calls[0][0].content, "关键决策")
        self.assertEqual(store.write_calls[0][0].related_directories, ["task-history/general"])
        self.assertEqual(json.loads(result.output)[0]["content"], "关键决策")

    def test_memory_write_rejects_invalid_memory_payload(self) -> None:
        store = FakeMemoryStore()

        result = memory_write_result(
            store,
            {"memories": [{"content": "有效", "related_directories": ["ok", 1]}]},
        )

        self.assertFalse(result.ok)
        self.assertEqual(result.output, "第 1 条记忆 related_directories 必须是字符串列表。")
        self.assertEqual(store.write_calls, [])
