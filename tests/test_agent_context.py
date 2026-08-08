from __future__ import annotations

import json
import tempfile
import threading
import time
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch

from omnicrawl.agent import (
    AgentModelReply,
    AgentConfig,
    AgentError,
    LocalToolAgent,
    ToolCall,
    ToolDefinition,
    ToolResult,
)
from omnicrawl.agent.history import restore_history_window
from omnicrawl.agent.llm_protocol import assistant_tool_call_message, function_name_for_tool
from omnicrawl.agent.prompt_context import build_system_prompt
from omnicrawl.config.context_compaction import ContextCompactionConfig
from omnicrawl.mcp.config import MCPConfig
from omnicrawl.project import ProjectStore
from omnicrawl.session import SessionStore
from omnicrawl.skill import Skill, SkillMatchResult, SkillMeta
from omnicrawl.slash_commands import (
    build_slash_command_options,
    build_slash_commands,
    handle_session_command,
    handle_subagent_task_command,
)
from omnicrawl.temp_workspace import AgentTempWorkspaceConfig


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
                context_compaction=ContextCompactionConfig(enabled=False),
                temp_workspace=AgentTempWorkspaceConfig(),
            )

            with patch("omnicrawl.agent.core.AgentTempWorkspace", FakeTempWorkspace):
                with self.assertRaisesRegex(AgentError, "缺少 API Key"):
                    LocalToolAgent(config)

        self.assertEqual(calls, ["init", "ensure", "clean_if_due"])

    def test_agent_initialization_defers_mcp_discovery_until_status_or_request(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            config = AgentConfig(
                llm=SimpleNamespace(
                    api_key="test-key",
                    base_url="https://example.test/v1",
                    model="test-model",
                ),
                workspace_root=Path(temp_dir),
                memory_enabled=False,
                context_compaction=ContextCompactionConfig(enabled=False),
                session_enabled=False,
                skills_enabled=False,
                mcp_config=MCPConfig(enabled=True),
                temp_workspace=AgentTempWorkspaceConfig(cleanup_enabled=False),
            )
            discover_calls = 0

            def fake_discover(manager) -> None:
                nonlocal discover_calls
                discover_calls += 1
                manager._discovered = True

            with patch("omnicrawl.mcp.client.MCPClientManager.discover", fake_discover):
                agent = LocalToolAgent(config)
                self.assertEqual(discover_calls, 0)

                agent.format_mcp_status()
                self.assertEqual(discover_calls, 1)
                agent.close()

    def test_agent_close_discards_empty_startup_session(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            workspace = Path(temp_dir)
            config = AgentConfig(
                llm=SimpleNamespace(
                    api_key="test-key",
                    base_url="https://example.test/v1",
                    model="test-model",
                ),
                workspace_root=workspace,
                memory_enabled=False,
                context_compaction=ContextCompactionConfig(enabled=False),
                skills_enabled=False,
                mcp_config=MCPConfig(enabled=False),
                temp_workspace=AgentTempWorkspaceConfig(cleanup_enabled=False),
            )

            with patch("openai.OpenAI", return_value=SimpleNamespace()):
                agent = LocalToolAgent(config)
                session_id = agent.current_session_id
                agent.close()

            store = SessionStore(workspace / ".agent_sessions")
            sessions = store.list_sessions(workspace_root=workspace)

        self.assertTrue(session_id)
        self.assertEqual(sessions, [])

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
        self.assertTrue(
            messages[0]["content"].startswith(
                '<project_instructions source="AGENTS.md" trust="user-and-workspace">'
            )
        )
        self.assertIn("<authority_boundary>", messages[0]["content"])
        self.assertIn("不得覆盖 system 安全规则", messages[0]["content"])
        self.assertIn("# AGENTS.md", messages[0]["content"])
        self.assertIn("必须先理解再执行。", messages[0]["content"])
        self.assertTrue(messages[0]["content"].rstrip().endswith("</project_instructions>"))

    def test_run_stream_sends_stable_context_before_history_and_current_user(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            workspace = Path(temp_dir)
            (workspace / "AGENTS.md").write_text(
                "# AGENTS.md\n\n进度实时可见。",
                encoding="utf-8",
            )
            agent = object.__new__(LocalToolAgent)
            agent.workspace_root = workspace
            agent.config = SimpleNamespace(
                max_history_turns=6,
                workspace_detection_summary="从启动目录发现 .git",
            )
            agent._history = [
                {"role": "user", "content": "上一轮问题"},
                {"role": "assistant", "content": "上一轮回答"},
            ]
            agent._session_store = None
            agent._session_state = None
            agent._skill_manager = None
            agent._active_skills = []
            agent._temp_workspace = SimpleNamespace(display_path=".agent_tmp")
            agent._tools = {
                "read_file": ToolDefinition(
                    name="read_file",
                    description="读取文件。",
                    argument_schema='{"path":"README.md"}',
                    requires_confirmation=True,
                    run=lambda _arguments: ToolResult(ok=True, output="ok"),
                )
            }
            captured_messages: list[list[dict[str, str]]] = []

            def fake_request(
                messages,
                _on_delta,
                _on_token_usage,
                _on_protocol_wait,
                _on_retry_status,
                on_stream_rollback=None,
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
        self.assertIn('<project_instructions source="AGENTS.md"', sent_messages[0]["content"])
        self.assertIn("进度实时可见。", sent_messages[0]["content"])
        self.assertIn("不得覆盖 system 安全规则", sent_messages[0]["content"])
        self.assertIn('<tool_capabilities source="host-tool-registry"', sent_messages[1]["content"])
        self.assertIn("search_tools", sent_messages[1]["content"])
        self.assertIn("invoke_tool", sent_messages[1]["content"])
        self.assertNotIn("read_file", sent_messages[1]["content"])
        self.assertIn('<runtime_context source="host-runtime"', sent_messages[2]["content"])
        self.assertIn("工作区检测：从启动目录发现 .git", sent_messages[2]["content"])
        self.assertEqual(sent_messages[3]["content"], "上一轮问题")
        self.assertEqual(sent_messages[4]["content"], "上一轮回答")
        self.assertEqual(sent_messages[-1]["content"], "请检查项目状态")
        self.assertEqual(agent._history[-2]["content"], "请检查项目状态")
        self.assertNotIn("project_instructions", agent._history[-2]["content"])

    def test_global_and_project_agents_are_both_loaded_with_project_last(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            global_agents = root / "global" / "AGENTS.md"
            global_agents.parent.mkdir()
            global_agents.write_text("全局规则", encoding="utf-8")
            workspace = root / "workspace"
            workspace.mkdir()
            (workspace / "AGENTS.md").write_text("项目规则", encoding="utf-8")
            agent = object.__new__(LocalToolAgent)
            agent.workspace_root = workspace

            with patch("omnicrawl.agent.core.global_agents_path", return_value=global_agents):
                messages = LocalToolAgent._project_instructions_messages(agent)

        self.assertEqual(len(messages), 1)
        content = messages[0]["content"]
        self.assertIn("全局规则", content)
        self.assertIn("项目规则", content)
        self.assertLess(content.index("全局规则"), content.index("项目规则"))
        self.assertIn("用户级", content)
        self.assertIn("项目级", content)

    def test_missing_agents_md_adds_no_project_context_message(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            agent = object.__new__(LocalToolAgent)
            agent.workspace_root = Path(temp_dir)

            with patch(
                "omnicrawl.agent.core.global_agents_path",
                return_value=Path(temp_dir) / "missing-global" / "AGENTS.md",
            ):
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
                on_stream_rollback=None,
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
                on_stream_rollback=None,
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
                on_stream_rollback=None,
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
                "powershell": ToolDefinition(
                    name="powershell",
                    description="Run a PowerShell command",
                    argument_schema=(
                        '{"type":"object","properties":'
                        '{"command":{"type":"string"},"timeout_seconds":{"type":"number"}}}'
                    ),
                    requires_confirmation=False,
                    run=run_tool,
                )
            }
            function_name = function_name_for_tool("powershell")
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
                                name="powershellcommand",
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
                on_stream_rollback=None,
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

    def test_run_stream_executes_same_reply_tools_concurrently_and_returns_results_in_call_order(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            agent = object.__new__(LocalToolAgent)
            agent.workspace_root = Path(temp_dir)
            agent.config = SimpleNamespace(max_history_turns=6, max_tool_output_chars=6000)
            agent._history = []
            agent._session_store = None
            agent._session_state = None
            agent._skill_manager = None
            agent._active_skills = []
            both_started = threading.Event()
            release = threading.Event()
            started: list[str] = []
            lock = threading.Lock()

            def concurrent_tool(name: str, *, ok: bool = True):
                def run(_arguments):
                    with lock:
                        started.append(name)
                        if len(started) == 2:
                            both_started.set()
                    if not both_started.wait(timeout=1):
                        return ToolResult(ok=False, output="未并发启动")
                    release.wait(timeout=1)
                    return ToolResult(ok=ok, output=f"{name}-result")
                return run

            agent._tools = {
                "read_file": ToolDefinition("read_file", "read", "{}", False, concurrent_tool("read")),
                "search_text": ToolDefinition("search_text", "search", "{}", False, concurrent_tool("search", ok=False)),
            }
            calls = [
                ToolCall("read_file", {}, "call_1", function_name_for_tool("read_file")),
                ToolCall("search_text", {}, "call_2", function_name_for_tool("search_text")),
            ]
            replies = iter([
                AgentModelReply(
                    message=assistant_tool_call_message({}, "", calls, "", function_name_for_tool=function_name_for_tool),
                    content="",
                    tool_calls=calls,
                ),
                AgentModelReply({"role": "assistant", "content": "完成"}, "完成"),
            ])
            captured_messages = []
            agent._request_agent_reply = lambda messages, *_args, **_kwargs: (captured_messages.append(list(messages)), next(replies))[1]  # type: ignore[method-assign]

            result_holder: list[str] = []
            thread = threading.Thread(
                target=lambda: result_holder.append(LocalToolAgent.run_stream(agent, "并发检查", lambda _delta: None))
            )
            thread.start()
            self.assertTrue(both_started.wait(timeout=1), "同批只读工具没有并发启动")
            release.set()
            thread.join(timeout=2)

        self.assertFalse(thread.is_alive())
        self.assertEqual(result_holder, ["完成"])
        tool_messages = [message for message in captured_messages[1] if message.get("role") == "tool"]
        self.assertEqual([message["tool_call_id"] for message in tool_messages], ["call_1", "call_2"])
        self.assertIn("状态：成功", tool_messages[0]["content"])
        self.assertIn("状态：失败", tool_messages[1]["content"])

    def test_run_stream_treats_replace_text_as_serial_barrier(self) -> None:
        tool = ToolDefinition("replace_text", "replace", "{}", False, lambda _args: None)

        self.assertTrue(
            LocalToolAgent._tool_call_requires_serial_execution(tool, {"path": "sample.txt"})
        )

    def test_run_stream_treats_write_and_delete_calls_as_serial_barriers(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            agent = object.__new__(LocalToolAgent)
            agent.workspace_root = Path(temp_dir)
            agent.config = SimpleNamespace(max_history_turns=6, max_tool_output_chars=6000)
            agent._history = []
            agent._session_store = None
            agent._session_state = None
            agent._skill_manager = None
            agent._active_skills = []
            active = 0
            overlap = False
            execution: list[str] = []
            lock = threading.Lock()

            def run_named(name: str):
                def run(_arguments):
                    nonlocal active, overlap
                    with lock:
                        if active:
                            overlap = True
                        active += 1
                        execution.append(name)
                    time.sleep(0.02)
                    with lock:
                        active -= 1
                    return ToolResult(ok=True, output=name)
                return run

            agent._tools = {
                "read_file": ToolDefinition("read_file", "read", "{}", False, run_named("read")),
                "write_file": ToolDefinition("write_file", "write", "{}", False, run_named("write")),
                "bash": ToolDefinition("bash", "bash", "{}", False, run_named("delete")),
            }
            calls = [
                ToolCall("read_file", {}, "call_1"),
                ToolCall("write_file", {"path": "a"}, "call_2"),
                ToolCall("bash", {"command": "rm a"}, "call_3"),
            ]
            replies = iter([
                AgentModelReply({"role": "assistant", "content": None}, "", calls),
                AgentModelReply({"role": "assistant", "content": "完成"}, "完成"),
            ])
            agent._request_agent_reply = lambda *_args, **_kwargs: next(replies)  # type: ignore[method-assign]

            LocalToolAgent.run_stream(agent, "执行", lambda _delta: None)

        self.assertFalse(overlap)
        self.assertEqual(execution, ["read", "write", "delete"])

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
                on_stream_rollback=None,
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
            ["session_started", "user_message", "assistant_message", "turn_snapshot"],
        )
        self.assertEqual(events[-1]["payload"]["version"], 1)
        self.assertIn("workspace", events[-1]["payload"]["roots"])
        self.assertEqual(restored.messages[-2]["content"], "记录会话")
        self.assertEqual(restored.messages[-1]["content"], "完成")

    def test_run_stream_persists_full_tool_output_as_session_artifact(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            workspace = Path(temp_dir)
            agent = object.__new__(LocalToolAgent)
            agent.workspace_root = workspace
            agent.config = SimpleNamespace(max_history_turns=6, max_tool_output_chars=32)
            agent._history = []
            agent._skill_manager = None
            agent._active_skills = []
            long_output = "0123456789" * 1000
            agent._tools = {
                "big_tool": ToolDefinition(
                    name="big_tool",
                    description="大输出工具",
                    argument_schema="{}",
                    requires_confirmation=False,
                    run=lambda _arguments: ToolResult(ok=True, output=long_output),
                )
            }
            store = SessionStore(workspace / ".agent_sessions")
            state = store.start_session(workspace)
            agent._session_store = store
            agent._session_state = state

            calls = 0

            def fake_request(
                _messages,
                _on_delta,
                _on_token_usage,
                _on_protocol_wait,
                _on_retry_status,
                on_stream_rollback=None,
            ):
                nonlocal calls
                calls += 1
                if calls == 1:
                    return AgentModelReply(
                        message={
                            "role": "assistant",
                            "content": "",
                            "tool_calls": [
                                {
                                    "id": "call_1",
                                    "type": "function",
                                    "function": {"name": "big_tool", "arguments": "{}"},
                                }
                            ],
                        },
                        content="",
                        tool_calls=[ToolCall(name="big_tool", id="call_1", function_name="big_tool")],
                    )
                return AgentModelReply(message={"role": "assistant", "content": "完成"}, content="完成")

            agent._request_agent_reply = fake_request  # type: ignore[method-assign]

            LocalToolAgent.run_stream(agent, "调用大工具", lambda _delta: None)

            events = [
                json.loads(line)
                for line in state.path.read_text(encoding="utf-8").splitlines()
                if line.strip()
            ]
            tool_payload = next(event["payload"] for event in events if event["type"] == "tool_result")
            artifact_path = workspace / ".agent_sessions" / tool_payload["artifact_path"]
            artifact_exists = artifact_path.is_file()
            artifact_text = artifact_path.read_text(encoding="utf-8")

        self.assertEqual(tool_payload["storage"], "artifact")
        self.assertIn("output_preview", tool_payload)
        self.assertIn("model_output", tool_payload)
        self.assertTrue(tool_payload["model_output"].endswith("... 工具输出已截断。"))
        self.assertTrue(artifact_exists)
        self.assertEqual(artifact_text, long_output)


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

    def test_resume_session_cancels_old_session_subagents_before_switch(self) -> None:
        """后台任务不能在父 Session 切换后继续占用旧会话能力。"""

        with tempfile.TemporaryDirectory() as temp_dir:
            workspace = Path(temp_dir)
            store = SessionStore(workspace / ".agent_sessions")
            current = store.start_session(workspace)
            target = store.start_session(workspace)
            store.append_event(current.session_id, "user_message", {"content": "当前任务"})
            store.append_event(target.session_id, "user_message", {"content": "目标会话"})
            calls = []
            coordinator = SimpleNamespace(
                cancel_and_wait=lambda **kwargs: calls.append(
                    ("cancel", dict(kwargs))
                )
                or True,
                resume_accepting_when_idle=lambda: calls.append(("resume", {})),
            )

            agent = object.__new__(LocalToolAgent)
            agent.workspace_root = workspace
            agent.config = SimpleNamespace(max_history_turns=6)
            agent._session_store = store
            agent._session_state = store.load_session(current.session_id)
            agent._history = [{"role": "user", "content": "当前任务"}]
            agent._pending_user_text = "处理中"
            agent._active_skills = []
            agent._subagent_coordinator = coordinator

            restored = LocalToolAgent.resume_session(agent, target.session_id)

        self.assertEqual(restored.session_id, target.session_id)
        self.assertEqual([name for name, _payload in calls], ["cancel", "resume"])
        self.assertIn("父 Session 即将切换", calls[0][1]["reason"])
        self.assertFalse(calls[0][1]["permanent"])

    def test_archive_session_keeps_current_session_when_subagent_will_not_stop(self) -> None:
        """取消超时必须阻止归档，避免后台任务失去父 Session 所有权。"""

        with tempfile.TemporaryDirectory() as temp_dir:
            workspace = Path(temp_dir)
            store = SessionStore(workspace / ".agent_sessions")
            current = store.start_session(workspace)
            store.append_event(current.session_id, "user_message", {"content": "当前任务"})
            calls = []
            coordinator = SimpleNamespace(
                cancel_and_wait=lambda **kwargs: calls.append(
                    ("cancel", dict(kwargs))
                )
                or False,
                resume_accepting_when_idle=lambda: calls.append(("resume", {})),
            )

            agent = object.__new__(LocalToolAgent)
            agent.workspace_root = workspace
            agent.config = SimpleNamespace(max_history_turns=6)
            agent._session_store = store
            agent._session_state = store.load_session(current.session_id)
            agent._history = [{"role": "user", "content": "当前任务"}]
            agent._pending_user_text = "处理中"
            agent._active_skills = []
            agent._subagent_coordinator = coordinator

            with self.assertRaisesRegex(AgentError, "父 Session 切换失败"):
                LocalToolAgent.archive_current_session(agent)
            persisted = store.load_session(current.session_id)

        self.assertEqual(agent.current_session_id, current.session_id)
        self.assertIsNone(persisted.archived_at)
        self.assertEqual([name for name, _payload in calls], ["cancel", "resume"])

    def test_start_or_resume_session_uses_configured_session_id(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            workspace = Path(temp_dir)
            store = SessionStore(workspace / ".agent_sessions")
            state = store.start_session(workspace)
            store.append_event(state.session_id, "user_message", {"content": "启动前问题"})
            store.append_event(state.session_id, "assistant_message", {"content": "启动前回答"})

            agent = object.__new__(LocalToolAgent)
            agent.workspace_root = workspace
            agent.config = SimpleNamespace(max_history_turns=6, resume_session_id=state.session_id)
            agent._session_store = store
            agent._session_state = None
            agent._history = []
            agent._pending_user_text = "旧的未完成任务"
            agent._active_skills = ["placeholder"]

            restored = LocalToolAgent._start_or_resume_session(agent)
            sessions = store.list_sessions(workspace_root=workspace)

        self.assertEqual(restored.session_id, state.session_id)
        self.assertEqual(agent._history, restored.messages)
        self.assertEqual(len(sessions), 1)
        self.assertEqual(agent._pending_user_text, None)
        self.assertEqual(agent._active_skills, [])

    def test_agent_reads_events_renames_and_exports_current_session(self) -> None:
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
            agent._session_state = store.load_session(state.session_id)
            agent._history = [{"role": "user", "content": "旧问题"}]

            events = LocalToolAgent.load_session_events(agent, state.session_id)
            rename_message = handle_session_command(agent, "/rename 设计实现会话")
            export_path = LocalToolAgent.export_current_session_markdown(
                agent,
                "# AI Voice Agent 对话记录\n",
            )
            exported_text = export_path.read_text(encoding="utf-8")
            sessions = LocalToolAgent.list_sessions(agent)

        self.assertEqual([event.type for event in events][-2:], ["user_message", "assistant_message"])
        self.assertIn("设计实现会话", rename_message or "")
        self.assertEqual(sessions[0].title, "设计实现会话")
        self.assertEqual(export_path.parent.name, "exports")
        self.assertEqual(exported_text, "# AI Voice Agent 对话记录\n")

    def test_archive_command_hides_current_session_and_resume_unarchives(self) -> None:
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
            agent._session_state = store.load_session(state.session_id)
            agent._history = [{"role": "user", "content": "旧问题"}]
            agent._pending_user_text = "处理中"
            agent._active_skills = ["placeholder"]
            agent._skill_manager = None

            archive_message = handle_session_command(agent, "/archive")
            new_session_id = agent.current_session_id
            active_sessions = LocalToolAgent.list_sessions(agent)
            archives_message = handle_session_command(agent, "/archives")
            restored = LocalToolAgent.resume_session(agent, state.session_id)
            active_after_resume = LocalToolAgent.list_sessions(agent)

        self.assertIn("已归档会话", archive_message or "")
        self.assertIn(new_session_id, {entry.session_id for entry in active_sessions})
        self.assertNotIn(state.session_id, {entry.session_id for entry in active_sessions})
        self.assertIn(state.session_id, archives_message or "")
        self.assertEqual(restored.session_id, state.session_id)
        self.assertIsNone(restored.archived_at)
        self.assertEqual(agent._history[-1]["content"], "旧回答")
        self.assertEqual(agent._pending_user_text, None)
        self.assertEqual(agent._active_skills, [])
        self.assertIn(state.session_id, {entry.session_id for entry in active_after_resume})
        self.assertIn("/archive", build_slash_commands(agent))
        self.assertIn("/archives", build_slash_commands(agent))

    def test_slash_command_options_include_skill_alias_search_for_qt_menu(self) -> None:
        agent = object.__new__(LocalToolAgent)

        class FakeSkillManager:
            def list_all(self):
                return [
                    SimpleNamespace(
                        name="ui-design",
                        description="Define frontend UI quality hierarchy and usability rules",
                    )
                ]

        agent._skill_manager = FakeSkillManager()

        options = build_slash_command_options(agent)
        by_command = {option["command"]: option for option in options}

        self.assertIn("/new", by_command)
        self.assertEqual(by_command["/reasoning"]["insert"], "/reasoning ")
        self.assertIn("/skill:ui-design", by_command)
        self.assertEqual(by_command["/skill:ui-design"]["category"], "Skill")
        self.assertIn("/ui-design", by_command["/skill:ui-design"]["search"])
        self.assertIn("frontend UI quality", by_command["/skill:ui-design"]["description"])

    def test_resume_archived_session_rejects_other_workspace_before_unarchive(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            workspace = Path(temp_dir) / "workspace"
            other_workspace = Path(temp_dir) / "other"
            workspace.mkdir()
            other_workspace.mkdir()
            store = SessionStore(workspace / ".agent_sessions")
            state = store.start_session(other_workspace)
            store.append_event(state.session_id, "user_message", {"content": "其他项目问题"})
            store.archive_session(state.session_id)

            agent = object.__new__(LocalToolAgent)
            agent.workspace_root = workspace
            agent.config = SimpleNamespace(max_history_turns=6)
            agent._session_store = store
            agent._session_state = None
            agent._history = []
            agent._pending_user_text = None
            agent._active_skills = []

            with self.assertRaisesRegex(AgentError, "不能恢复其他工作区的会话"):
                LocalToolAgent.resume_session(agent, state.session_id)

            still_archived = store.load_session(state.session_id)

        self.assertIsNotNone(still_archived.archived_at)
        self.assertEqual(still_archived.path.parent.name, "archive")

    def test_agent_project_methods_persist_projects_and_filter_sessions(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            workspace = Path(temp_dir) / "workspace"
            other_workspace = Path(temp_dir) / "other"
            workspace.mkdir()
            other_workspace.mkdir()
            store = SessionStore(workspace / ".agent_sessions")
            state = store.start_session(workspace)
            other_state = store.start_session(other_workspace)

            agent = object.__new__(LocalToolAgent)
            agent.workspace_root = workspace
            agent._session_store = store
            agent._project_store = ProjectStore(workspace / ".agent_sessions")

            projects = LocalToolAgent.list_projects(agent)
            imported = LocalToolAgent.import_project(agent, "外部项目", str(other_workspace))
            other_sessions = LocalToolAgent.list_sessions(
                agent,
                limit=10,
                project_path=imported.path,
            )
            current_sessions = LocalToolAgent.list_sessions(agent, limit=10)
            projects_file_exists = (workspace / ".agent_sessions" / "projects.json").is_file()

        self.assertTrue(projects_file_exists)
        self.assertIn(str(workspace.resolve()), {project.path for project in projects})
        self.assertEqual(imported.name, "外部项目")
        self.assertEqual([entry.session_id for entry in other_sessions], [other_state.session_id])
        self.assertEqual([entry.session_id for entry in current_sessions], [state.session_id])

    def test_run_stream_compacts_long_history_and_persists_summary(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            workspace = Path(temp_dir)
            agent = object.__new__(LocalToolAgent)
            agent.workspace_root = workspace
            agent.config = SimpleNamespace(max_history_turns=2)
            agent._history = [
                {"role": "user", "content": "第一轮问题"},
                {"role": "assistant", "content": "第一轮回答"},
                {"role": "user", "content": "第二轮问题"},
                {"role": "assistant", "content": "第二轮回答"},
            ]
            agent._skill_manager = None
            agent._active_skills = []
            agent._tools = {}
            store = SessionStore(workspace / ".agent_sessions")
            state = store.start_session(workspace)
            for message in agent._history:
                event_type = "user_message" if message["role"] == "user" else "assistant_message"
                store.append_event(state.session_id, event_type, {"content": message["content"]})
            agent._session_store = store
            agent._session_state = store.load_session(state.session_id)

            def fake_request(
                _messages,
                _on_delta,
                _on_token_usage,
                _on_protocol_wait,
                _on_retry_status,
                on_stream_rollback=None,
            ):
                return AgentModelReply(message={"role": "assistant", "content": "第三轮回答"}, content="第三轮回答")

            agent._request_agent_reply = fake_request  # type: ignore[method-assign]

            LocalToolAgent.run_stream(agent, "第三轮问题", lambda _delta: None)

            events = [
                json.loads(line)
                for line in state.path.read_text(encoding="utf-8").splitlines()
                if line.strip()
            ]
            restored = store.load_session(state.session_id)

        self.assertIn("compact_summary", [event["type"] for event in events])
        self.assertTrue(agent._history[0]["content"].startswith("会话压缩摘要："))
        self.assertEqual(agent._history[-2]["content"], "第三轮问题")
        self.assertEqual(agent._history[-1]["content"], "第三轮回答")
        self.assertEqual(restored.messages[0], agent._history[0])
        self.assertEqual(restored.messages[-2]["content"], "第三轮问题")

    def test_run_stream_records_cancelled_turn_separately_from_interruption(self) -> None:
        class UserCancelled(RuntimeError):
            pass

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
                on_stream_rollback=None,
            ):
                raise UserCancelled("用户取消")

            agent._request_agent_reply = fake_request  # type: ignore[method-assign]

            with self.assertRaises(UserCancelled):
                LocalToolAgent.run_stream(agent, "取消这一轮", lambda _delta: None)

            events = [
                json.loads(line)
                for line in state.path.read_text(encoding="utf-8").splitlines()
                if line.strip()
            ]

        self.assertEqual(events[-1]["type"], "turn_cancelled")
        self.assertEqual(events[-1]["payload"]["user_text"], "取消这一轮")

    def test_run_stream_cancelled_turn_preserves_task_in_history(self) -> None:
        """被取消的回合必须把任务文本与已执行工具摘要写入历史。

        否则用户取消后紧接着发送的延续消息会丢失前置上下文（AI 误判为
        新任务并重新探索项目）。两种取消入口（KeyboardInterrupt 与自定义
        取消异常）行为必须一致。
        """

        class UserCancelled(RuntimeError):
            pass

        def build_agent(temp_dir: str) -> LocalToolAgent:
            workspace = Path(temp_dir)
            agent = object.__new__(LocalToolAgent)
            agent.workspace_root = workspace
            agent.config = SimpleNamespace(
                max_history_turns=6,
                max_tool_output_chars=6000,
            )
            agent._history = []
            agent._skill_manager = None
            agent._active_skills = []
            agent._tools = {
                "read_file": ToolDefinition(
                    "read_file",
                    "read",
                    "{}",
                    False,
                    lambda _args: ToolResult(ok=True, output="文件内容"),
                ),
            }
            store = SessionStore(workspace / ".agent_sessions")
            state = store.start_session(workspace)
            agent._session_store = store
            agent._session_state = state
            return agent

        def run_with_cancel(agent: LocalToolAgent, cancel_exc: Exception) -> None:
            calls = [
                ToolCall(
                    "read_file",
                    {"path": "a.txt"},
                    "call_1",
                    function_name_for_tool("read_file"),
                )
            ]
            call_count = 0

            def fake_request(
                _messages,
                _on_delta,
                _on_token_usage,
                _on_protocol_wait,
                _on_retry_status,
                on_stream_rollback=None,
            ):
                nonlocal call_count
                call_count += 1
                if call_count == 1:
                    # 第一轮回复携带工具调用并正常执行，写入 executed_tools。
                    return AgentModelReply(
                        message=assistant_tool_call_message(
                            {},
                            "",
                            calls,
                            "",
                            function_name_for_tool=function_name_for_tool,
                        ),
                        content="",
                        tool_calls=calls,
                    )
                raise cancel_exc

            agent._request_agent_reply = fake_request  # type: ignore[method-assign]

            with self.assertRaises(type(cancel_exc)):
                LocalToolAgent.run_stream(agent, "取消这一轮", lambda _delta: None)

        for cancel_exc in (KeyboardInterrupt("用户取消"), UserCancelled("用户取消")):
            with tempfile.TemporaryDirectory() as temp_dir:
                agent = build_agent(temp_dir)
                run_with_cancel(agent, cancel_exc)
                self.assertEqual(
                    [message["content"] for message in agent._history],
                    [
                        "取消这一轮",
                        "（上一回合被取消，未生成最终回复）已执行工具：read_file",
                    ],
                    f"取消异常 {type(cancel_exc).__name__} 未保留回合上下文",
                )
                # 摘要为纯文本 assistant 消息，不携带未配对的 tool_calls 或 reasoning
                self.assertEqual(agent._history[1]["role"], "assistant")
                self.assertNotIn("tool_calls", agent._history[1])
                self.assertNotIn("reasoning_content", agent._history[1])

    def test_cancelled_turn_summary_covers_tool_counts_and_empty_case(self) -> None:
        from omnicrawl.agent.core import _ActiveTurnSnapshot

        class FakeStore:
            def capture(self, roots):  # noqa: ARG002 - 测试替身
                return {}

        snapshot = _ActiveTurnSnapshot(
            snapshot_id="s1",
            store=FakeStore(),  # type: ignore[arg-type]
            roots={},
            before={},
            executed_tools=["read_file", "search_text", "read_file"],
        )
        summary = LocalToolAgent._cancelled_turn_summary(snapshot)
        self.assertEqual(
            summary,
            "（上一回合被取消，未生成最终回复）已执行工具：read_file×2，search_text",
        )
        self.assertEqual(
            LocalToolAgent._cancelled_turn_summary(None),
            "（上一回合被取消，未生成最终回复）",
        )
        self.assertIn(
            "未执行任何工具",
            LocalToolAgent._cancelled_turn_summary(
                _ActiveTurnSnapshot(
                    snapshot_id="s2",
                    store=FakeStore(),  # type: ignore[arg-type]
                    roots={},
                    before={},
                )
            ),
        )

    def test_compact_command_writes_summary_and_is_listed(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            workspace = Path(temp_dir)
            agent = object.__new__(LocalToolAgent)
            agent.workspace_root = workspace
            agent.config = SimpleNamespace(max_history_turns=6)
            agent._history = [
                {"role": "user", "content": "第一轮问题"},
                {"role": "assistant", "content": "第一轮回答"},
                {"role": "user", "content": "第二轮问题"},
                {"role": "assistant", "content": "第二轮回答"},
            ]
            store = SessionStore(workspace / ".agent_sessions")
            state = store.start_session(workspace)
            for message in agent._history:
                event_type = "user_message" if message["role"] == "user" else "assistant_message"
                store.append_event(state.session_id, event_type, {"content": message["content"]})
            agent._session_store = store
            agent._session_state = store.load_session(state.session_id)
            agent._skill_manager = None

            message = handle_session_command(agent, "/compact")
            events = [
                json.loads(line)
                for line in state.path.read_text(encoding="utf-8").splitlines()
                if line.strip()
            ]

        self.assertIsNotNone(message)
        self.assertIn("已压缩当前会话", message or "")
        self.assertIn("/compact", build_slash_commands(agent))
        self.assertEqual(events[-1]["type"], "compact_summary")
        self.assertTrue(agent._history[0]["content"].startswith("会话压缩摘要："))

    def test_undo_command_rebuilds_runtime_history_and_is_listed(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            workspace = Path(temp_dir)
            store = SessionStore(workspace / ".agent_sessions")
            state = store.start_session(workspace)
            for role, content in (
                ("user", "第一轮问题"),
                ("assistant", "第一轮回答"),
                ("user", "第二轮问题"),
                ("assistant", "第二轮回答"),
            ):
                store.append_event(
                    state.session_id,
                    f"{role}_message",
                    {"content": content},
                )

            agent = object.__new__(LocalToolAgent)
            agent.workspace_root = workspace
            agent.config = SimpleNamespace(max_history_turns=6)
            agent._session_store = store
            agent._session_state = store.load_session(state.session_id)
            agent._history = list(agent._session_state.messages)
            agent._pending_user_text = None
            agent._active_skills = []
            agent._skill_manager = None

            message = handle_session_command(agent, "/undo")
            restored = store.load_session(state.session_id)
            options = {item["command"]: item for item in build_slash_command_options(agent)}

        self.assertIn("已回退最近一轮", message or "")
        self.assertEqual(agent._history, restored.messages)
        self.assertEqual(agent._history[-1]["content"], "第一轮回答")
        self.assertIn("/undo", build_slash_commands(agent))
        self.assertEqual(options["/undo"]["insert"], "/undo")

    def test_resume_session_keeps_compact_summary_and_complete_recent_turns(self) -> None:
        agent = object.__new__(LocalToolAgent)
        agent.config = SimpleNamespace(max_history_turns=2)
        messages = [
            {"role": "assistant", "content": "会话压缩摘要：\n早期摘要"},
            {"role": "user", "content": "第二轮问题"},
            {"role": "assistant", "content": "第二轮回答"},
            {"role": "user", "content": "第三轮问题"},
            {"role": "assistant", "content": "第三轮回答"},
        ]

        restored = restore_history_window(messages, max_history_turns=agent.config.max_history_turns)

        self.assertEqual(
            restored,
            [
                {"role": "assistant", "content": "会话压缩摘要：\n早期摘要"},
                {"role": "user", "content": "第二轮问题"},
                {"role": "assistant", "content": "第二轮回答"},
                {"role": "user", "content": "第三轮问题"},
                {"role": "assistant", "content": "第三轮回答"},
            ],
        )

    def test_run_stream_appends_prompt_history_and_prompt_history_texts_are_reusable(self) -> None:
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
                on_stream_rollback=None,
            ):
                return AgentModelReply(message={"role": "assistant", "content": "完成"}, content="完成")

            agent._request_agent_reply = fake_request  # type: ignore[method-assign]

            LocalToolAgent.run_stream(agent, "查看会话历史", lambda _delta: None)

            persisted = store.search_prompt_history(workspace_root=workspace)
            texts = LocalToolAgent.prompt_history_texts(agent, limit=10)

        self.assertEqual([entry.display for entry in persisted], ["查看会话历史"])
        self.assertEqual(texts, ["查看会话历史"])

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
            "demo.read_file": ToolDefinition(
                name="demo.read_file",
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
        self.assertEqual(len(tools), 2)
        self.assertEqual(tools[0]["type"], "function")
        search_function = tools[0]["function"]
        invoke_function = tools[1]["function"]
        for function in (search_function, invoke_function):
            self.assertRegex(function["name"], r"^[A-Za-z0-9_-]{1,64}$")
            self.assertNotIn(".", function["name"])
            self.assertNotIn(":", function["name"])
            self.assertEqual(function["parameters"]["type"], "object")
        self.assertIn("search_tools", search_function["description"])
        self.assertEqual(search_function["parameters"]["required"], ["query"])
        self.assertIn("invoke_tool", invoke_function["description"])
        self.assertEqual(
            invoke_function["parameters"]["required"],
            ["tool_name", "arguments"],
        )

    def test_reasoning_deltas_are_forwarded_to_optional_callback(self) -> None:
        class FakeChatCompletions:
            def create(self, **_kwargs):
                return [
                    SimpleNamespace(
                        choices=[
                            SimpleNamespace(
                                delta=SimpleNamespace(
                                    content=None,
                                    reasoning_content="先检查配置。",
                                    tool_calls=None,
                                )
                            )
                        ]
                    ),
                    SimpleNamespace(
                        choices=[
                            SimpleNamespace(
                                delta=SimpleNamespace(
                                    content="完成",
                                    reasoning_content=None,
                                    tool_calls=None,
                                )
                            )
                        ]
                    ),
                ]

        agent = object.__new__(LocalToolAgent)
        agent.workspace_root = Path("D:/workspace/project")
        agent.config = SimpleNamespace(
            request_timeout_seconds=180,
            llm=SimpleNamespace(model="test-model", thinking_enabled=True, reasoning_effort="max"),
        )
        agent._client = SimpleNamespace(
            chat=SimpleNamespace(completions=FakeChatCompletions())
        )
        agent._system_prompt = lambda: "system prompt"  # type: ignore[method-assign]
        agent._build_extra_body = lambda: {}  # type: ignore[method-assign]
        agent._tools = {}
        agent._reasoning_delta_callback = None
        reasoning_deltas: list[str] = []

        LocalToolAgent._request_agent_reply_once(
            agent,
            [{"role": "user", "content": "检查配置"}],
            lambda _delta: None,
            lambda _input_tokens, _output_tokens, _cached_input_tokens: None,
            lambda: None,
            on_reasoning_delta=reasoning_deltas.append,
        )

        self.assertEqual(reasoning_deltas, ["先检查配置。"])

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
        function_name = function_name_for_tool("read_file")
        tool_call = ToolCall(
            name="read_file",
            arguments={"path": "README.md"},
            id="call_1",
            function_name=function_name,
        )

        message = assistant_tool_call_message(
            {},
            "",
            [tool_call],
            "内部推理不应回传。",
            function_name_for_tool=function_name_for_tool,
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

    def test_gpt_prompt_cache_key_changes_when_stable_context_changes(self) -> None:
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

        with tempfile.TemporaryDirectory() as temp_dir:
            workspace = Path(temp_dir)
            agents_file = workspace / "AGENTS.md"
            agents_file.write_text("# AGENTS.md\n\n第一版规范。", encoding="utf-8")
            completions = FakeChatCompletions()
            agent = object.__new__(LocalToolAgent)
            agent.workspace_root = workspace
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
            agent._tools = {}

            LocalToolAgent._request_agent_reply_once(
                agent,
                [{"role": "user", "content": "第一轮问题"}],
                lambda _delta: None,
                lambda _input_tokens, _output_tokens, _cached_input_tokens: None,
                lambda: None,
            )
            agents_file.write_text("# AGENTS.md\n\n第二版规范。", encoding="utf-8")
            LocalToolAgent._request_agent_reply_once(
                agent,
                [{"role": "user", "content": "第二轮问题"}],
                lambda _delta: None,
                lambda _input_tokens, _output_tokens, _cached_input_tokens: None,
                lambda: None,
            )

        self.assertIn("prompt_cache_key", completions.calls[0])
        self.assertNotEqual(completions.calls[0]["prompt_cache_key"], completions.calls[1]["prompt_cache_key"])

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

    def test_runtime_environment_is_context_message_not_system_prompt(self) -> None:
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
            agent._temp_workspace = SimpleNamespace(display_path=".agent_tmp")
            agent._system_prompt_template = LocalToolAgent._load_system_prompt_template(agent)

            prompt = LocalToolAgent._system_prompt(agent)
            messages = LocalToolAgent._context_messages(agent)

        self.assertNotIn("工作区检测：从启动目录发现 .git", prompt)
        self.assertIn("工作区检测：从启动目录发现 .git", messages[-1]["content"])

    def test_system_prompt_rejects_dynamic_placeholders(self) -> None:
        with self.assertRaisesRegex(ValueError, "动态占位符"):
            build_system_prompt("工作区：{workspace_root}")

    def test_system_prompt_constrains_repeated_exploration_and_shell_mixing(self) -> None:
        agent = object.__new__(LocalToolAgent)
        agent._system_prompt_template = LocalToolAgent._load_system_prompt_template(agent)

        prompt = LocalToolAgent._system_prompt(agent)

        self.assertIn("证据已经足够", prompt)
        self.assertIn("不要重复执行相同的读取、搜索或验证命令", prompt)
        self.assertIn("Bash 工具中不得使用 PowerShell 语法", prompt)
        self.assertIn("diagnostic_command", prompt)
        self.assertIn("不要只更换角色重复调用", prompt)

    def test_system_prompt_describes_memory_operating_protocol(self) -> None:
        agent = object.__new__(LocalToolAgent)
        agent._system_prompt_template = LocalToolAgent._load_system_prompt_template(agent)

        prompt = LocalToolAgent._system_prompt(agent)

        self.assertIn("记忆不会自动注入当前上下文", prompt)
        self.assertIn("先搜索摘要，再按需读取全文", prompt)
        self.assertIn("必须提供具体 `query` 和说明检索目的的 `reason`", prompt)
        self.assertIn("project_memory_write", prompt)
        self.assertIn("session_memory_write", prompt)
        self.assertIn("user_memory_write", prompt)
        self.assertIn("任何作用域都禁止写入密码、API Key、Token、Cookie", prompt)

    def test_active_skill_body_is_context_message_not_system_prompt(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            workspace = Path(temp_dir)
            skill_path = workspace / ".claude" / "skills" / "demo" / "SKILL.md"
            meta = SkillMeta(
                name="demo-skill",
                description="Demo skill description",
                source_path=skill_path,
                base_dir=skill_path.parent,
                scope="project",
            )
            skill = Skill(meta=meta, body="Demo skill body should stay out of system.")
            agent = object.__new__(LocalToolAgent)
            agent.workspace_root = workspace
            agent.config = SimpleNamespace(workspace_detection_summary="")
            agent._tools = {}
            agent._memory_store = None
            agent._skill_manager = None
            agent._active_skills = [
                SkillMatchResult(skill=skill, score=1.0, reason="手动调用：demo-skill")
            ]
            agent._temp_workspace = SimpleNamespace(display_path=".agent_tmp")
            agent._system_prompt_template = LocalToolAgent._load_system_prompt_template(agent)

            prompt = LocalToolAgent._system_prompt(agent)
            messages = LocalToolAgent._context_messages(agent)

        self.assertNotIn("Demo skill description", prompt)
        self.assertNotIn("Demo skill body should stay out of system.", prompt)
        skill_context = messages[0]["content"]
        self.assertIn('<active_skill_instructions source="skill-registry"', skill_context)
        self.assertIn("Demo skill description", skill_context)
        self.assertIn("Demo skill body should stay out of system.", skill_context)

    def test_subagent_task_slash_commands_expose_only_safe_task_fields(self) -> None:
        class FakeAgent:
            skill_manager = None

            def __init__(self) -> None:
                self.cancelled: list[str] = []
                self.task = {
                    "task_id": "task-current",
                    "agent_type": "explore",
                    "description": "检查当前会话",
                    "status": "completed",
                    "result": {
                        "summary": "安全摘要",
                        "artifacts": [{"artifact_path": "artifacts/task.json"}],
                    },
                    "error": None,
                    "prompt": "完整任务 prompt，不应显示",
                }

            def list_subagent_tasks(self):
                return [dict(self.task)]

            def get_subagent_task(self, task_id: str):
                return dict(self.task) if task_id == self.task["task_id"] else None

            def cancel_subagent_task(self, task_id: str):
                if task_id != self.task["task_id"]:
                    return {"ok": False}
                self.cancelled.append(task_id)
                return {"ok": True, "status": "cancelling"}

        agent = FakeAgent()
        listed = handle_subagent_task_command(agent, "/tasks")
        detail = handle_subagent_task_command(agent, "/task task-current")
        cancelled = handle_subagent_task_command(agent, "/task cancel task-current")
        missing = handle_subagent_task_command(agent, "/task task-missing")

        self.assertIn("task-current", listed or "")
        self.assertIn("/task cancel", listed or "")
        self.assertIn("安全摘要", detail or "")
        self.assertIn("关联 artifact：1 项", detail or "")
        self.assertNotIn("完整任务 prompt", detail or "")
        self.assertEqual(cancelled, "已请求取消后台子任务：task-current。")
        self.assertEqual(agent.cancelled, ["task-current"])
        self.assertEqual(missing, "未找到当前会话的 SubAgent 任务。")
        self.assertIn("/tasks", build_slash_commands(agent))
        options = {item["command"]: item for item in build_slash_command_options(agent)}
        self.assertEqual(options["/task"]["insert"], "/task ")


if __name__ == "__main__":
    unittest.main()
