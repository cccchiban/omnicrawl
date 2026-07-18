"""模型目录：兼容旧 /models 探测，并支持自定义 + 原生 SDK 双列聚合。"""

from __future__ import annotations

import json
import os
import socket
import threading
import time
import urllib.error
import urllib.request
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Iterable, Mapping

from .llm import ActiveModelRef, LLMConfig, LLMError, load_llm_config, save_active_model_ref
from .llm_multi import parse_profiles
from .model_store import ModelStoreError, load_model_store, provider_from_protocol
from .runtime import RuntimeConfigError, get_section, load_config_data, save_config_data
from ..llm.capabilities import ModelCapabilities
from ..llm.protocol import ModelIdentity
from ..llm.registry import DiscoveryResult, ModelDescriptor, ProviderProfile, get_adapter


MODEL_LIST_TIMEOUT_SECONDS = 10
MAX_MODEL_LIST_BYTES = 2 * 1024 * 1024
MAX_MODELS_PER_PROFILE = 500
DEFAULT_DISCOVERY_CACHE_TTL_SECONDS = 300


class ModelCatalogError(RuntimeError):
    """模型列表检测或模型配置写回失败。"""


@dataclass(frozen=True)
class ModelOption:
    """可供 TUI 或 API 客户端展示和切换的模型项。"""

    id: str
    name: str
    provider: str

    def to_ui_dict(self) -> dict[str, str]:
        return {"id": self.id, "name": self.name, "provider": self.provider}


@dataclass(frozen=True)
class CatalogModel:
    source: str  # custom | detected | current_missing
    key: str
    profile_id: str
    provider: str
    protocol: str
    model_id: str
    display_name: str
    capabilities: ModelCapabilities = field(default_factory=ModelCapabilities)
    context_window_tokens: int = 0
    availability: str = "unknown"
    matched_custom_key: str = ""
    diagnostic: str = ""
    tags: tuple[str, ...] = ()
    aliases: tuple[str, ...] = ()
    sort_order: int = 0

    def triple(self) -> tuple[str, str, str]:
        return (self.profile_id, self.protocol, self.model_id)

    def to_option(self) -> ModelOption:
        option_id = self.key if self.source == "custom" and self.key else self.model_id
        return ModelOption(
            id=option_id,
            name=self.display_name or option_id,
            provider=self.provider or detect_model_provider(self.model_id),
        )


@dataclass
class _DiscoveryCacheEntry:
    result: DiscoveryResult
    fetched_at: float


_discovery_cache: dict[str, _DiscoveryCacheEntry] = {}
_discovery_lock = threading.Lock()


def detect_model_options(
    config: LLMConfig,
    *,
    timeout_seconds: float = MODEL_LIST_TIMEOUT_SECONDS,
) -> list[ModelOption]:
    """从当前 `llm.base_url` 的 OpenAI 兼容 `/models` 接口检测模型列表。

    保留 urllib 直连路径，兼容既有单测 mock。
    """

    endpoint = _models_endpoint(config.base_url)
    request = urllib.request.Request(
        endpoint,
        headers={
            "Accept": "application/json",
            "Authorization": f"Bearer {config.api_key}",
            "User-Agent": "ai-voice-agent/1.0",
        },
        method="GET",
    )

    try:
        with urllib.request.urlopen(request, timeout=timeout_seconds) as response:
            raw_body = response.read(MAX_MODEL_LIST_BYTES + 1)
    except urllib.error.HTTPError as exc:
        raise ModelCatalogError(_format_http_error(exc)) from exc
    except (urllib.error.URLError, TimeoutError, socket.timeout) as exc:
        raise ModelCatalogError(f"无法连接模型列表接口：{exc}") from exc
    except OSError as exc:
        raise ModelCatalogError(f"读取模型列表失败：{exc}") from exc

    if len(raw_body) > MAX_MODEL_LIST_BYTES:
        raise ModelCatalogError("模型列表响应过大，已拒绝解析。")

    try:
        payload = json.loads(raw_body.decode("utf-8-sig"))
    except UnicodeDecodeError as exc:
        raise ModelCatalogError("模型列表接口返回的内容不是 UTF-8 JSON。") from exc
    except json.JSONDecodeError as exc:
        raise ModelCatalogError(
            f"模型列表接口返回的内容不是合法 JSON：第 {exc.lineno} 行。"
        ) from exc

    model_ids = list(_extract_model_ids(payload))
    if not model_ids:
        error_message = _extract_error_message(payload)
        if error_message:
            raise ModelCatalogError(f"模型列表接口返回错误：{error_message}")
        raise ModelCatalogError("模型列表接口没有返回可用模型。")

    return [
        ModelOption(id=model_id, name=model_id, provider=detect_model_provider(model_id))
        for model_id in model_ids
    ]


def build_catalog(
    *,
    config: LLMConfig | None = None,
    refresh: bool = False,
    timeout_seconds: float = MODEL_LIST_TIMEOUT_SECONDS,
    include_custom: bool = True,
    config_data: dict[str, Any] | None = None,
) -> dict[str, Any]:
    """构建双列目录：custom / detected / diagnostics / current。"""

    if config is None:
        try:
            config = load_llm_config()
        except LLMError:
            config = None

    try:
        data = config_data if config_data is not None else load_config_data()
        llm_section = get_section(data, "llm")
    except RuntimeConfigError as exc:
        raise ModelCatalogError(str(exc)) from exc

    profiles = parse_profiles(llm_section)
    if not profiles and config is not None:
        profiles = {
            config.profile_id or "default-openai": ProviderProfile(
                id=config.profile_id or "default-openai",
                provider=config.provider or "openai",
                enabled=True,
                base_url=config.base_url,
                api_key=config.api_key,
                api_key_env=config.api_key_env,
                default_protocol=config.protocol or "openai_chat_completions",
                discovery_enabled=True,
                discovery_timeout_seconds=timeout_seconds,
            )
        }

    custom_items: list[CatalogModel] = []
    custom_index: dict[tuple[str, str, str], str] = {}
    if include_custom:
        try:
            store = load_model_store()
        except ModelStoreError as exc:
            raise ModelCatalogError(str(exc)) from exc
        for record in store.models:
            if not record.enabled:
                continue
            try:
                provider = (
                    profiles[record.profile].provider
                    if record.profile in profiles
                    else provider_from_protocol(record.protocol)
                )
            except Exception:
                provider = provider_from_protocol(record.protocol)
            item = CatalogModel(
                source="custom",
                key=record.key,
                profile_id=record.profile,
                provider=provider,
                protocol=record.protocol,
                model_id=record.model_id,
                display_name=record.display_name,
                capabilities=record.capabilities,
                context_window_tokens=record.context_window_tokens,
                availability="unknown",
                tags=record.tags,
                aliases=record.aliases,
                sort_order=record.sort_order,
            )
            custom_items.append(item)
            custom_index[item.triple()] = record.key

    detected_items: list[CatalogModel] = []
    diagnostics: list[dict[str, str]] = []
    for profile_id, profile in profiles.items():
        if not profile.discovery_enabled:
            continue
        result = _discover_for_profile(
            profile,
            refresh=refresh,
            timeout_seconds=timeout_seconds or profile.discovery_timeout_seconds,
        )
        if result.status != "ok":
            diagnostics.append(
                {
                    "profile": profile_id,
                    "status": result.status,
                    "message": result.message or "模型列表发现失败，自定义模型仍可使用。",
                }
            )
            continue
        for model in result.models[:MAX_MODELS_PER_PROFILE]:
            triple = (model.profile_id, model.protocol, model.model_id)
            matched = custom_index.get(triple, "")
            if matched:
                for idx, custom in enumerate(custom_items):
                    if custom.key == matched:
                        custom_items[idx] = CatalogModel(
                            source=custom.source,
                            key=custom.key,
                            profile_id=custom.profile_id,
                            provider=custom.provider,
                            protocol=custom.protocol,
                            model_id=custom.model_id,
                            display_name=custom.display_name,
                            capabilities=custom.capabilities,
                            context_window_tokens=custom.context_window_tokens,
                            availability="available",
                            matched_custom_key=custom.key,
                            tags=custom.tags,
                            aliases=custom.aliases,
                            sort_order=custom.sort_order,
                        )
                        break
            detected_items.append(
                CatalogModel(
                    source="detected",
                    key=f"{model.profile_id}/{model.model_id}",
                    profile_id=model.profile_id,
                    provider=model.provider,
                    protocol=model.protocol,
                    model_id=model.model_id,
                    display_name=model.display_name or model.model_id,
                    capabilities=model.capabilities,
                    context_window_tokens=model.context_window_tokens
                    or profile.default_context_window_tokens,
                    availability="available",
                    matched_custom_key=matched,
                )
            )

    return {
        "current": _current_catalog_view(config, custom_items, detected_items),
        "custom": custom_items,
        "detected": detected_items,
        "diagnostics": diagnostics,
    }


def detect_model_provider(model_id: str) -> str:
    """按常见模型名前缀给 UI 一个轻量 provider 分类。"""

    normalized = model_id.strip().lower()
    if normalized.startswith(("gpt-", "chatgpt-", "o1", "o3", "o4")):
        return "gpt"
    if normalized.startswith(("claude-", "anthropic/claude")):
        return "claude"
    if normalized.startswith(("gemini", "models/gemini")):
        return "gemini"
    if normalized.startswith(("deepseek", "deepseek/")):
        return "deepseek"
    if normalized.startswith(("qwen", "qwen/", "qwq")):
        return "qwen"
    if normalized.startswith(("glm", "chatglm", "zhipu")):
        return "glm"
    return "other"


def ensure_current_model_option(
    options: Iterable[ModelOption], current_model: str
) -> list[ModelOption]:
    current = current_model.strip()
    result = list(options)
    if not current:
        return result
    if any(option.id == current for option in result):
        return result
    return [
        ModelOption(id=current, name=current, provider=detect_model_provider(current)),
        *result,
    ]


def model_options_to_ui(options: Iterable[ModelOption]) -> list[dict[str, str]]:
    return [option.to_ui_dict() for option in options]


def format_model_options(
    options: Iterable[ModelOption], *, current_model: str = "", limit: int = 40
) -> str:
    rows = list(options)
    lines: list[str] = []
    for index, option in enumerate(rows[:limit], start=1):
        marker = " *" if option.id == current_model else ""
        lines.append(f"{index:>2}. {option.id}{marker}")
    remaining = len(rows) - limit
    if remaining > 0:
        lines.append(f"... 还有 {remaining} 个模型未显示。")
    return "\n".join(lines)


def save_llm_model(model_id: str, config_path: str | Path | None = None) -> Path:
    """兼容旧接口：把当前模型写回配置。"""

    model = model_id.strip()
    if not model:
        raise ModelCatalogError("模型 ID 不能为空。")

    try:
        data = load_config_data(config_path)
        llm_section = get_section(data, "llm")
    except RuntimeConfigError as exc:
        raise ModelCatalogError(str(exc)) from exc

    if not isinstance(llm_section.get("profiles"), dict):
        llm_section["model"] = model
        data["llm"] = llm_section
        try:
            return save_config_data(data, config_path)
        except RuntimeConfigError as exc:
            raise ModelCatalogError(str(exc)) from exc

    try:
        store = load_model_store()
        record = store.resolve_alias(model)
    except ModelStoreError as exc:
        raise ModelCatalogError(str(exc)) from exc

    if record is not None:
        ref = ActiveModelRef(source="custom", key=record.key, model_id=record.model_id)
        try:
            return save_active_model_ref(ref, config_path)
        except LLMError as exc:
            raise ModelCatalogError(str(exc)) from exc

    profile_id = ""
    model_name = model
    if "/" in model:
        profile_id, model_name = model.split("/", 1)
    profiles = parse_profiles(llm_section)
    if not profile_id:
        active = (
            llm_section.get("active_model")
            if isinstance(llm_section.get("active_model"), dict)
            else {}
        )
        profile_id = str(active.get("profile") or "").strip()
        if not profile_id and profiles:
            profile_id = next(iter(profiles.keys()))
    if profile_id not in profiles:
        raise ModelCatalogError(f"无法解析 Profile：{profile_id or '(空)'}")
    protocol = profiles[profile_id].resolve_protocol()
    ref = ActiveModelRef(
        source="detected",
        profile=profile_id,
        model_id=model_name,
        protocol=protocol,
    )
    try:
        return save_active_model_ref(ref, config_path)
    except LLMError as exc:
        raise ModelCatalogError(str(exc)) from exc


def model_env_override_active() -> bool:
    return bool(
        os.getenv("OMNICRAWL_MODEL", "").strip() or os.getenv("OPENAI_MODEL", "").strip()
    )


def catalog_model_to_descriptor(item: CatalogModel) -> ModelDescriptor:
    max_output = int(getattr(item.capabilities, "max_output_tokens", 0) or 0)
    return ModelDescriptor(
        identity=ModelIdentity(
            profile_id=item.profile_id,
            provider=item.provider,
            protocol=item.protocol,
            model_id=item.model_id,
            catalog_key=item.key if item.source == "custom" else "",
        ),
        display_name=item.display_name,
        capabilities=item.capabilities,
        context_window_tokens=item.context_window_tokens,
        max_output_tokens=max_output,
        aliases=item.aliases,
        tags=item.tags,
        source=item.source,
        sort_order=item.sort_order,
        enabled=True,
    )


def clear_discovery_cache() -> None:
    with _discovery_lock:
        _discovery_cache.clear()


def _discover_for_profile(
    profile: ProviderProfile, *, refresh: bool, timeout_seconds: float
) -> DiscoveryResult:
    ttl = DEFAULT_DISCOVERY_CACHE_TTL_SECONDS
    now = time.monotonic()
    with _discovery_lock:
        cached = _discovery_cache.get(profile.id)
        if cached is not None and not refresh and (now - cached.fetched_at) < ttl:
            return cached.result
    try:
        adapter = get_adapter(profile.resolve_protocol())
        result = adapter.discover_models(profile, timeout_seconds=timeout_seconds)
    except Exception as exc:
        result = DiscoveryResult(
            profile_id=profile.id,
            status="unavailable",
            message=f"模型列表发现失败：{exc}",
        )
    with _discovery_lock:
        if result.status == "ok":
            _discovery_cache[profile.id] = _DiscoveryCacheEntry(
                result=result,
                fetched_at=now,
            )
        else:
            # 网络或网关故障通常是短暂的；缓存失败会让服务恢复后仍持续
            # 展示旧诊断，直到 TTL 到期或用户手动刷新。
            _discovery_cache.pop(profile.id, None)
    return result


def _current_catalog_view(
    config: LLMConfig | None,
    custom_items: list[CatalogModel],
    detected_items: list[CatalogModel],
) -> dict[str, Any]:
    if config is None:
        return {}
    source = config.model_source if config.model_source in {"custom", "detected"} else "custom"
    if config.catalog_key:
        source = "custom"
    view = {
        "source": source if source != "legacy" else "custom",
        "key": config.catalog_key,
        "profile": config.profile_id,
        "protocol": config.protocol,
        "model_id": config.model,
    }
    triples_custom = {item.triple() for item in custom_items}
    triples_detected = {item.triple() for item in detected_items}
    triple = (config.profile_id, config.protocol, config.model)
    if config.profile_id and triple not in triples_custom and triple not in triples_detected:
        if config.catalog_key and any(item.key == config.catalog_key for item in custom_items):
            return view
        view["missing"] = True
    return view


def _models_endpoint(base_url: str) -> str:
    base = base_url.strip()
    if not base:
        raise ModelCatalogError("缺少 llm.base_url，无法检测模型列表。")
    return f"{base.rstrip('/')}/models"


def _extract_model_ids(payload: Any) -> Iterable[str]:
    data = payload.get("data") if isinstance(payload, Mapping) else payload
    if not isinstance(data, list):
        return []
    seen: set[str] = set()
    result: list[str] = []
    for item in data:
        model_id = _read_model_id(item)
        if not model_id or model_id in seen:
            continue
        seen.add(model_id)
        result.append(model_id)
    return result


def _read_model_id(item: Any) -> str:
    if isinstance(item, str):
        return item.strip()
    if not isinstance(item, Mapping):
        return ""
    for key in ("id", "model", "name"):
        value = item.get(key)
        if isinstance(value, str) and value.strip():
            return value.strip()
    return ""


def _extract_error_message(payload: Any) -> str:
    if not isinstance(payload, Mapping):
        return ""
    error = payload.get("error")
    if isinstance(error, Mapping):
        message = error.get("message")
        return message.strip() if isinstance(message, str) else ""
    return error.strip() if isinstance(error, str) else ""


def _format_http_error(exc: urllib.error.HTTPError) -> str:
    status = getattr(exc, "code", None)
    if status == 401:
        return "模型列表接口鉴权失败（HTTP 401），请检查 API Key。"
    if status == 403:
        return "当前 API Key 没有读取模型列表的权限（HTTP 403）。"
    if status == 404:
        return "模型列表接口不存在（HTTP 404），请检查 llm.base_url 是否指向 OpenAI 兼容 /v1 地址。"
    if status == 429:
        return "模型列表接口触发限流（HTTP 429），请稍后重试。"
    if isinstance(status, int) and 500 <= status <= 599:
        return f"模型列表服务暂时不可用（HTTP {status}）。"
    if isinstance(status, int):
        return f"模型列表接口返回错误（HTTP {status}）。"
    return "模型列表接口返回错误。"
