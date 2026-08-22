"""知识库工具的 ToolResult 适配层。

核心业务逻辑在 :mod:`omnicrawl.knowledge`；这里负责参数校验、
错误转换和 JSON 化输出，供 Agent 工具表注册使用。
"""

from __future__ import annotations

from typing import Any

from .tools import (
    json_tool_result,
    read_limited_int,
    read_optional_string_list,
)
from ..types import ToolResult
from ...knowledge import (
    KnowledgeBase,
    KnowledgeBaseError,
    KnowledgeNoteMeta,
    KnowledgeSearchResult,
)


def _optional_text(arguments: dict[str, Any], key: str) -> str | None:
    value = arguments.get(key)
    if value is None:
        return None
    if isinstance(value, str):
        stripped = value.strip()
        return stripped or None
    return None


def _meta_to_dict(meta: KnowledgeNoteMeta) -> dict[str, Any]:
    return {
        "path": meta.rel_path,
        "title": meta.title,
        "created": meta.created,
        "updated": meta.updated,
        "project": meta.project,
        "type": meta.type,
        "status": meta.status,
        "tags": list(meta.tags),
    }


def _search_result_to_dict(result: KnowledgeSearchResult) -> dict[str, Any]:
    return {
        "path": result.rel_path,
        "title": result.title,
        "project": result.project,
        "type": result.type,
        "status": result.status,
        "tags": list(result.tags),
        "snippet": result.snippet,
        "score": result.score,
    }


def kb_search_result(knowledge_base: KnowledgeBase, arguments: dict[str, Any]) -> ToolResult:
    query = str(arguments.get("query") or "").strip()
    if not query:
        return ToolResult(ok=False, output="query 不能为空。")
    try:
        results = knowledge_base.search(
            query,
            project=_optional_text(arguments, "project"),
            tags=read_optional_string_list(arguments, "tags"),
            note_type=_optional_text(arguments, "type"),
            status=_optional_text(arguments, "status"),
            max_results=read_limited_int(arguments, "max_results", default=10, maximum=50),
        )
    except KnowledgeBaseError as exc:
        return ToolResult(ok=False, output=str(exc))
    return json_tool_result([_search_result_to_dict(result) for result in results])


def kb_read_result(knowledge_base: KnowledgeBase, arguments: dict[str, Any]) -> ToolResult:
    path = str(arguments.get("path") or "").strip()
    if not path:
        return ToolResult(ok=False, output="path 不能为空。")
    try:
        text = knowledge_base.read(path)
    except KnowledgeBaseError as exc:
        return ToolResult(ok=False, output=str(exc))
    max_chars = read_limited_int(arguments, "max_chars", default=50000, maximum=200000)
    if len(text) > max_chars:
        text = text[:max_chars] + "\n…（已截断，可提高 max_chars 读取更多内容）"
    return ToolResult(ok=True, output=text)


def kb_write_result(knowledge_base: KnowledgeBase, arguments: dict[str, Any]) -> ToolResult:
    path = str(arguments.get("path") or "").strip()
    content = arguments.get("content")
    if not path:
        return ToolResult(ok=False, output="path 不能为空。")
    if not isinstance(content, str):
        return ToolResult(ok=False, output="content 必须是字符串。")
    mode = str(arguments.get("mode") or "overwrite").strip().lower()
    if mode not in {"create", "overwrite", "append"}:
        return ToolResult(ok=False, output="mode 必须是 create、overwrite 或 append。")
    try:
        meta = knowledge_base.write(
            path,
            content,
            title=_optional_text(arguments, "title"),
            project=_optional_text(arguments, "project"),
            tags=read_optional_string_list(arguments, "tags"),
            note_type=_optional_text(arguments, "type"),
            status=_optional_text(arguments, "status"),
            mode=mode,
        )
    except KnowledgeBaseError as exc:
        return ToolResult(ok=False, output=str(exc))
    return json_tool_result({"note": _meta_to_dict(meta), "mode": mode})


def kb_append_result(knowledge_base: KnowledgeBase, arguments: dict[str, Any]) -> ToolResult:
    path = str(arguments.get("path") or "").strip()
    content = arguments.get("content")
    if not path:
        return ToolResult(ok=False, output="path 不能为空。")
    if not isinstance(content, str):
        return ToolResult(ok=False, output="content 必须是字符串。")
    try:
        meta = knowledge_base.append(path, content)
    except KnowledgeBaseError as exc:
        return ToolResult(ok=False, output=str(exc))
    return json_tool_result({"note": _meta_to_dict(meta), "mode": "append"})


def kb_list_result(knowledge_base: KnowledgeBase, arguments: dict[str, Any]) -> ToolResult:
    try:
        notes = knowledge_base.list_entries(
            rel_path=_optional_text(arguments, "path"),
            project=_optional_text(arguments, "project"),
            tags=read_optional_string_list(arguments, "tags"),
            note_type=_optional_text(arguments, "type"),
            status=_optional_text(arguments, "status"),
            max_results=read_limited_int(arguments, "max_results", default=100, maximum=200),
        )
    except KnowledgeBaseError as exc:
        return ToolResult(ok=False, output=str(exc))
    return json_tool_result([_meta_to_dict(meta) for meta in notes])
