"""后台 Monitor 任务路由。"""

from __future__ import annotations

import asyncio
from typing import Any, Iterable, Optional

from fastapi import APIRouter, Header, Query, Request
from fastapi.responses import StreamingResponse

from ...agent import AgentError
from ..deps import data, jsonable, service
from ..models import APIServiceError, RunEvent


router = APIRouter(tags=["monitors"])


@router.get("/monitors")
def list_monitors(request: Request) -> dict[str, Any]:
    agent = service(request).agent
    try:
        tasks = agent.list_monitor_tasks()
    except AgentError as exc:
        raise APIServiceError("MONITOR_UNAVAILABLE", "后台监控不可用。", status_code=503) from exc
    return data([jsonable(task) for task in tasks])


@router.get("/monitors/{monitor_id}")
def get_monitor(monitor_id: str, request: Request) -> dict[str, Any]:
    try:
        task = service(request).agent.get_monitor_task(monitor_id)
    except AgentError as exc:
        raise APIServiceError("MONITOR_NOT_FOUND", "后台任务不存在。", status_code=404) from exc
    return data(jsonable(task))


@router.get("/monitors/{monitor_id}/events")
async def stream_monitor_events(
    monitor_id: str,
    request: Request,
    follow: bool = Query(default=True),
    max_events: int = Query(default=100, ge=1, le=200),
    cursor: int = Query(default=0, ge=0),
    last_event_id_header: Optional[str] = Header(default=None, alias="Last-Event-ID"),
) -> StreamingResponse:
    agent = service(request).agent
    try:
        event_cursor = max(0, int(last_event_id_header or cursor))
    except ValueError as exc:
        raise APIServiceError("INVALID_EVENT_ID", "Last-Event-ID 必须是整数。") from exc

    try:
        agent.get_monitor_task(monitor_id)
    except AgentError as exc:
        raise APIServiceError("MONITOR_NOT_FOUND", "后台任务不存在。", status_code=404) from exc

    async def generate() -> Iterable[str]:
        nonlocal event_cursor
        while True:
            try:
                result = agent.poll_monitor_events(
                    monitor_id,
                    cursor=event_cursor,
                    max_events=max_events,
                )
            except AgentError:
                return

            for event in result.events:
                event_cursor = event.sequence
                event_name = "monitor.output" if event.stream in {"stdout", "stderr"} else "monitor.status"
                payload = {
                    "monitor_id": monitor_id,
                    "sequence": event.sequence,
                    "created_at": event.created_at,
                    "stream": event.stream,
                    "text": event.text,
                    "status": result.snapshot.status,
                    "exit_code": result.snapshot.exit_code,
                }
                yield RunEvent(id=event.sequence, event=event_name, data=payload).to_sse()

            if not follow or result.snapshot.status != "running":
                break
            if await request.is_disconnected():
                break
            await asyncio.to_thread(agent.wait_for_monitor_events, monitor_id, event_cursor, 15.0)
            if not result.events:
                yield ": keep-alive\n\n"

    return StreamingResponse(
        generate(),
        media_type="text/event-stream",
        headers={"Cache-Control": "no-cache", "X-Accel-Buffering": "no"},
    )
