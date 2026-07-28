"""多模型 Profile / active_model 解析（从 llm.py 拆出，控制边界体积）。"""

from __future__ import annotations

import os
from typing import Any, Mapping

from .llm import (
    DEFAULT_REASONING_EFFORT,
    DEFAULT_THINKING_TYPE,
    LLMConfig,
    LLMError,
)
from .model_store import ModelStoreError, load_model_store, provider_from_protocol
from ..llm.providers.openai_common import resolve_api_key
from ..llm.registry import ProviderProfile


def is_multi_model_section(section: Mapping[str, Any]) -> bool:
    return isinstance(section.get("profiles"), dict) or isinstance(
        section.get("active_model"), dict
    )


def load_multi_model_llm_config(llm_section: dict[str, Any]) -> LLMConfig:
    """从 llm.profiles + active_model + models.yaml 构建当前 LLMConfig 视图。"""

    defaults = (
        llm_section.get("defaults")
        if isinstance(llm_section.get("defaults"), dict)
        else {}
    )
    profiles_raw = (
        llm_section.get("profiles")
        if isinstance(llm_section.get("profiles"), dict)
        else {}
    )
    if not profiles_raw:
        raise LLMError("多模型配置缺少 llm.profiles。")

    env_model = (
        os.getenv("OMNICRAWL_MODEL", "").strip()
        or os.getenv("OPENAI_MODEL", "").strip()
    )
    env_profile = os.getenv("OMNICRAWL_PROFILE", "").strip()

    active_raw = (
        llm_section.get("active_model")
        if isinstance(llm_section.get("active_model"), dict)
        else {}
    )
    try:
        store = load_model_store()
    except ModelStoreError as exc:
        raise LLMError(str(exc)) from exc

    source = str(active_raw.get("source") or "custom").strip() or "custom"
    profile_id = ""
    protocol = ""
    model_id = ""
    catalog_key = ""
    context_window = 128_000
    model_context_explicit = False
    max_output_tokens = 0
    temperature: float | None = None
    provider_options: dict[str, Any] = {}

    if env_model and "/" not in env_model:
        try:
            record = store.resolve_alias(env_model)
        except ModelStoreError as exc:
            raise LLMError(str(exc)) from exc
        if record is not None:
            source = "custom"
            catalog_key = record.key
            profile_id = record.profile
            protocol = record.protocol
            model_id = record.model_id
            context_window = record.context_window_tokens or context_window
            model_context_explicit = record.context_window_tokens > 0
            max_output_tokens = int(getattr(record, "max_output_tokens", 0) or 0)
            temperature = getattr(record, "temperature", None)
            provider_options = dict(record.provider_options)
        else:
            model_id = env_model
            source = "detected"
    elif env_model and "/" in env_model:
        profile_id, model_id = env_model.split("/", 1)
        source = "detected"
        if env_profile:
            profile_id = env_profile
    elif source == "custom":
        catalog_key = str(active_raw.get("key") or "").strip()
        if not catalog_key:
            raise LLMError("llm.active_model.source=custom 时必须提供 key。")
        try:
            record = store.resolve_alias(catalog_key)
        except ModelStoreError as exc:
            raise LLMError(str(exc)) from exc
        if record is None:
            raise LLMError(
                f"active_model 引用了不存在的自定义模型 key：{catalog_key}。"
                "请检查 models.yaml，或重新选择模型。"
            )
        catalog_key = record.key
        profile_id = record.profile
        protocol = record.protocol
        model_id = record.model_id
        context_window = record.context_window_tokens or context_window
        model_context_explicit = record.context_window_tokens > 0
        max_output_tokens = int(getattr(record, "max_output_tokens", 0) or 0)
        temperature = getattr(record, "temperature", None)
        provider_options = dict(record.provider_options)
    else:
        profile_id = str(active_raw.get("profile") or "").strip()
        model_id = str(active_raw.get("model_id") or "").strip()
        protocol = str(active_raw.get("protocol") or "").strip()
        if not profile_id or not model_id:
            raise LLMError(
                "llm.active_model.source=detected 时必须提供 profile 与 model_id。"
            )

    if env_profile:
        profile_id = env_profile

    profile_data = profiles_raw.get(profile_id)
    if not isinstance(profile_data, dict):
        raise LLMError(f"Profile 不存在或未启用：{profile_id}")
    if profile_data.get("enabled", True) is False:
        raise LLMError(f"Profile 已禁用：{profile_id}")

    provider = str(profile_data.get("provider") or "openai").strip()
    if not protocol:
        protocol = str(profile_data.get("default_protocol") or "").strip()
        if not protocol:
            if provider == "openai":
                protocol = "openai_chat_completions"
            elif provider == "anthropic":
                protocol = "anthropic_messages"
            elif provider == "gemini":
                protocol = "gemini_generate_content"
            else:
                raise LLMError(f"未知 Provider：{provider}")

    try:
        protocol_provider = provider_from_protocol(protocol)
    except Exception as exc:
        raise LLMError(str(exc)) from exc
    if protocol_provider != provider:
        raise LLMError(
            f"模型协议与 Profile Provider 不匹配：Profile {profile_id} 为 {provider}，"
            f"协议 {protocol} 属于 {protocol_provider}。"
        )

    api_key_env = str(profile_data.get("api_key_env") or "").strip()
    api_key_plain = str(profile_data.get("api_key") or "").strip()
    user_agent = str(profile_data.get("user_agent") or "").strip()
    base_url = str(profile_data.get("base_url") or "").strip()
    profile_obj = ProviderProfile(
        id=profile_id,
        provider=provider,
        enabled=True,
        base_url=base_url,
        api_key=api_key_plain,
        api_key_env=api_key_env,
        user_agent=user_agent,
        default_protocol=protocol,
    )
    api_key = resolve_api_key(profile_obj)
    if not api_key and provider == "openai":
        api_key = os.getenv("OPENAI_API_KEY", "").strip()
    if not base_url and provider == "openai":
        base_url = os.getenv("OPENAI_BASE_URL", "").strip()

    reasoning_effort = _read_optional_text(
        {**defaults, **llm_section},
        "reasoning_effort",
        "REASONING_EFFORT",
        DEFAULT_REASONING_EFFORT,
    )
    thinking_type = _read_optional_text(
        {**defaults, **llm_section},
        "thinking_type",
        "OPENAI_THINKING_TYPE",
        DEFAULT_THINKING_TYPE,
    )

    if profile_data.get("default_context_window_tokens"):
        value = profile_data["default_context_window_tokens"]
        if (
            isinstance(value, int)
            and value > 0
            and (context_window <= 0 or source == "detected")
        ):
            context_window = value
    if (
        isinstance(defaults.get("context_window_tokens"), int)
        and defaults["context_window_tokens"] > 0
    ):
        # 设置面板会把 detected 模型的用户选择写入 llm.defaults；它必须覆盖
        # Profile 提供的发现回退值，否则重启后设置会悄然恢复成 Profile 默认值。
        if source == "detected" or (
            not model_context_explicit
            and (context_window <= 0 or context_window == 128_000)
        ):
            context_window = defaults["context_window_tokens"]

    timeout = defaults.get("request_timeout_seconds", 180)
    retries = defaults.get("request_retry_count", 5)
    if not isinstance(timeout, int) or isinstance(timeout, bool) or timeout <= 0:
        timeout = 180
    if not isinstance(retries, int) or isinstance(retries, bool) or retries <= 0:
        retries = 5

    return LLMConfig(
        api_key=api_key,
        base_url=base_url,
        model=model_id,
        thinking_type=thinking_type,
        reasoning_effort=reasoning_effort,
        context_window_tokens=context_window if context_window > 0 else 128_000,
        max_output_tokens=max_output_tokens,
        temperature=temperature,
        profile_id=profile_id,
        provider=provider,
        protocol=protocol,
        catalog_key=catalog_key,
        model_source=source if source in {"custom", "detected"} else "custom",
        api_key_env=api_key_env,
        user_agent=user_agent,
        request_timeout_seconds=timeout,
        request_retry_count=retries,
        provider_options=provider_options,
    )


def parse_profiles(llm_section: Mapping[str, Any]) -> dict[str, ProviderProfile]:
    """解析 llm.profiles 为 ProviderProfile 字典。"""

    raw = llm_section.get("profiles")
    if not isinstance(raw, dict):
        return {}
    defaults = (
        llm_section.get("defaults")
        if isinstance(llm_section.get("defaults"), dict)
        else {}
    )
    result: dict[str, ProviderProfile] = {}
    for profile_id, item in raw.items():
        if not isinstance(item, dict):
            continue
        if item.get("enabled", True) is False:
            continue
        provider = str(item.get("provider") or "").strip()
        if not provider:
            continue
        discovery = (
            item.get("discovery") if isinstance(item.get("discovery"), dict) else {}
        )
        result[str(profile_id)] = ProviderProfile(
            id=str(profile_id),
            provider=provider,
            enabled=True,
            base_url=str(item.get("base_url") or "").strip(),
            api_key=str(item.get("api_key") or "").strip(),
            api_key_env=str(item.get("api_key_env") or "").strip(),
            user_agent=str(item.get("user_agent") or "").strip(),
            default_protocol=str(item.get("default_protocol") or "").strip(),
            discovery_enabled=bool(discovery.get("enabled", True)),
            default_context_window_tokens=(
                int(item.get("default_context_window_tokens") or 0)
                if isinstance(item.get("default_context_window_tokens"), int)
                else 0
            ),
            provider_options=(
                dict(item.get("provider_options") or {})
                if isinstance(item.get("provider_options"), dict)
                else {}
            ),
            request_timeout_seconds=float(defaults.get("request_timeout_seconds") or 180),
            request_retry_count=int(defaults.get("request_retry_count") or 5),
            discovery_timeout_seconds=float(
                defaults.get("discovery_timeout_seconds") or 10
            ),
        )
    return result


def llm_config_to_profile_and_descriptor(config: LLMConfig) -> tuple[Any, Any]:
    """把 LLMConfig 运行视图转换为 Runtime 所需的 Profile + Descriptor。"""

    from ..llm.capabilities import ModelCapabilities
    from ..llm.protocol import ModelIdentity
    from ..llm.registry import ModelDescriptor

    profile_id = getattr(config, "profile_id", "") or "default-openai"
    protocol = getattr(config, "protocol", "") or "openai_chat_completions"
    provider = getattr(config, "provider", "") or "openai"
    profile = ProviderProfile(
        id=profile_id,
        provider=provider,
        enabled=True,
        base_url=getattr(config, "base_url", ""),
        api_key=getattr(config, "api_key", ""),
        api_key_env=getattr(config, "api_key_env", ""),
        user_agent=getattr(config, "user_agent", ""),
        default_protocol=protocol,
        request_timeout_seconds=float(getattr(config, "request_timeout_seconds", 180)),
        request_retry_count=int(getattr(config, "request_retry_count", 5)),
        provider_options=dict(getattr(config, "provider_options", {}) or {}),
    )
    max_output_tokens = int(getattr(config, "max_output_tokens", 0) or 0)
    temperature = getattr(config, "temperature", None)
    context_window_tokens = int(getattr(config, "context_window_tokens", 128_000))
    capabilities = ModelCapabilities(
        streaming=True,
        tools=True,
        parallel_tool_calls=True,
        reasoning=bool(getattr(config, "thinking_enabled", False)),
        context_window_tokens=context_window_tokens,
        max_output_tokens=max_output_tokens,
    )
    descriptor = ModelDescriptor(
        identity=ModelIdentity(
            profile_id=profile_id,
            provider=provider,
            protocol=protocol,
            model_id=config.model,
            catalog_key=getattr(config, "catalog_key", ""),
        ),
        display_name=getattr(config, "catalog_key", "") or config.model,
        capabilities=capabilities,
        context_window_tokens=context_window_tokens,
        max_output_tokens=max_output_tokens,
        temperature=temperature,
        provider_options=dict(getattr(config, "provider_options", {}) or {}),
        source=(
            getattr(config, "model_source", "legacy")
            if getattr(config, "model_source", "legacy") != "legacy"
            else "migrated"
        ),
    )
    return profile, descriptor


def apply_model_selection(config: LLMConfig, selection: str) -> LLMConfig:
    """把 selection 解析为新的运行时 LLMConfig 视图。

    支持：
    - 自定义 models.yaml key / alias
    - profile/model_id
    - 裸 model_id（保留当前 profile/protocol/凭据）
    """

    token = selection.strip()
    if not token:
        raise LLMError("模型选择不能为空。")

    # 先尝试自定义 key/alias。models.yaml 损坏必须显式失败，不可静默降级。
    try:
        store = load_model_store()
        record = store.resolve_alias(token)
    except ModelStoreError as exc:
        raise LLMError(str(exc)) from exc

    if record is not None:
        return _config_from_custom_record(config, record)

    if "/" in token:
        profile_id, model_id = token.split("/", 1)
        profile_id = profile_id.strip()
        model_id = model_id.strip()
        if not profile_id or not model_id:
            raise LLMError("profile/model_id 格式无效。")
        return _config_from_profile_model(config, profile_id, model_id)

    # 裸 model_id：仅替换 model，保留当前 Profile 凭据与协议。
    # 若 token 看起来像自定义 key（含 : 或仅字母数字连字符的别名风格）且 store 已加载，
    # 仍允许作为 provider 侧真实 model_id 使用——这是旧 /model <id> 兼容路径。
    # 检测到的模型不继承上一自定义模型的 max_output/temperature。
    return LLMConfig(
        api_key=config.api_key,
        base_url=config.base_url,
        model=token,
        thinking_type=config.thinking_type,
        reasoning_effort=config.reasoning_effort,
        context_window_tokens=config.context_window_tokens,
        max_output_tokens=0,
        temperature=None,
        system_prompt=config.system_prompt,
        max_history_turns=config.max_history_turns,
        profile_id=config.profile_id,
        provider=config.provider,
        protocol=config.protocol or "openai_chat_completions",
        catalog_key="",
        model_source=config.model_source if config.model_source != "legacy" else "legacy",
        api_key_env=config.api_key_env,
        user_agent=config.user_agent,
        request_timeout_seconds=config.request_timeout_seconds,
        request_retry_count=config.request_retry_count,
        provider_options=dict(config.provider_options),
    )


def _config_from_custom_record(config: LLMConfig, record: Any) -> LLMConfig:
    profiles = _profiles_from_disk()
    profile = profiles.get(record.profile)
    if profile is None:
        raise LLMError(f"自定义模型 {record.key} 引用了不存在或已禁用的 Profile：{record.profile}")
    protocol_provider = provider_from_protocol(record.protocol)
    if protocol_provider != profile.provider:
        raise LLMError(
            f"自定义模型 {record.key} 的协议 {record.protocol} 属于 {protocol_provider}，"
            f"但 Profile {record.profile} 为 {profile.provider}。"
        )

    api_key = resolve_api_key(profile)
    if not api_key:
        raise LLMError(
            f"Profile {profile.id} 缺少 API Key；不同模型渠道不会复用当前模型凭据。"
        )
    base_url = profile.base_url
    return LLMConfig(
        api_key=api_key,
        base_url=base_url,
        model=record.model_id,
        thinking_type=config.thinking_type,
        reasoning_effort=config.reasoning_effort,
        context_window_tokens=record.context_window_tokens
        or profile.default_context_window_tokens
        or config.context_window_tokens,
        max_output_tokens=int(getattr(record, "max_output_tokens", 0) or 0),
        temperature=getattr(record, "temperature", None),
        system_prompt=config.system_prompt,
        max_history_turns=config.max_history_turns,
        profile_id=profile.id,
        provider=profile.provider,
        protocol=record.protocol or profile.resolve_protocol(),
        catalog_key=record.key,
        model_source="custom",
        api_key_env=profile.api_key_env,
        user_agent=profile.user_agent,
        request_timeout_seconds=int(profile.request_timeout_seconds or config.request_timeout_seconds),
        request_retry_count=int(profile.request_retry_count or config.request_retry_count),
        provider_options=dict(record.provider_options or profile.provider_options or {}),
    )


def _config_from_profile_model(
    config: LLMConfig, profile_id: str, model_id: str
) -> LLMConfig:
    profiles = _profiles_from_disk()
    profile = profiles.get(profile_id)
    if profile is None:
        # 多模型配置存在但 profile 缺失：直接失败，避免 silent fallback 到错误凭据。
        if profiles:
            raise LLMError(f"Profile 不存在或未启用：{profile_id}")
        # 无 profiles 配置时（旧单模型模式）按当前凭据切换 model，并保留 profile 标记。
        return LLMConfig(
            api_key=config.api_key,
            base_url=config.base_url,
            model=model_id,
            thinking_type=config.thinking_type,
            reasoning_effort=config.reasoning_effort,
            context_window_tokens=config.context_window_tokens,
            max_output_tokens=0,
            temperature=None,
            system_prompt=config.system_prompt,
            max_history_turns=config.max_history_turns,
            profile_id=profile_id,
            provider=config.provider or "openai",
            protocol=config.protocol or "openai_chat_completions",
            catalog_key="",
            model_source="detected",
            api_key_env=config.api_key_env,
            user_agent=config.user_agent,
            request_timeout_seconds=config.request_timeout_seconds,
            request_retry_count=config.request_retry_count,
            provider_options=dict(config.provider_options),
        )

    api_key = resolve_api_key(profile)
    if not api_key:
        raise LLMError(
            f"Profile {profile.id} 缺少 API Key；不同模型渠道不会复用当前模型凭据。"
        )
    base_url = profile.base_url
    protocol = profile.resolve_protocol() or config.protocol or "openai_chat_completions"
    return LLMConfig(
        api_key=api_key,
        base_url=base_url,
        model=model_id,
        thinking_type=config.thinking_type,
        reasoning_effort=config.reasoning_effort,
        context_window_tokens=profile.default_context_window_tokens
        or config.context_window_tokens,
        max_output_tokens=0,
        temperature=None,
        system_prompt=config.system_prompt,
        max_history_turns=config.max_history_turns,
        profile_id=profile.id,
        provider=profile.provider,
        protocol=protocol,
        catalog_key="",
        model_source="detected",
        api_key_env=profile.api_key_env,
        user_agent=profile.user_agent,
        request_timeout_seconds=int(profile.request_timeout_seconds or config.request_timeout_seconds),
        request_retry_count=int(profile.request_retry_count or config.request_retry_count),
        provider_options=dict(profile.provider_options or {}),
    )


def _profiles_from_disk() -> dict[str, ProviderProfile]:
    try:
        from .runtime import get_section, load_config_data

        data = load_config_data()
        llm_section = get_section(data, "llm")
        return parse_profiles(llm_section)
    except Exception:
        return {}


def _read_optional_text(
    section: dict[str, Any],
    key: str,
    env_name: str,
    default: str,
) -> str:
    env_value = os.getenv(env_name)
    if env_value is not None and env_value.strip():
        return env_value
    value = section.get(key, default)
    if value is None:
        return default
    if not isinstance(value, str):
        raise LLMError(f"配置项 llm.{key} 必须是字符串。")
    return value.strip() or default
