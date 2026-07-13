"""会话生命周期与 artifact 路由。"""

from __future__ import annotations

import logging
from pathlib import Path
from typing import Any

from fastapi import APIRouter, Query, Request, status
from fastapi.responses import PlainTextResponse

from ..deps import data, jsonable, service
from ..models import APIServiceError, ExportRequest, RenameRequest
from ...state.session_artifacts import redact_sensitive_text


LOGGER = logging.getLogger(__name__)
router = APIRouter(tags=["sessions"])


@router.get("/sessions")
def list_sessions(
    request: Request,
    limit: int = Query(default=20, ge=1, le=100),
    archived: bool = Query(default=False),
) -> dict[str, Any]:
    agent = service(request).agent
    entries = agent.list_archived_sessions(limit) if archived else agent.list_sessions(limit=limit)
    return data([jsonable(entry) for entry in entries])


@router.post("/sessions")
def new_session(request: Request) -> dict[str, Any]:
    current = service(request)
    current.ensure_mutation_allowed()
    current.agent.reset_conversation()
    return data({"session_id": current.agent.current_session_id})


@router.get("/sessions/diagnostics")
def session_diagnostics_overview(request: Request) -> dict[str, Any]:
    """返回提示历史诊断总览。

    不改写任何磁盘文件；用于排查静默跳过的坏行。
    必须声明在 `/{session_id}/...` 路由之前，避免被路径参数吞掉。
    """

    return data(service(request).agent.load_session_diagnostics())


@router.get("/sessions/{session_id}/diagnostics")
def session_diagnostics(session_id: str, request: Request) -> dict[str, Any]:
    """返回指定会话转录与提示历史的损坏/版本诊断。"""

    return data(service(request).agent.load_session_diagnostics(session_id))


@router.get("/sessions/{session_id}/events")
def session_events(session_id: str, request: Request) -> dict[str, Any]:
    return data(
        [jsonable(event) for event in service(request).agent.load_session_events(session_id)]
    )


@router.post("/sessions/{session_id}/resume")
def resume_session(session_id: str, request: Request) -> dict[str, Any]:
    current = service(request)
    current.ensure_mutation_allowed()
    return data(jsonable(current.agent.resume_session(session_id)))


@router.patch("/sessions/current")
def rename_session(payload: RenameRequest, request: Request) -> dict[str, Any]:
    current = service(request)
    current.ensure_mutation_allowed()
    return data(jsonable(current.agent.rename_current_session(payload.title)))


@router.post("/sessions/current/compact")
def compact_session(request: Request) -> dict[str, Any]:
    current = service(request)
    current.ensure_mutation_allowed()
    return data({"summary": current.agent.compact_conversation()})


@router.post("/sessions/current/archive")
def archive_session(request: Request) -> dict[str, Any]:
    current = service(request)
    current.ensure_mutation_allowed()
    return data(jsonable(current.agent.archive_current_session()))


@router.delete("/sessions/{session_id}")
def delete_session(session_id: str, request: Request) -> dict[str, Any]:
    current = service(request)
    current.ensure_mutation_allowed()
    current.agent.delete_session(session_id)
    return data({"deleted": True, "session_id": session_id})


@router.post("/sessions/current/export")
def export_session(payload: ExportRequest, request: Request) -> dict[str, Any]:
    current = service(request)
    current.ensure_mutation_allowed()
    path = current.agent.export_current_session_markdown(payload.markdown)
    return data({"path": str(path)})


@router.get("/sessions/{session_id}/artifacts/{artifact_path:path}")
def session_artifact(session_id: str, artifact_path: str, request: Request) -> PlainTextResponse:
    if not artifact_path or ".." in Path(artifact_path).parts:
        raise APIServiceError("INVALID_ARTIFACT_PATH", "artifact 路径不合法。")
    try:
        text = service(request).agent.read_session_artifact_text(session_id, artifact_path)
    except Exception as exc:
        # artifact 存储可能来自文件系统或会话索引；底层异常不应成为
        # 对外 API 文案，以免泄露路径、会话细节或敏感配置。
        LOGGER.warning(
            "Unable to read session artifact %s/%s: %s: %s",
            session_id,
            artifact_path,
            type(exc).__name__,
            redact_sensitive_text(str(exc)),
        )
        raise APIServiceError(
            "ARTIFACT_NOT_FOUND",
            "会话 artifact 不存在。",
            status_code=status.HTTP_404_NOT_FOUND,
        ) from exc
    return PlainTextResponse(text, media_type="text/html; charset=utf-8")
