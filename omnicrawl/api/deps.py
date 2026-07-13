"""API 依赖注入与统一响应/序列化辅助。"""

from __future__ import annotations

import secrets
from dataclasses import asdict, is_dataclass
from datetime import datetime
from pathlib import Path
from typing import Any

from fastapi import Request, status
from fastapi.responses import JSONResponse

from .models import APIServiceError
from .service import AgentAPIService


async def authorize(request: Request) -> None:
    expected = request.app.state.api_config.bearer_token
    authorization = request.headers.get("Authorization", "")
    scheme, separator, token = authorization.partition(" ")
    if not separator or scheme.casefold() != "bearer" or not secrets.compare_digest(token, expected):
        raise APIServiceError(
            "UNAUTHORIZED",
            "缺少或无效的 Bearer Token。",
            status_code=status.HTTP_401_UNAUTHORIZED,
        )


def service(request: Request) -> AgentAPIService:
    current = request.app.state.service
    if current is None:
        raise APIServiceError(
            "SERVICE_UNAVAILABLE",
            "Agent 服务尚未就绪。",
            status_code=status.HTTP_503_SERVICE_UNAVAILABLE,
        )
    return current


def data(value: Any) -> dict[str, Any]:
    return {"data": value}


def error_response(status_code: int, code: str, message: str, details: Any = None) -> JSONResponse:
    return JSONResponse(
        status_code=status_code,
        content={"error": {"code": code, "message": message, "details": details}},
    )


def jsonable(value: Any) -> Any:
    if is_dataclass(value):
        return jsonable(asdict(value))
    if isinstance(value, datetime):
        return value.isoformat()
    if isinstance(value, Path):
        return str(value)
    if isinstance(value, dict):
        return {str(key): jsonable(item) for key, item in value.items()}
    if isinstance(value, (list, tuple)):
        return [jsonable(item) for item in value]
    if hasattr(value, "__dict__") and not isinstance(value, type):
        return {
            key: jsonable(item)
            for key, item in vars(value).items()
            if not key.startswith("_")
        }
    return value


__all__ = [
    "authorize",
    "data",
    "error_response",
    "jsonable",
    "service",
]
