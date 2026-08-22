"""模型渠道配置的读取、校验与原子写回。"""

from __future__ import annotations

import os
import re
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Mapping
from urllib.parse import urlparse

from ...llm.protocol import (
    PROTOCOL_ANTHROPIC_MESSAGES,
    PROTOCOL_GEMINI_GENERATE_CONTENT,
    PROTOCOL_OPENAI_CHAT_COMPLETIONS,
    PROTOCOL_OPENAI_RESPONSES,
    PROVIDER_ANTHROPIC,
    PROVIDER_GEMINI,
    PROVIDER_OPENAI,
    SUPPORTED_PROTOCOLS,
)
from ..core.runtime import (
    RuntimeConfigError,
    atomic_write_text,
    dump_toml_text,
    load_config_data,
    load_raw_file,
    resolve_config_path,
    resolve_config_write_path,
    resolve_models_path,
    resolve_models_write_path,
)


_CHANNEL_KEY_RE = re.compile(r"^[a-z0-9][a-z0-9_-]*$")
_PROVIDER_PROTOCOLS = {
    PROVIDER_OPENAI: (
        PROTOCOL_OPENAI_CHAT_COMPLETIONS,
        PROTOCOL_OPENAI_RESPONSES,
    ),
    PROVIDER_ANTHROPIC: (PROTOCOL_ANTHROPIC_MESSAGES,),
    PROVIDER_GEMINI: (PROTOCOL_GEMINI_GENERATE_CONTENT,),
}
_PROVIDER_API_KEY_ENV = {
    PROVIDER_OPENAI: "OPENAI_API_KEY",
    PROVIDER_ANTHROPIC: "ANTHROPIC_API_KEY",
    PROVIDER_GEMINI: "GEMINI_API_KEY",
}
_PROVIDER_DEFAULT_URL = {
    PROVIDER_OPENAI: "https://api.openai.com/v1",
    PROVIDER_ANTHROPIC: "https://api.anthropic.com",
    PROVIDER_GEMINI: "https://generativelanguage.googleapis.com",
}
_PROVIDER_DEFAULT_MODEL = {
    PROVIDER_OPENAI: "gpt-5.2",
    PROVIDER_ANTHROPIC: "claude-sonnet-4-5",
    PROVIDER_GEMINI: "gemini-2.5-pro",
}
_PROVIDER_LABEL = {
    PROVIDER_OPENAI: "OpenAI",
    PROVIDER_ANTHROPIC: "Anthropic",
    PROVIDER_GEMINI: "Gemini",
}


class ChannelConfigError(RuntimeError):
    """渠道配置无法读取或保存。"""


@dataclass(frozen=True)
class ChannelConfig:
    """一个可独立选择的模型渠道。"""

    key: str
    name: str
    profile_id: str
    provider: str
    protocol: str
    base_url: str
    api_key: str = field(repr=False)
    model_id: str
    enabled: bool = True
    api_key_env: str = ""
    user_agent: str = ""

    @property
    def provider_label(self) -> str:
        return _PROVIDER_LABEL.get(self.provider, self.provider)

    @property
    def protocol_label(self) -> str:
        return protocol_label(self.protocol)


@dataclass(frozen=True)
class ChannelConfiguration:
    """渠道集合及当前默认模型 key。"""

    channels: tuple[ChannelConfig, ...]
    default_key: str


def provider_options() -> tuple[str, ...]:
    return (PROVIDER_OPENAI, PROVIDER_ANTHROPIC, PROVIDER_GEMINI)


def protocols_for_provider(provider: str) -> tuple[str, ...]:
    return _PROVIDER_PROTOCOLS.get(provider, ())


def provider_label(provider: str) -> str:
    return _PROVIDER_LABEL.get(provider, provider)


def protocol_label(protocol: str) -> str:
    return {
        PROTOCOL_OPENAI_CHAT_COMPLETIONS: "Chat Completions",
        PROTOCOL_OPENAI_RESPONSES: "Responses",
        PROTOCOL_ANTHROPIC_MESSAGES: "Messages",
        PROTOCOL_GEMINI_GENERATE_CONTENT: "Generate Content",
    }.get(protocol, protocol)


def default_channel(provider: str, *, key: str | None = None) -> ChannelConfig:
    """创建指定 Provider 的安全默认草稿，不包含凭据。"""

    if provider not in _PROVIDER_PROTOCOLS:
        raise ChannelConfigError(f"不支持的请求方式：{provider}")
    channel_key = key or f"{provider}-main"
    return ChannelConfig(
        key=channel_key,
        name=f"{provider_label(provider)} 主渠道",
        profile_id=channel_key,
        provider=provider,
        protocol=_PROVIDER_PROTOCOLS[provider][0],
        base_url=_PROVIDER_DEFAULT_URL[provider],
        api_key="",
        api_key_env=_PROVIDER_API_KEY_ENV[provider],
        model_id=_PROVIDER_DEFAULT_MODEL[provider],
    )


def unique_channel_key(name: str, existing: set[str]) -> str:
    """把渠道名转为稳定 key，并在冲突时追加序号。"""

    base = re.sub(r"[^a-z0-9_-]+", "-", name.strip().lower()).strip("-_")
    if not base:
        base = "channel"
    if not base[0].isalnum():
        base = f"channel-{base}"
    candidate = base
    suffix = 2
    while candidate in existing:
        candidate = f"{base}-{suffix}"
        suffix += 1
    return candidate


def load_channel_configuration(
    config_path: str | Path | None = None,
    models_path: str | Path | None = None,
) -> ChannelConfiguration:
    """从现有 profiles 和自定义模型目录构建渠道列表。"""

    resolved_config = resolve_config_path(config_path)
    resolved_models = resolve_models_path(models_path)
    try:
        config_data = load_config_data(resolved_config)
        models_data = load_raw_file(resolved_models) if resolved_models.exists() else {}
    except RuntimeConfigError as exc:
        raise ChannelConfigError(str(exc)) from exc

    llm = config_data.get("llm") if isinstance(config_data.get("llm"), dict) else {}
    profiles = llm.get("profiles") if isinstance(llm.get("profiles"), dict) else {}
    models = models_data.get("models") if isinstance(models_data.get("models"), dict) else {}
    channels: list[ChannelConfig] = []
    for key, raw_model in models.items():
        if not isinstance(raw_model, Mapping):
            continue
        profile_id = str(raw_model.get("profile") or "").strip()
        raw_profile = profiles.get(profile_id)
        if not profile_id or not isinstance(raw_profile, Mapping):
            continue
        provider = str(raw_profile.get("provider") or "openai").strip().lower()
        protocol = str(
            raw_model.get("protocol")
            or raw_profile.get("default_protocol")
            or (_PROVIDER_PROTOCOLS.get(provider) or ("",))[0]
        ).strip()
        api_key_env = str(
            raw_profile.get("api_key_env") or _PROVIDER_API_KEY_ENV.get(provider, "")
        ).strip()
        channels.append(
            ChannelConfig(
                key=str(key),
                name=str(raw_model.get("display_name") or key).strip(),
                profile_id=profile_id,
                provider=provider,
                protocol=protocol,
                base_url=str(raw_profile.get("base_url") or "").strip(),
                api_key=str(raw_profile.get("api_key") or "").strip(),
                api_key_env=api_key_env,
                user_agent=str(raw_profile.get("user_agent") or "").strip(),
                model_id=str(raw_model.get("model_id") or "").strip(),
                enabled=(
                    raw_model.get("enabled", True) is not False
                    and raw_profile.get("enabled", True) is not False
                ),
            )
        )

    active = llm.get("active_model") if isinstance(llm.get("active_model"), dict) else {}
    requested_default = str(active.get("key") or "").strip()
    default_key = _enabled_default(channels, requested_default)
    return ChannelConfiguration(tuple(channels), default_key)


def save_channel_configuration(
    configuration: ChannelConfiguration,
    config_path: str | Path | None = None,
    models_path: str | Path | None = None,
) -> tuple[Path, Path]:
    """把渠道配置写回现有 TOML，同时保证 models.toml 不含凭据。"""

    channels = tuple(configuration.channels)
    if not channels:
        raise ChannelConfigError("至少需要保留一个模型渠道。")
    _validate_channels(channels)

    source_config = resolve_config_path(config_path)
    source_models = resolve_models_path(models_path)
    resolved_config = resolve_config_write_path(config_path)
    resolved_models = resolve_models_write_path(models_path)
    try:
        config_data = load_config_data(source_config)
        models_data = load_raw_file(source_models) if source_models.exists() else {}
    except RuntimeConfigError as exc:
        raise ChannelConfigError(str(exc)) from exc

    llm = config_data.get("llm")
    if not isinstance(llm, dict):
        llm = {}
        config_data["llm"] = llm
    profiles = llm.get("profiles")
    if not isinstance(profiles, dict):
        profiles = {}
        llm["profiles"] = profiles

    raw_models = models_data.get("models")
    if not isinstance(raw_models, dict):
        raw_models = {}
        models_data = {"version": int(models_data.get("version") or 1), "models": raw_models}

    previous_model_profiles = {
        str(key): str(value.get("profile") or "")
        for key, value in raw_models.items()
        if isinstance(value, Mapping)
    }
    final_keys = {item.key for item in channels}
    final_profiles = {item.profile_id for item in channels}
    for removed_key in set(previous_model_profiles) - final_keys:
        raw_models.pop(removed_key, None)
    for removed_profile in set(previous_model_profiles.values()) - final_profiles:
        if removed_profile:
            profiles.pop(removed_profile, None)

    for channel in channels:
        profile = profiles.get(channel.profile_id)
        if not isinstance(profile, dict):
            profile = {}
            profiles[channel.profile_id] = profile
        profile.update(
            {
                "provider": channel.provider,
                "enabled": channel.enabled,
                "base_url": channel.base_url,
                "api_key_env": channel.api_key_env
                or _PROVIDER_API_KEY_ENV[channel.provider],
                "default_protocol": channel.protocol,
            }
        )
        if channel.api_key:
            profile["api_key"] = channel.api_key
        else:
            profile.pop("api_key", None)
        if channel.user_agent.strip():
            profile["user_agent"] = channel.user_agent.strip()
        else:
            profile.pop("user_agent", None)
        discovery = profile.get("discovery")
        if not isinstance(discovery, dict):
            discovery = {}
            profile["discovery"] = discovery
        discovery.setdefault("enabled", True)

        model = raw_models.get(channel.key)
        if not isinstance(model, dict):
            model = {}
            raw_models[channel.key] = model
        model.update(
            {
                "display_name": channel.name,
                "profile": channel.profile_id,
                "model_id": channel.model_id,
                "protocol": channel.protocol,
                "enabled": channel.enabled,
            }
        )
        for forbidden in ("api_key", "token", "cookie", "authorization"):
            model.pop(forbidden, None)

    default_key = _enabled_default(channels, configuration.default_key)
    if not default_key:
        raise ChannelConfigError("至少需要启用一个模型渠道。")
    llm["active_model"] = {"source": "custom", "key": default_key}
    config_data.setdefault("version", 2)
    models_data.setdefault("version", 1)

    written: list[tuple[Path, str | None]] = []
    try:
        original_config = (
            resolved_config.read_text(encoding="utf-8-sig")
            if resolved_config.exists()
            else None
        )
        original_models = (
            resolved_models.read_text(encoding="utf-8-sig")
            if resolved_models.exists()
            else None
        )
        atomic_write_text(resolved_config, dump_toml_text(config_data))
        written.append((resolved_config, original_config))
        atomic_write_text(resolved_models, dump_toml_text(models_data))
        written.append((resolved_models, original_models))
    except (RuntimeConfigError, OSError) as exc:
        rollback_errors: list[str] = []
        for path, original in reversed(written):
            try:
                if original is None:
                    path.unlink(missing_ok=True)
                else:
                    atomic_write_text(path, original)
            except (RuntimeConfigError, OSError) as rollback_exc:
                rollback_errors.append(f"{path}: {rollback_exc}")
        detail = f"；回滚失败：{'；'.join(rollback_errors)}" if rollback_errors else ""
        raise ChannelConfigError(f"模型渠道配置写入失败：{exc}{detail}") from exc
    return resolved_config, resolved_models


def missing_enabled_credentials(configuration: ChannelConfiguration) -> tuple[str, ...]:
    """返回已启用但既无直接 Key 也无环境变量 Key 的渠道名称。"""

    missing: list[str] = []
    for channel in configuration.channels:
        if not channel.enabled:
            continue
        if channel.api_key:
            continue
        if channel.api_key_env and os.getenv(channel.api_key_env, "").strip():
            continue
        missing.append(channel.name)
    return tuple(missing)


def has_usable_channel(configuration: ChannelConfiguration) -> bool:
    """判断默认渠道是否具备可解析的 Key。"""

    for channel in configuration.channels:
        if channel.key != configuration.default_key or not channel.enabled:
            continue
        return bool(
            channel.api_key
            or (channel.api_key_env and os.getenv(channel.api_key_env, "").strip())
        )
    return False


def _enabled_default(channels: list[ChannelConfig] | tuple[ChannelConfig, ...], requested: str) -> str:
    for channel in channels:
        if channel.key == requested and channel.enabled:
            return requested
    return next((item.key for item in channels if item.enabled), "")


def _validate_channels(channels: tuple[ChannelConfig, ...]) -> None:
    keys: set[str] = set()
    profiles: dict[str, ChannelConfig] = {}
    enabled = 0
    for channel in channels:
        if not _CHANNEL_KEY_RE.fullmatch(channel.key):
            raise ChannelConfigError(f"渠道 key 非法：{channel.key}")
        if channel.key in keys:
            raise ChannelConfigError(f"渠道 key 重复：{channel.key}")
        keys.add(channel.key)
        if not channel.name.strip():
            raise ChannelConfigError(f"渠道 {channel.key} 缺少名称。")
        if channel.provider not in _PROVIDER_PROTOCOLS:
            raise ChannelConfigError(f"渠道 {channel.name} 的请求方式不支持：{channel.provider}")
        if (
            channel.protocol not in SUPPORTED_PROTOCOLS
            or channel.protocol not in _PROVIDER_PROTOCOLS[channel.provider]
        ):
            raise ChannelConfigError(f"渠道 {channel.name} 的协议与请求方式不匹配。")
        parsed = urlparse(channel.base_url)
        if parsed.scheme not in {"http", "https"} or not parsed.netloc:
            raise ChannelConfigError(f"渠道 {channel.name} 的 Base URL 无效。")
        if not channel.model_id.strip():
            raise ChannelConfigError(f"渠道 {channel.name} 缺少模型 ID。")
        if "\r" in channel.user_agent or "\n" in channel.user_agent:
            raise ChannelConfigError(f"渠道 {channel.name} 的 User-Agent 不能包含换行。")
        previous = profiles.get(channel.profile_id)
        if previous is not None and (
            previous.provider,
            previous.protocol,
            previous.base_url,
            previous.api_key,
            previous.user_agent,
        ) != (
            channel.provider,
            channel.protocol,
            channel.base_url,
            channel.api_key,
            channel.user_agent,
        ):
            raise ChannelConfigError(f"Profile {channel.profile_id} 被多个不同渠道配置复用。")
        profiles[channel.profile_id] = channel
        if channel.enabled:
            enabled += 1
    if enabled == 0:
        raise ChannelConfigError("至少需要启用一个模型渠道。")
