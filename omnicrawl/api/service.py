"""OmniCrawl API 运行编排服务。

`AgentAPIService` 拥有单活动生成、确认等待、取消和 SSE 事件缓冲，不实现 HTTP 细节。
"""

from __future__ import annotations

import logging
import secrets
import threading
import time
from typing import Any, Callable

from fastapi import status

from ..agent import ToolCall, ToolResult
from ..state.session_artifacts import redact_sensitive_text
from .models import (
    ACTIVE_RUN_STATUSES,
    APIServiceError,
    PendingConfirmation,
    RunCancelled,
    RunEvent,
    RunState,
)


LOGGER = logging.getLogger(__name__)


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

    def close(self) -> None:
        with self._lock:
            if self._closed:
                return
            self._closed = True
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
    def _ensure_event_cursor_available(run: RunState, last_event_id: int) -> None:
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
