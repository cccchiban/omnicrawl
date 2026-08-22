"""全屏工作台的 Agent 回合协议适配。

本模块刻意不依赖 Textual：它只持有 Agent 与取消令牌，并把 Agent 协议事件
转发给调用方。Textual worker、主线程切换、组件渲染和审批模态框仍由
``OmniCrawlApp`` 负责，从而使该控制器可以独立验证而不引入第二套 UI 状态。
"""

from __future__ import annotations

import threading
from contextlib import contextmanager
from dataclasses import dataclass
from typing import Any, Callable, Protocol

from ....llm.stream_registry import close_active_resources, close_active_streams, stream_scope


class StreamAgent(Protocol):
    """全屏回合实际需要的最小 Agent 协议。"""

    def preload_mcp_tools(self) -> None:
        """在首次交互前发现可用 MCP 工具。"""

    def run_stream(
        self,
        user_text: str,
        on_delta: Callable[[str], None],
        **callbacks: Any,
    ) -> str:
        """执行一轮流式 Agent 请求。"""


@dataclass(frozen=True)
class AgentTurnCallbacks:
    """UI 提供的流式事件接收器。

    控制器不改变事件顺序、内容或异常类型；调用方可在每个回调中切回 UI 主线程。
    """

    on_delta: Callable[[str], None]
    on_status: Callable[[str], None]
    on_tool_start: Callable[[int, Any], None]
    on_tool_result: Callable[[Any, Any], None]
    on_token_usage: Callable[[int, int, int], None]
    on_protocol_wait: Callable[[], None]
    on_retry_status: Callable[[str], None]
    on_reasoning_delta: Callable[[str], None]
    on_subagent_event: Callable[[str, dict[str, Any]], None]
    on_todo_update: Callable[[dict[str, Any]], None] | None = None
    on_stream_rollback: Callable[[], None] | None = None


class AgentTurnController:
    """管理单轮 Agent 调用及其协作式取消令牌。

    ``LocalToolAgent`` 在开始请求、工具批次前后都会调用 ``cancel_check``。
    此处只把取消状态转换为既有的 ``KeyboardInterrupt``，以保留上层将用户
    取消、``AgentError`` 和未预期异常分别呈现的既有行为。
    """

    def __init__(self, agent: StreamAgent, cancel_requested: threading.Event) -> None:
        self._agent = agent
        self._cancel_requested = cancel_requested
        self._owner_lock = threading.Lock()
        self._active_owner: object | None = None

    def preload_mcp_tools(self) -> None:
        """执行 MCP 预热，异常原样交给 UI 层分类和展示。"""

        self._agent.preload_mcp_tools()

    @contextmanager
    def scope(self):
        """为一次可取消操作建立唯一资源归属。"""

        owner = object()
        with self._owner_lock:
            self._active_owner = owner
        try:
            with stream_scope(owner):
                yield
        finally:
            with self._owner_lock:
                if self._active_owner is owner:
                    self._active_owner = None

    def run(self, text: str, callbacks: AgentTurnCallbacks) -> str:
        """执行一轮流式请求，并完整转发现有 Agent 回调协议。"""

        with self.scope():
            callback_kwargs: dict[str, Any] = {
                "on_status": callbacks.on_status,
                "on_tool_start": callbacks.on_tool_start,
                "on_tool_result": callbacks.on_tool_result,
                "on_token_usage": callbacks.on_token_usage,
                "on_protocol_wait": callbacks.on_protocol_wait,
                "on_retry_status": callbacks.on_retry_status,
                "cancel_check": self.raise_if_cancelled,
                "on_reasoning_delta": callbacks.on_reasoning_delta,
                "on_subagent_event": callbacks.on_subagent_event,
                "on_stream_rollback": callbacks.on_stream_rollback,
            }
            if callbacks.on_todo_update is not None:
                callback_kwargs["on_todo_update"] = callbacks.on_todo_update
            return self._agent.run_stream(
                text,
                callbacks.on_delta,
                **callback_kwargs,
            )
    def cancel(self) -> int:
        """设置取消信号并主动关闭当前回合的模型流与外部资源。"""

        self._cancel_requested.set()
        with self._owner_lock:
            owner = self._active_owner
        closed = 0
        if owner is not None:
            closed = close_active_streams(owner=owner)
            closed += close_active_resources(owner=owner)
        cancel_active_turn = getattr(self._agent, "cancel_active_turn", None)
        if callable(cancel_active_turn):
            cancel_active_turn()
        return closed

    def raise_if_cancelled(self) -> None:
        """在 Agent 的可中断点将 UI 取消状态转为既有取消异常。"""

        if self._cancel_requested.is_set():
            raise KeyboardInterrupt("用户取消当前任务")



__all__ = ["AgentTurnCallbacks", "AgentTurnController", "StreamAgent"]
