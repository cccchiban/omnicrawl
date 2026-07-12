"""Agent 子系统内部模块。

本文件由原合并入口按既有模块边界恢复，职责说明见模块内公开对象。
"""

from __future__ import annotations

from typing import Any

from .tools import (
    json_tool_result,
    read_limited_int,
    read_optional_string_list,
    read_required_string_list,
)
from .types import ToolResult
from ..memory import (
    MemoryStore,
    MemoryStoreError,
    MemoryWriteRequest,
    record_to_dict,
    search_result_to_dict,
)


def memory_search_result(store: MemoryStore, arguments: dict[str, Any]) -> ToolResult:
    """执行 memory_search 的参数校验和结果格式化，保持原工具返回语义。"""

    query = str(arguments.get("query") or "").strip()
    reason = str(arguments.get("reason") or "").strip()
    if not query:
        return ToolResult(ok=False, output="query 不能为空。")
    if not reason:
        return ToolResult(ok=False, output="reason 不能为空。")

    try:
        results = store.search(
            query=query,
            candidate_directories=read_optional_string_list(
                arguments,
                "candidate_directories",
            ),
            max_results=read_limited_int(arguments, "max_results", default=5, maximum=20),
        )
    except MemoryStoreError as exc:
        return ToolResult(ok=False, output=str(exc))

    return json_tool_result([search_result_to_dict(result) for result in results])


def memory_read_result(store: MemoryStore, arguments: dict[str, Any]) -> ToolResult:
    """执行 memory_read 的 id 校验和结果格式化。"""

    memory_ids = read_required_string_list(arguments, "memory_ids")
    if not memory_ids:
        return ToolResult(ok=False, output="memory_ids 不能为空。")

    try:
        records = store.read(memory_ids)
    except MemoryStoreError as exc:
        return ToolResult(ok=False, output=str(exc))

    return json_tool_result([record_to_dict(record) for record in records])


def memory_expand_related_result(store: MemoryStore, arguments: dict[str, Any]) -> ToolResult:
    """执行 memory_expand_related 的参数裁剪和结果格式化。"""

    memory_ids = read_required_string_list(arguments, "memory_ids")
    if not memory_ids:
        return ToolResult(ok=False, output="memory_ids 不能为空。")

    try:
        results = store.expand_related(
            memory_ids,
            max_depth=read_limited_int(arguments, "max_depth", default=1, maximum=3),
            max_results=read_limited_int(arguments, "max_results", default=5, maximum=20),
        )
    except MemoryStoreError as exc:
        return ToolResult(ok=False, output=str(exc))

    return json_tool_result([search_result_to_dict(result) for result in results])


def memory_write_result(store: MemoryStore, arguments: dict[str, Any]) -> ToolResult:
    """把模型提交的 JSON 记忆写入请求转换成 MemoryStore 可处理的结构。"""

    raw_memories = arguments.get("memories")
    if not isinstance(raw_memories, list) or not raw_memories:
        return ToolResult(ok=False, output="memories 必须是非空列表。")

    requests: list[MemoryWriteRequest] = []
    for index, raw_memory in enumerate(raw_memories, start=1):
        if not isinstance(raw_memory, dict):
            return ToolResult(ok=False, output=f"第 {index} 条记忆必须是 JSON 对象。")

        content = str(raw_memory.get("content") or "").strip()
        if not content:
            return ToolResult(ok=False, output=f"第 {index} 条记忆 content 不能为空。")

        related = raw_memory.get("related_directories", [])
        if not isinstance(related, list) or not all(isinstance(item, str) for item in related):
            return ToolResult(ok=False, output=f"第 {index} 条记忆 related_directories 必须是字符串列表。")

        storage_directory = raw_memory.get("storage_directory")
        if storage_directory is not None and not isinstance(storage_directory, str):
            return ToolResult(ok=False, output=f"第 {index} 条记忆 storage_directory 必须是字符串或 null。")

        source_event = raw_memory.get("source_event")
        if source_event is not None and not isinstance(source_event, str):
            return ToolResult(ok=False, output=f"第 {index} 条记忆 source_event 必须是字符串或 null。")

        requests.append(
            MemoryWriteRequest(
                content=content,
                related_directories=list(related),
                storage_directory=storage_directory,
                source_event=source_event,
            )
        )

    try:
        records = store.write(requests)
    except MemoryStoreError as exc:
        return ToolResult(ok=False, output=str(exc))

    return json_tool_result([record_to_dict(record) for record in records])
