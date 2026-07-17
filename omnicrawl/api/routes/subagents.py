"""当前会话后台 SubAgent 任务、跨回合审批与 SSE 控制路由。"""

from __future__ import annotations

import asyncio
from typing import Any, Iterable, Optional

from fastapi import APIRouter, Header, Query, Request, status
from fastapi.responses import StreamingResponse

from ...agent import AgentError
from ..deps import data, jsonable, service
from ..models import APIServiceError, ConfirmationDecision


router = APIRouter(tags=["subagents"])


def _subagent_unavailable(_exc: AgentError) -> APIServiceError:
    """把 Host 未启用的增量能力映射为稳定的本地 API 错误。"""

    return APIServiceError(
        "SUBAGENT_UNAVAILABLE",
        "SubAgent 后台任务不可用。",
        status_code=status.HTTP_503_SERVICE_UNAVAILABLE,
    )


@router.get("/subagents/events")
async def stream_subagent_events(
    request: Request,
    follow: bool = Query(default=True),
    last_event_id_header: Optional[str] = Header(default=None, alias="Last-Event-ID"),
) -> StreamingResponse:
    """流式返回当前 Session 的后台任务/审批事件，不绑定某个已结束父 Run。"""

    current = service(request)
    try:
        last_event_id = max(0, int(last_event_id_header or "0"))
    except ValueError as exc:
        raise APIServiceError("INVALID_EVENT_ID", "Last-Event-ID 必须是整数。") from exc
    session_id = current.current_subagent_session_id()

    async def generate() -> Iterable[str]:
        cursor = last_event_id
        while True:
            events = current.subagent_events_after(session_id, cursor)
            for event in events:
                cursor = event.id
                yield event.to_sse()
            if not follow:
                break
            if await request.is_disconnected():
                break
            await asyncio.to_thread(
                current.wait_for_subagent_events,
                session_id,
                cursor,
                15.0,
            )
            if not current.subagent_events_after(session_id, cursor):
                yield ": keep-alive\n\n"

    return StreamingResponse(
        generate(),
        media_type="text/event-stream",
        headers={"Cache-Control": "no-cache", "X-Accel-Buffering": "no"},
    )


@router.get("/subagents/confirmations")
def list_subagent_confirmations(request: Request) -> dict[str, Any]:
    """列出当前 Session 仍等待远程决议的跨父 Run 后台审批。"""

    return data(service(request).list_background_subagent_confirmations())


@router.post("/subagents/confirmations/{confirmation_id}")
def decide_subagent_confirmation(
    confirmation_id: str,
    payload: ConfirmationDecision,
    request: Request,
) -> dict[str, Any]:
    """提交一次后台审批决定；已取消、超时或已决议的请求不能再次批准。"""

    return data(
        service(request).decide_background_subagent_confirmation(
            confirmation_id,
            payload.approved,
        )
    )


@router.get("/subagents")
def list_subagents(request: Request) -> dict[str, Any]:
    """列出当前 Agent 当前会话可见的后台任务，不接受跨会话筛选条件。"""

    try:
        tasks = service(request).agent.list_subagent_tasks()
    except AgentError as exc:
        raise _subagent_unavailable(exc) from exc
    return data([jsonable(task) for task in tasks])


@router.get("/subagents/{task_id}")
def get_subagent(task_id: str, request: Request) -> dict[str, Any]:
    """读取当前会话中的一个任务；其他会话的任务统一不可见。"""

    try:
        task = service(request).agent.get_subagent_task(task_id)
    except AgentError as exc:
        raise _subagent_unavailable(exc) from exc
    if task is None:
        raise APIServiceError(
            "SUBAGENT_NOT_FOUND",
            "未找到当前会话的 SubAgent 任务。",
            status_code=status.HTTP_404_NOT_FOUND,
        )
    return data(jsonable(task))


@router.post("/subagents/{task_id}/cancel")
def cancel_subagent(task_id: str, request: Request) -> dict[str, Any]:
    """请求取消当前会话的单个后台任务；不会创建或重启任务。"""

    try:
        result = service(request).agent.cancel_subagent_task(task_id)
    except AgentError as exc:
        raise _subagent_unavailable(exc) from exc
    if not bool(result.get("ok")):
        raise APIServiceError(
            "SUBAGENT_NOT_FOUND",
            "未找到当前会话的 SubAgent 任务。",
            status_code=status.HTTP_404_NOT_FOUND,
        )
    return data(jsonable(result))


__all__ = ["router"]
