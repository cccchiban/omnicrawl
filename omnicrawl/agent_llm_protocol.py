from __future__ import annotations

import hashlib
import json
import re
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Callable, Iterable

from .agent_types import AgentModelReply, ToolCall, ToolDefinition
from .llm import OpenAIResponseLLM, VALID_REASONING_EFFORTS


class AgentProtocolError(RuntimeError):
    """LLM 协议层失败；调用方负责转换成对外的 AgentError。"""


class EmptyAgentReply(AgentProtocolError):
    """网关请求成功但没有返回可用文本，交由上层按策略重试。"""


class RetryableAgentRequestError(AgentProtocolError):
    """模型请求遇到临时连接或服务端错误，可按请求重试策略重新发起。"""


@dataclass(frozen=True)
class AgentLLMProtocol:
    """封装 Chat Completions 流式协议和 tool call 聚合逻辑。

    LocalToolAgent 仍负责系统提示词、工具定义和配置生命周期；本类只处理
    一次或多次模型请求中的协议细节，避免主运行循环直接操作 SDK 流事件。
    """

    client: Any
    model: str
    request_timeout_seconds: int
    request_retry_count: int
    workspace_root: Path
    system_prompt_provider: Callable[[], str]
    prompt_cache_identity_provider: Callable[[], dict[str, str]]
    tools_provider: Callable[[], list[dict[str, Any]]]
    extra_body_provider: Callable[[], dict[str, Any]]
    tool_name_from_function_name: Callable[[str], str]
    function_name_for_tool: Callable[[str], str]

    def request_reply(
        self,
        messages: list[dict[str, Any]],
        on_delta: Callable[[str], None],
        on_token_usage: Callable[[int, int, int], None],
        on_protocol_wait: Callable[[], None],
        on_retry_status: Callable[[str], None],
    ) -> AgentModelReply:
        """请求模型给出下一步：要么返回 tool_calls，要么输出最终回答。"""

        last_retryable_error: Exception | None = None
        for attempt in range(1, self.request_retry_count + 1):
            try:
                return self.request_reply_once(
                    messages,
                    on_delta,
                    on_token_usage,
                    on_protocol_wait,
                )
            except EmptyAgentReply as exc:
                last_retryable_error = exc
                if attempt < self.request_retry_count:
                    continue
                raise AgentProtocolError(
                    f"Agent 连续 {self.request_retry_count} 次返回空响应，已停止本轮请求。"
                ) from exc
            except RetryableAgentRequestError as exc:
                last_retryable_error = exc
                if attempt < self.request_retry_count:
                    on_retry_status(
                        f"模型请求中断，正在重试 {attempt + 1}/{self.request_retry_count}：{exc}"
                    )
                    continue
                raise AgentProtocolError(f"Agent 模型请求中断：{exc}") from exc

        raise AgentProtocolError(
            f"Agent 连续 {self.request_retry_count} 次返回空响应，已停止本轮请求。"
        ) from last_retryable_error

    def request_reply_once(
        self,
        messages: list[dict[str, Any]],
        on_delta: Callable[[str], None],
        on_token_usage: Callable[[int, int, int], None],
        on_protocol_wait: Callable[[], None],
    ) -> AgentModelReply:
        """执行一次 Chat Completions 流式工具调用请求。"""

        system_prompt = self.system_prompt_provider()
        request_kwargs: dict[str, Any] = {
            "model": self.model,
            "messages": [{"role": "system", "content": system_prompt}, *messages],
            "tools": self.tools_provider(),
            "tool_choice": "auto",
            "stream": True,
            "extra_body": self.extra_body_provider(),
            "timeout": self.request_timeout_seconds,
        }
        prompt_cache_key = build_prompt_cache_key(
            self.prompt_cache_identity_provider(),
            model=self.model,
        )
        if prompt_cache_key:
            request_kwargs["prompt_cache_key"] = prompt_cache_key

        try:
            stream = self.client.chat.completions.create(**request_kwargs)
        except Exception as exc:
            if "prompt_cache_key" in request_kwargs and is_unsupported_prompt_cache_error(exc):
                request_kwargs.pop("prompt_cache_key", None)
                try:
                    stream = self.client.chat.completions.create(**request_kwargs)
                except Exception as retry_exc:
                    raise AgentProtocolError(
                        f"Agent 请求失败：{OpenAIResponseLLM.format_request_error(retry_exc)}"
                    ) from retry_exc
            elif is_retryable_model_request_error(exc):
                raise RetryableAgentRequestError(OpenAIResponseLLM.format_request_error(exc)) from exc
            else:
                raise AgentProtocolError(
                    f"Agent 请求失败：{OpenAIResponseLLM.format_request_error(exc)}"
                ) from exc

        content_parts: list[str] = []
        reasoning_parts: list[str] = []
        tool_call_delta_buffers: dict[int, dict[str, Any]] = {}
        latest_usage: tuple[int, int, int] | None = None
        has_streamed_visible = False
        protocol_wait_sent = False

        try:
            for event in stream:
                usage = OpenAIResponseLLM.extract_token_usage(event)
                if usage is not None:
                    latest_usage = usage

                delta = extract_stream_delta(event)
                if delta is None:
                    continue

                delta_content = read_attr_or_key(delta, "content")
                if isinstance(delta_content, str) and delta_content:
                    content_parts.append(delta_content)
                    on_delta(delta_content)
                    has_streamed_visible = True

                delta_reasoning = read_attr_or_key(delta, "reasoning_content")
                if isinstance(delta_reasoning, str):
                    reasoning_parts.append(delta_reasoning)

                tc_deltas = read_attr_or_key(delta, "tool_calls")
                if isinstance(tc_deltas, list) and tc_deltas:
                    if has_streamed_visible and not protocol_wait_sent:
                        on_protocol_wait()
                        protocol_wait_sent = True
                    accumulate_tool_call_deltas(tc_deltas, tool_call_delta_buffers)
        except Exception as exc:
            raise RetryableAgentRequestError(OpenAIResponseLLM.format_request_error(exc)) from exc

        if latest_usage is not None:
            on_token_usage(*latest_usage)

        content = "".join(content_parts)
        reasoning = "".join(reasoning_parts).strip()
        tool_calls = build_tool_calls_from_deltas(
            tool_call_delta_buffers,
            tool_name_from_function_name=self.tool_name_from_function_name,
        )

        if not content.strip() and not tool_calls:
            raise EmptyAgentReply("Agent 返回内容为空，且未返回工具调用。")

        message = assistant_tool_call_message(
            {},
            content,
            tool_calls,
            reasoning,
            function_name_for_tool=self.function_name_for_tool,
        )
        return AgentModelReply(
            message=message,
            content=content,
            tool_calls=tool_calls,
            reasoning=reasoning,
            content_streamed=has_streamed_visible,
        )


def is_openai_gpt_model(model: str) -> bool:
    """只为 OpenAI GPT 系列模型启用官方 prompt_cache_key 参数。"""

    return model.startswith("gpt-") or model.startswith("chatgpt-") or bool(re.match(r"^o\d", model))


def is_unsupported_prompt_cache_error(exc: Exception) -> bool:
    """兼容网关不认识 prompt_cache_key 时，自动移除该参数重试一次。"""

    message = str(exc).lower()
    return (
        "prompt_cache_key" in message
        and any(
            marker in message
            for marker in (
                "unknown",
                "unsupported",
                "unexpected",
                "unrecognized",
                "extra",
                "invalid",
                "not permitted",
            )
        )
    )


def is_retryable_model_request_error(exc: Exception) -> bool:
    """识别请求建立阶段可直接重试的临时模型服务错误。"""

    status_code = getattr(exc, "status_code", None)
    if isinstance(status_code, int) and status_code in {408, 409, 500, 502, 503, 504}:
        return True

    message = str(exc).lower()
    return any(
        marker in message
        for marker in (
            "peer closed connection",
            "incomplete chunked read",
            "remote protocol error",
            "server disconnected",
            "connection reset",
            "connection aborted",
            "broken pipe",
            "timeout",
            "timed out",
            "readtimeout",
            "connecttimeout",
        )
    )


def build_prompt_cache_key(identity: dict[str, str], *, model: str) -> str:
    """为 GPT/OpenAI 请求提供稳定缓存路由 key。

    key 只来自稳定上下文身份：prompt 版本、模型、工作区、项目规范 hash、
    Skill 索引/手动 Skill hash 和工具 schema hash。它不读取当前 user、历史
    消息或工具结果，避免请求态内容打散缓存路由。
    """

    normalized_model = model.strip().lower()
    if not is_openai_gpt_model(normalized_model):
        return ""

    stable_identity = dict(identity)
    stable_identity["model"] = model.strip()
    digest = hashlib.sha256(
        json.dumps(
            stable_identity,
            ensure_ascii=False,
            sort_keys=True,
            separators=(",", ":"),
        ).encode("utf-8")
    ).hexdigest()[:32]
    return f"local-agent-{digest}"


def build_extra_body(llm_config: Any) -> dict[str, Any]:
    """构造网关扩展参数；根据 reasoning_effort 决定是否启用思考模式。"""

    thinking_type = "enabled" if llm_config.thinking_enabled else "disabled"
    body: dict[str, Any] = {"thinking": {"type": thinking_type}}
    if llm_config.thinking_enabled and llm_config.reasoning_effort:
        if llm_config.reasoning_effort in VALID_REASONING_EFFORTS:
            body["reasoning_effort"] = llm_config.reasoning_effort
    return body


def parse_tool_arguments(raw_arguments: Any) -> dict[str, Any]:
    if isinstance(raw_arguments, dict):
        return raw_arguments
    if isinstance(raw_arguments, str) and raw_arguments.strip():
        try:
            parsed = json.loads(raw_arguments)
        except json.JSONDecodeError:
            return {}
        return parsed if isinstance(parsed, dict) else {}
    return {}


def read_attr_or_key(value: Any, key: str) -> Any:
    if value is None:
        return None
    attr = getattr(value, key, None)
    if attr is not None:
        return attr
    if isinstance(value, dict):
        return value.get(key)
    if hasattr(value, "model_dump"):
        data = value.model_dump()
        return data.get(key) if isinstance(data, dict) else None
    return None


def extract_stream_delta(event: Any) -> Any | None:
    """从流式事件中提取 choices[0].delta，兼容 SDK 模型与字典。"""

    choices = getattr(event, "choices", None)
    if isinstance(choices, list) and choices:
        delta = getattr(choices[0], "delta", None)
        if delta is not None:
            return delta
        first = choices[0]
        if isinstance(first, dict):
            return first.get("delta")
    elif isinstance(event, dict):
        choices_data = event.get("choices")
        if isinstance(choices_data, list) and choices_data:
            first = choices_data[0]
            if isinstance(first, dict):
                return first.get("delta")
    return None


def accumulate_tool_call_deltas(
    tc_deltas: list[Any],
    buffers: dict[int, dict[str, Any]],
) -> None:
    """把流式 tool_calls 增量块按 index 累积到缓冲区。"""

    for tc in tc_deltas:
        idx = read_attr_or_key(tc, "index")
        if not isinstance(idx, int):
            idx = 0
        if idx not in buffers:
            buffers[idx] = {
                "id": "",
                "function": {"name": "", "arguments": ""},
            }
        buf = buffers[idx]
        tc_id = read_attr_or_key(tc, "id")
        if tc_id:
            buf["id"] = str(tc_id)
        func = read_attr_or_key(tc, "function")
        if isinstance(func, dict):
            fn_name = func.get("name")
            if fn_name:
                buf["function"]["name"] += str(fn_name)
            fn_args = func.get("arguments")
            if fn_args:
                buf["function"]["arguments"] += str(fn_args)
        elif func is not None:
            fn_name = getattr(func, "name", None)
            if fn_name:
                buf["function"]["name"] += str(fn_name)
            fn_args = getattr(func, "arguments", None)
            if fn_args:
                buf["function"]["arguments"] += str(fn_args)


def build_tool_calls_from_deltas(
    buffers: dict[int, dict[str, Any]],
    *,
    tool_name_from_function_name: Callable[[str], str],
) -> list[ToolCall]:
    """把累积的流式 tool_call 增量块解析为结构化 ToolCall 列表。"""

    calls: list[ToolCall] = []
    for idx in sorted(buffers.keys()):
        buf = buffers[idx]
        fn_name = buf["function"]["name"].strip()
        if not fn_name:
            continue
        calls.append(
            ToolCall(
                name=tool_name_from_function_name(fn_name),
                arguments=parse_tool_arguments(buf["function"]["arguments"]),
                id=buf["id"],
                function_name=fn_name,
            )
        )
    return calls


def assistant_tool_call_message(
    raw_message: Any,
    content: str,
    tool_calls: list[ToolCall],
    reasoning: str,
    *,
    function_name_for_tool: Callable[[str], str],
) -> dict[str, Any]:
    message: dict[str, Any] = {"role": "assistant", "content": content or None}
    if reasoning:
        message["reasoning_content"] = reasoning
    if tool_calls:
        message["tool_calls"] = [
            {
                "id": tool_call.id,
                "type": "function",
                "function": {
                    "name": tool_call.function_name or function_name_for_tool(tool_call.name),
                    "arguments": json.dumps(tool_call.arguments, ensure_ascii=False),
                },
            }
            for tool_call in tool_calls
        ]
    return message


def chat_completion_tools(
    tools: Iterable[ToolDefinition],
    *,
    function_name_for_tool: Callable[[str], str],
) -> list[dict[str, Any]]:
    return [
        {
            "type": "function",
            "function": {
                "name": function_name_for_tool(tool.name),
                "description": tool.description,
                "parameters": tool_parameters_schema(tool),
            },
        }
        for tool in tools
    ]


def function_name_for_tool(tool_name: str) -> str:
    readable = re.sub(r"[^A-Za-z0-9_]+", "_", tool_name).strip("_").lower()
    readable = readable or "tool"
    digest = hashlib.sha1(tool_name.encode("utf-8")).hexdigest()[:10]
    return f"tool_{readable[:40]}_{digest}"


def tool_name_from_function_name(
    function_name: str,
    tool_names: Iterable[str],
    *,
    function_name_for_tool_callback: Callable[[str], str] = function_name_for_tool,
) -> str:
    for tool_name in tool_names:
        if function_name_for_tool_callback(tool_name) == function_name:
            return tool_name
    return function_name


def tool_parameters_schema(tool: ToolDefinition) -> dict[str, Any]:
    try:
        raw_schema = json.loads(tool.argument_schema)
    except json.JSONDecodeError:
        raw_schema = {}
    if not isinstance(raw_schema, dict):
        raw_schema = {}
    if raw_schema.get("type") == "object" and isinstance(raw_schema.get("properties"), dict):
        schema = dict(raw_schema)
    else:
        properties = {
            key: infer_tool_property_schema(value)
            for key, value in raw_schema.items()
            if isinstance(key, str)
        }
        schema = {
            "type": "object",
            "properties": properties,
        }
    schema.setdefault("type", "object")
    schema.setdefault("properties", {})
    return schema


def infer_tool_property_schema(example: Any) -> dict[str, Any]:
    if isinstance(example, bool):
        return {"type": "boolean"}
    if isinstance(example, int) and not isinstance(example, bool):
        return {"type": "integer"}
    if isinstance(example, (float, int)) and not isinstance(example, bool):
        return {"type": "number"}
    if isinstance(example, list):
        return {"type": "array", "items": {"type": "string"}}
    if isinstance(example, dict):
        return {"type": "object"}
    return {"type": "string"}
