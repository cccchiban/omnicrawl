from __future__ import annotations

import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch

from ai_voice_agent.agent import (
    AgentConfig,
    AgentError,
    LocalToolAgent,
    ToolDefinition,
    ToolResult,
    _AgentReplyStreamer,
)
from ai_voice_agent.temp_workspace import AgentTempWorkspaceConfig


class AgentContextInjectionTest(unittest.TestCase):
    def test_agent_initialization_runs_due_temp_cleanup_before_llm_setup(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            calls: list[str] = []

            class FakeTempWorkspace:
                display_path = ".agent_tmp"

                def __init__(self, *_args, **_kwargs) -> None:
                    calls.append("init")

                def ensure(self) -> None:
                    calls.append("ensure")

                def clean_if_due(self) -> None:
                    calls.append("clean_if_due")

            config = AgentConfig(
                llm=SimpleNamespace(api_key=""),
                workspace_root=Path(temp_dir),
                memory_enabled=False,
                temp_workspace=AgentTempWorkspaceConfig(),
            )

            with patch("ai_voice_agent.agent.AgentTempWorkspace", FakeTempWorkspace):
                with self.assertRaisesRegex(AgentError, "缺少 API Key"):
                    LocalToolAgent(config)

        self.assertEqual(calls, ["init", "ensure", "clean_if_due"])

    def test_agents_md_is_first_context_message(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            workspace = Path(temp_dir)
            (workspace / "AGENTS.md").write_text(
                "# AGENTS.md\n\n必须先理解再执行。",
                encoding="utf-8",
            )
            agent = object.__new__(LocalToolAgent)
            agent.workspace_root = workspace

            messages = LocalToolAgent._project_instructions_messages(agent)

        self.assertEqual(len(messages), 1)
        self.assertEqual(messages[0]["role"], "user")
        self.assertTrue(messages[0]["content"].startswith("<project_instructions file=\"AGENTS.md\">"))
        self.assertIn("# AGENTS.md", messages[0]["content"])
        self.assertIn("必须先理解再执行。", messages[0]["content"])
        self.assertTrue(messages[0]["content"].rstrip().endswith("</project_instructions>"))

    def test_run_stream_sends_agents_md_before_history_and_current_user(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            workspace = Path(temp_dir)
            (workspace / "AGENTS.md").write_text(
                "# AGENTS.md\n\n进度实时可见。",
                encoding="utf-8",
            )
            agent = object.__new__(LocalToolAgent)
            agent.workspace_root = workspace
            agent.config = SimpleNamespace(max_history_turns=6)
            agent._history = [
                {"role": "user", "content": "上一轮问题"},
                {"role": "assistant", "content": "上一轮回答"},
            ]
            agent._skill_manager = None
            agent._active_skills = []
            agent._tools = {}
            captured_messages: list[list[dict[str, str]]] = []

            def fake_request(
                messages,
                _on_delta,
                _on_token_usage,
                _on_protocol_wait,
                _on_retry_status,
            ):
                captured_messages.append(messages)
                return "<final>完成</final>", "", False

            agent._request_agent_reply = fake_request  # type: ignore[method-assign]
            deltas: list[str] = []

            result = LocalToolAgent.run_stream(
                agent,
                "请检查项目状态",
                deltas.append,
            )

        self.assertEqual(result, "完成")
        self.assertEqual(deltas, ["完成"])
        self.assertEqual(len(captured_messages), 1)
        sent_messages = captured_messages[0]
        self.assertIn("<project_instructions file=\"AGENTS.md\">", sent_messages[0]["content"])
        self.assertIn("进度实时可见。", sent_messages[0]["content"])
        self.assertEqual(sent_messages[1]["content"], "上一轮问题")
        self.assertEqual(sent_messages[2]["content"], "上一轮回答")
        self.assertEqual(sent_messages[-1]["content"], "请检查项目状态")
        self.assertEqual(agent._history[-2]["content"], "请检查项目状态")
        self.assertNotIn("project_instructions", agent._history[-2]["content"])

    def test_missing_agents_md_adds_no_project_context_message(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            agent = object.__new__(LocalToolAgent)
            agent.workspace_root = Path(temp_dir)

            messages = LocalToolAgent._project_instructions_messages(agent)

        self.assertEqual(messages, [])

    def test_failed_request_keeps_pending_user_task_for_continue(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            agent = object.__new__(LocalToolAgent)
            agent.workspace_root = Path(temp_dir)
            agent.config = SimpleNamespace(max_history_turns=6)
            agent._history = []
            agent._skill_manager = None
            agent._active_skills = []
            agent._tools = {}
            agent._pending_user_text = None

            def fake_request(
                _messages,
                _on_delta,
                _on_token_usage,
                _on_protocol_wait,
                _on_retry_status,
            ):
                raise AgentError("Agent 请求失败：无法连接模型服务。")

            agent._request_agent_reply = fake_request  # type: ignore[method-assign]

            with self.assertRaisesRegex(AgentError, "无法连接模型服务"):
                LocalToolAgent.run_stream(agent, "检查项目并修复启动失败", lambda _delta: None)

        self.assertEqual(agent._pending_user_text, "检查项目并修复启动失败")
        self.assertEqual(agent._history, [])

    def test_continue_after_failed_request_reuses_pending_user_task(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            agent = object.__new__(LocalToolAgent)
            agent.workspace_root = Path(temp_dir)
            agent.config = SimpleNamespace(max_history_turns=6)
            agent._history = []
            agent._skill_manager = None
            agent._active_skills = []
            agent._tools = {}
            agent._pending_user_text = "检查项目并修复启动失败"
            captured_messages: list[list[dict[str, str]]] = []

            def fake_request(
                messages,
                _on_delta,
                _on_token_usage,
                _on_protocol_wait,
                _on_retry_status,
            ):
                captured_messages.append(messages)
                return "<final>已继续</final>", "", False

            agent._request_agent_reply = fake_request  # type: ignore[method-assign]
            deltas: list[str] = []

            result = LocalToolAgent.run_stream(agent, "继续", deltas.append)

        self.assertEqual(result, "已继续")
        self.assertEqual(deltas, ["已继续"])
        self.assertEqual(agent._pending_user_text, None)
        self.assertEqual(len(captured_messages), 1)
        sent_text = captured_messages[0][-1]["content"]
        self.assertIn("继续上一轮未完成任务", sent_text)
        self.assertIn("检查项目并修复启动失败", sent_text)
        self.assertNotEqual(sent_text, "继续")

    def test_repeated_continue_after_failure_keeps_original_pending_task(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            agent = object.__new__(LocalToolAgent)
            agent.workspace_root = Path(temp_dir)
            agent.config = SimpleNamespace(max_history_turns=6)
            agent._history = []
            agent._skill_manager = None
            agent._active_skills = []
            agent._tools = {}
            agent._pending_user_text = "检查项目并修复启动失败"

            def fake_request(
                _messages,
                _on_delta,
                _on_token_usage,
                _on_protocol_wait,
                _on_retry_status,
            ):
                raise AgentError("Agent 请求失败：无法连接模型服务。")

            agent._request_agent_reply = fake_request  # type: ignore[method-assign]

            with self.assertRaisesRegex(AgentError, "无法连接模型服务"):
                LocalToolAgent.run_stream(agent, "继续", lambda _delta: None)

        self.assertEqual(agent._pending_user_text, "检查项目并修复启动失败")

    def test_run_stream_executes_repaired_tool_call_without_streaming_protocol_text(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            agent = object.__new__(LocalToolAgent)
            agent.workspace_root = Path(temp_dir)
            agent.config = SimpleNamespace(max_history_turns=6, max_tool_output_chars=6000)
            agent._history = []
            agent._skill_manager = None
            agent._active_skills = []
            executed_arguments: list[dict[str, object]] = []

            def run_tool(arguments: dict[str, object]) -> ToolResult:
                executed_arguments.append(arguments)
                return ToolResult(ok=True, output="ok")

            agent._tools = {
                "run_command": ToolDefinition(
                    name="run_command",
                    description="Run a command",
                    argument_schema=(
                        '{"type":"object","properties":'
                        '{"command":{"type":"string"},"timeout_seconds":{"type":"number"}}}'
                    ),
                    requires_confirmation=False,
                    run=run_tool,
                )
            }
            replies = iter(
                [
                    (
                        '<tool>{"name":"runcommand","arguments":'
                        '{"command":"echo hi","timeoutseconds":30}</tool>',
                        "",
                        False,
                    ),
                    ("<final>完成</final>", "", False),
                ]
            )

            def fake_request(
                messages,
                _on_delta,
                _on_token_usage,
                _on_protocol_wait,
                _on_retry_status,
            ):
                return next(replies)

            agent._request_agent_reply = fake_request  # type: ignore[method-assign]
            deltas: list[str] = []

            result = LocalToolAgent.run_stream(agent, "请运行命令", deltas.append)

        self.assertEqual(result, "完成")
        self.assertEqual(deltas, ["完成"])
        self.assertEqual(executed_arguments, [{"command": "echo hi", "timeout_seconds": 30}])
        self.assertNotIn("<tool>", "".join(deltas))

    def test_stream_interruption_retries_and_reports_retry_status(self) -> None:
        class BrokenStream:
            def __init__(self) -> None:
                self._events = iter([SimpleNamespace(type="response.output_text.delta", delta="【进度】1/2\n")])

            def __iter__(self):
                return self

            def __next__(self):
                try:
                    return next(self._events)
                except StopIteration as exc:
                    raise RuntimeError("peer closed connection without sending complete message body") from exc

        class FakeResponses:
            def __init__(self) -> None:
                self.calls = 0

            def create(self, **_kwargs):
                self.calls += 1
                if self.calls == 1:
                    return BrokenStream()
                return iter([SimpleNamespace(type="response.output_text.delta", delta="<final>完成</final>")])

        agent = object.__new__(LocalToolAgent)
        agent.config = SimpleNamespace(
            request_retry_count=2,
            request_timeout_seconds=180,
            llm=SimpleNamespace(
                model="test-model",
                thinking_enabled=False,
                reasoning_effort="",
            ),
        )
        agent._client = SimpleNamespace(responses=FakeResponses())
        agent._system_prompt = lambda: "system"  # type: ignore[method-assign]
        agent._build_extra_body = lambda: {}  # type: ignore[method-assign]

        deltas: list[str] = []
        retry_statuses: list[str] = []

        reply, _reasoning, streamed = LocalToolAgent._request_agent_reply(
            agent,
            [{"role": "user", "content": "安装 Skill"}],
            deltas.append,
            lambda _input_tokens, _output_tokens: None,
            lambda: None,
            retry_statuses.append,
        )

        self.assertEqual(reply, "<final>完成</final>")
        self.assertTrue(streamed)
        self.assertEqual(deltas, ["完成"])
        self.assertEqual(len(retry_statuses), 1)
        self.assertIn("模型流式连接中断，正在重试 2/2", retry_statuses[0])
        self.assertIn("模型服务流式连接提前断开", retry_statuses[0])
        self.assertNotIn("peer closed connection", retry_statuses[0])

    def test_gpt_requests_include_stable_prompt_cache_key(self) -> None:
        class FakeResponses:
            def __init__(self) -> None:
                self.calls: list[dict[str, object]] = []

            def create(self, **kwargs):
                self.calls.append(kwargs)
                return iter([SimpleNamespace(type="response.output_text.delta", delta="<final>完成</final>")])

        responses = FakeResponses()
        agent = object.__new__(LocalToolAgent)
        agent.workspace_root = Path("D:/workspace/project")
        agent.config = SimpleNamespace(
            request_timeout_seconds=180,
            llm=SimpleNamespace(
                model="gpt-5.5",
                thinking_enabled=False,
                reasoning_effort="",
            ),
        )
        agent._client = SimpleNamespace(responses=responses)
        agent._system_prompt = lambda: "stable system prompt"  # type: ignore[method-assign]
        agent._build_extra_body = lambda: {}  # type: ignore[method-assign]
        messages_a = [
            {"role": "user", "content": "<project_instructions file=\"AGENTS.md\">\nstable\n</project_instructions>"},
            {"role": "user", "content": "第一轮问题"},
        ]
        messages_b = [
            {"role": "user", "content": "<project_instructions file=\"AGENTS.md\">\nstable\n</project_instructions>"},
            {"role": "user", "content": "第二轮问题"},
        ]

        LocalToolAgent._request_agent_reply_once(
            agent,
            messages_a,
            lambda _delta: None,
            lambda _input_tokens, _output_tokens, _cached_input_tokens: None,
            lambda: None,
        )
        LocalToolAgent._request_agent_reply_once(
            agent,
            messages_b,
            lambda _delta: None,
            lambda _input_tokens, _output_tokens, _cached_input_tokens: None,
            lambda: None,
        )

        self.assertIn("prompt_cache_key", responses.calls[0])
        self.assertEqual(responses.calls[0]["prompt_cache_key"], responses.calls[1]["prompt_cache_key"])

    def test_non_gpt_requests_skip_prompt_cache_key(self) -> None:
        class FakeResponses:
            def __init__(self) -> None:
                self.call: dict[str, object] | None = None

            def create(self, **kwargs):
                self.call = kwargs
                return iter([SimpleNamespace(type="response.output_text.delta", delta="<final>完成</final>")])

        responses = FakeResponses()
        agent = object.__new__(LocalToolAgent)
        agent.workspace_root = Path("D:/workspace/project")
        agent.config = SimpleNamespace(
            request_timeout_seconds=180,
            llm=SimpleNamespace(
                model="deepseek-v4-flash",
                thinking_enabled=False,
                reasoning_effort="",
            ),
        )
        agent._client = SimpleNamespace(responses=responses)
        agent._system_prompt = lambda: "stable system prompt"  # type: ignore[method-assign]
        agent._build_extra_body = lambda: {}  # type: ignore[method-assign]

        LocalToolAgent._request_agent_reply_once(
            agent,
            [{"role": "user", "content": "问题"}],
            lambda _delta: None,
            lambda _input_tokens, _output_tokens, _cached_input_tokens: None,
            lambda: None,
        )

        self.assertIsNotNone(responses.call)
        self.assertNotIn("prompt_cache_key", responses.call or {})

    def test_reply_streamer_emits_progress_before_line_start_tool_only(self) -> None:
        deltas: list[str] = []
        streamer = _AgentReplyStreamer(deltas.append)

        streamer.push("【进度】1/3\n")
        streamer.push("- 正在读取项目\n<to")
        streamer.push('ol>{"name":"read_file","arguments":{"path":"main.py"}}</tool>')
        streamer.finish()

        text = "".join(deltas)
        self.assertIn("正在读取项目", text)
        self.assertNotIn("<tool>", text)
        self.assertNotIn("read_file", text)

    def test_reply_streamer_notifies_wait_when_hiding_line_start_tool(self) -> None:
        deltas: list[str] = []
        wait_events: list[str] = []
        streamer = _AgentReplyStreamer(deltas.append, lambda: wait_events.append("wait"))

        streamer.push("【进度】1/3\n")
        streamer.push("- 正在读取项目\n<to")
        streamer.push('ol>{"name":"read_file","arguments":{"path":"main.py"}}</tool>')
        streamer.finish()

        self.assertEqual(wait_events, ["wait"])
        self.assertIn("正在读取项目", "".join(deltas))

    def test_reply_streamer_does_not_notify_wait_without_visible_progress(self) -> None:
        deltas: list[str] = []
        wait_events: list[str] = []
        streamer = _AgentReplyStreamer(deltas.append, lambda: wait_events.append("wait"))

        streamer.push('<tool>{"name":"read_file","arguments":{"path":"main.py"}}</tool>')
        streamer.finish()

        self.assertEqual(wait_events, [])
        self.assertEqual(deltas, [])

    def test_reply_streamer_hides_inline_tool_text_as_protocol_risk(self) -> None:
        deltas: list[str] = []
        streamer = _AgentReplyStreamer(deltas.append)

        streamer.push('我会说明一下 <tool>{"name":"read_file","arguments":{"path":"main.py"}}')
        streamer.finish()

        text = "".join(deltas)
        self.assertIn("我会说明一下", text)
        self.assertNotIn("<tool>", text)
        self.assertNotIn("read_file", text)

    def test_agents_md_body_is_not_added_to_system_prompt(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            workspace = Path(temp_dir)
            (workspace / "AGENTS.md").write_text(
                "# AGENTS.md\n\n这句正文不应进入 system prompt。",
                encoding="utf-8",
            )
            agent = object.__new__(LocalToolAgent)
            agent.workspace_root = workspace
            agent._tools = {}
            agent._memory_store = None
            agent._skill_manager = None
            agent._active_skills = []
            agent._system_prompt_template = LocalToolAgent._load_system_prompt_template(agent)

            prompt = LocalToolAgent._system_prompt(agent)

        self.assertIn("AGENTS.md", prompt)
        self.assertNotIn("这句正文不应进入 system prompt。", prompt)

    def test_workspace_detection_summary_is_added_to_system_prompt(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            workspace = Path(temp_dir)
            agent = object.__new__(LocalToolAgent)
            agent.workspace_root = workspace
            agent.config = SimpleNamespace(
                workspace_detection_summary=f"从启动目录发现 .git，选定：{workspace}"
            )
            agent._tools = {}
            agent._memory_store = None
            agent._skill_manager = None
            agent._active_skills = []
            agent._system_prompt_template = LocalToolAgent._load_system_prompt_template(agent)

            prompt = LocalToolAgent._system_prompt(agent)

        self.assertIn("工作区检测：从启动目录发现 .git", prompt)


if __name__ == "__main__":
    unittest.main()
