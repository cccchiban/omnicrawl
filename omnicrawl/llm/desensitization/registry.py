"""序号注册表：占位符序号分配、发送-接收周期登记与还原查询（设计稿 §6/§7）。

原文只进内存注册表：不落盘、不进日志、不进会话事件与 SSE（§7.1）；可观测信息
只到「计数 / 规则 ID / 序号」粒度（§10.2）。
"""

from __future__ import annotations

import itertools
import re
import threading
from dataclasses import dataclass, field
from typing import Callable, Iterable

PLACEHOLDER_MARKER = "Desensitized"
FULLWIDTH_OPEN_BRACE = "｛"
FULLWIDTH_CLOSE_BRACE = "｝"

# 生成端唯一规范为全角 ｛Desensitized:n｝；还原端兼容半角、冒号变体、大小写
# 与序号两侧空白（设计稿 §6.1/§6.2）。
PLACEHOLDER_PATTERN = re.compile(
    r"[｛{]\s*desensitized\s*[:：]\s*(\d+)\s*[｝}]",
    re.IGNORECASE,
)

# 疑似占位符前缀：用于识别「半截 / 畸形」文本（保留 + 告警）。
PLACEHOLDER_PREFIX_PATTERN = re.compile(r"[｛{]\s*desensitized", re.IGNORECASE)

_SEQUENCE_LOCK = threading.Lock()
_SEQUENCE_COUNTER = itertools.count(1)
_CYCLE_ID_LOCK = threading.Lock()
_CYCLE_ID_COUNTER = itertools.count(1)


def next_sequence_number() -> int:
    """进程内全局单调递增序号（从 1 开始，注销后不复用，§7.2）。"""

    with _SEQUENCE_LOCK:
        return next(_SEQUENCE_COUNTER)


def format_placeholder(seq: int) -> str:
    """按规范形式生成占位符（全角花括号，序号无前导零，§6.1）。"""

    return f"{FULLWIDTH_OPEN_BRACE}{PLACEHOLDER_MARKER}:{int(seq)}{FULLWIDTH_CLOSE_BRACE}"


def collect_placeholder_numbers(texts: Iterable[str]) -> set[int]:
    """收集出站内容中已存在的占位符样式序号；分配时跳过以避免还原冲突（§6.3）。"""

    numbers: set[int] = set()
    for text in texts:
        if not text:
            continue
        for match in PLACEHOLDER_PATTERN.finditer(text):
            numbers.add(int(match.group(1)))
    return numbers


@dataclass
class DesensitizationStats:
    """只到「数量级」粒度的审计计数；禁止记录任何原文（§10.2）。"""

    cycles_started: int = 0
    cycles_reused: int = 0
    values_masked: int = 0
    restore_hits: int = 0
    restore_unresolved: int = 0
    restore_malformed: int = 0
    skipped_values: int = 0
    entropy_masked: int = 0
    mask_failures: int = 0
    last_mask_duration_ms: float = 0.0


@dataclass
class PlaceholderCycle:
    """一次发送-接收周期的注册集合：序号 → 原文，同值同号（§5.3/§7）。"""

    cycle_id: str
    sequence_source: Callable[[], int]
    entries: dict[int, str] = field(default_factory=dict)
    reserved: set[int] = field(default_factory=set)
    closed: bool = False
    source_request: object | None = None
    masked_request: object | None = None
    _value_index: dict[str, int] = field(default_factory=dict, repr=False)

    def seq_for_value(self, value: str) -> tuple[int, bool]:
        """返回值对应序号；同值复用同一序号，新值分配并跳过预留序号。"""

        existing = self._value_index.get(value)
        if existing is not None:
            return existing, False
        seq = self.sequence_source()
        while seq in self.reserved:
            seq = self.sequence_source()
        self._value_index[value] = seq
        self.entries[seq] = value
        return seq, True

    def lookup(self, seq: int) -> str | None:
        """按序号取原文；未注册返回 None（调用方保留占位符 + 告警，§6.2）。"""

        return self.entries.get(seq)

    def close(self) -> None:
        """注销本周期全部序号并释放原文（§7.3）。"""

        self.entries.clear()
        self._value_index.clear()
        self.closed = True


class SequenceRegistry:
    """按周期管理注册表：分配 / 复用 / 注销 / 关闭丢弃（设计稿 §7）。"""

    def __init__(self, *, sequence_source: Callable[[], int] | None = None) -> None:
        self._lock = threading.Lock()
        self._sequence_source = sequence_source or next_sequence_number
        self._open_cycles: dict[str, PlaceholderCycle] = {}
        self._last_cycle: PlaceholderCycle | None = None
        self.stats = DesensitizationStats()

    def begin_cycle(self, request: object) -> tuple[PlaceholderCycle, bool]:
        """开始新周期；若与上一个未完成周期请求全量相等则为同一逻辑请求的重试复用。"""

        with self._lock:
            last = self._last_cycle
            if (
                last is not None
                and not last.closed
                and last.masked_request is not None
                and last.source_request == request
            ):
                self.stats.cycles_reused += 1
                return last, True
            cycle = PlaceholderCycle(
                cycle_id=_next_cycle_id(),
                sequence_source=self._sequence_source,
            )
            cycle.source_request = request
            self._open_cycles[cycle.cycle_id] = cycle
            self._last_cycle = cycle
            self.stats.cycles_started += 1
            return cycle, False

    def close_cycle(self, cycle: PlaceholderCycle) -> None:
        """周期还原组装结束：注销本周期全部序号（含未被引用项，§7.3）。"""

        with self._lock:
            cycle.close()
            self._open_cycles.pop(cycle.cycle_id, None)
            if self._last_cycle is cycle:
                self._last_cycle = None

    def drop_all(self) -> None:
        """运行时关闭：丢弃全部未注销序号，不落盘、不恢复（§7.3）。"""

        with self._lock:
            for cycle in self._open_cycles.values():
                cycle.close()
            self._open_cycles.clear()
            self._last_cycle = None

    @property
    def open_cycle_count(self) -> int:
        with self._lock:
            return len(self._open_cycles)


def _next_cycle_id() -> str:
    with _CYCLE_ID_LOCK:
        return f"cycle-{next(_CYCLE_ID_COUNTER)}"


__all__ = [
    "DesensitizationStats",
    "FULLWIDTH_CLOSE_BRACE",
    "FULLWIDTH_OPEN_BRACE",
    "PLACEHOLDER_MARKER",
    "PLACEHOLDER_PATTERN",
    "PLACEHOLDER_PREFIX_PATTERN",
    "PlaceholderCycle",
    "SequenceRegistry",
    "collect_placeholder_numbers",
    "format_placeholder",
    "next_sequence_number",
]
