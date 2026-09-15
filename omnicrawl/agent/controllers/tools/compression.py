"""工具输出压缩：结果进入模型上下文前，用外接小模型压成精简观察。

压缩是旁路调用，不是工具执行的一部分：失败、超时、取消都必须退回原始输出，
不能改变工具的成功/失败、协议配对与并发工具各自的完成时刻。
"""

from __future__ import annotations

import json
import logging
import time
from concurrent.futures import ThreadPoolExecutor, TimeoutError as FutureTimeoutError
from dataclasses import replace
from typing import Callable, Sequence

from ....config.features.tool_output_compression import (
    ToolOutputCompressionConfig,
    load_tool_output_compression_config,
)
from ....config.models.llm import LLMConfig
from ...runtime.tool_output_compressor import (
    ToolOutputCompressor,
    ToolOutputCompressionError,
    looks_like_cancellation,
)
from ...types import ToolCall, ToolResult

LOGGER = logging.getLogger(__name__)

# 参数摘要只用于给压缩模型提供上下文，超长参数（write_file 正文等）必须截断。
ARGUMENTS_PREVIEW_CHARS = 600
# 压缩是逐工具旁路调用；并发上限避免一批大输出同时占满 Provider 配额。
MAX_PARALLEL_COMPRESSIONS = 4
# 超出模型请求超时后的宽限，避免线程池回收阻塞本回合。
COMPACTION_GRACE_SECONDS = 5.0
RAW_OUTPUT_SEPARATOR = "—— 原始输出（未压缩）——"
# 压缩作用域：只有这几类会产生大段原始输出的工具参与压缩，其余工具一律保留原文。
COMPACTABLE_TOOLS = frozenset({"bash", "powershell", "git", "grep"})


class ToolOutputCompressionMixin:
    """把本批工具结果交给压缩模型，同时保留原始输出供人查看。"""

    def _tool_output_compression_config(self) -> ToolOutputCompressionConfig:
        """返回当前工具输出压缩配置；Agent 内存优先，未初始化时读取磁盘。"""

        config = getattr(getattr(self, "config", None), "tool_output_compression", None)
        if isinstance(config, ToolOutputCompressionConfig):
            return config
        return load_tool_output_compression_config()

    def _compact_tool_outputs(
        self,
        items: Sequence[tuple[ToolCall, ToolResult]],
        *,
        prompt: str,
        check_cancelled: Callable[[], None],
        report_update: Callable[[ToolCall, ToolResult], None],
        status: Callable[[str], None],
    ) -> list[ToolResult]:
        """按顺序返回工具结果；被压缩项替换模型可见文本并保留原始输出。

        未启用、未选模型或没有合格结果时零开销直接返回原结果。
        """

        results = [result for _tool_call, result in items]
        configuration = self._tool_output_compression_config()
        base_llm = getattr(getattr(self, "config", None), "llm", None)
        if not configuration.active or not isinstance(base_llm, LLMConfig):
            return results

        eligible = [
            index
            for index, (tool_call, result) in enumerate(items)
            if _should_compact(tool_call, result, configuration)
        ]
        if not eligible:
            return results

        compressor = ToolOutputCompressor(
            base_llm=base_llm,
            configuration=configuration,
            workspace_root=self.workspace_root,
        )
        status(f"正在压缩工具结果（{len(eligible)} 条）…")
        try:
            compacted = self._run_compactions(
                compressor,
                items,
                eligible,
                prompt=prompt,
                check_cancelled=check_cancelled,
                configuration=configuration,
            )
        finally:
            status("")

        for index in sorted(compacted):
            tool_call = items[index][0]
            new_result = compacted[index]
            results[index] = new_result
            try:
                report_update(tool_call, new_result)
            except Exception:  # noqa: BLE001 - 展示回调不得改变压缩结果
                LOGGER.warning("工具输出压缩展示回调失败", exc_info=True)
        return results

    def _run_compactions(
        self,
        compressor: ToolOutputCompressor,
        items: Sequence[tuple[ToolCall, ToolResult]],
        eligible: Sequence[int],
        *,
        prompt: str,
        check_cancelled: Callable[[], None],
        configuration: ToolOutputCompressionConfig,
    ) -> dict[int, ToolResult]:
        """并发压缩合格结果，返回「下标 → 新结果」映射（失败项不入表）。"""

        compacted: dict[int, ToolResult] = {}
        if not eligible:
            return compacted

        deadline = time.perf_counter() + float(configuration.timeout_seconds)
        deadline += COMPACTION_GRACE_SECONDS
        executor = ThreadPoolExecutor(
            max_workers=min(MAX_PARALLEL_COMPRESSIONS, len(eligible))
        )
        futures = {
            index: executor.submit(
                _compress_one,
                compressor,
                items[index][0],
                items[index][1],
                prompt=prompt,
                check_cancelled=check_cancelled,
            )
            for index in eligible
        }
        try:
            for index in eligible:
                check_cancelled()
                remaining = max(0.0, deadline - time.perf_counter())
                try:
                    new_result = futures[index].result(timeout=remaining)
                except FutureTimeoutError:
                    # 压缩线程可能仍在后台收尾；其结果丢弃，原始输出继续进入模型。
                    LOGGER.warning("工具输出压缩超时，保留原始输出：index=%s", index)
                    continue
                except ToolOutputCompressionError as exc:
                    LOGGER.warning("工具输出压缩失败，保留原始输出：%s", exc)
                    continue
                except Exception as exc:  # noqa: BLE001 - 取消必须上抛，其余退化为原文
                    if looks_like_cancellation(exc):
                        raise
                    LOGGER.warning("工具输出压缩异常，保留原始输出：%s", exc)
                    continue
                if new_result is not None:
                    compacted[index] = new_result
        finally:
            executor.shutdown(wait=False)
        return compacted


def _should_compact(
    tool_call: ToolCall,
    result: ToolResult,
    configuration: ToolOutputCompressionConfig,
) -> bool:
    """工具结果是否值得压缩：属于压缩作用域且模型可见文本够长。"""

    if tool_call.name not in COMPACTABLE_TOOLS:
        return False
    if not result.output.strip():
        return False
    return len(result.output) >= configuration.min_chars


def _compress_one(
    compressor: ToolOutputCompressor,
    tool_call: ToolCall,
    result: ToolResult,
    *,
    prompt: str,
    check_cancelled: Callable[[], None],
) -> ToolResult | None:
    """压缩单个工具结果；未缩小或失败时返回 None（调用方保留原文）。"""

    check_cancelled()
    outcome = compressor.compress(
        tool_name=tool_call.name,
        arguments_summary=_arguments_summary(tool_call.arguments),
        task_hint=prompt,
        output=result.output,
        cancel_check=check_cancelled,
    )
    if len(outcome.text) >= len(result.output):
        # 模型没有真正压缩（复述原文或更长）：不采纳，避免用更差的文本替换原文。
        LOGGER.info("工具输出压缩未缩小结果，保留原始输出：%s", tool_call.name)
        return None
    raw_display = result.full_output or result.output
    return replace(
        result,
        output=outcome.text,
        full_output=_compacted_display(
            outcome.text,
            raw_display,
            compressed_chars=len(outcome.text),
            raw_chars=len(result.output),
            model=outcome.model,
        ),
    )


def _compacted_display(
    compressed: str,
    raw_display: str,
    *,
    compressed_chars: int,
    raw_chars: int,
    model: str,
) -> str:
    """TUI/会话显示文本：压缩结果在前，原始输出随后，便于展开核对。"""

    return (
        f"（已压缩：{raw_chars} → {compressed_chars} 字符，模型 {model}）\n"
        f"{compressed}\n\n"
        f"{RAW_OUTPUT_SEPARATOR}\n"
        f"{raw_display}"
    )


def _arguments_summary(arguments: object) -> str:
    """工具参数摘要：给压缩模型判断「这次调用在做什么」，超长部分截断。"""

    if not isinstance(arguments, dict) or not arguments:
        return ""
    try:
        text = json.dumps(arguments, ensure_ascii=False, default=str)
    except Exception:  # noqa: BLE001 - 不可序列化参数退化为空摘要
        return ""
    if len(text) <= ARGUMENTS_PREVIEW_CHARS:
        return text
    return text[:ARGUMENTS_PREVIEW_CHARS] + "…"


__all__ = [
    "ARGUMENTS_PREVIEW_CHARS",
    "MAX_PARALLEL_COMPRESSIONS",
    "ToolOutputCompressionMixin",
]
