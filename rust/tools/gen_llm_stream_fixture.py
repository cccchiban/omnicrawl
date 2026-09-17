#!/usr/bin/env python3
"""生成 openai_chat 流StreamEvent对照数据集，供 Rust 侧 `omnicrawl-llm` 的 parity 测试使用。

期望值全部由 Python 真实现（``omnicrawl/llm/providers/openai_chat.py``）产出，
Rust 侧只做同构映射后逐字段比对，避免两侧语义各自漂移。覆盖四组：
参数完整性判定、工具调用分片归并、SSE 负载解码、SSE 负载流消费。

用法：``python rust/tools/gen_llm_stream_fixture.py``
输出：``rust/crates/omnicrawl-llm/tests/fixtures/openai_chat_stream_parity.json``
"""

from __future__ import annotations

import json
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
FIXTURE_PATH = (
    ROOT / "rust/crates/omnicrawl-llm/tests/fixtures/openai_chat_stream_parity.json"
)

# 必须加载仓库源码：已安装的 omnicrawl 在 site-packages，会对照到另一份实现。
sys.path.insert(0, str(ROOT))

import omnicrawl.llm.providers.openai_chat as C  # noqa: E402
from openai import APIError  # noqa: E402

if not Path(C.__file__).resolve().is_relative_to(ROOT):
    raise SystemExit(f"加载到的不是仓库源码：{C.__file__}")


class FakeSSE:
    """只暴露 ``data`` 的 SSE 事件替身，强制走 Python 侧的原始字符串分支。"""

    def __init__(self, data: str) -> None:
        self.data = data


class FakeResponse:
    """记录是否被关闭的 HTTP 响应替身。"""

    def __init__(self) -> None:
        self.request = None
        self.closed = False

    def close(self) -> None:
        self.closed = True


class FakeStream:
    """只提供 ``_iter_events`` 与 ``response`` 的流替身。"""

    def __init__(self, payloads: list) -> None:
        self._payloads = list(payloads)
        self.response = FakeResponse()

    def _iter_events(self):
        for payload in self._payloads:
            yield FakeSSE(payload)


def event_to_json(event) -> dict:
    """分片事件的稳定投影；只可能出现两种工具调用事件。"""

    if hasattr(event, "delta") and hasattr(event, "call_id"):
        return {
            "kind": "tool_call_arguments_delta",
            "call_id": event.call_id,
            "delta": event.delta,
        }
    if hasattr(event, "name") and hasattr(event, "call_id"):
        return {
            "kind": "tool_call_started",
            "call_id": event.call_id,
            "name": event.name,
        }
    raise TypeError(f"未知分片事件：{type(event)!r}")


ARGS_CASES = [
    "",
    "   ",
    "{}",
    '{"a": 1}',
    '{"a":',
    "{oops",
    "[]",
    "null",
    "123",
    '{"路径": "中文"}',
    '  {"a": 1}  ',
    123,
    0,
    None,
    {"a": 1},
    [],
]

TOOL_DELTA_CASES = [
    {
        "name": "name_then_arguments",
        "deltas": [
            {"index": 0, "id": "call_a", "function": {"name": "read_file", "arguments": ""}},
            {"index": 0, "function": {"arguments": '{"path"'}},
            {"index": 0, "function": {"arguments": ': "a.py"}'}},
        ],
    },
    {
        "name": "missing_index_defaults_to_zero",
        "deltas": [
            {"function": {"name": "grep"}},
            {"function": {"arguments": '{"q": 1}'}},
        ],
    },
    {
        "name": "id_arrives_after_started",
        "deltas": [
            {"index": 0, "function": {"name": "read_file"}},
            {"index": 0, "id": "call_late", "function": {"arguments": "{}"}},
            {"index": 0, "function": {"arguments": " "}},
        ],
    },
    {
        "name": "parallel_interleaved_calls",
        "deltas": [
            {"index": 0, "id": "c0", "function": {"name": "a"}},
            {"index": 1, "id": "c1", "function": {"name": "b"}},
            {"index": 0, "function": {"arguments": '{"x"'}},
            {"index": 1, "function": {"arguments": '{"y": 1}'}},
            {"index": 0, "function": {"arguments": ": 2}"}},
        ],
    },
    {
        "name": "arguments_without_name",
        "deltas": [{"index": 2, "function": {"arguments": '{"z": 3}'}}],
    },
    {
        "name": "falsy_fields_are_ignored",
        "deltas": [
            {"index": 0, "id": "", "function": {}},
            {"index": 0, "id": 0, "function": {"name": "", "arguments": ""}},
            {"index": 0, "function": {"name": "read", "arguments": "{}"}},
        ],
    },
    {
        "name": "numeric_id_is_stringified",
        "deltas": [{"index": 3, "id": 12345, "function": {"name": "bash"}}],
    },
    {
        "name": "non_object_function_is_ignored",
        "deltas": [
            {"index": 0, "id": "c0", "function": "read_file"},
            {"index": 0, "id": "c0", "function": None},
            {"index": 0, "id": "c0", "function": {"name": "read_file"}},
        ],
    },
]

SSE_DECODE_CASES = [
    '{"choices": []}',
    '  {"a": 1}  ',
    "[1, 2]",
    '"text"',
    "null",
    "123",
    "not json",
    "",
    "   ",
    '{"error": {"message": "boom"}}',
]

SSE_STREAM_CASES = [
    {
        "name": "stops_at_done",
        "payloads": ['{"choices": []}', "[DONE]", '{"choices": [1]}'],
    },
    {
        "name": "done_with_trailing_suffix",
        "payloads": ['{"ok": true}', "[DONE]extra"],
    },
    {
        "name": "skips_non_object_payloads",
        "payloads": ["[1, 2]", '"text"', '{"ok": true}', "null"],
    },
    {
        "name": "skips_unparseable_payloads",
        "payloads": ["not json", "", '{"ok": true}'],
    },
    {
        "name": "empty_stream",
        "payloads": [],
    },
    {
        "name": "error_payload_raises",
        "payloads": ['{"ok": true}', '{"error": {"message": "boom", "type": "x"}}'],
    },
    {
        "name": "error_without_message",
        "payloads": ['{"error": {"type": "x"}}'],
    },
    {
        "name": "error_message_not_string",
        "payloads": ['{"error": {"message": 500}}'],
    },
    {
        "name": "error_not_object",
        "payloads": ['{"error": "plain"}'],
    },
]

FIRST_CHOICE_CASES = [
    {"choices": [{"delta": {"content": "a"}}]},
    {"choices": []},
    {},
    {"choices": "not a list"},
    {"choices": [None, {"x": 1}]},
    {"choices": [{"finish_reason": "stop"}]},
]


def run_args_case(value) -> dict:
    return {"value": value, "expected": C._arguments_json_complete(value)}


def run_tool_delta_case(case: dict) -> dict:
    buffers: dict = {}
    started: set = set()
    events = list(C._emit_tool_call_deltas(list(case["deltas"]), buffers, started))
    return {
        "name": case["name"],
        "deltas": case["deltas"],
        "events": [event_to_json(event) for event in events],
        "buffers": {
            str(index): {
                "id": buffer["id"],
                "name": buffer["name"],
                "arguments": buffer["arguments"],
            }
            for index, buffer in sorted(buffers.items())
        },
        "started": sorted(started),
    }


def run_sse_decode_case(payload: str) -> dict:
    return {"payload": payload, "expected": C._decode_sse_data(FakeSSE(payload))}


def run_sse_stream_case(case: dict) -> dict:
    stream = FakeStream(case["payloads"])
    events: list = []
    error = None
    try:
        events = list(C._iter_raw_sse_events(stream))
    except APIError as exc:
        error = {
            "type": "APIError",
            "message": getattr(exc, "message", None) or str(exc),
            "body": getattr(exc, "body", None),
        }
    return {
        "name": case["name"],
        "payloads": case["payloads"],
        "events": events,
        "closed": stream.response.closed,
        "error": error,
    }


def run_first_choice_case(chunk: dict) -> dict:
    return {"chunk": chunk, "expected": C._first_choice(chunk)}


def main() -> None:
    fixture = {
        "source": "omnicrawl/llm/providers/openai_chat.py",
        "args_complete": [run_args_case(value) for value in ARGS_CASES],
        "tool_call_deltas": [run_tool_delta_case(case) for case in TOOL_DELTA_CASES],
        "sse_decode": [run_sse_decode_case(payload) for payload in SSE_DECODE_CASES],
        "sse_stream": [run_sse_stream_case(case) for case in SSE_STREAM_CASES],
        "first_choice": [run_first_choice_case(chunk) for chunk in FIRST_CHOICE_CASES],
    }
    FIXTURE_PATH.parent.mkdir(parents=True, exist_ok=True)
    FIXTURE_PATH.write_text(
        json.dumps(fixture, ensure_ascii=False, indent=2) + "\n", encoding="utf-8"
    )
    print(
        f"已写入 {FIXTURE_PATH.relative_to(ROOT)}："
        f"参数 {len(fixture['args_complete'])}、分片 {len(fixture['tool_call_deltas'])}、"
        f"SSE 解码 {len(fixture['sse_decode'])}、SSE 流 {len(fixture['sse_stream'])}、"
        f"首选项 {len(fixture['first_choice'])}"
    )


if __name__ == "__main__":
    main()
