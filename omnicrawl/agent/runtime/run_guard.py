"""Agent 运行节奏护栏的纯逻辑实现。

本模块不依赖 Provider、UI 或 Session。reasoning 流只需要把每个增量交给
``create_reasoning_guard``，检测器即可按单次模型调用维护精确的滑动窗口统计。
协议层使用 ``ReasoningGuardTriggered`` 和 ``ConfiguredAutoRetryError`` 把
检测结果转换成可控、可审计的回合级重试。
"""

from __future__ import annotations

from collections import Counter
from contextvars import ContextVar, Token
from dataclasses import dataclass
from threading import Event
from typing import Any, Callable, Iterable


REASONING_GUARD_CODE = "REASONING_GUARD"
_PAUSE_EVENT: ContextVar[Event | None] = ContextVar(
    "omnicrawl_run_guard_pause_event",
    default=None,
)


@dataclass(frozen=True)
class ReasoningGuardVerdict:
    """一次检查的不可变结果。"""

    triggered: bool
    blocks: int
    chars: int
    reason: str = ""
    ratio: float | None = None


class ReasoningGuardTriggered(RuntimeError):
    """reasoning 护栏触发，当前模型流必须丢弃并按回合预算重试。"""

    code = REASONING_GUARD_CODE

    def __init__(self, verdict: ReasoningGuardVerdict, message: str) -> None:
        super().__init__(message)
        self.verdict = verdict
        self.message = message


class RunGuardPaused(RuntimeError):
    """当前回合已主动暂停，禁止 Guard 自动重试或 Continue。"""


class ConfiguredAutoRetryError(RuntimeError):
    """命中 run_guard.guard.auto_retry_errors 的上游请求错误。"""

    def __init__(self, code: str, message: str, *, cause: BaseException | None = None) -> None:
        super().__init__(message)
        self.code = code
        self.message = message
        if cause is not None:
            self.__cause__ = cause


@dataclass
class GuardRetryState:
    """一个 Agent 回合共享的护栏/白名单错误重试计数。"""

    max_retries: int
    used: int = 0

    def consume(self) -> int | None:
        """消耗一次重试额度，返回 1-based 次数；没有额度时返回 None。"""

        if self.used >= self.max_retries:
            return None
        self.used += 1
        return self.used


class _ReasoningGuard:
    """维护精确滑动窗口与子串频率多重集的检测器。"""

    def __init__(
        self,
        *,
        substr_len: int,
        window_chars: int,
        repeat_ratio: float,
        check_every: int,
        max_blocks: int,
        max_chars: int,
    ) -> None:
        self.substr_len = int(substr_len)
        self.window_chars = int(window_chars)
        self.repeat_ratio = float(repeat_ratio)
        self.check_every = int(check_every)
        self.max_blocks = int(max_blocks)
        self.max_chars = int(max_chars)
        self._window = ""
        self._frequency: Counter[str] = Counter()
        self._total_substrings = 0
        self._blocks = 0
        self._chars = 0
        self._since_check = 0

    def push(self, text: str) -> ReasoningGuardVerdict | None:
        """加入一个 reasoning 增量，在检查边界返回判断结果。"""

        if not isinstance(text, str):
            raise TypeError("reasoning 增量必须是字符串。")
        # 空增量也算一个 reasoning block；真实 Provider 可能切出很短的 SSE
        # 分片，不能用“长度足够”作为是否进入统计的条件。
        self._blocks += 1
        self._chars += len(text)
        self._add_substrings(text)
        self._trim_window()
        self._since_check += 1
        if self._since_check < self.check_every:
            return None
        self._since_check = 0
        return self._check()

    def stats(self) -> dict[str, int]:
        """返回当前窗口与累计计数，用于诊断和单元测试。"""

        return {
            "blocks": self._blocks,
            "chars": self._chars,
            "total_substrings": self._total_substrings,
            "unique": len(self._frequency),
        }

    def _add_substrings(self, text: str) -> None:
        combined = self._window + text
        # 旧窗口中已完整存在的子串起点不重复加入；从窗口尾部可能跨越
        # 边界的起点开始，保证短分片的跨块子串也被精确统计。
        start = max(0, len(self._window) - self.substr_len + 1)
        for index in range(start, len(combined) - self.substr_len + 1):
            substring = combined[index : index + self.substr_len]
            self._frequency[substring] += 1
            self._total_substrings += 1
        self._window = combined

    def _trim_window(self) -> None:
        while len(self._window) > self.window_chars:
            # 当 window_chars 小于 substr_len 时，窗口会从“有完整子串”
            # 退化到“完全没有完整子串”。窗口长度降到 substr_len 以下后，
            # 统计表中剩余的条目也已经全部离开窗口，必须整体清空，不能只
            # 依赖逐字符删除起点为 0 的子串。
            if len(self._window) >= self.substr_len:
                substring = self._window[: self.substr_len]
                current = self._frequency.get(substring, 0)
                if current <= 1:
                    self._frequency.pop(substring, None)
                else:
                    self._frequency[substring] = current - 1
                self._total_substrings = max(0, self._total_substrings - 1)
            self._window = self._window[1:]
        if len(self._window) < self.substr_len:
            self._frequency.clear()
            self._total_substrings = 0

    def _check(self) -> ReasoningGuardVerdict:
        ratio: float | None = None
        if self._total_substrings >= self.substr_len * 2:
            ratio = 1.0 - len(self._frequency) / self._total_substrings
            if ratio >= self.repeat_ratio:
                return ReasoningGuardVerdict(
                    triggered=True,
                    blocks=self._blocks,
                    chars=self._chars,
                    reason="repeat",
                    ratio=ratio,
                )
        if self._blocks >= self.max_blocks:
            return ReasoningGuardVerdict(
                triggered=True,
                blocks=self._blocks,
                chars=self._chars,
                reason="blocks",
                ratio=ratio,
            )
        if self._chars >= self.max_chars:
            return ReasoningGuardVerdict(
                triggered=True,
                blocks=self._blocks,
                chars=self._chars,
                reason="chars",
                ratio=ratio,
            )
        return ReasoningGuardVerdict(
            triggered=False,
            blocks=self._blocks,
            chars=self._chars,
            ratio=ratio,
        )


def activate_pause_event(event: Event) -> Token[Event | None]:
    """把当前 Agent 回合的暂停信号放入可复制到工具线程的上下文。"""

    return _PAUSE_EVENT.set(event)


def reset_pause_event(token: Token[Event | None]) -> None:
    """恢复工具调用前的上下文，避免后续回合继承暂停标志。"""

    _PAUSE_EVENT.reset(token)


def mark_pause_requested() -> bool:
    """标记当前回合主动暂停；没有活动回合时返回 False。"""

    event = _PAUSE_EVENT.get()
    if event is None:
        return False
    event.set()
    return True


def pause_requested() -> bool:
    """返回当前回合是否已由 ``pause_work`` 请求暂停。"""

    event = _PAUSE_EVENT.get()
    return event is not None and event.is_set()


def create_reasoning_guard(config: Any) -> _ReasoningGuard:
    """按配置创建单次模型调用的 reasoning 检测器。"""

    return _ReasoningGuard(
        substr_len=int(getattr(config, "substr_len")),
        window_chars=int(getattr(config, "window_chars")),
        repeat_ratio=float(getattr(config, "repeat_ratio")),
        check_every=int(getattr(config, "check_every")),
        max_blocks=int(getattr(config, "max_blocks")),
        max_chars=int(getattr(config, "max_chars")),
    )


def guard_message(verdict: ReasoningGuardVerdict, config: Any) -> str:
    """生成不含敏感信息的用户可读护栏说明。"""

    _ = config
    ratio_text = "n/a" if verdict.ratio is None else f"{verdict.ratio:.3f}"
    reason_text = {
        "repeat": "重复率异常：模型持续输出高度重复的推理内容（疑似推理死循环）",
        "blocks": "单次调用推理块数超过上限",
        "chars": "单次调用推理字符数超过上限",
    }.get(verdict.reason, verdict.reason or "未知原因")
    lines = [
        "推理输出疑似死循环，已被推理护栏中断。",
        f"原因：{reason_text}",
        f"触发时状态：{verdict.blocks} 个推理块 / {verdict.chars} 字符"
        + ("" if verdict.ratio is None else f"，窗口重复率 {ratio_text}"),
        "这是保护性中断：模型可能在空转输出，继续会浪费时间和 Token。"
        "系统会按护栏重试预算自动重试；若仍失败，可直接继续对话。",
    ]
    return "\n".join(lines)


def wrap_reasoning_callback(
    callback: Callable[[str], None] | None,
    config: Any | None,
) -> Callable[[str], None] | None:
    """把 detector 接到 reasoning 回调；检测器异常时降级为透传。"""

    if config is None or not bool(getattr(config, "enabled", False)):
        return callback
    try:
        detector: _ReasoningGuard | None = create_reasoning_guard(config)
    except Exception:
        # 配置通常在 Agent 启动时已校验；直连协议测试或第三方调用即使传入
        # 不完整对象，也不能因为护栏自身初始化失败而阻断健康模型流。
        detector = None

    def guarded(text: str) -> None:
        nonlocal detector
        if detector is not None:
            try:
                verdict = detector.push(text)
                if verdict is not None and verdict.triggered:
                    raise ReasoningGuardTriggered(
                        verdict,
                        guard_message(verdict, config),
                    )
            except ReasoningGuardTriggered:
                raise
            except Exception:
                # 与参考插件一致：检测器内部缺陷只让本次护栏降级，不伤害正常流。
                detector = None
        if callback is not None:
            callback(text)

    return guarded


def _exception_chain(exc: BaseException) -> Iterable[BaseException]:
    """遍历有限异常因果链，兼容 Provider 包装和异常上下文循环。"""

    current: BaseException | None = exc
    seen: set[int] = set()
    for _ in range(5):
        if current is None or id(current) in seen:
            return
        seen.add(id(current))
        yield current
        current = current.__cause__ or current.__context__


def _candidate_codes(exc: BaseException) -> Iterable[str]:
    """从异常自身和有限因果链提取可配置的上游错误码。"""

    for current in _exception_chain(exc):
        for attribute in ("code", "error_code", "failure_code"):
            value = getattr(current, attribute, None)
            if value is not None:
                code = getattr(value, "value", value)
                if isinstance(code, str) and code.strip():
                    yield code.strip()


def configured_retry_code(exc: BaseException, config: Any | None) -> str | None:
    """返回异常命中的 auto_retry_errors 错误码。"""

    if config is None:
        return None
    allowed = {
        str(code).strip()
        for code in (getattr(config, "auto_retry_errors", ()) or ())
        if str(code).strip()
    }
    if not allowed:
        return None
    for code in _candidate_codes(exc):
        if code in allowed:
            return code
    # Runtime Provider 通常会把原始 SDK 异常包装成 ModelError；上游业务码
    # 可能只存在于因果链异常文本中（例如网关返回 PI_AI_ERROR）。只在用户
    # 明确配置的白名单中做匹配，并限制因果链深度，避免把普通错误误判为可重试。
    for candidate in _exception_chain(exc):
        text = str(candidate)
        for code in allowed:
            if code and code in text:
                return code
    return None


__all__ = [
    "ConfiguredAutoRetryError",
    "GuardRetryState",
    "RunGuardPaused",
    "REASONING_GUARD_CODE",
    "ReasoningGuardTriggered",
    "ReasoningGuardVerdict",
    "create_reasoning_guard",
    "configured_retry_code",
    "guard_message",
    "wrap_reasoning_callback",
    "activate_pause_event",
    "mark_pause_requested",
    "pause_requested",
    "reset_pause_event",
]
