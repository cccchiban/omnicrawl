"""Agent 子系统的共享定义：异常、常量、回合快照与模块级辅助函数。"""
from __future__ import annotations

import os
from concurrent.futures import ThreadPoolExecutor, TimeoutError as FutureTimeoutError
from contextvars import copy_context
from dataclasses import dataclass, field, replace
from pathlib import Path, PurePosixPath
from typing import Any, Callable, Mapping, Sequence
from ..runtime.llm_protocol import (
    AgentLLMProtocol,
    AgentProtocolError,
    build_extra_body,
    chat_completion_tools,
    function_name_for_tool,
    resolve_tool_name_from_hashed_function_name,
    tool_name_from_function_name,
)
from ..types import AgentModelReply, ToolCall, ToolDefinition, ToolResult
from ...config.features.context_compaction import (
    ContextCompactionConfig,
    load_context_compaction_config,
)
from ...llm import (
    LLMConfig,
    LLMError,
    ModelError,
    ModelErrorCode,
    ModelRuntimeManager,
    OpenAIResponseLLM,
    load_llm_config,
    normalize_reasoning_effort,
)
from ...state.turn_snapshot import (
    SnapshotConflictError,
    SnapshotError,
    WorktreeSnapshot,
    WorktreeSnapshotStore,
)


DEFAULT_TOOL_TIMEOUT_SECONDS = 600


MAX_TOOL_TIMEOUT_SECONDS = 3600


TOOL_OUTPUT_INLINE_LIMIT_CHARS = 50_000


TOOL_OUTPUT_BATCH_BUDGET_CHARS = 200_000


TOOL_OUTPUT_ARCHIVED_PREVIEW_CHARS = 4_000


SUBAGENT_LIFECYCLE_WAIT_SECONDS = 5.0


SYSTEM_PROMPT_FILE = "system_prompt.md"


AGENTS_INSTRUCTIONS_FILE = "AGENTS.md"


_CONTEXT_OVERFLOW_RECOVERY_PROMPT = "请依据上方的结构化工作摘要继续完成当前任务。"


_CONTEXT_OVERFLOW_ERROR_MARKERS = (
    "context length",
    "context window",
    "maximum context",
    "max context",
    "context limit",
    "too many tokens",
    "token limit",
    "input is too long",
    "prompt is too long",
    "请求过长",
    "上下文过长",
    "上下文长度",
    "超过上下文",
    "超出上下文",
    "token 超限",
    "令牌超限",
)


_RATE_LIMIT_ERROR_MARKERS = (
    "rate limit",
    "too many requests",
    "insufficient_quota",
    "quota",
    "429",
)


_CONTINUE_LAST_TASK_TEXTS = {
    "继续",
    "继续上次",
    "继续上一轮",
    "接着来",
    "接着做",
    "重试",
    "再试一次",
    "再试试",
    "retry",
    "continue",
}


class AgentError(RuntimeError):
    """Agent 循环、工具调用或安全校验失败时抛出。"""


@dataclass
class _ActiveTurnSnapshot:
    """当前模型轮次的工作区 diff 快照与副作用账本。"""

    snapshot_id: str
    store: WorktreeSnapshotStore
    workspace: Path
    before: WorktreeSnapshot
    executed_tools: list[str] = field(default_factory=list)
    irreversible_tools: list[str] = field(default_factory=list)
    completed: bool = False


_READ_ONLY_UNDO_TOOLS = frozenset(
    {
        "list",
        "find",
        "read",
        "read_image",
        "grep",
        "recall_session_evidence",
        "dev_router_status",
        "memory_search",
        "memory_read",
        "memory_expand_related",
        "project_memory_search",
        "project_memory_read",
        "project_memory_expand_related",
        "session_memory_search",
        "session_memory_read",
        "session_memory_expand_related",
        "user_memory_search",
        "user_memory_read",
        "user_memory_expand_related",
    }
)


_REVERSIBLE_UNDO_TOOLS = frozenset(
    {
        "replace_text",
        "write_file",
    }
)


_MEMORY_UNDO_EXEMPT_TOOLS = frozenset(
    {
        "memory_write",
        "project_memory_write",
        "session_memory_write",
        "user_memory_write",
    }
)


_ROUTER_UNDO_EXEMPT_TOOLS = frozenset(
    {
        "dev_router_mode",
        "dev_mode_subagent",
    }
)


def _read_int_env(name: str, default: int, *, min_value: int, max_value: int) -> int:
    """读取整数环境变量，并把配置错误转成 Agent 可捕获的中文错误。"""

    raw_value = os.getenv(name)
    if raw_value is None or raw_value.strip() == "":
        return default

    try:
        value = int(raw_value.strip())
    except ValueError as exc:
        raise AgentError(
            f"{name} 必须是 {min_value} 到 {max_value} 的整数，当前值：{raw_value}。"
        ) from exc

    return _validate_int_range(name, value, min_value=min_value, max_value=max_value)


def _validate_int_range(name: str, value: int, *, min_value: int, max_value: int) -> int:
    """校验整数范围，覆盖测试或调用方手动构造 AgentConfig 的情况。"""

    if isinstance(value, bool) or not isinstance(value, int):
        raise AgentError(f"{name} 必须是 {min_value} 到 {max_value} 的整数。")
    if value < min_value or value > max_value:
        raise AgentError(f"{name} 必须是 {min_value} 到 {max_value} 的整数，当前值：{value}。")
    return value


def _validate_context_compaction_window(
    config: ContextCompactionConfig | None,
    llm: LLMConfig,
    *,
    context_window_tokens: int | None = None,
) -> None:
    """确保自动压缩能在下一次主模型请求达到窗口上限前触发。"""

    if config is None or not config.enabled:
        return
    context_window = (
        llm.context_window_tokens
        if context_window_tokens is None
        else context_window_tokens
    )
    # 压缩请求只需为摘要输出预留空间：取“摘要预算（0=无预算时按 8K 默认）”
    # 与“模型实际输出能力”的较小值。不能把模型最大输出预算（max_output_tokens，
    # 大输出模型可接近整个窗口，如 deepseek 系列约 90%）全量计入——否则
    # 触发阈值必须小于窗口的 ~10%，压缩几乎无法配置。
    summary_budget = max(config.target_summary_tokens or 8_192, 8_192)
    max_output = int(getattr(llm, "max_output_tokens", 0) or 8_192)
    output_reserve = min(summary_budget, max_output)
    required_window = (
        config.trigger_context_tokens
        + config.next_user_reserve_tokens
        + output_reserve
    )
    if context_window <= required_window:
        raise AgentError(
            "启用 context_compaction 时，活动模型上下文窗口必须大于 "
            f"{required_window} Token，当前为 {context_window}。"
        )


def _unknown_tool_result(
    requested_name: str,
    active_tools: Mapping[str, ToolDefinition],
) -> ToolResult:
    """构造“未知工具”错误结果，并在可识别时给出纠正提示。

    模型回显旧式哈希函数名时可能截断可读段（如 tool_search_e960b0242f 而非
    tool_search_tools_e960b0242f），此时按 digest 反查命中真实工具，错误信息
    直接提示正确名称，帮助模型下一轮使用准确名称调用。
    """
    candidates = tuple(active_tools)
    resolved = resolve_tool_name_from_hashed_function_name(requested_name, candidates)
    if resolved is not None and resolved != requested_name:
        output = (
            f"未知工具：{requested_name}。该名称疑似 {resolved} 的哈希函数名变体，"
            f"正确名称为 {resolved}。请直接调用 {resolved}。"
        )
    else:
        output = (
            f"未知工具：{requested_name}。请从已注册的工具名中选择正确的名称重试。"
        )
    return ToolResult(ok=False, output=output)


def _tool_timeout_result(timeout_seconds: int) -> ToolResult:
    """构造工具执行超时的结构化错误结果。

    返回给模型的是可读的超时说明；后台线程无法安全强杀，其结果被丢弃，
    因此该结果会在会话里留下“工具超时”记录，提示模型下一步处理。
    """

    return ToolResult(
        ok=False,
        output=(
            f"工具执行超时（超过 {timeout_seconds} 秒未完成），已中止等待。"
            "（后台线程仍在运行，其结果已被丢弃。）"
        ),
    )


def _execute_call_with_timeout(
    execute_call: Callable[[int], ToolResult],
    index: int,
    timeout_seconds: int,
) -> ToolResult:
    """在独立线程执行串行工具并限时等待；超时返回错误结果不阻塞回合。

    与并行分支的 ``future.result(timeout=...)`` 保持同一语义：超时后线程
    继续运行但结果被丢弃，回合继续推进。
    """

    executor = ThreadPoolExecutor(max_workers=1)
    try:
        future = executor.submit(
            copy_context().copy().run,
            execute_call,
            index,
        )
        try:
            return future.result(timeout=timeout_seconds)
        except FutureTimeoutError:
            return _tool_timeout_result(timeout_seconds)
    finally:
        # 超时线程仍在后台运行：不等待其结束，避免串行屏障被拖到工具自然完成。
        executor.shutdown(wait=False)
