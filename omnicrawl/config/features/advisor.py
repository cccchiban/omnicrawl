"""Advisor（顾问策略）配置：默认关闭，仅显式启用后注入 advisor 工具。

对应 rpiv-advisor 的持久化选择（advisor.json），落到 config.toml 的 ``[advisor]`` 段：

.. code-block:: yaml

    [advisor]
    enabled = true                  # 显式启用后才把 advisor 工具注册进工具表
    model_key = "deepseek-v4-flash" # 顾问模型：models.toml key/alias 或 profile/model_id
    effort = "high"                 # 推理档位：none/low/medium/high/xhigh/max
    disabled_for_models = []        # executor 黑名单：这些模型运行时剥离 advisor 工具
"""

from __future__ import annotations

from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Mapping

from ..core.runtime import RuntimeConfigError, get_section, load_config_data, save_config_data
from ..models.llm import VALID_REASONING_EFFORTS

DEFAULT_ADVISOR_EFFORT = "high"
ADVISOR_EFFORT_OPTIONS = tuple(
    effort
    for effort in ("none", "low", "medium", "high", "xhigh", "max")
    if effort in VALID_REASONING_EFFORTS
)


class AdvisorConfigError(RuntimeError):
    """Advisor 配置无效或无法写回。"""


@dataclass(frozen=True)
class AdvisorConfig:
    """Advisor 顾问策略开关与模型选择。

    ``enabled`` 为 False 时工具表不注册 advisor 工具，引导提示词也不渲染
    （零成本，与 rpiv issue #72 的“未选模型即剥离”语义一致）。
    ``model_key`` 空串表示未选择顾问模型（等价禁用）。
    ``disabled_for_models`` 列出不应展示 advisor 工具的执行者模型
    （models.toml key/alias 或 profile/model_id 片段匹配）。
    """

    enabled: bool = False
    model_key: str = ""
    effort: str = DEFAULT_ADVISOR_EFFORT
    disabled_for_models: tuple[str, ...] = field(default_factory=tuple)

    @property
    def active(self) -> bool:
        """是否真正可用：显式启用且已选择顾问模型。"""

        return self.enabled and bool((self.model_key or "").strip())

    @property
    def display_effort(self) -> str:
        return self.effort or DEFAULT_ADVISOR_EFFORT

    def __post_init__(self) -> None:
        if not isinstance(self.enabled, bool):
            raise AdvisorConfigError("advisor.enabled 必须是布尔值。")
        if not isinstance(self.model_key, str):
            raise AdvisorConfigError("advisor.model_key 必须是字符串。")
        effort = str(self.effort or "").strip()
        if effort and effort not in VALID_REASONING_EFFORTS:
            allowed = ", ".join(sorted(VALID_REASONING_EFFORTS))
            raise AdvisorConfigError(f"advisor.effort 仅支持 {allowed}，当前值：{effort}。")
        if not isinstance(self.disabled_for_models, (tuple, list)):
            raise AdvisorConfigError("advisor.disabled_for_models 必须是字符串数组。")
        object.__setattr__(
            self,
            "disabled_for_models",
            tuple(str(item).strip() for item in self.disabled_for_models if str(item).strip()),
        )


def load_advisor_config(config_path: str | Path | None = None) -> AdvisorConfig:
    """读取 config.toml 的 ``[advisor]`` 段；缺失或为空时返回默认关闭配置。"""

    try:
        data = load_config_data(config_path)
        section = get_section(data, "advisor")
    except RuntimeConfigError as exc:
        raise AdvisorConfigError(str(exc)) from exc
    return _parse_advisor_section(section)


def _parse_advisor_section(section: Mapping[str, Any]) -> AdvisorConfig:
    enabled = _bool_field(section, "enabled", False)
    model_key = str(section.get("model_key") or "").strip()
    effort_raw = str(section.get("effort") or "").strip()
    effort = effort_raw or DEFAULT_ADVISOR_EFFORT
    raw_disabled = section.get("disabled_for_models", ())
    if raw_disabled in (None, ""):
        raw_disabled = ()
    if not isinstance(raw_disabled, (list, tuple)):
        raise AdvisorConfigError("advisor.disabled_for_models 必须是字符串数组。")
    disabled = tuple(str(item).strip() for item in raw_disabled if str(item).strip())
    return AdvisorConfig(
        enabled=enabled,
        model_key=model_key,
        effort=effort,
        disabled_for_models=disabled,
    )


def save_advisor_config(
    config: AdvisorConfig,
    config_path: str | Path | None = None,
) -> Path:
    """把 advisor 配置写回 config.toml 的 ``[advisor]`` 段（保留其他段）。"""

    if not isinstance(config, AdvisorConfig):
        raise AdvisorConfigError("advisor 配置对象无效。")
    try:
        data = load_config_data(config_path)
        data["advisor"] = {
            "enabled": config.enabled,
            "model_key": config.model_key,
            "effort": config.effort or DEFAULT_ADVISOR_EFFORT,
            "disabled_for_models": list(config.disabled_for_models),
        }
        return save_config_data(data, config_path)
    except RuntimeConfigError as exc:
        raise AdvisorConfigError(str(exc)) from exc


def clear_advisor_config(config_path: str | Path | None = None) -> Path:
    """清除 advisor 选择：置 enabled=False 并清空 model_key。"""

    return save_advisor_config(AdvisorConfig(enabled=False), config_path)


def _bool_field(section: Mapping[str, Any], name: str, default: bool) -> bool:
    value = section.get(name, default)
    if not isinstance(value, bool):
        raise AdvisorConfigError(f"advisor.{name} 必须是布尔值。")
    return value


__all__ = [
    "AdvisorConfig",
    "AdvisorConfigError",
    "ADVISOR_EFFORT_OPTIONS",
    "DEFAULT_ADVISOR_EFFORT",
    "clear_advisor_config",
    "load_advisor_config",
    "save_advisor_config",
]
