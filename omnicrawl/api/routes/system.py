"""系统运行时路由。"""

from __future__ import annotations

from typing import Any

from fastapi import APIRouter, Request

from ..deps import data, service


router = APIRouter(tags=["system"])


@router.get("/runtime")
def runtime(request: Request) -> dict[str, Any]:
    current = service(request)
    active_run = current.active_run
    return data(
        {
            "workspace_root": str(current.agent.workspace_root),
            "session_id": str(current.agent.current_session_id),
            "model": current.agent.current_model,
            "reasoning_effort": current.agent.reasoning_effort,
            "approval_mode": current.agent.approval_mode,
            "active_run_id": active_run.run_id if active_run is not None else None,
        }
    )
