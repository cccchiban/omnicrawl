from __future__ import annotations

import json
import subprocess
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
from omnicrawl.agent.session.history import restore_history_window
from omnicrawl.agent.runtime.llm_protocol import assistant_tool_call_message, function_name_for_tool
from omnicrawl.agent.context.prompt_context import build_system_prompt
from omnicrawl.config.features.context_compaction import ContextCompactionConfig
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
                display_path = ".omnicrawl/.agent_tmp"

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
                self.assertIn("update_todos", agent._tools)

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
            agent._temp_workspace = SimpleNamespace(display_path=".omnicrawl/.agent_tmp")
            agent._tools = {
                "read": ToolDefinition(
                    name="read",
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
        self.assertIn("顶层 tools 已注册", sent_messages[1]["content"])
        self.assertIn("原生调用", sent_messages[1]["content"])
        self.assertIn("call_id", sent_messages[1]["content"])
        self.assertNotIn("invoke_tool", sent_messages[1]["content"])
        self.assertNotIn("search_tools", sent_messages[1]["content"])
        self.assertNotIn("read", sent_messages[1]["content"])
        self.assertIn('<runtime_context source="host-runtime"', sent_messages[2]["content"])
        self.assertIn("工作区检测：从启动目录发现 .git", sent_messages[2]["content"])
        self.assertEqual(sent_messages[3]["content"], "上一轮问题")
        self.assertEqual(sent_messages[4]["content"], "上一轮回答")
        self.assertEqual(sent_messages[-1]["content"], "请检查项目状态")
        self.assertEqual(agent._history[-2]["content"], "请检查项目状态")
        self.assertNotIn("project_instructions", agent._history[-2]["content"])

    def test_tts_enabled_no_longer_injects_speak_instruction(self) -> None:
        """TTS 启用不再注入要求模型调用 tts_synthesize 的 system 指令。

        自动朗读已改为主 TUI 程序侧播报：模型只负责输出文字，不感知 TTS，
        system prompt 保持纯净。
        """
        with tempfile.TemporaryDirectory() as temp_dir:
            workspace = Path(temp_dir)
            agent = object.__new__(LocalToolAgent)
            agent.workspace_root = workspace
            agent._system_prompt_template = "基础系统提示词"
            agent.config = SimpleNamespace(
                max_history_turns=6,
                tts=SimpleNamespace(
                    enabled=True,
                    resolved_model_dir=lambda: workspace / "models",
                ),
            )
            agent._history = []
            agent._session_store = None
            agent._session_state = None
            agent._skill_manager = None
            agent._active_skills = []
            agent._tools = {
                "tts_synthesize": ToolDefinition(
                    name="tts_synthesize",
                    description="TTS",
                    argument_schema="{}",
                    requires_confirmation=False,
                    run=lambda _arguments: ToolResult(ok=True, output="{}"),
                )
            }
            captured_messages: list[list[dict[str, str]]] = []
            captured_system_prompts: list[str] = []

            def fake_request(
                messages,
                _on_delta,
                _on_token_usage,
                _on_protocol_wait,
                _on_retry_status,
                on_stream_rollback=None,
            ):
                captured_messages.append(messages)
                captured_system_prompts.append(LocalToolAgent._system_prompt(agent))
                return AgentModelReply(
                    message={"role": "assistant", "content": "完成"},
                    content="完成",
                )

            agent._request_agent_reply = fake_request  # type: ignore[method-assign]

            with patch("omnicrawl.tts.models_ready", return_value=True):
                result = LocalToolAgent.run_stream(agent, "请朗读这段文字", lambda _delta: None)

        sent_messages = captured_messages[0]
        self.assertEqual(result, "完成")
        # system prompt 不再携带任何 TTS 说话指令；用户消息原样透传。
        self.assertEqual(sent_messages[-1]["content"], "请朗读这段文字")
        self.assertEqual(len(captured_system_prompts), 1)
        self.assertNotIn("tts_instruction", captured_system_prompts[0])
        self.assertNotIn("tts_synthesize", captured_system_prompts[0])
        # 模型历史不受影响。
        self.assertEqual(agent._history[-1]["content"], "完成")
        self.assertEqual(agent._history[-2]["content"], "请朗读这段文字")

    def test_run_stream_keeps_tts_tool_but_no_prompt_injection(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            workspace = Path(temp_dir)

            def run_turn(
                *,
                tts_enabled: bool,
                has_tool: bool,
                models_ready_value: bool,
            ) -> tuple[str, str]:
                agent = object.__new__(LocalToolAgent)
                agent.workspace_root = workspace
                agent._system_prompt_template = "基础系统提示词"
                agent.config = SimpleNamespace(
                    max_history_turns=6,
                    tts=SimpleNamespace(
                        enabled=tts_enabled,
                        resolved_model_dir=lambda: workspace / "models",
                    ),
                )
                agent._history = []
                agent._session_store = None
                agent._session_state = None
                agent._skill_manager = None
                agent._active_skills = []
                agent._tools = (
                    {
                        "tts_synthesize": ToolDefinition(
                            name="tts_synthesize",
                            description="TTS",
                            argument_schema="{}",
                            requires_confirmation=False,
                            run=lambda _arguments: ToolResult(ok=True, output="{}"),
                        )
                    }
                    if has_tool
                    else {}
                )
                captured_messages: list[list[dict[str, str]]] = []
                captured_system_prompts: list[str] = []

                def fake_request(
                    messages,
                    _on_delta,
                    _on_token_usage,
                    _on_protocol_wait,
                    _on_retry_status,
                    on_stream_rollback=None,
                ):
                    captured_messages.append(messages)
                    captured_system_prompts.append(LocalToolAgent._system_prompt(agent))
                    return AgentModelReply(
                        message={"role": "assistant", "content": "完成"},
                        content="完成",
                    )

                agent._request_agent_reply = fake_request  # type: ignore[method-assign]
                with patch("omnicrawl.tts.models_ready", return_value=models_ready_value):
                    LocalToolAgent.run_stream(agent, "读一下这句话", lambda _delta: None)
                return str(captured_messages[0][-1]["content"]), captured_system_prompts[0]

            with self.subTest("tts 未启用"):
                last_content, system_prompt = run_turn(
                    tts_enabled=False, has_tool=True, models_ready_value=True
                )
                self.assertEqual(last_content, "读一下这句话")
                self.assertNotIn("tts_instruction", system_prompt)

            with self.subTest("工具被开关禁用"):
                last_content, system_prompt = run_turn(
                    tts_enabled=True, has_tool=False, models_ready_value=True
                )
                self.assertEqual(last_content, "读一下这句话")
                self.assertNotIn("tts_instruction", system_prompt)

            with self.subTest("模型未就绪"):
                last_content, system_prompt = run_turn(
                    tts_enabled=True, has_tool=True, models_ready_value=False
                )
                self.assertEqual(last_content, "读一下这句话")
                self.assertNotIn("tts_instruction", system_prompt)

            with self.subTest("一切就绪仍不注入"):
                last_content, system_prompt = run_turn(
                    tts_enabled=True, has_tool=True, models_ready_value=True
                )
                self.assertEqual(last_content, "读一下这句话")
                self.assertNotIn("tts_instruction", system_prompt)

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

            with patch("omnicrawl.agent.controllers.turn.loop.global_agents_path", return_value=global_agents):
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
                "omnicrawl.agent.controllers.turn.loop.global_agents_path",
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
            agent.config = SimpleNamespace(max_history_turns=6)
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
            agent.config = SimpleNamespace(max_history_turns=6)
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
                "read": ToolDefinition("read", "read", "{}", False, concurrent_tool("read")),
                "grep": ToolDefinition("grep", "search", "{}", False, concurrent_tool("search", ok=False)),
            }
            calls = [
                ToolCall("read", {}, "call_1", function_name_for_tool("read")),
                ToolCall("grep", {}, "call_2", function_name_for_tool("grep")),
            ]
            replies = iter([
                AgentModelReply(
                    message=assistant_tool_call_message("", calls, "", function_name_for_tool=function_name_for_tool),
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

    def test_run_stream_executes_write_and_delete_calls_concurrently(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            agent = object.__new__(LocalToolAgent)
            agent.workspace_root = Path(temp_dir)
            agent.config = SimpleNamespace(max_history_turns=6)
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
                "read": ToolDefinition("read", "read", "{}", False, run_named("read")),
                "write_file": ToolDefinition("write_file", "write", "{}", False, run_named("write")),
                "bash": ToolDefinition("bash", "bash", "{}", False, run_named("delete")),
            }
            calls = [
                ToolCall("read", {}, "call_1"),
                ToolCall("write_file", {"path": "a"}, "call_2"),
                ToolCall("bash", {"command": "rm a"}, "call_3"),
            ]
            replies = iter([
                AgentModelReply({"role": "assistant", "content": None}, "", calls),
                AgentModelReply({"role": "assistant", "content": "完成"}, "完成"),
            ])
            agent._request_agent_reply = lambda *_args, **_kwargs: next(replies)  # type: ignore[method-assign]

            LocalToolAgent.run_stream(agent, "执行", lambda _delta: None)

        # 即使写入/删除类工具也与同批次其它调用并发启动；模型观察仍按
        # 调用顺序回填，但实际完成顺序不再被类型屏障强制串行化。
        self.assertTrue(overlap)
        self.assertCountEqual(execution, ["read", "write", "delete"])

    def test_run_stream_writes_session_events(self) -> None:
        """纯对话轮次只写基础会话事件，不产生 turn_snapshot（惰性快照）。

        纯对话/只读轮无工作区副作用，不需要 begin/end diff 快照；/undo 走
        “无快照且无副作用”安全逻辑路径即可。
        """

        with tempfile.TemporaryDirectory() as temp_dir:
            workspace = Path(temp_dir)
            # 初始化为 Git 仓库，保证若执行写工具则事务式轮次快照生效。
            subprocess.run(["git", "init", "-q"], cwd=workspace, check=True)
            subprocess.run(["git", "config", "user.name", "OmniCrawl Test"], cwd=workspace, check=True)
            subprocess.run(["git", "config", "user.email", "test@example.invalid"], cwd=workspace, check=True)
            subprocess.run(["git", "commit", "-qm", "init", "--allow-empty"], cwd=workspace, check=True)
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
            ["session_started", "user_message", "assistant_message"],
        )
        self.assertEqual(restored.messages[-2]["content"], "记录会话")
        self.assertEqual(restored.messages[-1]["content"], "完成")

    def test_run_stream_write_turn_records_turn_snapshot(self) -> None:
        """含可回退写工具的轮次才产生 version 2 turn_snapshot 事件。"""

        with tempfile.TemporaryDirectory() as temp_dir:
            workspace = Path(temp_dir)
            subprocess.run(["git", "init", "-q"], cwd=workspace, check=True)
            subprocess.run(["git", "config", "user.name", "OmniCrawl Test"], cwd=workspace, check=True)
            subprocess.run(["git", "config", "user.email", "test@example.invalid"], cwd=workspace, check=True)
            subprocess.run(["git", "commit", "-qm", "init", "--allow-empty"], cwd=workspace, check=True)
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

            def write_file(_arguments: dict[str, object]) -> ToolResult:
                (workspace / "note.txt").write_text("hello\n", encoding="utf-8")
                return ToolResult(ok=True, output="written")

            agent._tools = {
                "write_file": ToolDefinition(
                    "write_file", "write", "{}", False, write_file
                ),
            }
            write_call = ToolCall(
                "write_file",
                {"path": str(workspace / "note.txt"), "content": "hello\n"},
                "call_write",
                function_name_for_tool("write_file"),
            )
            replies = iter(
                [
                    AgentModelReply(
                        message=assistant_tool_call_message(
                            "",
                            [write_call],
                            "",
                            function_name_for_tool=function_name_for_tool,
                        ),
                        content="",
                        tool_calls=[write_call],
                    ),
                    AgentModelReply(
                        {"role": "assistant", "content": "完成"}, "完成"
                    ),
                ]
            )
            agent._request_agent_reply = lambda messages, *_args, **_kwargs: next(replies)  # type: ignore[method-assign]

            LocalToolAgent.run_stream(agent, "写文件", lambda _delta: None)

            events = [
                json.loads(line)
                for line in state.path.read_text(encoding="utf-8").splitlines()
                if line.strip()
            ]

        self.assertEqual(events[-1]["type"], "turn_snapshot")
        self.assertEqual(events[-1]["payload"]["version"], 2)
        self.assertEqual(events[-1]["payload"]["begin_patch"], "undo/begin.patch")
        self.assertEqual(events[-1]["payload"]["end_patch"], "undo/end.patch")
        self.assertIn("workspace", events[-1]["payload"])
        self.assertIn("write_file", events[-1]["payload"]["executed_tools"])

    def test_run_stream_persists_full_tool_output_as_session_artifact(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            workspace = Path(temp_dir)
            agent = object.__new__(LocalToolAgent)
            agent.workspace_root = workspace
            agent.config = SimpleNamespace(max_history_turns=6)
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
        # 10000 字符 < 50K 单工具阈值：模型看到完整输出，不再被 6000 截断。
        self.assertEqual(tool_payload["model_output"], long_output)
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

    def test_current_session_messages_returns_full_restored_transcript(self) -> None:
        """UI 重放应拿到恢复后的完整会话消息，而不受历史窗口裁剪。"""

        with tempfile.TemporaryDirectory() as temp_dir:
            workspace = Path(temp_dir)
            store = SessionStore(workspace / ".agent_sessions")
            state = store.start_session(workspace)
            for content in ("旧问题一", "旧回答一", "旧问题二", "旧回答二"):
                role = "user_message" if content.startswith("旧问题") else "assistant_message"
                store.append_event(state.session_id, role, {"content": content})

            agent = object.__new__(LocalToolAgent)
            agent.workspace_root = workspace
            agent.config = SimpleNamespace(max_history_turns=1)
            agent._session_store = store
            agent._session_state = None
            agent._history = []

            LocalToolAgent.resume_session(agent, state.session_id)
            projected = LocalToolAgent.current_session_messages(agent)

        # max_history_turns=1 只让 `_history` 保留 2 条，但 UI 重放必须看到全部 4 条。
        self.assertEqual(len(agent._history), 2)
        self.assertEqual(
            [message["content"] for message in projected],
            ["旧问题一", "旧回答一", "旧问题二", "旧回答二"],
        )

    def test_current_session_messages_empty_without_session(self) -> None:
        agent = object.__new__(LocalToolAgent)
        agent._session_state = None
        self.assertEqual(LocalToolAgent.current_session_messages(agent), [])

    def test_update_todos_normalizes_payload_and_notifies_observer(self) -> None:
        agent = object.__new__(LocalToolAgent)
        received: list[dict[str, Any]] = []
        agent._todo_update_callback = received.append

        result = LocalToolAgent._tool_update_todos(
            agent,
            {
                "todos": [
                    {"id": "a", "step": "  第一步  ", "completed": True},
                    {"step": "", "completed": False},
                    {"title": "用 title 兜底", "status": "done"},
                    {"step": "纯文本步骤"},
                ]
            },
        )

        self.assertTrue(result.ok)
        self.assertEqual(received, [{"todos": [
            {"id": "a", "step": "第一步", "completed": True},
            {"id": "3", "step": "用 title 兜底", "completed": True},
            {"id": "4", "step": "纯文本步骤", "completed": False},
        ]}])
        self.assertEqual(len(json.loads(result.output)["todos"]), 3)

    def test_update_todos_requires_list_and_survives_observer_failure(self) -> None:
        agent = object.__new__(LocalToolAgent)

        def failing_observer(_payload: dict[str, Any]) -> None:
            raise RuntimeError("UI observer 崩溃")

        agent._todo_update_callback = failing_observer

        invalid = LocalToolAgent._tool_update_todos(agent, {"todos": "不是数组"})
        valid = LocalToolAgent._tool_update_todos(
            agent,
            {"todos": [{"step": "仍能通知", "completed": False}]},
        )

        self.assertFalse(invalid.ok)
        self.assertTrue(valid.ok)
        self.assertEqual(json.loads(valid.output)["updated"], 1)

    def test_update_todos_truncates_to_twenty_items(self) -> None:
        agent = object.__new__(LocalToolAgent)
        agent._todo_update_callback = lambda _payload: None

        result = LocalToolAgent._tool_update_todos(
            agent,
            {"todos": [{"step": f"步骤 {index}", "completed": False} for index in range(30)]},
        )

        self.assertTrue(result.ok)
        self.assertEqual(len(json.loads(result.output)["todos"]), 20)

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

    def test_resume_archived_session_allows_other_workspace_before_unarchive(self) -> None:
        """会话已解除工作区绑定：可以跨工作区恢复已归档会话。"""
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

            restored = LocalToolAgent.resume_session(agent, state.session_id)
            self.assertEqual(restored.session_id, state.session_id)
            self.assertIsNone(restored.archived_at)
            self.assertTrue(agent._history)

            still_archived = store.load_session(state.session_id)

        # 恢复成功后会取消归档，与会话解除工作区绑定后的跨工作区恢复语义一致。
        self.assertIsNone(still_archived.archived_at)

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
        # 会话已解除工作区绑定：默认列表返回全部工作区会话。
        self.assertEqual(
            {entry.session_id for entry in current_sessions},
            {state.session_id, other_state.session_id},
        )

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
            # 初始化为 Git 仓库，保证轮次快照生效（取消摘要依赖已执行工具）。
            subprocess.run(["git", "init", "-q"], cwd=workspace, check=True)
            subprocess.run(
                ["git", "config", "user.name", "OmniCrawl Test"], cwd=workspace, check=True
            )
            subprocess.run(
                ["git", "config", "user.email", "test@example.invalid"], cwd=workspace, check=True
            )
            subprocess.run(
                ["git", "commit", "-qm", "init", "--allow-empty"], cwd=workspace, check=True
            )
            agent = object.__new__(LocalToolAgent)
            agent.workspace_root = workspace
            agent.config = SimpleNamespace(
                max_history_turns=6,
            )
            agent._history = []
            agent._skill_manager = None
            agent._active_skills = []
            agent._tools = {
                "read": ToolDefinition(
                    "read",
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
                    "read",
                    {"path": "a.txt"},
                    "call_1",
                    function_name_for_tool("read"),
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
                        "（上一回合被取消，未生成最终回复）已执行工具：read",
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
            pass

        snapshot = _ActiveTurnSnapshot(
            snapshot_id="s1",
            store=FakeStore(),  # type: ignore[arg-type]
            workspace=Path("."),
            before=None,  # type: ignore[arg-type]
            executed_tools=["read", "grep", "read"],
        )
        summary = LocalToolAgent._cancelled_turn_summary(snapshot)
        self.assertEqual(
            summary,
            "（上一回合被取消，未生成最终回复）已执行工具：read×2，grep",
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
                    workspace=Path("."),
                    before=None,  # type: ignore[arg-type]
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
        # 重试期间只在状态行显示第几次重试，不暴露失败详情；
        # 失败详情随最终错误在全部重试结束后一并提示。
        self.assertEqual(retry_statuses[0], "正在重试(第1次)")
        self.assertNotIn("模型服务连接提前断开", retry_statuses[0])

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
        self.assertEqual(len(tools), 1)
        self.assertEqual(tools[0]["type"], "function")
        function = tools[0]["function"]
        self.assertRegex(function["name"], r"^[A-Za-z0-9_-]{1,64}$")
        self.assertNotIn(".", function["name"])
        self.assertNotIn(":", function["name"])
        self.assertEqual(function["parameters"]["type"], "object")
        # 顶层注册真实工具：描述与参数契约来自工具目录（压缩后）。
        self.assertIn("读取文件", function["description"])
        self.assertEqual(function["parameters"]["required"], ["path"])

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
            "read": ToolDefinition(
                name="read",
                description="读取文件。",
                argument_schema='{"path":"README.md"}',
                requires_confirmation=False,
                run=lambda _arguments: ToolResult(ok=True, output="ok"),
            )
        }
        function_name = function_name_for_tool("read")
        tool_call = ToolCall(
            name="read",
            arguments={"path": "README.md"},
            id="call_1",
            function_name=function_name,
        )

        message = assistant_tool_call_message(
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

        # AGENTS.md 正文只进入 context 消息；系统提示词保留文档索引标识。
        self.assertIn("omnicrawl://docs/", prompt)
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
            agent._temp_workspace = SimpleNamespace(display_path=".omnicrawl/.agent_tmp")
            agent._system_prompt_template = LocalToolAgent._load_system_prompt_template(agent)

            prompt = LocalToolAgent._system_prompt(agent)
            messages = LocalToolAgent._context_messages(agent)

        self.assertNotIn("工作区检测：从启动目录发现 .git", prompt)
        self.assertIn("工作区检测：从启动目录发现 .git", messages[-1]["content"])

    def test_system_prompt_rejects_dynamic_placeholders(self) -> None:
        with self.assertRaisesRegex(ValueError, "动态占位符"):
            build_system_prompt("工作区：{workspace_root}")

    def test_ask_user_tool_definition_uses_structured_question_protocol(self) -> None:
        agent = object.__new__(LocalToolAgent)
        agent._tools = {
            "ask_user": ToolDefinition(
                name="ask_user",
                description="通过工具向用户提问。",
                argument_schema=json.dumps(
                    {
                        "type": "object",
                        "properties": {
                            "kind": {"enum": ["question", "select", "confirm"]},
                            "question": {"type": "string"},
                            "options": {
                                "type": "array",
                                "minItems": 1,
                                "items": {"type": "string"},
                            },
                        },
                        "required": ["kind", "question", "options"],
                    },
                    ensure_ascii=False,
                ),
                requires_confirmation=False,
                run=lambda _arguments: ToolResult(ok=True, output="ok"),
            )
        }
        definition = agent._tools["ask_user"]
        schema = json.loads(definition.argument_schema)
        self.assertEqual(
            schema["properties"]["kind"]["enum"],
            ["question", "select", "confirm"],
        )
        self.assertEqual(schema["required"], ["kind", "question", "options"])
        self.assertEqual(schema["properties"]["options"]["minItems"], 1)
        self.assertNotIn("[选项]", definition.description)
        self.assertNotIn("[需要用户确认]", definition.description)

    def test_ask_user_tool_validates_arguments_and_returns_answer(self) -> None:
        agent = object.__new__(LocalToolAgent)
        requests = []
        agent._ask_user_handler = lambda request: requests.append(request) or "方案 B"

        missing_options = LocalToolAgent._tool_ask_user(
            agent,
            {"kind": "select", "question": "选择"},
        )
        empty_options = LocalToolAgent._tool_ask_user(
            agent,
            {"kind": "select", "question": "选择", "options": []},
        )
        no_options_question = LocalToolAgent._tool_ask_user(
            agent,
            {"kind": "question", "question": "请补充"},
        )
        valid = LocalToolAgent._tool_ask_user(
            agent,
            {"kind": "select", "question": "选择", "options": ["方案 A", "方案 B"]},
        )

        self.assertFalse(missing_options.ok)
        self.assertFalse(empty_options.ok)
        self.assertFalse(no_options_question.ok)
        self.assertTrue(valid.ok)
        self.assertEqual(requests[0].kind, "select")
        self.assertEqual(json.loads(valid.output)["answer"], "方案 B")

    def test_system_prompt_constrains_repeated_exploration_and_shell_mixing(self) -> None:
        agent = object.__new__(LocalToolAgent)
        agent._system_prompt_template = LocalToolAgent._load_system_prompt_template(agent)

        prompt = LocalToolAgent._system_prompt(agent)

        # 系统提示词为中文精简版；shell 混用约束与诊断工具指引仍保留，
        # 完整工具调用协议（低成本读取、失败换策略等）改为按需读取文档。
        self.assertIn("never mix syntax", prompt)
        self.assertIn("diagnostic_command", prompt)
        self.assertIn("pipefail", prompt)
        self.assertIn("TOOL_CALLING.md", prompt)
        self.assertIn("提问与选项交互协议", prompt)
        self.assertIn("ask_user", prompt)
        self.assertIn("kind=question", prompt)
        self.assertIn("kind=select", prompt)
        self.assertIn("kind=confirm", prompt)
        self.assertNotIn("[选项]", prompt)
        self.assertNotIn("[/选项]", prompt)
        self.assertNotIn("[需要用户确认]", prompt)

    def test_system_prompt_describes_memory_operating_protocol(self) -> None:
        agent = object.__new__(LocalToolAgent)
        agent._system_prompt_template = LocalToolAgent._load_system_prompt_template(agent)

        prompt = LocalToolAgent._system_prompt(agent)

        # 系统提示词为中文精简版；断言仍保留的记忆协议要点。
        self.assertIn("不要读取全部记忆", prompt)
        self.assertIn("Search summaries first", prompt)
        self.assertIn("related_directories", prompt)
        self.assertIn("project_memory", prompt)
        self.assertIn("session_memory", prompt)
        self.assertIn("user_memory", prompt)
        self.assertIn("Credentials are forbidden in every scope", prompt)

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
            agent._temp_workspace = SimpleNamespace(display_path=".omnicrawl/.agent_tmp")
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

    def test_update_todos_sends_safe_plan_projection_to_observer(self) -> None:
        agent = object.__new__(LocalToolAgent)
        observed: list[dict[str, object]] = []
        agent._todo_update_callback = observed.append

        result = agent._tool_update_todos(
            {
                "todos": [
                    {"id": "inspect", "step": "检查 Agent 流程", "completed": True},
                    {"step": "接入 TUI 计划区", "completed": False},
                    {"step": "", "completed": False},
                ]
            }
        )

        self.assertTrue(result.ok)
        self.assertEqual(
            observed,
            [
                {
                    "todos": [
                        {"id": "inspect", "step": "检查 Agent 流程", "completed": True},
                        {"id": "2", "step": "接入 TUI 计划区", "completed": False},
                    ]
                }
            ],
        )
        self.assertIn('"updated": 2', result.output)

    def test_update_todos_empty_list_clears_plan(self) -> None:
        agent = object.__new__(LocalToolAgent)
        observed: list[dict[str, object]] = []
        agent._todo_update_callback = observed.append

        result = agent._tool_update_todos({"todos": []})

        self.assertTrue(result.ok)
        self.assertEqual(observed, [{"todos": []}])


if __name__ == "__main__":
    unittest.main()
