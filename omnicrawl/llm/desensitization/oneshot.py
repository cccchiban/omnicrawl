"""旁路调用的一次性文本屏蔽 / 还原（设计稿 §4.4：审批自动审查等非 Runtime 链路）。

不经统一运行时的模型调用（如审批自动审查直接走 Responses API）同样需要
「出站屏蔽 + 入站还原」。本模块把核心引擎（匹配、序号注册表、还原状态机）
编排为单次使用的 ``OneShotMasker``：屏蔽 → 发送 → 还原 → 注销，生命周期最短
（§7.3 单次使用语义）；未启用或配置不可读时调用方拿到的脱敏器为 None，零改动。
"""

from __future__ import annotations

from pathlib import Path
from typing import Callable

from ...config.features.desensitization import (
    DesensitizationConfig,
    load_desensitization_config,
)
from .engine import MaskContext, SensitiveMatcher, mask_text
from .ner import NerLayer, build_ner_layer
from .registry import (
    DesensitizationStats,
    PlaceholderCycle,
    SequenceRegistry,
    collect_placeholder_numbers,
)
from .rules import PatternRule, build_enabled_rules
from .stream import StreamRestorer


class OneShotMasker:
    """单次「屏蔽 → 还原」调用的脱敏器（一个实例对应一次出站请求）。

    与运行时装饰器共用引擎与注册表；``mask()`` 建立周期并注册序号，
    ``restore()`` 做整段文本还原，``close()`` 注销本周期全部序号。
    """

    def __init__(
        self,
        config: DesensitizationConfig,
        *,
        matcher: SensitiveMatcher | None = None,
        sequence_source: Callable[[], int] | None = None,
    ) -> None:
        self._config = config
        self._matcher = matcher or SensitiveMatcher(
            extra_keys=config.extra_sensitive_keys,
            exempt_keys=config.exempt_keys,
        )
        self._stats = DesensitizationStats()
        self._rules: tuple[PatternRule, ...] = build_enabled_rules(config)
        # NER 兜底层：与运行时装饰器共用构建入口与共享抽取器（同一模型只加载一次）。
        self._ner_layer: NerLayer | None = build_ner_layer(config)
        self._registry = SequenceRegistry(sequence_source=sequence_source)
        self._cycle: PlaceholderCycle | None = None

    @property
    def config(self) -> DesensitizationConfig:
        """本次屏蔽使用的配置（调用方据 ``fail_closed`` 选择失败策略）。"""

        return self._config

    def mask(self, text: str) -> str:
        """屏蔽文本并登记本次周期；返回替换后的文本。"""

        cycle, _ = self._registry.begin_cycle(text)
        cycle.reserved = collect_placeholder_numbers([text])
        context = MaskContext(
            matcher=self._matcher,
            cycle=cycle,
            stats=self._stats,
            entropy_enabled=self._config.entropy_enabled,
            entropy_min_length=self._config.entropy_min_length,
            entropy_min_bits=self._config.entropy_min_bits,
            entropy_pure_letters=self._config.entropy_pure_letters,
            entropy_pure_digits=self._config.entropy_pure_digits,
            pattern_rules=self._rules,
            ner_layer=self._ner_layer,
        )
        masked = mask_text(text, context)
        # 稳定序号复用计数（只到计数粒度，§10.2）。
        self._stats.sequence_reuses += cycle.stable_reuses
        self._cycle = cycle
        return masked

    def restore(self, text: str) -> str:
        """还原文本中的完整占位符；未注册序号保留原样（严格模式抛错）。"""

        if self._cycle is None or not text:
            return text
        restorer = StreamRestorer(
            self._cycle,
            self._stats,
            strict=self._config.strict_restore,
        )
        return restorer.restore_string(text)

    def close(self) -> None:
        """注销本周期全部序号（单次使用、不落盘、不可恢复，§7.3）。"""

        self._registry.drop_all()
        self._cycle = None


def maybe_create_oneshot_masker(
    config_path: str | Path | None = None,
) -> OneShotMasker | None:
    """按 ``[desensitization]`` 配置创建脱敏器；未启用或配置不可读时返回 None。"""

    try:
        config = load_desensitization_config(config_path)
        if not config.enabled:
            return None
        return OneShotMasker(config)
    except Exception:
        # 配置读取 / 构造失败不得影响旁路调用；脱敏默认关闭（与 maybe_wrap_runtime 一致）。
        return None


__all__ = ["OneShotMasker", "maybe_create_oneshot_masker"]
