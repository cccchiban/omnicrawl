#!/usr/bin/env python3
"""生成 OpenAI Responses **流事件映射**的对照数据集。

期望值来自 Python 真实现：用假客户端 + 假流把事件负载喂给
`OpenAIResponsesRuntime._stream_turn_events`，记下它产出的事件序列
（统一投影成 `kind` 形式）或抛出的错误文案与可重试标记。

用法：``python rust/tools/gen_llm_responses_stream_fixture.py``
输出：``rust/crates/omnicrawl-llm/tests/fixtures/openai_responses_stream_parity.json``
"""

from __future__ import annotations

import json
import sys
import threading
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
FIXTURE_PATH = (
    ROOT / "rust/crates/omnicrawl-llm/tests/fixtures/openai_responses_stream_parity.json"
)

sys.path.insert(0, str(ROOT))

from omnicrawl.llm import protocol as P  # noqa: E402
from omnicrawl.llm import registry as R  # noqa: E402
from omnicrawl.llm.errors import ModelError  # noqa: E402
from omnicrawl.llm.providers.openai_responses import OpenAIResponsesAdapter  # noqa: E402

MODEL_ID = "gpt-5-codex"
PROTOCOL = P.PROTOCOL_OPENAI_RESPONSES


class FakeStream:
    """按顺序吐出事件负载的假流。"""

    def __init__(self, payloads, error=None):
        self._payloads = payloads
        self._error = error

    def __iter__(self):
        for payload in self._payloads:
            yield payload
        if self._error is not None:
            raise self._error

    def close(self):
        return None


class FakeResponses:
    def __init__(self, stream):
        self._stream = stream

    def create(self, **_kwargs):
        return self._stream


class FakeClient:
    def __init__(self, stream):
        self.responses = FakeResponses(stream)


def make_runtime(stream):
    profile = R.ProviderProfile(
        id="p1",
        provider="openai",
        api_key="test-key",
        default_protocol=PROTOCOL,
    )
    model = R.ModelDescriptor(
        identity=P.ModelIdentity(
            profile_id="p1",
            provider="openai",
            protocol=PROTOCOL,
            model_id=MODEL_ID,
        )
    )
    runtime = OpenAIResponsesAdapter().create_runtime(profile, model)
    runtime.client = FakeClient(stream)
    runtime._owns_client = False
    return runtime


def make_request():
    return P.ModelTurnRequest(
        identity=P.ModelIdentity(
            profile_id="p1",
            provider="openai",
            protocol=PROTOCOL,
            model_id=MODEL_ID,
        ),
        system_prompt="系统",
        messages=(P.ConversationMessage(role="user", blocks=(P.TextBlock(text="hi"),)),),
        tools=(),
        generation_options=P.GenerationOptions(),
        prompt_cache_identity={},
    )


def event_to_json(event) -> dict:
    if isinstance(event, P.TextDelta):
        return {"kind": "text_delta", "text": event.text}
    if isinstance(event, P.ReasoningDelta):
        return {"kind": "reasoning_delta", "text": event.text}
    if isinstance(event, P.ToolCallStarted):
        return {"kind": "tool_call_started", "call_id": event.call_id, "name": event.name}
    if isinstance(event, P.ToolCallCompleted):
        return {
            "kind": "tool_call_completed",
            "call_id": event.call_id,
            "name": event.name,
            "arguments": dict(event.arguments),
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
    raise TypeError(f"未覆盖的事件：{type(event)!r}")


class DotDict(dict):
    """模拟 SDK 的 pydantic 对象：属性访问与 dict 取值都能用。

    Python 侧的事件读取是「先 getattr，再 isinstance(dict) 回退」，真实链路里拿到的是 SDK 对象。
    数据集里的负载必须仿这个形态，否则会测出一个只在 dict 载荷下成立的假行为
    （例如 `response.status` 用 getattr 读不到，finish_reason 会永远是 stop）。
    """

    def __getattr__(self, name):
        try:
            return self[name]
        except KeyError as exc:
            raise AttributeError(name) from exc


def wrap(value):
    if isinstance(value, dict):
        return DotDict({key: wrap(item) for key, item in value.items()})
    if isinstance(value, list):
        return [wrap(item) for item in value]
    return value


def run_case(payloads, error=None) -> dict:
    runtime = make_runtime(FakeStream([wrap(payload) for payload in payloads], error))
    events: list[dict] = []
    try:
        for event in runtime._stream_turn_events(
            make_request(), None, None, threading.Event()
        ):
            events.append(event_to_json(event))
        return {"events": events}
    except ModelError as exc:
        return {
            "events": events,
            "error": exc.message,
            "retryable": exc.retryable,
            "code": exc.code.value,
        }


def text_delta(text: str) -> dict:
    return {"type": "response.output_text.delta", "delta": text}


def reasoning_delta(text: str) -> dict:
    return {"type": "response.reasoning_summary_text.delta", "delta": text}


def added(call_id: str, name: str, **extra) -> dict:
    item = {"type": "function_call", "id": call_id, "call_id": call_id, "name": name}
    item.update(extra)
    return {"type": "response.output_item.added", "item": item}


def arguments_delta(text: str, *, item_id: str = "", call_id: str = "", name: str = "") -> dict:
    event = {"type": "response.function_call_arguments.delta", "delta": text}
    if item_id:
        event["item_id"] = item_id
    if call_id:
        event["call_id"] = call_id
    if name:
        event["name"] = name
    return event


def arguments_done(
    *,
    item_id: str = "",
    call_id: str = "",
    name: str = "",
    arguments: str = "",
    item=None,
) -> dict:
    event = {"type": "response.function_call_arguments.done"}
    if item_id:
        event["item_id"] = item_id
    if call_id:
        event["call_id"] = call_id
    if name:
        event["name"] = name
    if arguments:
        event["arguments"] = arguments
    if item is not None:
        event["item"] = item
    return event


def completed(*, status="completed", output=(), usage=None) -> dict:
    response = {"status": status, "output": list(output)}
    if usage is not None:
        response["usage"] = usage
    return {"type": "response.completed", "response": response}


CALL_ITEM = {
    "type": "function_call",
    "call_id": "call_1",
    "name": "write_file",
    "arguments": '{"path": "a.txt"}',
}

CASES = [
    (
        "纯文本 + completed",
        [
            text_delta("你"),
            text_delta("好"),
            completed(status="completed"),
        ],
    ),
    (
        "推理增量（两种事件名）",
        [
            reasoning_delta("想"),
            {"type": "response.reasoning_text.delta", "delta": "考"},
            completed(),
        ],
    ),
    (
        "标准工具调用（added + delta + done 带 item）",
        [
            added("call_1", "write_file"),
            arguments_delta('{"path": ', item_id="call_1"),
            arguments_delta('"a.txt"}', item_id="call_1"),
            arguments_done(item=CALL_ITEM),
            completed(output=[CALL_ITEM]),
        ],
    ),
    (
        "added 只带名字 + 参数 delta + EOF（无 completed）",
        [
            added("call_1", "write_file"),
            arguments_delta('{"path": "a.txt"}', item_id="call_1"),
        ],
    ),
    (
        "arguments.done 顶层带 name 与最终参数",
        [
            arguments_done(item_id="call_1", name="write_file", arguments='{"path": "a.txt"}'),
            completed(),
        ],
    ),
    (
        "item_id 与 call_id 别名统一",
        [
            added("item_1", "write_file"),
            arguments_delta('{"path": ', item_id="item_1", call_id="call_9"),
            arguments_delta('"a.txt"}', item_id="item_1", call_id="call_9"),
            arguments_done(item_id="item_1", call_id="call_9", name="write_file", arguments='{"path": "a.txt"}'),
            completed(),
        ],
    ),
    (
        "output_item.done 直接给完整 item",
        [
            {"type": "response.output_item.done", "item": CALL_ITEM},
            completed(),
        ],
    ),
    (
        "只在 completed 的 output 里出现调用",
        [completed(output=[CALL_ITEM])],
    ),
    (
        "截断：只有名字、无参数",
        [added("call_1", "write_file")],
    ),
    (
        "截断：参数半截",
        [
            added("call_1", "write_file"),
            arguments_delta('{"path":', item_id="call_1"),
        ],
    ),
    ("截断：完全空流", []),
    (
        "只有文本 delta、无 completed（EOF 判定为真）",
        [text_delta("半截也算完成")],
    ),
    (
        "usage 事件",
        [
            text_delta("x"),
            completed(
                usage={
                    "input_tokens": 12,
                    "output_tokens": 3,
                    "input_tokens_details": {"cached_tokens": 5},
                    "output_tokens_details": {"reasoning_tokens": 2},
                }
            ),
        ],
    ),
    ("completed status=failed", [completed(status="failed")]),
    (
        "非 function_call 的 item 被忽略",
        [
            {"type": "response.output_item.done", "item": {"type": "message", "id": "m1"}},
            completed(output=[{"type": "reasoning", "id": "rs_1"}]),
        ],
    ),
    (
        "arguments.delta 无 id 被忽略",
        [arguments_delta("{}"), completed()],
    ),
    (
        "已发出的调用不重复产出",
        [
            {"type": "response.output_item.done", "item": CALL_ITEM},
            {"type": "response.output_item.done", "item": CALL_ITEM},
            completed(output=[CALL_ITEM]),
        ],
    ),
    (
        "多个调用按插入序冲刷",
        [
            added("call_a", "first"),
            arguments_delta('{"a": 1}', item_id="call_a"),
            added("call_b", "second"),
            arguments_delta('{"b": 2}', item_id="call_b"),
        ],
    ),
]


def main() -> None:
    entries = []
    for label, payloads in CASES:
        result = run_case(payloads)
        entries.append({"label": label, "payloads": payloads, **result})

    fixture = {
        "source": "omnicrawl/llm/providers/openai_responses.py（流事件映射部分）",
        "cases": entries,
    }
    FIXTURE_PATH.parent.mkdir(parents=True, exist_ok=True)
    FIXTURE_PATH.write_text(
        json.dumps(fixture, ensure_ascii=False, indent=2) + chr(10), encoding="utf-8"
    )
    errors = sum(1 for entry in entries if "error" in entry)
    print(
        "已写入 %s：场景 %d（其中报错 %d）"
        % (FIXTURE_PATH.relative_to(ROOT), len(entries), errors)
    )


if __name__ == "__main__":
    main()
