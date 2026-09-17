#!/usr/bin/env python3
"""生成内核 runtime 的端到端对照数据集，供 Rust 侧 `omnicrawl-llm` 的 parity 测试使用。

期望值来自 Python 真实现：每个用例起一个本机回环 HTTP 服务端，把固定的 SSE 响应喂给
``OpenAIChatCompletionsRuntime``（真 SDK 客户端 + 仓库自己的 httpx 客户端工厂），
记录它收到的事件序列、它实际发出的请求体，以及失败时的错误文案。
Rust 侧用同一份 SSE 重放，逐字段比对事件、请求体与归并结果。

用法：``python rust/tools/gen_llm_runtime_fixture.py``
输出：``rust/crates/omnicrawl-llm/tests/fixtures/openai_chat_runtime_parity.json``
"""

from __future__ import annotations

import json
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from types import SimpleNamespace

ROOT = Path(__file__).resolve().parents[2]
FIXTURE_PATH = (
    ROOT / "rust/crates/omnicrawl-llm/tests/fixtures/openai_chat_runtime_parity.json"
)

sys.path.insert(0, str(ROOT))
sys.path.insert(0, str(Path(__file__).resolve().parent))

import omnicrawl.llm.providers.openai_chat as C  # noqa: E402
from gen_llm_request_fixture import python_messages  # noqa: E402
from omnicrawl.llm.protocol import (  # noqa: E402
    ConversationMessage,
    GenerationOptions,
    ModelTurnRequest,
    ProviderWarning,
    ReasoningDelta,
    ResponseCompleted,
    TextDelta,
    ToolCallArgumentsDelta,
    ToolCallCompleted,
    ToolCallStarted,
    ToolSpec,
    UsageUpdated,
    aggregate_stream_events,
)
from omnicrawl.net.http_client import create_direct_client  # noqa: E402

if not Path(C.__file__).resolve().is_relative_to(ROOT):
    raise SystemExit(f"加载到的不是仓库源码：{C.__file__}")


TEXT_STREAM = (
    'data: {"choices":[{"delta":{"role":"assistant"}}]}\n\n'
    'data: {"choices":[{"delta":{"content":"你"}}]}\n\n'
    'data: {"choices":[{"delta":{"content":"好"}}]}\n\n'
    'data: {"choices":[{"delta":{"reasoning_content":"先想一下"}}]}\n\n'
    'data: {"choices":[{"delta":{},"finish_reason":"stop"}],"usage":'
    '{"prompt_tokens":11,"completion_tokens":3,"prompt_tokens_details":{"cached_tokens":4},'
    '"completion_tokens_details":{"reasoning_tokens":2}}}\n\n'
    "data: [DONE]\n\n"
)

TOOL_STREAM = (
    'data: {"choices":[{"delta":{"tool_calls":'
    '[{"index":0,"id":"call_a","function":{"name":"read_file"}}]}}]}\n\n'
    'data: {"choices":[{"delta":{"tool_calls":'
    '[{"index":0,"function":{"arguments":"{\\"path\\""}}]}}]}\n\n'
    'data: {"choices":[{"delta":{"tool_calls":'
    '[{"index":0,"function":{"arguments":":\\"a.py\\"}"}}]}}]}\n\n'
    "data: [DONE]\n\n"
)

TRUNCATED_ARGUMENTS_STREAM = (
    'data: {"choices":[{"delta":{"tool_calls":'
    '[{"index":0,"id":"call_a","function":{"name":"read_file"}}]}}]}\n\n'
    'data: {"choices":[{"delta":{"tool_calls":'
    '[{"index":0,"function":{"arguments":"{\\"path\\""}}]}}]}\n\n'
    "data: [DONE]\n\n"
)

TRUNCATED_NAME_STREAM = (
    'data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_a"}]}}]}\n\n'
    "data: [DONE]\n\n"
)

STREAM_ERROR = (
    'data: {"choices":[{"delta":{"content":"半"}}]}\n\n'
    'data: {"error":{"message":"boom"}}\n\n'
)

PROMPT_CACHE_REJECTION = (
    '{"error":{"message":"Unrecognized request argument supplied: prompt_cache_key"}}'
)


def user_message(text: str) -> dict:
    return {
        "role": "user",
        "blocks": [{"Text": {"text": text}}],
        "reasoning": "",
        "tools": [],
    }


def case(name: str, sse: str, **overrides) -> dict:
    base = {
        "name": name,
        "model": "qwen3.6-plus",
        "system_prompt": "",
        "messages": [user_message("你好")],
        "tools": [],
        "options": {},
        "prompt_cache_capable": False,
        "prompt_cache_identity": {},
        "replies": [[200, sse]],
    }
    base.update(overrides)
    return base


CASES = [
    case("text_reasoning_usage", TEXT_STREAM),
    case("tool_call_stream", TOOL_STREAM),
    case("truncated_arguments", TRUNCATED_ARGUMENTS_STREAM),
    case("truncated_name", TRUNCATED_NAME_STREAM),
    case("empty_stream", "data: [DONE]\n\n"),
    case("stream_error_payload", STREAM_ERROR),
    case(
        "http_401",
        '{"error":{"message":"bad key"}}',
        replies=[[401, '{"error":{"message":"bad key"}}']],
    ),
    case(
        "http_503",
        '{"error":{"message":"upstream down"}}',
        replies=[[503, '{"error":{"message":"upstream down"}}']],
    ),
    case(
        "prompt_cache_rejected",
        TEXT_STREAM,
        model="gpt-5.2",
        prompt_cache_capable=True,
        prompt_cache_identity={"profile_id": "p1", "provider": "openai"},
        replies=[[400, PROMPT_CACHE_REJECTION], [200, TEXT_STREAM]],
    ),
]


class _Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    replies: list = []
    bodies: list = []

    def do_POST(self) -> None:  # noqa: N802 - BaseHTTPRequestHandler 的接口
        length = int(self.headers.get("Content-Length") or 0)
        raw = self.rfile.read(length) if length else b""
        try:
            self.bodies.append(json.loads(raw.decode("utf-8")))
        except Exception:  # noqa: BLE001 - 记录不到正文不影响对照
            self.bodies.append({})

        status, body = self.replies.pop(0) if len(self.replies) > 1 else self.replies[0]
        payload = body.encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Content-Length", str(len(payload)))
        self.send_header("Connection", "close")
        self.end_headers()
        self.wfile.write(payload)
        self.close_connection = True

    def log_message(self, *args: object) -> None:
        return


def event_to_json(event: object) -> dict:
    """事件投影：与 Rust `ModelStreamEvent` 的变体一一对应。"""
    if isinstance(event, TextDelta):
        return {"kind": "text", "text": event.text}
    if isinstance(event, ReasoningDelta):
        return {"kind": "reasoning", "text": event.text}
    if isinstance(event, ToolCallStarted):
        return {"kind": "tool_started", "call_id": event.call_id, "name": event.name}
    if isinstance(event, ToolCallArgumentsDelta):
        return {
            "kind": "tool_arguments",
            "call_id": event.call_id,
            "delta": event.delta,
        }
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
    if isinstance(event, ProviderWarning):
        return {"kind": "warning", "code": event.code, "message": event.message}
    raise TypeError(f"未知事件：{type(event)!r}")


def usage_to_json(usage: object) -> dict | None:
    if usage is None:
        return None
    return {
        "input_tokens": usage.input_tokens,
        "output_tokens": usage.output_tokens,
        "cached_input_tokens": usage.cached_input_tokens,
        "reasoning_tokens": usage.reasoning_tokens,
    }


def reply_to_json(result: object) -> dict:
    return {
        "content": result.content,
        "reasoning": result.reasoning,
        "finish_reason": result.finish_reason,
        "content_streamed": result.content_streamed,
        "usage": usage_to_json(result.usage),
        "tool_calls": [tool_call_to_json(call) for call in result.tool_calls],
    }


def tool_call_to_json(call: object) -> dict:
    return {
        "call_id": call.call_id,
        "name": call.name,
        "arguments": call.arguments,
    }


def run_case(raw_case: dict) -> dict:
    replies = [(status, body) for status, body in raw_case["replies"]]
    bodies: list = []
    handler = type(
        "CaseHandler",
        (_Handler,),
        {"replies": list(replies), "bodies": bodies},
    )
    server = ThreadingHTTPServer(("127.0.0.1", 0), handler)
    port = server.server_address[1]
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()

    events: list = []
    error = None
    reply = None
    try:
        client = __import__("openai").OpenAI(
            api_key="test-key",
            base_url=f"http://127.0.0.1:{port}/v1",
            http_client=create_direct_client(),
        )
        runtime = C.OpenAIChatCompletionsRuntime(
            identity=SimpleNamespace(model_id=raw_case["model"]),
            capabilities=SimpleNamespace(
                streaming=True,
                tools=True,
                prompt_cache=raw_case["prompt_cache_capable"],
            ),
            client=client,
            profile=SimpleNamespace(request_timeout_seconds=30.0),
            descriptor=SimpleNamespace(),
        )
        request = ModelTurnRequest(
            identity=runtime.identity,
            system_prompt=raw_case["system_prompt"],
            messages=python_messages(raw_case["messages"]),
            tools=tuple(ToolSpec(**tool) for tool in raw_case["tools"]),
            generation_options=GenerationOptions(**(raw_case.get("options") or {})),
            prompt_cache_identity=dict(raw_case.get("prompt_cache_identity") or {}),
        )

        collected: list = []
        retryable = None
        try:
            for event in runtime.stream_turn(request):
                collected.append(event)
        except Exception as exc:  # noqa: BLE001 - 只对照错误文案与可重试标记
            error = getattr(exc, "message", None) or str(exc)
            retryable = getattr(exc, "retryable", None)
        events = [event_to_json(event) for event in collected]
        if error is None:
            reply = reply_to_json(aggregate_stream_events(collected))
        client.close()
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=5)

    return {
        "name": raw_case["name"],
        "input": {
            "model": raw_case["model"],
            "system_prompt": raw_case["system_prompt"],
            "messages": raw_case["messages"],
            "tools": raw_case["tools"],
            "options": raw_case["options"],
            "profile_request_timeout_seconds": 30.0,
            "prompt_cache_capable": raw_case["prompt_cache_capable"],
            "prompt_cache_identity": raw_case["prompt_cache_identity"],
        },
        "replies": [
            {"status": status, "body": body} for status, body in raw_case["replies"]
        ],
        "expected": {
            "events": events,
            "error": error,
            "retryable": retryable,
            "reply": reply,
            "requests": bodies,
        },
    }


def main() -> None:
    fixture = {
        "source": [
            "omnicrawl/llm/providers/openai_chat.py",
            "omnicrawl/net/http_client.py",
        ],
        "cases": [run_case(item) for item in CASES],
    }
    FIXTURE_PATH.parent.mkdir(parents=True, exist_ok=True)
    FIXTURE_PATH.write_text(
        json.dumps(fixture, ensure_ascii=False, indent=2) + "\n", encoding="utf-8"
    )
    failed = [case["name"] for case in fixture["cases"] if case["expected"]["error"]]
    print(
        f"已写入 {FIXTURE_PATH.relative_to(ROOT)}：用例 {len(fixture['cases'])}"
        f"（其中失败用例 {len(failed)}：{failed}）"
    )


if __name__ == "__main__":
    main()
