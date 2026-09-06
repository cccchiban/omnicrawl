"""Run Guard 纯逻辑、协议边界与 Session 恢复回归测试。"""

from __future__ import annotations

import json
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch

from omnicrawl.agent.runtime.llm_protocol import (
    AgentLLMProtocol,
    AgentProtocolError,
)
from omnicrawl.agent.runtime.run_guard import (
    ConfiguredAutoRetryError,
    GuardRetryState,
    ReasoningGuardTriggered,
    create_reasoning_guard,
    configured_retry_code,
    mark_pause_requested,
    pause_requested,
    reset_pause_event,
    activate_pause_event,
)
from omnicrawl.agent.core import LocalToolAgent
from omnicrawl.agent.controllers.shared import AgentError
from omnicrawl.agent.types import AgentModelReply, ToolCall, ToolResult
from omnicrawl.config.features.run_guard import (
    ContinueConfig,
    ReasoningGuardConfig,
    RunGuardConfig,
    RunGuardConfigError,
    load_run_guard_config,
    save_run_guard_config,
)
from omnicrawl.state.session_models import SessionEvent
from omnicrawl.state.session_projection import recover_run_guard_state


class ReasoningGuardTests(unittest.TestCase):
    def _config(self, **overrides):
        values = {
            "substr_len": 8,
            "window_chars": 64,
            "repeat_ratio": 0.7,
            "check_every": 1,
            "max_blocks": 100,
            "max_chars": 10_000,
        }
        values.update(overrides)
        return SimpleNamespace(**values)

    def test_short_chunks_count_cross_boundary_substrings(self) -> None:
        guard = create_reasoning_guard(self._config(check_every=2))
        self.assertIsNone(guard.push("abc"))
        verdict = guard.push("defghijk")
        self.assertIsNotNone(verdict)
        self.assertGreater(guard.stats()["total_substrings"], 0)

    def test_window_shorter_than_substring_drops_all_frequency_state(self) -> None:
        guard = create_reasoning_guard(
            self._config(substr_len=8, window_chars=4)
        )
        guard.push("abcdefgh")
        guard.push("Z")
        self.assertEqual(guard.stats()["total_substrings"], 0)
        self.assertEqual(guard.stats()["unique"], 0)

    def test_empty_reasoning_block_is_checked(self) -> None:
        guard = create_reasoning_guard(self._config(max_blocks=2))
        guard.push("")
        verdict = guard.push("")
        self.assertIsNotNone(verdict)
        self.assertTrue(verdict.triggered)
        self.assertEqual(verdict.reason, "blocks")

    def test_guard_retry_state_is_shared_and_bounded(self) -> None:
        state = GuardRetryState(max_retries=2)
        self.assertEqual([state.consume(), state.consume(), state.consume()], [1, 2, None])


class RunGuardConfigTests(unittest.TestCase):
    def _tmp_config(self, body: str) -> str:
        directory = tempfile.mkdtemp(prefix="run_guard_cfg_")
        path = Path(directory) / "config.toml"
        path.write_text(body, encoding="utf-8")
        return str(path)

    def test_default_auto_retry_errors_use_unified_code(self) -> None:
        config = RunGuardConfig()
        self.assertEqual(config.guard.auto_retry_errors, ("SERVICE_UNAVAILABLE",))
        self.assertEqual(config.guard.max_guard_retries, 2)
        self.assertTrue(config.enabled)
        self.assertTrue(config.continuation.enabled)

    def test_load_and_save_roundtrip_preserves_other_sections(self) -> None:
        config_path = self._tmp_config(
            "other = 1\n"
            "[run_guard]\n"
            "enabled = true\n"
            "[run_guard.guard]\n"
            "auto_retry_errors = [\"SERVICE_UNAVAILABLE\", \"PI_AI_ERROR\"]\n"
            "[run_guard.continue]\n"
            "max_auto_followups = 5\n"
        )
        loaded = load_run_guard_config(config_path)
        self.assertEqual(
            loaded.guard.auto_retry_errors,
            ("SERVICE_UNAVAILABLE", "PI_AI_ERROR"),
        )
        self.assertEqual(loaded.continuation.max_auto_followups, 5)

        saved_path = save_run_guard_config(loaded, config_path)
        self.assertEqual(Path(saved_path).resolve(), Path(config_path).resolve())
        reloaded = load_run_guard_config(config_path)
        self.assertEqual(reloaded.guard.auto_retry_errors, ("SERVICE_UNAVAILABLE", "PI_AI_ERROR"))
        self.assertEqual(reloaded.continuation.max_auto_followups, 5)

    def test_unknown_field_is_rejected(self) -> None:
        config_path = self._tmp_config(
            "[run_guard.guard]\n"
            "unknown_field = 1\n"
        )
        with self.assertRaises(RunGuardConfigError):
            load_run_guard_config(config_path)

    def test_auto_retry_errors_dedupes_and_caps(self) -> None:
        config_path = self._tmp_config(
            "[run_guard.guard]\n"
            "auto_retry_errors = [\"A\", \"A\", \"B\"]\n"
        )
        loaded = load_run_guard_config(config_path)
        self.assertEqual(loaded.guard.auto_retry_errors, ("A", "B"))


class ErrorAndPauseTests(unittest.TestCase):
    def test_configured_retry_code_reads_exception_cause_and_text(self) -> None:
        config = SimpleNamespace(auto_retry_errors=("SERVICE_UNAVAILABLE",))
        cause = RuntimeError("upstream HTTP 503 SERVICE_UNAVAILABLE")
        exc = RuntimeError("wrapped")
        exc.__cause__ = cause
        self.assertEqual(configured_retry_code(exc, config), "SERVICE_UNAVAILABLE")

    def test_configured_retry_code_matches_vendor_business_code(self) -> None:
        # 白名单支持任意供应商业务错误码/文本（如网关 PI_AI_ERROR），
        # 不限于 ModelErrorCode 枚举成员。
        config = SimpleNamespace(auto_retry_errors=("PI_AI_ERROR",))
        cause = RuntimeError("gateway PI_AI_ERROR")
        exc = RuntimeError("wrapped")
        exc.__cause__ = cause
        self.assertEqual(configured_retry_code(exc, config), "PI_AI_ERROR")

    def test_pause_context_is_scoped(self) -> None:
        from threading import Event

        event = Event()
        token = activate_pause_event(event)
        try:
            self.assertTrue(mark_pause_requested())
            self.assertTrue(pause_requested())
        finally:
            reset_pause_event(token)
        self.assertFalse(pause_requested())
        self.assertFalse(mark_pause_requested())


class PauseBoundaryTests(unittest.TestCase):
    """run_stream 暂停 ContextVar 的激活/清理边界回归。

    使用最小 LocalToolAgent 对象（参照 tests/test_agent_execution.py 的
    run_stream 最小对象模板）：mock 掉 _request_agent_reply，不触碰真实
    Provider/Session。验证重点是暂停事件在回合结束（正常/异常/取消）后
    不遗留到下一回合，以及 pause_work 只停止自动路径、不影响用户新回合。
    """

    def _agent(self, replies=("完成",), *, run_guard_enabled: bool = True):
        from threading import Event as ThreadEvent

        agent = object.__new__(LocalToolAgent)
        agent.workspace_root = Path.cwd()
        if run_guard_enabled:
            run_guard = SimpleNamespace(
                enabled=True,
                guard=SimpleNamespace(
                    enabled=True,
                    max_guard_retries=2,
                    auto_retry_errors=("SERVICE_UNAVAILABLE",),
                ),
                continuation=SimpleNamespace(enabled=True, max_auto_followups=0),
            )
        else:
            run_guard = SimpleNamespace(enabled=False)
        agent.config = SimpleNamespace(
            llm=object(),
            max_history_turns=6,
            run_guard=run_guard,
            subagents=SimpleNamespace(enabled=False, allow_fork=False),
        )
        agent._history = []
        agent._pending_user_text = None
        agent._skill_manager = None
        agent._active_skills = []
        agent._tools = {}
        agent._session_store = None
        agent._session_state = None
        calls = iter(replies)

        def reply(*_args, **_kwargs):
            value = next(calls)
            if isinstance(value, BaseException):
                raise value
            return AgentModelReply(
                {"role": "assistant", "content": value},
                value,
            )

        agent._request_agent_reply = reply  # type: ignore[method-assign]
        return agent

    def _tool_call_reply(self, tool_name: str, arguments: dict) -> AgentModelReply:
        call = ToolCall(
            name=tool_name,
            arguments=dict(arguments),
            id="call-1",
            function_name=tool_name,
        )
        return AgentModelReply(
            {
                "role": "assistant",
                "content": "",
                "tool_calls": [
                    {"id": "call-1", "type": "function", "function": {"name": tool_name, "arguments": '{}'}}
                ],
            },
            "",
            tool_calls=[call],
        )

    def test_pause_context_cleared_after_normal_turn(self) -> None:
        agent = self._agent()
        self.assertFalse(pause_requested())
        result = agent.run_stream("完成任务", lambda _delta: None)
        self.assertEqual(result, "完成")
        self.assertFalse(pause_requested())
        self.assertFalse(mark_pause_requested())

    def test_pause_context_cleared_after_request_error(self) -> None:
        # 模型请求抛错发生在暂停事件激活之后、try 之内，必须由 finally 清理。
        agent = self._agent(replies=(AgentError("模型请求失败"),))
        with self.assertRaises(AgentError):
            agent.run_stream("完成任务", lambda _delta: None)
        self.assertFalse(pause_requested())
        self.assertFalse(mark_pause_requested())

    def test_pause_context_cleared_after_turn_start_rejected(self) -> None:
        # turn.start 拒绝发生在暂停激活之前的插件阶段；本就未激活，也不能遗留。
        agent = self._agent()
        agent._dispatch_plugin_hook = lambda *_args, **_kwargs: None  # type: ignore[method-assign]
        with self.assertRaises(AgentError):
            agent.run_stream("完成任务", lambda _delta: None)
        self.assertFalse(pause_requested())
        self.assertFalse(mark_pause_requested())

    def test_next_turn_does_not_inherit_previous_pause(self) -> None:
        # 连续两个正常回合：第二回合不能继承第一回合的暂停 ContextVar 值。
        agent = self._agent(replies=("第一轮", "第二轮"))
        first = agent.run_stream("任务一", lambda _delta: None)
        second = agent.run_stream("任务二", lambda _delta: None)
        self.assertEqual(first, "第一轮")
        self.assertEqual(second, "第二轮")
        self.assertFalse(pause_requested())

    def test_pause_work_stops_auto_path_and_keeps_user_new_turn_working(self) -> None:
        # pause_work 工具执行后：回合内的暂停 ContextVar 生效，工具批次后
        # stop_check 命中 paused，自动路径停止并记录 run_guard_paused 事件；
        # 回合结束清理暂停状态，用户紧接着发起的新回合仍能正常执行。
        from omnicrawl.agent.runtime.execution import AgentLoopObservation

        agent = self._agent()
        tool_call = ToolCall(
            name="pause_work",
            arguments={},
            id="call-1",
            function_name="pause_work",
        )
        events: list[tuple[str, dict]] = []
        real_append = agent._append_session_event
        agent._append_session_event = (  # type: ignore[method-assign]
            lambda event_type, payload: (
                events.append((event_type, payload)) or real_append(event_type, payload)
            )
        )

        def fake_execute_tool_batch(raw_tool_calls, first_step, **_kwargs):
            # 模拟 pause_work 工具执行：标记当前回合暂停。
            self.assertEqual(len(raw_tool_calls), 1)
            self.assertTrue(mark_pause_requested())
            result = SimpleNamespace(
                ok=True,
                output="已暂停",
                full_output="已暂停",
                ui_artifact={},
                model_images=(),
                completed_at=None,
                error_code=None,
                retryable=False,
            )
            return [
                AgentLoopObservation(
                    tool_call=tool_call,
                    result=result,
                    message={"role": "tool", "content": "已暂停"},
                )
            ]

        agent._execute_tool_batch = fake_execute_tool_batch  # type: ignore[method-assign]
        agent._request_agent_reply = (  # type: ignore[method-assign]
            lambda *_args, **_kwargs: self._tool_call_reply("pause_work", {})
        )

        result = agent.run_stream("执行任务", lambda _delta: None)
        self.assertEqual(result, "")
        self.assertFalse(pause_requested())
        self.assertFalse(mark_pause_requested())
        self.assertTrue(
            any(event_type == "run_guard_paused" for event_type, _payload in events)
        )
        # 用户新回合仍可正常执行（暂停不跨回合遗留）。
        agent2 = self._agent(replies=("新回合完成",))
        self.assertEqual(agent2.run_stream("新任务", lambda _delta: None), "新回合完成")

    def test_run_guard_disabled_never_activates_pause(self) -> None:
        agent = self._agent(run_guard_enabled=False)
        result = agent.run_stream("完成任务", lambda _delta: None)
        self.assertEqual(result, "完成")
        self.assertFalse(pause_requested())
        self.assertFalse(mark_pause_requested())


class ContinueStateMachineTests(unittest.TestCase):
    """run_stream 的 Continue/pause 状态机行为回归。

    使用与 PauseBoundaryTests 相同的最小 LocalToolAgent 模板：mock
    _request_agent_reply 按序列返回不同回复，验证 Continue 触发/聚合/
    上限/暂停组合的终态与事件。
    """

    def _agent(self, replies, *, max_followups=3, run_guard_enabled=True):
        from threading import Event as ThreadEvent

        agent = object.__new__(LocalToolAgent)
        agent.workspace_root = Path.cwd()
        if run_guard_enabled:
            run_guard = SimpleNamespace(
                enabled=True,
                guard=SimpleNamespace(
                    enabled=True,
                    max_guard_retries=2,
                    auto_retry_errors=("SERVICE_UNAVAILABLE",),
                ),
                continuation=SimpleNamespace(
                    enabled=True,
                    max_auto_followups=max_followups,
                ),
            )
        else:
            run_guard = SimpleNamespace(enabled=False)
        agent.config = SimpleNamespace(
            llm=object(),
            max_history_turns=6,
            run_guard=run_guard,
            subagents=SimpleNamespace(enabled=False, allow_fork=False),
        )
        agent._history = []
        agent._pending_user_text = None
        agent._skill_manager = None
        agent._active_skills = []
        agent._tools = {}
        agent._session_store = None
        agent._session_state = None
        calls = iter(replies)

        def reply(*_args, **_kwargs):
            value = next(calls)
            if isinstance(value, BaseException):
                raise value
            return value

        agent._request_agent_reply = reply  # type: ignore[method-assign]
        return agent

    @staticmethod
    def _reasoning_only(text: str = "思考中") -> AgentModelReply:
        return AgentModelReply(
            {"role": "assistant", "content": "", "reasoning_content": text},
            "",
            reasoning=text,
        )

    @staticmethod
    def _text_reply(text: str, *, streamed: bool = False) -> AgentModelReply:
        return AgentModelReply(
            {"role": "assistant", "content": text},
            text,
            content_streamed=streamed,
        )

    def test_reasoning_only_auto_continues_and_aggregates(self) -> None:
        # 首回合只有 reasoning，自动续跑；续跑产出真实文本后终止。
        # 最终回复聚合首回合 reasoning（不可见）与最终文本。
        agent = self._agent(
            [
                self._reasoning_only("第一轮推理"),
                self._text_reply("最终答案"),
            ],
            max_followups=3,
        )
        result = agent.run_stream("完成任务", lambda _delta: None)
        self.assertEqual(result, "最终答案")
        self.assertFalse(pause_requested())

    def test_reasoning_only_reaches_limit_and_records_exhausted(self) -> None:
        # 连续 reasoning-only 且达到 max_auto_followups 上限：写入
        # run_guard_continue_exhausted 事件，保留 pending_user_text 供
        # 用户显式“继续”。
        events: list[tuple[str, dict]] = []
        agent = self._agent(
            [
                self._reasoning_only("推理A"),
                self._reasoning_only("推理B"),
                self._reasoning_only("推理C"),
            ],
            max_followups=2,
        )
        real_append = agent._append_session_event
        agent._append_session_event = (  # type: ignore[method-assign]
            lambda event_type, payload: (
                events.append((event_type, payload)) or real_append(event_type, payload)
            )
        )
        result = agent.run_stream("完成任务", lambda _delta: None)
        self.assertEqual(result, "")
        exhausted = [
            p for t, p in events if t == "run_guard_continue_exhausted"
        ]
        self.assertEqual(len(exhausted), 1)
        self.assertEqual(exhausted[0]["pending_user_text"], "完成任务")
        self.assertEqual(exhausted[0]["followups"], 2)
        # 回合结束清理暂停状态，且待续文本保留供下一回合“继续”。
        self.assertEqual(agent._pending_user_text, "完成任务")
        self.assertFalse(pause_requested())

    def test_todo_incomplete_triggers_continue_and_respects_limit(self) -> None:
        # 用户“继续”且上一回合有未完成 Todo：自动续跑；由于 mock 回复不会
        # 真正更新 Todo，续跑持续到 max_auto_followups 上限并写入 exhausted。
        events: list[tuple[str, dict]] = []
        agent = self._agent(
            [
                self._text_reply("完成第一个 Todo"),
                self._text_reply("完成第二个 Todo"),
                self._text_reply("完成第三个 Todo"),
            ],
            max_followups=2,
        )
        agent._active_todo_items = [
            {"id": "1", "step": "任务一", "completed": False},
            {"id": "2", "step": "任务二", "completed": True},
        ]
        agent._pending_user_text = "上一任务"
        agent._todo_update_callback = lambda _payload: None
        real_append = agent._append_session_event
        agent._append_session_event = (  # type: ignore[method-assign]
            lambda event_type, payload: (
                events.append((event_type, payload)) or real_append(event_type, payload)
            )
        )
        result = agent.run_stream("继续", lambda _delta: None)
        # 三次回复全部聚合输出（两轮 Continue）。
        self.assertEqual(result, "完成第一个 Todo\n\n完成第二个 Todo\n\n完成第三个 Todo")
        continue_events = [p for t, p in events if t == "run_guard_continue"]
        self.assertEqual(len(continue_events), 2)
        exhausted = [p for t, p in events if t == "run_guard_continue_exhausted"]
        self.assertEqual(len(exhausted), 1)
        self.assertEqual(exhausted[0]["followups"], 2)
        self.assertFalse(pause_requested())

    def test_todo_all_completed_does_not_continue(self) -> None:
        agent = self._agent(
            [self._text_reply("任务已全部完成")],
            max_followups=3,
        )
        agent._active_todo_items = [
            {"id": "1", "step": "任务一", "completed": True},
        ]
        agent._pending_user_text = "上一任务"
        events: list[tuple[str, dict]] = []
        real_append = agent._append_session_event
        agent._append_session_event = (  # type: ignore[method-assign]
            lambda event_type, payload: (
                events.append((event_type, payload)) or real_append(event_type, payload)
            )
        )
        result = agent.run_stream("继续", lambda _delta: None)
        self.assertEqual(result, "任务已全部完成")
        self.assertFalse(
            any(t == "run_guard_continue" for t, _p in events)
        )

    def test_streamed_reasoning_then_plain_text_continue_aggregates_once(self) -> None:
        # 首回合为 reasoning-only（无可见文本）触发 Continue；续跑是非流式
        # 文本。reasoning 流式不会把 content_streamed 置真（它只表示可见
        # content 增量已输出），因此最终聚合文本通过 on_delta 输出一次。
        deltas: list[str] = []
        agent = self._agent(
            [
                self._reasoning_only("推理"),
                self._text_reply("续跑最终答案", streamed=False),
            ],
            max_followups=3,
        )
        result = agent.run_stream("完成任务", deltas.append)
        self.assertEqual(result, "续跑最终答案")
        self.assertEqual(deltas, ["续跑最终答案"])
        self.assertFalse(pause_requested())

    def test_plain_text_turn_does_not_trigger_continue(self) -> None:
        # 纯文本回复（有 content、无 reasoning、无 tool_calls）终止自动路径：
        # 不会触发 Continue，只输出一个文本。
        events: list[tuple[str, dict]] = []
        agent = self._agent(
            [self._text_reply("直接答案")],
            max_followups=3,
        )
        real_append = agent._append_session_event
        agent._append_session_event = (  # type: ignore[method-assign]
            lambda event_type, payload: (
                events.append((event_type, payload)) or real_append(event_type, payload)
            )
        )
        result = agent.run_stream("完成任务", lambda _delta: None)
        self.assertEqual(result, "直接答案")
        self.assertFalse(
            any(t == "run_guard_continue" for t, _p in events)
        )

    def test_cancel_or_provider_error_does_not_trigger_continue(self) -> None:
        # 首回合只有 reasoning 触发一次自动续跑（run_guard_continue 在发起
        # 请求前写入）；但续跑请求抛出 Provider 错误：回合中断，不会写入
        # run_guard_continue_exhausted（错误中断而非达到上限），也不会继续
        # 下一次续跑；暂停 ContextVar 仍被清理。
        events: list[tuple[str, dict]] = []
        agent = self._agent(
            [
                self._reasoning_only("推理中"),
                AgentError("Provider 临时错误"),
            ],
            max_followups=3,
        )
        real_append = agent._append_session_event
        agent._append_session_event = (  # type: ignore[method-assign]
            lambda event_type, payload: (
                events.append((event_type, payload)) or real_append(event_type, payload)
            )
        )
        with self.assertRaises(AgentError):
            agent.run_stream("完成任务", lambda _delta: None)
        # 错误中断：不写入 exhausted（未达到上限），也只在错误前尝试过一次续跑。
        self.assertFalse(
            any(t == "run_guard_continue_exhausted" for t, _p in events)
        )
        continue_events = [p for t, p in events if t == "run_guard_continue"]
        self.assertLessEqual(len(continue_events), 1)
        self.assertFalse(pause_requested())

    def test_pause_work_blocks_continue_but_not_next_user_turn(self) -> None:
        # pause_work 标记暂停后：即使存在未完成 Todo 或 reasoning-only，
        # Continue 也不会触发；回合结束清理暂停，下一用户回合正常。
        from omnicrawl.agent.runtime.execution import AgentLoopObservation

        agent = self._agent([self._text_reply("部分完成")], max_followups=3)
        agent._active_todo_items = [
            {"id": "1", "step": "任务一", "completed": False},
        ]
        agent._pending_user_text = "上一任务"
        agent._todo_update_callback = lambda _payload: None
        events: list[tuple[str, dict]] = []
        real_append = agent._append_session_event
        agent._append_session_event = (  # type: ignore[method-assign]
            lambda event_type, payload: (
                events.append((event_type, payload)) or real_append(event_type, payload)
            )
        )
        # 让第一次模型请求返回 pause_work 工具调用，工具执行时标记暂停。
        tool_call = ToolCall(
            name="pause_work",
            arguments={},
            id="call-1",
            function_name="pause_work",
        )
        agent._request_agent_reply = (  # type: ignore[method-assign]
            lambda *_args, **_kwargs: AgentModelReply(
                {
                    "role": "assistant",
                    "content": "",
                    "tool_calls": [
                        {"id": "call-1", "type": "function", "function": {"name": "pause_work", "arguments": "{}"}}
                    ],
                },
                "",
                tool_calls=[tool_call],
            )
        )

        def fake_execute_tool_batch(raw_tool_calls, first_step, **_kwargs):
            self.assertTrue(mark_pause_requested())
            return [
                AgentLoopObservation(
                    tool_call=tool_call,
                    result=SimpleNamespace(
                        ok=True,
                        output="已暂停",
                        full_output="已暂停",
                        ui_artifact={},
                        model_images=(),
                        completed_at=None,
                        error_code=None,
                        retryable=False,
                    ),
                    message={"role": "tool", "content": "已暂停"},
                )
            ]

        agent._execute_tool_batch = fake_execute_tool_batch  # type: ignore[method-assign]

        result = agent.run_stream("继续", lambda _delta: None)
        self.assertEqual(result, "")
        self.assertFalse(pause_requested())
        # 暂停后未触发 Continue。
        self.assertFalse(
            any(t == "run_guard_continue" for t, _p in events)
        )
        # 下一用户回合正常执行。
        agent2 = self._agent([self._text_reply("新回合完成")], max_followups=3)
        self.assertEqual(agent2.run_stream("新任务", lambda _d: None), "新回合完成")


class PauseToolRegistrationTests(unittest.TestCase):
    """pause_work 在工具表中的条件注册边界。"""

    def _common(self):
        from omnicrawl.agent.toolkit.tools import build_agent_tools

        runner = lambda _arguments: ToolResult(ok=True, output="ok")
        manager = SimpleNamespace(
            registry=SimpleNamespace(tools={}, resources={}, prompts={})
        )
        return build_agent_tools, {
            "mcp_manager": manager,
            "memory_enabled": False,
            "list": runner,
            "read": runner,
            "grep": runner,
            "edit_file": runner,
            "write_file": runner,
            "bash": runner,
            "powershell": runner,
            "monitor": runner,
            "memory_search": runner,
            "memory_read": runner,
            "memory_expand_related": runner,
            "memory_write": runner,
            "mcp_call": lambda _meta, _arguments: ToolResult(ok=True, output="ok"),
            "mcp_read_resource": lambda _uri: ToolResult(ok=True, output="ok"),
            "mcp_get_prompt": lambda _name, _arguments: ToolResult(ok=True, output="ok"),
        }

    def test_pause_work_registered_when_enabled(self) -> None:
        from omnicrawl.agent.toolkit.tools import PAUSE_WORK_TOOL_NAME

        build_agent_tools, common = self._common()
        tools = build_agent_tools(**common, pause_work=lambda _args: ToolResult(ok=True, output="ok"))
        self.assertIn(PAUSE_WORK_TOOL_NAME, tools)
        self.assertFalse(tools[PAUSE_WORK_TOOL_NAME].requires_confirmation)

    def test_pause_work_omitted_when_disabled(self) -> None:
        from omnicrawl.agent.toolkit.tools import PAUSE_WORK_TOOL_NAME

        build_agent_tools, common = self._common()
        tools = build_agent_tools(**common, pause_work=None)
        self.assertNotIn(PAUSE_WORK_TOOL_NAME, tools)


class ProtocolGuardTests(unittest.TestCase):
    def _protocol(self, replies, *, state=None, config=None):
        calls = iter(replies)

        class Protocol(AgentLLMProtocol):
            def request_reply_once(inner, *args, **kwargs):
                reply = next(calls)
                if isinstance(reply, BaseException):
                    raise reply
                return reply

        return Protocol(
            client=None,
            model="demo",
            request_timeout_seconds=30,
            request_retry_count=1,
            workspace_root=Path("."),
            system_prompt_provider=lambda: "sys",
            prompt_cache_identity_provider=lambda: {},
            tools_provider=lambda: [],
            extra_body_provider=lambda: {},
            tool_name_from_function_name=lambda name: name,
            function_name_for_tool=lambda name: name,
            reasoning_guard_config=config,
            guard_retry_state=state,
        )

    @staticmethod
    def _reply(text: str) -> AgentModelReply:
        return AgentModelReply(
            {"role": "assistant", "content": text},
            text,
        )

    def test_reasoning_guard_retry_uses_injected_shared_budget(self) -> None:
        state = GuardRetryState(max_retries=1)
        protocol = self._protocol(
            [ReasoningGuardTriggered(
                SimpleNamespace(blocks=1, chars=1, reason="repeat", ratio=1.0),
                "guard",
            ), self._reply("ok")],
            state=state,
        )
        statuses: list[str] = []
        result = protocol.request_reply(
            [], lambda _text: None, lambda *_usage: None, lambda: None,
            statuses.append,
        )
        self.assertEqual(result.content, "ok")
        self.assertEqual(state.used, 1)
        self.assertEqual(len(statuses), 1)

    def test_configured_error_retry_uses_same_budget(self) -> None:
        state = GuardRetryState(max_retries=1)
        protocol = self._protocol(
            [ConfiguredAutoRetryError("PI_AI_ERROR", "temporary"), self._reply("ok")],
            state=state,
        )
        result = protocol.request_reply(
            [], lambda _text: None, lambda *_usage: None, lambda: None,
            lambda _status: None,
        )
        self.assertEqual(result.content, "ok")
        self.assertEqual(state.used, 1)

    def test_guard_retry_stops_when_pause_is_requested(self) -> None:
        from threading import Event

        token = activate_pause_event(Event())
        try:
            mark_pause_requested()
            protocol = self._protocol(
                [ConfiguredAutoRetryError("PI_AI_ERROR", "temporary")],
                state=GuardRetryState(max_retries=2),
            )
            with self.assertRaises(AgentProtocolError):
                protocol.request_reply(
                    [], lambda _text: None, lambda *_usage: None, lambda: None,
                    lambda _status: None,
                )
        finally:
            reset_pause_event(token)


class SessionRecoveryTests(unittest.TestCase):
    SESSION_ID = "20260831-120000-a1b2c3"

    def _event(self, event_type: str, payload: dict) -> SessionEvent:
        return SessionEvent.create(
            session_id=self.SESSION_ID,
            event_type=event_type,
            payload=payload,
        )

    def test_paused_todo_and_pending_text_are_recovered(self) -> None:
        events = [
            self._event("user_message", {"content": "完成任务", "pending_user_text": "完成任务"}),
            self._event(
                "tool_call_requested",
                {
                    "tool": "update_todos",
                    "arguments": {"todos": [{"id": "1", "step": "写测试", "completed": False}]},
                },
            ),
            self._event(
                "run_guard_paused",
                {"pending_user_text": "完成任务"},
            ),
        ]
        pending, todos = recover_run_guard_state(events)
        self.assertEqual(pending, "完成任务")
        self.assertEqual(todos[0]["step"], "写测试")

    def test_normal_assistant_reply_clears_pending_without_using_reply_text(self) -> None:
        events = [
            self._event("user_message", {"content": "原任务", "pending_user_text": "原任务"}),
            self._event("assistant_message", {"content": "已完成"}),
        ]
        pending, todos = recover_run_guard_state(events)
        self.assertEqual(pending, "")
        self.assertEqual(todos, ())

    def test_tool_result_json_updates_todo_projection(self) -> None:
        events = [
            self._event(
                "tool_result",
                {
                    "tool": "update_todos",
                    "model_output": json.dumps(
                        {"updated": 1, "todos": [{"step": "完成", "status": "done"}]},
                        ensure_ascii=False,
                    ),
                },
            )
        ]
        _pending, todos = recover_run_guard_state(events)
        self.assertEqual(todos, ({"id": "1", "step": "完成", "completed": True},))


if __name__ == "__main__":
    unittest.main()
