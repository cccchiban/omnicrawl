"""视觉模型代理配置的读取、校验与写回。"""

from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path
from typing import Any, Mapping

from ...llm.protocol import SUPPORTED_PROTOCOLS
from .llm import ActiveModelRef
from ..core.runtime import RuntimeConfigError, load_config_data, save_config_data


class VisionConfigError(RuntimeError):
    """视觉模型代理配置无效或无法写回。"""


@dataclass(frozen=True)
class VisionConfiguration:
    """视觉模型代理的开关和有序故障转移模型引用。"""

    enabled: bool = False
    models: tuple[ActiveModelRef, ...] = ()

    def __post_init__(self) -> None:
        if not isinstance(self.enabled, bool):
            raise VisionConfigError("vision.enabled 必须是布尔值。")
        seen: set[tuple[tuple[str, str], ...]] = set()
        for index, ref in enumerate(self.models):
            if not isinstance(ref, ActiveModelRef):
                raise VisionConfigError(f"vision.models[{index}] 不是有效的模型引用。")
            identity = tuple(sorted(ref.to_dict().items()))
            if identity in seen:
                raise VisionConfigError(f"vision.models[{index}] 与前面的模型重复。")
            seen.add(identity)

    @property
    def model_refs(self) -> tuple[ActiveModelRef, ...]:
        """兼容调用方使用更明确的有序引用名称。"""

        return self.models


def load_vision_configuration(
    config_path: str | Path | None = None,
) -> VisionConfiguration:
    """读取视觉代理配置；缺少 ``vision`` 段时保持关闭。"""

    try:
        data = load_config_data(config_path)
        raw_section = data.get("vision", {})
        if raw_section in (None, ""):
            raw_section = {}
        if not isinstance(raw_section, Mapping):
            raise VisionConfigError("配置段 vision 必须是对象。")
        enabled = raw_section.get("enabled", False)
        models = _parse_model_refs(raw_section.get("models", []))
        return VisionConfiguration(enabled=enabled, models=models)
    except VisionConfigError:
        raise
    except RuntimeConfigError as exc:
        raise VisionConfigError(str(exc)) from exc


def save_vision_configuration(
    configuration: VisionConfiguration,
    config_path: str | Path | None = None,
) -> Path:
    """保留其他配置段，只更新完整的视觉代理配置。"""

    if not isinstance(configuration, VisionConfiguration):
        raise VisionConfigError("视觉代理配置对象无效。")
    try:
        data = load_config_data(config_path)
        data["vision"] = {
            "enabled": configuration.enabled,
            "models": [ref.to_dict() for ref in configuration.models],
        }
        return save_config_data(data, config_path)
    except RuntimeConfigError as exc:
        raise VisionConfigError(str(exc)) from exc


def _parse_model_refs(raw: Any) -> tuple[ActiveModelRef, ...]:
    if raw in (None, ""):
        return ()
    if not isinstance(raw, list):
        raise VisionConfigError("配置项 vision.models 必须是列表。")

    refs: list[ActiveModelRef] = []
    seen: set[tuple[tuple[str, str], ...]] = set()
    for index, item in enumerate(raw):
        if not isinstance(item, Mapping):
            raise VisionConfigError(f"配置项 vision.models[{index}] 必须是对象。")
        source = str(item.get("source") or "").strip().lower()
        if source == "custom":
            key = str(item.get("key") or "").strip()
            if not key:
                raise VisionConfigError(f"配置项 vision.models[{index}] 缺少 key。")
            ref = ActiveModelRef(source="custom", key=key)
        elif source == "detected":
            profile = str(item.get("profile") or "").strip()
            model_id = str(item.get("model_id") or "").strip()
            protocol = str(item.get("protocol") or "").strip()
            if not profile or not model_id:
                raise VisionConfigError(
                    f"配置项 vision.models[{index}] 必须包含 profile 和 model_id。"
                )
            if protocol and protocol not in SUPPORTED_PROTOCOLS:
                raise VisionConfigError(
                    f"配置项 vision.models[{index}].protocol 不支持：{protocol}"
                )
            ref = ActiveModelRef(
                source="detected",
                profile=profile,
                model_id=model_id,
                protocol=protocol,
            )
        else:
            raise VisionConfigError(
                f"配置项 vision.models[{index}].source 仅支持 custom 或 detected。"
            )
        identity = tuple(sorted(ref.to_dict().items()))
        if identity in seen:
            raise VisionConfigError(f"配置项 vision.models[{index}] 与前面的模型重复。")
        seen.add(identity)
        refs.append(ref)
    return tuple(refs)


__all__ = [
    "VisionConfigError",
    "VisionConfiguration",
    "load_vision_configuration",
    "save_vision_configuration",
]
