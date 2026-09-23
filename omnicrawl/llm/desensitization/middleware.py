"""出站屏蔽 / 入站还原的「脱敏运行时」装饰器（设计稿 §4.2）。

在 ``build_runtime`` 返回处包一层本装饰器：入口对 ``request.messages`` 做屏蔽
并注册序号；出口对事件流逐事件做还原、按周期注销；对上层完全透明（回调 /
回复组装 / 后续工具执行全部拿到还原后的内容）。未启用（或配置读取失败）时
不包装，零成本。
"""

from __future__ import annotations

import hashlib
import json
import logging
import threading
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
    ImageBlock,
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
    ToolCallArgumentsDelta,
    ToolCallCompleted,
    ToolResultBlock,
)
from .engine import MaskContext, SensitiveMatcher, mask_structured_value, mask_text
from .ner import NerLayer, build_ner_layer
from .plan_cache import MaskPlanCache
from .registry import (
    DesensitizationStats,
    PlaceholderCycle,
    SequenceRegistry,
    SessionSequenceCache,
    PLACEHOLDER_PATTERN,
    collect_placeholder_numbers,
)
from .rules import PatternRule, build_enabled_rules
from .stream import StreamRestorer


LOGGER = logging.getLogger(__name__)


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
        # 值类型规则层（含 gitleaks）在运行时构建时解析一次，请求间复用（规则对象不可变）。
        self._rules: tuple[PatternRule, ...] = build_enabled_rules(config)
        # NER 兜底层（可选依赖 torch）：未启用 / 环境不满足时为 None，静默跳过。
        self._ner_layer: NerLayer | None = build_ner_layer(config)
        self._memo = _MessageMaskMemo()
        self._registry = registry or SequenceRegistry()
        self._plan_cache = MaskPlanCache()

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

<<<<<<< ours
<<<<<<< ours
        self._memo.clear()
=======
        self._plan_cache.clear()
>>>>>>> theirs
=======
        self._plan_cache.clear()
>>>>>>> theirs
        self._registry.drop_all()
        self._inner.close()

    @property
    def plan_cache(self) -> MaskPlanCache:
        """计划缓存（观测 / 测试用）。"""

        return self._plan_cache

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
                    request,
                    self._matcher,
                    cycle,
                    self._registry.stats,
                    self._config,
                    self._rules,
                    self._ner_layer,
<<<<<<< ours
<<<<<<< ours
                    self._memo,
=======
                    plan_cache=self._plan_cache,
>>>>>>> theirs
=======
                    plan_cache=self._plan_cache,
>>>>>>> theirs
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
                _log_restore_warning(warning)
                yield warning
        # 流正常结束：冲刷挂起缓冲；回复可用时按成功注销（§7.3/§9.1）。
        text_tail, reasoning_tail = restorer.flush()
        if text_tail:
            yield TextDelta(text=text_tail)
        if reasoning_tail:
            yield ReasoningDelta(text=reasoning_tail)
        for call_id, tail in restorer.flush_tool_arguments().items():
            yield ToolCallArgumentsDelta(call_id=call_id, delta=tail)
        for warning in restorer.take_warnings():
            _log_restore_warning(warning)
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
        if isinstance(event, ToolCallArgumentsDelta):
            text = restorer.feed_tool_arguments(event.call_id, event.delta)
            return [replace(event, delta=text)] if text else []
        if isinstance(event, ToolCallCompleted):
            restorer.note_tool_call()
            arguments = restorer.restore_arguments(event.arguments)
            leaked = _assigned_but_unresolved(self._registry, arguments)
            if leaked:
                raise DesensitizationError(
                    "工具参数里出现本进程分配过、但已无法还原的脱敏占位符"
                    f"（序号 {', '.join(str(item) for item in leaked)}）；"
                    "已按 fail-closed 中止，避免把占位符写进文件。"
                )
            return [replace(event, arguments=arguments)]
        if isinstance(event, ResponseCompleted):
            restorer.note_completed(event.finish_reason)
            return [event]
        # ToolCallStarted / UsageUpdated / ProviderWarning 原样透传（不含文本内容）。
        return [event]


def maybe_wrap_runtime(
    runtime: ModelRuntime,
    *,
    store_provider: Callable[[], SessionSequenceCache] | None = None,
) -> ModelRuntime:
    """按配置决定是否包装运行时；未启用或配置不可读时原样返回（零成本）。

    ``store_provider`` 返回当前会话的「序号 → 原文」映射（会话所有者持有，见 §7.2）；
    未提供时注册表用自己的私有映射，即按运行时隔离（等价于旧行为）。
    """

    try:
        config = load_desensitization_config()
    except Exception:
        # 配置读取失败不得影响模型运行时构建；脱敏默认关闭。
        return runtime
    if not config.enabled:
        return runtime
    if store_provider is None:
        return DesensitizationRuntime(runtime, config)
    return DesensitizationRuntime(
        runtime,
        config,
        registry=SequenceRegistry(store_provider=store_provider),
    )


def _mask_request(
    request: ModelTurnRequest,
    matcher: SensitiveMatcher,
    cycle: PlaceholderCycle,
    stats: DesensitizationStats,
    config: DesensitizationConfig,
    rules: tuple[PatternRule, ...] = (),
    ner_layer: NerLayer | None = None,
<<<<<<< ours
<<<<<<< ours
    memo: "_MessageMaskMemo | None" = None,
=======
    plan_cache: MaskPlanCache | None = None,
>>>>>>> theirs
=======
    plan_cache: MaskPlanCache | None = None,
>>>>>>> theirs
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
        pattern_rules=rules,
        ner_layer=ner_layer,
        plan_cache=plan_cache,
<<<<<<< ours
    )
    messages = tuple(
        _mask_message_cached(message, context, memo) for message in request.messages
=======
>>>>>>> theirs
    )
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


#: 逐消息屏蔽结果缓存的字符预算（超出按最久未用淘汰）。历史文本本身已在内存里，
#: 这里多留一份屏蔽后的内容，是为了让「只追加」的历史每轮只扫新消息。
#: 预算同时也是「被屏蔽值原文」在缓存里的驻留上限：调小更保守、命中率随之下降。
#: 设为 0 即关闭该缓存（退回每轮全量重扫，行为与优化前一致）。
_MEMO_MAX_CHARS = 1024 * 1024


class _MessageMaskMemo:
    """逐消息屏蔽结果缓存（§5.6 性能）。

    历史每轮全量重发且只追加：已出现过且逐字未变的消息不必重复走匹配引擎。
    条目保存「屏蔽后的 blocks / reasoning」与新增的 (序号, 原文) 对——序号由进程级
    稳定索引保证永久一致，缓存只是省掉扫描；原文那份用于把缓存结果重新登记进本周期的
    还原集合，否则模型回引历史占位符会落入「未注册序号」分支。

    结果对象按当前消息重建（只替换参与屏蔽的字段），因此未参与屏蔽的字段
    （如 per-message 工具声明）不会因复用而丢失。
    """

    def __init__(self, max_chars: int = _MEMO_MAX_CHARS) -> None:
        self._max_chars = max_chars
        self._entries: dict[
            bytes, tuple[Any, Any, tuple[tuple[int, str], ...], int, tuple[int, ...]]
        ] = {}
        self._chars = 0
        self._lock = threading.Lock()
        self.hits = 0
        self.misses = 0

    def get(
        self, key: bytes
    ) -> tuple[Any, Any, tuple[tuple[int, str], ...], tuple[int, ...]] | None:
        with self._lock:
            entry = self._entries.get(key)
            if entry is None:
                self.misses += 1
                return None
            # 命中即移到最新（dict 保序，重插即置尾）。
            self._entries[key] = self._entries.pop(key)
            self.hits += 1
            return entry[0], entry[1], entry[2], entry[4]

    def put(
        self,
        key: bytes,
        blocks: Any,
        reasoning: Any,
        pairs: tuple[tuple[int, str], ...],
        chars: int,
        seqs: tuple[int, ...] = (),
    ) -> None:
        if self._max_chars <= 0:
            return
        with self._lock:
            previous = self._entries.pop(key, None)
            if previous is not None:
                self._chars -= previous[3]
            self._entries[key] = (blocks, reasoning, pairs, chars, seqs)
            self._chars += chars
            while self._chars > self._max_chars and len(self._entries) > 1:
                # 淘汰「最近用过」而不是「最久没用」：历史是每轮从头到尾顺序全扫，
                # 工作集超出预算时 LRU 会正好淘汰下一轮马上要用的条目（抖动到 0 命中），
                # 淘汰最近用过的条目则等价于把历史头部钉在缓存里，命中率随预算线性下降。
                evicted_key = next(reversed(self._entries))
                self._chars -= self._entries.pop(evicted_key)[3]

    def clear(self) -> None:
        with self._lock:
            self._entries.clear()
            self._chars = 0


def _digest_text(digest: Any, tag: bytes, text: str) -> None:
    digest.update(tag)
    digest.update(str(len(text)).encode("ascii"))
    digest.update(b"\x1f")
    digest.update(text.encode("utf-8", "replace"))


def _message_digest(message: ConversationMessage) -> bytes:
    """缓存键：覆盖「复用安全」所需的全部字段，而不只是被屏蔽的文本。

    命中缓存后是整块复用缓存里的 ``blocks``，因此任何决定消息身份的字段都必须
    进键：``tool_call_id`` / 函数名 / 图片数据 / ``ok``。只按正文做键会让两条正文
    相同的消息互相串号——最典型的是同一批里两个返回值完全一样的工具结果（如两个
    ``memory_search`` 都返回 ``[]``）：第二条会拿到第一条的 ``tool_call_id``，使
    assistant 声明的另一个 ``tool_call_id`` 没有配对结果，Provider 直接以 HTTP 400
    拒收整轮请求（Anthropic / OpenAI / 网关均为硬校验）。
    """

    digest = hashlib.blake2b(digest_size=16)
    _digest_text(digest, b"r", message.role or "")
    if message.reasoning:
        _digest_text(digest, b"q", message.reasoning)
    for block in message.blocks:
        if isinstance(block, TextBlock):
            _digest_text(digest, b"t", block.text or "")
        elif isinstance(block, ToolCallBlock):
            # 调用身份：id 不同即不同消息，重复的 id 会让 Provider 报「工具结果无配对调用」。
            _digest_text(digest, b"c", block.call_id or "")
            _digest_text(digest, b"n", block.name or "")
            _digest_text(digest, b"p", block.provider_call_id or "")
            _digest_text(digest, b"a", _json_text(block.arguments))
        elif isinstance(block, ToolResultBlock):
            # 结果身份：call_id 与 ok 必须进键，否则「同样返回 [] 的两条结果」会串号。
            _digest_text(digest, b"s", block.call_id or "")
            _digest_text(digest, b"k", "1" if block.ok else "0")
            _digest_text(digest, b"v", block.content or "")
        elif isinstance(block, ImageBlock):
            # 图片不参与文本屏蔽，但缓存复用会整块替换：不同图片绝不能共享条目。
            _digest_text(digest, b"i", block.media_type or "")
            _digest_text(digest, b"d", block.detail or "")
            _digest_text(digest, b"b", block.data_base64 or "")
        else:
            _digest_text(digest, b"x", type(block).__name__)
    return digest.digest()


def _masked_chars(message: ConversationMessage) -> int:
    total = len(message.reasoning or "")
    for block in message.blocks:
        if isinstance(block, TextBlock):
            total += len(block.text or "")
        elif isinstance(block, ToolResultBlock):
            total += len(block.content or "")
        elif isinstance(block, ToolCallBlock):
            total += len(_json_text(block.arguments))
    return total


def _mask_message_cached(
    message: ConversationMessage,
    ctx: MaskContext,
    memo: "_MessageMaskMemo | None" = None,
) -> ConversationMessage:
    """带缓存的单条消息屏蔽：命中即复用，未命中才走引擎并记入缓存。"""

    if memo is None:
        return _mask_message(message, ctx)
    key = _message_digest(message)
    cached = memo.get(key)
    if cached is not None:
        blocks, reasoning, pairs, seqs = cached
        for seq, value in pairs:
            ctx.cycle.adopt(value, seq)
        if all(ctx.cycle.lookup(seq) is not None for seq in seqs):
            if blocks == message.blocks and reasoning == message.reasoning:
                return message
            return replace(message, blocks=blocks, reasoning=reasoning)
        # 缓存里的占位符引用了本周期无法还原的序号（登记过该值的消息已被
        # 压缩 / 丢弃）。复用会把无法还原的占位符交给模型，模型回写进工具参数
        # 就是一处被改坏的内容；退回重扫，把序号重新登记进本周期。
    registered_before = len(ctx.cycle.entries)
    masked = _mask_message(message, ctx)
    memo.put(
        key,
        masked.blocks,
        masked.reasoning,
        ctx.cycle.pairs_from(registered_before),
        _masked_chars(masked),
        _referenced_sequences(masked),
    )
    return masked


def _referenced_sequences(message: ConversationMessage) -> tuple[int, ...]:
    """消息文本里出现的占位符序号（缓存复用前据此确认本周期可还原）。"""

    seqs: list[int] = []
    for text in _iter_message_texts(message):
        for match in PLACEHOLDER_PATTERN.finditer(text):
            seq = int(match.group(1))
            if seq not in seqs:
                seqs.append(seq)
    return tuple(seqs)


def _iter_message_texts(message: ConversationMessage):
    """遍历消息中参与屏蔽的文本字段（与屏蔽路径保持一致）。"""

    yield message.reasoning or ""
    for block in message.blocks:
        if isinstance(block, TextBlock):
            yield block.text or ""
        elif isinstance(block, ToolResultBlock):
            yield block.content or ""
        elif isinstance(block, ToolCallBlock):
            yield _json_text(block.arguments)


def _log_restore_warning(warning: ProviderWarning) -> None:
    """还原类告警同时落日志：只带序号，不带原文（§10.2）。"""

    if warning.code in ("desensitization_unresolved", "desensitization_malformed"):
        LOGGER.warning("%s", warning.message)


def _iter_arg_strings(value: Any) -> Iterator[str]:
    if isinstance(value, str):
        yield value
    elif isinstance(value, dict):
        for item in value.values():
            yield from _iter_arg_strings(item)
    elif isinstance(value, list):
        for item in value:
            yield from _iter_arg_strings(item)


def _assigned_but_unresolved(registry: SequenceRegistry, value: Any) -> tuple[int, ...]:
    """工具参数里「本进程分配过、但当前映射已无法还原」的占位符序号。

    未注册序号默认原样保留（§6.2）；这类占位符一旦落盘就是被改坏的内容，因此写路径
    单独取 fail-closed。只统计本进程确实分配过的序号，避免拦下文档里本就存在的示例文本。
    """

    index = registry.stable_index
    found: list[int] = []
    for text in _iter_arg_strings(value):
        for match in PLACEHOLDER_PATTERN.finditer(text):
            seq = int(match.group(1))
            if index.assigned(seq) and seq not in found:
                found.append(seq)
    return tuple(found)


__all__ = [
    "DesensitizationError",
    "DesensitizationRuntime",
    "maybe_wrap_runtime",
]
