#!/usr/bin/env python3
"""生成协议对照数据集，供 Rust 侧 parity 测试使用。

期望值全部由 Python 实现（``omnicrawl/llm/protocol.py``）直接产出，Rust 侧只做同构投影
后逐字段比对，避免两侧语义各自漂移。

用法：``python rust/tools/gen_parity_fixture.py``
输出：``rust/crates/omnicrawl-protocol/tests/fixtures/protocol_parity.json``
"""

from __future__ import annotations

import importlib.util
import json
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
FIXTURE_PATH = ROOT / "rust/crates/omnicrawl-protocol/tests/fixtures/protocol_parity.json"
PROTOCOL_PATH = ROOT / "omnicrawl/llm/protocol.py"


def _load_protocol():
    spec = importlib.util.spec_from_file_location("oc_python_protocol", PROTOCOL_PATH)
    module = importlib.util.module_from_spec(spec)
    # dataclasses 会按 __module__ 回查 sys.modules，缺注册时在 3.9 上取不到类型。
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


P = _load_protocol()


def block_to_json(block) -> dict:
    if isinstance(block, P.TextBlock):
        return {"kind": "text", "text": block.text}
    if isinstance(block, P.ImageBlock):
        return {
            "kind": "image",
            "media_type": block.media_type,
            "data_base64": block.data_base64,
            "detail": block.detail,
        }
    if isinstance(block, P.ToolCallBlock):
        return {
            "kind": "tool_call",
            "call_id": block.call_id,
            "name": block.name,
            "arguments": block.arguments,
        }
    if isinstance(block, P.ToolResultBlock):
        return {
            "kind": "tool_result",
            "call_id": block.call_id,
            "ok": block.ok,
            "content": block.content,
        }
    raise TypeError(f"未知消息块：{type(block)!r}")


def tool_to_json(tool) -> dict:
    return {
        "name": tool.name,
        "description": tool.description,
        "parameters": tool.parameters,
    }


def project_message(message) -> dict:
    return {
        "role": message.role,
        "text": message.text,
        "reasoning": message.reasoning,
        "blocks": [block_to_json(block) for block in message.blocks],
        "tools": [tool_to_json(tool) for tool in message.tools],
    }


def project_reply(reply) -> dict:
    usage = reply.usage
    return {
        "content": reply.content,
        "reasoning": reply.reasoning,
        "finish_reason": reply.finish_reason,
        "content_streamed": reply.content_streamed,
        "usage": None
        if usage is None
        else {
            "input_tokens": usage.input_tokens,
            "output_tokens": usage.output_tokens,
            "cached_input_tokens": usage.cached_input_tokens,
            "reasoning_tokens": usage.reasoning_tokens,
        },
        "tool_calls": [
            {"call_id": call.call_id, "name": call.name, "arguments": call.arguments}
            for call in reply.tool_calls
        ],
        "warnings": [
            {"code": warning.code, "message": warning.message} for warning in reply.warnings
        ],
        "blocks": [block_to_json(block) for block in reply.assistant_message.blocks],
        "assistant_role": reply.assistant_message.role,
        "message_reasoning": reply.assistant_message.reasoning,
        "message_tool_count": len(reply.assistant_message.tools),
    }


def event_to_json(event) -> dict:
    if isinstance(event, P.TextDelta):
        return {"kind": "text_delta", "text": event.text}
    if isinstance(event, P.ReasoningDelta):
        return {"kind": "reasoning_delta", "text": event.text}
    if isinstance(event, P.ToolCallStarted):
        return {"kind": "tool_call_started", "call_id": event.call_id, "name": event.name}
    if isinstance(event, P.ToolCallArgumentsDelta):
        return {
            "kind": "tool_call_arguments_delta",
            "call_id": event.call_id,
            "delta": event.delta,
        }
    if isinstance(event, P.ToolCallCompleted):
        return {
            "kind": "tool_call_completed",
            "call_id": event.call_id,
            "name": event.name,
            "arguments": event.arguments,
        }
    if isinstance(event, P.UsageUpdated):
        return {
            "kind": "usage",
            "input_tokens": event.input_tokens,
            "output_tokens": event.output_tokens,
            "cached_input_tokens": event.cached_input_tokens,
            "reasoning_tokens": event.reasoning_tokens,
        }
    if isinstance(event, P.ResponseCompleted):
        return {"kind": "finished", "finish_reason": event.finish_reason}
    if isinstance(event, P.ProviderWarning):
        return {"kind": "warning", "code": event.code, "message": event.message}
    raise TypeError(f"未知流事件：{type(event)!r}")


CONVERSATION_CASES = [
    {
        "name": "text_and_tool_loop",
        "messages": [
            {"role": "system", "content": "系统提示"},
            {"role": "user", "content": "你好"},
            {"role": "user", "content": ""},
            {},
            {
                "role": "assistant",
                "reasoning_content": "先看一眼",
                "content": "开始读文件",
                "tool_calls": [
                    {
                        "id": "call_1",
                        "function": {"name": "read", "arguments": '{"path": "a.py"}'},
                    },
                    {"id": "call_2", "function": {"name": "noop", "arguments": {"x": 1}}},
                    {"id": "call_3", "function": {"name": "", "arguments": "{}"}},
                    {"id": "call_4", "function": {"name": "bad_json", "arguments": "{not json"}},
                    "not-a-dict",
                ],
            },
            {"role": "tool", "tool_call_id": "call_1", "content": "file body"},
            {"role": "tool", "content": [{"type": "text", "text": "ignored"}]},
            {"role": "developer", "content": "自定义角色"},
            {"role": "assistant", "content": ""},
        ],
    },
    {
        "name": "content_parts_and_images",
        "messages": [
            {
                "role": "user",
                "content": [
                    {"type": "text", "text": "看这张图"},
                    {"type": "input_text", "text": "还有这张"},
                    {"type": "text", "text": ""},
                    {
                        "type": "input_image",
                        "image_url": {
                            "url": "data:image/PNG;base64,QUJD",
                            "detail": "high",
                        },
                    },
                    {
                        "type": "image_url",
                        "image_url": {"url": " data:image/jpeg;base64,//4A ", "detail": "low"},
                    },
                    {"type": "image_url", "image_url": {"url": "https://example.com/a.png"}},
                    {"type": "image_url", "image_url": {"url": "data:image/svg+xml;base64,QUJD"}},
                    {"type": "image_url", "image_url": {"url": "data:image/gif;base64,****"}},
                    {"type": "image_url", "image_url": {"detail": "high"}},
                    {"type": "image_url", "image_url": "data:image/webp;base64,QUJD", "detail": "weird"},
                    {"type": "unknown_part", "text": "skip me"},
                    "not-a-dict",
                ],
            }
        ],
    },
    {
        "name": "system_tools",
        "messages": [
            {
                "role": "system",
                "content": "",
                "tools": [
                    {
                        "type": "function",
                        "function": {
                            "name": "grep",
                            "description": "搜索",
                            "parameters": {"type": "object", "properties": {"pattern": {"type": "string"}}},
                        },
                    },
                    {"type": "function", "function": {"name": "no_params"}},
                    {"type": "function", "function": {"name": "  "}},
                    {"type": "function", "description": "缺少函数体"},
                    "not-a-dict",
                ],
            },
            {
                "role": "system",
                "content": "再来一遍",
                "tools": [
                    {"type": "function", "function": {"name": "grep", "description": "重复"}},
                    {"type": "function", "function": {"name": "glob", "description": "找文件"}},
                ],
            },
        ],
    },
]

AGGREGATE_CASES = [
    {
        "name": "text_reasoning_tool_and_usage",
        "events": [
            P.TextDelta(text="你好"),
            P.TextDelta(text=""),
            P.ReasoningDelta(text=" 想想 "),
            P.ReasoningDelta(text="再想想 "),
            P.ToolCallStarted(call_id="c1", name="read"),
            P.ToolCallArgumentsDelta(call_id="c1", delta='{"path":'),
            P.ToolCallArgumentsDelta(call_id="c1", delta=' "a.py"}'),
            P.ToolCallCompleted(call_id="c1", name="read", arguments={"path": "a.py"}),
            P.UsageUpdated(
                input_tokens=120,
                output_tokens=30,
                cached_input_tokens=64,
                reasoning_tokens=7,
            ),
            P.ProviderWarning(code="rate_limit", message="被限流"),
            P.ResponseCompleted(finish_reason="tool_calls"),
        ],
    },
    {
        "name": "unfinished_tool_calls_are_recovered",
        "events": [
            P.ToolCallStarted(call_id="pending_ok", name="read"),
            P.ToolCallArgumentsDelta(call_id="pending_ok", delta='{"path": "b.py"}'),
            P.ToolCallArgumentsDelta(call_id="orphan", delta='{"x": 1}'),
            P.ToolCallStarted(call_id="empty_name", name="   "),
            P.ToolCallStarted(call_id="bad_json", name="read"),
            P.ToolCallArgumentsDelta(call_id="bad_json", delta="{oops"),
            P.ToolCallStarted(call_id="overwritten", name="first"),
            P.ToolCallStarted(call_id="overwritten", name="second"),
            P.ToolCallArgumentsDelta(call_id="overwritten", delta='{"n": 2}'),
            P.ResponseCompleted(finish_reason=""),
        ],
    },
    {
        "name": "empty_stream",
        "events": [],
    },
]


def build_conversation_cases() -> list:
    cases = []
    for case in CONVERSATION_CASES:
        converted = P.conversation_from_openai_messages(case["messages"])
        cases.append(
            {
                "name": case["name"],
                "messages": case["messages"],
                "expected": [project_message(message) for message in converted],
                "expected_tools": [
                    tool_to_json(tool)
                    for tool in P.tools_from_conversation_messages(converted)
                ],
            }
        )
    return cases


def build_aggregate_cases() -> list:
    cases = []
    for case in AGGREGATE_CASES:
        reply = P.aggregate_stream_events(case["events"])
        cases.append(
            {
                "name": case["name"],
                "events": [event_to_json(event) for event in case["events"]],
                "expected": project_reply(reply),
            }
        )
    return cases


def main() -> None:
    fixture = {
        "source": "omnicrawl/llm/protocol.py",
        "conversation": build_conversation_cases(),
        "aggregate": build_aggregate_cases(),
    }
    FIXTURE_PATH.parent.mkdir(parents=True, exist_ok=True)
    FIXTURE_PATH.write_text(
        json.dumps(fixture, ensure_ascii=False, indent=2) + "\n", encoding="utf-8"
    )
    print(f"已写入 {FIXTURE_PATH.relative_to(ROOT)}")


if __name__ == "__main__":
    main()
