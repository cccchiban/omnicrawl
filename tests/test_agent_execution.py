from __future__ import annotations

import threading
import time
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch

import omnicrawl.agent as agent_package
from omnicrawl.agent.runtime.execution import (
    AgentLoopBudgetExceeded,
    AgentLoopLimits,
    AgentLoopObservation,
    AgentLoopRunner,
)
from omnicrawl.agent.core import LocalToolAgent
from omnicrawl.agent.toolkit.tools import public_tool_arguments
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
        with patch("omnicrawl.agent.runtime.execution.time.monotonic", _FakeClock(10.0, 12.1)):
            with self.assertRaisesRegex(AgentLoopBudgetExceeded, "时间预算"):
                AgentLoopRunner().run(
                    messages=[],
                    request_reply=lambda _messages: self.fail("超时后不应请求模型"),
                    execute_tool_batch=lambda _calls, _first_step: [],
                    limits=AgentLoopLimits(timeout_seconds=2.0),
                )

    def test_exact_duplicate_tool_calls_are_executed_each_time(self) -> None:
        agent = object.__new__(LocalToolAgent)
        agent.config = SimpleNamespace()
        executed = 0

        def run(_args: dict) -> ToolResult:
            nonlocal executed
            executed += 1
            return ToolResult(ok=True, output="command completed")

        agent._tools = {
            "bash": ToolDefinition("bash", "run", "{}", False, run),
        }
        agent._approve_tool_for_batch = lambda _tool, _arguments: None
        agent._append_session_event = lambda _event, _payload: None

        first = agent._execute_tool_batch(
            [ToolCall("bash", {"command": "printf ok"}, "call-1")],
            1,
            report_tool_start=lambda _step, _call: None,
            report_tool_result=lambda _call, _result: None,
            check_cancelled=lambda: None,
            status=lambda _message: None,
        )
        second = agent._execute_tool_batch(
            [ToolCall("bash", {"command": "printf ok"}, "call-2")],
            2,
            report_tool_start=lambda _step, _call: None,
            report_tool_result=lambda _call, _result: None,
            check_cancelled=lambda: None,
            status=lambda _message: None,
        )

        self.assertEqual(executed, 2)
        self.assertEqual(first[0].result.output, "command completed")
        self.assertEqual(second[0].result.output, "command completed")

    def test_local_agent_approves_complete_batch_before_any_execution(self) -> None:
        agent = object.__new__(LocalToolAgent)
        agent.config = SimpleNamespace()
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

    def test_tool_result_event_is_emitted_at_each_tool_completion(self) -> None:
        """慢工具不能把已完成的快工具结果和计时事件延后到批次末尾。"""

        agent = object.__new__(LocalToolAgent)
        agent.config = SimpleNamespace(tool_timeout_seconds=2)
        release_slow = threading.Event()
        fast_done = threading.Event()
        events: list[tuple[str, float, float | None]] = []
        events_lock = threading.Lock()

        def slow(_arguments: dict) -> ToolResult:
            release_slow.wait(timeout=2)
            return ToolResult(ok=True, output="slow")

        def fast(_arguments: dict) -> ToolResult:
            result = ToolResult(ok=True, output="fast")
            fast_done.set()
            return result

        agent._tools = {
            "slow_tool": ToolDefinition("slow_tool", "slow", "{}", False, slow),
            "fast_tool": ToolDefinition("fast_tool", "fast", "{}", False, fast),
        }
        agent._approve_tool_for_batch = lambda _tool, _arguments, **_kwargs: None
        agent._append_session_event = lambda _event, _payload: None
        agent._prepare_tool_result_for_model = (
            lambda _call, result, **_kwargs: (result, [])
        )
        agent._tool_result_message = lambda _call, result: {
            "role": "tool",
            "content": result.output,
        }

        def report_result(call: ToolCall, result: ToolResult) -> None:
            with events_lock:
                events.append((call.name, time.perf_counter(), result.completed_at))

        holder: list[list[AgentLoopObservation]] = []
        worker = threading.Thread(
            target=lambda: holder.append(
                agent._execute_tool_batch(
                    [
                        ToolCall("slow_tool", {}, "slow-call"),
                        ToolCall("fast_tool", {}, "fast-call"),
                    ],
                    1,
                    report_tool_start=lambda _step, _call: None,
                    report_tool_result=report_result,
                    check_cancelled=lambda: None,
                    status=lambda _message: None,
                    persist_session_events=False,
                )
            ),
            daemon=True,
        )
        worker.start()
        self.assertTrue(fast_done.wait(1), "快工具没有完成")
        # 此时慢工具仍未释放；快工具的完成事件必须已经可见。
        with events_lock:
            self.assertEqual([name for name, _seen, _completed in events], ["fast_tool"])
        release_slow.set()
        worker.join(timeout=2)
        self.assertFalse(worker.is_alive())
        self.assertEqual([item.tool_call.name for item in holder[0]], ["slow_tool", "fast_tool"])
        self.assertEqual([name for name, _seen, _completed in events], ["fast_tool", "slow_tool"])
        self.assertTrue(all(completed is not None for _name, _seen, completed in events))

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
        agent.config = SimpleNamespace()
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

    def test_tool_output_budget_formats_archived_preview(self) -> None:
        # 超限输出的模型可见文本：头尾预览 + 大小与落盘路径提示。
        output = "0123456789ABCDEFGHIJ" * 100
        visible = LocalToolAgent._format_archived_output_preview(
            output,
            "C:/session/artifacts/tool_result_x.txt",
        )
        self.assertIn("0123456789ABCDEFGHIJ", visible)
        self.assertIn("输出太大（2KB）", visible)
        self.assertIn("完整内容已保存到：C:/session/artifacts/tool_result_x.txt", visible)
        # 落盘失败时提示明确，避免模型误以为完整内容可读。
        visible_no_path = LocalToolAgent._format_archived_output_preview(output, "")
        self.assertIn("完整内容未能保存到磁盘", visible_no_path)

    def test_apply_batch_output_budget_archives_oversized_and_overflow(self) -> None:
        from omnicrawl.agent.core import (
            TOOL_OUTPUT_BATCH_BUDGET_CHARS,
            TOOL_OUTPUT_INLINE_LIMIT_CHARS,
        )

        agent = object.__new__(LocalToolAgent)
        agent.config = SimpleNamespace(max_history_turns=6)
        agent._history = []
        agent._pending_user_text = None
        agent._skill_manager = None
        agent._active_skills = []
        agent._session_store = None
        agent._session_state = None

        # 单工具超过 50K 直接落盘（无会话时退化为纯预览提示）。
        oversized = ToolResult(ok=True, output="x" * (TOOL_OUTPUT_INLINE_LIMIT_CHARS + 1))
        results = agent._apply_batch_output_budget([oversized])
        self.assertIn("完整内容未能保存到磁盘", results[0].output)
        self.assertNotEqual(results[0].output, oversized.output)

        # 聚合超预算：多个小输出合计超过 200K 时从最大者开始落盘。
        small = ToolResult(ok=True, output="y" * 40_000)
        many = [ToolResult(ok=True, output="z" * 60_000) for _ in range(6)]
        combined = agent._apply_batch_output_budget([small, *many])
        # 保留在上下文的原始输出（未落盘）总长必须回到 200K 预算以内。
        kept_sizes = [
            len(item.output)
            for item in combined
            if item.output == "y" * 40_000 or item.output == "z" * 60_000
        ]
        self.assertLessEqual(sum(kept_sizes), TOOL_OUTPUT_BATCH_BUDGET_CHARS)
        # 6×60K + 40K = 400K > 200K：从最大的 60K 输出开始落盘，直到预算达标。
        # 400K - 6×60K = 40K ≤ 200K，因此全部 6 个 60K 输出都被落盘。
        kept_z = sum(1 for item in combined if item.output == "z" * 60_000)
        self.assertEqual(kept_z, 0)
        # 被落盘的输出带预览与落盘提示。
        self.assertIn("完整内容未能保存到磁盘", combined[1].output)

        # 预算以内保持不变。
        within = agent._apply_batch_output_budget([small])
        self.assertEqual(within[0].output, small.output)

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
