"""流式响应还原：占位符感知的尾部挂起缓冲与结构化还原（设计稿 §8）。

文本分片可能把占位符切在中间（``…｛Desensitized:1`` + ``2｝…``）；朴素替换会
漏还原或误还原。这里只挂起「可能是占位符前缀 / 未闭合占位符」的尾串（长度
有界），其余照常输出；完整占位符出现即还原。工具调用参数在 ``ToolCallCompleted``
（已解析为 dict）处做结构化还原；``ToolCallArgumentsDelta`` 原样透传。
"""

from __future__ import annotations

import re
from typing import Any

from ..protocol import ProviderWarning
from .registry import (
    PLACEHOLDER_PATTERN,
    PLACEHOLDER_PREFIX_PATTERN,
    DesensitizationStats,
    PlaceholderCycle,
)

# 与 omnicrawl/agent/runtime/llm_protocol.py::_TRUNCATED_FINISH_REASONS 一致：
# 这些 finish_reason 表示输出被截断，上层会重试重启本请求（周期不能按成功注销）。
TRUNCATED_FINISH_REASONS = frozenset(
    {"length", "incomplete", "max_tokens", "content_filter", "failed"}
)

_WORD = "desensitized"
# 占位符最长合理长度（防止「疑似前缀」在异常输入下无限挂起）。
_MAX_HOLD_CHARS = 64


class StreamRestorer:
    """单次流式响应的还原状态机（每个尝试一个实例）。"""

    def __init__(
        self,
        cycle: PlaceholderCycle,
        stats: DesensitizationStats,
        *,
        strict: bool = False,
    ) -> None:
        self._cycle = cycle
        self._stats = stats
        self._strict = strict
        self._buffers = {"text": "", "reasoning": ""}
        self._pending_warnings: list[ProviderWarning] = []
        self._warned_unresolved = False
        self._warned_malformed = False
        self.saw_text = False
        self.saw_reasoning = False
        self.saw_tool_call = False
        self.finish_reason = "stop"

    # ── 文本还原（尾部挂起缓冲） ─────────────────────────────────────────

    def feed_text(self, text: str) -> str:
        return self._feed("text", text)

    def feed_reasoning(self, text: str) -> str:
        return self._feed("reasoning", text)

    def feed_tool_arguments(self, call_id: str, text: str) -> str:
        """工具参数增量：与文本同一套尾部挂起缓冲，跨分片的占位符同样要还原。"""

        return self._feed(f"args:{call_id or ''}", text)

    def flush_tool_arguments(self) -> dict[str, str]:
        """流结束：冲刷各工具参数的挂起缓冲，返回 ``{call_id: 需补发的文本}``。"""

        tails: dict[str, str] = {}
        for channel in [name for name in self._buffers if name.startswith("args:")]:
            emitted, _ = self._fold(self._buffers.pop(channel), flush=True)
            if emitted:
                tails[channel[len("args:") :]] = emitted
        return tails

    def flush(self) -> tuple[str, str]:
        """流结束：冲刷两路挂起缓冲（未闭合前缀按原样保留 + 告警）。"""

        text_emit, _ = self._fold(self._buffers["text"], flush=True)
        reasoning_emit, _ = self._fold(self._buffers["reasoning"], flush=True)
        self._buffers["text"] = ""
        self._buffers["reasoning"] = ""
        return text_emit, reasoning_emit

    def _feed(self, channel: str, chunk: str) -> str:
        if not chunk:
            return ""
        emit, hold = self._fold(self._buffers.get(channel, "") + chunk, flush=False)
        self._buffers[channel] = hold
        return emit

    def _fold(self, buf: str, *, flush: bool) -> tuple[str, str]:
        """把缓冲折叠为「可安全输出的文本 + 需继续挂起的尾串」。"""

        parts: list[str] = []
        while True:
            match = PLACEHOLDER_PATTERN.search(buf)
            if match is None:
                break
            parts.append(self._emit_literal(buf[: match.start()]))
            parts.append(self._resolve(match))
            buf = buf[match.end() :]
        index = _partial_start_index(buf)
        if index is None or flush or len(buf) - index > _MAX_HOLD_CHARS:
            parts.append(self._emit_literal(buf))
            return "".join(parts), ""
        parts.append(self._emit_literal(buf[:index]))
        return "".join(parts), buf[index:]

    def _resolve(self, match: re.Match) -> str:
        """还原一个完整占位符；未注册序号保留原样 + 告警（§6.2）。"""

        seq = int(match.group(1))
        value = self._cycle.lookup(seq)
        if value is None:
            self._stats.restore_unresolved += 1
            if not self._warned_unresolved:
                self._warned_unresolved = True
                self._pending_warnings.append(
                    ProviderWarning(
                        code="desensitization_unresolved",
                        message=f"模型返回了未注册的脱敏占位符（序号 {seq}），已按原样保留。",
                    )
                )
            if self._strict:
                # 函数内导入避免与 middleware 形成循环依赖。
                from .middleware import DesensitizationError

                raise DesensitizationError(
                    f"还原遇到未注册的脱敏占位符（序号 {seq}）。"
                )
            return match.group(0)
        self._stats.restore_hits += 1
        return value

    def _emit_literal(self, text: str) -> str:
        """输出不含完整占位符的文本；残留的疑似畸形前缀保留 + 告警（§6.2）。"""

        if text:
            count = len(PLACEHOLDER_PREFIX_PATTERN.findall(text))
            if count:
                self._stats.restore_malformed += count
                if not self._warned_malformed:
                    self._warned_malformed = True
                    self._pending_warnings.append(
                        ProviderWarning(
                            code="desensitization_malformed",
                            message=f"检测到 {count} 处疑似畸形脱敏占位符，已按原样保留。",
                        )
                    )
                if self._strict:
                    from .middleware import DesensitizationError

                    raise DesensitizationError("还原遇到畸形脱敏占位符（strict_restore）。")
        return text

    # ── 结构化还原（工具调用参数） ───────────────────────────────────────

    def restore_string(self, text: str) -> str:
        """替换字符串中所有完整占位符（子串位置还原，§6.2）。"""

        if not text:
            return text
        return PLACEHOLDER_PATTERN.sub(self._resolve, text)

    def restore_arguments(self, value: Any) -> Any:
        """工具调用参数：递归还原字符串值（键与结构件不动，§6.2）。"""

        if isinstance(value, str):
            return self.restore_string(value)
        if isinstance(value, dict):
            return {key: self.restore_arguments(item) for key, item in value.items()}
        if isinstance(value, list):
            return [self.restore_arguments(item) for item in value]
        return value

    # ── 生命周期标记与告警 ──────────────────────────────────────────────

    def note_text(self, text: str) -> None:
        if text:
            self.saw_text = True

    def note_reasoning(self, text: str) -> None:
        if text:
            self.saw_reasoning = True

    def note_tool_call(self) -> None:
        self.saw_tool_call = True

    def note_completed(self, finish_reason: str) -> None:
        self.finish_reason = finish_reason or "stop"

    @property
    def reply_usable(self) -> bool:
        """回复可用（有内容 / 工具调用 / 推理且未被截断）→ 周期按成功注销（§7.3/§9.1）。"""

        if self.finish_reason in TRUNCATED_FINISH_REASONS:
            return False
        return self.saw_text or self.saw_reasoning or self.saw_tool_call

    def take_warnings(self) -> list[ProviderWarning]:
        warnings, self._pending_warnings = self._pending_warnings, []
        return warnings


def _partial_start_index(buf: str) -> int | None:
    """返回最后一个「可能成为占位符前缀」的起点；其后文本需继续挂起。"""

    index = max(buf.rfind("｛"), buf.rfind("{"))
    if index < 0:
        return None
    if _is_partial_prefix(buf[index:]):
        return index
    return None


def _is_partial_prefix(tail: str) -> bool:
    """判断尾串是否可能是完整占位符的前缀（等待后续分片补全）。"""

    rest = tail[1:]
    i = 0
    while i < len(rest) and rest[i].isspace():
        i += 1
    j = 0
    while i < len(rest) and j < len(_WORD) and rest[i].lower() == _WORD[j]:
        i += 1
        j += 1
    if j < len(_WORD):
        # 单词未匹配完：只有「已到串尾」才可能续接，否则永久无效。
        return i >= len(rest)
    while i < len(rest) and rest[i].isspace():
        i += 1
    if i >= len(rest):
        return True
    if rest[i] not in ":：":
        return False
    i += 1
    while i < len(rest) and rest[i].isspace():
        i += 1
    while i < len(rest) and rest[i].isdigit() and ord(rest[i]) < 128:
        i += 1
    while i < len(rest) and rest[i].isspace():
        i += 1
    return i >= len(rest)


__all__ = ["StreamRestorer", "TRUNCATED_FINISH_REASONS"]
