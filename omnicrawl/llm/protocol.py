"""Provider 无关的模型请求、流事件与消息块类型。"""

from __future__ import annotations

from dataclasses import dataclass, field
import re
from typing import Any, Callable, Iterable, Mapping, Protocol, Union


PROTOCOL_OPENAI_RESPONSES = "openai_responses"
PROTOCOL_OPENAI_CHAT_COMPLETIONS = "openai_chat_completions"
PROTOCOL_ANTHROPIC_MESSAGES = "anthropic_messages"
PROTOCOL_GEMINI_GENERATE_CONTENT = "gemini_generate_content"

SUPPORTED_PROTOCOLS = frozenset(
    {
        PROTOCOL_OPENAI_RESPONSES,
        PROTOCOL_OPENAI_CHAT_COMPLETIONS,
        PROTOCOL_ANTHROPIC_MESSAGES,
        PROTOCOL_GEMINI_GENERATE_CONTENT,
    }
)

PROVIDER_OPENAI = "openai"
PROVIDER_ANTHROPIC = "anthropic"
PROVIDER_GEMINI = "gemini"

PROVIDER_DEFAULT_PROTOCOL = {
    PROVIDER_OPENAI: PROTOCOL_OPENAI_CHAT_COMPLETIONS,
    PROVIDER_ANTHROPIC: PROTOCOL_ANTHROPIC_MESSAGES,
    PROVIDER_GEMINI: PROTOCOL_GEMINI_GENERATE_CONTENT,
}


@dataclass(frozen=True)
class ModelIdentity:
    """模型唯一身份：同一 model_id 可来自不同 Profile / 协议。"""

    profile_id: str
    provider: str
    protocol: str
    model_id: str
    catalog_key: str = ""

    @property
    def triple(self) -> tuple[str, str, str]:
        return (self.profile_id, self.protocol, self.model_id)

    def as_ref(self) -> str:
        if self.catalog_key:
            return self.catalog_key
        return f"{self.profile_id}/{self.model_id}"


@dataclass(frozen=True)
class TextBlock:
    text: str


@dataclass(frozen=True)
class ImageBlock:
    """Provider 无关的内联图片块。

    当前只接收 Host 生成的 Base64 数据，不支持远程 URL，避免模型请求在未审批的
    情况下触发额外网络读取。图片仅存在于当前工具循环，不写入长期会话历史。
    """

    media_type: str
    data_base64: str
    detail: str = "auto"

    @property
    def data_url(self) -> str:
        return f"data:{self.media_type};base64,{self.data_base64}"


@dataclass(frozen=True)
class ToolCallBlock:
    call_id: str
    name: str
    arguments: dict[str, Any]
    provider_call_id: str = ""


@dataclass(frozen=True)
class ToolResultBlock:
    call_id: str
    ok: bool
    content: str


MessageBlock = Union[TextBlock, ImageBlock, ToolCallBlock, ToolResultBlock]


@dataclass(frozen=True)
class ConversationMessage:
    role: str  # user | assistant | tool | system
    blocks: tuple[MessageBlock, ...] = ()
    reasoning: str = ""  # 思考模式思维链；回传历史时须原样携带
    tools: tuple[ToolSpec, ...] = ()  # system 消息可携带动态加载的工具声明

    @property
    def text(self) -> str:
        parts = [block.text for block in self.blocks if isinstance(block, TextBlock)]
        return "".join(parts)


@dataclass(frozen=True)
class ToolSpec:
    name: str
    description: str
    parameters: dict[str, Any]


@dataclass(frozen=True)
class GenerationOptions:
    max_output_tokens: int | None = None
    temperature: float | None = None
    reasoning_effort: str = ""
    request_timeout_seconds: float = 180.0
    request_retry_count: int = 5
    provider_options: Mapping[str, Any] = field(default_factory=dict)


@dataclass(frozen=True)
class TokenUsage:
    input_tokens: int = 0
    output_tokens: int = 0
    cached_input_tokens: int = 0
    reasoning_tokens: int = 0

    def as_tuple(self) -> tuple[int, int, int]:
        """兼容旧 UI 回调签名 (input, output, cached_input)。"""

        return self.input_tokens, self.output_tokens, self.cached_input_tokens


@dataclass(frozen=True)
class TextDelta:
    text: str


@dataclass(frozen=True)
class ReasoningDelta:
    text: str


@dataclass(frozen=True)
class ToolCallStarted:
    call_id: str
    name: str


@dataclass(frozen=True)
class ToolCallArgumentsDelta:
    call_id: str
    delta: str


@dataclass(frozen=True)
class ToolCallCompleted:
    call_id: str
    name: str
    arguments: dict[str, Any]


@dataclass(frozen=True)
class UsageUpdated:
    input_tokens: int
    output_tokens: int
    cached_input_tokens: int = 0
    reasoning_tokens: int = 0

    def to_usage(self) -> TokenUsage:
        return TokenUsage(
            input_tokens=self.input_tokens,
            output_tokens=self.output_tokens,
            cached_input_tokens=self.cached_input_tokens,
            reasoning_tokens=self.reasoning_tokens,
        )


@dataclass(frozen=True)
class ResponseCompleted:
    finish_reason: str = "stop"


@dataclass(frozen=True)
class ProviderWarning:
    code: str
    message: str


ModelStreamEvent = Union[
    TextDelta,
    ReasoningDelta,
    ToolCallStarted,
    ToolCallArgumentsDelta,
    ToolCallCompleted,
    UsageUpdated,
    ResponseCompleted,
    ProviderWarning,
]


@dataclass(frozen=True)
class ModelTurnRequest:
    identity: ModelIdentity
    system_prompt: str
    messages: tuple[ConversationMessage, ...]
    tools: tuple[ToolSpec, ...] = ()
    generation_options: GenerationOptions = field(default_factory=GenerationOptions)
    prompt_cache_identity: Mapping[str, str] = field(default_factory=dict)


@dataclass(frozen=True)
class ModelTurnResult:
    assistant_message: ConversationMessage
    content: str
    reasoning: str = ""
    tool_calls: tuple[ToolCallBlock, ...] = ()
    usage: TokenUsage | None = None
    finish_reason: str = "stop"
    content_streamed: bool = False
    warnings: tuple[ProviderWarning, ...] = ()


class ModelRuntime(Protocol):
    identity: ModelIdentity
    capabilities: Any

    def stream_turn(
        self,
        request: ModelTurnRequest,
        *,
        cancel_check: Callable[[], None] | None = None,
    ) -> Iterable[ModelStreamEvent]:
        ...

    def close(self) -> None:
        ...


class ModelProviderAdapter(Protocol):
    provider_type: str

    def create_runtime(self, profile: Any, model: Any) -> ModelRuntime:
        ...

    def discover_models(
        self,
        profile: Any,
        *,
        timeout_seconds: float,
    ) -> Any:
        ...


def aggregate_stream_events(
    events: Iterable[ModelStreamEvent],
) -> ModelTurnResult:
    """把 Adapter 流事件归并为一次 ModelTurnResult。"""

    content_parts: list[str] = []
    reasoning_parts: list[str] = []
    tool_calls: list[ToolCallBlock] = []
    pending: dict[str, dict[str, Any]] = {}
    usage: TokenUsage | None = None
    finish_reason = "stop"
    content_streamed = False
    warnings: list[ProviderWarning] = []

    for event in events:
        if isinstance(event, TextDelta):
            if event.text:
                content_parts.append(event.text)
                content_streamed = True
        elif isinstance(event, ReasoningDelta):
            if event.text:
                reasoning_parts.append(event.text)
        elif isinstance(event, ToolCallStarted):
            pending[event.call_id] = {
                "call_id": event.call_id,
                "name": event.name,
                "arguments": "",
            }
        elif isinstance(event, ToolCallArgumentsDelta):
            buf = pending.setdefault(
                event.call_id,
                {"call_id": event.call_id, "name": "", "arguments": ""},
            )
            buf["arguments"] = str(buf.get("arguments", "")) + event.delta
        elif isinstance(event, ToolCallCompleted):
            tool_calls.append(
                ToolCallBlock(
                    call_id=event.call_id,
                    name=event.name,
                    arguments=dict(event.arguments or {}),
                )
            )
            pending.pop(event.call_id, None)
        elif isinstance(event, UsageUpdated):
            usage = event.to_usage()
        elif isinstance(event, ResponseCompleted):
            finish_reason = event.finish_reason or "stop"
        elif isinstance(event, ProviderWarning):
            warnings.append(event)

    # 未以 Completed 结束但已有缓冲的工具调用：尽量解析 JSON 参数。
    for call_id, buf in pending.items():
        raw_args = buf.get("arguments", "")
        arguments: dict[str, Any] = {}
        if isinstance(raw_args, str) and raw_args.strip():
            try:
                import json

                parsed = json.loads(raw_args)
                if isinstance(parsed, dict):
                    arguments = parsed
            except Exception:
                arguments = {}
        name = str(buf.get("name") or "").strip()
        if name:
            tool_calls.append(
                ToolCallBlock(call_id=str(call_id), name=name, arguments=arguments)
            )

    content = "".join(content_parts)
    reasoning = "".join(reasoning_parts).strip()
    blocks: list[MessageBlock] = []
    if content:
        blocks.append(TextBlock(text=content))
    blocks.extend(tool_calls)
    return ModelTurnResult(
        assistant_message=ConversationMessage(role="assistant", blocks=tuple(blocks)),
        content=content,
        reasoning=reasoning,
        tool_calls=tuple(tool_calls),
        usage=usage,
        finish_reason=finish_reason,
        content_streamed=content_streamed,
        warnings=tuple(warnings),
    )


def conversation_from_openai_messages(
    messages: list[dict[str, Any]],
) -> tuple[ConversationMessage, ...]:
    """把现有 OpenAI Chat 风格历史转换为统一消息块（兼容迁移期）。"""

    converted: list[ConversationMessage] = []
    for message in messages:
        role = str(message.get("role") or "user")
        if role == "tool":
            call_id = str(message.get("tool_call_id") or message.get("id") or "")
            content = message.get("content")
            text = content if isinstance(content, str) else ""
            converted.append(
                ConversationMessage(
                    role="tool",
                    blocks=(ToolResultBlock(call_id=call_id, ok=True, content=text),),
                )
            )
            continue

        tools: tuple[ToolSpec, ...] = ()
        raw_tools = message.get("tools")
        if role == "system" and isinstance(raw_tools, list):
            tools = tuple(
                spec
                for item in raw_tools
                if (spec := _tool_spec_from_openai_item(item)) is not None
            )

        blocks: list[MessageBlock] = []
        content = message.get("content")
        if isinstance(content, str) and content:
            blocks.append(TextBlock(text=content))
        elif isinstance(content, list):
            blocks.extend(_blocks_from_openai_content_parts(content))
        tool_calls = message.get("tool_calls")
        if isinstance(tool_calls, list):
            for item in tool_calls:
                if not isinstance(item, dict):
                    continue
                function = item.get("function") if isinstance(item.get("function"), dict) else {}
                name = str(function.get("name") or "")
                raw_args = function.get("arguments") or "{}"
                arguments: dict[str, Any] = {}
                if isinstance(raw_args, dict):
                    arguments = raw_args
                elif isinstance(raw_args, str) and raw_args.strip():
                    try:
                        import json

                        parsed = json.loads(raw_args)
                        if isinstance(parsed, dict):
                            arguments = parsed
                    except Exception:
                        arguments = {}
                call_id = str(item.get("id") or "")
                if name:
                    blocks.append(
                        ToolCallBlock(call_id=call_id, name=name, arguments=arguments)
                    )
        if blocks or role in {"user", "assistant", "system"}:
            reasoning = message.get("reasoning_content")
            converted.append(
                ConversationMessage(
                    role=role,
                    blocks=tuple(blocks),
                    # 思考模式网关要求历史 assistant 消息回传 reasoning_content，
                    # 转换时保留以免二次请求被拒（HTTP 400）。
                    reasoning=reasoning if isinstance(reasoning, str) else "",
                    tools=tools,
                )
            )
    return tuple(converted)


def _tool_spec_from_openai_item(item: Any) -> ToolSpec | None:
    """把 Chat Completions 风格 tools 数组项转成统一 ToolSpec。"""

    if not isinstance(item, dict):
        return None
    function = item.get("function") if isinstance(item.get("function"), dict) else item
    if not isinstance(function, dict):
        return None
    name = str(function.get("name") or "").strip()
    if not name:
        return None
    parameters = function.get("parameters")
    if not isinstance(parameters, dict):
        parameters = {"type": "object", "properties": {}}
    return ToolSpec(
        name=name,
        description=str(function.get("description") or ""),
        parameters=dict(parameters),
    )


def tools_from_conversation_messages(
    messages: Iterable[ConversationMessage],
) -> tuple[ToolSpec, ...]:
    """收集 system 消息中携带的动态工具声明，供不支持消息内 tools 的 Provider 合并。

    按工具名去重：同一工具在多次搜索加载中重复出现时只合并一次，避免请求级
    tools 出现重复函数声明。
    """

    seen: set[str] = set()
    result: list[ToolSpec] = []
    for message in messages:
        if message.role != "system" or not message.tools:
            continue
        for tool in message.tools:
            if tool.name in seen:
                continue
            seen.add(tool.name)
            result.append(tool)
    return tuple(result)


_DATA_IMAGE_URL_PATTERN = re.compile(
    r"^data:(image/(?:png|jpeg|webp|gif));base64,([A-Za-z0-9+/=]+)$",
    re.IGNORECASE,
)


def _blocks_from_openai_content_parts(parts: list[Any]) -> list[MessageBlock]:
    """解析 Host 内部使用的 OpenAI 风格文本/图片内容块。"""

    blocks: list[MessageBlock] = []
    for part in parts:
        if not isinstance(part, dict):
            continue
        part_type = str(part.get("type") or "").strip().casefold()
        if part_type in {"text", "input_text"}:
            text = part.get("text")
            if isinstance(text, str) and text:
                blocks.append(TextBlock(text=text))
            continue
        if part_type not in {"image_url", "input_image"}:
            continue
        image_url = part.get("image_url")
        detail = str(part.get("detail") or "auto")
        if isinstance(image_url, dict):
            detail = str(image_url.get("detail") or detail)
            image_url = image_url.get("url")
        if not isinstance(image_url, str):
            continue
        match = _DATA_IMAGE_URL_PATTERN.fullmatch(image_url.strip())
        if match is None:
            continue
        blocks.append(
            ImageBlock(
                media_type=match.group(1).lower(),
                data_base64=match.group(2),
                detail=detail if detail in {"auto", "low", "high"} else "auto",
            )
        )
    return blocks
