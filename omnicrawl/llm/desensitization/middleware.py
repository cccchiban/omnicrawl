"""出站屏蔽 / 入站还原的「脱敏运行时」装饰器（设计稿 §4.2）。

在 ``build_runtime`` 返回处包一层本装饰器：入口对 ``request.messages`` 做屏蔽
并注册序号；出口对事件流逐事件做还原、按周期注销；对上层完全透明（回调 /
回复组装 / 后续工具执行全部拿到还原后的内容）。未启用（或配置读取失败）时
不包装，零成本。
"""

from __future__ import annotations

import json
import time
from dataclasses import replace
from typing import Any, Callable, Iterator

from ...config.features.desensitization import (
    DesensitizationConfig,
    load_desensitization_config,
)
from ..errors import ModelError, ModelErrorCode
from ..protocol import (
    ConversationMessage,
    MessageBlock,
    ModelRuntime,
    ModelStreamEvent,
    ModelTurnRequest,
    ProviderWarning,
    ReasoningDelta,
    ResponseCompleted,
    TextBlock,
    TextDelta,
    ToolCallBlock,
    ToolCallCompleted,
    ToolResultBlock,
)
from .engine import MaskContext, SensitiveMatcher, mask_structured_value, mask_text
from .registry import (
    DesensitizationStats,
    PlaceholderCycle,
    SequenceRegistry,
    collect_placeholder_numbers,
)
from .stream import StreamRestorer


class DesensitizationError(ModelError):
    """脱敏层中止请求（fail-closed / 严格还原）；不向模型发送原文。"""

    def __init__(self, message: str) -> None:
        super().__init__(code=ModelErrorCode.UNKNOWN, message=message, retryable=False)


class DesensitizationRuntime:
    """消息脱敏装饰器：屏蔽出站请求、还原入站事件、管理序号生命周期。"""

    def __init__(
        self,
        inner: ModelRuntime,
        config: DesensitizationConfig,
        *,
        matcher: SensitiveMatcher | None = None,
        registry: SequenceRegistry | None = None,
    ) -> None:
        self._inner = inner
        self._config = config
        self._matcher = matcher or SensitiveMatcher(
            extra_keys=config.extra_sensitive_keys,
            exempt_keys=config.exempt_keys,
        )
        self._registry = registry or SequenceRegistry()

    @property
    def identity(self):
        return self._inner.identity

    @property
    def capabilities(self):
        return self._inner.capabilities

    @property
    def stats(self) -> DesensitizationStats:
        return self._registry.stats

    def close(self) -> None:
        """关闭：丢弃全部未注销序号（不落盘、不恢复，§7.3），再关闭内层运行时。"""

        self._registry.drop_all()
        self._inner.close()

    def stream_turn(
        self,
        request: ModelTurnRequest,
        *,
        cancel_check: Callable[[], None] | None = None,
    ) -> Iterator[ModelStreamEvent]:
        started = time.monotonic()
        try:
            cycle, reused = self._registry.begin_cycle(request)
            if reused:
                masked_request = cycle.masked_request
            else:
                cycle.reserved = collect_placeholder_numbers(_iter_request_texts(request))
                masked_request = _mask_request(
                    request, self._matcher, cycle, self._registry.stats, self._config
                )
                cycle.masked_request = masked_request
        except Exception as exc:
            self._registry.stats.mask_failures += 1
            if self._config.fail_closed:
                raise DesensitizationError(
                    "消息脱敏失败，已按 fail-closed 策略中止本次请求（未向模型发送原文）。"
                ) from exc
            # 可用性优先降级：发送原文、不做还原（fail_closed=false）。
            yield ProviderWarning(
                code="desensitization_mask_failed",
                message="消息脱敏失败，已按降级策略发送原文。",
            )
            yield from self._inner.stream_turn(request, cancel_check=cancel_check)
            return
        self._registry.stats.last_mask_duration_ms = (time.monotonic() - started) * 1000.0
        restorer = StreamRestorer(
            cycle,
            self._registry.stats,
            strict=self._config.strict_restore,
        )
        for event in self._inner.stream_turn(masked_request, cancel_check=cancel_check):
            for mapped in self._map_event(event, restorer):
                yield mapped
            for warning in restorer.take_warnings():
                yield warning
        # 流正常结束：冲刷挂起缓冲；回复可用时按成功注销（§7.3/§9.1）。
        text_tail, reasoning_tail = restorer.flush()
        if text_tail:
            yield TextDelta(text=text_tail)
        if reasoning_tail:
            yield ReasoningDelta(text=reasoning_tail)
        for warning in restorer.take_warnings():
            yield warning
        if restorer.reply_usable:
            self._registry.close_cycle(cycle)

    def _map_event(
        self,
        event: ModelStreamEvent,
        restorer: StreamRestorer,
    ) -> list[ModelStreamEvent]:
        if isinstance(event, TextDelta):
            restorer.note_text(event.text)
            text = restorer.feed_text(event.text)
            return [TextDelta(text=text)] if text else []
        if isinstance(event, ReasoningDelta):
            restorer.note_reasoning(event.text)
            text = restorer.feed_reasoning(event.text)
            return [ReasoningDelta(text=text)] if text else []
        if isinstance(event, ToolCallCompleted):
            restorer.note_tool_call()
            arguments = restorer.restore_arguments(event.arguments)
            return [replace(event, arguments=arguments)]
        if isinstance(event, ResponseCompleted):
            restorer.note_completed(event.finish_reason)
            return [event]
        # ToolCallStarted / ToolCallArgumentsDelta / UsageUpdated / ProviderWarning 原样透传。
        return [event]


def maybe_wrap_runtime(runtime: ModelRuntime) -> ModelRuntime:
    """按配置决定是否包装运行时；未启用或配置不可读时原样返回（零成本）。"""

    try:
        config = load_desensitization_config()
    except Exception:
        # 配置读取失败不得影响模型运行时构建；脱敏默认关闭。
        return runtime
    if not config.enabled:
        return runtime
    return DesensitizationRuntime(runtime, config)


def _mask_request(
    request: ModelTurnRequest,
    matcher: SensitiveMatcher,
    cycle: PlaceholderCycle,
    stats: DesensitizationStats,
    config: DesensitizationConfig,
) -> ModelTurnRequest:
    context = MaskContext(
        matcher=matcher,
        cycle=cycle,
        stats=stats,
        entropy_enabled=config.entropy_enabled,
        entropy_min_length=config.entropy_min_length,
        entropy_min_bits=config.entropy_min_bits,
        entropy_pure_letters=config.entropy_pure_letters,
        entropy_pure_digits=config.entropy_pure_digits,
    )
    messages = tuple(_mask_message(message, context) for message in request.messages)
    # 稳定序号复用计数（只到计数粒度，§10.2）：同一值跨请求复用同一序号即前缀缓存可命中。
    stats.sequence_reuses += cycle.stable_reuses
    return replace(request, messages=messages)


def _mask_message(message: ConversationMessage, ctx: MaskContext) -> ConversationMessage:
    blocks = tuple(_mask_block(block, message.role, ctx) for block in message.blocks)
    reasoning = (
        mask_text(message.reasoning, ctx) if message.reasoning else message.reasoning
    )
    if blocks == message.blocks and reasoning == message.reasoning:
        return message
    return replace(message, blocks=blocks, reasoning=reasoning)


def _mask_block(block: MessageBlock, role: str, ctx: MaskContext) -> MessageBlock:
    if isinstance(block, TextBlock):
        # 只处理 user / assistant 文本；system 角色文本与工具声明属结构定义（§2.3）。
        if role in ("user", "assistant") and block.text:
            masked = mask_text(block.text, ctx)
            if masked != block.text:
                return replace(block, text=masked)
        return block
    if isinstance(block, ToolCallBlock):
        masked_arguments = mask_structured_value(block.arguments, ctx)
        if masked_arguments != block.arguments:
            return replace(block, arguments=masked_arguments)
        return block
    if isinstance(block, ToolResultBlock):
        if block.content:
            masked = mask_text(block.content, ctx)
            if masked != block.content:
                return replace(block, content=masked)
        return block
    return block  # ImageBlock 不参与文本匹配（§2.3）


def _iter_request_texts(request: ModelTurnRequest) -> Iterator[str]:
    """遍历出站内容中的全部文本（含 system_prompt 与工具声明）用于碰撞扫描。"""

    yield request.system_prompt
    for message in request.messages:
        yield message.reasoning
        for block in message.blocks:
            if isinstance(block, TextBlock):
                yield block.text
            elif isinstance(block, ToolCallBlock):
                yield _json_text(block.arguments)
            elif isinstance(block, ToolResultBlock):
                yield block.content
        for spec in message.tools:
            yield spec.name
            yield spec.description
            yield _json_text(spec.parameters)
    for spec in request.tools:
        yield spec.name
        yield spec.description
        yield _json_text(spec.parameters)


def _json_text(value: Any) -> str:
    try:
        return json.dumps(value, ensure_ascii=False, default=str)
    except Exception:
        return ""


__all__ = [
    "DesensitizationError",
    "DesensitizationRuntime",
    "maybe_wrap_runtime",
]
