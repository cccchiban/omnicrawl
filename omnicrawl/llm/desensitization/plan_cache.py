"""屏蔽计划缓存：固定文本的匹配计划跨请求复用（设计稿 §5.3.2 的同一思路）。

历史每轮全量重发，同一段未变文本会被反复处理：结构层（``.env`` / ``key: value`` /
JSON 片段）、值类型规则层、熵兜底、NER。规则扫描结果已有缓存，但每条消息仍要重建
上下文并逐层重跑。本模块把一次屏蔽的**匹配计划**按文本存起来：

- 计划只记录「各阶段待替换区间的起止 + 稳定序号 + 阶段标签」，**不含原文**；
- 命中时按区间从**当前文本**取值，再走标准 ``placeholder_for`` 重新登记到当前周期，
  因此还原 / 注销 / 并发周期隔离的语义与未命中路径完全一致；
- 命中后序号必须与计划一致（序号由进程级稳定索引按值指纹决定）：不一致说明规则或
  稳定索引变了，按未命中重算——宁慢勿错。

缓存按运行时实例持有（匹配器、规则集合、NER 层都是实例级），随运行时关闭清空；
条目数与字节预算双上限，只保存区间与序号，不保存文本。
"""

from __future__ import annotations

import hashlib
import threading
from collections import OrderedDict
from dataclasses import dataclass

# 默认容量：条目数 + 字节预算（与规则扫描缓存同量级，单条计划只是若干小整数）。
DEFAULT_MAX_ENTRIES = 1024
DEFAULT_MAX_BYTES = 4 * 1024 * 1024

# 阶段标签 → 计数口径（只在「本周期首次登记」时递增，与未命中路径一致）。
STAGE_COUNTER_FIELDS = {
    "rules": "rules_masked",
    "entropy": "entropy_masked",
    "ner": "ner_masked",
}


@dataclass(frozen=True)
class PlanSpan:
    """一个待替换区间：``(start, end)`` 是所在阶段输入文本的坐标。"""

    start: int
    end: int
    seq: int
    counter: str = ""


@dataclass(frozen=True)
class MaskPlan:
    """一次屏蔽的匹配计划：阶段按执行顺序排列，阶段内的区间按起点升序且互不重叠。"""

    stages: tuple[tuple[PlanSpan, ...], ...] = ()

    @property
    def span_count(self) -> int:
        """区间总数（观测与容量估算用）。"""

        return sum(len(stage) for stage in self.stages)


class MaskPlanBuilder:
    """按阶段记录待替换区间，供 ``MaskContext`` 在屏蔽过程中调用。

    记录数与实际占位符分配数不一致（例如转义值与文本切片不一致、计划漏记）时
    ``build`` 返回 ``None``，该文本不进缓存——漏缓存只损失速度，错缓存会改语义。
    """

    def __init__(self) -> None:
        self._stages: list[list[PlanSpan]] = []
        self._current: list[PlanSpan] | None = None
        self._counter = ""
        self._records = 0
        self.registrations = 0

    def begin_stage(self, counter: str = "") -> None:
        """开启一个新阶段（对应一次 ``.sub`` / 一层规则）；阶段内坐标同源。"""

        self._stages.append([])
        self._current = self._stages[-1]
        self._counter = counter

    def record(self, start: int, end: int, seq: int) -> None:
        """记录一个待替换区间；未显式开始阶段时按无标签阶段处理。"""

        if self._current is None:
            self.begin_stage()
        self._current.append(
            PlanSpan(start=start, end=end, seq=seq, counter=self._counter)
        )
        self._records += 1

    def note_registration(self) -> None:
        """记账一次占位符分配（含无法记录区间的调用，用于判定计划是否完整）。"""

        self.registrations += 1

    def build(self) -> MaskPlan | None:
        """产出计划；记录与实际分配不一致时返回 ``None``（不缓存该文本）。"""

        if self._records != self.registrations:
            return None
        return MaskPlan(stages=tuple(tuple(stage) for stage in self._stages if stage))


def text_key(text: str) -> bytes:
    """文本指纹（SHA-256）：缓存键只用指纹，不保存文本本体。"""

    return hashlib.sha256(text.encode("utf-8", "surrogatepass")).digest()


def _plan_bytes(plan: MaskPlan) -> int:
    """占用估算：指纹键 + 区间对象与阶段列表的固定开销。"""

    return 64 + plan.span_count * 128 + len(plan.stages) * 64


class MaskPlanCache:
    """按文本指纹缓存屏蔽计划的有界 LRU（条目数 + 字节预算双上限）。"""

    def __init__(
        self,
        *,
        max_entries: int = DEFAULT_MAX_ENTRIES,
        max_bytes: int = DEFAULT_MAX_BYTES,
    ) -> None:
        self._max_entries = max(0, int(max_entries))
        self._max_bytes = max(0, int(max_bytes))
        self._lock = threading.Lock()
        self._entries: "OrderedDict[bytes, MaskPlan]" = OrderedDict()
        self._size_bytes = 0
        self.lookups = 0
        self.hits = 0
        self.misses = 0
        self.invalid = 0
        self.evictions = 0

    @property
    def enabled(self) -> bool:
        """任一上限为 0 时整体停用，等价于未接线。"""

        return self._max_entries > 0 and self._max_bytes > 0

    def get(self, text: str) -> MaskPlan | None:
        """取计划；未命中返回 ``None``（调用方走完整屏蔽）。"""

        if not self.enabled or not text:
            return None
        key = text_key(text)
        with self._lock:
            self.lookups += 1
            plan = self._entries.get(key)
            if plan is None:
                self.misses += 1
                return None
            self._entries.move_to_end(key)
            self.hits += 1
            return plan

    def put(self, text: str, plan: MaskPlan) -> None:
        """存计划；超预算的条目直接放弃（不淘汰既有条目）。"""

        if not self.enabled or not text:
            return
        cost = _plan_bytes(plan)
        if cost > self._max_bytes:
            return
        key = text_key(text)
        with self._lock:
            previous = self._entries.pop(key, None)
            if previous is not None:
                self._size_bytes -= _plan_bytes(previous)
            self._entries[key] = plan
            self._size_bytes += cost
            while self._entries and (
                self._size_bytes > self._max_bytes
                or len(self._entries) > self._max_entries
            ):
                _evicted_key, evicted = self._entries.popitem(last=False)
                self._size_bytes -= _plan_bytes(evicted)
                self.evictions += 1

    def note_invalid(self) -> None:
        """命中但重放失败（序号 / 值与计划不符）：按未命中计数，便于观测规则漂移。"""

        with self._lock:
            self.invalid += 1
            self.hits -= 1
            self.misses += 1

    def clear(self) -> None:
        """清空条目（运行时关闭时调用）；计数保留，便于收尾观测。"""

        with self._lock:
            self._entries.clear()
            self._size_bytes = 0

    def stats(self) -> dict[str, int]:
        """返回计数与占用（不含任何原文或计划内容）。"""

        with self._lock:
            return {
                "entries": len(self._entries),
                "size_bytes": self._size_bytes,
                "lookups": self.lookups,
                "hits": self.hits,
                "misses": self.misses,
                "invalid": self.invalid,
                "evictions": self.evictions,
            }


__all__ = [
    "DEFAULT_MAX_BYTES",
    "DEFAULT_MAX_ENTRIES",
    "STAGE_COUNTER_FIELDS",
    "MaskPlan",
    "MaskPlanBuilder",
    "MaskPlanCache",
    "PlanSpan",
    "text_key",
]
