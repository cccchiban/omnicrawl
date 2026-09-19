#!/usr/bin/env python3
"""生成 Gemini Generate Content 对照数据集，供 Rust 侧 `omnicrawl-llm` 的 parity 测试使用。

期望值全部由 Python 真实现（``omnicrawl/llm/providers/gemini.py``）产出：
``_to_gemini_contents``、``_sanitize_options``、``_build_generate_config``（dict 形态）、
``_format_gemini_error``、``GeminiGenerateContentRuntime._stream_turn_events``，
以及 ``omnicrawl/llm/usage.py`` 的 ``usage_from_gemini_payload``。

`wire` 组的期望值是**真 SDK 的线上请求体**：用本机回环服务端拦下 google-genai
实际发出的路径与 body，喂进去的 contents / config 是上面那些真实现的产物。
回环只在本机，不出网。

用法：``python rust/tools/gen_llm_gemini_fixture.py``
输出：``rust/crates/omnicrawl-llm/tests/fixtures/gemini_parity.json``
"""

from __future__ import annotations

import json
import sys
import threading
from contextlib import contextmanager
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from types import SimpleNamespace
from unittest import mock

ROOT = Path(__file__).resolve().parents[2]
FIXTURE_PATH = ROOT / "rust/crates/omnicrawl-llm/tests/fixtures/gemini_parity.json"

# 必须加载仓库源码：已安装的 omnicrawl 在 site-packages，会对照到另一份实现。
sys.path.insert(0, str(ROOT))

import google.genai.types as genai_types  # noqa: E402
import omnicrawl.llm.providers.gemini as G  # noqa: E402
from omnicrawl.llm.protocol import (  # noqa: E402
    ConversationMessage,
    GenerationOptions,
    ImageBlock,
    ModelIdentity,
    ModelTurnRequest,
    ReasoningDelta,
    ResponseCompleted,
    TextBlock,
    TextDelta,
    ToolCallBlock,
    ToolCallCompleted,
    ToolCallStarted,
    ToolResultBlock,
    ToolSpec,
    UsageUpdated,
)
from omnicrawl.llm.usage import usage_from_gemini_payload  # noqa: E402

if not Path(G.__file__).resolve().is_relative_to(ROOT):
    raise SystemExit(f"加载到的不是仓库源码：{G.__file__}")

PROTOCOL = G.PROTOCOL_GEMINI_GENERATE_CONTENT


class _ConfigMustStayDict:
    """让 `_build_generate_config` 走 `except` 分支，直接返回它组装的 dict。"""

    def __init__(self, **kwargs) -> None:
        raise RuntimeError("fixture：只取 dict 形态的期望值")


@contextmanager
def config_as_dict():
    with mock.patch.object(genai_types, "GenerateContentConfig", _ConfigMustStayDict):
        yield


def namespace(value):
    """JSON → SimpleNamespace：让真实现走 SDK 那样的属性访问路径。"""

    if isinstance(value, dict):
        return SimpleNamespace(**{key: namespace(item) for key, item in value.items()})
    if isinstance(value, list):
        return [namespace(item) for item in value]
    return value


def namespace_chunk(value):
    """分片的 SimpleNamespace 化：`function_call.args` 在真 SDK 里是普通 dict，不是对象。"""

    if isinstance(value, dict):
        return SimpleNamespace(
            **{
                key: (item if key == "args" else namespace_chunk(item))
                for key, item in value.items()
            }
        )
    if isinstance(value, list):
        return [namespace_chunk(item) for item in value]
    return value


class FakeModels:
    """记录 ``generate_content_stream(**kwargs)`` 的 kwargs 并返回预设分片。"""

    def __init__(self, chunks: list, raw: bool = False) -> None:
        self._chunks = [chunk if raw else namespace_chunk(chunk) for chunk in chunks]
        self.kwargs = None

    def generate_content_stream(self, **kwargs):
        self.kwargs = kwargs
        return iter(self._chunks)


class FakeClient:
    def __init__(self, chunks: list, raw: bool = False) -> None:
        self.models = FakeModels(chunks, raw=raw)
        self.closed = False

    def close(self) -> None:
        self.closed = True


def make_runtime(chunks: list, model_id: str = "gemini-2.5-flash", raw: bool = False):
    client = FakeClient(chunks, raw=raw)
    runtime = G.GeminiGenerateContentRuntime(
        identity=SimpleNamespace(model_id=model_id),
        capabilities=SimpleNamespace(tools=True),
        client=client,
        profile=SimpleNamespace(),
        descriptor=SimpleNamespace(),
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
    return {
        "name": tool.name,
        "description": tool.description,
        "parameters": tool.parameters,
    }


def block_to_protocol_json(block) -> dict:
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

    if isinstance(event, TextDelta):
        return {"kind": "text", "text": event.text}
    if isinstance(event, ReasoningDelta):
        return {"kind": "reasoning", "text": event.text}
    if isinstance(event, ToolCallStarted):
        return {"kind": "tool_started", "call_id": event.call_id, "name": event.name}
    if isinstance(event, ToolCallCompleted):
        return {
            "kind": "tool_completed",
            "call_id": event.call_id,
            "name": event.name,
            "arguments": event.arguments,
        }
    if isinstance(event, UsageUpdated):
        return {
            "kind": "usage",
            "input_tokens": event.input_tokens,
            "output_tokens": event.output_tokens,
            "cached_input_tokens": event.cached_input_tokens,
            "reasoning_tokens": event.reasoning_tokens,
        }
    if isinstance(event, ResponseCompleted):
        return {"kind": "finished", "finish_reason": event.finish_reason}
    raise SystemExit(f"未知事件类型：{type(event).__name__}")


def build_turn_request(case: dict) -> ModelTurnRequest:
    authored = [message_from_json(item) for item in case.get("messages", [])]
    encoded = [message_to_protocol_json(message) for message in authored]
    tools = [tool_from_json(item) for item in case.get("tools", [])]
    return ModelTurnRequest(
        identity=ModelIdentity(
            profile_id="p",
            provider="gemini",
            protocol=PROTOCOL,
            model_id=case.get("model", "gemini-2.5-flash"),
        ),
        system_prompt=case.get("system_prompt", ""),
        messages=tuple(message_from_protocol_json(item) for item in encoded),
        tools=tuple(tool_from_json(tool_to_protocol_json(tool)) for tool in tools),
        generation_options=options_from_json(case.get("options", {})),
    )


# --------------------------------------------------------------------------- 线上抓取


class _CaptureHandler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def do_POST(self):  # noqa: N802
        length = int(self.headers.get("Content-Length") or 0)
        self.server.captured = {
            "path": self.path,
            "body": self.rfile.read(length).decode("utf-8", "replace"),
        }
        payload = (
            b'data: {"candidates":[{"content":{"parts":[{"text":"ok"}],"role":"model"},'
            b'"finishReason":"STOP"}]}\n\n'
        )
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def log_message(self, *args):  # noqa: A003
        return


def capture_wire(model: str, contents: list, config: dict) -> tuple[str, dict]:
    """把真 SDK 打到本机回环，取它实际发出的路径与请求体。"""

    from google import genai

    server = ThreadingHTTPServer(("127.0.0.1", 0), _CaptureHandler)
    server.captured = {}
    port = server.server_address[1]
    threading.Thread(target=server.serve_forever, daemon=True).start()
    try:
        client = genai.Client(
            api_key="fixture-key",
            http_options={"base_url": f"http://127.0.0.1:{port}"},
        )
        list(
            client.models.generate_content_stream(
                model=model,
                contents=contents,
                config=config,
            )
        )
    finally:
        server.shutdown()
        server.server_close()
    captured = server.captured
    return captured["path"], json.loads(captured["body"])


# --------------------------------------------------------------------------- 数据集

CONTENTS_CASES = [
    {
        "label": "纯文本与 system 文本",
        "messages": [
            {"role": "system", "blocks": [{"kind": "text", "text": "你是助手"}]},
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
        "label": "assistant 工具调用与结果",
        "messages": [
            {"role": "user", "blocks": [{"kind": "text", "text": "看下 a.py"}]},
            {
                "role": "assistant",
                "blocks": [
                    {"kind": "text", "text": "好的"},
                    {
                        "kind": "tool_call",
                        "call_id": "gemini_1",
                        "name": "read_file",
                        "arguments": {"path": "a.py"},
                    },
                ],
            },
            {
                "role": "tool",
                "blocks": [
                    {"kind": "tool_result", "call_id": "gemini_1", "ok": True, "content": "print(1)"}
                ],
            },
            {"role": "user", "blocks": [{"kind": "text", "text": "继续"}]},
        ],
    },
    {
        "label": "工具结果查不到名字时回落 tool",
        "messages": [
            {
                "role": "tool",
                "blocks": [
                    {"kind": "tool_result", "call_id": "unknown", "ok": False, "content": "boom"}
                ],
            },
        ],
    },
    {
        "label": "空 assistant 内容不产出条目",
        "messages": [
            {"role": "assistant", "blocks": []},
            {"role": "user", "blocks": []},
        ],
    },
    {
        "label": "空文本块不参与组装",
        "messages": [
            {"role": "assistant", "blocks": [{"kind": "text", "text": ""}]},
            {"role": "user", "blocks": [{"kind": "text", "text": ""}]},
        ],
    },
    {
        "label": "图片走 inline_data",
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
        "label": "连续 user 内容合并成一条",
        "messages": [
            {"role": "user", "blocks": [{"kind": "text", "text": "第一段"}]},
            {"role": "user", "blocks": [{"kind": "text", "text": "第二段"}]},
            {"role": "assistant", "blocks": [{"kind": "text", "text": "收到"}]},
        ],
    },
]

OPTION_CASES = [
    {
        "label": "允许的字段",
        "options": {
            "top_p": 0.9,
            "top_k": 8.0,
            "candidate_count": 2,
            "stop_sequences": ["A"],
            "response_mime_type": "application/json",
            "safety_settings": [{"category": "HARM_CATEGORY_HATE_SPEECH"}],
        },
    },
    {"label": "覆盖 model", "options": {"model": "gemini-x"}},
    {"label": "覆盖 contents", "options": {"contents": []}},
    {"label": "覆盖 tools", "options": {"tools": []}},
    {"label": "覆盖 system_instruction", "options": {"system_instruction": "x"}},
    {"label": "覆盖 stream", "options": {"stream": False}},
    {"label": "覆盖 api_key", "options": {"api_key": "x"}},
    {"label": "覆盖 timeout", "options": {"timeout": 1}},
    {"label": "白名单之外", "options": {"temperature": 0.5}},
    {"label": "未知字段", "options": {"nope": 1}},
    {"label": "空", "options": {}},
]

TOOL_SCHEMA = {
    "type": "object",
    "properties": {
        "path": {"type": "string", "description": "路径"},
        "count": {"type": "integer", "format": "int32"},
        "mode": {"type": "string", "enum": ["a", "b"]},
        "items": {"type": "array", "items": {"type": "string"}},
        "nested": {
            "type": "object",
            "properties": {"flag": {"type": "boolean"}},
            "required": ["flag"],
        },
    },
    "required": ["path"],
}

REQUEST_CASES = [
    {
        "label": "最小请求",
        "model": "gemini-2.5-flash",
        "system_prompt": "",
        "messages": [{"role": "user", "blocks": [{"kind": "text", "text": "你好"}]}],
    },
    {
        "label": "系统提示与生成选项",
        "model": "gemini-2.5-flash",
        "system_prompt": "你是助手",
        "messages": [{"role": "user", "blocks": [{"kind": "text", "text": "你好"}]}],
        "options": {"temperature": 0.2, "max_output_tokens": 256},
    },
    {
        "label": "空白系统提示不下发",
        "model": "gemini-2.5-flash",
        "system_prompt": "   ",
        "messages": [{"role": "user", "blocks": [{"kind": "text", "text": "你好"}]}],
    },
    {
        "label": "max_output_tokens 为 0 视为未声明",
        "model": "gemini-2.5-flash",
        "system_prompt": "",
        "messages": [{"role": "user", "blocks": [{"kind": "text", "text": "hi"}]}],
        "options": {"max_output_tokens": 0},
    },
    {
        "label": "工具声明与 provider_options 合并",
        "model": "gemini-2.5-flash",
        "system_prompt": "s",
        "messages": [{"role": "user", "blocks": [{"kind": "text", "text": "跑一下"}]}],
        "tools": [
            {"name": "bash", "description": "执行", "parameters": TOOL_SCHEMA},
            {"name": "empty", "description": "", "parameters": {}},
        ],
        "options": {"provider_options": {"top_k": 8.0}},
    },
    {
        "label": "system 动态声明并入请求级工具",
        "model": "gemini-2.5-flash",
        "system_prompt": "s",
        "messages": [
            {
                "role": "system",
                "tools": [{"name": "dynamic", "description": "动态", "parameters": {"type": "object"}}],
            },
            {"role": "user", "blocks": [{"kind": "text", "text": "hi"}]},
        ],
    },
]

WIRE_CASES = [
    {
        "label": "最小请求",
        "model": "gemini-2.5-flash",
        "system_prompt": "",
        "messages": [{"role": "user", "blocks": [{"kind": "text", "text": "你好"}]}],
    },
    {
        "label": "系统提示与全部生成参数",
        "model": "gemini-2.5-flash",
        "system_prompt": "你是助手",
        "messages": [{"role": "user", "blocks": [{"kind": "text", "text": "hi"}]}],
        "options": {
            "temperature": 0.4,
            "max_output_tokens": 128,
            "provider_options": {
                "top_p": 0.8,
                "top_k": 12.0,
                "candidate_count": 2,
                "stop_sequences": ["A", "B"],
                "response_mime_type": "application/json",
            },
        },
    },
    {
        "label": "安全设置与工具 schema",
        "model": "gemini-2.5-flash",
        "system_prompt": "s",
        "messages": [{"role": "user", "blocks": [{"kind": "text", "text": "hi"}]}],
        "tools": [{"name": "do_thing", "description": "desc", "parameters": TOOL_SCHEMA}],
        "options": {
            "provider_options": {
                "safety_settings": [
                    {
                        "category": "HARM_CATEGORY_HATE_SPEECH",
                        "threshold": "BLOCK_LOW_AND_ABOVE",
                    }
                ]
            }
        },
    },
    {
        "label": "模型名已带 models 前缀",
        "model": "models/gemini-2.5-flash",
        "system_prompt": "",
        "messages": [{"role": "user", "blocks": [{"kind": "text", "text": "hi"}]}],
    },
]

STREAM_CASES = [
    {
        "label": "文本增量与结束原因",
        "payloads": [
            {
                "candidates": [{"content": {"role": "model", "parts": [{"text": "你好"}]}}],
                "usageMetadata": {"promptTokenCount": 3, "candidatesTokenCount": 1},
            },
            {
                "candidates": [
                    {"content": {"role": "model", "parts": [{"text": "，世界"}]}, "finishReason": "STOP"}
                ]
            },
        ],
    },
    {
        "label": "单个工具调用",
        "payloads": [
            {
                "candidates": [
                    {
                        "content": {
                            "role": "model",
                            "parts": [
                                {"function_call": {"name": "read_file", "args": {"path": "a.py"}}}
                            ],
                        },
                        "finishReason": "STOP",
                    }
                ]
            }
        ],
    },
    {
        "label": "两个不同调用依次编号",
        "payloads": [
            {
                "candidates": [
                    {
                        "content": {
                            "role": "model",
                            "parts": [
                                {"function_call": {"name": "a", "args": {"x": 1}}},
                                {"function_call": {"name": "b", "args": {"y": 2}}},
                            ],
                        }
                    }
                ]
            }
        ],
    },
    {
        "label": "内容相同的调用被去重",
        "payloads": [
            {
                "candidates": [
                    {"content": {"role": "model", "parts": [{"function_call": {"name": "a", "args": {"x": 1}}}]}}
                ]
            },
            {
                "candidates": [
                    {"content": {"role": "model", "parts": [{"function_call": {"name": "a", "args": {"x": 1}}}]}}
                ]
            },
            {
                "candidates": [
                    {"content": {"role": "model", "parts": [{"function_call": {"name": "a", "args": {"x": 2}}}]}}
                ]
            },
        ],
    },
    {
        "label": "参数键序不同但内容相同视为重复",
        "payloads": [
            {
                "candidates": [
                    {"content": {"role": "model", "parts": [{"function_call": {"name": "a", "args": {"x": 1, "y": 2}}}]}}
                ]
            },
            {
                "candidates": [
                    {"content": {"role": "model", "parts": [{"function_call": {"name": "a", "args": {"y": 2, "x": 1}}}]}}
                ]
            },
        ],
    },
    {
        "label": "camelCase 的 functionCall（裸负载）",
        "raw": True,
        "payloads": [
            {
                "candidates": [
                    {"content": {"role": "model", "parts": [{"functionCall": {"name": "z", "args": {}}}]}}
                ]
            }
        ],
    },
    {
        "label": "参数字符串形态",
        "payloads": [
            {
                "candidates": [
                    {
                        "content": {
                            "role": "model",
                            "parts": [{"function_call": {"name": "x", "args": '{"a": 1}'}}],
                        }
                    }
                ]
            }
        ],
    },
    {
        "label": "参数非法 JSON 字符串回落空对象",
        "payloads": [
            {
                "candidates": [
                    {
                        "content": {
                            "role": "model",
                            "parts": [{"function_call": {"name": "x", "args": '{"a":'}}],
                        }
                    }
                ]
            }
        ],
    },
    {
        "label": "参数非对象回落空对象",
        "payloads": [
            {
                "candidates": [
                    {"content": {"role": "model", "parts": [{"function_call": {"name": "x", "args": 5}}]}}
                ]
            }
        ],
    },
    {
        "label": "名称为空跳过",
        "payloads": [
            {
                "candidates": [
                    {"content": {"role": "model", "parts": [{"function_call": {"name": "", "args": {"a": 1}}}]}}
                ]
            }
        ],
    },
    {
        "label": "没有 candidates 时读 chunk.text",
        "payloads": [{"text": "只有文本"}],
    },
    {
        "label": "结束原因为空时保持默认",
        "payloads": [
            {"candidates": [{"content": {"role": "model", "parts": [{"text": "x"}]}, "finishReason": ""}]}
        ],
    },
    {
        "label": "空内容与未知分片忽略",
        "payloads": [
            {"candidates": []},
            {"candidates": [{"content": {"role": "model", "parts": [{"text": ""}]}}]},
            {"candidates": [{"content": {"role": "model", "parts": [{"inline_data": {"mime_type": "image/png", "data": "aGk="}}]}}]},
        ],
    },
    {
        "label": "空流",
        "payloads": [],
    },
]

USAGE_CASES = [
    {
        "label": "snake_case 完整字段",
        "payload": {"usage_metadata": {"prompt_token_count": 10, "candidates_token_count": 4}},
    },
    {
        "label": "camelCase 完整字段",
        "payload": {"usageMetadata": {"promptTokenCount": 7, "candidatesTokenCount": 2}},
    },
    {
        "label": "只给总量时算输入",
        "payload": {"usageMetadata": {"totalTokenCount": 9}},
    },
    {
        "label": "零值字段仍算出实体",
        "payload": {"usage_metadata": {}},
    },
    {
        "label": "布尔与字符串不算整数",
        "payload": {"usage_metadata": {"prompt_token_count": True, "candidates_token_count": "5"}},
    },
    {
        "label": "无 usage_metadata",
        "payload": {"candidates": []},
    },
    {
        "label": "usage_metadata 为 null",
        "payload": {"usage_metadata": None},
    },
]

ERROR_CASES = [
    {"message": "API key not valid. Please pass a valid API key.", "type_name": "ClientError"},
    {"message": "401 UNAUTHENTICATED", "type_name": "ClientError"},
    {"message": "unauthenticated: missing credentials", "type_name": "ClientError"},
    {"message": "403 PERMISSION_DENIED for model", "type_name": "ClientError"},
    {"message": "permission denied", "type_name": "ClientError"},
    {"message": "404 NOT_FOUND: model not found", "type_name": "ClientError"},
    {"message": "model not found", "type_name": "ClientError"},
    {"message": "429 RESOURCE_EXHAUSTED", "type_name": "ClientError"},
    {"message": "resource exhausted: quota", "type_name": "ClientError"},
    {"message": "quota exceeded", "type_name": "ClientError"},
    {"message": "Deadline exceeded: timeout", "type_name": "ClientError"},
    {"message": "internal server error", "type_name": "ServerError"},
]


def run_contents_case(case: dict) -> dict:
    authored = [message_from_json(item) for item in case["messages"]]
    encoded = [message_to_protocol_json(message) for message in authored]
    restored = tuple(message_from_protocol_json(item) for item in encoded)
    return {
        "label": case["label"],
        "messages": encoded,
        "expected": G._to_gemini_contents(restored),
    }


def run_option_case(case: dict) -> dict:
    error = None
    result = None
    try:
        result = G._sanitize_options(case["options"])
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
    request = build_turn_request(case)
    runtime, client = make_runtime([], model_id=case.get("model", "gemini-2.5-flash"))
    with config_as_dict():
        list(runtime._stream_turn_events(request, None, None, threading.Event()))
    return {
        "label": case["label"],
        "model": request.identity.model_id,
        "system_prompt": case.get("system_prompt", ""),
        "messages": [message_to_protocol_json(message) for message in request.messages],
        "tools": [tool_to_protocol_json(tool) for tool in request.tools],
        "options": case.get("options", {}),
        "kwargs": client.models.kwargs,
    }


def run_wire_case(case: dict) -> dict:
    request = build_turn_request(case)
    options = request.generation_options
    with config_as_dict():
        provider_options = G._sanitize_options(options.provider_options)
        config = G._build_generate_config(request, options, provider_options)
    contents = G._to_gemini_contents(request.messages)
    path, body = capture_wire(request.identity.model_id, contents, config)
    return {
        "label": case["label"],
        "model": request.identity.model_id,
        "system_prompt": case.get("system_prompt", ""),
        "messages": [message_to_protocol_json(message) for message in request.messages],
        "tools": [tool_to_protocol_json(tool) for tool in request.tools],
        "options": case.get("options", {}),
        "path": path,
        "body": body,
    }


def run_stream_case(case: dict) -> dict:
    request = build_turn_request(
        {
            "model": "gemini-2.5-flash",
            "messages": [{"role": "user", "blocks": [{"kind": "text", "text": "hi"}]}],
        }
    )
    runtime, _ = make_runtime(case["payloads"], raw=case.get("raw", False))
    events = list(runtime._stream_turn_events(request, None, None, threading.Event()))
    return {
        "label": case["label"],
        "payloads": case["payloads"],
        "expected": [event_to_json(event) for event in events],
    }


def main() -> None:
    fixture = {
        "source": "omnicrawl/llm/providers/gemini.py",
        "contents": [run_contents_case(case) for case in CONTENTS_CASES],
        "options": [run_option_case(case) for case in OPTION_CASES],
        "request": [run_request_case(case) for case in REQUEST_CASES],
        "wire": [run_wire_case(case) for case in WIRE_CASES],
        "stream": [run_stream_case(case) for case in STREAM_CASES],
        "usage": [
            {
                "label": case["label"],
                "payload": case["payload"],
                "expected": usage_from_gemini_payload(namespace(case["payload"])),
            }
            for case in USAGE_CASES
        ],
        "errors": [
            {
                "message": case["message"],
                "type_name": case["type_name"],
                "expected": G._format_gemini_error(
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
                "reasoning_tokens": expected.reasoning_tokens,
            }
        )

    FIXTURE_PATH.parent.mkdir(parents=True, exist_ok=True)
    FIXTURE_PATH.write_text(
        json.dumps(fixture, ensure_ascii=False, indent=2) + "\n", encoding="utf-8"
    )
    print(
        f"已写入 {FIXTURE_PATH.relative_to(ROOT)}："
        f"contents {len(fixture['contents'])}、选项 {len(fixture['options'])}、"
        f"请求 {len(fixture['request'])}、线上 {len(fixture['wire'])}、"
        f"流 {len(fixture['stream'])}、用量 {len(fixture['usage'])}、"
        f"错误 {len(fixture['errors'])}"
    )


if __name__ == "__main__":
    main()
