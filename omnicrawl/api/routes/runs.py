"""生成任务与人工确认路由。"""

from __future__ import annotations

import asyncio
from typing import Any, Iterable, Optional

from fastapi import APIRouter, Header, Query, Request, status
from fastapi.responses import StreamingResponse

from ..deps import data, service
from ..models import (
    APIServiceError,
    ConfirmationDecision,
    RunRequest,
    TERMINAL_RUN_STATUSES,
    UserQuestionAnswer,
)


router = APIRouter(tags=["runs"])


@router.post("/runs", status_code=status.HTTP_202_ACCEPTED)
def create_run(payload: RunRequest, request: Request) -> dict[str, Any]:
    return data(service(request).start_run(payload.message).summary())


@router.get("/runs/{run_id}")
def get_run(run_id: str, request: Request) -> dict[str, Any]:
    return data(service(request).get_run(run_id).summary())


@router.get("/runs/{run_id}/events")
async def stream_run_events(
    run_id: str,
    request: Request,
    follow: bool = Query(default=True),
    last_event_id_header: Optional[str] = Header(default=None, alias="Last-Event-ID"),
) -> StreamingResponse:
    current = service(request)
    run = current.get_run(run_id)
    try:
        last_event_id = max(0, int(last_event_id_header or "0"))
    except ValueError as exc:
        raise APIServiceError("INVALID_EVENT_ID", "Last-Event-ID 必须是整数。") from exc

    async def generate() -> Iterable[str]:
        cursor = last_event_id
        while True:
            events = current.events_after(run, cursor)
            for event in events:
                cursor = event.id
                yield event.to_sse()
            if not follow or run.status in TERMINAL_RUN_STATUSES:
                break
            if await request.is_disconnected():
                break
            await asyncio.to_thread(current.wait_for_events, run, cursor, 15.0)
            if not current.events_after(run, cursor) and run.status not in TERMINAL_RUN_STATUSES:
                yield ": keep-alive\n\n"

    return StreamingResponse(
        generate(),
        media_type="text/event-stream",
        headers={"Cache-Control": "no-cache", "X-Accel-Buffering": "no"},
    )


@router.post("/runs/{run_id}/cancel")
def cancel_run(run_id: str, request: Request) -> dict[str, Any]:
    return data(service(request).cancel_run(run_id).summary())


@router.post("/runs/{run_id}/confirmations/{confirmation_id}")
def confirm_run(
    run_id: str,
    confirmation_id: str,
    payload: ConfirmationDecision,
    request: Request,
) -> dict[str, Any]:
    confirmation = service(request).decide_confirmation(
        run_id,
        confirmation_id,
        payload.approved,
    )
    return data(
        {"confirmation_id": confirmation.confirmation_id, "approved": payload.approved}
    )


@router.post("/runs/{run_id}/questions/{question_id}")
def answer_user_question(
    run_id: str,
    question_id: str,
    payload: UserQuestionAnswer,
    request: Request,
) -> dict[str, Any]:
    question = service(request).decide_user_question(
        run_id,
        question_id,
        payload.answer,
    )
    return data(
        {
            "question_id": question.question_id,
            "answer": question.answer,
        }
    )
