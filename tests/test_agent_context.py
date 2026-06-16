from __future__ import annotations

import json
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch

from ai_voice_agent.agent import (
    AgentModelReply,
    AgentConfig,
    AgentError,
    LocalToolAgent,
    ToolCall,
    ToolDefinition,
    ToolResult,
)
from ai_voice_agent.session import SessionStore
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
            agent._session_store = None
            agent._session_state = None
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
                return AgentModelReply(message={"role": "assistant", "content": "完成"}, content="完成")

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
            agent._session_store = None
            agent._session_state = None
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
            agent._session_store = None
            agent._session_state = None
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
                return AgentModelReply(message={"role": "assistant", "content": "已继续"}, content="已继续")

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
            agent._session_store = None
            agent._session_state = None
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

    def test_run_stream_executes_official_tool_call_and_returns_tool_result_message(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            agent = object.__new__(LocalToolAgent)
            agent.workspace_root = Path(temp_dir)
            agent.config = SimpleNamespace(max_history_turns=6, max_tool_output_chars=6000)
            agent._history = []
            agent._session_store = None
            agent._session_state = None
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
            function_name = LocalToolAgent._function_name_for_tool(agent, "run_command")
            replies = iter(
                [
                    AgentModelReply(
                        message={
                            "role": "assistant",
                            "content": None,
                            "tool_calls": [
                                {
                                    "id": "call_1",
                                    "type": "function",
                                    "function": {
                                        "name": function_name,
                                        "arguments": '{"command":"echo hi","timeoutseconds":30}',
                                    },
                                }
                            ],
                        },
                        content="",
                        tool_calls=[
                            ToolCall(
                                name="runcommand",
                                arguments={"command": "echo hi", "timeoutseconds": 30},
                                id="call_1",
                                function_name=function_name,
                            )
                        ],
                    ),
                    AgentModelReply(message={"role": "assistant", "content": "完成"}, content="完成"),
                ]
            )
            captured_messages: list[list[dict[str, object]]] = []

            def fake_request(
                messages,
                _on_delta,
                _on_token_usage,
                _on_protocol_wait,
                _on_retry_status,
            ):
                captured_messages.append(messages)
                return next(replies)

            agent._request_agent_reply = fake_request  # type: ignore[method-assign]
            deltas: list[str] = []

            result = LocalToolAgent.run_stream(agent, "请运行命令", deltas.append)

        self.assertEqual(result, "完成")
        self.assertEqual(deltas, ["完成"])
        self.assertEqual(executed_arguments, [{"command": "echo hi", "timeout_seconds": 30}])
        self.assertEqual(captured_messages[1][-1]["role"], "tool")
        self.assertEqual(captured_messages[1][-1]["tool_call_id"], "call_1")
        self.assertIn("状态：成功", str(captured_messages[1][-1]["content"]))

    def test_run_stream_writes_session_events(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            workspace = Path(temp_dir)
            agent = object.__new__(LocalToolAgent)
            agent.workspace_root = workspace
            agent.config = SimpleNamespace(max_history_turns=6)
            agent._history = []
            agent._skill_manager = None
            agent._active_skills = []
            agent._tools = {}
            store = SessionStore(workspace / ".agent_sessions")
            state = store.start_session(workspace)
            agent._session_store = store
            agent._session_state = state

            def fake_request(
                _messages,
                _on_delta,
                _on_token_usage,
                _on_protocol_wait,
                _on_retry_status,
            ):
                return AgentModelReply(message={"role": "assistant", "content": "完成"}, content="完成")

            agent._request_agent_reply = fake_request  # type: ignore[method-assign]

            LocalToolAgent.run_stream(agent, "记录会话", lambda _delta: None)

            events = [
                json.loads(line)
                for line in state.path.read_text(encoding="utf-8").splitlines()
                if line.strip()
            ]
            restored = store.load_session(state.session_id)

        self.assertEqual(
            [event["type"] for event in events],
            ["session_started", "user_message", "assistant_message"],
        )
        self.assertEqual(restored.messages[-2]["content"], "记录会话")
        self.assertEqual(restored.messages[-1]["content"], "完成")

    def test_resume_session_restores_recent_history(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            workspace = Path(temp_dir)
            store = SessionStore(workspace / ".agent_sessions")
            state = store.start_session(workspace)
            store.append_event(state.session_id, "user_message", {"content": "旧问题"})
            store.append_event(state.session_id, "assistant_message", {"content": "旧回答"})

            agent = object.__new__(LocalToolAgent)
            agent.workspace_root = workspace
            agent.config = SimpleNamespace(max_history_turns=6)
            agent._session_store = store
            agent._session_state = None
            agent._history = []
            agent._pending_user_text = "未完成"
            agent._active_skills = ["placeholder"]

            restored = LocalToolAgent.resume_session(agent, state.session_id)

        self.assertEqual(restored.session_id, state.session_id)
        self.assertEqual(agent._history, restored.messages)
        self.assertIsNone(agent._pending_user_text)
        self.assertEqual(agent._active_skills, [])

    def test_retryable_request_errors_retry_and_report_status(self) -> None:
        class FakeChatCompletions:
            def __init__(self) -> None:
                self.calls = 0

            def create(self, **_kwargs):
                self.calls += 1
                if self.calls == 1:
                    raise RuntimeError("peer closed connection without sending complete message body")
                return [
                    SimpleNamespace(
                        choices=[SimpleNamespace(delta=SimpleNamespace(content="完成", tool_calls=None))],
                    ),
                    SimpleNamespace(usage=SimpleNamespace(prompt_tokens=1, completion_tokens=1)),
                ]

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
        completions = FakeChatCompletions()
        agent._client = SimpleNamespace(chat=SimpleNamespace(completions=completions))
        agent._system_prompt = lambda: "system"  # type: ignore[method-assign]
        agent._build_extra_body = lambda: {}  # type: ignore[method-assign]
        agent._chat_completion_tools = lambda: []  # type: ignore[method-assign]

        deltas: list[str] = []
        retry_statuses: list[str] = []

        reply = LocalToolAgent._request_agent_reply(
            agent,
            [{"role": "user", "content": "安装 Skill"}],
            deltas.append,
            lambda _input_tokens, _output_tokens, _cached_input_tokens: None,
            lambda: None,
            retry_statuses.append,
        )

        self.assertEqual(reply.content, "完成")
        self.assertEqual(completions.calls, 2)
        self.assertEqual(deltas, ["完成"])
        self.assertEqual(len(retry_statuses), 1)
        self.assertIn("模型请求中断，正在重试 2/2", retry_statuses[0])
        self.assertIn("模型服务连接提前断开", retry_statuses[0])
        self.assertNotIn("peer closed connection", retry_statuses[0])

    def test_chat_completion_request_uses_official_tool_calls_shape(self) -> None:
        class FakeChatCompletions:
            def __init__(self) -> None:
                self.call: dict[str, object] | None = None

            def create(self, **kwargs):
                self.call = kwargs
                return [
                    SimpleNamespace(
                        choices=[SimpleNamespace(delta=SimpleNamespace(content="完成", tool_calls=None))],
                    ),
                    SimpleNamespace(usage=SimpleNamespace(prompt_tokens=1, completion_tokens=1)),
                ]

        completions = FakeChatCompletions()
        agent = object.__new__(LocalToolAgent)
        agent.workspace_root = Path("D:/workspace/project")
        agent.config = SimpleNamespace(
            request_timeout_seconds=180,
            llm=SimpleNamespace(
                model="deepseek-v4-pro",
                thinking_enabled=False,
                reasoning_effort="",
            ),
        )
        agent._client = SimpleNamespace(chat=SimpleNamespace(completions=completions))
        agent._system_prompt = lambda: "system prompt"  # type: ignore[method-assign]
        agent._build_extra_body = lambda: {"thinking": {"type": "disabled"}}  # type: ignore[method-assign]
        agent._tools = {
            "local_project.workspace.read_file": ToolDefinition(
                name="local_project.workspace.read_file",
                description="读取文件。",
                argument_schema=(
                    '{"type":"object","properties":{"path":{"type":"string"}},'
                    '"required":["path"]}'
                ),
                requires_confirmation=False,
                run=lambda _arguments: ToolResult(ok=True, output="ok"),
            )
        }

        LocalToolAgent._request_agent_reply_once(
            agent,
            [{"role": "user", "content": "读取 README"}],
            lambda _delta: None,
            lambda _input_tokens, _output_tokens, _cached_input_tokens: None,
            lambda: None,
        )

        self.assertIsNotNone(completions.call)
        call = completions.call or {}
        self.assertEqual(call["model"], "deepseek-v4-pro")
        self.assertEqual(call["tool_choice"], "auto")
        self.assertEqual(call["extra_body"], {"thinking": {"type": "disabled"}})
        self.assertNotIn("instructions", call)
        self.assertNotIn("input", call)
        self.assertTrue(call.get("stream"))
        messages = call["messages"]
        assert isinstance(messages, list)
        self.assertEqual(messages[0], {"role": "system", "content": "system prompt"})
        self.assertEqual(messages[1], {"role": "user", "content": "读取 README"})
        tools = call["tools"]
        assert isinstance(tools, list)
        self.assertEqual(tools[0]["type"], "function")
        function = tools[0]["function"]
        self.assertRegex(function["name"], r"^[A-Za-z0-9_-]{1,64}$")
        self.assertNotIn(".", function["name"])
        self.assertNotIn(":", function["name"])
        self.assertEqual(function["description"], "读取文件。")
        self.assertEqual(function["parameters"]["type"], "object")
        self.assertEqual(function["parameters"]["required"], ["path"])

    def test_assistant_tool_call_message_preserves_official_function_name_only(self) -> None:
        agent = object.__new__(LocalToolAgent)
        agent._tools = {
            "read_file": ToolDefinition(
                name="read_file",
                description="读取文件。",
                argument_schema='{"path":"README.md"}',
                requires_confirmation=False,
                run=lambda _arguments: ToolResult(ok=True, output="ok"),
            )
        }
        function_name = LocalToolAgent._function_name_for_tool(agent, "read_file")
        tool_call = ToolCall(
            name="read_file",
            arguments={"path": "README.md"},
            id="call_1",
            function_name=function_name,
        )

        message = LocalToolAgent._assistant_tool_call_message(
            agent,
            {},
            "",
            [tool_call],
            "内部推理不应回传。",
        )

        self.assertEqual(message["role"], "assistant")
        self.assertIsNone(message["content"])
        self.assertEqual(message["tool_calls"][0]["id"], "call_1")
        self.assertEqual(message["tool_calls"][0]["type"], "function")
        self.assertEqual(message["tool_calls"][0]["function"]["name"], function_name)
        self.assertEqual(
            message["tool_calls"][0]["function"]["arguments"],
            '{"path": "README.md"}',
        )
        self.assertEqual(message["reasoning_content"], "内部推理不应回传。")

    def test_gpt_requests_include_stable_prompt_cache_key(self) -> None:
        class FakeChatCompletions:
            def __init__(self) -> None:
                self.calls: list[dict[str, object]] = []

            def create(self, **kwargs):
                self.calls.append(kwargs)
                return [
                    SimpleNamespace(
                        choices=[SimpleNamespace(delta=SimpleNamespace(content="完成", tool_calls=None))],
                    ),
                    SimpleNamespace(usage=SimpleNamespace(prompt_tokens=1, completion_tokens=1)),
                ]

        completions = FakeChatCompletions()
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
        agent._client = SimpleNamespace(chat=SimpleNamespace(completions=completions))
        agent._system_prompt = lambda: "stable system prompt"  # type: ignore[method-assign]
        agent._build_extra_body = lambda: {}  # type: ignore[method-assign]
        agent._chat_completion_tools = lambda: []  # type: ignore[method-assign]
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

        self.assertIn("prompt_cache_key", completions.calls[0])
        self.assertEqual(completions.calls[0]["prompt_cache_key"], completions.calls[1]["prompt_cache_key"])

    def test_non_gpt_requests_skip_prompt_cache_key(self) -> None:
        class FakeChatCompletions:
            def __init__(self) -> None:
                self.call: dict[str, object] | None = None

            def create(self, **kwargs):
                self.call = kwargs
                return [
                    SimpleNamespace(
                        choices=[SimpleNamespace(delta=SimpleNamespace(content="完成", tool_calls=None))],
                    ),
                    SimpleNamespace(usage=SimpleNamespace(prompt_tokens=1, completion_tokens=1)),
                ]

        completions = FakeChatCompletions()
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
        agent._client = SimpleNamespace(chat=SimpleNamespace(completions=completions))
        agent._system_prompt = lambda: "stable system prompt"  # type: ignore[method-assign]
        agent._build_extra_body = lambda: {}  # type: ignore[method-assign]
        agent._chat_completion_tools = lambda: []  # type: ignore[method-assign]

        LocalToolAgent._request_agent_reply_once(
            agent,
            [{"role": "user", "content": "问题"}],
            lambda _delta: None,
            lambda _input_tokens, _output_tokens, _cached_input_tokens: None,
            lambda: None,
        )

        self.assertIsNotNone(completions.call)
        self.assertNotIn("prompt_cache_key", completions.call or {})

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
