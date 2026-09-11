"""脱敏（Agent 网关消息脱敏）配置：默认关闭，仅显式启用后对出网 AI 消息做可逆脱敏。

对应设计稿 ``omnicrawl/docs/agent_gateway_desensitization_design.md``，落到
config.toml 的 ``[desensitization]`` 段：

.. code-block:: yaml

    [desensitization]
    enabled = false                # opt-in；未启用零成本
    fail_closed = true             # 屏蔽异常时中止请求，不静默发送原文
    strict_restore = false         # 还原缺失时中止并报错（默认保留 + 告警）
    extra_sensitive_keys = []      # 追加敏感键名
    exempt_keys = []               # 豁免键名（优先于命中）
    entropy_enabled = true         # 熵兜底开关（关闭则仅键名 / 结构匹配）
    entropy_min_length = 20        # 熵兜底：长度下限
    entropy_min_bits = 3.5         # 熵兜底：熵阈值
    entropy_pure_letters = false   # 纯字母令牌：长度达标即脱敏（默认关闭）
    entropy_pure_digits = false    # 纯数字令牌：长度达标即脱敏（默认关闭）
"""

from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path
from typing import Any, Mapping

from ..core.runtime import (
    RuntimeConfigError,
    get_section,
    load_config_data,
    save_config_data,
)

DEFAULT_ENTROPY_MIN_LENGTH = 20
DEFAULT_ENTROPY_MIN_BITS = 3.5


class DesensitizationConfigError(RuntimeError):
    """脱敏配置无效或无法写回。"""


@dataclass(frozen=True)
class DesensitizationConfig:
    """脱敏开关、失败策略与匹配参数。

    ``enabled`` 为 False 时运行时不做任何包装（零成本）；``fail_closed`` 为
    True 时屏蔽阶段异常会中止本次请求，而不是退化为发送原文；默认值面向
    opt-in 的保守语义（设计稿 §10.1）。
    """

    enabled: bool = False
    fail_closed: bool = True
    strict_restore: bool = False
    extra_sensitive_keys: tuple[str, ...] = ()
    exempt_keys: tuple[str, ...] = ()
    entropy_enabled: bool = True
    entropy_min_length: int = DEFAULT_ENTROPY_MIN_LENGTH
    entropy_min_bits: float = DEFAULT_ENTROPY_MIN_BITS
    entropy_pure_letters: bool = False
    entropy_pure_digits: bool = False

    def __post_init__(self) -> None:
        for name in (
            "enabled",
            "fail_closed",
            "strict_restore",
            "entropy_enabled",
            "entropy_pure_letters",
            "entropy_pure_digits",
        ):
            if not isinstance(getattr(self, name), bool):
                raise DesensitizationConfigError(f"desensitization.{name} 必须是布尔值。")
        for name in ("extra_sensitive_keys", "exempt_keys"):
            value = getattr(self, name)
            if not isinstance(value, (tuple, list)):
                raise DesensitizationConfigError(f"desensitization.{name} 必须是字符串数组。")
            object.__setattr__(
                self,
                name,
                tuple(str(item).strip() for item in value if str(item).strip()),
            )
        length = self.entropy_min_length
        if isinstance(length, bool) or not isinstance(length, int) or length < 0:
            raise DesensitizationConfigError(
                "desensitization.entropy_min_length 必须是非负整数。"
            )
        bits = self.entropy_min_bits
        if (
            isinstance(bits, bool)
            or not isinstance(bits, (int, float))
            or not 0 <= float(bits) <= 8
        ):
            raise DesensitizationConfigError(
                "desensitization.entropy_min_bits 必须是 0–8 之间的数。"
            )
        object.__setattr__(self, "entropy_min_bits", float(bits))


def load_desensitization_config(
    config_path: str | Path | None = None,
) -> DesensitizationConfig:
    """读取 config.toml 的 ``[desensitization]`` 段；缺失或为空时返回默认关闭配置。"""

    try:
        data = load_config_data(config_path)
        section = get_section(data, "desensitization")
    except RuntimeConfigError as exc:
        raise DesensitizationConfigError(str(exc)) from exc
    return _parse_desensitization_section(section)


def _parse_desensitization_section(section: Mapping[str, Any]) -> DesensitizationConfig:
    return DesensitizationConfig(
        enabled=_bool_field(section, "enabled", False),
        fail_closed=_bool_field(section, "fail_closed", True),
        strict_restore=_bool_field(section, "strict_restore", False),
        extra_sensitive_keys=_key_list_field(section, "extra_sensitive_keys"),
        exempt_keys=_key_list_field(section, "exempt_keys"),
        entropy_enabled=_bool_field(section, "entropy_enabled", True),
        entropy_min_length=_int_field(
            section, "entropy_min_length", DEFAULT_ENTROPY_MIN_LENGTH
        ),
        entropy_min_bits=_float_field(
            section, "entropy_min_bits", DEFAULT_ENTROPY_MIN_BITS
        ),
        entropy_pure_letters=_bool_field(section, "entropy_pure_letters", False),
        entropy_pure_digits=_bool_field(section, "entropy_pure_digits", False),
    )


def save_desensitization_config(
    config: DesensitizationConfig,
    config_path: str | Path | None = None,
) -> Path:
    """把脱敏配置写回 config.toml 的 ``[desensitization]`` 段（保留其他段）。"""

    if not isinstance(config, DesensitizationConfig):
        raise DesensitizationConfigError("脱敏配置对象无效。")
    try:
        data = load_config_data(config_path)
        data["desensitization"] = {
            "enabled": config.enabled,
            "fail_closed": config.fail_closed,
            "strict_restore": config.strict_restore,
            "extra_sensitive_keys": list(config.extra_sensitive_keys),
            "exempt_keys": list(config.exempt_keys),
            "entropy_enabled": config.entropy_enabled,
            "entropy_min_length": config.entropy_min_length,
            "entropy_min_bits": config.entropy_min_bits,
            "entropy_pure_letters": config.entropy_pure_letters,
            "entropy_pure_digits": config.entropy_pure_digits,
        }
        return save_config_data(data, config_path)
    except RuntimeConfigError as exc:
        raise DesensitizationConfigError(str(exc)) from exc


def _bool_field(section: Mapping[str, Any], name: str, default: bool) -> bool:
    value = section.get(name, default)
    if not isinstance(value, bool):
        raise DesensitizationConfigError(f"desensitization.{name} 必须是布尔值。")
    return value


def _key_list_field(section: Mapping[str, Any], name: str) -> tuple[str, ...]:
    raw = section.get(name, ())
    if raw in (None, ""):
        raw = ()
    if not isinstance(raw, (list, tuple)):
        raise DesensitizationConfigError(f"desensitization.{name} 必须是字符串数组。")
    return tuple(str(item).strip() for item in raw if str(item).strip())


def _int_field(section: Mapping[str, Any], name: str, default: int) -> int:
    value = section.get(name, default)
    if isinstance(value, bool) or not isinstance(value, int):
        raise DesensitizationConfigError(f"desensitization.{name} 必须是非负整数。")
    return value


def _float_field(section: Mapping[str, Any], name: str, default: float) -> float:
    value = section.get(name, default)
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        raise DesensitizationConfigError(f"desensitization.{name} 必须是 0–8 之间的数。")
    return float(value)


__all__ = [
    "DEFAULT_ENTROPY_MIN_BITS",
    "DEFAULT_ENTROPY_MIN_LENGTH",
    "DesensitizationConfig",
    "DesensitizationConfigError",
    "load_desensitization_config",
    "save_desensitization_config",
]
