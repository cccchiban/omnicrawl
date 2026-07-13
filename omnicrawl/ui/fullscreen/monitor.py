"""全屏工作台的 Monitor 状态适配。

本模块持有仅属于全屏 UI 的日志消费 cursor 与工作区切换暂停状态。它不依赖
Textual、不启动定时器，也不渲染组件；``OmniCrawlApp`` 仍负责调度刷新和展示
返回的结构化批次。这样 API、模型工具和其他消费者能够继续使用各自独立的
Monitor cursor。
"""

from __future__ import annotations

from dataclasses import dataclass
from types import MappingProxyType
from typing import Mapping, Protocol

from ...agent import AgentError
from ...workspace.monitor import MonitorEvent, MonitorPollResult, MonitorTaskSnapshot


class MonitorAgent(Protocol):
    """全屏 Monitor 刷新依赖的最小只读 Agent 协议。"""

    def list_monitor_tasks(self) -> list[MonitorTaskSnapshot]:
        """返回当前工作区受管后台任务的快照列表。"""

    def poll_monitor_events(
        self,
        monitor_id: str,
        *,
        cursor: int = 0,
        max_events: int = 100,
    ) -> MonitorPollResult:
        """从指定 cursor 读取一项任务的增量日志。"""


@dataclass(frozen=True)
class MonitorDisplayBatch:
    """一次刷新中可由 UI 直接追加的单个 Monitor 事件批次。"""

    monitor_id: str
    status: str
    events: tuple[MonitorEvent, ...]


class MonitorStateAdapter:
    """维护全屏 UI 独有的 Monitor 消费状态。

    Agent 的 Monitor 管理器负责进程、日志缓冲与事件序号；这里仅保存本全屏
    界面已消费到的位置。任务列表读取失败或单任务轮询失败时均静默跳过当前
    刷新，沿用此前 UI 不干扰模型回合和其他后台任务的降级策略。
    """

    def __init__(self, agent: MonitorAgent, *, max_events: int = 50) -> None:
        self._agent = agent
        self._max_events = max(1, int(max_events))
        self._cursors: dict[str, int] = {}
        self._polling_suspended = False

    @property
    def can_schedule_refresh(self) -> bool:
        """是否保留当前 UI 的定时轮询注册条件。"""

        return callable(getattr(self._agent, "list_monitor_tasks", None))

    @property
    def cursors(self) -> Mapping[str, int]:
        """返回 cursor 快照，避免 UI 外部修改适配器的长期状态。"""

        return MappingProxyType(self._cursors)

    @property
    def polling_suspended(self) -> bool:
        """当前是否因工作区切换暂时禁止调用 Agent Monitor 接口。"""

        return self._polling_suspended

    def suspend_for_workspace_switch(self) -> None:
        """在切换 I/O 开始前暂停轮询并废弃旧工作区 cursor。

        必须在后台工作区切换成功之前清空 cursor：新工作区可能复用任务 ID，
        若沿用旧位置会跳过新工作区的首批事件。
        """

        self._polling_suspended = True
        self._cursors.clear()

    def resume_polling(self) -> None:
        """在工作区切换 worker 结束后恢复后续定时刷新。"""

        self._polling_suspended = False

    def refresh(self) -> tuple[MonitorDisplayBatch, ...]:
        """读取当前各任务的新事件，并返回可由 UI 渲染的结构化批次。

        成功 poll 即使没有事件也会写入 ``next_cursor``，以遵守 Agent 返回的
        消费位置。单个任务的 ``AgentError`` 不会推进其 cursor，也不阻断同轮
        其他任务；下一次刷新会从原 cursor 重试。
        """

        if self._polling_suspended:
            return ()

        list_tasks = getattr(self._agent, "list_monitor_tasks", None)
        poll_events = getattr(self._agent, "poll_monitor_events", None)
        if not callable(list_tasks) or not callable(poll_events):
            return ()

        try:
            tasks = list_tasks()
        except AgentError:
            return ()

        batches: list[MonitorDisplayBatch] = []
        for task in tasks:
            monitor_id = str(getattr(task, "monitor_id", ""))
            if not monitor_id:
                continue
            cursor = self._cursors.get(monitor_id, 0)
            try:
                result = poll_events(
                    monitor_id,
                    cursor=cursor,
                    max_events=self._max_events,
                )
            except AgentError:
                continue

            self._cursors[monitor_id] = result.next_cursor
            if result.events:
                batches.append(
                    MonitorDisplayBatch(
                        monitor_id=monitor_id,
                        status=result.snapshot.status,
                        events=result.events,
                    )
                )
        return tuple(batches)


def format_monitor_display_batch(batch: MonitorDisplayBatch) -> str:
    """保留全屏对话区既有的 Monitor 工具消息文案。"""

    lines = [
        f"Monitor · {batch.monitor_id} · {batch.status}",
        *[
            f"[{event.stream}] {event.text or '(空行)'}"
            for event in batch.events
        ],
    ]
    return "\n".join(lines)


__all__ = [
    "MonitorAgent",
    "MonitorDisplayBatch",
    "MonitorStateAdapter",
    "format_monitor_display_batch",
]
