#!/usr/bin/env python3
"""生成 OpenAI Responses **请求构建**的对照数据集（`providers/openai_responses.py`）。

期望值来自 Python 真实现：
- `messages_to_responses_input`（会话消息 → `input` items，含 reasoning item 的 SHA-1 id）；
- `_tools_for_responses`（请求 tools 在前、消息携带的声明在后）；
- `_flatten_tool_history_to_text` 与 `_has_tool_history_items` / `_is_tool_history_rejection`；
- `_build_responses_kwargs` 经真 Adapter 构造的 runtime 调用，拆成线上请求体与传输层 timeout。

用法：``python rust/tools/gen_llm_responses_fixture.py``
输出：``rust/crates/omnicrawl-llm/tests/fixtures/openai_responses_request_parity.json``
"""

from __future__ import annotations

import copy
import json
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
FIXTURE_PATH = (
    ROOT / "rust/crates/omnicrawl-llm/tests/fixtures/openai_responses_request_parity.json"
)

sys.path.insert(0, str(ROOT))

from omnicrawl.llm import protocol as P  # noqa: E402
from omnicrawl.llm import registry as R  # noqa: E402
from omnicrawl.llm.providers.openai_responses import (  # noqa: E402
    OpenAIResponsesAdapter,
    _flatten_tool_history_to_text,
    _has_tool_history_items,
    _is_tool_history_rejection,
    _tools_for_responses,
    messages_to_responses_input,
)

GPT_MODEL = "gpt-5-codex"
PLAIN_MODEL = "deepseek-chat"
PROTOCOL = P.PROTOCOL_OPENAI_RESPONSES


def to_json(message: P.ConversationMessage) -> dict:
    blocks = []
    for block in message.blocks:
        if isinstance(block, P.TextBlock):
            blocks.append({"Text": {"text": block.text}})
        elif isinstance(block, P.ImageBlock):
            blocks.append(
                {
                    "Image": {
                        "media_type": block.media_type,
                        "data_base64": block.data_base64,
                        "detail": block.detail,
                    }
                }
            )
        elif isinstance(block, P.ToolCallBlock):
            blocks.append(
                {
                    "ToolCall": {
                        "call_id": block.call_id,
                        "name": block.name,
                        "arguments": block.arguments,
                        "provider_call_id": block.provider_call_id,
                    }
                }
            )
        elif isinstance(block, P.ToolResultBlock):
            blocks.append(
                {
                    "ToolResult": {
                        "call_id": block.call_id,
                        "ok": block.ok,
                        "content": block.content,
                    }
                }
            )
        else:
            raise SystemExit("未覆盖的块类型：%r" % type(block))
    return {
        "role": message.role,
        "blocks": blocks,
        "reasoning": message.reasoning,
        "tools": [
            {
                "name": tool.name,
                "description": tool.description,
                "parameters": tool.parameters,
            }
            for tool in message.tools
        ],
    }


def text_message(role: str, text: str, *, reasoning: str = "", tools=()) -> P.ConversationMessage:
    return P.ConversationMessage(
        role=role,
        blocks=(P.TextBlock(text=text),),
        reasoning=reasoning,
        tools=tuple(tools),
    )


def image_message(role: str = "user") -> P.ConversationMessage:
    return P.ConversationMessage(
        role=role,
        blocks=(P.ImageBlock(media_type="image/png", data_base64="iVBORw0KGgo=", detail="low"),),
    )


def tool_call_message(
    *,
    text: str = "",
    reasoning: str = "",
    call_id: str = "call_1",
    provider_call_id: str = "",
    arguments=None,
) -> P.ConversationMessage:
    blocks = []
    if text:
        blocks.append(P.TextBlock(text=text))
    blocks.append(
        P.ToolCallBlock(
            call_id=call_id,
            name="write_file",
            arguments=arguments if arguments is not None else {"path": "a.txt", "内容": True},
            provider_call_id=provider_call_id,
        )
    )
    return P.ConversationMessage(role="assistant", blocks=tuple(blocks), reasoning=reasoning)


def tool_result_message(*contents) -> P.ConversationMessage:
    return P.ConversationMessage(
        role="tool",
        blocks=tuple(
            P.ToolResultBlock(call_id="call_%d" % (index + 1), ok=True, content=content)
            for index, content in enumerate(contents)
        ),
    )


TOOL = P.ToolSpec(
    name="write_file",
    description="写文件",
    parameters={"type": "object", "properties": {"path": {"type": "string"}}},
)
EMPTY_TOOL = P.ToolSpec(name="no_args", description="", parameters={})
MESSAGE_TOOL = P.ToolSpec(name="dynamic", description="动态声明", parameters={"type": "object"})

MESSAGE_CASES = [
    ("用户文本", [text_message("user", "你好")]),
    ("system 无工具声明", [text_message("system", "系统提示")]),
    ("system 带工具声明（跳过）", [text_message("system", "系统提示", tools=[MESSAGE_TOOL])]),
    ("用户图片", [image_message()]),
    ("空 blocks", [P.ConversationMessage(role="user", blocks=())]),
    ("空文本块被过滤", [text_message("user", "")]),
    ("未知角色回落 user", [text_message("developer", "自定义角色")]),
    ("assistant 纯文本", [text_message("assistant", "回复")]),
    (
        "assistant 文本 + 工具调用（带 reasoning）",
        [tool_call_message(text="先说一句", reasoning="思考中")],
    ),
    ("assistant 工具调用（reasoning 为空）", [tool_call_message()]),
    (
        "assistant 工具调用（call_id 回落 provider_call_id）",
        [tool_call_message(call_id="", provider_call_id="resp_call_9")],
    ),
    (
        "assistant 工具调用（嵌套参数）",
        [tool_call_message(arguments={"nested": {"a": [1, 2]}, "文本": "值"})],
    ),
    ("reasoning 长度 55", [tool_call_message(reasoning="x" * 55)]),
    ("reasoning 长度 56", [tool_call_message(reasoning="x" * 56)]),
    ("reasoning 长度 63", [tool_call_message(reasoning="x" * 63)]),
    ("reasoning 长度 64", [tool_call_message(reasoning="x" * 64)]),
    ("reasoning 长度 200（多块）", [tool_call_message(reasoning="长" * 200)]),
    ("工具结果（多块）", [tool_result_message("结果一", "结果二")]),
    ("工具结果（无结果块）", [P.ConversationMessage(role="tool", blocks=())]),
    (
        "一问一答一工具链",
        [
            text_message("user", "开始"),
            tool_call_message(text="调用工具", reasoning="理由"),
            tool_result_message("完成"),
        ],
    ),
]

TOOLS_CASES = [
    ("仅请求 tools", {"messages": [text_message("user", "hi")], "tools": [TOOL, EMPTY_TOOL]}),
    (
        "请求 tools + 消息携带",
        {
            "messages": [
                text_message("system", "系统", tools=[MESSAGE_TOOL]),
                text_message("user", "hi"),
            ],
            "tools": [TOOL],
        },
    ),
    ("空 tools", {"messages": [text_message("user", "hi")], "tools": []}),
    (
        "仅消息携带",
        {"messages": [text_message("system", "系统", tools=[MESSAGE_TOOL, TOOL])], "tools": []},
    ),
]

FLATTEN_CASES = [
    ("无前置 assistant", [{"type": "function_call", "name": "f", "arguments": "{}"}]),
    (
        "追加到最近 assistant",
        [
            {"role": "assistant", "content": [{"type": "output_text", "text": "先说"}]},
            {"type": "function_call", "name": "f", "arguments": '{"a":1}'},
        ],
    ),
    (
        "隔着 reasoning 仍找 assistant",
        [
            {"role": "assistant", "content": [{"type": "output_text", "text": "先说"}]},
            {"type": "reasoning", "id": "rs_x", "summary": []},
            {"type": "function_call", "name": "f", "arguments": "{}"},
        ],
    ),
    (
        "工具结果独立成 user 消息",
        [
            {"role": "user", "content": [{"type": "input_text", "text": "指令"}]},
            {"type": "function_call_output", "call_id": "c1", "output": "输出"},
        ],
    ),
    (
        "混合序列",
        [
            {"role": "user", "content": [{"type": "input_text", "text": "开始"}]},
            {"role": "assistant", "content": [{"type": "output_text", "text": "调用"}]},
            {"type": "function_call", "name": "f", "arguments": "{}"},
            {"type": "function_call_output", "call_id": "c1", "output": "o1"},
            {"type": "function_call", "name": "g", "arguments": "{}"},
        ],
    ),
    (
        "非工具 item 原样保留",
        [{"role": "user", "content": [{"type": "input_text", "text": "x"}]}],
    ),
]

REJECTION_CASES = [
    ("含工具历史 + 400", [{"type": "function_call"}], 400),
    ("含工具历史 + 500", [{"type": "function_call_output"}], 500),
    ("含工具历史 + 无状态码", [{"type": "function_call"}], None),
    ("无工具历史 + 400", [{"role": "user"}], 400),
]

KWARGS_CASES = [
    ("基础", {}),
    ("reasoning_effort=high", {"reasoning_effort": "high"}),
    ("reasoning_effort=none", {"reasoning_effort": "none"}),
    ("reasoning_effort=disabled", {"reasoning_effort": "disabled"}),
    (
        "provider_options 清洗（thinking / reasoning_effort 被剔除）",
        {
            "provider_options": {
                "thinking": {"type": "enabled"},
                "reasoning_effort": "low",
                "top_p": 0.9,
                "store": False,
            }
        },
    ),
    ("provider_options 保留白名单字段", {"provider_options": {"top_p": 0.5, "metadata": {"k": "v"}}}),
    ("max_output_tokens=0", {"max_output_tokens": 0}),
    ("max_output_tokens=4096", {"max_output_tokens": 4096}),
    ("temperature=0.0", {"temperature": 0.0}),
    ("timeout=0 回落 Profile", {"request_timeout_seconds": 0.0}),
    ("timeout=30", {"request_timeout_seconds": 30.0}),
]

RUNTIME_CACHE: dict[tuple[str, str, float], object] = {}


def runtime_for(model_id: str, protocol: str = PROTOCOL, profile_timeout: float = 180.0):
    key = (model_id, protocol, profile_timeout)
    if key not in RUNTIME_CACHE:
        profile = R.ProviderProfile(
            id="p1",
            provider="openai",
            api_key="test-key",
            default_protocol=protocol,
            request_timeout_seconds=profile_timeout,
        )
        model = R.ModelDescriptor(
            identity=P.ModelIdentity(
                profile_id="p1",
                provider="openai",
                protocol=protocol,
                model_id=model_id,
            )
        )
        RUNTIME_CACHE[key] = OpenAIResponsesAdapter().create_runtime(profile, model)
    return RUNTIME_CACHE[key]


def request_for(messages, tools, options, model_id: str, prompt_cache_identity: dict, system_prompt: str):
    identity = P.ModelIdentity(
        profile_id="p1",
        provider="openai",
        protocol=PROTOCOL,
        model_id=model_id,
    )
    return P.ModelTurnRequest(
        identity=identity,
        system_prompt=system_prompt,
        messages=tuple(messages),
        tools=tuple(tools),
        generation_options=options,
        prompt_cache_identity=prompt_cache_identity,
    )


def kwargs_entry(label, options_kwargs, *, model_id=GPT_MODEL, prompt_cache_identity=None, tools=(TOOL,), system_prompt="系统提示"):
    options = P.GenerationOptions(**options_kwargs)
    if prompt_cache_identity is None:
        prompt_cache_identity = {"workspace": "ws-1"}
    messages = [text_message("user", "hi")]
    request = request_for(messages, tools, options, model_id, prompt_cache_identity, system_prompt)
    runtime = runtime_for(model_id)
    input_items = messages_to_responses_input(tuple(messages))
    kwargs = runtime._build_responses_kwargs(
        request, tools=_tools_for_responses(request), input_items=input_items
    )
    body = {key: value for key, value in kwargs.items() if key not in {"timeout", "extra_body"}}
    body.update(kwargs["extra_body"])
    return {
        "label": label,
        "options": {
            "max_output_tokens": options.max_output_tokens,
            "temperature": options.temperature,
            "reasoning_effort": options.reasoning_effort,
            "tool_choice": options.tool_choice,
            "request_timeout_seconds": options.request_timeout_seconds,
            "provider_options": dict(options.provider_options),
        },
        "model": model_id,
        "system_prompt": system_prompt,
        "tools": [{"name": tool.name, "description": tool.description, "parameters": tool.parameters} for tool in tools],
        "messages": [to_json(message) for message in messages],
        "prompt_cache_identity": dict(prompt_cache_identity),
        "body": body,
        "timeout_seconds": kwargs["timeout"],
    }


def main() -> None:
    fixture = {
        "source": "omnicrawl/llm/providers/openai_responses.py（请求构建部分）",
        "messages": [
            {
                "label": label,
                "messages": [to_json(message) for message in messages],
                "items": messages_to_responses_input(tuple(messages)),
            }
            for label, messages in MESSAGE_CASES
        ],
        "tools": [
            {
                "label": label,
                "messages": [
                    to_json(message) if isinstance(message, P.ConversationMessage) else {"note": message}
                    for message in spec["messages"]
                ],
                "request_tools": [
                    {"name": tool.name, "description": tool.description, "parameters": tool.parameters}
                    for tool in spec["tools"]
                ],
                "expected": _tools_for_responses(
                    request_for(
                        [
                            message
                            for message in spec["messages"]
                            if isinstance(message, P.ConversationMessage)
                        ],
                        spec["tools"],
                        P.GenerationOptions(),
                        GPT_MODEL,
                        {},
                        "",
                    )
                ),
            }
            for label, spec in TOOLS_CASES
        ],
        "flatten": [
            {
                "label": label,
                "items": copy.deepcopy(items),
                # Python 的实现会就地改写入参（把工具调用追加进那条 assistant 消息），
                # 所以两侧都用副本：录进数据集的 items 必须是原始形态。
                "expected": _flatten_tool_history_to_text(copy.deepcopy(items)),
            }
            for label, items in FLATTEN_CASES
        ],
        "history_detection": [
            {
                "label": label,
                "items": items,
                "status": status,
                "has_tool_history": _has_tool_history_items(items),
                "is_rejection": _is_tool_history_rejection(_FakeExc(status), items),
            }
            for label, items, status in REJECTION_CASES
        ],
        "kwargs": [kwargs_entry(label, options) for label, options in KWARGS_CASES],
        "kwargs_extra": [
            kwargs_entry("非 GPT 模型不带缓存键", {}, model_id=PLAIN_MODEL),
            kwargs_entry("空身份不带缓存键", {}, prompt_cache_identity={}),
            kwargs_entry("无工具不写 tool_choice", {}, tools=()),
            kwargs_entry(
                "有工具且指定 tool_choice",
                {"tool_choice": "auto"},
            ),
            kwargs_entry("空系统提示词", {}, system_prompt=""),
        ],
    }

    FIXTURE_PATH.parent.mkdir(parents=True, exist_ok=True)
    FIXTURE_PATH.write_text(
        json.dumps(fixture, ensure_ascii=False, indent=2) + chr(10), encoding="utf-8"
    )
    print(
        "已写入 %s：消息 %d、tools %d、展平 %d、历史判定 %d、kwargs %d"
        % (
            FIXTURE_PATH.relative_to(ROOT),
            len(fixture["messages"]),
            len(fixture["tools"]),
            len(fixture["flatten"]),
            len(fixture["history_detection"]),
            len(fixture["kwargs"]) + len(fixture["kwargs_extra"]),
        )
    )


class _FakeExc(Exception):
    def __init__(self, status_code):
        super().__init__("fake")
        if status_code is not None:
            self.status_code = status_code


if __name__ == "__main__":
    main()
