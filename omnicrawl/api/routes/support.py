"""历史、Skill、MCP 与 Memory 支持路由。"""

from __future__ import annotations

from typing import Any

from fastapi import APIRouter, Query, Request

from ..deps import data, jsonable, service


router = APIRouter(tags=["support"])


@router.get("/history")
def prompt_history(
    request: Request,
    query: str = "",
    limit: int = Query(default=20, ge=1, le=100),
    current_session_only: bool = False,
) -> dict[str, Any]:
    entries = service(request).agent.search_prompt_history(
        query=query,
        limit=limit,
        current_session_only=current_session_only,
    )
    return data([jsonable(entry) for entry in entries])


@router.get("/skills")
def skills(request: Request) -> dict[str, Any]:
    manager = service(request).agent.skill_manager
    if manager is None:
        return data([])
    return data([jsonable(meta) for meta in manager.list_all()])


@router.get("/mcp")
def mcp_status(request: Request) -> dict[str, Any]:
    return data({"status": service(request).agent.format_mcp_status()})


@router.post("/memory/clean")
def clean_memory(request: Request) -> dict[str, Any]:
    current = service(request)
    current.ensure_mutation_allowed()
    deleted = current.agent.clean_memory()
    return data({"deleted": deleted, "count": len(deleted)})
