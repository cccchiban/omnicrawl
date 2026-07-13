"""Provider Adapter 注册与 Runtime 工厂。"""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any, Mapping

from .capabilities import ModelCapabilities, merge_capabilities
from .errors import ModelError, ModelErrorCode
from .protocol import (
    PROVIDER_ANTHROPIC,
    PROVIDER_GEMINI,
    PROVIDER_OPENAI,
    PROTOCOL_ANTHROPIC_MESSAGES,
    PROTOCOL_GEMINI_GENERATE_CONTENT,
    PROTOCOL_OPENAI_CHAT_COMPLETIONS,
    PROTOCOL_OPENAI_RESPONSES,
    SUPPORTED_PROTOCOLS,
    ModelIdentity,
    ModelProviderAdapter,
    ModelRuntime,
)


@dataclass(frozen=True)
class ProviderProfile:
    """一个可连接的 Provider 凭据与默认协议配置。"""

    id: str
    provider: str
    enabled: bool = True
    base_url: str = ""
    api_key: str = ""
    api_key_env: str = ""
    default_protocol: str = ""
    discovery_enabled: bool = True
    default_context_window_tokens: int = 0
    provider_options: Mapping[str, Any] = field(default_factory=dict)
    request_timeout_seconds: float = 180.0
    request_retry_count: int = 5
    discovery_timeout_seconds: float = 10.0

    def resolve_protocol(self, protocol: str = "") -> str:
        chosen = (protocol or self.default_protocol or "").strip()
        if not chosen:
            if self.provider == PROVIDER_OPENAI:
                chosen = PROTOCOL_OPENAI_CHAT_COMPLETIONS
            elif self.provider == PROVIDER_ANTHROPIC:
                chosen = PROTOCOL_ANTHROPIC_MESSAGES
            elif self.provider == PROVIDER_GEMINI:
                chosen = PROTOCOL_GEMINI_GENERATE_CONTENT
            else:
                raise ModelError(
                    code=ModelErrorCode.CONFIGURATION_ERROR,
                    message=f"未知 Provider：{self.provider}",
                )
        if chosen not in SUPPORTED_PROTOCOLS:
            raise ModelError(
                code=ModelErrorCode.CONFIGURATION_ERROR,
                message=f"不支持的协议：{chosen}",
            )
        _validate_protocol_matches_provider(self.provider, chosen)
        return chosen


@dataclass(frozen=True)
class ModelDescriptor:
    """可构建 Runtime 的完整模型描述。"""

    identity: ModelIdentity
    display_name: str = ""
    capabilities: ModelCapabilities = field(default_factory=ModelCapabilities)
    context_window_tokens: int = 0
    max_output_tokens: int = 0
    temperature: float | None = None
    aliases: tuple[str, ...] = ()
    description: str = ""
    tags: tuple[str, ...] = ()
    provider_options: Mapping[str, Any] = field(default_factory=dict)
    source: str = "custom"  # custom | detected | migrated | current_missing
    sort_order: int = 0
    enabled: bool = True

    @property
    def model_id(self) -> str:
        return self.identity.model_id

    @property
    def profile_id(self) -> str:
        return self.identity.profile_id

    @property
    def protocol(self) -> str:
        return self.identity.protocol

    @property
    def provider(self) -> str:
        return self.identity.provider


@dataclass(frozen=True)
class DiscoveryModel:
    profile_id: str
    provider: str
    protocol: str
    model_id: str
    display_name: str = ""
    capabilities: ModelCapabilities = field(default_factory=ModelCapabilities)
    context_window_tokens: int = 0
    raw: Mapping[str, Any] = field(default_factory=dict)


@dataclass(frozen=True)
class DiscoveryResult:
    profile_id: str
    models: tuple[DiscoveryModel, ...] = ()
    status: str = "ok"  # ok | unavailable | unsupported
    message: str = ""


_ADAPTERS: dict[str, ModelProviderAdapter] = {}
_REGISTERED = False


def register_adapter(adapter: ModelProviderAdapter) -> None:
    _ADAPTERS[adapter.provider_type] = adapter


def ensure_default_adapters() -> None:
    global _REGISTERED
    if _REGISTERED:
        return
    from .providers.anthropic import AnthropicMessagesAdapter
    from .providers.gemini import GeminiGenerateContentAdapter
    from .providers.openai_chat import OpenAIChatCompletionsAdapter
    from .providers.openai_responses import OpenAIResponsesAdapter

    register_adapter(OpenAIChatCompletionsAdapter())
    register_adapter(OpenAIResponsesAdapter())
    register_adapter(AnthropicMessagesAdapter())
    register_adapter(GeminiGenerateContentAdapter())
    _REGISTERED = True


def list_adapter_types() -> list[str]:
    ensure_default_adapters()
    return sorted(_ADAPTERS.keys())


def get_adapter(protocol_or_provider: str) -> ModelProviderAdapter:
    ensure_default_adapters()
    key = protocol_or_provider.strip()
    if key in _ADAPTERS:
        return _ADAPTERS[key]
    # 允许按 provider 取默认 adapter
    if key == PROVIDER_OPENAI:
        return _ADAPTERS[PROTOCOL_OPENAI_CHAT_COMPLETIONS]
    if key == PROVIDER_ANTHROPIC:
        return _ADAPTERS[PROTOCOL_ANTHROPIC_MESSAGES]
    if key == PROVIDER_GEMINI:
        return _ADAPTERS[PROTOCOL_GEMINI_GENERATE_CONTENT]
    raise ModelError(
        code=ModelErrorCode.CONFIGURATION_ERROR,
        message=f"未注册的 Adapter：{protocol_or_provider}",
    )


def build_runtime(profile: ProviderProfile, model: ModelDescriptor) -> ModelRuntime:
    protocol = profile.resolve_protocol(model.protocol)
    if protocol != model.protocol:
        identity = ModelIdentity(
            profile_id=model.profile_id,
            provider=profile.provider,
            protocol=protocol,
            model_id=model.model_id,
            catalog_key=model.identity.catalog_key,
        )
        model = ModelDescriptor(
            identity=identity,
            display_name=model.display_name,
            capabilities=model.capabilities,
            context_window_tokens=model.context_window_tokens,
            max_output_tokens=model.max_output_tokens,
            temperature=model.temperature,
            aliases=model.aliases,
            description=model.description,
            tags=model.tags,
            provider_options=model.provider_options,
            source=model.source,
            sort_order=model.sort_order,
            enabled=model.enabled,
        )
    adapter = get_adapter(protocol)
    return adapter.create_runtime(profile, model)


def protocol_for_provider(provider: str, preferred: str = "") -> str:
    profile = ProviderProfile(id="tmp", provider=provider, default_protocol=preferred)
    return profile.resolve_protocol(preferred)


def _validate_protocol_matches_provider(provider: str, protocol: str) -> None:
    mapping = {
        PROVIDER_OPENAI: {
            PROTOCOL_OPENAI_CHAT_COMPLETIONS,
            PROTOCOL_OPENAI_RESPONSES,
        },
        PROVIDER_ANTHROPIC: {PROTOCOL_ANTHROPIC_MESSAGES},
        PROVIDER_GEMINI: {PROTOCOL_GEMINI_GENERATE_CONTENT},
    }
    allowed = mapping.get(provider)
    if allowed is None:
        raise ModelError(
            code=ModelErrorCode.CONFIGURATION_ERROR,
            message=f"未知 Provider：{provider}",
        )
    if protocol not in allowed:
        raise ModelError(
            code=ModelErrorCode.CONFIGURATION_ERROR,
            message=f"协议 {protocol} 与 Provider {provider} 不匹配。",
        )


def with_merged_capabilities(
    descriptor: ModelDescriptor,
    *layers: ModelCapabilities | None,
) -> ModelDescriptor:
    return ModelDescriptor(
        identity=descriptor.identity,
        display_name=descriptor.display_name,
        capabilities=merge_capabilities(descriptor.capabilities, *layers),
        context_window_tokens=descriptor.context_window_tokens,
        max_output_tokens=descriptor.max_output_tokens,
        temperature=descriptor.temperature,
        aliases=descriptor.aliases,
        description=descriptor.description,
        tags=descriptor.tags,
        provider_options=descriptor.provider_options,
        source=descriptor.source,
        sort_order=descriptor.sort_order,
        enabled=descriptor.enabled,
    )
