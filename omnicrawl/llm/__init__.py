"""统一多模型运行时：Provider 无关协议、Runtime Manager 与适配器入口。

兼容既有 `from omnicrawl.llm import LLMConfig / OpenAIResponseLLM` 导入路径，
真正的配置读写仍位于 `omnicrawl.config.models.llm`。
"""

from __future__ import annotations

from ..config.models.llm import (
    DEFAULT_REASONING_EFFORT,
    DEFAULT_THINKING_TYPE,
    KNOWN_AVAILABLE_MODELS,
    LLMConfig,
    LLMError,
    VALID_REASONING_EFFORTS,
    load_llm_config,
    normalize_reasoning_effort,
    save_context_window_tokens,
    save_reasoning_effort,
)
from ..config.models.llm_client import OpenAIResponseLLM
from .capabilities import ModelCapabilities, merge_capabilities
from .errors import ModelError, ModelErrorCode, map_openai_exception
from .protocol import (
    PROTOCOL_ANTHROPIC_MESSAGES,
    PROTOCOL_GEMINI_GENERATE_CONTENT,
    PROTOCOL_OPENAI_CHAT_COMPLETIONS,
    PROTOCOL_OPENAI_RESPONSES,
    ConversationMessage,
    GenerationOptions,
    ModelIdentity,
    ModelStreamEvent,
    ModelTurnRequest,
    ModelTurnResult,
    ProviderWarning,
    ReasoningDelta,
    ResponseCompleted,
    TextBlock,
    TextDelta,
    TokenUsage,
    ToolCallBlock,
    ToolCallArgumentsDelta,
    ToolCallCompleted,
    ToolCallStarted,
    ToolResultBlock,
    ToolSpec,
    UsageUpdated,
)
from .registry import (
    ModelDescriptor,
    ProviderProfile,
    build_runtime,
    get_adapter,
    list_adapter_types,
)
from .runtime import ModelRuntimeManager, RuntimeSnapshot

__all__ = [
    "DEFAULT_REASONING_EFFORT",
    "DEFAULT_THINKING_TYPE",
    "KNOWN_AVAILABLE_MODELS",
    "LLMConfig",
    "LLMError",
    "ModelCapabilities",
    "ModelDescriptor",
    "ModelError",
    "ModelErrorCode",
    "ModelIdentity",
    "ModelRuntimeManager",
    "ModelStreamEvent",
    "ModelTurnRequest",
    "ModelTurnResult",
    "OpenAIResponseLLM",
    "PROTOCOL_ANTHROPIC_MESSAGES",
    "PROTOCOL_GEMINI_GENERATE_CONTENT",
    "PROTOCOL_OPENAI_CHAT_COMPLETIONS",
    "PROTOCOL_OPENAI_RESPONSES",
    "ProviderProfile",
    "ProviderWarning",
    "ReasoningDelta",
    "ResponseCompleted",
    "RuntimeSnapshot",
    "TextBlock",
    "TextDelta",
    "TokenUsage",
    "ToolCallArgumentsDelta",
    "ToolCallBlock",
    "ToolCallCompleted",
    "ToolCallStarted",
    "ToolResultBlock",
    "ToolSpec",
    "UsageUpdated",
    "VALID_REASONING_EFFORTS",
    "build_runtime",
    "get_adapter",
    "list_adapter_types",
    "load_llm_config",
    "map_openai_exception",
    "merge_capabilities",
    "normalize_reasoning_effort",
    "save_context_window_tokens",
    "save_reasoning_effort",
]
