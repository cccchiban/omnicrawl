"""SubAgent 专用的串行人工审批与风险策略。

该模块只负责“是否把一次已通过 Host guard 的子工具调用交给人工确认”以及
“多个子任务同时请求确认时如何串行化”。它不执行工具、不保存 prompt，也不
创建 API 路由；工具执行仍回到 ``LocalToolAgent`` 的既有审批和 Hook 链路。

当前 read-only 子任务会把受限 MCP/桌面/外部工具交给 Broker；删除与 Git 变更
仍沿用专门风险类别。普通读取和通过只读命令策略的 Shell/Monitor 不重复确认。
"""

from __future__ import annotations

import logging
import threading
import time
import uuid
from collections import deque
from collections.abc import Callable, Iterator, Mapping
from contextlib import contextmanager
from contextvars import ContextVar
from dataclasses import dataclass, field
from typing import Any

from ..approval_policy import is_delete_behavior_tool_call, is_git_mutation_tool_call
from ..types import ToolDefinition
from ...state.session_artifacts import redact_sensitive_text, redact_sensitive_values


LOGGER = logging.getLogger(__name__)


@dataclass(frozen=True)
class SubAgentApprovalOrigin:
    """一项子工具确认的可信 Host 来源，不含模型 prompt。"""

    batch_id: str
    task_id: str
    agent_label: str
    description: str
    permission_mode: str = "delegated-read-only"

    def as_public_dict(self) -> dict[str, str]:
        """生成 UI、Session 和 SSE 可使用的有界来源投影。"""

        return {
            "batch_id": redact_sensitive_text(self.batch_id)[:80],
            "task_id": redact_sensitive_text(self.task_id)[:80],
            "agent_label": redact_sensitive_text(self.agent_label)[:80],
            "description": redact_sensitive_text(self.description)[:120],
            "permission_mode": redact_sensitive_text(self.permission_mode)[:40],
        }


@dataclass(frozen=True)
class SubAgentApprovalRequest:
    """交给确认处理器的单次风险操作请求。"""

    request_id: str
    origin: SubAgentApprovalOrigin
    tool_name: str
    public_arguments: Mapping[str, Any]
    risk_summary: str
    created_at: float = field(default_factory=time.time)

    def as_event_payload(self, *, status: str) -> dict[str, Any]:
        """构建不含原始参数、prompt 或异常文本的公开生命周期事件。"""

        return {
            **self.origin.as_public_dict(),
            "request_id": self.request_id,
            "tool": redact_sensitive_text(self.tool_name)[:120],
            "arguments": redact_sensitive_values(dict(self.public_arguments)),
            "risk_summary": redact_sensitive_text(self.risk_summary)[:120],
            "status": status,
        }


@dataclass(frozen=True)
class SubAgentApprovalScope:
    """绑定到单个子任务执行线程的 Broker 与可信来源。"""

    broker: "ApprovalBroker"
    origin: SubAgentApprovalOrigin


_ACTIVE_SUBAGENT_APPROVAL_SCOPE: ContextVar[SubAgentApprovalScope | None] = ContextVar(
    "omnicrawl_subagent_approval_scope",
    default=None,
)


@contextmanager
def activate_subagent_approval_scope(
    scope: SubAgentApprovalScope,
) -> Iterator[None]:
    """在一个子任务 worker 内绑定审批来源，避免并发任务共享可变字段。"""

    token = _ACTIVE_SUBAGENT_APPROVAL_SCOPE.set(scope)
    try:
        yield
    finally:
        _ACTIVE_SUBAGENT_APPROVAL_SCOPE.reset(token)


def current_subagent_approval_scope() -> SubAgentApprovalScope | None:
    """返回当前子任务线程的审批来源；父 Agent 和普通工具调用恒为 ``None``。"""

    return _ACTIVE_SUBAGENT_APPROVAL_SCOPE.get()


def subagent_approval_risk_summary(
    tool: ToolDefinition,
    arguments: dict[str, Any],
    *,
    permission_mode: str = "delegated-read-only",
) -> str:
    """返回需要人工确认的子工具风险类别；空字符串表示无需确认。

    基线策略（read_only / verify）：
    - 删除意图、Git 变更操作逐次确认
    - read-only 继承的受限 MCP、桌面和其他外部工具保留 Host 确认
    - 普通读取、固定验证和通过只读策略的命令不重复确认

    standard 写 Agent 额外策略：
    - write_file / replace_text 一律确认
    - bash / powershell 默认确认（避免静默执行变更命令）
    - 未知 Git 子命令仍按变更性处理
    """

    if is_delete_behavior_tool_call(tool, arguments):
        return "删除操作"
    if is_git_mutation_tool_call(tool, arguments):
        return "Git 变更操作"

    mode = str(permission_mode or "delegated-read-only").strip().casefold()
    if mode == "standard":
        name = str(getattr(tool, "name", "") or "").strip()
        if name in {"write_file", "replace_text"}:
            return "工作区写入"
        if name in {"bash", "powershell"}:
            return "命令执行"
    name = str(getattr(tool, "name", "") or "").strip()
    if (
        mode == "delegated-read-only"
        and tool.requires_confirmation
        and name not in {"write_file", "replace_text", "memory_write", "subagent"}
    ):
        return "受限外部操作"
    return ""


@dataclass
class _QueuedApproval:
    request: SubAgentApprovalRequest
    completed: threading.Event = field(default_factory=threading.Event)
    decision: bool = False
    cancelled: bool = False


class ApprovalBroker:
    """对子任务的人工确认进行 FIFO 串行仲裁。

    确认处理器可能是终端输入、Textual 模态框或 API 的同步等待器。Broker 不假设
    它可被强制中断：取消活跃请求时会立即让子任务得到拒绝结果，但仍保留该活跃
    槽位直到旧处理器返回，从而绝不同时展示第二个确认。处理器迟到返回的批准会
    因 ``cancelled`` 标记被忽略。
    """

    def __init__(
        self,
        *,
        approve: Callable[[SubAgentApprovalRequest], bool],
        event_sink: Callable[[str, dict[str, Any]], None] | None = None,
    ) -> None:
        self._approve = approve
        self._event_sink = event_sink or (lambda _event, _payload: None)
        self._condition = threading.Condition(threading.RLock())
        self._queue: deque[_QueuedApproval] = deque()
        self._active: _QueuedApproval | None = None
        self._closed = False

    @property
    def pending_count(self) -> int:
        """返回仍占用队列或确认槽位的请求数，仅供诊断和测试。"""

        with self._condition:
            return len(self._queue)

    def request(
        self,
        *,
        origin: SubAgentApprovalOrigin,
        tool_name: str,
        public_arguments: Mapping[str, Any],
        risk_summary: str,
        cancel_check: Callable[[], None] | None,
    ) -> bool:
        """排队一项已判定为高风险的调用，并等待单次人工决策。

        ``cancel_check`` 抛出的控制流异常原样向上传递，避免子任务把父取消误报
        为普通的工具拒绝。外部 ``cancel_task`` 则以 ``False`` 结束等待，随后
        子任务原有取消令牌会在下一边界继续终止执行。
        """

        request = SubAgentApprovalRequest(
            request_id=f"approval-{uuid.uuid4().hex[:12]}",
            origin=origin,
            tool_name=tool_name,
            public_arguments=redact_sensitive_values(dict(public_arguments)),
            risk_summary=risk_summary,
        )
        queued = _QueuedApproval(request=request)
        with self._condition:
            if self._closed:
                return False
            self._queue.append(queued)
            self._condition.notify_all()
        self._activate_next()

        while not queued.completed.wait(timeout=0.05):
            if cancel_check is None:
                continue
            try:
                cancel_check()
            except BaseException:
                self.cancel_request(request.request_id)
                raise
        return queued.decision is True

    def cancel_request(self, request_id: str) -> None:
        """拒绝一项待决请求；活跃处理器的迟到结果不会恢复它。"""

        self._cancel_matching(lambda item: item.request.request_id == request_id)

    def cancel_task(self, task_id: str) -> None:
        """拒绝指定任务的所有待决审批。"""

        self._cancel_matching(lambda item: item.request.origin.task_id == task_id)

    def cancel_batch(self, batch_id: str) -> None:
        """拒绝指定批次的所有待决审批。"""

        self._cancel_matching(lambda item: item.request.origin.batch_id == batch_id)

    def cancel_all(self) -> None:
        """拒绝所有待决审批，供父取消、关闭和工作区切换调用。"""

        self._cancel_matching(lambda _item: True)

    def close(self) -> None:
        """关闭 Broker 并拒绝后续请求；已显示的确认只能安全地晚到失效。"""

        with self._condition:
            self._closed = True
        self.cancel_all()

    def _cancel_matching(self, predicate: Callable[[_QueuedApproval], bool]) -> None:
        cancelled_requests: list[SubAgentApprovalRequest] = []
        with self._condition:
            for queued in tuple(self._queue):
                if queued.cancelled or not predicate(queued):
                    continue
                queued.cancelled = True
                queued.decision = False
                queued.completed.set()
                cancelled_requests.append(queued.request)
                # 正在展示的确认不能从队首移除；其处理器返回前不得展示下一项。
                if queued is not self._active:
                    self._queue.remove(queued)
            self._condition.notify_all()
        # 远程后台审批需要在 Broker 取消时立刻解除自身的等待，而不是等到
        # confirmation timeout。公开事件不含 prompt，可同时供 TUI/API 收敛陈旧状态。
        for request in cancelled_requests:
            self._emit(
                "subagent.task.approval_cancelled",
                request,
                status="cancelled",
            )
        self._activate_next()

    def _activate_next(self) -> None:
        """如当前没有展示中的确认，则按 FIFO 启动下一项处理器。"""

        with self._condition:
            if self._closed or self._active is not None:
                return
            while self._queue and self._queue[0].cancelled:
                skipped = self._queue.popleft()
                skipped.decision = False
                skipped.completed.set()
            if not self._queue:
                self._condition.notify_all()
                return
            active = self._queue[0]
            self._active = active

        self._emit("subagent.task.waiting_approval", active.request, status="waiting_approval")
        threading.Thread(
            target=self._run_confirmation_handler,
            args=(active,),
            name=f"omnicrawl-subagent-approval-{active.request.request_id[-6:]}",
            daemon=True,
        ).start()

    def _run_confirmation_handler(self, queued: _QueuedApproval) -> None:
        try:
            approved = bool(self._approve(queued.request))
        except Exception as exc:  # noqa: BLE001 - UI/API 处理器异常按拒绝收敛
            LOGGER.warning("SubAgent approval handler failed: %s", type(exc).__name__)
            approved = False

        emit_running = False
        with self._condition:
            if self._active is not queued:
                # 理论上不会发生；防御性拒绝，避免未知并发错误误批准工具。
                return
            if self._queue and self._queue[0] is queued:
                self._queue.popleft()
            else:
                try:
                    self._queue.remove(queued)
                except ValueError:
                    pass
            self._active = None
            queued.decision = approved and not queued.cancelled
            queued.completed.set()
            emit_running = not queued.cancelled
            self._condition.notify_all()

        if emit_running:
            self._emit("subagent.task.running", queued.request, status="running")
        self._activate_next()

    def _emit(
        self,
        event_name: str,
        request: SubAgentApprovalRequest,
        *,
        status: str,
    ) -> None:
        try:
            self._event_sink(event_name, request.as_event_payload(status=status))
        except Exception as exc:  # noqa: BLE001 - 观察者失败不能破坏审批状态机
            LOGGER.warning("SubAgent approval event sink failed: %s", type(exc).__name__)


__all__ = [
    "ApprovalBroker",
    "SubAgentApprovalOrigin",
    "SubAgentApprovalRequest",
    "SubAgentApprovalScope",
    "activate_subagent_approval_scope",
    "current_subagent_approval_scope",
    "subagent_approval_risk_summary",
]
