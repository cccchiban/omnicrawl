"""OmniCrawl API 运行编排服务。

`AgentAPIService` 拥有单活动生成、确认等待、取消和 SSE 事件缓冲，不实现 HTTP 细节。
"""

from __future__ import annotations

import logging
import secrets
import threading
import time
from dataclasses import dataclass, field
from typing import Any, Callable

from fastapi import status

from ..agent import ToolCall, ToolResult
from ..agent.subagents.approval import SubAgentApprovalOrigin, SubAgentApprovalRequest
from ..agent.tools import public_tool_arguments
from ..state.session_artifacts import redact_sensitive_text, redact_sensitive_values
from .models import (
    ACTIVE_RUN_STATUSES,
    APIServiceError,
    PendingConfirmation,
    RunCancelled,
    RunEvent,
    RunState,
)


LOGGER = logging.getLogger(__name__)


@dataclass(frozen=True)
class _SubAgentTaskSource:
    """把后台任务绑定到创建它的 API Run/Session，避免后续回合串线。"""

    run_id: str
    session_id: str


@dataclass
class _SubAgentEventStream:
    """当前 Session 的独立后台任务 SSE 缓冲。"""

    events: list[RunEvent] = field(default_factory=list)
    next_event_id: int = 1
    condition: threading.Condition = field(default_factory=threading.Condition)


@dataclass
class _BackgroundSubAgentConfirmation:
    """父 Run 结束后仍可由当前 Session 决议的单次后台审批。"""

    confirmation_id: str
    session_id: str
    task_id: str
    batch_id: str
    agent_label: str
    description: str
    tool_name: str
    arguments: dict[str, Any]
    risk_summary: str
    created_at: float = field(default_factory=time.time)
    updated_at: float = field(default_factory=time.time)
    status: str = "pending"
    decision: bool | None = None
    resolved: threading.Event = field(default_factory=threading.Event)

    def as_public_dict(self) -> dict[str, Any]:
        """只投影远程控制面所需的脱敏任务来源与工具摘要。"""

        return {
            "confirmation_id": self.confirmation_id,
            "task_id": self.task_id,
            "batch_id": self.batch_id,
            "agent_label": self.agent_label,
            "description": self.description,
            "tool": self.tool_name,
            "arguments": dict(self.arguments),
            "risk_summary": self.risk_summary,
            "status": self.status,
            "created_at": self.created_at,
            "updated_at": self.updated_at,
        }


class AgentAPIService:
    """单 Agent、单活动生成的线程安全服务编排器。"""

    def __init__(
        self,
        agent: Any,
        *,
        confirmation_timeout_seconds: float = 300.0,
        max_retained_runs: int = 100,
        max_events_per_run: int = 2000,
        close_timeout_seconds: float = 5.0,
    ) -> None:
        self.agent = agent
        self.confirmation_timeout_seconds = confirmation_timeout_seconds
        self.max_retained_runs = max(1, int(max_retained_runs))
        self.max_events_per_run = max(10, int(max_events_per_run))
        self.close_timeout_seconds = max(0.1, float(close_timeout_seconds))
        self._lock = threading.RLock()
        self._runs: dict[str, RunState] = {}
        self._active_run_id = ""
        self._workers: dict[str, threading.Thread] = {}
        self._closed = False
        # 后台任务可能跨越父 Run，因此其公开事件和等待中的审批不能只存放在
        # RunState。任务来源表只保存 Run/Session ID，不保留 prompt 或模型输出。
        self._subagent_task_sources: dict[str, _SubAgentTaskSource] = {}
        self._subagent_event_streams: dict[str, _SubAgentEventStream] = {}
        self._background_subagent_confirmations: dict[
            str,
            _BackgroundSubAgentConfirmation,
        ] = {}
        self.agent.set_confirm_handler(self._confirm_tool_call)
        set_subagent_confirm_handler = getattr(
            self.agent,
            "set_subagent_confirm_handler",
            None,
        )
        if callable(set_subagent_confirm_handler):
            set_subagent_confirm_handler(self._confirm_subagent_tool_call)
        set_subagent_event_handler = getattr(
            self.agent,
            "set_subagent_event_handler",
            None,
        )
        if callable(set_subagent_event_handler):
            set_subagent_event_handler(self._on_persistent_subagent_event)

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
            if self._closed:
                raise APIServiceError(
                    "SERVICE_CLOSED",
                    "API 服务已关闭。",
                    status_code=status.HTTP_503_SERVICE_UNAVAILABLE,
                )
            self.ensure_mutation_allowed()
            self._prune_runs_locked()
            run_id = secrets.token_hex(12)
            run = RunState(
                run_id=run_id,
                message=text,
                session_id=str(getattr(self.agent, "current_session_id", "")),
            )
            self._runs[run_id] = run
            self._active_run_id = run_id
            self._prune_runs_locked()
            thread = threading.Thread(target=self._execute_run, args=(run,), daemon=True)
            self._workers[run_id] = thread
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
            self._ensure_event_cursor_available(run, last_event_id)
            return [event for event in run.events if event.id > last_event_id]

    def wait_for_events(self, run: RunState, last_event_id: int, timeout: float) -> None:
        with run.condition:
            self._ensure_event_cursor_available(run, last_event_id)
            if not any(event.id > last_event_id for event in run.events):
                run.condition.wait(timeout=timeout)

    def current_subagent_session_id(self) -> str:
        """冻结当前 API 可见的 Session，用于后续后台事件流与审批授权。"""

        return str(getattr(self.agent, "current_session_id", "") or "")

    def subagent_events_after(
        self,
        session_id: str,
        last_event_id: int,
    ) -> list[RunEvent]:
        """读取指定已冻结 Session 的后台 SubAgent SSE 事件。"""

        stream = self._subagent_event_stream(session_id)
        with stream.condition:
            self._ensure_event_cursor_available(stream, last_event_id)
            return [event for event in stream.events if event.id > last_event_id]

    def wait_for_subagent_events(
        self,
        session_id: str,
        last_event_id: int,
        timeout: float,
    ) -> None:
        """等待当前 Session 的后台 SubAgent 事件，避免 API SSE 忙轮询。"""

        stream = self._subagent_event_stream(session_id)
        with stream.condition:
            self._ensure_event_cursor_available(stream, last_event_id)
            if not any(event.id > last_event_id for event in stream.events):
                stream.condition.wait(timeout=timeout)

    def list_background_subagent_confirmations(self) -> list[dict[str, Any]]:
        """列出当前 Session 仍可决议的跨父 Run 后台审批。"""

        session_id = self.current_subagent_session_id()
        with self._lock:
            self._expire_background_confirmations_locked()
            confirmations = [
                confirmation.as_public_dict()
                for confirmation in self._background_subagent_confirmations.values()
                if confirmation.session_id == session_id
                and confirmation.status == "pending"
            ]
        return sorted(confirmations, key=lambda item: (item["created_at"], item["confirmation_id"]))

    def decide_background_subagent_confirmation(
        self,
        confirmation_id: str,
        approved: bool,
    ) -> dict[str, Any]:
        """决议当前 Session 的后台审批，首个终态操作拥有唯一胜出权。"""

        session_id = self.current_subagent_session_id()
        with self._lock:
            self._expire_background_confirmations_locked()
            confirmation = self._background_subagent_confirmations.get(confirmation_id)
            if confirmation is None or confirmation.session_id != session_id:
                raise APIServiceError(
                    "SUBAGENT_CONFIRMATION_NOT_FOUND",
                    "未找到当前会话的后台 SubAgent 确认请求。",
                    status_code=status.HTTP_404_NOT_FOUND,
                )
            if confirmation.resolved.is_set():
                raise APIServiceError(
                    "CONFIRMATION_RESOLVED",
                    "该确认请求已经处理。",
                    status_code=status.HTTP_409_CONFLICT,
                )
            self._resolve_background_confirmation_locked(
                confirmation,
                approved=approved,
                status_name="approved" if approved else "rejected",
                event_name="subagent.confirmation.resolved",
            )
            return confirmation.as_public_dict()

    def close(self) -> None:
        with self._lock:
            if self._closed:
                return
            self._closed = True
            self._reject_all_background_confirmations_locked(
                event_name="subagent.confirmation.cancelled",
            )
            active_run = self._runs.get(self._active_run_id) if self._active_run_id else None
            workers = list(self._workers.values())
        if active_run is not None:
            self.cancel_run(active_run.run_id)
        deadline = time.monotonic() + self.close_timeout_seconds
        for worker in workers:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                break
            worker.join(timeout=remaining)
        alive = [worker for worker in workers if worker.is_alive()]
        if alive:
            # 不能在线程仍可能访问 Agent 时关闭其资源。保持服务 closed，并由
            # worker 的 finally 在最后一个线程退出后完成延迟关闭。
            LOGGER.warning("API close timeout: %d worker(s) still running", len(alive))
            return
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
                on_stream_rollback=lambda: self._on_status(
                    run,
                    "模型流中断，先前输出已作废，正在自动重试",
                    check_cancelled,
                ),
                cancel_check=check_cancelled,
                on_subagent_event=lambda event_name, payload: self._on_subagent_event(
                    run,
                    event_name,
                    payload,
                ),
            )
            check_cancelled()
            run.status = "completed"
            run.result = result
            self._emit(run, "run.completed", {"run_id": run.run_id, "result": result})
        except (RunCancelled, KeyboardInterrupt) as exc:
            run.status = "cancelled"
            run.error = str(exc) or "用户取消生成。"
            self._emit(run, "run.cancelled", {"run_id": run.run_id, "message": run.error})
        except Exception as exc:
            # 第三方异常消息不可信：日志也必须脱敏，避免诊断输出泄露凭据。
            LOGGER.error(
                "OmniCrawl API run %s failed: %s: %s",
                run.run_id,
                type(exc).__name__,
                redact_sensitive_text(str(exc)),
            )
            run.status = "failed"
            run.error = "生成任务失败。"
            self._emit(run, "run.failed", {"run_id": run.run_id, "message": run.error})
        finally:
            run.updated_at = time.time()
            close_agent = False
            with self._lock:
                if self._active_run_id == run.run_id:
                    self._active_run_id = ""
                self._workers.pop(run.run_id, None)
                self._prune_runs_locked()
                close_agent = self._closed and not self._workers
            if close_agent:
                self.agent.close()
            with run.condition:
                run.condition.notify_all()

    def _confirm_subagent_tool_call(self, request: SubAgentApprovalRequest) -> bool:
        """将子任务确认路由到所属父 Run 或跨父 Run 的后台控制面。"""

        with self._lock:
            source = self._subagent_task_sources.get(request.origin.task_id)
            active = self._runs.get(self._active_run_id) if self._active_run_id else None
            parent_run = None
            if active is not None and active.status in ACTIVE_RUN_STATUSES:
                # 没有来源记录时保持旧的同步确认兼容；有记录则只允许回到创建
                # 该任务的父 Run，避免后续无关 Run 接管旧后台任务的风险操作。
                if source is None or source.run_id == active.run_id:
                    parent_run = active
            session_id = source.session_id if source is not None else ""

        if parent_run is not None:
            return self._confirm_tool_call(
                request.tool_name,
                dict(request.public_arguments),
                subagent_origin=request.origin,
                run=parent_run,
            )
        if not session_id:
            # 未观察到任务创建事件时无法证明 owner/session 来源，保守拒绝。
            return False
        return self._confirm_background_subagent_tool_call(request, session_id)

    def _confirm_background_subagent_tool_call(
        self,
        request: SubAgentApprovalRequest,
        session_id: str,
    ) -> bool:
        """等待当前 Session 的远程决议，不依赖已结束的顶层 Run。"""

        confirmation = _BackgroundSubAgentConfirmation(
            confirmation_id=request.request_id,
            session_id=session_id,
            task_id=request.origin.task_id,
            batch_id=request.origin.batch_id,
            agent_label=request.origin.agent_label,
            description=request.origin.description,
            tool_name=request.tool_name,
            arguments=redact_sensitive_values(dict(request.public_arguments)),
            risk_summary=request.risk_summary,
        )
        with self._lock:
            if self._closed:
                return False
            if confirmation.confirmation_id in self._background_subagent_confirmations:
                # Broker request_id 在同一 Agent 内唯一；若重复，拒绝而不是让两个
                # worker 共享一个可能已经被决议的 Event。
                return False
            self._background_subagent_confirmations[confirmation.confirmation_id] = confirmation
            self._emit_subagent_event_locked(
                session_id,
                "subagent.confirmation.required",
                {
                    **request.origin.as_public_dict(),
                    "confirmation_id": confirmation.confirmation_id,
                    "tool": request.tool_name,
                    "arguments": dict(confirmation.arguments),
                    "risk_summary": request.risk_summary,
                    "timeout_seconds": self.confirmation_timeout_seconds,
                    "status": "pending",
                },
            )

        deadline = time.monotonic() + self.confirmation_timeout_seconds
        while not confirmation.resolved.is_set():
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                with self._lock:
                    if not confirmation.resolved.is_set():
                        self._resolve_background_confirmation_locked(
                            confirmation,
                            approved=False,
                            status_name="expired",
                            event_name="subagent.confirmation.expired",
                        )
                break
            confirmation.resolved.wait(timeout=min(0.1, remaining))
        return confirmation.decision is True

    def _confirm_tool_call(
        self,
        tool_name: str,
        arguments: dict[str, Any],
        *,
        subagent_origin: SubAgentApprovalOrigin | None = None,
        run: RunState | None = None,
    ) -> bool:
        run = self.active_run if run is None else run
        if run is None:
            return False
        confirmation_id = secrets.token_hex(12)
        safe_arguments = public_tool_arguments(tool_name, arguments)
        origin_payload = (
            subagent_origin.as_public_dict() if subagent_origin is not None else None
        )
        confirmation = PendingConfirmation(
            confirmation_id=confirmation_id,
            tool_name=tool_name,
            arguments=safe_arguments,
            task_id=origin_payload["task_id"] if origin_payload is not None else None,
            agent_label=(
                origin_payload["agent_label"] if origin_payload is not None else None
            ),
            batch_id=origin_payload["batch_id"] if origin_payload is not None else None,
        )
        run.confirmations[confirmation_id] = confirmation
        run.status = "waiting_confirmation"
        event_payload = {
            "confirmation_id": confirmation_id,
            "tool": tool_name,
            "arguments": safe_arguments,
            "timeout_seconds": self.confirmation_timeout_seconds,
        }
        if origin_payload is not None:
            event_payload["subagent"] = origin_payload
        self._emit(run, "confirmation.required", event_payload)

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
        arguments = public_tool_arguments(tool_call.name, tool_call.arguments)
        self._emit(
            run,
            "tool.started",
            {
                "step": step,
                "tool_call_id": tool_call.id,
                "tool": tool_call.name,
                "arguments": arguments,
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

    def _on_subagent_event(
        self,
        run: RunState,
        event_name: str,
        payload: dict[str, Any],
    ) -> None:
        # 子任务 cancelled 事件通常发生在父取消令牌置位之后，不能再次调用
        # check_cancelled，否则终态会被拦在 SSE 之外。父 Run 的取消仍由主循环处理。
        self._record_subagent_task_source(run, event_name, payload)
        self._emit(
            run,
            event_name,
            {
                "run_id": run.run_id,
                "session_id": run.session_id,
                **payload,
            },
        )

    def _on_persistent_subagent_event(
        self,
        event_name: str,
        payload: dict[str, Any],
    ) -> None:
        """接收跨父回合的脱敏事件，并写入所属 Session 的独立 SSE 流。"""

        safe_payload = redact_sensitive_values(dict(payload))
        with self._lock:
            if self._closed:
                return
            source = self._subagent_source_for_event_locked(safe_payload)
            if source is None:
                # 未知来源不能因为当前 Session 恰好可见就被远程客户端接管。
                return
            self._emit_subagent_event_locked(
                source.session_id,
                event_name,
                {
                    "session_id": source.session_id,
                    "parent_run_id": source.run_id,
                    **safe_payload,
                },
            )

            if event_name == "subagent.task.approval_cancelled":
                request_id = safe_payload.get("request_id")
                confirmation = (
                    self._background_subagent_confirmations.get(str(request_id))
                    if isinstance(request_id, str)
                    else None
                )
                if (
                    confirmation is not None
                    and confirmation.session_id == source.session_id
                    and not confirmation.resolved.is_set()
                ):
                    self._resolve_background_confirmation_locked(
                        confirmation,
                        approved=False,
                        status_name="cancelled",
                        event_name="subagent.confirmation.cancelled",
                    )

            if event_name in {
                "subagent.task.completed",
                "subagent.task.failed",
                "subagent.task.cancelled",
            }:
                task_id = safe_payload.get("task_id")
                if isinstance(task_id, str):
                    self._subagent_task_sources.pop(task_id, None)

    def _record_subagent_task_source(
        self,
        run: RunState,
        event_name: str,
        payload: dict[str, Any],
    ) -> None:
        """从 Run 级事件记录任务来源，兼容不支持持久事件观察器的测试 Host。"""

        task_id = payload.get("task_id")
        if not isinstance(task_id, str) or not task_id:
            return
        with self._lock:
            if event_name in {
                "subagent.task.completed",
                "subagent.task.failed",
                "subagent.task.cancelled",
            }:
                self._subagent_task_sources.pop(task_id, None)
                return
            self._subagent_task_sources[task_id] = _SubAgentTaskSource(
                run_id=run.run_id,
                session_id=run.session_id,
            )

    def _subagent_source_for_event_locked(
        self,
        payload: dict[str, Any],
    ) -> _SubAgentTaskSource | None:
        """解析事件的可信任务来源；仅创建时的活动父 Run 可建立新映射。"""

        task_id = payload.get("task_id")
        if isinstance(task_id, str) and task_id:
            known = self._subagent_task_sources.get(task_id)
            if known is not None:
                return known
        active = self._runs.get(self._active_run_id) if self._active_run_id else None
        if active is None or active.status not in ACTIVE_RUN_STATUSES:
            return None
        source = _SubAgentTaskSource(run_id=active.run_id, session_id=active.session_id)
        if isinstance(task_id, str) and task_id:
            self._subagent_task_sources[task_id] = source
        return source

    def _subagent_event_stream(self, session_id: str) -> _SubAgentEventStream:
        with self._lock:
            return self._subagent_event_streams.setdefault(
                session_id,
                _SubAgentEventStream(),
            )

    def _emit_subagent_event_locked(
        self,
        session_id: str,
        event_name: str,
        payload: dict[str, Any],
    ) -> None:
        """向 Session 级后台流追加一个已经脱敏的事件。"""

        stream = self._subagent_event_streams.setdefault(
            session_id,
            _SubAgentEventStream(),
        )
        with stream.condition:
            event = RunEvent(
                id=stream.next_event_id,
                event=event_name,
                data=redact_sensitive_values(dict(payload)),
            )
            stream.next_event_id += 1
            stream.events.append(event)
            if len(stream.events) > self.max_events_per_run:
                del stream.events[: len(stream.events) - self.max_events_per_run]
            stream.condition.notify_all()

    def _resolve_background_confirmation_locked(
        self,
        confirmation: _BackgroundSubAgentConfirmation,
        *,
        approved: bool,
        status_name: str,
        event_name: str,
    ) -> bool:
        """在同一把服务锁下完成一次后台确认，迟到决议不可覆盖首个结果。"""

        if confirmation.resolved.is_set():
            return False
        confirmation.decision = approved
        confirmation.status = status_name
        confirmation.updated_at = time.time()
        confirmation.resolved.set()
        self._emit_subagent_event_locked(
            confirmation.session_id,
            event_name,
            {
                **confirmation.as_public_dict(),
                "approved": approved,
            },
        )
        self._prune_background_confirmations_locked()
        return True

    def _expire_background_confirmations_locked(self) -> None:
        deadline = time.time() - self.confirmation_timeout_seconds
        for confirmation in tuple(self._background_subagent_confirmations.values()):
            if confirmation.status != "pending" or confirmation.created_at > deadline:
                continue
            self._resolve_background_confirmation_locked(
                confirmation,
                approved=False,
                status_name="expired",
                event_name="subagent.confirmation.expired",
            )

    def _reject_all_background_confirmations_locked(self, *, event_name: str) -> None:
        for confirmation in tuple(self._background_subagent_confirmations.values()):
            if confirmation.status != "pending":
                continue
            self._resolve_background_confirmation_locked(
                confirmation,
                approved=False,
                status_name="cancelled",
                event_name=event_name,
            )

    def _prune_background_confirmations_locked(self) -> None:
        """限制已终态确认的内存保留量；待决确认始终保留。"""

        terminal = sorted(
            (
                confirmation
                for confirmation in self._background_subagent_confirmations.values()
                if confirmation.status != "pending"
            ),
            key=lambda item: (item.updated_at, item.confirmation_id),
            reverse=True,
        )
        for confirmation in terminal[self.max_retained_runs :]:
            self._background_subagent_confirmations.pop(
                confirmation.confirmation_id,
                None,
            )

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

    def _emit(self, run: RunState, event_name: str, data: dict[str, Any]) -> None:
        with run.condition:
            event = RunEvent(id=run.next_event_id, event=event_name, data=data)
            run.next_event_id += 1
            run.events.append(event)
            if len(run.events) > self.max_events_per_run:
                del run.events[: len(run.events) - self.max_events_per_run]
            run.updated_at = time.time()
            run.condition.notify_all()

    @staticmethod
    def _ensure_event_cursor_available(
        run: RunState | _SubAgentEventStream,
        last_event_id: int,
    ) -> None:
        if not run.events or last_event_id <= 0:
            return
        earliest = run.events[0].id
        if last_event_id < earliest - 1:
            raise APIServiceError(
                "EVENT_CURSOR_EXPIRED",
                "请求的事件游标已超出内存保留窗口，请重新获取任务状态。",
                status_code=status.HTTP_409_CONFLICT,
                details={"earliest_event_id": earliest},
            )

    def _prune_runs_locked(self) -> None:
        terminal = [
            run
            for run in self._runs.values()
            if run.status not in ACTIVE_RUN_STATUSES
        ]
        terminal.sort(key=lambda item: item.updated_at, reverse=True)
        keep_terminal = max(0, self.max_retained_runs - sum(
            1 for run in self._runs.values() if run.status in ACTIVE_RUN_STATUSES
        ))
        for run in terminal[keep_terminal:]:
            self._runs.pop(run.run_id, None)


__all__ = ["AgentAPIService"]
