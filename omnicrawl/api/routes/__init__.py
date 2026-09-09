"""按资源分组的 API 路由集合。"""

from __future__ import annotations

from fastapi import APIRouter, Depends

from ..deps import authorize
from ..models import API_PREFIX
from . import (
    configuration,
    monitors,
    projects,
    runs,
    sessions,
    settings,
    subagents,
    support,
    system,
)


def build_api_router() -> APIRouter:
    """组装受 Bearer 鉴权保护的 `/api/v1` 路由。"""

    router = APIRouter(prefix=API_PREFIX, dependencies=[Depends(authorize)])
    router.include_router(system.router)
    router.include_router(runs.router)
    router.include_router(monitors.router)
    router.include_router(subagents.router)
    router.include_router(sessions.router)
    router.include_router(projects.router)
    router.include_router(configuration.router)
    router.include_router(settings.router)
    router.include_router(support.router)
    return router


__all__ = ["build_api_router"]
