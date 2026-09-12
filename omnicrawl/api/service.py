"""OmniCrawl API 运行编排服务。

`AgentAPIService` 拥有单活动生成、确认等待、取消和 SSE 事件缓冲，不实现 HTTP 细节。
"""

from __future__ import annotations

import json
import logging
import os
import secrets
import threading
import time
from dataclasses import dataclass, field
from typing import Any, Callable

from fastapi import status

from ..agent import AskUserRequest, ToolCall, ToolResult
from ..agent.toolkit.tools import TODO_TOOL_NAME, public_tool_arguments
from ..state.session_artifacts import redact_sensitive_text, redact_sensitive_values
from ..workspace.connector_singleton import pid_is_running
from .models import (
    ACTIVE_RUN_STATUSES,
    APIServiceError,
    PendingConfirmation,
    PendingUserQuestion,
    RunCancelled,
    RunEvent,
    RunState,
)
from .shared_store import (
    DECISION_ANSWER,
    DECISION_CANCEL,
    DECISION_CONFIRM,
    SharedRunStore,
)


LOGGER = logging.getLogger(__name__)

# 所有者扫描跨进程决策的最小间隔。审批/提问的等待循环本来就以 ≤0.1s 粒度轮询，
# 这里再限流一次，避免 check_cancelled() 的高频调用把 SQLite 查询放大成热点。
DECISION_SCAN_INTERVAL_SECONDS = 0.05

# 共享存储不可用时的跨进程等待轮询间隔。SSE 本身每 15s 发一次 keep-alive，
# 本地 SQLite 单次读取在微秒级，这个粒度对单机 API 完全够用。
SHARED_WAIT_POLL_SECONDS = 0.1


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


def _json_dict(raw: Any) -> dict[str, Any]:
    """解析共享存储里的 JSON 对象列；异常或形状不符时返回空字典。"""

    try:
        value = json.loads(raw) if raw else {}
    except (TypeError, ValueError):
        return {}
    return value if isinstance(value, dict) else {}


def _json_list(raw: Any) -> list[Any]:
    try:
        value = json.loads(raw) if raw else []
    except (TypeError, ValueError):
        return []
    return value if isinstance(value, list) else []


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
        store: SharedRunStore | None = None,
    ) -> None:
        self.agent = agent
        self.confirmation_timeout_seconds = confirmation_timeout_seconds
        self.max_retained_runs = max(1, int(max_retained_runs))
        self.max_events_per_run = max(10, int(max_events_per_run))
        self.close_timeout_seconds = max(0.1, float(close_timeout_seconds))
        # store 为 None 时是单进程部署（workers=1）：Run 状态、事件流与人工决策
        # 全部留在本进程内存里，行为与历史版本完全一致。store 不为 None 时，
        # 跨进程可见的那一层（Run 元数据、事件、审批、提问、任务来源）改由共享
        # 存储承载，因为内核会在 worker 之间轮询连接、不保证粘连。
        self._store = store
        self._owner_pid = os.getpid()
        # 决策扫描限流时间戳（仅所有者使用）。
        self._decision_scan_at: dict[str, float] = {}
        self._lock = threading.RLock()
        self._runs: dict[str, RunState] = {}
        self._active_run_id = ""
        self._workers: dict[str, threading.Thread] = {}
        self._closed = False
        # 后台任务可能跨越父 Run，因此其公开事件和等待中的审批不能只存放在
        # RunState。任务来源表只保存 Run/Session ID，不保留 prompt 或模型输出。
        self._subagent_task_sources: dict[str, _SubAgentTaskSource] = {}
        self._subagent_event_streams: dict[str, _SubAgentEventStream] = {}
        if self._store is not None:
            # 上一个 worker 崩溃时会留下永远处于活动状态的 Run，其内存状态已随
            # 进程消失。启动时按所有者 PID 存活情况收敛，避免客户端无限等待。
            try:
                self._store.reconcile_orphan_runs(pid_is_running)
            except Exception:  # noqa: BLE001 - 收敛失败不能阻断服务启动
                LOGGER.warning("收敛孤儿 API 生成任务失败", exc_info=True)
        self.agent.set_confirm_handler(self._confirm_tool_call)
        set_ask_user_handler = getattr(self.agent, "set_ask_user_handler", None)
        if callable(set_ask_user_handler):
            set_ask_user_handler(self._ask_user)
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
                "当前已有生成任务运行，暂不能修改会话或项目。",
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
            if self._store is not None:
                # 建库失败必须向上抛：Run 无法被其他 worker 解析时不能假装已受理。
                # 检查与创建由共享存储在同一写事务内完成，避免多 worker 同时启动。
                active_run_id = self._store.create_run(
                    run_id,
                    message=text,
                    session_id=run.session_id,
                    owner_pid=self._owner_pid,
                )
                if active_run_id is not None:
                    raise APIServiceError(
                        "RUN_ACTIVE",
                        "当前已有生成任务运行，暂不能开始新的生成任务。",
                        status_code=status.HTTP_409_CONFLICT,
                        details={"run_id": active_run_id},
                    )
            self._runs[run_id] = run
            self._active_run_id = run_id
            self._prune_runs_locked()
            thread = threading.Thread(target=self._execute_run, args=(run,), daemon=True)
            self._workers[run_id] = thread
            thread.start()
            return run

    @property
    def store(self) -> SharedRunStore | None:
        """当前服务使用的跨进程共享存储；单进程部署时为 None。"""

        return self._store

    def get_run(self, run_id: str) -> RunState:
        with self._lock:
            run = self._runs.get(run_id)
        if run is not None:
            # 本进程就是该 Run 的所有者：内存 RunState 是最新的权威视图。
            return run
        if self._store is not None:
            shared = self._shared_run_state(run_id)
            if shared is not None:
                return shared
        raise APIServiceError(
            "RUN_NOT_FOUND",
            f"生成任务不存在：{run_id}",
            status_code=status.HTTP_404_NOT_FOUND,
        )

    def cancel_run(self, run_id: str) -> RunState:
        run = self.get_run(run_id)
        if self._is_owner(run_id):
            self._cancel_locally(run)
            return run
        if self._store is not None:
            # 非所有者：先按本地取消的语义把待决审批/提问置为终态（让迟到的
            # 批准得到 409 而不是“200 但无效”），再投递取消决策给所有者。
            for confirmation_id in self._store.unresolved_confirmation_ids(run_id):
                self._store.resolve_confirmation(confirmation_id, False)
            for question_id in self._store.unresolved_question_ids(run_id):
                self._store.resolve_question(question_id, None)
            self._store.request_cancel(run_id)
        return run

    def decide_user_question(
        self,
        run_id: str,
        question_id: str,
        answer: str,
    ) -> PendingUserQuestion:
        run = self.get_run(run_id)
        answer_text = answer.strip()
        if not answer_text:
            raise APIServiceError("INVALID_ANSWER", "answer 不能为空。")
        if not self._is_owner(run_id):
            return self._decide_shared_question(run_id, question_id, answer_text)
        with run.condition:
            question = run.user_questions.get(question_id)
            if question is None:
                raise APIServiceError(
                    "QUESTION_NOT_FOUND",
                    f"提问请求不存在：{question_id}",
                    status_code=status.HTTP_404_NOT_FOUND,
                )
            if question.resolved.is_set():
                raise APIServiceError(
                    "QUESTION_RESOLVED",
                    "该提问请求已经处理。",
                    status_code=status.HTTP_409_CONFLICT,
                )
            if question.kind == "select" and answer_text not in question.options:
                raise APIServiceError(
                    "INVALID_ANSWER",
                    "answer 必须是 options 中的选项。",
                )
            question.answer = answer_text
            question.resolved.set()
            run.condition.notify_all()
            return question

    def decide_confirmation(
        self,
        run_id: str,
        confirmation_id: str,
        approved: bool,
    ) -> PendingConfirmation:
        run = self.get_run(run_id)
        if not self._is_owner(run_id):
            return self._decide_shared_confirmation(run_id, confirmation_id, approved)
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
        if self._store is not None:
            # 同步共享存储的终态，让其他 worker 上的迟到请求得到 409 而不是
            # 再投递一条无人消费的决策（本地已经处理过了）。
            self._store_best_effort(
                lambda: self._store.resolve_confirmation(confirmation_id, approved)
            )
        return confirmation

    def events_after(self, run: RunState | str, last_event_id: int) -> list[RunEvent]:
        """读取 Run 的事件流。

        ``run`` 允许是 RunState 或 run_id：多 worker 下非所有者手里没有对应的
        内存 RunState，只能靠 run_id 从共享存储读。
        """

        run_id = self._run_key(run)
        local = self._local_run(run_id)
        if local is not None:
            with local.condition:
                self._ensure_event_cursor_available(local, last_event_id)
                return [event for event in local.events if event.id > last_event_id]
        if self._store is not None:
            self._ensure_shared_cursor_available(
                self._store.earliest_event_id(run_id),
                last_event_id,
            )
            return self._store.events_after(run_id, last_event_id)
        if isinstance(run, RunState):
            with run.condition:
                self._ensure_event_cursor_available(run, last_event_id)
                return [event for event in run.events if event.id > last_event_id]
        raise APIServiceError(
            "RUN_NOT_FOUND",
            f"生成任务不存在：{run_id}",
            status_code=status.HTTP_404_NOT_FOUND,
        )

    def wait_for_events(
        self,
        run: RunState | str,
        last_event_id: int,
        timeout: float,
    ) -> None:
        run_id = self._run_key(run)
        local = self._local_run(run_id)
        if local is not None:
            with local.condition:
                self._ensure_event_cursor_available(local, last_event_id)
                if not any(event.id > last_event_id for event in local.events):
                    local.condition.wait(timeout=timeout)
            return
        if self._store is not None:
            self._wait_for_shared_events(run_id, last_event_id, timeout)
            return
        if isinstance(run, RunState):
            with run.condition:
                self._ensure_event_cursor_available(run, last_event_id)
                if not any(event.id > last_event_id for event in run.events):
                    run.condition.wait(timeout=timeout)

    def current_subagent_session_id(self) -> str:
        """冻结当前 API 可见的 Session，用于后续后台事件流与审批授权。

        多 worker 下不能取本进程 Agent 的会话：产生事件的 Run 可能由别的 worker
        持有，取本地会话会让 SSE 订到错误的 Session。改取最近一次 Run 的 Session；
        尚未跑过 Run 的 worker 退回本地 Agent 的会话。
        """

        if self._store is not None:
            latest = self._store.latest_run()
            if latest is not None:
                return latest.session_id
        return str(getattr(self.agent, "current_session_id", "") or "")

    def subagent_events_after(
        self,
        session_id: str,
        last_event_id: int,
    ) -> list[RunEvent]:
        """读取指定已冻结 Session 的后台 SubAgent SSE 事件。"""

        if self._store is not None:
            # 共享存储是唯一事实来源：所有者与旁观 worker 必须读到同一份事件。
            self._ensure_shared_cursor_available(
                self._store.earliest_subagent_event_id(session_id),
                last_event_id,
            )
            return self._store.subagent_events_after(session_id, last_event_id)
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

        if self._store is not None:
            self._wait_for_shared_subagent_events(session_id, last_event_id, timeout)
            return
        stream = self._subagent_event_stream(session_id)
        with stream.condition:
            self._ensure_event_cursor_available(stream, last_event_id)
            if not any(event.id > last_event_id for event in stream.events):
                stream.condition.wait(timeout=timeout)

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
        self._set_run_status(run, "running")
        run.updated_at = time.time()
        self._emit(run, "run.started", {"run_id": run.run_id, "session_id": run.session_id})

        def check_cancelled() -> None:
            # 非所有者投递的取消/审批决策在这里被所有者消费。
            self._maybe_drain_shared_decisions(run)
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
                on_todo_update=lambda payload: self._on_todo_update(run, payload),
                cancel_check=check_cancelled,
                on_subagent_event=lambda event_name, payload: self._on_subagent_event(
                    run,
                    event_name,
                    payload,
                ),
            )
            check_cancelled()
            self._set_run_status(run, "completed", result=result)
            self._emit(run, "run.completed", {"run_id": run.run_id, "result": result})
        except (RunCancelled, KeyboardInterrupt) as exc:
            cancelled_message = str(exc) or "用户取消生成。"
            self._set_run_status(run, "cancelled", error=cancelled_message)
            self._emit(run, "run.cancelled", {"run_id": run.run_id, "message": run.error})
        except Exception as exc:
            # 第三方异常消息不可信：日志也必须脱敏，避免诊断输出泄露凭据。
            LOGGER.error(
                "OmniCrawl API run %s failed: %s: %s",
                run.run_id,
                type(exc).__name__,
                redact_sensitive_text(str(exc)),
            )
            self._set_run_status(run, "failed", error="生成任务失败。")
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

    def _ask_user(self, request: AskUserRequest, *, run: RunState | None = None) -> str | None:
        run = self.active_run if run is None else run
        if run is None:
            return None
        question_id = secrets.token_hex(12)
        question = PendingUserQuestion(
            question_id=question_id,
            kind=request.kind,
            question=request.question,
            options=request.options,
        )
        with run.condition:
            if run.cancel_requested.is_set():
                raise RunCancelled("用户取消生成。")
            run.user_questions[question_id] = question
            self._set_run_status(run, "waiting_user")
        self._register_shared_question(run, question_id, request)
        self._emit(
            run,
            "ask_user.required",
            {
                "question_id": question_id,
                "kind": question.kind,
                "question": question.question,
                "options": list(question.options),
                "timeout_seconds": self.confirmation_timeout_seconds,
            },
        )

        deadline = time.monotonic() + self.confirmation_timeout_seconds
        while not question.resolved.is_set():
            self._maybe_drain_shared_decisions(run)
            if run.cancel_requested.is_set():
                with run.condition:
                    if not question.resolved.is_set():
                        question.answer = None
                        question.resolved.set()
                        run.condition.notify_all()
                raise RunCancelled("用户取消生成。")
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                with run.condition:
                    if not question.resolved.is_set():
                        question.answer = None
                        question.resolved.set()
                        run.condition.notify_all()
                self._emit(
                    run,
                    "ask_user.expired",
                    {"question_id": question.question_id},
                )
                break
            question.resolved.wait(timeout=min(0.1, remaining))
        if run.cancel_requested.is_set():
            raise RunCancelled("用户取消生成。")
        self._set_run_status(run, "running")
        return question.answer

    def _confirm_tool_call(
        self,
        tool_name: str,
        arguments: dict[str, Any],
        *,
        run: RunState | None = None,
    ) -> bool:
        run = self.active_run if run is None else run
        if run is None:
            return False
        confirmation_id = secrets.token_hex(12)
        safe_arguments = public_tool_arguments(tool_name, arguments)
        confirmation = PendingConfirmation(
            confirmation_id=confirmation_id,
            tool_name=tool_name,
            arguments=safe_arguments,
        )
        run.confirmations[confirmation_id] = confirmation
        self._set_run_status(run, "waiting_confirmation")
        self._register_shared_confirmation(
            run,
            confirmation_id,
            tool_name,
            safe_arguments,
        )
        event_payload = {
            "confirmation_id": confirmation_id,
            "tool": tool_name,
            "arguments": safe_arguments,
            "timeout_seconds": self.confirmation_timeout_seconds,
        }
        self._emit(run, "confirmation.required", event_payload)

        deadline = time.monotonic() + self.confirmation_timeout_seconds
        while not confirmation.resolved.is_set():
            # 审批可能由任意 worker 投递（内核不保证粘连），所有者在这里消费。
            self._maybe_drain_shared_decisions(run)
            if run.cancel_requested.is_set():
                self._set_run_status(run, "running")
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
        self._set_run_status(run, "running")
        if self._store is not None:
            # 本地已定终态时同步存储标记，让其他 worker 上的迟到批准得到 409，
            # 而不是“返回 200 但无人消费”。
            self._store_best_effort(
                lambda: self._store.resolve_confirmation(
                    confirmation_id,
                    confirmation.decision is True,
                )
            )
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

    def _on_todo_update(self, run: RunState, payload: dict[str, Any]) -> None:
        """把 Agent 的执行清单（update_todos）作为事件推给订阅方。"""
        todos = payload.get("todos") if isinstance(payload, dict) else None
        if not isinstance(todos, list):
            return
        safe_items = tuple(
            {
                "id": str(item.get("id") or str(index)),
                "step": str(item.get("step") or ""),
                "completed": bool(item.get("completed")),
            }
            for index, item in enumerate(todos[:20], start=1)
            if isinstance(item, dict) and str(item.get("step") or "").strip()
        )
        run.last_todo_items = safe_items
        if self._store is not None:
            # get_run 会从共享存储还原清单，供非所有者 worker 响应轮询。
            self._store_best_effort(
                lambda: self._store.update_run(run.run_id, todo_items=list(safe_items))
            )
        self._emit(
            run,
            "todo.updated",
            {"run_id": run.run_id, "session_id": run.session_id, "todos": list(safe_items)},
        )

    def _on_tool_start(
        self,
        run: RunState,
        step: int,
        tool_call: ToolCall,
        check_cancelled: Callable[[], None],
    ) -> None:
        check_cancelled()
        if tool_call.name == TODO_TOOL_NAME:
            # Todo 是展示层状态（输入框上方计划区），不在会话流里生成工具卡；
            # 清单由 update_todos 工具的 UI 回调经 todo.updated 事件单独推送。
            return
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
        if tool_call.name == TODO_TOOL_NAME and tool_result.ok:
            # todo 工具成功时清单已由 todo.updated 承载，不产生工具卡；
            # 失败（参数非法等）仍走通用事件，用户可见错误。
            return
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
        if self._store is not None:
            self._on_shared_subagent_event(event_name, safe_payload)
            return
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
        terminal = event_name in {
            "subagent.task.completed",
            "subagent.task.failed",
            "subagent.task.cancelled",
        }
        if self._store is not None:
            # 多 worker 下任务来源必须跨进程可见：别的 worker 处理的后台事件
            # 也要能定位到创建它的 Run/Session。
            def record() -> None:
                if terminal:
                    self._store.drop_task_source(task_id)
                else:
                    self._store.record_task_source(task_id, run.run_id, run.session_id)

            self._store_best_effort(record)
            return
        with self._lock:
            if terminal:
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
        if self._store is not None:
            # 所有者写、任意 worker 读：事件必须落库，否则别的 worker 上的 SSE
            # 订阅会看不到这段输出。
            self._store_best_effort(
                lambda: self._store.append_event(run.run_id, event.id, event_name, data)
            )

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

    # ---- 跨进程共享状态辅助 -----------------------------------------------

    @staticmethod
    def _run_key(run: RunState | str) -> str:
        return run if isinstance(run, str) else run.run_id

    def _local_run(self, run_id: str) -> RunState | None:
        """本进程持有的内存 RunState；只有所有者会有。"""

        with self._lock:
            return self._runs.get(run_id)

    def _is_owner(self, run_id: str) -> bool:
        """本进程是否为该 Run 的所有者（即 start_run 发生在本 worker）。"""

        return self._local_run(run_id) is not None

    def _shared_run_state(self, run_id: str) -> RunState | None:
        """把共享存储的一条 Run 记录还原成 RunState，供路由读取状态与摘要。

        还原出的对象只用于对外读取（``status`` / ``summary()``），事件与审批
        一律以共享存储为准，因此不填充 events / confirmations。
        """

        assert self._store is not None
        record = self._store.get_run(run_id)
        if record is None:
            return None
        return RunState(
            run_id=record.run_id,
            message=record.message,
            session_id=record.session_id,
            status=record.status,
            created_at=record.created_at,
            updated_at=record.updated_at,
            result=record.result,
            error=record.error,
            last_todo_items=record.todo_items,
        )

    def _set_run_status(
        self,
        run: RunState,
        status: str,
        *,
        result: str | None = None,
        error: str | None = None,
    ) -> None:
        """同步内存与共享存储的 Run 状态。

        非所有者 worker 只能从存储读状态（SSE 循环的终态判断依赖它），所以
        所有者的每一次状态流转都必须落库。
        """

        run.status = status
        if result is not None:
            run.result = result
        if error is not None:
            run.error = error
        if self._store is None:
            return
        self._store_best_effort(
            lambda: self._store.update_run(
                run.run_id,
                status=status,
                result=result,
                error=error,
            )
        )

    def _store_best_effort(self, action: Callable[[], Any]) -> None:
        """把共享存储写入当作尽力而为，一次写失败不中断正在跑的回合。

        代价是跨进程可见性会短暂缺失（其他 worker 少看到一个事件），但所有者
        自己的内存状态仍然完整，回合结束时的终态会再次落库。
        """

        try:
            action()
        except Exception:  # noqa: BLE001 - 见上
            LOGGER.warning("写入 API 共享状态存储失败", exc_info=True)

    @staticmethod
    def _ensure_shared_cursor_available(
        earliest_event_id: int | None,
        last_event_id: int,
    ) -> None:
        """共享存储版游标校验：语义与 _ensure_event_cursor_available 一致。"""

        if earliest_event_id is None or last_event_id <= 0:
            return
        if last_event_id < earliest_event_id - 1:
            raise APIServiceError(
                "EVENT_CURSOR_EXPIRED",
                "请求的事件游标已超出共享存储的保留窗口，请重新获取任务状态。",
                status_code=status.HTTP_409_CONFLICT,
                details={"earliest_event_id": earliest_event_id},
            )

    def _wait_for_shared_events(
        self,
        run_id: str,
        last_event_id: int,
        timeout: float,
    ) -> None:
        """等待共享存储上的新 Run 事件。

        跨进程没有可用的条件变量（所有者与旁观 worker 不在同一进程），改用短
        轮询；本地 SQLite 单次存在性查询在微秒级，对单机 API 足够。
        """

        assert self._store is not None
        deadline = time.monotonic() + max(0.0, float(timeout))
        while True:
            if self._store.has_events_after(run_id, last_event_id):
                return
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                return
            time.sleep(min(SHARED_WAIT_POLL_SECONDS, remaining))

    def _wait_for_shared_subagent_events(
        self,
        session_id: str,
        last_event_id: int,
        timeout: float,
    ) -> None:
        assert self._store is not None
        deadline = time.monotonic() + max(0.0, float(timeout))
        while True:
            if self._store.has_subagent_events_after(session_id, last_event_id):
                return
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                return
            time.sleep(min(SHARED_WAIT_POLL_SECONDS, remaining))

    def _register_shared_confirmation(
        self,
        run: RunState,
        confirmation_id: str,
        tool_name: str,
        safe_arguments: dict[str, Any],
    ) -> None:
        """登记审批供其他 worker 查询；只存已脱敏的公开参数。"""

        if self._store is None:
            return
        self._store_best_effort(
            lambda: self._store.register_confirmation(
                confirmation_id,
                run_id=run.run_id,
                tool_name=tool_name,
                arguments=safe_arguments,
            )
        )

    def _register_shared_question(
        self,
        run: RunState,
        question_id: str,
        request: AskUserRequest,
    ) -> None:
        if self._store is None:
            return
        self._store_best_effort(
            lambda: self._store.register_question(
                question_id,
                run_id=run.run_id,
                kind=request.kind,
                question=request.question,
                options=request.options,
            )
        )

    def _decide_shared_confirmation(
        self,
        run_id: str,
        confirmation_id: str,
        approved: bool,
    ) -> PendingConfirmation:
        """非所有者处理审批：校验共享状态 → 原子抢占终态 → 投递给所有者。

        “抢占”用 ``UPDATE ... WHERE resolved = 0`` 的 rowcount 判定，保证多个
        worker 同时批准同一 confirmation 时只有一个成功，其余得到 409。
        """

        store = self._store
        assert store is not None
        row = store.confirmation(confirmation_id)
        if row is None or str(row["run_id"]) != run_id:
            raise APIServiceError(
                "CONFIRMATION_NOT_FOUND",
                f"确认请求不存在：{confirmation_id}",
                status_code=status.HTTP_404_NOT_FOUND,
            )
        if int(row["resolved"]):
            raise APIServiceError(
                "CONFIRMATION_RESOLVED",
                "该确认请求已经处理。",
                status_code=status.HTTP_409_CONFLICT,
            )
        if not store.resolve_confirmation(confirmation_id, approved):
            raise APIServiceError(
                "CONFIRMATION_RESOLVED",
                "该确认请求已经处理。",
                status_code=status.HTTP_409_CONFLICT,
            )
        store.push_decision(
            run_id,
            DECISION_CONFIRM,
            confirmation_id,
            {"approved": bool(approved)},
        )
        return PendingConfirmation(
            confirmation_id=confirmation_id,
            tool_name=str(row["tool_name"]),
            arguments=_json_dict(row["arguments"]),
            decision=bool(approved),
        )

    def _decide_shared_question(
        self,
        run_id: str,
        question_id: str,
        answer_text: str,
    ) -> PendingUserQuestion:
        store = self._store
        assert store is not None
        row = store.question(question_id)
        if row is None or str(row["run_id"]) != run_id:
            raise APIServiceError(
                "QUESTION_NOT_FOUND",
                f"提问请求不存在：{question_id}",
                status_code=status.HTTP_404_NOT_FOUND,
            )
        if int(row["resolved"]):
            raise APIServiceError(
                "QUESTION_RESOLVED",
                "该提问请求已经处理。",
                status_code=status.HTTP_409_CONFLICT,
            )
        options = tuple(
            str(item) for item in _json_list(row["options"])
        )
        kind = str(row["kind"])
        if kind == "select" and answer_text not in options:
            raise APIServiceError(
                "INVALID_ANSWER",
                "answer 必须是 options 中的选项。",
            )
        if not store.resolve_question(question_id, answer_text):
            raise APIServiceError(
                "QUESTION_RESOLVED",
                "该提问请求已经处理。",
                status_code=status.HTTP_409_CONFLICT,
            )
        store.push_decision(
            run_id,
            DECISION_ANSWER,
            question_id,
            {"answer": answer_text},
        )
        return PendingUserQuestion(
            question_id=question_id,
            kind=kind,
            question=str(row["question"]),
            options=options,
            answer=answer_text,
        )

    def _cancel_locally(self, run: RunState) -> None:
        """本进程内把 Run 置为取消终态（幂等，取消与决策消费都会调用）。"""

        run.cancel_requested.set()
        with run.condition:
            for confirmation in run.confirmations.values():
                if not confirmation.resolved.is_set():
                    confirmation.decision = False
                    confirmation.resolved.set()
            for question in run.user_questions.values():
                if not question.resolved.is_set():
                    question.answer = None
                    question.resolved.set()
            run.condition.notify_all()

    def _maybe_drain_shared_decisions(self, run: RunState) -> None:
        """所有者限流地消费跨进程决策（取消 / 审批 / 提问）。

        check_cancelled() 在一次工具批处理里会被高频调用，因此按 run 做时间
        限流；审批与提问的等待循环本身就是 ≤0.1s 粒度，不会被限流挡住。
        """

        if self._store is None:
            return
        now = time.monotonic()
        with self._lock:
            if now - self._decision_scan_at.get(run.run_id, 0.0) < DECISION_SCAN_INTERVAL_SECONDS:
                return
            self._decision_scan_at[run.run_id] = now
        try:
            decisions = self._store.take_decisions(run.run_id)
        except Exception:  # noqa: BLE001 - 决策扫描失败不能中断回合
            LOGGER.warning("读取跨进程决策失败", exc_info=True)
            return
        for decision in decisions:
            self._apply_shared_decision(run, decision)

    def _apply_shared_decision(self, run: RunState, decision: Any) -> None:
        if decision.kind == DECISION_CANCEL:
            self._cancel_locally(run)
            return
        if decision.kind == DECISION_CONFIRM:
            with run.condition:
                confirmation = run.confirmations.get(decision.target_id)
                if confirmation is None or confirmation.resolved.is_set():
                    return
                confirmation.decision = bool(decision.payload.get("approved"))
                confirmation.resolved.set()
                run.condition.notify_all()
            return
        if decision.kind == DECISION_ANSWER:
            with run.condition:
                question = run.user_questions.get(decision.target_id)
                if question is None or question.resolved.is_set():
                    return
                answer = decision.payload.get("answer")
                question.answer = answer if isinstance(answer, str) else None
                question.resolved.set()
                run.condition.notify_all()

    def _on_shared_subagent_event(
        self,
        event_name: str,
        safe_payload: dict[str, Any],
    ) -> None:
        """多 worker 版：后台任务事件写入共享存储的 Session 流。"""

        assert self._store is not None
        terminal = event_name in {
            "subagent.task.completed",
            "subagent.task.failed",
            "subagent.task.cancelled",
        }
        task_id = safe_payload.get("task_id")
        try:
            source = self._shared_subagent_source(safe_payload)
            if source is None:
                return
            run_id, session_id = source
            event_id = self._store.allocate_subagent_event_id(session_id)
            self._store.append_subagent_event(
                session_id,
                event_id,
                event_name,
                {
                    "session_id": session_id,
                    "parent_run_id": run_id,
                    **safe_payload,
                },
            )
            if terminal and isinstance(task_id, str):
                self._store.drop_task_source(task_id)
        except Exception:  # noqa: BLE001 - 后台事件写入失败不能影响主回合
            LOGGER.warning("写入共享 SubAgent 事件失败", exc_info=True)

    def _shared_subagent_source(
        self,
        safe_payload: dict[str, Any],
    ) -> tuple[str, str] | None:
        """解析事件的可信任务来源；语义与 _subagent_source_for_event_locked 一致。"""

        assert self._store is not None
        task_id = safe_payload.get("task_id")
        if isinstance(task_id, str) and task_id:
            known = self._store.task_source(task_id)
            if known is not None:
                return known
        with self._lock:
            active = self._runs.get(self._active_run_id) if self._active_run_id else None
        if active is None or active.status not in ACTIVE_RUN_STATUSES:
            return None
        if isinstance(task_id, str) and task_id:
            self._store.record_task_source(task_id, active.run_id, active.session_id)
        return (active.run_id, active.session_id)

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
