#!/usr/bin/env python3
"""生成 Anthropic Messages 对照数据集，供 Rust 侧 `omnicrawl-llm` 的 parity 测试使用。

期望值全部由 Python 真实现（``omnicrawl/llm/providers/anthropic.py``）产出：
``_to_anthropic_messages``、``_sanitize_options``、``_format_anthropic_error``、
``AnthropicMessagesRuntime._stream_turn_events``（含请求 kwargs 与流事件映射）
与 ``omnicrawl/llm/usage.py`` 的 ``usage_from_anthropic_payload``。

流用例里的负载是**纯 JSON 形状**：Python 侧先转成 ``SimpleNamespace`` 再喂给真实现
（等价于 SDK 对象），Rust 侧直接按 JSON 字段读——两侧读的是同一份数据。

用法：``python rust/tools/gen_llm_anthropic_fixture.py``
输出：``rust/crates/omnicrawl-llm/tests/fixtures/anthropic_parity.json``
"""

from __future__ import annotations

import json
import sys
import threading
from pathlib import Path
from types import SimpleNamespace

ROOT = Path(__file__).resolve().parents[2]
FIXTURE_PATH = ROOT / "rust/crates/omnicrawl-llm/tests/fixtures/anthropic_parity.json"

# 必须加载仓库源码：已安装的 omnicrawl 在 site-packages，会对照到另一份实现。
sys.path.insert(0, str(ROOT))

import omnicrawl.llm.providers.anthropic as A  # noqa: E402
from omnicrawl.llm.protocol import (  # noqa: E402
    ConversationMessage,
    GenerationOptions,
    ImageBlock,
    ModelIdentity,
    ModelTurnRequest,
    TextBlock,
    ToolCallBlock,
    ToolResultBlock,
    ToolSpec,
)
from omnicrawl.llm.usage import usage_from_anthropic_payload  # noqa: E402

if not Path(A.__file__).resolve().is_relative_to(ROOT):
    raise SystemExit(f"加载到的不是仓库源码：{A.__file__}")


def namespace(value):
    """JSON → SimpleNamespace：让真实现走 SDK 那样的属性访问路径。"""

    if isinstance(value, dict):
        return SimpleNamespace(**{key: namespace(item) for key, item in value.items()})
    if isinstance(value, list):
        return [namespace(item) for item in value]
    return value


class FakeStream:
    """可迭代并记录关闭的流替身。"""

    def __init__(self, payloads: list) -> None:
        self._payloads = [namespace(payload) for payload in payloads]
        self.closed = False

    def __iter__(self):
        return iter(self._payloads)

    def close(self) -> None:
        self.closed = True


class FakeMessages:
    """记录 ``create(**kwargs)`` 的 kwargs 并返回预设流。"""

    def __init__(self, stream: FakeStream) -> None:
        self._stream = stream
        self.kwargs = None

    def create(self, **kwargs):
        self.kwargs = kwargs
        return self._stream


class FakeClient:
    def __init__(self, stream: FakeStream) -> None:
        self.messages = FakeMessages(stream)
        self.closed = False

    def close(self) -> None:
        self.closed = True


def make_runtime(payloads: list, descriptor_max_output_tokens, model_id="claude-sonnet-4-5"):
    stream = FakeStream(payloads)
    client = FakeClient(stream)
    runtime = A.AnthropicMessagesRuntime(
        identity=SimpleNamespace(model_id=model_id),
        capabilities=SimpleNamespace(tools=True),
        client=client,
        profile=SimpleNamespace(),
        descriptor=SimpleNamespace(max_output_tokens=descriptor_max_output_tokens),
    )
    return runtime, client


def options_from_json(data: dict) -> GenerationOptions:
    return GenerationOptions(
        max_output_tokens=data.get("max_output_tokens"),
        temperature=data.get("temperature"),
        reasoning_effort=data.get("reasoning_effort", ""),
        tool_choice=data.get("tool_choice", ""),
        request_timeout_seconds=data.get("request_timeout_seconds", 180.0),
        request_retry_count=data.get("request_retry_count", 5),
        provider_options=data.get("provider_options", {}),
    )


def tool_from_json(data: dict) -> ToolSpec:
    return ToolSpec(
        name=data["name"],
        description=data.get("description", ""),
        parameters=data.get("parameters", {}),
    )


def block_from_json(data: dict):
    kind = data["kind"]
    if kind == "text":
        return TextBlock(text=data["text"])
    if kind == "image":
        return ImageBlock(
            media_type=data["media_type"],
            data_base64=data["data_base64"],
            detail=data.get("detail", "auto"),
        )
    if kind == "tool_call":
        return ToolCallBlock(
            call_id=data["call_id"],
            name=data["name"],
            arguments=data.get("arguments", {}),
            provider_call_id=data.get("provider_call_id", ""),
        )
    if kind == "tool_result":
        return ToolResultBlock(
            call_id=data["call_id"],
            ok=data.get("ok", True),
            content=data.get("content", ""),
        )
    raise SystemExit(f"未知块类型：{kind}")


def message_from_json(data: dict) -> ConversationMessage:
    return ConversationMessage(
        role=data["role"],
        blocks=tuple(block_from_json(item) for item in data.get("blocks", [])),
        reasoning=data.get("reasoning", ""),
        tools=tuple(tool_from_json(item) for item in data.get("tools", [])),
    )


def tool_to_protocol_json(tool: ToolSpec) -> dict:
    """Rust 侧 `ToolSpec` 的 serde 形状。"""

    return {
        "name": tool.name,
        "description": tool.description,
        "parameters": tool.parameters,
    }


def block_to_protocol_json(block) -> dict:
    """Rust 侧 `MessageBlock` 的 serde 形状（外部标签枚举）。"""

    if isinstance(block, TextBlock):
        return {"Text": {"text": block.text}}
    if isinstance(block, ImageBlock):
        return {
            "Image": {
                "media_type": block.media_type,
                "data_base64": block.data_base64,
                "detail": block.detail,
            }
        }
    if isinstance(block, ToolCallBlock):
        return {
            "ToolCall": {
                "call_id": block.call_id,
                "name": block.name,
                "arguments": block.arguments,
                "provider_call_id": block.provider_call_id,
            }
        }
    if isinstance(block, ToolResultBlock):
        return {
            "ToolResult": {
                "call_id": block.call_id,
                "ok": block.ok,
                "content": block.content,
            }
        }
    raise SystemExit(f"未知块：{type(block).__name__}")


def message_to_protocol_json(message: ConversationMessage) -> dict:
    return {
        "role": message.role,
        "blocks": [block_to_protocol_json(block) for block in message.blocks],
        "reasoning": message.reasoning,
        "tools": [tool_to_protocol_json(tool) for tool in message.tools],
    }


def block_from_protocol_json(data: dict):
    (kind, payload), = data.items()
    if kind == "Text":
        return TextBlock(text=payload["text"])
    if kind == "Image":
        return ImageBlock(
            media_type=payload["media_type"],
            data_base64=payload["data_base64"],
            detail=payload.get("detail", "auto"),
        )
    if kind == "ToolCall":
        return ToolCallBlock(
            call_id=payload["call_id"],
            name=payload["name"],
            arguments=payload.get("arguments", {}),
            provider_call_id=payload.get("provider_call_id", ""),
        )
    if kind == "ToolResult":
        return ToolResultBlock(
            call_id=payload["call_id"],
            ok=payload.get("ok", True),
            content=payload.get("content", ""),
        )
    raise SystemExit(f"未知块类型：{kind}")


def message_from_protocol_json(data: dict) -> ConversationMessage:
    return ConversationMessage(
        role=data["role"],
        blocks=tuple(block_from_protocol_json(item) for item in data.get("blocks", [])),
        reasoning=data.get("reasoning", ""),
        tools=tuple(tool_from_json(item) for item in data.get("tools", [])),
    )


def event_to_json(event) -> dict:
    """内核事件的稳定投影，与 Rust 侧 `tests/common::event_to_json` 一一对应。"""

    if isinstance(event, A.TextDelta):
        return {"kind": "text", "text": event.text}
    if isinstance(event, A.ReasoningDelta):
        return {"kind": "reasoning", "text": event.text}
    if isinstance(event, A.ToolCallStarted):
        return {"kind": "tool_started", "call_id": event.call_id, "name": event.name}
    if isinstance(event, A.ToolCallCompleted):
        return {
            "kind": "tool_completed",
            "call_id": event.call_id,
            "name": event.name,
            "arguments": event.arguments,
        }
    if isinstance(event, A.UsageUpdated):
        return {
            "kind": "usage",
            "input_tokens": event.input_tokens,
            "output_tokens": event.output_tokens,
            "cached_input_tokens": event.cached_input_tokens,
            "reasoning_tokens": event.reasoning_tokens,
        }
    if isinstance(event, A.ResponseCompleted):
        return {"kind": "finished", "finish_reason": event.finish_reason}
    raise SystemExit(f"未知事件类型：{type(event).__name__}")


# --------------------------------------------------------------------------- 数据集

MESSAGE_CASES = [
    {
        "label": "纯文本与系统提示",
        "messages": [
            {"role": "user", "blocks": [{"kind": "text", "text": "你好"}]},
        ],
    },
    {
        "label": "system 携带工具声明时整条跳过",
        "messages": [
            {
                "role": "system",
                "tools": [{"name": "read_file", "description": "读文件", "parameters": {}}],
            },
            {"role": "user", "blocks": [{"kind": "text", "text": "读一下"}]},
        ],
    },
    {
        "label": "工具结果与紧随的用户内容合并成一条 user",
        "messages": [
            {"role": "user", "blocks": [{"kind": "text", "text": "看下 a.py"}]},
            {
                "role": "assistant",
                "blocks": [
                    {"kind": "text", "text": "好的"},
                    {
                        "kind": "tool_call",
                        "call_id": "toolu_1",
                        "name": "read_file",
                        "arguments": {"path": "a.py"},
                    },
                ],
            },
            {
                "role": "tool",
                "blocks": [{"kind": "tool_result", "call_id": "toolu_1", "ok": True, "content": "print(1)"}],
            },
            {"role": "user", "blocks": [{"kind": "text", "text": "继续"}]},
        ],
    },
    {
        "label": "工具结果收尾时单独成条",
        "messages": [
            {
                "role": "assistant",
                "blocks": [
                    {"kind": "tool_call", "call_id": "", "provider_call_id": "provider_x", "name": "bash", "arguments": {}},
                ],
            },
            {"role": "tool", "blocks": [{"kind": "tool_result", "call_id": "provider_x", "ok": False, "content": "boom"}]},
        ],
    },
    {
        "label": "空 assistant 内容补空文本块",
        "messages": [
            {"role": "assistant", "blocks": []},
        ],
    },
    {
        "label": "空 user 内容退化为空串",
        "messages": [
            {"role": "user", "blocks": []},
        ],
    },
    {
        "label": "图片走原生 base64 source",
        "messages": [
            {
                "role": "user",
                "blocks": [
                    {"kind": "text", "text": "这是什么"},
                    {
                        "kind": "image",
                        "media_type": "image/png",
                        "data_base64": "aGVsbG8=",
                        "detail": "high",
                    },
                ],
            },
        ],
    },
    {
        "label": "空文本块不参与组装",
        "messages": [
            {"role": "assistant", "blocks": [{"kind": "text", "text": ""}]},
            {"role": "user", "blocks": [{"kind": "text", "text": ""}]},
        ],
    },
]

OPTION_CASES = [
    {"label": "允许的字段", "options": {"top_p": 0.9, "top_k": 10, "metadata": {"a": 1}}},
    {"label": "thinking 透传", "options": {"thinking": {"type": "enabled", "budget_tokens": 1024}}},
    {"label": "stop_sequences", "options": {"stop_sequences": ["\n\n", "END"]}},
    {"label": "覆盖 Host 字段", "options": {"model": "claude-x"}},
    {"label": "覆盖 max_tokens", "options": {"max_tokens": 1}},
    {"label": "覆盖 base_url", "options": {"base_url": "http://evil"}},
    {"label": "白名单之外", "options": {"temperature": 0.5}},
    {"label": "未知字段", "options": {"nope": 1}},
    {"label": "空", "options": {}},
]

REQUEST_CASES = [
    {
        "label": "最小请求",
        "model": "claude-sonnet-4-5",
        "system_prompt": "你是助手",
        "messages": [{"role": "user", "blocks": [{"kind": "text", "text": "你好"}]}],
    },
    {
        "label": "空白系统提示不下发",
        "model": "claude-sonnet-4-5",
        "system_prompt": "   ",
        "messages": [{"role": "user", "blocks": [{"kind": "text", "text": "你好"}]}],
    },
    {
        "label": "工具声明与生成选项",
        "model": "claude-opus-4-1",
        "system_prompt": "s",
        "messages": [{"role": "user", "blocks": [{"kind": "text", "text": "跑一下"}]}],
        "tools": [
            {"name": "bash", "description": "执行", "parameters": {"type": "object", "properties": {"cmd": {"type": "string"}}}},
            {"name": "empty", "description": "", "parameters": {}},
        ],
        "options": {"temperature": 0.2, "provider_options": {"top_k": 5}},
    },
    {
        "label": "system 动态声明并入请求级工具",
        "model": "claude-sonnet-4-5",
        "system_prompt": "s",
        "messages": [
            {
                "role": "system",
                "tools": [{"name": "dynamic", "description": "动态", "parameters": {"type": "object"}}],
            },
            {"role": "user", "blocks": [{"kind": "text", "text": "hi"}]},
        ],
    },
    {
        "label": "生成选项声明 max_tokens",
        "model": "claude-sonnet-4-5",
        "system_prompt": "",
        "messages": [{"role": "user", "blocks": [{"kind": "text", "text": "hi"}]}],
        "options": {"max_output_tokens": 256},
        "descriptor_max_output_tokens": 8192,
    },
    {
        "label": "描述上限兜底",
        "model": "claude-sonnet-4-5",
        "system_prompt": "",
        "messages": [{"role": "user", "blocks": [{"kind": "text", "text": "hi"}]}],
        "descriptor_max_output_tokens": 2048,
    },
    {
        "label": "都没有时用 4096",
        "model": "claude-sonnet-4-5",
        "system_prompt": "",
        "messages": [{"role": "user", "blocks": [{"kind": "text", "text": "hi"}]}],
    },
]

STREAM_CASES = [
    {
        "label": "文本与思考增量",
        "payloads": [
            {"type": "message_start", "message": {"usage": {"input_tokens": 12, "output_tokens": 1}}},
            {"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}},
            {"type": "content_block_delta", "index": 0, "delta": {"type": "thinking_delta", "thinking": "想一下"}},
            {"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "你好"}},
            {"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": ""}},
            {"type": "content_block_stop", "index": 0},
            {"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 9}},
            {"type": "message_stop"},
        ],
    },
    {
        "label": "单个工具调用",
        "payloads": [
            {"type": "message_start", "message": {"usage": {"input_tokens": 30, "output_tokens": 2, "cache_read_input_tokens": 8}}},
            {"type": "content_block_start", "index": 0, "content_block": {"type": "tool_use", "id": "toolu_a", "name": "read_file"}},
            {"type": "content_block_delta", "index": 0, "delta": {"type": "input_json_delta", "partial_json": '{"path"'}},
            {"type": "content_block_delta", "index": 0, "delta": {"type": "input_json_delta", "partial_json": ': "a.py"}'}},
            {"type": "content_block_stop", "index": 0},
            {"type": "message_delta", "delta": {"stop_reason": "tool_use"}},
            {"type": "message_stop"},
        ],
    },
    {
        "label": "并行工具调用交错分片",
        "payloads": [
            {"type": "content_block_start", "index": 0, "content_block": {"type": "tool_use", "id": "c0", "name": "a"}},
            {"type": "content_block_start", "index": 1, "content_block": {"type": "tool_use", "id": "c1", "name": "b"}},
            {"type": "content_block_delta", "index": 1, "delta": {"type": "input_json_delta", "partial_json": '{"y": 1}'}},
            {"type": "content_block_delta", "index": 0, "delta": {"type": "input_json_delta", "partial_json": '{"x": 2}'}},
            {"type": "content_block_stop", "index": 0},
            {"type": "content_block_stop", "index": 1},
            {"type": "message_delta", "delta": {"stop_reason": "tool_use"}},
        ],
    },
    {
        "label": "工具调用没收尾时补齐",
        "payloads": [
            {"type": "content_block_start", "index": 0, "content_block": {"type": "tool_use", "id": "toolu_b", "name": "grep"}},
            {"type": "content_block_delta", "index": 0, "delta": {"type": "input_json_delta", "partial_json": '{"q"'}},
        ],
    },
    {
        "label": "缺 id 时回落 toolu_index",
        "payloads": [
            {"type": "content_block_start", "index": 3, "content_block": {"type": "tool_use", "name": "bash"}},
            {"type": "content_block_stop", "index": 3},
        ],
    },
    {
        "label": "名称为空不发 started",
        "payloads": [
            {"type": "content_block_start", "index": 0, "content_block": {"type": "tool_use", "id": "t0", "name": ""}},
            {"type": "content_block_stop", "index": 0},
        ],
    },
    {
        "label": "参数不是合法 JSON 时回空对象",
        "payloads": [
            {"type": "content_block_start", "index": 0, "content_block": {"type": "tool_use", "id": "t1", "name": "x"}},
            {"type": "content_block_delta", "index": 0, "delta": {"type": "input_json_delta", "partial_json": '{"a":'}},
            {"type": "content_block_stop", "index": 0},
        ],
    },
    {
        "label": "增量落在未开的下标上被忽略",
        "payloads": [
            {"type": "content_block_delta", "index": 7, "delta": {"type": "input_json_delta", "partial_json": "{}"}},
            {"type": "content_block_stop", "index": 7},
        ],
    },
    {
        "label": "非 tool_use 的块开始整块忽略",
        "payloads": [
            {"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": "hi"}},
            {"type": "content_block_stop", "index": 0},
        ],
    },
    {
        "label": "缺 index 时按 0 处理",
        "payloads": [
            {"type": "content_block_start", "content_block": {"type": "tool_use", "id": "t2", "name": "z"}},
            {"type": "content_block_delta", "delta": {"type": "input_json_delta", "partial_json": "{}"}},
            {"type": "content_block_stop"},
        ],
    },
    {
        "label": "未知事件与空字段忽略",
        "payloads": [
            {"type": "ping"},
            {"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": 5}},
            {"type": "content_block_delta", "index": 0, "delta": {"type": "input_json_delta", "partial_json": ""}},
            {"type": "message_delta", "delta": {"stop_reason": ""}, "usage": None},
            {"type": "message_stop"},
        ],
    },
    {
        "label": "空流",
        "payloads": [],
    },
]

USAGE_CASES = [
    {"label": "完整字段", "payload": {"usage": {"input_tokens": 10, "output_tokens": 20, "cache_read_input_tokens": 5}}},
    {"label": "cached_input_tokens 写法", "payload": {"usage": {"input_tokens": 1, "output_tokens": 2, "cached_input_tokens": 3}}},
    {"label": "缺字段按 0", "payload": {"usage": {}}},
    {"label": "布尔与字符串不算整数", "payload": {"usage": {"input_tokens": True, "output_tokens": "5"}}},
    {"label": "无 usage", "payload": {}},
    {"label": "usage 为 null", "payload": {"usage": None}},
]

ERROR_CASES = [
    {"message": "authentication_error: invalid x-api-key", "type_name": "AuthenticationError"},
    {"message": "401 unauthorized", "type_name": "APIStatusError"},
    {"message": "permission denied for model", "type_name": "PermissionDeniedError"},
    {"message": "403", "type_name": "APIStatusError"},
    {"message": "not_found_error", "type_name": "NotFoundError"},
    {"message": "404 model: claude-x", "type_name": "APIStatusError"},
    {"message": "rate_limit_error", "type_name": "RateLimitError"},
    {"message": "429 too many requests", "type_name": "APIStatusError"},
    {"message": "Request timed out.", "type_name": "APITimeoutError"},
    {"message": "Connection error.", "type_name": "APIConnectionError"},
]


def run_message_case(case: dict) -> dict:
    authored = [message_from_json(item) for item in case["messages"]]
    encoded = [message_to_protocol_json(message) for message in authored]
    # 期望值由「fixture 里那份字节」还原出来的对象产出：两侧读的是同一份数据。
    restored = tuple(message_from_protocol_json(item) for item in encoded)
    return {
        "label": case["label"],
        "messages": encoded,
        "expected": A._to_anthropic_messages(restored),
    }


def run_option_case(case: dict) -> dict:
    error = None
    result = None
    try:
        result = A._sanitize_options(case["options"])
    except Exception as exc:  # noqa: BLE001 - 期望值就是错误文案
        error = str(exc)
    return {
        "label": case["label"],
        "options": case["options"],
        "ok": error is None,
        "result": result,
        "error": error,
    }


def run_request_case(case: dict) -> dict:
    runtime, client = make_runtime(
        [], case.get("descriptor_max_output_tokens"), model_id=case["model"]
    )
    authored = [message_from_json(item) for item in case.get("messages", [])]
    encoded = [message_to_protocol_json(message) for message in authored]
    tools = [tool_from_json(item) for item in case.get("tools", [])]
    request = ModelTurnRequest(
        identity=ModelIdentity(
            profile_id="p",
            provider="anthropic",
            protocol="anthropic_messages",
            model_id=case["model"],
        ),
        system_prompt=case["system_prompt"],
        messages=tuple(message_from_protocol_json(item) for item in encoded),
        tools=tuple(tool_from_json(tool_to_protocol_json(tool)) for tool in tools),
        generation_options=options_from_json(case.get("options", {})),
    )
    list(runtime._stream_turn_events(request, None, None, threading.Event()))
    entry = {
        "label": case["label"],
        "model": case["model"],
        "system_prompt": case["system_prompt"],
        "messages": encoded,
        "tools": [tool_to_protocol_json(tool) for tool in tools],
        "options": case.get("options", {}),
        "descriptor_max_output_tokens": case.get("descriptor_max_output_tokens"),
        "kwargs": client.messages.kwargs,
    }
    return entry


def run_stream_case(case: dict) -> dict:
    runtime, client = make_runtime(case["payloads"], None)
    request = ModelTurnRequest(
        identity=ModelIdentity(
            profile_id="p",
            provider="anthropic",
            protocol="anthropic_messages",
            model_id="claude-sonnet-4-5",
        ),
        system_prompt="s",
        messages=(ConversationMessage(role="user", blocks=(TextBlock(text="hi"),)),),
    )
    events = list(runtime._stream_turn_events(request, None, None, threading.Event()))
    return {
        "label": case["label"],
        "payloads": case["payloads"],
        "expected": [event_to_json(event) for event in events],
        "closed": client.messages._stream.closed,
    }


def main() -> None:
    fixture = {
        "source": "omnicrawl/llm/providers/anthropic.py",
        "messages": [run_message_case(case) for case in MESSAGE_CASES],
        "options": [run_option_case(case) for case in OPTION_CASES],
        "request": [run_request_case(case) for case in REQUEST_CASES],
        "stream": [run_stream_case(case) for case in STREAM_CASES],
        "usage": [
            {
                "label": case["label"],
                "payload": case["payload"],
                "expected": usage_from_anthropic_payload(namespace(case["payload"])),
            }
            for case in USAGE_CASES
        ],
        "errors": [
            {
                "message": case["message"],
                "type_name": case["type_name"],
                "expected": A._format_anthropic_error(
                    type(case["type_name"], (Exception,), {})(case["message"]),
                ),
            }
            for case in ERROR_CASES
        ],
    }
    for item in fixture["usage"]:
        expected = item["expected"]
        item["expected"] = (
            None
            if expected is None
            else {
                "input_tokens": expected.input_tokens,
                "output_tokens": expected.output_tokens,
                "cached_input_tokens": expected.cached_input_tokens,
            }
        )

    FIXTURE_PATH.parent.mkdir(parents=True, exist_ok=True)
    FIXTURE_PATH.write_text(
        json.dumps(fixture, ensure_ascii=False, indent=2) + "\n", encoding="utf-8"
    )
    print(
        f"已写入 {FIXTURE_PATH.relative_to(ROOT)}："
        f"消息 {len(fixture['messages'])}、选项 {len(fixture['options'])}、"
        f"请求 {len(fixture['request'])}、流 {len(fixture['stream'])}、"
        f"用量 {len(fixture['usage'])}、错误 {len(fixture['errors'])}"
    )


if __name__ == "__main__":
    main()
