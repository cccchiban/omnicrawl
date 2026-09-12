"""Agent 网关消息脱敏（可逆占位符 ｛Desensitized:n｝）。

模块划分：engine（匹配引擎）/ registry（序号注册表与周期）/ stream（事件流
还原）/ middleware（运行时装饰器与接线助手）/ oneshot（旁路一次性屏蔽还原）。
设计稿见 ``omnicrawl/docs/agent_gateway_desensitization_design.md``。
"""

from .engine import (
    MaskContext,
    SensitiveMatcher,
    find_entropy_spans,
    is_entropy_candidate,
    is_entropy_exempt,
    mask_structured_value,
    mask_text,
    shannon_entropy_bits,
)
from .middleware import DesensitizationError, DesensitizationRuntime, maybe_wrap_runtime
from .oneshot import OneShotMasker, maybe_create_oneshot_masker
from .registry import (
    DesensitizationStats,
    PlaceholderCycle,
    SequenceRegistry,
    StableSequenceIndex,
    collect_placeholder_numbers,
    format_placeholder,
)
from .stream import StreamRestorer

__all__ = [
    "DesensitizationError",
    "DesensitizationRuntime",
    "DesensitizationStats",
    "MaskContext",
    "OneShotMasker",
    "PlaceholderCycle",
    "SensitiveMatcher",
    "SequenceRegistry",
    "StableSequenceIndex",
    "StreamRestorer",
    "collect_placeholder_numbers",
    "find_entropy_spans",
    "format_placeholder",
    "is_entropy_candidate",
    "is_entropy_exempt",
    "mask_structured_value",
    "mask_text",
    "maybe_create_oneshot_masker",
    "maybe_wrap_runtime",
    "shannon_entropy_bits",
]
