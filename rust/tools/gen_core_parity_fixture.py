#!/usr/bin/env python3
"""生成回合循环对照数据集，供 Rust 侧 `omnicrawl-core` 的 parity 测试使用。

期望值全部由 Python 实现（``omnicrawl/agent/runtime/execution.py`` 的循环执行器）直接产出，
Rust 侧只做同构投影后逐字段比对。侧重放｛Desensitized:881｝脚本化：模型回复、
工具批次、取消与停止触发点、时钟序列都来自用例定义，两侧跑同一份输入。

用法：``python rust/tools/gen_core_parity_fixture.py``
输出：``rust/crates/omnicrawl-core/tests/fixtures/turn_loop_parity.json``
"""

from __future__ import annotations

import json
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
FIXTURE_PATH = ROOT / "rust/crates/omnicrawl-core/tests/fixtures/turn_loop_parity.json"

# 必须加载仓库源码：已安装的 omnicrawl 包可能在 site-packages，会对照到另一份实现。
sys.path.insert(0, str(ROOT))

import omnicrawl.agent.runtime.execution as E  # noqa: E402
from omnicrawl.agent.types import ToolCall, ToolResult, AgentModelReply  # noqa: E402

if not Path(E.__file__).resolve().is_relative_to(ROOT):
    raise SystemExit(f"加载到的不是仓库源码：{E.__file__}")

SEED_MESSAGES = [
    {"role": "system", "content": "sys"},
    {"role": "user", "content": "hi"},
]


class _Cancelled(Exception):
    """宿主取消（Python 侧由 cancel_check 抛出）。"""


class _ReplyBoom(Exception):
    """模型回复失败。"""


class _BatchBoom(Exception):
    """工具批次执行失败。"""


class _FakeClock:
    """按序列｛Desensitized:879｝时刻的单调时钟，并记录取时刻次数。"""

    def __init__(self, values: list) -> None:
        self._values = list(values)
        self._last = self._values[0] if self._values else 0.0
        self.calls = 0

    def monotonic(self) -> float:
        self.calls += 1
        if self._values:
            self._last = self._values.pop(0)
        return self._last


def build_limits(spec: dict):
    if spec is None:
        return None
    return E.AgentLoopLimits(
        max_model_turns=spec.get("max_model_turns"),
        max_tool_calls=spec.get("max_tool_calls"),
        timeout_seconds=spec.get("timeout_seconds"),
    )


def build_reply(spec: dict):
    return AgentModelReply(
        message={"role": "assistant", "content": spec.get("content", "")},
        content=spec.get("content", ""),
        tool_calls=[
            ToolCall(
                name=call["name"],
                arguments=call.get("arguments", {}),
                id=call.get("id", ""),
                function_name=call.get("function_name", ""),
            )
            for call in spec.get("tool_calls", [])
        ],
        reasoning=spec.get("reasoning", ""),
        content_streamed=spec.get("content_streamed", False),
    )


def build_observation(spec: dict):
    call = spec["tool_call"]
    return E.AgentLoopObservation(
        tool_call=ToolCall(
            name=call["name"],
            arguments=call.get("arguments", {}),
            id=call.get("id", ""),
        ),
        result=ToolResult(
            ok=spec["result"].get("ok", True),
            output=spec["result"].get("output", ""),
            full_output=spec["result"].get("full_output", ""),
            error_code=spec["result"].get("error_code"),
            retryable=spec["result"].get("retryable", False),
        ),
        message=spec["message"],
        followup_messages=tuple(spec.get("followup_messages", [])),
    )


def project_call(call) -> dict:
    return {
        "name": call.name,
        "arguments": call.arguments,
        "id": call.id,
        "function_name": call.function_name,
    }


def project_reply(reply) -> dict:
    return {
        "message": reply.message,
        "content": reply.content,
        "tool_calls": [project_call(call) for call in reply.tool_calls],
        "reasoning": reply.reasoning,
        "content_streamed": reply.content_streamed,
    }


def project_observation(observation) -> dict:
    result = observation.result
    return {
        "tool_call": project_call(observation.tool_call),
        "result": {
            "ok": result.ok,
            "output": result.output,
            "full_output": result.full_output,
            "error_code": result.error_code,
            "retryable": result.retryable,
        },
        "message": observation.message,
        "followup_messages": list(observation.followup_messages),
    }


def build_wire(spec: dict) -> tuple:
    """两侧共同消费的输入形状：直接交给 Rust 反序列化，避免各自拼装。"""

    replies = [project_reply(build_reply(reply)) for reply in spec["scripted_replies"]]
    batches = [
        [project_observation(build_observation(observation)) for observation in batch]
        for batch in spec.get("scripted_batches", [])
    ]
    return replies, batches


def classify(exc: BaseException) -> dict:
    if isinstance(exc, E.AgentLoopBudgetExceeded):
        tag = "budget_exceeded"
    elif isinstance(exc, _Cancelled):
        tag = "cancelled"
    elif isinstance(exc, _ReplyBoom):
        tag = "reply_source"
    elif isinstance(exc, _BatchBoom):
        tag = "tool_batch"
    elif isinstance(exc, ValueError):
        tag = "invalid_budget"
    elif isinstance(exc, RuntimeError):
        tag = "observation_mismatch"
    else:
        tag = "other"
    return {"tag": tag, "message": str(exc)}


class Harness:
    """把用例里的脚本化输入接到循环的四个回调上，并记录调用轨迹。"""

    def __init__(self, spec: dict) -> None:
        self.replies = [build_reply(reply) for reply in spec["scripted_replies"]]
        self.batches = [
            [build_observation(observation) for observation in batch]
            for batch in spec.get("scripted_batches", [])
        ]
        self.clock = _FakeClock(spec.get("clock", [0.0]))
        self.cancel_at = spec.get("cancel_at")
        self.stop_after_batch = spec.get("stop_after_batch")
        self.reply_error_at = spec.get("reply_error_at")
        self.batch_error_at = spec.get("tool_error_at")
        self.request_log: list = []
        self.batch_log: list = []
        self.boundary_calls = 0

    def cancel_check(self) -> None:
        self.boundary_calls += 1
        if self.cancel_at is not None and self.boundary_calls == self.cancel_at:
            raise _Cancelled("宿主取消")

    def stop_check(self) -> bool:
        if self.stop_after_batch is None:
            return False
        return len(self.batch_log) >= self.stop_after_batch

    def request_reply(self, messages):
        self.request_log.append({"messages_len": len(messages)})
        index = len(self.request_log)
        if self.reply_error_at is not None and index == self.reply_error_at:
            raise _ReplyBoom("模型请求失败")
        return self.replies[index - 1]

    def execute_tool_batch(self, calls, first_step: int):
        index = len(self.batch_log) + 1
        self.batch_log.append(
            {"first_step": first_step, "calls": [call.name for call in calls]}
        )
        if self.batch_error_at is not None and index == self.batch_error_at:
            raise _BatchBoom("工具批次失败")
        return list(self.batches[index - 1])


def run_case(spec: dict) -> dict:
    harness = Harness(spec)
    messages = [dict(message) for message in SEED_MESSAGES]
    original_time = E.time
    E.time = harness.clock
    try:
        try:
            limits = build_limits(spec.get("limits"))
        except ValueError as exc:
            return {
                "ok": False,
                "error": classify(exc),
                "messages": messages,
                "request_log": harness.request_log,
                "batch_log": harness.batch_log,
                "clock_calls": harness.clock.calls,
                "boundary_calls": harness.boundary_calls,
            }
        try:
            result = E.AgentLoopRunner().run(
                messages=messages,
                request_reply=harness.request_reply,
                execute_tool_batch=harness.execute_tool_batch,
                limits=limits,
                cancel_check=harness.cancel_check,
                stop_check=harness.stop_check,
            )
        except BaseException as exc:  # noqa: BLE001 - 期望值需要记录真实specifically
            return {
                "ok": False,
                "error": classify(exc),
                "messages": messages,
                "request_log": harness.request_log,
                "batch_log": harness.batch_log,
                "clock_calls": harness.clock.calls,
                "boundary_calls": harness.boundary_calls,
            }
    finally:
        E.time = original_time

    # 结果内嵌的 messages 就是调用方这一份，两侧都以此为准。
    assert result.messages is messages, "循环结果应引用调用方的消息列表"
    return {
        "ok": True,
        "final_text": result.final_text,
        "reasoning": result.reasoning,
        "content_streamed": result.content_streamed,
        "model_turns": result.model_turns,
        "tool_calls": result.tool_calls,
        "paused": result.paused,
        "has_last_reply": result.last_reply is not None,
        "messages": messages,
        "request_log": harness.request_log,
        "batch_log": harness.batch_log,
        "clock_calls": harness.clock.calls,
        "boundary_calls": harness.boundary_calls,
    }


def _text_case(name: str, **kwargs) -> dict:
    spec = {
        "name": name,
        "limits": None,
        "clock": [0.0],
        "scripted_replies": [{"content": "答案", "content_streamed": True}],
        "scripted_batches": [],
    }
    spec.update(kwargs)
    return spec


def tool_call(name: str, index: int) -> dict:
    return {"name": name, "arguments": {"i": index}, "id": f"call_{index}"}


def observation(name: str, index: int, followups: list, ok: bool = True) -> dict:
    return {
        "tool_call": tool_call(name, index),
        "result": {"ok": ok, "output": f"{name}-output-{index}"},
        "message": {"role": "tool", "tool_call_id": f"call_{index}", "content": f"{name}-output-{index}"},
        "followup_messages": followups,
    }


CASES = [
    _text_case("empty_reply", scripted_replies=[{"content": ""}]),
    _text_case("padded_text_is_stripped", scripted_replies=[{"content": "  答案  "}]),
    _text_case(
        "reasoning_is_joined",
        scripted_replies=[
            {
                "content": "",
                "reasoning": "think-1",
                "tool_calls": [tool_call("read", 0)],
            },
            {"content": "done", "reasoning": "think-2"},
        ],
        scripted_batches=[[observation("read", 0, [])]],
    ),
    _text_case(
        "single_tool_round_orders_messages",
        scripted_replies=[
            {
                "content": "",
                "reasoning": "plan",
                "tool_calls": [tool_call("read", 0), tool_call("grep", 1)],
            },
            {"content": "final"},
        ],
        scripted_batches=[[observation("read", 0, []), observation("grep", 1, [])]],
    ),
    _text_case(
        "followups_come_after_all_tool_messages",
        scripted_replies=[
            {"content": "", "tool_calls": [tool_call("shot", 0), tool_call("read", 1)]},
            {"content": "final"},
        ],
        scripted_batches=[
            [
                observation("shot", 0, [{"role": "user", "content": "image-a"}, {"role": "user", "content": "image-b"}]),
                observation("read", 1, [{"role": "user", "content": "image-c"}]),
            ]
        ],
    ),
    _text_case(
        "second_round_keeps_call_counting",
        scripted_replies=[
            {"content": "", "tool_calls": [tool_call("read", 0), tool_call("read", 1)]},
            {"content": "", "tool_calls": [tool_call("read", 2)]},
            {"content": "final"},
        ],
        scripted_batches=[
            [observation("read", 0, []), observation("read", 1, [])],
            [observation("read", 2, [])],
        ],
    ),
    _text_case(
        "observation_count_mismatch",
        scripted_replies=[
            {"content": "", "tool_calls": [tool_call("read", 0), tool_call("read", 1)]},
        ],
        scripted_batches=[[observation("read", 0, [])]],
    ),
    _text_case(
        "cancel_before_first_request",
        cancel_at=1,
        scripted_replies=[{"content": "unused"}],
    ),
    _text_case(
        "cancel_after_first_batch",
        cancel_at=2,
        scripted_replies=[
            {"content": "", "tool_calls": [tool_call("read", 0)]},
            {"content": "unused"},
        ],
        scripted_batches=[[observation("read", 0, [])]],
    ),
    _text_case(
        "max_model_turns_exceeded",
        limits={"max_model_turns": 1},
        scripted_replies=[
            {"content": "", "tool_calls": [tool_call("read", 0)]},
            {"content": "unused"},
        ],
        scripted_batches=[[observation("read", 0, [])]],
    ),
    _text_case(
        "max_tool_calls_exceeded_before_append",
        limits={"max_tool_calls": 1},
        scripted_replies=[
            {"content": "", "tool_calls": [tool_call("read", 0), tool_call("read", 1)]},
        ],
        scripted_batches=[],
    ),
    _text_case(
        "timeout_exceeded_after_batch",
        limits={"timeout_seconds": 5},
        clock=[0.0, 0.0, 6.0],
        scripted_replies=[
            {"content": "", "tool_calls": [tool_call("read", 0)]},
            {"content": "unused"},
        ],
        scripted_batches=[[observation("read", 0, [])]],
    ),
    _text_case(
        "timeout_not_exceeded_uses_deadline",
        limits={"timeout_seconds": 5},
        clock=[0.0, 0.0, 4.9, 4.9],
        scripted_replies=[
            {"content": "", "tool_calls": [tool_call("read", 0)]},
            {"content": "final"},
        ],
        scripted_batches=[[observation("read", 0, [])]],
    ),
    _text_case("invalid_budget_zero_model_turns", limits={"max_model_turns": 0}),
    _text_case("invalid_budget_zero_timeout", limits={"timeout_seconds": 0}),
    _text_case("reply_error_first_request", reply_error_at=1),
    _text_case(
        "reply_error_second_request",
        reply_error_at=2,
        scripted_replies=[
            {"content": "", "tool_calls": [tool_call("read", 0)]},
            {"content": "unused"},
        ],
        scripted_batches=[[observation("read", 0, [])]],
    ),
    _text_case(
        "batch_error_after_assistant_message",
        tool_error_at=1,
        scripted_replies=[
            {"content": "", "tool_calls": [tool_call("read", 0)]},
        ],
        scripted_batches=[[]],
    ),
    _text_case(
        "failed_tool_result_is_not_fatal",
        scripted_replies=[
            {"content": "", "tool_calls": [tool_call("read", 0)]},
            {"content": "recovered"},
        ],
        scripted_batches=[[observation("read", 0, [], ok=False)]],
    ),
    _text_case(
        "stop_after_first_batch_pauses",
        stop_after_batch=1,
        scripted_replies=[
            {"content": "", "tool_calls": [tool_call("read", 0), tool_call("read", 1)]},
            {"content": "unused"},
        ],
        scripted_batches=[[observation("read", 0, []), observation("read", 1, [])]],
    ),
    _text_case(
        "stop_check_after_second_batch",
        stop_after_batch=2,
        scripted_replies=[
            {"content": "", "tool_calls": [tool_call("read", 0)]},
            {"content": "", "tool_calls": [tool_call("read", 1)]},
            {"content": "unused"},
        ],
        scripted_batches=[
            [observation("read", 0, [])],
            [observation("read", 1, [])],
        ],
    ),
]


def main() -> None:
    cases = []
    for spec in CASES:
        wire_replies, wire_batches = build_wire(spec)
        cases.append(
            {
                "name": spec["name"],
                "limits": spec.get("limits"),
                "clock": spec.get("clock", [0.0]),
                "cancel_at": spec.get("cancel_at"),
                "stop_after_batch": spec.get("stop_after_batch"),
                "reply_error_at": spec.get("reply_error_at"),
                "tool_error_at": spec.get("tool_error_at"),
                "replies": wire_replies,
                "batches": wire_batches,
                "seed_messages": SEED_MESSAGES,
                "expected": run_case(spec),
            }
        )
    fixture = {
        "source": "omnicrawl/agent/runtime/execution.py",
        "cases": cases,
    }
    FIXTURE_PATH.parent.mkdir(parents=True, exist_ok=True)
    FIXTURE_PATH.write_text(
        json.dumps(fixture, ensure_ascii=False, indent=2) + "\n", encoding="utf-8"
    )
    print(f"已写入 {FIXTURE_PATH.relative_to(ROOT)}，用例数 {len(cases)}")


if __name__ == "__main__":
    main()
