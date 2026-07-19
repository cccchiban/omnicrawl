"""Agent 模型循环的内部执行器。

本模块只负责“模型回复 -> 整批工具观察 -> 下一次模型请求”的协议循环。
用户输入、Session、Plugin、Runtime、Skill、审批以及具体工具调度仍由调用方持有，
避免 Runner 与 ``LocalToolAgent`` 的进程级状态耦合。
"""

from __future__ import annotations

import time
from dataclasses import dataclass, field
from typing import Any, Callable, Sequence

from .types import AgentModelReply, ToolCall, ToolResult


class AgentLoopBudgetExceeded(RuntimeError):
    """Agent Loop 超过模型回合、工具调用或调用边界时间预算。"""


@dataclass(frozen=True)
class AgentLoopLimits:
    """可选循环预算；``None`` 表示该维度不设上限。

    主 Agent 使用默认值以保持既有无限工具循环语义；SubAgent 可传入有界预算。
    时间预算在模型请求和工具批次边界检查，并使用单调时钟；它能限制继续启动新步骤，
    但不能强制中断一个已阻塞且不响应取消的第三方 SDK 或系统调用。
    """

    max_model_turns: int | None = None
    max_tool_calls: int | None = None
    timeout_seconds: float | None = None

    def __post_init__(self) -> None:
        for name, value in (
            ("max_model_turns", self.max_model_turns),
            ("max_tool_calls", self.max_tool_calls),
        ):
            if value is not None and (isinstance(value, bool) or not isinstance(value, int) or value < 1):
                raise ValueError(f"{name} 必须是正整数或 None。")
        if self.timeout_seconds is not None and (
            isinstance(self.timeout_seconds, bool)
            or not isinstance(self.timeout_seconds, (int, float))
            or self.timeout_seconds <= 0
        ):
            raise ValueError("timeout_seconds 必须是正数或 None。")


@dataclass(frozen=True)
class AgentLoopObservation:
    """一个已执行工具调用及其回填给模型的观察消息。

    ``followup_messages`` 用于工具结果之后的补充观察（例如视觉截图）。Runner 会先
    回填同一批次的全部 tool 消息，再追加这些 user 消息，保持工具协议要求的顺序。
    """

    tool_call: ToolCall
    result: ToolResult
    message: dict[str, Any]
    followup_messages: tuple[dict[str, Any], ...] = ()


@dataclass(frozen=True)
class AgentLoopResult:
    """循环正常完成后的内部结果。"""

    final_text: str
    reasoning: str
    content_streamed: bool
    model_turns: int
    tool_calls: int
    messages: list[dict[str, Any]] = field(default_factory=list)


class AgentLoopRunner:
    """执行独立且可取消、可设预算的 Agent 模型循环。

    ``execute_tool_batch`` 是刻意保留的批次边界：同一次模型回复中的全部调用
    必须一次性交给 Host。Host 可先完成整批规范化与审批，再按写入/删除屏障调度，
    最后以模型调用顺序返回观察。Runner 不提供逐工具 execute callback，防止调用方
    在审批尚未完成时提前执行同批中的某个工具。
    """

    def run(
        self,
        *,
        messages: list[dict[str, Any]],
        request_reply: Callable[[list[dict[str, Any]]], AgentModelReply],
        execute_tool_batch: Callable[
            [Sequence[ToolCall], int],
            Sequence[AgentLoopObservation],
        ],
        limits: AgentLoopLimits | None = None,
        cancel_check: Callable[[], None] | None = None,
    ) -> AgentLoopResult:
        active_limits = limits or AgentLoopLimits()
        started_at = time.monotonic()
        model_turns = 0
        tool_calls = 0
        reasoning_parts: list[str] = []

        def check_boundary() -> None:
            if cancel_check is not None:
                cancel_check()
            if (
                active_limits.timeout_seconds is not None
                and time.monotonic() - started_at >= active_limits.timeout_seconds
            ):
                raise AgentLoopBudgetExceeded(
                    f"Agent Loop 已超过时间预算 {active_limits.timeout_seconds:g} 秒。"
                )

        while True:
            check_boundary()
            if (
                active_limits.max_model_turns is not None
                and model_turns >= active_limits.max_model_turns
            ):
                raise AgentLoopBudgetExceeded(
                    f"Agent Loop 已达到模型回合预算 {active_limits.max_model_turns}。"
                )

            reply = request_reply(messages)
            model_turns += 1
            if reply.reasoning:
                reasoning_parts.append(reply.reasoning)

            if not reply.tool_calls:
                return AgentLoopResult(
                    final_text=reply.content.strip(),
                    reasoning="\n".join(reasoning_parts),
                    content_streamed=reply.content_streamed,
                    model_turns=model_turns,
                    tool_calls=tool_calls,
                    messages=messages,
                )

            next_tool_count = tool_calls + len(reply.tool_calls)
            if (
                active_limits.max_tool_calls is not None
                and next_tool_count > active_limits.max_tool_calls
            ):
                raise AgentLoopBudgetExceeded(
                    f"Agent Loop 工具调用预算为 {active_limits.max_tool_calls}，"
                    f"当前批次将使调用数达到 {next_tool_count}。"
                )

            # assistant tool-call 消息必须先于对应 observation 回填，保持模型协议完整。
            messages.append(reply.message)
            observations = list(execute_tool_batch(tuple(reply.tool_calls), tool_calls + 1))
            if len(observations) != len(reply.tool_calls):
                raise RuntimeError(
                    "工具批次观察数量与模型调用数量不一致："
                    f"期望 {len(reply.tool_calls)}，实际 {len(observations)}。"
                )
            # 所有 tool result 必须紧跟同一条 assistant tool_calls 消息；若在两条
            # tool result 之间插入视觉 user 消息，OpenAI/Anthropic 会判定协议顺序无效。
            messages.extend(observation.message for observation in observations)
            for observation in observations:
                messages.extend(observation.followup_messages)
            tool_calls = next_tool_count
            check_boundary()
