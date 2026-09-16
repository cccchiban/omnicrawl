"""视觉模型代理配置的读取、校验与写回。"""

from __future__ import annotations

from dataclasses import dataclass, replace
from pathlib import Path
from typing import Any, Mapping

from ...llm.protocol import SUPPORTED_PROTOCOLS
from .llm import ActiveModelRef
from ..core.runtime import (
    RuntimeConfigError,
    load_config_data,
    resolve_models_path,
    save_config_data,
)
from .model_store import ModelStoreError, load_model_store, save_model_store


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


NATIVE_VISION_FIELD = "native_vision"


@dataclass(frozen=True)
class NativeVisionSetting:
    """当前模型「模型原生视觉」的覆盖值与来源。"""

    value: bool | None = None
    scope: str = ""
    label: str = ""

    @property
    def scope_text(self) -> str:
        if self.scope == "model":
            return f"模型设置（{self.label}）"
        if self.scope == "channel":
            return f"渠道设置（{self.label}）"
        return "未配置（按模型能力）"


def parse_native_vision(raw: Any) -> bool | None:
    """把 TOML 取值解析为三态开关；缺失或非法一律视为未配置。"""

    return raw if isinstance(raw, bool) else None


def resolve_native_vision(
    *,
    catalog_key: str = "",
    profile_id: str = "",
    config_path: str | Path | None = None,
    models_path: str | Path | None = None,
) -> NativeVisionSetting:
    """按「模型覆盖 > 渠道覆盖 > 未配置」读取当前模型的模型原生视觉设置。"""

    model_value = _read_model_native_vision(catalog_key, models_path)
    if model_value is not None:
        return NativeVisionSetting(value=model_value, scope="model", label=catalog_key)
    channel_value = _read_channel_native_vision(profile_id, config_path)
    if channel_value is not None:
        return NativeVisionSetting(value=channel_value, scope="channel", label=profile_id)
    return NativeVisionSetting(label=catalog_key or profile_id)


def save_native_vision(
    value: bool | None,
    *,
    scope: str,
    catalog_key: str = "",
    profile_id: str = "",
    config_path: str | Path | None = None,
    models_path: str | Path | None = None,
) -> Path:
    """把模型原生视觉开关写回模型或渠道 TOML；``None`` 表示删除该键。"""

    if value is not None and not isinstance(value, bool):
        raise VisionConfigError("模型原生视觉必须是布尔值或未配置。")
    if scope == "model":
        if not str(catalog_key or "").strip():
            raise VisionConfigError("缺少模型 key，无法写入模型原生视觉。")
        return _write_model_native_vision(catalog_key, value, models_path)
    if scope == "channel":
        if not str(profile_id or "").strip():
            raise VisionConfigError("缺少渠道标识，无法写入模型原生视觉。")
        return _write_channel_native_vision(profile_id, value, config_path)
    raise VisionConfigError("模型原生视觉必须写入范围仅支持 model 或 channel。")


def _read_model_native_vision(
    catalog_key: str,
    models_path: str | Path | None,
) -> bool | None:
    key = str(catalog_key or "").strip()
    if not key:
        return None
    store = _load_native_vision_store(models_path)
    record = store.by_key().get(key)
    if record is None:
        return None
    return parse_native_vision(getattr(record, NATIVE_VISION_FIELD, None))


def _read_channel_native_vision(
    profile_id: str,
    config_path: str | Path | None,
) -> bool | None:
    key = str(profile_id or "").strip()
    if not key:
        return None
    profile = _channel_profile(_load_native_vision_data(config_path), key)
    if profile is None:
        return None
    return parse_native_vision(profile.get(NATIVE_VISION_FIELD))


def _write_model_native_vision(
    catalog_key: str,
    value: bool | None,
    models_path: str | Path | None,
) -> Path:
    key = str(catalog_key or "").strip()
    store = _load_native_vision_store(models_path)
    records = tuple(
        replace(item, **{NATIVE_VISION_FIELD: value}) if item.key == key else item
        for item in store.models
    )
    if all(item.key != key for item in store.models):
        raise VisionConfigError(f"models.toml 中没有模型 {key}。")
    try:
        return save_model_store(replace(store, models=records), models_path)
    except (ModelStoreError, RuntimeConfigError, OSError) as exc:
        raise VisionConfigError(str(exc)) from exc


def _write_channel_native_vision(
    profile_id: str,
    value: bool | None,
    config_path: str | Path | None,
) -> Path:
    key = str(profile_id or "").strip()
    data = _load_native_vision_data(config_path)
    profile = _channel_profile(data, key)
    if profile is None:
        raise VisionConfigError(f"config.toml 的 llm.profiles 中没有渠道 {key}。")
    if value is None:
        profile.pop(NATIVE_VISION_FIELD, None)
    else:
        profile[NATIVE_VISION_FIELD] = value
    try:
        return save_config_data(data, config_path)
    except (RuntimeConfigError, OSError) as exc:
        raise VisionConfigError(str(exc)) from exc


def _load_native_vision_data(config_path: str | Path | None = None) -> dict[str, Any]:
    """读取整份 config.toml 数据；失败时按视觉配置错误上报。"""

    try:
        data = load_config_data(config_path)
    except (RuntimeConfigError, OSError) as exc:
        raise VisionConfigError(str(exc)) from exc
    return data if isinstance(data, dict) else {}


def _load_native_vision_store(models_path: str | Path | None):
    try:
        return load_model_store(resolve_models_path(models_path))
    except (ModelStoreError, RuntimeConfigError, OSError) as exc:
        raise VisionConfigError(str(exc)) from exc


def _channel_profile(data: Mapping[str, Any], profile_id: str) -> dict[str, Any] | None:
    llm = data.get("llm")
    profiles = llm.get("profiles") if isinstance(llm, Mapping) else None
    profile = profiles.get(profile_id) if isinstance(profiles, Mapping) else None
    return profile if isinstance(profile, dict) else None


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
    "NATIVE_VISION_FIELD",
    "NativeVisionSetting",
    "load_vision_configuration",
    "parse_native_vision",
    "resolve_native_vision",
    "save_native_vision",
    "save_vision_configuration",
]
