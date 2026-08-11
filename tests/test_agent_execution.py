from __future__ import annotations

import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch

import omnicrawl.agent as agent_package
from omnicrawl.agent.execution import (
    AgentLoopBudgetExceeded,
    AgentLoopLimits,
    AgentLoopObservation,
    AgentLoopRunner,
)
from omnicrawl.agent.core import LocalToolAgent
from omnicrawl.agent.tools import public_tool_arguments
from omnicrawl.agent.types import AgentModelReply, ToolCall, ToolDefinition, ToolResult


class _FakeClock:
    def __init__(self, *values: float) -> None:
        self._values = iter(values)

    def __call__(self) -> float:
        return next(self._values)


class AgentLoopRunnerTest(unittest.TestCase):
    def test_runner_returns_final_reply_and_combined_reasoning(self) -> None:
        replies = iter(
            [
                AgentModelReply(
                    message={"role": "assistant", "content": "完成"},
                    content="完成",
                    reasoning="先检查边界",
                )
            ]
        )
        messages = [{"role": "user", "content": "检查项目"}]

        result = AgentLoopRunner().run(
            messages=messages,
            request_reply=lambda _messages: next(replies),
            execute_tool_batch=lambda _calls, _first_step: self.fail("不应执行工具"),
        )

        self.assertEqual(result.final_text, "完成")
        self.assertEqual(result.reasoning, "先检查边界")
        self.assertEqual(result.model_turns, 1)
        self.assertEqual(result.tool_calls, 0)
        self.assertFalse(result.content_streamed)
        self.assertEqual(result.messages, messages)

    def test_runner_hands_each_reply_to_one_batch_callback_and_preserves_observation_order(self) -> None:
        first = ToolCall("read", {"path": "a.py"}, "call_1")
        second = ToolCall("grep", {"text": "Agent"}, "call_2")
        replies = iter(
            [
                AgentModelReply(
                    message={"role": "assistant", "content": None, "tool_calls": []},
                    content="",
                    tool_calls=[first, second],
                    reasoning="查找证据",
                ),
                AgentModelReply(
                    message={"role": "assistant", "content": "已完成"},
                    content="已完成",
                    reasoning="形成结论",
                    content_streamed=True,
                ),
            ]
        )
        batch_calls: list[tuple[list[ToolCall], int]] = []

        def execute_batch(calls, first_step):
            batch_calls.append((list(calls), first_step))
            return [
                AgentLoopObservation(
                    tool_call=second,
                    result=ToolResult(ok=True, output="second"),
                    message={"role": "tool", "tool_call_id": "call_2", "content": "second"},
                ),
                AgentLoopObservation(
                    tool_call=first,
                    result=ToolResult(ok=True, output="first"),
                    message={"role": "tool", "tool_call_id": "call_1", "content": "first"},
                ),
            ]

        result = AgentLoopRunner().run(
            messages=[{"role": "user", "content": "检查"}],
            request_reply=lambda _messages: next(replies),
            execute_tool_batch=execute_batch,
        )

        self.assertEqual(batch_calls, [([first, second], 1)])
        self.assertEqual(
            [message.get("tool_call_id") for message in result.messages if message["role"] == "tool"],
            ["call_2", "call_1"],
        )
        self.assertEqual(result.reasoning, "查找证据\n形成结论")
        self.assertEqual(result.model_turns, 2)
        self.assertEqual(result.tool_calls, 2)
        self.assertTrue(result.content_streamed)

    def test_runner_checks_cancellation_before_request_and_after_batch(self) -> None:
        call = ToolCall("read", {}, "call_1")
        replies = iter(
            [
                AgentModelReply(
                    message={"role": "assistant", "content": None},
                    content="",
                    tool_calls=[call],
                ),
                AgentModelReply({"role": "assistant", "content": "完成"}, "完成"),
            ]
        )
        checks = 0

        def cancel_check() -> None:
            nonlocal checks
            checks += 1

        AgentLoopRunner().run(
            messages=[],
            request_reply=lambda _messages: next(replies),
            execute_tool_batch=lambda calls, _first_step: [
                AgentLoopObservation(
                    tool_call=calls[0],
                    result=ToolResult(ok=True, output="ok"),
                    message={"role": "tool", "tool_call_id": "call_1", "content": "ok"},
                )
            ],
            cancel_check=cancel_check,
        )

        self.assertGreaterEqual(checks, 3)

    def test_runner_rejects_model_turn_and_tool_call_budget_overruns(self) -> None:
        tool_call = ToolCall("read", {}, "call_1")
        runner = AgentLoopRunner()

        with self.assertRaisesRegex(AgentLoopBudgetExceeded, "模型回合预算"):
            runner.run(
                messages=[],
                request_reply=lambda _messages: AgentModelReply(
                    {"role": "assistant", "content": None},
                    "",
                    [tool_call],
                ),
                execute_tool_batch=lambda calls, _first_step: [
                    AgentLoopObservation(
                        tool_call=calls[0],
                        result=ToolResult(ok=True, output="ok"),
                        message={"role": "tool", "tool_call_id": "call_1", "content": "ok"},
                    )
                ],
                limits=AgentLoopLimits(max_model_turns=1),
            )

        with self.assertRaisesRegex(AgentLoopBudgetExceeded, "工具调用预算"):
            runner.run(
                messages=[],
                request_reply=lambda _messages: AgentModelReply(
                    {"role": "assistant", "content": None},
                    "",
                    [tool_call, ToolCall("grep", {}, "call_2")],
                ),
                execute_tool_batch=lambda _calls, _first_step: self.fail("超预算批次不应执行"),
                limits=AgentLoopLimits(max_tool_calls=1),
            )

    def test_runner_enforces_timeout_with_monotonic_clock(self) -> None:
        with patch("omnicrawl.agent.execution.time.monotonic", _FakeClock(10.0, 12.1)):
            with self.assertRaisesRegex(AgentLoopBudgetExceeded, "时间预算"):
                AgentLoopRunner().run(
                    messages=[],
                    request_reply=lambda _messages: self.fail("超时后不应请求模型"),
                    execute_tool_batch=lambda _calls, _first_step: [],
                    limits=AgentLoopLimits(timeout_seconds=2.0),
                )

    def test_local_agent_approves_complete_batch_before_any_execution(self) -> None:
        agent = object.__new__(LocalToolAgent)
        agent.config = SimpleNamespace(max_tool_output_chars=6000)
        agent._tools = {
            "read": ToolDefinition("read", "read", "{}", False, lambda _args: run("read")),
            "grep": ToolDefinition("grep", "search", "{}", False, lambda _args: run("search")),
        }
        events: list[str] = []

        def approve(tool, _arguments):
            events.append(f"approve:{tool.name}")
            return None

        def run(name: str) -> ToolResult:
            events.append(f"execute:{name}")
            return ToolResult(ok=True, output=name)

        agent._approve_tool_for_batch = approve  # type: ignore[method-assign]
        agent._append_session_event = lambda _event, _payload: None  # type: ignore[method-assign]
        agent._execute_tool_batch(
            [ToolCall("read", {}, "call_1"), ToolCall("grep", {}, "call_2")],
            1,
            report_tool_start=lambda _step, _call: None,
            report_tool_result=lambda _call, _result: None,
            check_cancelled=lambda: None,
            status=lambda _message: None,
        )

        self.assertEqual(events[:2], ["approve:read", "approve:grep"])
        self.assertCountEqual(events[2:], ["execute:read", "execute:search"])

    def test_subagent_public_arguments_projection_is_idempotent(self) -> None:
        raw = {
            "action": "run",
            "tasks": [
                {
                    "description": "检查 Session",
                    "prompt": "完整 prompt",
                    "subagent_type": "explore",
                }
            ],
            "max_concurrency": 2,
        }

        first = public_tool_arguments("subagent", raw)
        second = public_tool_arguments("subagent", first)

        self.assertEqual(second, first)

    def test_subagent_tool_session_event_does_not_persist_complete_prompt(self) -> None:
        agent = object.__new__(LocalToolAgent)
        agent.config = SimpleNamespace(max_tool_output_chars=6000)
        agent._tools = {
            "subagent": ToolDefinition(
                "subagent",
                "delegate",
                "{}",
                False,
                lambda _args: ToolResult(ok=True, output="ok"),
            )
        }
        events = []
        agent._approve_tool_for_batch = lambda _tool, _arguments: None
        agent._append_session_event = lambda event, payload: events.append((event, payload))

        agent._execute_tool_batch(
            [
                ToolCall(
                    "subagent",
                    {
                        "action": "run",
                        "tasks": [
                            {
                                "description": "检查 Session",
                                "prompt": "完整子任务 prompt token=should-not-persist",
                                "subagent_type": "explore",
                            }
                        ],
                        "max_concurrency": 2,
                    },
                    "call-subagent",
                )
            ],
            1,
            report_tool_start=lambda _step, _call: None,
            report_tool_result=lambda _call, _result: None,
            check_cancelled=lambda: None,
            status=lambda _message: None,
        )

        requested = next(payload for event, payload in events if event == "tool_call_requested")
        self.assertEqual(requested["arguments"]["task_count"], 1)
        self.assertNotIn("tasks", requested["arguments"])
        self.assertNotIn("should-not-persist", str(requested))

    def test_tool_output_truncation_preserves_head_and_tail(self) -> None:
        agent = object.__new__(LocalToolAgent)
        agent.config = SimpleNamespace(max_tool_output_chars=10)

        output = LocalToolAgent._truncate_tool_output(agent, "0123456789ABCDEFGHIJ")

        self.assertIn("01234", output)
        self.assertIn("FGHIJ", output)
        self.assertIn("... 工具输出已截断。", output)

    def test_local_agent_acquires_and_releases_one_runtime_snapshot_per_turn(self) -> None:
        agent = object.__new__(LocalToolAgent)
        agent.workspace_root = Path.cwd()
        agent.config = SimpleNamespace(llm=object(), max_history_turns=6)
        agent._history = []
        agent._pending_user_text = None
        agent._skill_manager = None
        agent._active_skills = []
        agent._tools = {}
        agent._session_store = None
        agent._session_state = None
        snapshot = object()

        class FakeRuntimeManager:
            def __init__(self) -> None:
                self.acquired = 0
                self.released: list[object] = []

            def acquire_turn(self):
                self.acquired += 1
                return snapshot

            def release_turn(self, value) -> None:
                self.released.append(value)

        manager = FakeRuntimeManager()
        agent._ensure_runtime_manager = lambda: manager  # type: ignore[method-assign]
        agent._request_agent_reply = lambda *_args, **_kwargs: AgentModelReply(  # type: ignore[method-assign]
            {"role": "assistant", "content": "完成"},
            "完成",
        )

        result = agent.run_stream("检查 Runtime", lambda _delta: None)

        self.assertEqual(result, "完成")
        self.assertEqual(manager.acquired, 1)
        self.assertEqual(manager.released, [snapshot])
        self.assertNotIn("_active_runtime_snapshot", agent.__dict__)

    def test_runner_is_not_part_of_stable_agent_exports(self) -> None:
        self.assertNotIn("AgentLoopRunner", agent_package.__all__)
        self.assertFalse(hasattr(agent_package, "AgentLoopRunner"))


if __name__ == "__main__":
    unittest.main()
