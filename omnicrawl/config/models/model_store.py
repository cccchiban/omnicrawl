"""models.toml 读取、校验与写回。"""

from __future__ import annotations

import re
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Mapping

from ...llm.capabilities import ModelCapabilities, capabilities_from_mapping
from ...llm.protocol import (
    PROVIDER_ANTHROPIC,
    PROVIDER_GEMINI,
    PROVIDER_OPENAI,
    SUPPORTED_PROTOCOLS,
    ModelIdentity,
)
from ...llm.registry import ModelDescriptor
from ..core.runtime import (
    RuntimeConfigError,
    atomic_write_text,
    dump_toml_text,
    load_raw_file,
    resolve_models_path,
    resolve_models_write_path,
)


_MODEL_KEY_RE = re.compile(r"^[a-z0-9][a-z0-9_-]*$")


class ModelStoreError(RuntimeError):
    """models.toml 校验或读写失败。"""


@dataclass(frozen=True)
class CustomModelRecord:
    key: str
    display_name: str
    profile: str
    model_id: str
    protocol: str
    enabled: bool = True
    aliases: tuple[str, ...] = ()
    description: str = ""
    tags: tuple[str, ...] = ()
    context_window_tokens: int = 0
    max_output_tokens: int = 0
    temperature: float | None = None
    native_vision: bool | None = None
    capabilities: ModelCapabilities = field(default_factory=ModelCapabilities)
    provider_options: Mapping[str, Any] = field(default_factory=dict)
    sort_order: int = 0

    def to_descriptor(self, *, provider: str) -> ModelDescriptor:
        return ModelDescriptor(
            identity=ModelIdentity(
                profile_id=self.profile,
                provider=provider,
                protocol=self.protocol,
                model_id=self.model_id,
                catalog_key=self.key,
            ),
            display_name=self.display_name or self.key,
            capabilities=self.capabilities,
            context_window_tokens=self.context_window_tokens,
            max_output_tokens=self.max_output_tokens,
            temperature=self.temperature,
            aliases=self.aliases,
            description=self.description,
            tags=self.tags,
            provider_options=dict(self.provider_options),
            source="custom",
            sort_order=self.sort_order,
            enabled=self.enabled,
        )


@dataclass(frozen=True)
class ModelStore:
    version: int
    models: tuple[CustomModelRecord, ...]
    path: Path | None = None

    def by_key(self) -> dict[str, CustomModelRecord]:
        return {item.key: item for item in self.models}

    def resolve_alias(self, token: str) -> CustomModelRecord | None:
        needle = token.strip()
        if not needle:
            return None
        by_key = self.by_key()
        if needle in by_key:
            return by_key[needle]
        matches = [item for item in self.models if needle in item.aliases]
        if len(matches) == 1:
            return matches[0]
        if len(matches) > 1:
            keys = ", ".join(item.key for item in matches)
            raise ModelStoreError(f"别名 {needle} 匹配到多个模型：{keys}")
        return None


def load_model_store(models_path: str | Path | None = None) -> ModelStore:
    path = resolve_models_path(models_path)
    if not path.exists():
        return ModelStore(version=1, models=(), path=path)
    try:
        data = load_raw_file(path)
    except RuntimeConfigError as exc:
        raise ModelStoreError(str(exc)) from exc
    return parse_model_store(data, path=path)


def parse_model_store(data: Mapping[str, Any], *, path: Path | None = None) -> ModelStore:
    if not isinstance(data, Mapping):
        raise ModelStoreError("models.toml 顶层必须是对象。")
    version = data.get("version", 1)
    if isinstance(version, bool) or not isinstance(version, int) or version < 1:
        raise ModelStoreError("models.toml 的 version 必须是正整数。")

    raw_models = data.get("models", {})
    if raw_models in (None, ""):
        raw_models = {}
    if not isinstance(raw_models, dict):
        raise ModelStoreError("models.toml 的 models 必须是对象。")

    records: list[CustomModelRecord] = []
    alias_owners: dict[str, str] = {}
    for key, raw in raw_models.items():
        record = _parse_model_record(str(key), raw)
        # 凭据字段禁止出现
        if isinstance(raw, Mapping):
            for forbidden in ("api_key", "token", "cookie", "authorization"):
                if forbidden in raw:
                    raise ModelStoreError(
                        f"models.toml 模型 {key} 不允许包含凭据字段 {forbidden}。"
                    )
        for alias in record.aliases:
            owner = alias_owners.get(alias)
            if owner and owner != record.key:
                raise ModelStoreError(
                    f"models.toml 别名冲突：{alias} 同时属于 {owner} 与 {record.key}。"
                )
            alias_owners[alias] = record.key
        records.append(record)

    records.sort(key=lambda item: (item.sort_order, item.display_name.lower(), item.key))
    return ModelStore(version=version, models=tuple(records), path=path)


def save_model_store(store: ModelStore, models_path: str | Path | None = None) -> Path:
    path = resolve_models_write_path(models_path)
    payload: dict[str, Any] = {"version": store.version, "models": {}}
    for record in store.models:
        item: dict[str, Any] = {
            "display_name": record.display_name,
            "profile": record.profile,
            "model_id": record.model_id,
            "protocol": record.protocol,
            "enabled": record.enabled,
        }
        if record.aliases:
            item["aliases"] = list(record.aliases)
        if record.description:
            item["description"] = record.description
        if record.tags:
            item["tags"] = list(record.tags)
        if record.context_window_tokens > 0:
            item["context_window_tokens"] = record.context_window_tokens
        if record.max_output_tokens > 0:
            item["max_output_tokens"] = record.max_output_tokens
        if record.temperature is not None:
            item["temperature"] = record.temperature
        if record.native_vision is not None:
            item["native_vision"] = record.native_vision
        caps = record.capabilities.to_dict()
        # 只写非默认能力，保持文件简洁
        meaningful = {
            k: v
            for k, v in caps.items()
            if v not in (False, 0, "", None)
            or k in {"streaming", "tools"} and v is True
        }
        if meaningful:
            item["capabilities"] = {
                k: v
                for k, v in caps.items()
                if k in meaningful or k in {"streaming", "tools", "reasoning", "vision", "parallel_tool_calls"}
            }
        if record.provider_options:
            item["provider_options"] = dict(record.provider_options)
        if record.sort_order:
            item["sort_order"] = record.sort_order
        payload["models"][record.key] = item
    atomic_write_text(path, dump_toml_text(payload))
    return path


def _parse_model_record(key: str, raw: Any) -> CustomModelRecord:
    if not _MODEL_KEY_RE.match(key) or "/" in key:
        raise ModelStoreError(
            f"模型 key 非法：{key}。仅允许小写字母、数字、'-' 与 '_'，且不能包含 '/'。"
        )
    if not isinstance(raw, Mapping):
        raise ModelStoreError(f"模型 {key} 必须是对象。")

    profile = str(raw.get("profile") or "").strip()
    model_id = str(raw.get("model_id") or "").strip()
    protocol = str(raw.get("protocol") or "").strip()
    if not profile:
        raise ModelStoreError(f"模型 {key} 缺少 profile。")
    if not model_id:
        raise ModelStoreError(f"模型 {key} 缺少 model_id。")
    if protocol not in SUPPORTED_PROTOCOLS:
        raise ModelStoreError(f"模型 {key} 的 protocol 不支持：{protocol}")

    display_name = str(raw.get("display_name") or key).strip() or key
    enabled = raw.get("enabled", True)
    if not isinstance(enabled, bool):
        raise ModelStoreError(f"模型 {key} 的 enabled 必须是布尔值。")

    aliases_raw = raw.get("aliases") or []
    if not isinstance(aliases_raw, list):
        raise ModelStoreError(f"模型 {key} 的 aliases 必须是列表。")
    aliases = tuple(str(item).strip() for item in aliases_raw if str(item).strip())

    tags_raw = raw.get("tags") or []
    if not isinstance(tags_raw, list):
        raise ModelStoreError(f"模型 {key} 的 tags 必须是列表。")
    tags = tuple(str(item).strip() for item in tags_raw if str(item).strip())

    description = str(raw.get("description") or "").strip()
    context_window_tokens = _positive_int(raw.get("context_window_tokens", 0), f"模型 {key}.context_window_tokens")
    max_output_tokens = _positive_int(raw.get("max_output_tokens", 0), f"模型 {key}.max_output_tokens")
    temperature = _optional_temperature(raw.get("temperature", None), f"模型 {key}.temperature")
    sort_order = raw.get("sort_order", 0)
    if isinstance(sort_order, bool) or not isinstance(sort_order, int):
        raise ModelStoreError(f"模型 {key}.sort_order 必须是整数。")

    capabilities = capabilities_from_mapping(raw.get("capabilities"))
    # 顶层 max_output_tokens 优先，其次 capabilities 中的值。
    if max_output_tokens <= 0 and capabilities.max_output_tokens > 0:
        max_output_tokens = capabilities.max_output_tokens
    if context_window_tokens > 0 and capabilities.context_window_tokens <= 0:
        capabilities = ModelCapabilities(
            streaming=capabilities.streaming,
            tools=capabilities.tools,
            parallel_tool_calls=capabilities.parallel_tool_calls,
            reasoning=capabilities.reasoning,
            vision=capabilities.vision,
            model_discovery=capabilities.model_discovery,
            prompt_cache=capabilities.prompt_cache,
            context_window_tokens=context_window_tokens,
            max_output_tokens=max_output_tokens or capabilities.max_output_tokens,
        )
    elif max_output_tokens > 0 and capabilities.max_output_tokens != max_output_tokens:
        capabilities = ModelCapabilities(
            streaming=capabilities.streaming,
            tools=capabilities.tools,
            parallel_tool_calls=capabilities.parallel_tool_calls,
            reasoning=capabilities.reasoning,
            vision=capabilities.vision,
            model_discovery=capabilities.model_discovery,
            prompt_cache=capabilities.prompt_cache,
            context_window_tokens=capabilities.context_window_tokens or context_window_tokens,
            max_output_tokens=max_output_tokens,
        )

    native_vision = raw.get("native_vision")
    if native_vision is not None and not isinstance(native_vision, bool):
        raise ModelStoreError(f"模型 {key}.native_vision 必须是布尔值。")

    provider_options = raw.get("provider_options") or {}
    if not isinstance(provider_options, dict):
        raise ModelStoreError(f"模型 {key}.provider_options 必须是对象。")

    # 允许把 temperature 写在 provider_options 中，但顶层字段优先。
    if temperature is None and "temperature" in provider_options:
        temperature = _optional_temperature(
            provider_options.get("temperature"),
            f"模型 {key}.provider_options.temperature",
        )

    return CustomModelRecord(
        key=key,
        display_name=display_name,
        profile=profile,
        model_id=model_id,
        protocol=protocol,
        enabled=enabled,
        aliases=aliases,
        description=description,
        tags=tags,
        context_window_tokens=context_window_tokens,
        max_output_tokens=max_output_tokens,
        temperature=temperature,
        native_vision=native_vision,
        capabilities=capabilities,
        provider_options=dict(provider_options),
        sort_order=sort_order,
    )


def _optional_temperature(value: Any, label: str) -> float | None:
    """解析可选 temperature；空值表示使用厂商默认。"""

    if value in (None, ""):
        return None
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        raise ModelStoreError(f"{label} 必须是数字。")
    return float(value)


def _positive_int(value: Any, label: str) -> int:
    if value in (None, ""):
        return 0
    if isinstance(value, bool) or not isinstance(value, int) or value < 0:
        raise ModelStoreError(f"{label} 必须是非负整数。")
    return value


def provider_from_protocol(protocol: str) -> str:
    if protocol.startswith("openai_"):
        return PROVIDER_OPENAI
    if protocol.startswith("anthropic_"):
        return PROVIDER_ANTHROPIC
    if protocol.startswith("gemini_"):
        return PROVIDER_GEMINI
    raise ModelStoreError(f"无法从 protocol 推断 provider：{protocol}")
