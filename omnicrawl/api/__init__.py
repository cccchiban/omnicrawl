"""OmniCrawl 本地 HTTP/SSE 接口。

接口层只编排已有 LocalToolAgent 能力，不复制 Agent 业务逻辑。服务默认仅绑定
回环地址，并用 Bearer Token 保护所有 `/api/v1` 路由。
"""

from __future__ import annotations

import asyncio
import json
import logging
import os
import secrets
import threading
import time
from contextlib import asynccontextmanager
from dataclasses import asdict, dataclass, field, is_dataclass
from datetime import datetime
from pathlib import Path
from typing import Any, Callable, Iterable, Optional

from fastapi import APIRouter, Depends, FastAPI, Header, Query, Request, status
from fastapi.exceptions import RequestValidationError
from fastapi.middleware.cors import CORSMiddleware
from fastapi.responses import JSONResponse, PlainTextResponse, StreamingResponse
from pydantic import BaseModel, Field

from ..agent import AgentConfig, AgentError, LocalToolAgent, ToolCall, ToolResult
from ..config.approval import normalize_approval_mode, save_approval_mode
from ..config.llm import LLMError, load_llm_config, save_reasoning_effort
from ..config.model_catalog import (
    ModelCatalogError,
    detect_model_options,
    ensure_current_model_option,
    save_llm_model,
)
from ..config.runtime import RuntimeConfigError, get_section, load_config_data
from ..workspace.context import ProjectContextError, detect_project_context
from ..workspace.temp import AgentTempWorkspaceError, load_agent_temp_workspace_config


API_PREFIX = "/api/v1"
TERMINAL_RUN_STATUSES = {"completed", "cancelled", "failed"}
ACTIVE_RUN_STATUSES = {"pending", "running", "waiting_confirmation"}
LOGGER = logging.getLogger(__name__)


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
    model: str = Field(min_length=1, max_length=300)


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
    condition: threading.Condition = field(default_factory=threading.Condition)

    def summary(self) -> dict[str, Any]:
        return {
            "run_id": self.run_id,
            "session_id": self.session_id,
            "status": self.status,
            "created_at": self.created_at,
            "updated_at": self.updated_at,
            "result": self.result,
            "error": self.error,
        }


class AgentAPIService:
    """单 Agent、单活动生成的线程安全服务编排器。"""

    def __init__(self, agent: Any, *, confirmation_timeout_seconds: float = 300.0) -> None:
        self.agent = agent
        self.confirmation_timeout_seconds = confirmation_timeout_seconds
        self._lock = threading.RLock()
        self._runs: dict[str, RunState] = {}
        self._active_run_id = ""
        self._closed = False
        self.agent.set_confirm_handler(self._confirm_tool_call)

    @property
    def active_run(self) -> RunState | None:
        with self._lock:
            if not self._active_run_id:
                return None
            return self._runs.get(self._active_run_id)

    def ensure_mutation_allowed(self) -> None:
        run = self.active_run
        if run is not None and run.status in ACTIVE_RUN_STATUSES:
            raise APIServiceError(
                "RUN_ACTIVE",
                "当前已有生成任务运行，暂不能修改会话、项目或运行配置。",
                status_code=status.HTTP_409_CONFLICT,
                details={"run_id": run.run_id},
            )

    def start_run(self, message: str) -> RunState:
        text = message.strip()
        if not text:
            raise APIServiceError("INVALID_MESSAGE", "message 不能为空。")
        with self._lock:
            self.ensure_mutation_allowed()
            run_id = secrets.token_hex(12)
            run = RunState(
                run_id=run_id,
                message=text,
                session_id=str(getattr(self.agent, "current_session_id", "")),
            )
            self._runs[run_id] = run
            self._active_run_id = run_id
            thread = threading.Thread(target=self._execute_run, args=(run,), daemon=True)
            thread.start()
            return run

    def get_run(self, run_id: str) -> RunState:
        with self._lock:
            run = self._runs.get(run_id)
        if run is None:
            raise APIServiceError(
                "RUN_NOT_FOUND",
                f"生成任务不存在：{run_id}",
                status_code=status.HTTP_404_NOT_FOUND,
            )
        return run

    def cancel_run(self, run_id: str) -> RunState:
        run = self.get_run(run_id)
        run.cancel_requested.set()
        with run.condition:
            for confirmation in run.confirmations.values():
                if not confirmation.resolved.is_set():
                    confirmation.decision = False
                    confirmation.resolved.set()
            run.condition.notify_all()
        return run

    def decide_confirmation(
        self,
        run_id: str,
        confirmation_id: str,
        approved: bool,
    ) -> PendingConfirmation:
        run = self.get_run(run_id)
        # 与取消/超时共用同一条件锁，保证一个确认请求只能由首个终态操作处理。
        with run.condition:
            confirmation = run.confirmations.get(confirmation_id)
            if confirmation is None:
                raise APIServiceError(
                    "CONFIRMATION_NOT_FOUND",
                    f"确认请求不存在：{confirmation_id}",
                    status_code=status.HTTP_404_NOT_FOUND,
                )
            if confirmation.resolved.is_set():
                raise APIServiceError(
                    "CONFIRMATION_RESOLVED",
                    "该确认请求已经处理。",
                    status_code=status.HTTP_409_CONFLICT,
                )
            confirmation.decision = approved
            confirmation.resolved.set()
            run.condition.notify_all()
            return confirmation

    def events_after(self, run: RunState, last_event_id: int) -> list[RunEvent]:
        with run.condition:
            return [event for event in run.events if event.id > last_event_id]

    def wait_for_events(self, run: RunState, last_event_id: int, timeout: float) -> None:
        with run.condition:
            if not any(event.id > last_event_id for event in run.events):
                run.condition.wait(timeout=timeout)

    def close(self) -> None:
        with self._lock:
            if self._closed:
                return
            self._closed = True
            active_run = self.active_run
        if active_run is not None:
            self.cancel_run(active_run.run_id)
        self.agent.close()

    def _execute_run(self, run: RunState) -> None:
        run.status = "running"
        run.updated_at = time.time()
        self._emit(run, "run.started", {"run_id": run.run_id, "session_id": run.session_id})

        def check_cancelled() -> None:
            if run.cancel_requested.is_set():
                raise RunCancelled("用户取消生成。")

        try:
            result = self.agent.run_stream(
                run.message,
                lambda delta: self._on_delta(run, delta, check_cancelled),
                on_status=lambda message: self._on_status(run, message, check_cancelled),
                on_tool_start=lambda step, tool_call: self._on_tool_start(
                    run, step, tool_call, check_cancelled
                ),
                on_tool_result=lambda tool_call, tool_result: self._on_tool_result(
                    run, tool_call, tool_result, check_cancelled
                ),
                on_token_usage=lambda input_tokens, output_tokens, cached_input_tokens: self._on_usage(
                    run,
                    input_tokens,
                    output_tokens,
                    cached_input_tokens,
                    check_cancelled,
                ),
                on_protocol_wait=lambda: self._on_status(run, "正在继续", check_cancelled),
                on_retry_status=lambda message: self._on_status(run, message, check_cancelled),
                cancel_check=check_cancelled,
            )
            check_cancelled()
            run.status = "completed"
            run.result = result
            self._emit(run, "run.completed", {"run_id": run.run_id, "result": result})
        except (RunCancelled, KeyboardInterrupt) as exc:
            run.status = "cancelled"
            run.error = str(exc) or "用户取消生成。"
            self._emit(run, "run.cancelled", {"run_id": run.run_id, "message": run.error})
        except Exception:
            # Agent、SDK 或外部工具的异常消息可能包含请求头、密钥或本地路径；
            # 对客户端保持稳定的通用错误，同时仅在服务端日志保留完整诊断。
            LOGGER.exception("OmniCrawl API run %s failed", run.run_id)
            run.status = "failed"
            run.error = "生成任务失败。"
            self._emit(run, "run.failed", {"run_id": run.run_id, "message": run.error})
        finally:
            run.updated_at = time.time()
            with self._lock:
                if self._active_run_id == run.run_id:
                    self._active_run_id = ""
            with run.condition:
                run.condition.notify_all()

    def _confirm_tool_call(self, tool_name: str, arguments: dict[str, Any]) -> bool:
        run = self.active_run
        if run is None:
            return False
        confirmation_id = secrets.token_hex(12)
        confirmation = PendingConfirmation(
            confirmation_id=confirmation_id,
            tool_name=tool_name,
            arguments=dict(arguments),
        )
        run.confirmations[confirmation_id] = confirmation
        run.status = "waiting_confirmation"
        self._emit(
            run,
            "confirmation.required",
            {
                "confirmation_id": confirmation_id,
                "tool": tool_name,
                "arguments": arguments,
                "timeout_seconds": self.confirmation_timeout_seconds,
            },
        )

        deadline = time.monotonic() + self.confirmation_timeout_seconds
        while not confirmation.resolved.is_set():
            if run.cancel_requested.is_set():
                run.status = "running"
                raise RunCancelled("用户取消生成。")
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                # 超时和显式拒绝均为终态；必须置位，避免运行结束后接口又接受
                # 同一 confirmation_id 的迟到批准请求。锁还会与 HTTP 批准确保原子性。
                with run.condition:
                    if not confirmation.resolved.is_set():
                        confirmation.decision = False
                        confirmation.resolved.set()
                        run.condition.notify_all()
                break
            confirmation.resolved.wait(timeout=min(0.1, remaining))
        run.status = "running"
        return confirmation.decision is True

    def _on_delta(
        self,
        run: RunState,
        delta: str,
        check_cancelled: Callable[[], None],
    ) -> None:
        check_cancelled()
        self._emit(run, "assistant.delta", {"delta": delta})

    def _on_status(
        self,
        run: RunState,
        message: str,
        check_cancelled: Callable[[], None],
    ) -> None:
        check_cancelled()
        self._emit(run, "status.changed", {"message": message})

    def _on_tool_start(
        self,
        run: RunState,
        step: int,
        tool_call: ToolCall,
        check_cancelled: Callable[[], None],
    ) -> None:
        check_cancelled()
        self._emit(
            run,
            "tool.started",
            {
                "step": step,
                "tool_call_id": tool_call.id,
                "tool": tool_call.name,
                "arguments": tool_call.arguments,
            },
        )

    def _on_tool_result(
        self,
        run: RunState,
        tool_call: ToolCall,
        tool_result: ToolResult,
        check_cancelled: Callable[[], None],
    ) -> None:
        check_cancelled()
        artifact = tool_result.ui_artifact if isinstance(tool_result.ui_artifact, dict) else {}
        # HTML 正文仅保存于受会话访问控制的 artifact 存储中，不能随着 SSE
        # 事件回传；事件只提供客户端定位展示所需的元数据。
        public_artifact = {key: value for key, value in artifact.items() if key != "html"}
        if str(public_artifact.get("type", "")).casefold() == "html":
            # 不让 SSE 事件携带可被前端误当作内联 HTML 的类型提示；正文仍留在
            # 受会话 artifact 接口保护的存储中。
            public_artifact.pop("type", None)
        self._emit(
            run,
            "tool.completed",
            {
                "tool_call_id": tool_call.id,
                "tool": tool_call.name,
                "ok": tool_result.ok,
                "output": tool_result.output,
                "artifact": public_artifact,
            },
        )
        if public_artifact:
            self._emit(run, "artifact.available", public_artifact)

    def _on_usage(
        self,
        run: RunState,
        input_tokens: int,
        output_tokens: int,
        cached_input_tokens: int,
        check_cancelled: Callable[[], None],
    ) -> None:
        check_cancelled()
        self._emit(
            run,
            "usage.updated",
            {
                "input_tokens": input_tokens,
                "output_tokens": output_tokens,
                "cached_input_tokens": cached_input_tokens,
            },
        )

    @staticmethod
    def _emit(run: RunState, event_name: str, data: dict[str, Any]) -> None:
        with run.condition:
            event = RunEvent(id=run.next_event_id, event=event_name, data=data)
            run.next_event_id += 1
            run.events.append(event)
            run.updated_at = time.time()
            run.condition.notify_all()


def load_api_config() -> APIConfig:
    """按环境变量优先级读取本地 API 配置。"""

    data = load_config_data()
    section = get_section(data, "api")
    bearer_token = os.getenv("OMNICRAWL_API_TOKEN", "").strip()
    if not bearer_token:
        raw_token = section.get("bearer_token", "")
        bearer_token = raw_token.strip() if isinstance(raw_token, str) else ""
    host = os.getenv("OMNICRAWL_API_HOST", "").strip() or section.get("host", "127.0.0.1")
    raw_port: Any = os.getenv("OMNICRAWL_API_PORT", "").strip() or section.get("port", 8765)
    try:
        port = int(raw_port)
    except (TypeError, ValueError) as exc:
        raise ValueError("api.port 必须是整数。") from exc
    raw_origins = section.get("allowed_origins", [])
    if isinstance(raw_origins, str):
        origins = tuple(item.strip() for item in raw_origins.split(",") if item.strip())
    elif isinstance(raw_origins, list) and all(isinstance(item, str) for item in raw_origins):
        origins = tuple(raw_origins)
    else:
        raise ValueError("api.allowed_origins 必须是字符串列表。")
    raw_timeout = section.get("confirmation_timeout_seconds", 300)
    try:
        timeout_seconds = float(raw_timeout)
    except (TypeError, ValueError) as exc:
        raise ValueError("api.confirmation_timeout_seconds 必须是数字。") from exc
    return APIConfig(
        bearer_token=bearer_token,
        host=str(host),
        port=port,
        allowed_origins=origins,
        confirmation_timeout_seconds=timeout_seconds,
    )


def create_default_agent() -> LocalToolAgent:
    """按 TUI 相同的配置与工作区检测规则创建 Agent。"""

    app_root = Path(__file__).resolve().parents[2]
    project_context = detect_project_context(app_root=app_root)
    return LocalToolAgent(
        AgentConfig(
            llm=load_llm_config(),
            workspace_root=project_context.workspace_root,
            workspace_detection_summary=project_context.detection_summary,
            temp_workspace=load_agent_temp_workspace_config(),
        )
    )


def create_app(
    *,
    config: APIConfig | None = None,
    agent_factory: Callable[[], Any] | None = None,
) -> FastAPI:
    """创建可测试、可嵌入的 FastAPI 应用。"""

    api_config = config or load_api_config()
    @asynccontextmanager
    async def lifespan(application: FastAPI):
        application.state.service = AgentAPIService(
            application.state.agent_factory(),
            confirmation_timeout_seconds=api_config.confirmation_timeout_seconds,
        )
        try:
            yield
        finally:
            service = application.state.service
            if service is not None:
                service.close()
                application.state.service = None

    app = FastAPI(
        title="OmniCrawl Local API",
        version="1.0.0",
        description="OmniCrawl 本地 Agent 的 HTTP/SSE 接口。",
        lifespan=lifespan,
    )
    app.state.api_config = api_config
    app.state.agent_factory = agent_factory or create_default_agent
    app.state.service = None

    if api_config.allowed_origins:
        app.add_middleware(
            CORSMiddleware,
            allow_origins=list(api_config.allowed_origins),
            allow_credentials=False,
            allow_methods=["GET", "POST", "PUT", "PATCH", "DELETE", "OPTIONS"],
            allow_headers=["Authorization", "Content-Type", "Last-Event-ID"],
        )

    @app.exception_handler(APIServiceError)
    async def handle_service_error(_request: Request, exc: APIServiceError) -> JSONResponse:
        return _error_response(exc.status_code, exc.code, exc.message, exc.details)

    @app.exception_handler(RequestValidationError)
    async def handle_validation_error(_request: Request, exc: RequestValidationError) -> JSONResponse:
        return _error_response(
            status.HTTP_422_UNPROCESSABLE_ENTITY,
            "VALIDATION_ERROR",
            "请求参数校验失败。",
            exc.errors(),
        )

    @app.exception_handler(Exception)
    async def handle_unexpected_error(_request: Request, exc: Exception) -> JSONResponse:
        # 不把框架、文件系统或第三方库的异常消息作为 API 契约暴露给客户端。
        LOGGER.exception("Unhandled OmniCrawl API request error", exc_info=exc)
        return _error_response(
            status.HTTP_500_INTERNAL_SERVER_ERROR,
            "INTERNAL_ERROR",
            "服务器内部错误。",
        )

    @app.get("/health", tags=["system"])
    def health() -> dict[str, Any]:
        return _data({"status": "ok", "service": "omnicrawl"})

    router = APIRouter(prefix=API_PREFIX, dependencies=[Depends(_authorize)])

    @router.get("/runtime", tags=["system"])
    def runtime(request: Request) -> dict[str, Any]:
        service = _service(request)
        active_run = service.active_run
        return _data(
            {
                "workspace_root": str(service.agent.workspace_root),
                "session_id": str(service.agent.current_session_id),
                "model": service.agent.current_model,
                "reasoning_effort": service.agent.reasoning_effort,
                "approval_mode": service.agent.approval_mode,
                "active_run_id": active_run.run_id if active_run is not None else None,
            }
        )

    @router.post("/runs", status_code=status.HTTP_202_ACCEPTED, tags=["runs"])
    def create_run(payload: RunRequest, request: Request) -> dict[str, Any]:
        return _data(_service(request).start_run(payload.message).summary())

    @router.get("/runs/{run_id}", tags=["runs"])
    def get_run(run_id: str, request: Request) -> dict[str, Any]:
        return _data(_service(request).get_run(run_id).summary())

    @router.get("/runs/{run_id}/events", tags=["runs"])
    async def stream_run_events(
        run_id: str,
        request: Request,
        follow: bool = Query(default=True),
        last_event_id_header: Optional[str] = Header(default=None, alias="Last-Event-ID"),
    ) -> StreamingResponse:
        service = _service(request)
        run = service.get_run(run_id)
        try:
            last_event_id = max(0, int(last_event_id_header or "0"))
        except ValueError as exc:
            raise APIServiceError("INVALID_EVENT_ID", "Last-Event-ID 必须是整数。") from exc

        async def generate() -> Iterable[str]:
            cursor = last_event_id
            while True:
                events = service.events_after(run, cursor)
                for event in events:
                    cursor = event.id
                    yield event.to_sse()
                if not follow or run.status in TERMINAL_RUN_STATUSES:
                    break
                if await request.is_disconnected():
                    break
                await asyncio.to_thread(service.wait_for_events, run, cursor, 15.0)
                if not service.events_after(run, cursor) and run.status not in TERMINAL_RUN_STATUSES:
                    yield ": keep-alive\n\n"

        return StreamingResponse(
            generate(),
            media_type="text/event-stream",
            headers={"Cache-Control": "no-cache", "X-Accel-Buffering": "no"},
        )

    @router.post("/runs/{run_id}/cancel", tags=["runs"])
    def cancel_run(run_id: str, request: Request) -> dict[str, Any]:
        return _data(_service(request).cancel_run(run_id).summary())

    @router.post("/runs/{run_id}/confirmations/{confirmation_id}", tags=["runs"])
    def confirm_run(
        run_id: str,
        confirmation_id: str,
        payload: ConfirmationDecision,
        request: Request,
    ) -> dict[str, Any]:
        confirmation = _service(request).decide_confirmation(
            run_id,
            confirmation_id,
            payload.approved,
        )
        return _data(
            {"confirmation_id": confirmation.confirmation_id, "approved": payload.approved}
        )

    @router.get("/monitors", tags=["monitors"])
    def list_monitors(request: Request) -> dict[str, Any]:
        agent = _service(request).agent
        try:
            tasks = agent.list_monitor_tasks()
        except AgentError as exc:
            raise APIServiceError("MONITOR_UNAVAILABLE", "后台监控不可用。", status_code=503) from exc
        return _data([_jsonable(task) for task in tasks])

    @router.get("/monitors/{monitor_id}", tags=["monitors"])
    def get_monitor(monitor_id: str, request: Request) -> dict[str, Any]:
        try:
            task = _service(request).agent.get_monitor_task(monitor_id)
        except AgentError as exc:
            raise APIServiceError("MONITOR_NOT_FOUND", "后台任务不存在。", status_code=404) from exc
        return _data(_jsonable(task))

    @router.get("/monitors/{monitor_id}/events", tags=["monitors"])
    async def stream_monitor_events(
        monitor_id: str,
        request: Request,
        follow: bool = Query(default=True),
        max_events: int = Query(default=100, ge=1, le=200),
        cursor: int = Query(default=0, ge=0),
        last_event_id_header: Optional[str] = Header(default=None, alias="Last-Event-ID"),
    ) -> StreamingResponse:
        agent = _service(request).agent
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

    @router.get("/sessions", tags=["sessions"])
    def list_sessions(
        request: Request,
        limit: int = Query(default=20, ge=1, le=100),
        archived: bool = Query(default=False),
    ) -> dict[str, Any]:
        agent = _service(request).agent
        entries = agent.list_archived_sessions(limit) if archived else agent.list_sessions(limit=limit)
        return _data([_jsonable(entry) for entry in entries])

    @router.post("/sessions", tags=["sessions"])
    def new_session(request: Request) -> dict[str, Any]:
        service = _service(request)
        service.ensure_mutation_allowed()
        service.agent.reset_conversation()
        return _data({"session_id": service.agent.current_session_id})

    @router.get("/sessions/{session_id}/events", tags=["sessions"])
    def session_events(session_id: str, request: Request) -> dict[str, Any]:
        return _data(
            [_jsonable(event) for event in _service(request).agent.load_session_events(session_id)]
        )

    @router.post("/sessions/{session_id}/resume", tags=["sessions"])
    def resume_session(session_id: str, request: Request) -> dict[str, Any]:
        service = _service(request)
        service.ensure_mutation_allowed()
        return _data(_jsonable(service.agent.resume_session(session_id)))

    @router.patch("/sessions/current", tags=["sessions"])
    def rename_session(payload: RenameRequest, request: Request) -> dict[str, Any]:
        service = _service(request)
        service.ensure_mutation_allowed()
        return _data(_jsonable(service.agent.rename_current_session(payload.title)))

    @router.post("/sessions/current/compact", tags=["sessions"])
    def compact_session(request: Request) -> dict[str, Any]:
        service = _service(request)
        service.ensure_mutation_allowed()
        return _data({"summary": service.agent.compact_conversation()})

    @router.post("/sessions/current/archive", tags=["sessions"])
    def archive_session(request: Request) -> dict[str, Any]:
        service = _service(request)
        service.ensure_mutation_allowed()
        return _data(_jsonable(service.agent.archive_current_session()))

    @router.delete("/sessions/{session_id}", tags=["sessions"])
    def delete_session(session_id: str, request: Request) -> dict[str, Any]:
        service = _service(request)
        service.ensure_mutation_allowed()
        service.agent.delete_session(session_id)
        return _data({"deleted": True, "session_id": session_id})

    @router.post("/sessions/current/export", tags=["sessions"])
    def export_session(payload: ExportRequest, request: Request) -> dict[str, Any]:
        service = _service(request)
        service.ensure_mutation_allowed()
        path = service.agent.export_current_session_markdown(payload.markdown)
        return _data({"path": str(path)})

    @router.get("/sessions/{session_id}/artifacts/{artifact_path:path}", tags=["sessions"])
    def session_artifact(session_id: str, artifact_path: str, request: Request) -> PlainTextResponse:
        if not artifact_path or ".." in Path(artifact_path).parts:
            raise APIServiceError("INVALID_ARTIFACT_PATH", "artifact 路径不合法。")
        try:
            text = _service(request).agent.read_session_artifact_text(session_id, artifact_path)
        except Exception as exc:
            # artifact 存储可能来自文件系统或会话索引；底层异常不应成为
            # 对外 API 文案，以免泄露路径、会话细节或敏感配置。
            LOGGER.warning(
                "Unable to read session artifact %s/%s: %s",
                session_id,
                artifact_path,
                exc,
            )
            raise APIServiceError(
                "ARTIFACT_NOT_FOUND",
                "会话 artifact 不存在。",
                status_code=status.HTTP_404_NOT_FOUND,
            ) from exc
        return PlainTextResponse(text, media_type="text/html; charset=utf-8")

    @router.get("/projects", tags=["projects"])
    def list_projects(request: Request) -> dict[str, Any]:
        return _data([_jsonable(entry) for entry in _service(request).agent.list_projects()])

    @router.post("/projects", tags=["projects"])
    def create_project(payload: ProjectRequest, request: Request) -> dict[str, Any]:
        service = _service(request)
        service.ensure_mutation_allowed()
        return _data(_jsonable(service.agent.create_project(payload.name, payload.path)))

    @router.post("/projects/import", tags=["projects"])
    def import_project(payload: ProjectRequest, request: Request) -> dict[str, Any]:
        service = _service(request)
        service.ensure_mutation_allowed()
        return _data(_jsonable(service.agent.import_project(payload.name, payload.path)))

    @router.patch("/projects", tags=["projects"])
    def rename_project(payload: ProjectRenameRequest, request: Request) -> dict[str, Any]:
        service = _service(request)
        service.ensure_mutation_allowed()
        return _data(_jsonable(service.agent.rename_project(payload.path, payload.name)))

    @router.post("/projects/pin", tags=["projects"])
    def pin_project(payload: ProjectPinRequest, request: Request) -> dict[str, Any]:
        service = _service(request)
        service.ensure_mutation_allowed()
        return _data(_jsonable(service.agent.pin_project(payload.path, pinned=payload.pinned)))

    @router.delete("/projects", tags=["projects"])
    def remove_project(path: str, request: Request) -> dict[str, Any]:
        service = _service(request)
        service.ensure_mutation_allowed()
        service.agent.remove_project(path)
        return _data({"removed": True, "path": path})

    @router.post("/projects/switch", tags=["projects"])
    def switch_project(payload: ProjectPathRequest, request: Request) -> dict[str, Any]:
        service = _service(request)
        service.ensure_mutation_allowed()
        new_root = service.agent.switch_workspace(payload.path)
        return _data({"workspace_root": str(new_root), "session_id": service.agent.current_session_id})

    @router.get("/models", tags=["configuration"])
    def list_models(request: Request) -> dict[str, Any]:
        agent = _service(request).agent
        try:
            options = ensure_current_model_option(
                detect_model_options(agent.config.llm),
                agent.current_model,
            )
        except ModelCatalogError as exc:
            raise APIServiceError("MODEL_LIST_FAILED", str(exc), status_code=502) from exc
        return _data([option.to_ui_dict() for option in options])

    @router.put("/models/current", tags=["configuration"])
    def set_model(payload: ModelChangeRequest, request: Request) -> dict[str, Any]:
        service = _service(request)
        service.ensure_mutation_allowed()
        service.agent.set_model(payload.model)
        save_llm_model(payload.model)
        return _data({"model": service.agent.current_model})

    @router.put("/reasoning", tags=["configuration"])
    def set_reasoning(payload: ReasoningChangeRequest, request: Request) -> dict[str, Any]:
        service = _service(request)
        service.ensure_mutation_allowed()
        effort = service.agent.set_reasoning_effort(payload.effort)
        save_reasoning_effort(effort)
        return _data({"reasoning_effort": effort})

    @router.put("/approval", tags=["configuration"])
    def set_approval(payload: ApprovalChangeRequest, request: Request) -> dict[str, Any]:
        service = _service(request)
        service.ensure_mutation_allowed()
        mode = normalize_approval_mode(payload.mode)
        service.agent.set_approval_mode(mode)
        save_approval_mode(mode)
        return _data({"approval_mode": mode})

    @router.get("/history", tags=["support"])
    def prompt_history(
        request: Request,
        query: str = "",
        limit: int = Query(default=20, ge=1, le=100),
        current_session_only: bool = False,
    ) -> dict[str, Any]:
        entries = _service(request).agent.search_prompt_history(
            query=query,
            limit=limit,
            current_session_only=current_session_only,
        )
        return _data([_jsonable(entry) for entry in entries])

    @router.get("/skills", tags=["support"])
    def skills(request: Request) -> dict[str, Any]:
        manager = _service(request).agent.skill_manager
        if manager is None:
            return _data([])
        return _data([_jsonable(meta) for meta in manager.list_all()])

    @router.get("/mcp", tags=["support"])
    def mcp_status(request: Request) -> dict[str, Any]:
        return _data({"status": _service(request).agent.format_mcp_status()})

    @router.post("/memory/clean", tags=["support"])
    def clean_memory(request: Request) -> dict[str, Any]:
        service = _service(request)
        service.ensure_mutation_allowed()
        deleted = service.agent.clean_memory()
        return _data({"deleted": deleted, "count": len(deleted)})

    app.include_router(router)
    return app


async def _authorize(request: Request) -> None:
    expected = request.app.state.api_config.bearer_token
    authorization = request.headers.get("Authorization", "")
    scheme, separator, token = authorization.partition(" ")
    if not separator or scheme.casefold() != "bearer" or not secrets.compare_digest(token, expected):
        raise APIServiceError(
            "UNAUTHORIZED",
            "缺少或无效的 Bearer Token。",
            status_code=status.HTTP_401_UNAUTHORIZED,
        )


def _service(request: Request) -> AgentAPIService:
    service = request.app.state.service
    if service is None:
        raise APIServiceError(
            "SERVICE_UNAVAILABLE",
            "Agent 服务尚未就绪。",
            status_code=status.HTTP_503_SERVICE_UNAVAILABLE,
        )
    return service


def _data(value: Any) -> dict[str, Any]:
    return {"data": value}


def _error_response(status_code: int, code: str, message: str, details: Any = None) -> JSONResponse:
    return JSONResponse(
        status_code=status_code,
        content={"error": {"code": code, "message": message, "details": details}},
    )


def _jsonable(value: Any) -> Any:
    if is_dataclass(value):
        return _jsonable(asdict(value))
    if isinstance(value, datetime):
        return value.isoformat()
    if isinstance(value, Path):
        return str(value)
    if isinstance(value, dict):
        return {str(key): _jsonable(item) for key, item in value.items()}
    if isinstance(value, (list, tuple)):
        return [_jsonable(item) for item in value]
    if hasattr(value, "__dict__") and not isinstance(value, type):
        return {
            key: _jsonable(item)
            for key, item in vars(value).items()
            if not key.startswith("_")
        }
    return value


__all__ = [
    "APIConfig",
    "APIServiceError",
    "AgentAPIService",
    "create_app",
    "create_default_agent",
    "load_api_config",
]
