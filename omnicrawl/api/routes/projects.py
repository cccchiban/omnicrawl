"""项目与工作区切换路由。"""

from __future__ import annotations

from typing import Any

from fastapi import APIRouter, Request

from ..deps import data, jsonable, service
from ..models import ProjectPathRequest, ProjectPinRequest, ProjectRenameRequest, ProjectRequest


router = APIRouter(tags=["projects"])


@router.get("/projects")
def list_projects(request: Request) -> dict[str, Any]:
    return data([jsonable(entry) for entry in service(request).agent.list_projects()])


@router.post("/projects")
def create_project(payload: ProjectRequest, request: Request) -> dict[str, Any]:
    current = service(request)
    current.ensure_mutation_allowed()
    return data(jsonable(current.agent.create_project(payload.name, payload.path)))


@router.post("/projects/import")
def import_project(payload: ProjectRequest, request: Request) -> dict[str, Any]:
    current = service(request)
    current.ensure_mutation_allowed()
    return data(jsonable(current.agent.import_project(payload.name, payload.path)))


@router.patch("/projects")
def rename_project(payload: ProjectRenameRequest, request: Request) -> dict[str, Any]:
    current = service(request)
    current.ensure_mutation_allowed()
    return data(jsonable(current.agent.rename_project(payload.path, payload.name)))


@router.post("/projects/pin")
def pin_project(payload: ProjectPinRequest, request: Request) -> dict[str, Any]:
    current = service(request)
    current.ensure_mutation_allowed()
    return data(jsonable(current.agent.pin_project(payload.path, pinned=payload.pinned)))


@router.delete("/projects")
def remove_project(path: str, request: Request) -> dict[str, Any]:
    current = service(request)
    current.ensure_mutation_allowed()
    current.agent.remove_project(path)
    return data({"removed": True, "path": path})


@router.post("/projects/switch")
def switch_project(payload: ProjectPathRequest, request: Request) -> dict[str, Any]:
    current = service(request)
    current.ensure_mutation_allowed()
    new_root = current.agent.switch_workspace(payload.path)
    return data({"workspace_root": str(new_root), "session_id": current.agent.current_session_id})
