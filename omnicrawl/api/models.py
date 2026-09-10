"""OmniCrawl API 请求/响应模型与运行时状态结构。

本模块只承载数据形状、配置校验和 SSE 事件序列化，不负责业务编排。
"""

from __future__ import annotations

import json
import threading
import time
from dataclasses import dataclass, field
from typing import Any, Optional

from fastapi import status
from pydantic import BaseModel, Field


API_PREFIX = "/api/v1"
TERMINAL_RUN_STATUSES = {"completed", "cancelled", "failed"}
ACTIVE_RUN_STATUSES = {"pending", "running", "waiting_confirmation", "waiting_user"}


class APIServiceError(RuntimeError):
    """可稳定映射为 HTTP 错误结构的服务异常。"""

    def __init__(
        self,
        code: str,
        message: str,
        *,
        status_code: int = status.HTTP_400_BAD_REQUEST,
        details: Any = None,
    ) -> None:
        super().__init__(message)
        self.code = code
        self.message = message
        self.status_code = status_code
        self.details = details


class RunCancelled(RuntimeError):
    """HTTP 客户端主动取消生成时中断 Agent 的内部异常。"""


@dataclass(frozen=True)
class APIConfig:
    """本地 API 服务配置。"""

    bearer_token: str
    host: str = "127.0.0.1"
    port: int = 8765
    allowed_origins: tuple[str, ...] = ()
    confirmation_timeout_seconds: float = 300.0

    def __post_init__(self) -> None:
        token = self.bearer_token.strip()
        if not token:
            raise ValueError("api.bearer_token 或 OMNICRAWL_API_TOKEN 不能为空。")
        if self.host.strip().lower() not in {"127.0.0.1", "localhost", "::1"}:
            raise ValueError("API 服务首版仅允许绑定回环地址。")
        if isinstance(self.port, bool) or not 1 <= int(self.port) <= 65535:
            raise ValueError("api.port 必须是 1 到 65535 的整数。")
        if self.confirmation_timeout_seconds <= 0:
            raise ValueError("api.confirmation_timeout_seconds 必须大于 0。")
        origins = tuple(origin.strip() for origin in self.allowed_origins if origin.strip())
        if "*" in origins:
            raise ValueError("api.allowed_origins 不允许使用通配符 *。")
        object.__setattr__(self, "bearer_token", token)
        object.__setattr__(self, "host", self.host.strip())
        object.__setattr__(self, "port", int(self.port))
        object.__setattr__(self, "allowed_origins", origins)


class RunRequest(BaseModel):
    message: str = Field(min_length=1, max_length=200_000)


class ConfirmationDecision(BaseModel):
    approved: bool


class UserQuestionAnswer(BaseModel):
    answer: str = Field(min_length=1)


class RenameRequest(BaseModel):
    title: str = Field(min_length=1, max_length=200)


class ExportRequest(BaseModel):
    markdown: str = Field(min_length=1, max_length=2_000_000)


class ProjectRequest(BaseModel):
    name: str = Field(min_length=1, max_length=200)
    path: str = Field(default="", max_length=32_768)


class ProjectRenameRequest(BaseModel):
    path: str = Field(min_length=1, max_length=32_768)
    name: str = Field(min_length=1, max_length=200)


class ProjectPinRequest(BaseModel):
    path: str = Field(min_length=1, max_length=32_768)
    pinned: bool = True


class ProjectPathRequest(BaseModel):
    path: str = Field(min_length=1, max_length=32_768)


class ModelChangeRequest(BaseModel):
    """切换当前模型。

    兼容字段：
    - model: 旧扁平模型 ID / 自定义 key / profile/model_id
    规范字段：
    - source=custom + key
    - source=detected + profile + model_id + protocol
    """

    model: Optional[str] = Field(default=None, max_length=300)
    source: Optional[str] = Field(default=None, max_length=30)
    key: Optional[str] = Field(default=None, max_length=300)
    profile: Optional[str] = Field(default=None, max_length=300)
    model_id: Optional[str] = Field(default=None, max_length=300)
    protocol: Optional[str] = Field(default=None, max_length=100)


class ReasoningChangeRequest(BaseModel):
    effort: str = Field(min_length=1, max_length=30)


class ApprovalChangeRequest(BaseModel):
    mode: str = Field(min_length=1, max_length=30)


@dataclass(frozen=True)
class RunEvent:
    id: int
    event: str
    data: dict[str, Any]

    def to_sse(self) -> str:
        payload = json.dumps(self.data, ensure_ascii=False, separators=(",", ":"))
        return f"id: {self.id}\nevent: {self.event}\ndata: {payload}\n\n"


@dataclass
class PendingConfirmation:
    confirmation_id: str
    tool_name: str
    arguments: dict[str, Any]
    created_at: float = field(default_factory=time.time)
    decision: bool | None = None
    resolved: threading.Event = field(default_factory=threading.Event)


@dataclass
class PendingUserQuestion:
    question_id: str
    kind: str
    question: str
    options: tuple[str, ...] = ()
    created_at: float = field(default_factory=time.time)
    answer: str | None = None
    resolved: threading.Event = field(default_factory=threading.Event)


@dataclass
class RunState:
    run_id: str
    message: str
    session_id: str
    status: str = "pending"
    created_at: float = field(default_factory=time.time)
    updated_at: float = field(default_factory=time.time)
    events: list[RunEvent] = field(default_factory=list)
    next_event_id: int = 1
    result: str = ""
    error: str = ""
    cancel_requested: threading.Event = field(default_factory=threading.Event)
    confirmations: dict[str, PendingConfirmation] = field(default_factory=dict)
    user_questions: dict[str, PendingUserQuestion] = field(default_factory=dict)
    condition: threading.Condition = field(default_factory=threading.Condition)
    # update_todos 最近一次清单（供 get_run 恢复计划区；事件流仍保留全部 todo.updated）
    last_todo_items: tuple[dict[str, Any], ...] = ()

    def summary(self) -> dict[str, Any]:
        return {
            "run_id": self.run_id,
            "session_id": self.session_id,
            "status": self.status,
            "created_at": self.created_at,
            "updated_at": self.updated_at,
            "result": self.result,
            "error": self.error,
            "todo_items": list(self.last_todo_items),
        }


__all__ = [
    "ACTIVE_RUN_STATUSES",
    "APIConfig",
    "APIServiceError",
    "API_PREFIX",
    "ApprovalChangeRequest",
    "ConfirmationDecision",
    "ExportRequest",
    "UserQuestionAnswer",
    "ModelChangeRequest",
    "PendingConfirmation",
    "PendingUserQuestion",
    "ProjectPathRequest",
    "ProjectPinRequest",
    "ProjectRenameRequest",
    "ProjectRequest",
    "ReasoningChangeRequest",
    "RenameRequest",
    "RunCancelled",
    "RunEvent",
    "RunRequest",
    "RunState",
    "TERMINAL_RUN_STATUSES",
]
