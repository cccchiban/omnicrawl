"""序号注册表：占位符序号分配、发送-接收周期登记与还原查询（设计稿 §6/§7）。

原文只进内存注册表：不落盘、不进日志、不进会话事件与 SSE（§7.1）；可观测信息
只到「计数 / 规则 ID / 序号」粒度（§10.2）。

序号按「值的指纹」稳定分配（§7.2）：同一值在连续请求中始终复用同一序号，使未变
历史脱敏后逐字一致，从而命中提供方前缀缓存；原文在会话内驻留、随会话切换 / 关闭释放。
"""

from __future__ import annotations

import hashlib
import hmac
import itertools
import re
import secrets
import threading
from dataclasses import dataclass, field
from typing import Callable, Collection, Iterable

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

# 稳定序号索引的进程级随机盐：指纹经 HMAC 生成，不存原文、不可反推、也不可跨进程关联。
_STABLE_INDEX_SALT = secrets.token_bytes(32)
_SHARED_STABLE_INDEX_LOCK = threading.Lock()
_SHARED_STABLE_INDEX: "StableSequenceIndex | None" = None


def next_sequence_number() -> int:
    """进程内全局单调递增序号（从 1 开始，作为稳定索引的新值分配来源，§7.2）。"""

    with _SEQUENCE_LOCK:
        return next(_SEQUENCE_COUNTER)


def sequence_fingerprint(value: str) -> str:
    """值 → 进程级指纹（HMAC-SHA256）：仅用于同值判定，不落原文、不可反推。"""

    return hmac.new(
        _STABLE_INDEX_SALT,
        value.encode("utf-8", "surrogatepass"),
        hashlib.sha256,
    ).hexdigest()


class StableSequenceIndex:
    """值指纹 → 序号 的稳定映射（§7.2 前缀缓存保护）。

    前缀缓存的前提是「同一段历史在连续请求中逐字一致」：若每个发送周期都重新分配
    序号，被脱敏的历史会在第一个占位符处整体失配，命中率随历史增长跌到个位数。
    本索引保证同一值永远拿到同一序号：

    - 只保存 HMAC 指纹与整数序号，不保存原文（原文仍只在周期注册表内驻留）；
    - 周期注销（``PlaceholderCycle.close``）不清理本索引，因此跨请求保持稳定；
    - 新值分配仍走进程级单调计数器，序号在进程内不歧义；并发由内部锁串行化。
    """

    def __init__(self, *, sequence_source: Callable[[], int] | None = None) -> None:
        self._lock = threading.Lock()
        self._sequence_source = sequence_source or next_sequence_number
        self._entries: dict[str, int] = {}

    def sequence_for(
        self,
        value: str,
        *,
        reserved: Collection[int] = (),
    ) -> tuple[int, bool]:
        """取值对应序号，返回 ``(seq, reused)``；``reused`` 表示此前已分配过。

        新值分配时跳过 ``reserved``（本请求中已出现的占位符样式序号，§6.3）；已分配
        序号不因本请求出现同号文本而改号——改号会让整段历史失配，代价远大于同号歧义
        （歧义由「还原只查本周期条目」兜底）。
        """

        fingerprint = sequence_fingerprint(value)
        with self._lock:
            existing = self._entries.get(fingerprint)
            if existing is not None:
                return existing, True
            seq = self._sequence_source()
            while seq in reserved:
                seq = self._sequence_source()
            self._entries[fingerprint] = seq
            return seq, False

    @property
    def size(self) -> int:
        """已登记的不同值数量（只到计数粒度，不含任何原文，§10.2）。"""

        with self._lock:
            return len(self._entries)

    def snapshot(self) -> dict[str, int]:
        """返回「值指纹 → 序号」快照（审计 / 测试用；键是指纹，不含原文）。"""

        with self._lock:
            return dict(self._entries)


class SessionSequenceCache:
    """会话级「序号 → 原文」映射（§7.1/§7.2）。

    由会话所有者持有一个实例，同一会话内构建的所有运行时共享它：切换模型会重建
    运行时，但不会丢序号；会话切换（新建 / 恢复）时 ``rebind`` 丢弃上一会话的原文。
    映射只在本进程内存驻留，不落盘、不进日志、不进会话事件（§10.2）。
    """

    def __init__(self, session_id: str = "") -> None:
        self.session_id = session_id
        self.entries: dict[int, str] = {}

    def rebind(self, session_id: str) -> None:
        """会话标识变化 → 丢弃上一会话的映射（会话隔离，§7.2）。"""

        if session_id != self.session_id:
            self.entries.clear()
            self.session_id = session_id

    def clear(self) -> None:
        """会话结束：丢弃全部原文，不落盘、不恢复（§7.3）。"""

        self.entries.clear()


def _shared_stable_index() -> StableSequenceIndex:
    """进程级共享稳定索引：同一值在任何运行时 / 任何请求都拿到同一序号。"""

    global _SHARED_STABLE_INDEX
    with _SHARED_STABLE_INDEX_LOCK:
        if _SHARED_STABLE_INDEX is None:
            _SHARED_STABLE_INDEX = StableSequenceIndex()
        return _SHARED_STABLE_INDEX


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
    sequence_reuses: int = 0
    restore_hits: int = 0
    restore_unresolved: int = 0
    restore_malformed: int = 0
    skipped_values: int = 0
    entropy_masked: int = 0
    # 值类型规则层（PEM / 连接串 / 邮箱 / 银行卡 / IP / URL / MAC / 车牌 / gitleaks）命中数。
    rules_masked: int = 0
    # NER 兜底层（BiLSTM-CRF：人名 / 地名 / 机构名）命中数。
    ner_masked: int = 0
    mask_failures: int = 0
    last_mask_duration_ms: float = 0.0


@dataclass
class PlaceholderCycle:
    """一次发送-接收周期的注册集合：序号 → 原文，同值同号（§5.3/§7）。

    ``entries`` 是**会话级共享**映射：由会话所有者注入各周期（未注入时退化为注册表私有映射），
    序号在会话内不再单次使用，模型把同一序号写多少个、写在文本 / 推理 / 工具参数
    哪个位置，都还原为同一原文；上下文里出现过的旧序号同样有效（§7.2）。
    """

    cycle_id: str
    stable_index: StableSequenceIndex
    # 会话级共享映射：注册表注入同一份实例；单独直接构造（测试）时退化为周期私有。
    entries: dict[int, str] = field(default_factory=dict)
    reserved: set[int] = field(default_factory=set)
    closed: bool = False
    source_request: object | None = None
    masked_request: object | None = None
    stable_reuses: int = 0
    _value_index: dict[str, int] = field(default_factory=dict, repr=False)

    def seq_for_value(self, value: str) -> tuple[int, bool]:
        """返回值对应序号；同值复用同一序号，新值分配并跳过预留序号。

        第二个返回值是「本周期首次登记」的审计口径（§10.2）；跨周期复用同一序号由
        ``stable_reuses`` 单独计数（前缀缓存稳定性指标）。
        """

        existing = self._value_index.get(value)
        if existing is not None:
            return existing, False
        seq, reused = self.stable_index.sequence_for(value, reserved=self.reserved)
        if reused:
            self.stable_reuses += 1
        self._value_index[value] = seq
        self.entries[seq] = value
        return seq, True

    def lookup(self, seq: int) -> str | None:
        """按序号取原文；未注册返回 None（调用方保留占位符 + 告警，§6.2）。"""

        return self.entries.get(seq)

    def adopt(self, value: str, seq: int) -> None:
        """把「本进程已确定过序号」的值登记进本周期（复用缓存屏蔽结果时使用）。

        序号分配结果由进程级稳定索引保证永久一致；缓存只是跳过重复扫描，仍必须把
        (序号 → 原文) 落回本周期还原集合，否则模型回引历史占位符时会落入未注册分支。
        """

        if not value or seq <= 0:
            return
        if self._value_index.get(value) == seq:
            return
        self._value_index[value] = seq
        self.entries[seq] = value
        self.stable_reuses += 1

    def pairs_from(self, start: int) -> tuple[tuple[int, str], ...]:
        """返回 ``start`` 之后新增的 (序号, 值) 对（``entries`` 按登记顺序追加）。"""

        return tuple(itertools.islice(self.entries.items(), start, None))

    def close(self) -> None:
        """结束本周期：只释放周期自身状态，会话级「序号 → 原文」映射保留（§7.3）。

        保留映射是「序号在会话内可反复还原」的前提：后续回复里同一序号出现任意多次、
        或模型回引上下文里出现过的序号，都还原为同一原文。原文只在本进程内存驻留，
        由映射所有者（会话所有者，或未注入时的注册表）统一释放，不落盘、不恢复。
        """

        self._value_index.clear()
        # 结束的周期不再需要请求副本：否则失败 / 取消遗留的周期会各留一份完整历史。
        self.source_request = None
        self.masked_request = None
        self.closed = True


class SequenceRegistry:
    """按周期管理注册表：分配 / 复用 / 还原 / 关闭丢弃（设计稿 §7）。

    「序号 → 原文」映射由本类或注入方（会话所有者）持有并注入各周期：周期结束只释放
    周期自身状态，映射随会话切换 / 关闭释放（§7.2）。
    """

    def __init__(
        self,
        *,
        sequence_source: Callable[[], int] | None = None,
        stable_index: StableSequenceIndex | None = None,
        store_provider: Callable[[], SessionSequenceCache] | None = None,
    ) -> None:
        self._lock = threading.Lock()
        if stable_index is not None:
            self._stable_index = stable_index
        elif sequence_source is not None:
            # 注入自定义计数器（测试 / 旁路定制）→ 私有稳定索引，避免跨注册表串号。
            self._stable_index = StableSequenceIndex(sequence_source=sequence_source)
        else:
            self._stable_index = _shared_stable_index()
        # 「序号 → 原文」映射的来源：注入者（会话所有者）持有则运行时关闭只清周期状态，
        # 序号在同一会话的多个运行时之间共享；未注入时退化为注册表私有映射（§7.2）。
        if store_provider is not None:
            self._store_provider = store_provider
            self._release_store = False
        else:
            private_store = SessionSequenceCache()
            self._store_provider = lambda: private_store
            self._release_store = True
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

            # 只有紧邻的上一个未完成周期才可能被同一逻辑请求的重试复用；请求一旦换新，
            # 它就永远不会再命中，就地注销，避免原文与屏蔽副本随失败 / 取消无限累积。
            if last is not None and not last.closed:
                last.close()
                self._open_cycles.pop(last.cycle_id, None)
            cycle = PlaceholderCycle(
                cycle_id=_next_cycle_id(),
                stable_index=self._stable_index,
                entries=self._store_provider().entries,
            )
            cycle.source_request = request
            self._open_cycles[cycle.cycle_id] = cycle
            self._last_cycle = cycle
            self.stats.cycles_started += 1
            return cycle, False

    def close_cycle(self, cycle: PlaceholderCycle) -> None:
        """周期还原组装结束：释放周期自身状态，会话级序号映射保留（§7.3）。"""

        with self._lock:
            cycle.close()
            self._open_cycles.pop(cycle.cycle_id, None)
            if self._last_cycle is cycle:
                self._last_cycle = None

    def drop_all(self) -> None:
        """运行时关闭：丢弃未完成周期的请求副本；映射由注入方持有（§7.3）。

        会话级映射不随运行时关闭释放（切换模型会重建运行时）；只有注册表自己创建
        的私有映射才在此清空。
        """

        with self._lock:
            for cycle in self._open_cycles.values():
                cycle.close()
            self._open_cycles.clear()
            self._last_cycle = None
            if self._release_store:
                self._store_provider().clear()

    @property
    def open_cycle_count(self) -> int:
        with self._lock:
            return len(self._open_cycles)

    @property
    def stable_index(self) -> StableSequenceIndex:
        """本注册表使用的稳定索引（进程级共享或调用方注入）。"""

        return self._stable_index


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
    "SessionSequenceCache",
    "StableSequenceIndex",
    "collect_placeholder_numbers",
    "format_placeholder",
    "next_sequence_number",
    "sequence_fingerprint",
]
