"""工作区后台命令监控。

Monitor 不把后台进程脱离 Agent 管理：所有任务都由当前 Agent 实例持有，
关闭 Agent 或切换工作区时会终止进程树。模型通过任务 ID 轮询增量事件，
因此既能观察长命令输出，也不会因为日志持续到达而自动触发额外模型请求。
"""

from __future__ import annotations

import os
import subprocess
import threading
import time
import uuid
from collections import deque
from dataclasses import dataclass, field
from typing import Any, Deque, TextIO

from ..llm.stream_registry import current_stream_scope, register_resource, unregister_resource
from .process_control import (
    assign_process_to_kill_on_close_job as _assign_process_to_kill_on_close_job,
    close_windows_handle as _close_windows_handle,
)
from .tools import (
    WorkspaceCommandResult,
    WorkspaceToolError,
    WorkspaceTools,
)


MAX_ACTIVE_MONITORS = 20
MAX_BUFFERED_EVENTS = 1_000
MAX_EVENT_CHARS = 4_000
MAX_EVENTS_PER_POLL = 200


class WorkspaceMonitorError(WorkspaceToolError):
    """后台命令监控参数或进程管理失败。"""


@dataclass(frozen=True)
class MonitorEvent:
    """后台命令的一条可轮询事件。"""

    sequence: int
    created_at: float
    stream: str
    text: str


@dataclass(frozen=True)
class MonitorTaskSnapshot:
    """供 UI 和 API 展示的后台任务状态快照。"""

    monitor_id: str
    command: str
    shell: str
    status: str
    exit_code: int | None
    started_at: float
    next_cursor: int
    dropped_events: int


@dataclass(frozen=True)
class MonitorPollResult:
    """按游标读取后台任务增量事件的结构化结果。"""

    snapshot: MonitorTaskSnapshot
    events: tuple[MonitorEvent, ...]
    next_cursor: int
    first_available_cursor: int


@dataclass
class ManagedMonitor:
    """当前 Agent 管理的一个后台进程及其有限日志缓冲。"""

    monitor_id: str
    command: str
    shell: str
    process: subprocess.Popen[str]
    job_handle: int | None = None
    started_at: float = field(default_factory=time.time)
    status: str = "running"
    exit_code: int | None = None
    stop_requested: bool = False
    # 有限环形缓冲：满了之后从头部淘汰，使用 deque 让逐条淘汰保持 O(1)，
    # 避免 list.pop(0) 在高频输出（构建日志/服务器等）下反复整体移位。
    events: Deque[MonitorEvent] = field(default_factory=deque)
    next_sequence: int = 1
    dropped_events: int = 0
    reader_threads: list[threading.Thread] = field(default_factory=list)
    waiter_thread: threading.Thread | None = None
    resource_owner: object | None = None


class BackgroundMonitorManager:
    """启动、轮询和终止当前工作区的后台命令。

    进程输出由独立读取线程持续收集到内存中的有限环形缓冲。`poll` 返回从
    指定游标之后的新事件和下一游标；调用者不需要读取日志文件，也不会重复
    获得已经消费过的输出。这里不写持久化日志，避免将用户命令输出长期保存到
    项目目录；会话转录仍会记录 Agent 实际调用 Monitor 时看到的工具结果。
    """

    def __init__(self, workspace_tools: WorkspaceTools) -> None:
        self._workspace_tools = workspace_tools
        self._lock = threading.RLock()
        self._condition = threading.Condition(self._lock)
        self._monitors: dict[str, ManagedMonitor] = {}
        self._pending_starts = 0
        self._closed = False

    def run(self, arguments: dict[str, Any]) -> WorkspaceCommandResult:
        """执行 Monitor 工具动作：start、poll、stop 或 list。"""

        action = str(arguments.get("action") or "start").strip().lower()
        action_aliases = {
            "status": "poll",
            "log": "poll",
            "logs": "poll",
            "read": "poll",
        }
        action = action_aliases.get(action, action)

        if action == "start":
            return self._start(arguments)
        if action == "poll":
            return self._poll(arguments)
        if action == "stop":
            return self._stop(arguments)
        if action == "list":
            return self._list()
        raise WorkspaceMonitorError("action 仅支持 start、poll、stop 或 list。")

    def close(self) -> None:
        """终止所有仍在运行的后台进程，避免 Agent 退出后留下孤儿进程。"""

        with self._lock:
            if self._closed:
                return
            self._closed = True
            running_ids = [
                monitor_id
                for monitor_id, task in self._monitors.items()
                if task.status == "running"
            ]

        for monitor_id in running_ids:
            try:
                self._stop_task(monitor_id, reason="Agent 已关闭，已停止后台任务。")
            except WorkspaceMonitorError:
                # 关闭路径优先尽力回收其他任务；单个进程已经退出或清理失败不应
                # 阻止 Agent 继续释放其余资源。
                continue

    def list_snapshots(self) -> list[MonitorTaskSnapshot]:
        """列出全部后台任务，供本地 TUI 和 HTTP API 展示。"""

        with self._lock:
            return [self._snapshot_locked(task) for task in self._monitors.values()]

    def get_snapshot(self, monitor_id: str) -> MonitorTaskSnapshot:
        """读取一个后台任务的当前状态。"""

        with self._lock:
            return self._snapshot_locked(self._task_locked(monitor_id))

    def poll_events(
        self,
        monitor_id: str,
        *,
        cursor: int = 0,
        max_events: int = 100,
    ) -> MonitorPollResult:
        """返回游标之后的有限事件批次，不改变后台任务状态。"""

        normalized_cursor = max(0, cursor)
        normalized_max_events = max(1, min(max_events, MAX_EVENTS_PER_POLL))
        with self._lock:
            task = self._task_locked(monitor_id)
            latest_sequence = task.next_sequence - 1
            effective_cursor = min(normalized_cursor, latest_sequence)
            first_sequence = task.events[0].sequence if task.events else task.next_sequence
            events = tuple(
                event for event in task.events if event.sequence > effective_cursor
            )[:normalized_max_events]
            next_cursor = events[-1].sequence if events else effective_cursor
            return MonitorPollResult(
                snapshot=self._snapshot_locked(task),
                events=events,
                next_cursor=next_cursor,
                first_available_cursor=max(0, first_sequence - 1),
            )

    def wait_for_events(self, monitor_id: str, cursor: int, timeout: float) -> None:
        """等待新日志或任务结束，用于 API SSE 的低开销长轮询。"""

        with self._condition:
            task = self._task_locked(monitor_id)
            if task.next_sequence - 1 <= max(0, cursor) and task.status == "running":
                self._condition.wait(timeout=max(0.0, timeout))

    def _start(self, arguments: dict[str, Any]) -> WorkspaceCommandResult:
        command = str(arguments.get("command") or "").strip()
        if not command:
            raise WorkspaceMonitorError("启动监控时 command 不能为空。")

        shell = str(arguments.get("shell") or "powershell").strip().lower()
        if shell not in {"bash", "powershell"}:
            raise WorkspaceMonitorError("shell 仅支持 bash 或 powershell。")

        with self._lock:
            if self._closed:
                raise WorkspaceMonitorError("Agent 已关闭，不能启动新的后台任务。")
            active_count = sum(task.status == "running" for task in self._monitors.values())
            if active_count + self._pending_starts >= MAX_ACTIVE_MONITORS:
                raise WorkspaceMonitorError(
                    f"同时运行的后台任务最多为 {MAX_ACTIVE_MONITORS} 个，请先停止不需要的任务。"
                )
            # 在启动进程前预留名额，避免并发 start 同时通过容量检查。
            self._pending_starts += 1

        try:
            invocation = self._workspace_tools.command_invocation(command, shell=shell)
        except WorkspaceToolError as exc:
            with self._lock:
                self._pending_starts -= 1
            raise WorkspaceMonitorError(str(exc)) from exc

        popen_kwargs: dict[str, Any] = {
            "cwd": str(self._workspace_tools.workspace_root),
            "shell": False,
            "stdout": subprocess.PIPE,
            "stderr": subprocess.PIPE,
            "text": True,
            "encoding": "utf-8",
            "errors": "replace",
            "bufsize": 1,
        }
        if os.name == "nt":
            popen_kwargs["creationflags"] = subprocess.CREATE_NEW_PROCESS_GROUP
        else:
            popen_kwargs["start_new_session"] = True

        try:
            process = subprocess.Popen(invocation.args, **popen_kwargs)
        except OSError as exc:
            with self._lock:
                self._pending_starts -= 1
            raise WorkspaceMonitorError(f"后台命令启动失败：{exc}") from exc

        job_handle = _assign_process_to_kill_on_close_job(process)

        monitor_id = f"monitor-{uuid.uuid4().hex[:12]}"
        task = ManagedMonitor(
            monitor_id=monitor_id,
            command=command,
            shell=shell,
            process=process,
            job_handle=job_handle,
            resource_owner=current_stream_scope(),
        )
        with self._lock:
            self._pending_starts -= 1
            if self._closed:
                # 进程在 close 与 Popen 之间启动时必须立即回收，不能变成孤儿。
                self._terminate_process_tree(process, job_handle=job_handle, wait=True)
                raise WorkspaceMonitorError("Agent 已关闭，后台任务已终止。")
            self._monitors[monitor_id] = task
            self._record_event_locked(task, "system", f"已启动，shell={shell}。")
            if task.resource_owner is not None:
                register_resource(
                    process,
                    owner=task.resource_owner,
                    close_callback=lambda task=task: self._stop_task(
                        task.monitor_id,
                        reason="当前回合已取消，后台任务已强制终止。",
                        wait=False,
                    ),
                )

        self._start_readers(task)
        waiter = threading.Thread(
            target=self._wait_for_exit,
            args=(task,),
            name=f"agent-monitor-wait-{monitor_id}",
            daemon=True,
        )
        task.waiter_thread = waiter
        waiter.start()

        return WorkspaceCommandResult(
            ok=True,
            output=(
                f"已启动后台任务：{monitor_id}\n"
                f"Shell：{shell}\n"
                "状态：running\n"
                "下一游标：0\n"
                f"使用 monitor action=poll、monitor_id={monitor_id}、cursor=0 读取增量日志；"
                "使用 action=stop 停止任务。"
            ),
        )

    def _poll(self, arguments: dict[str, Any]) -> WorkspaceCommandResult:
        task = self._task_from_arguments(arguments)
        cursor = _read_limited_int(arguments, "cursor", default=0, minimum=0, maximum=2_000_000_000)
        max_events = _read_limited_int(
            arguments,
            "max_events",
            default=100,
            minimum=1,
            maximum=MAX_EVENTS_PER_POLL,
        )
        result = self.poll_events(task.monitor_id, cursor=cursor, max_events=max_events)
        snapshot = result.snapshot

        lines = [
            f"后台任务：{snapshot.monitor_id}",
            f"状态：{snapshot.status}",
            f"下一游标：{result.next_cursor}",
        ]
        if snapshot.exit_code is not None:
            lines.append(f"退出码：{snapshot.exit_code}")
        if cursor < result.first_available_cursor and snapshot.dropped_events:
            lines.append(f"提示：早期 {snapshot.dropped_events} 条事件已从内存缓冲清理。")
        if result.events:
            lines.append("事件：")
            lines.extend(_format_event(event) for event in result.events)
        else:
            lines.append("事件：暂无新增输出。")

        return WorkspaceCommandResult(
            ok=snapshot.status != "failed",
            output="\n".join(lines),
        )

    def _stop(self, arguments: dict[str, Any]) -> WorkspaceCommandResult:
        task = self._task_from_arguments(arguments)
        self._stop_task(task.monitor_id, reason="收到停止请求，正在终止后台任务。")
        return self._poll({"monitor_id": task.monitor_id, "cursor": 0, "max_events": 20})

    def _list(self) -> WorkspaceCommandResult:
        rows = self.list_snapshots()

        if not rows:
            return WorkspaceCommandResult(ok=True, output="当前没有后台任务。")

        lines = ["后台任务："]
        for snapshot in rows:
            suffix = "" if snapshot.exit_code is None else f"，退出码：{snapshot.exit_code}"
            lines.append(
                f"- {snapshot.monitor_id}：{snapshot.status}，shell={snapshot.shell}，"
                f"当前游标：{snapshot.next_cursor}{suffix}"
            )
        return WorkspaceCommandResult(ok=True, output="\n".join(lines))

    def _task_from_arguments(self, arguments: dict[str, Any]) -> ManagedMonitor:
        monitor_id = str(arguments.get("monitor_id") or "").strip()
        if not monitor_id:
            raise WorkspaceMonitorError("monitor_id 不能为空。")
        with self._lock:
            return self._task_locked(monitor_id)

    def _task_locked(self, monitor_id: str) -> ManagedMonitor:
        task = self._monitors.get(monitor_id)
        if task is None:
            raise WorkspaceMonitorError(f"未找到后台任务：{monitor_id}")
        return task

    @staticmethod
    def _snapshot_locked(task: ManagedMonitor) -> MonitorTaskSnapshot:
        return MonitorTaskSnapshot(
            monitor_id=task.monitor_id,
            command=task.command,
            shell=task.shell,
            status=task.status,
            exit_code=task.exit_code,
            started_at=task.started_at,
            next_cursor=task.next_sequence - 1,
            dropped_events=task.dropped_events,
        )

    def _start_readers(self, task: ManagedMonitor) -> None:
        streams: tuple[tuple[str, TextIO | None], ...] = (
            ("stdout", task.process.stdout),
            ("stderr", task.process.stderr),
        )
        for stream_name, stream in streams:
            if stream is None:
                continue
            reader = threading.Thread(
                target=self._read_stream,
                args=(task, stream_name, stream),
                name=f"agent-monitor-{stream_name}-{task.monitor_id}",
                daemon=True,
            )
            task.reader_threads.append(reader)
            reader.start()

    def _read_stream(self, task: ManagedMonitor, stream_name: str, stream: TextIO) -> None:
        try:
            for raw_line in iter(stream.readline, ""):
                line = raw_line.rstrip("\r\n")
                if len(line) > MAX_EVENT_CHARS:
                    for offset in range(0, len(line), MAX_EVENT_CHARS):
                        self._record_event(
                            task,
                            stream_name,
                            line[offset : offset + MAX_EVENT_CHARS],
                        )
                    continue
                self._record_event(task, stream_name, line)

        finally:
            try:
                stream.close()
            except OSError:
                pass

    def _wait_for_exit(self, task: ManagedMonitor) -> None:
        exit_code = task.process.wait()
        for reader in task.reader_threads:
            reader.join(timeout=5)

        with self._lock:
            task.exit_code = exit_code
            if task.stop_requested:
                task.status = "stopped"
                message = f"任务已停止，退出码：{exit_code}。"
            elif exit_code == 0:
                task.status = "completed"
                message = "任务已完成，退出码：0。"
            else:
                task.status = "failed"
                message = f"任务失败，退出码：{exit_code}。"
            self._record_event_locked(task, "system", message)
            job_handle = task.job_handle
            task.job_handle = None
        _close_windows_handle(job_handle)

    def _stop_task(self, monitor_id: str, *, reason: str, wait: bool = True) -> None:
        with self._lock:
            task = self._monitors.get(monitor_id)
            if task is None:
                raise WorkspaceMonitorError(f"未找到后台任务：{monitor_id}")
            if task.status != "running":
                return
            task.stop_requested = True
            self._record_event_locked(task, "system", reason)
            process = task.process
            job_handle = task.job_handle
            task.job_handle = None

        self._terminate_process_tree(process, job_handle=job_handle, wait=wait)
        if not wait:
            unregister_resource(process)
            return
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            raise WorkspaceMonitorError(f"后台任务未能在 5 秒内停止：{monitor_id}")

        waiter = task.waiter_thread
        if waiter is not None:
            waiter.join(timeout=5)
        unregister_resource(process)

    @staticmethod
    def _terminate_process_tree(
        process: subprocess.Popen[str],
        *,
        job_handle: int | None,
        wait: bool = True,
    ) -> None:
        from .process_control import terminate_process_tree

        terminate_process_tree(process, job_handle=job_handle, wait=wait)

    def _record_event(self, task: ManagedMonitor, stream: str, text: str) -> None:
        with self._lock:
            self._record_event_locked(task, stream, text)

    def _record_event_locked(self, task: ManagedMonitor, stream: str, text: str) -> None:
        event = MonitorEvent(
            sequence=task.next_sequence,
            created_at=time.time(),
            stream=stream,
            text=text,
        )
        task.next_sequence += 1
        task.events.append(event)
        if len(task.events) > MAX_BUFFERED_EVENTS:
            task.events.popleft()
            task.dropped_events += 1
        self._condition.notify_all()


def _read_limited_int(
    arguments: dict[str, Any],
    key: str,
    *,
    default: int,
    minimum: int,
    maximum: int,
) -> int:
    value = arguments.get(key, default)
    if isinstance(value, bool):
        return default
    try:
        parsed = int(value)
    except (TypeError, ValueError):
        return default
    return max(minimum, min(maximum, parsed))


def _format_event(event: MonitorEvent) -> str:
    timestamp = time.strftime("%H:%M:%S", time.localtime(event.created_at))
    text = event.text or "(空行)"
    return f"{event.sequence} [{timestamp}] {event.stream}: {text}"
