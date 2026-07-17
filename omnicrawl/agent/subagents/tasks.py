"""进程内 SubAgent 后台任务状态、取消、TTL 和一次性通知。

该模块不持有模型、Session 或工具对象；Coordinator 通过 ``runner`` 注入单个
任务执行。这样后台线程不会复制 Agent Loop，也不会把 prompt、推理或凭据写入
任务快照。所有公开 payload 都只来自 Coordinator 已完成安全投影的元数据。
"""
from __future__ import annotations

import logging
import threading
import time
from concurrent.futures import Future, ThreadPoolExecutor
from dataclasses import dataclass, field
from typing import Any, Callable, Mapping, Sequence

from ...state.session_artifacts import redact_sensitive_text

LOGGER = logging.getLogger(__name__)

_TERMINAL = frozenset(("completed", "failed", "cancelled"))


@dataclass(frozen=True)
class SubAgentTaskSpec:
    """后台任务的安全元数据；不得把原始 prompt 放进 metadata。"""

    task_id: str
    description: str
    agent_type: str
    batch_id: str
    metadata: Mapping[str, Any] = field(default_factory=dict)


@dataclass(frozen=True)
class SubAgentTaskSnapshot:
    """面向 list/get 的有界任务快照。"""

    task_id: str
    batch_id: str
    owner_id: str
    session_id: str
    description: str
    agent_type: str
    status: str
    result: Mapping[str, Any] | None = None
    error: Mapping[str, Any] | None = None
    created_at: float = 0.0
    updated_at: float = 0.0

    def as_dict(self) -> dict[str, Any]:
        """只公开任务元数据；owner/session 仅用于 Host 内部隔离。"""

        return {
            "task_id": self.task_id,
            "batch_id": self.batch_id,
            "description": self.description,
            "agent_type": self.agent_type,
            "status": self.status,
            "result": dict(self.result) if self.result is not None else None,
            "error": dict(self.error) if self.error is not None else None,
            "created_at": self.created_at,
            "updated_at": self.updated_at,
        }


@dataclass
class _Task:
    spec: SubAgentTaskSpec
    owner_id: str
    session_id: str
    observer: Callable[[str, dict[str, Any]], None] | None
    cancel_event: threading.Event = field(default_factory=threading.Event)
    start_event: threading.Event = field(default_factory=threading.Event)
    status: str = "queued"
    result: Mapping[str, Any] | None = None
    error: Mapping[str, Any] | None = None
    created_at: float = field(default_factory=time.time)
    updated_at: float = field(default_factory=time.time)
    future: Future[Any] | None = None


class SubAgentTaskManager:
    """管理一个 LocalToolAgent 范围内的后台任务。

    ``spawn`` 只提交线程并立即返回；任务 runner 必须合作式检查 cancel_event。锁
    保护状态和通知游标，observer 在锁外调用且异常被隔离。每个通知进入队列一次，
    ``drain_notifications`` 消费一次后即删除，确保 exactly-once，不进入普通模型历史。
    """

    def __init__(
        self,
        *,
        retention_seconds: float = 3600.0,
        max_workers: int = 4,
    ) -> None:
        self.retention_seconds = max(1.0, float(retention_seconds))
        self._executor = ThreadPoolExecutor(
            max_workers=max(1, int(max_workers)),
            thread_name_prefix="omnicrawl-subagent-bg",
        )
        self._lock = threading.RLock()
        self._tasks: dict[str, _Task] = {}
        self._batch_tasks: dict[str, set[str]] = {}
        self._notifications: dict[tuple[str, str], list[dict[str, Any]]] = {}
        self._idle_callbacks: list[
            tuple[str, str | None, Callable[[], None]]
        ] = []
        self._closed = False
        # 终态任务的 TTL 不能依赖下一次 list/get/drain 才被动执行。清理线程
        # 使用同一把状态锁的 Condition，只有最早到期的记录需要清理时才唤醒；
        # 新任务完成或永久关闭会主动唤醒它重新计算，不采用固定频率轮询。
        self._cleanup_condition = threading.Condition(self._lock)
        self._cleanup_thread = threading.Thread(
            target=self._run_cleanup_scheduler,
            name=f"omnicrawl-subagent-cleanup-{id(self):x}",
            daemon=True,
        )
        self._cleanup_thread.start()

    def spawn(
        self,
        *,
        owner_id: str,
        session_id: str,
        specs: Sequence[SubAgentTaskSpec],
        runner: Callable[[SubAgentTaskSpec, threading.Event], Mapping[str, Any]],
        observer: Callable[[str, dict[str, Any]], None] | None = None,
    ) -> dict[str, Any]:
        """登记并后台执行任务，返回安全的 batch/task ID 投影。"""
        if not specs:
            raise ValueError("后台任务不能为空。")
        with self._lock:
            if self._closed:
                raise RuntimeError("SubAgentTaskManager 已关闭。")
            if any(spec.task_id in self._tasks for spec in specs):
                raise ValueError("后台任务 ID 重复。")
            for spec in specs:
                task = _Task(
                    spec=spec,
                    owner_id=owner_id,
                    session_id=session_id,
                    observer=observer,
                )
                self._tasks[spec.task_id] = task
                self._batch_tasks.setdefault(spec.batch_id, set()).add(spec.task_id)
            tasks = tuple(self._tasks[spec.task_id] for spec in specs)
        for task in tasks:
            # worker 先提交但等待 start_event；future 赋值完成后再发布 queued。
            # 这样既保证 queued→running 事件顺序，也消除“worker 已运行但
            # cancel() 仍看到 future=None”的关闭竞态。
            with self._lock:
                future = self._executor.submit(self._run, task, runner)
                task.future = future
            self._publish(task, "queued")
            task.start_event.set()
        return {
            "batch_id": specs[0].batch_id,
            "task_ids": [spec.task_id for spec in specs],
            "status": "queued",
        }

    def _run(
        self,
        task: _Task,
        runner: Callable[[SubAgentTaskSpec, threading.Event], Mapping[str, Any]],
    ) -> None:
        task.start_event.wait()
        with self._lock:
            if task.status in _TERMINAL:
                return
            task.status = "running"
            task.updated_at = time.time()
        self._publish(task, "running")
        try:
            if task.cancel_event.is_set():
                raise _Cancelled("任务已取消。")
            result = dict(runner(task.spec, task.cancel_event))
            status = str(result.get("status", "completed"))
            if status not in _TERMINAL:
                status = "completed"
            if task.cancel_event.is_set() and status == "completed":
                status = "cancelled"
            if status == "cancelled":
                self._finish(
                    task,
                    status,
                    error={
                        "code": "SUBAGENT_CANCELLED",
                        "message": "任务已取消。",
                    },
                )
            elif status == "failed":
                self._finish(
                    task,
                    status,
                    result=result,
                    error=result.get("error"),
                )
            else:
                self._finish(task, "completed", result=result)
        except _Cancelled as exc:
            self._finish(
                task,
                "cancelled",
                error={"code": "SUBAGENT_CANCELLED", "message": str(exc)},
            )
        except BaseException as exc:  # noqa: BLE001 - worker 边界必须稳定收敛
            if "cancel" in type(exc).__name__.casefold() or task.cancel_event.is_set():
                self._finish(
                    task,
                    "cancelled",
                    error={
                        "code": "SUBAGENT_CANCELLED",
                        "message": "任务已取消。",
                    },
                )
            else:
                LOGGER.warning(
                    "后台 SubAgent worker failed: %s",
                    type(exc).__name__,
                )
                self._finish(
                    task,
                    "failed",
                    error={
                        "code": "SUBAGENT_MODEL_ERROR",
                        "message": "子任务执行失败。",
                    },
                )

    def _finish(
        self,
        task: _Task,
        status: str,
        *,
        result: Mapping[str, Any] | None = None,
        error: Mapping[str, Any] | None = None,
    ) -> None:
        with self._lock:
            if task.status in _TERMINAL:
                return
            task.status = status
            task.result = self._bound_result(result)
            task.error = self._bound_error(error)
            task.updated_at = time.time()
            # 通知清理线程重新计算最早过期时间；这一步必须与终态写入在同一
            # 临界区内，避免线程先看到“没有终态任务”而无限期等待。
            self._cleanup_condition.notify_all()
            callbacks = self._take_idle_callbacks_locked()
        self._publish(task, status)
        for callback in callbacks:
            try:
                callback()
            except Exception as exc:  # noqa: BLE001 - 清理回调不能破坏任务终态
                LOGGER.warning(
                    "SubAgent task idle callback failed: %s",
                    type(exc).__name__,
                )

    def import_recovered_snapshots(
        self,
        *,
        owner_id: str,
        session_id: str,
        snapshots: Sequence[Mapping[str, Any]],
    ) -> int:
        """导入跨进程恢复的终态任务快照。

        仅用于 list/get 控制面可见性：
        - 只接受 completed/failed/cancelled；
        - 不覆盖已有 live/同 ID 任务；
        - 不触发 runner，也不写入通知队列（避免模型上下文重复注入）；
        - 不恢复审批、Runtime、Fork 上下文或原始 prompt。
        """

        if not snapshots:
            return 0
        imported = 0
        with self._lock:
            if self._closed:
                return 0
            for raw in snapshots:
                if not isinstance(raw, Mapping):
                    continue
                task_id = str(raw.get("task_id", "") or "").strip()
                if not task_id or task_id in self._tasks:
                    # 已存在任务（含运行中）一律不覆盖，防止恢复路径干扰 live 状态。
                    continue
                status = str(raw.get("status", "") or "").strip()
                if status not in _TERMINAL:
                    continue
                snap_owner = str(raw.get("owner_id", "") or owner_id).strip()
                snap_session = str(raw.get("session_id", "") or session_id).strip()
                if snap_owner != owner_id or snap_session != session_id:
                    # 严格绑定当前 owner/session，拒绝跨会话导入。
                    continue
                batch_id = str(raw.get("batch_id", "") or "").strip()
                if not batch_id:
                    suffix = task_id[5:] if task_id.startswith("task-") else task_id
                    batch_id = f"batch-{suffix[:12]}"
                description = str(raw.get("description", "") or "SubAgent 任务")[:120]
                agent_type = str(raw.get("agent_type", "") or "unknown")[:80]
                try:
                    created_at = float(raw.get("created_at") or time.time())
                except (TypeError, ValueError):
                    created_at = time.time()
                try:
                    updated_at = float(raw.get("updated_at") or created_at)
                except (TypeError, ValueError):
                    updated_at = created_at
                result_raw = raw.get("result")
                error_raw = raw.get("error")
                result = self._bound_result(
                    result_raw if isinstance(result_raw, Mapping) else None
                )
                error = self._bound_error(
                    error_raw if isinstance(error_raw, Mapping) else None
                )
                if result is None and status == "completed":
                    result = {
                        "status": "completed",
                        "summary": "",
                        "artifacts": [],
                        "usage": {},
                        "recovered": True,
                    }
                elif result is not None:
                    result = dict(result)
                    result["recovered"] = True
                    result.setdefault("status", status)
                if error is None and status in {"failed", "cancelled"}:
                    error = {
                        "code": (
                            "SUBAGENT_CANCELLED"
                            if status == "cancelled"
                            else "SUBAGENT_ERROR"
                        ),
                        "message": (
                            "任务已取消。"
                            if status == "cancelled"
                            else "子任务失败。"
                        ),
                    }
                task = _Task(
                    spec=SubAgentTaskSpec(
                        task_id=task_id,
                        description=description,
                        agent_type=agent_type,
                        batch_id=batch_id,
                        metadata={"recovered": True},
                    ),
                    owner_id=owner_id,
                    session_id=session_id,
                    observer=None,
                    status=status,
                    result=result,
                    error=error,
                    created_at=created_at,
                    updated_at=updated_at,
                )
                self._tasks[task_id] = task
                self._batch_tasks.setdefault(batch_id, set()).add(task_id)
                imported += 1
            if imported:
                # 终态快照也参与 TTL 清理；通知调度线程重算最早过期时间。
                self._cleanup_condition.notify_all()
        return imported

    @staticmethod
    def _bound_result(result: Mapping[str, Any] | None) -> dict[str, Any] | None:
        if result is None:
            return None
        # runner 返回的结果已由 Coordinator 脱敏；这里仍只复制允许的公开字段。
        allowed = {
            "status",
            "summary",
            "artifacts",
            "usage",
            "error",
            "task_id",
            "description",
            "agent_type",
            "definition_source",
            "recovered",
        }
        bounded = {key: value for key, value in result.items() if key in allowed}
        if "summary" in bounded:
            bounded["summary"] = redact_sensitive_text(str(bounded["summary"]))[:6000]
        if isinstance(bounded.get("artifacts"), (list, tuple)):
            bounded["artifacts"] = list(bounded["artifacts"])[:16]
        if "recovered" in bounded:
            bounded["recovered"] = bool(bounded["recovered"])
        return bounded

    @staticmethod
    def _bound_error(error: Mapping[str, Any] | None) -> dict[str, Any] | None:
        if error is None:
            return None
        return {
            "code": str(error.get("code", "SUBAGENT_ERROR"))[:80],
            "message": redact_sensitive_text(
                str(error.get("message", "子任务失败。"))
            )[:500],
        }

    def _publish(self, task: _Task, status: str) -> None:
        with self._lock:
            payload: dict[str, Any] = {
                "batch_id": task.spec.batch_id,
                "task_id": task.spec.task_id,
                "description": task.spec.description[:120],
                "agent_type": task.spec.agent_type[:80],
                "status": status,
                "timestamp": time.time(),
            }
            if task.result is not None:
                payload["result"] = dict(task.result)
            if task.error is not None:
                payload["error"] = dict(task.error)
            # 模型上下文只需要终态结果；queued/running 仍实时发送给 SSE/TUI，
            # 但不进入下一次模型请求，避免高频状态挤占上下文。
            if status in _TERMINAL:
                key = (task.owner_id, task.session_id)
                self._notifications.setdefault(key, []).append(dict(payload))
            observer = task.observer
        if observer is not None:
            try:
                observer(f"subagent.task.{status}", dict(payload))
            except Exception as exc:  # observer 失败不得破坏状态
                LOGGER.warning("SubAgent task observer failed: %s", type(exc).__name__)

    def get(
        self,
        task_id: str,
        *,
        owner_id: str,
        session_id: str | None = None,
    ) -> dict[str, Any] | None:
        self._cleanup()
        with self._lock:
            task = self._tasks.get(task_id)
            if (
                task is None
                or task.owner_id != owner_id
                or (session_id is not None and task.session_id != session_id)
            ):
                return None
            return self._snapshot(task).as_dict()

    def list(
        self,
        *,
        owner_id: str,
        session_id: str | None = None,
    ) -> list[dict[str, Any]]:
        self._cleanup()
        with self._lock:
            tasks = [
                task
                for task in self._tasks.values()
                if task.owner_id == owner_id
                and (session_id is None or task.session_id == session_id)
            ]
            tasks.sort(key=lambda item: (item.created_at, item.spec.task_id))
            return [self._snapshot(task).as_dict() for task in tasks]

    def cancel(
        self,
        *,
        owner_id: str,
        session_id: str | None = None,
        task_id: str | None = None,
        batch_id: str | None = None,
    ) -> dict[str, Any]:
        with self._lock:
            ids = (
                [task_id]
                if task_id
                else sorted(self._batch_tasks.get(batch_id or "", set()))
            )
            selected = [self._tasks.get(item) for item in ids]
            selected = [
                task
                for task in selected
                if task is not None
                and task.owner_id == owner_id
                and (session_id is None or task.session_id == session_id)
            ]
            if not selected:
                return {
                    "ok": False,
                    "code": "SUBAGENT_NOT_FOUND",
                    "message": "未找到任务或批次。",
                }
            changed = 0
            active_count = 0
            immediate: list[_Task] = []
            for task in selected:
                if task.status in _TERMINAL:
                    continue
                task.cancel_event.set()
                active_count += 1
                was_queued = task.future is None or bool(task.future.cancel())
                task.updated_at = time.time()
                if was_queued:
                    changed += 1
                    immediate.append(task)
        for task in immediate:
            self._finish(
                task,
                "cancelled",
                error={
                    "code": "SUBAGENT_CANCELLED",
                    "message": "任务已取消。",
                },
            )
        return {
            "ok": True,
            "batch_id": batch_id,
            "task_id": task_id,
            "cancelled_count": changed,
            "status": "cancelled" if changed else ("cancelling" if active_count else "already_terminal"),
        }

    def cancel_all(self, *, owner_id: str, session_id: str | None = None) -> int:
        snapshots = self.list(owner_id=owner_id, session_id=session_id)
        count = 0
        for item in snapshots:
            result = self.cancel(owner_id=owner_id, session_id=session_id, task_id=item["task_id"])
            count += int(result.get("cancelled_count", 0))
        return count

    def drain_notifications(
        self,
        *,
        owner_id: str,
        session_id: str | None = None,
    ) -> list[dict[str, Any]]:
        """消费未过期终态通知；消费后不会再次返回。"""

        self._cleanup()
        with self._lock:
            keys = [
                key
                for key in self._notifications
                if key[0] == owner_id
                and (session_id is None or key[1] == session_id)
            ]
            events: list[dict[str, Any]] = []
            for key in keys:
                events.extend(self._notifications.pop(key))
            return events

    def is_idle(self, *, owner_id: str, session_id: str | None = None) -> bool:
        """判断 owner 范围内是否已无运行中或排队任务。"""

        with self._lock:
            return not self._has_active_locked(owner_id, session_id)

    def wait_for_idle(
        self,
        *,
        owner_id: str,
        session_id: str | None = None,
        timeout: float = 0.0,
    ) -> bool:
        deadline = time.monotonic() + max(0.0, timeout)
        while True:
            if self.is_idle(owner_id=owner_id, session_id=session_id):
                return True
            if time.monotonic() >= deadline:
                return False
            time.sleep(min(0.01, max(0.0, deadline - time.monotonic())))

    def call_when_idle(
        self,
        *,
        owner_id: str,
        session_id: str | None = None,
        callback: Callable[[], None],
    ) -> None:
        """在 owner 范围真实空闲后调用一次，用于延迟释放共享资源。"""

        with self._lock:
            if self._has_active_locked(owner_id, session_id):
                self._idle_callbacks.append((owner_id, session_id, callback))
                return
        callback()

    def close(self, *, owner_id: str, session_id: str | None = None) -> None:
        """关闭指定任务范围；永久关闭时同时回收 TTL 清理线程。"""

        permanent = session_id is None
        if permanent:
            # 先封闭入口，避免 cancel_all 与新 spawn 竞争后遗留未受控任务。
            # Condition 唤醒后会观察 _closed 并立即退出。
            with self._cleanup_condition:
                self._closed = True
                self._cleanup_condition.notify_all()
        self.cancel_all(owner_id=owner_id, session_id=session_id)
        if permanent:
            # shutdown 只在 manager 所属 Agent 永久关闭时调用；已有 worker 已由
            # Coordinator 的有界等待确认退出，或由延迟关闭回调最终收尾。
            self._executor.shutdown(wait=False, cancel_futures=True)
            if threading.current_thread() is not self._cleanup_thread:
                self._cleanup_thread.join(timeout=1)

    def _snapshot(self, task: _Task) -> SubAgentTaskSnapshot:
        return SubAgentTaskSnapshot(
            task.spec.task_id,
            task.spec.batch_id,
            task.owner_id,
            task.session_id,
            task.spec.description[:120],
            task.spec.agent_type[:80],
            task.status,
            task.result,
            task.error,
            task.created_at,
            task.updated_at,
        )

    def _has_active_locked(self, owner_id: str, session_id: str | None) -> bool:
        return any(
            task.owner_id == owner_id
            and (session_id is None or task.session_id == session_id)
            and task.status not in _TERMINAL
            for task in self._tasks.values()
        )

    def _take_idle_callbacks_locked(self) -> list[Callable[[], None]]:
        ready: list[Callable[[], None]] = []
        waiting: list[tuple[str, str | None, Callable[[], None]]] = []
        for owner_id, session_id, callback in self._idle_callbacks:
            if self._has_active_locked(owner_id, session_id):
                waiting.append((owner_id, session_id, callback))
            else:
                ready.append(callback)
        self._idle_callbacks = waiting
        return ready

    def _cleanup(self) -> None:
        """同步清理已过期记录，保留给 list/get/drain 的即时一致性边界。"""

        with self._lock:
            self._cleanup_expired_locked(time.time())

    def _run_cleanup_scheduler(self) -> None:
        """按最早过期时间清理终态任务与通知，直到永久关闭。"""

        while True:
            with self._cleanup_condition:
                if self._closed:
                    return
                wait_seconds = self._seconds_until_next_cleanup_locked(time.time())
                self._cleanup_condition.wait(timeout=wait_seconds)
                if self._closed:
                    return
                self._cleanup_expired_locked(time.time())

    def _seconds_until_next_cleanup_locked(self, now: float) -> float | None:
        """返回下一项终态记录的到期等待时间；没有记录时无限等待。"""

        expiry_times = [
            task.updated_at + self.retention_seconds
            for task in self._tasks.values()
            if task.status in _TERMINAL
        ]
        for events in self._notifications.values():
            expiry_times.extend(
                float(event.get("timestamp", now)) + self.retention_seconds
                for event in events
            )
        if not expiry_times:
            return None
        return max(0.0, min(expiry_times) - now)

    def _cleanup_expired_locked(self, now: float) -> None:
        """在已持有状态锁时移除超过 TTL 的终态任务和未消费通知。"""

        cutoff = now - self.retention_seconds
        expired_task_ids = [
            task_id
            for task_id, task in self._tasks.items()
            if task.status in _TERMINAL and task.updated_at <= cutoff
        ]
        for task_id in expired_task_ids:
            task = self._tasks.pop(task_id)
            ids = self._batch_tasks.get(task.spec.batch_id)
            if ids is not None:
                ids.discard(task_id)
                if not ids:
                    self._batch_tasks.pop(task.spec.batch_id, None)
        for key, events in tuple(self._notifications.items()):
            fresh = [
                event
                for event in events
                if float(event.get("timestamp", now)) > cutoff
            ]
            if fresh:
                self._notifications[key] = fresh
            else:
                self._notifications.pop(key, None)


class _Cancelled(RuntimeError):
    pass


# Event objects are intentionally not exported as model messages.
__all__ = ["SubAgentTaskManager", "SubAgentTaskSpec", "SubAgentTaskSnapshot"]
