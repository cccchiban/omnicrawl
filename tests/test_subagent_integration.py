from __future__ import annotations

import json
import tempfile
import threading
import time
import unittest
from concurrent.futures import ThreadPoolExecutor
from typing import Callable, Iterator
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import Mock, patch

from omnicrawl.agent import (
    AgentConfig,
    AgentError,
    AgentModelReply,
    LocalToolAgent,
    ToolCall,
    ToolDefinition,
    ToolResult,
)
from omnicrawl.agent.subagents.coordinator import SubAgentExecutionResult
from omnicrawl.agent.subagents.execution import SubAgentExecutionContext
from omnicrawl.llm.capabilities import ModelCapabilities
from omnicrawl.llm.protocol import (
    PROTOCOL_ANTHROPIC_MESSAGES,
    PROTOCOL_GEMINI_GENERATE_CONTENT,
    PROTOCOL_OPENAI_CHAT_COMPLETIONS,
    PROTOCOL_OPENAI_RESPONSES,
    ModelIdentity,
    ModelStreamEvent,
    ModelTurnRequest,
    ReasoningDelta,
    ResponseCompleted,
    TextDelta,
    ToolCallCompleted,
    ToolCallStarted,
    ToolResultBlock,
    UsageUpdated,
)
from omnicrawl.llm.registry import ModelDescriptor, ProviderProfile
from omnicrawl.llm.runtime import ModelRuntimeManager
from omnicrawl.agent.subagents.definitions import AgentDefinition
from omnicrawl.api.app import create_default_agent
from omnicrawl.config.subagents import SubAgentConfig
from omnicrawl.mcp.config import MCPConfig
from omnicrawl.session import SessionStore
from omnicrawl.temp_workspace import AgentTempWorkspaceConfig


def _agent_config(
    workspace: Path,
    *,
    enabled: bool,
    enable_verify_agent: bool = False,
) -> AgentConfig:
    return AgentConfig(
        llm=SimpleNamespace(
            api_key="test-key",
            base_url="https://example.test/v1",
            model="test-model",
            context_window_tokens=128_000,
            request_retry_count=3,
        ),
        workspace_root=workspace,
        memory_enabled=False,
        session_enabled=False,
        skills_enabled=False,
        mcp_config=MCPConfig(enabled=False),
        subagents=SubAgentConfig(
            enabled=enabled,
            enable_verify_agent=enable_verify_agent,
        ),
        temp_workspace=AgentTempWorkspaceConfig(cleanup_enabled=False),
    )


_PROVIDER_RUNTIME_CONTRACTS = (
    ("openai", PROTOCOL_OPENAI_CHAT_COMPLETIONS),
    ("openai", PROTOCOL_OPENAI_RESPONSES),
    ("anthropic", PROTOCOL_ANTHROPIC_MESSAGES),
    ("gemini", PROTOCOL_GEMINI_GENERATE_CONTENT),
)


class _ProviderContractCancelled(RuntimeError):
    """测试用取消信号，必须穿过统一 Runtime 协议原样返回。"""


class _ProviderContractFakeRuntime:
    """模拟已完成 SDK 适配的 Provider Runtime，记录 SubAgent 协议输入。

    Fake Runtime 不复刻任一厂商 SDK 的事件对象；它只发出 Host 已归一化的
    ``ModelStreamEvent``。这样测试关注的是 SubAgent 是否能在四种 Provider
    身份下通过 RuntimeManager 保持 fresh 上下文、工具往返、用量和取消边界。
    """

    def __init__(
        self,
        identity: ModelIdentity,
        *,
        response_text: str,
        reasoning_text: str = "",
        emit_tool_on_first_turn: bool = False,
        cancel_signal: threading.Event | None = None,
    ) -> None:
        self.identity = identity
        self.capabilities = ModelCapabilities(streaming=True, tools=True)
        self.response_text = response_text
        self.reasoning_text = reasoning_text
        self.emit_tool_on_first_turn = emit_tool_on_first_turn
        self.cancel_signal = cancel_signal
        self.requests: list[ModelTurnRequest] = []
        self.cancel_checks = 0
        self.closed = False

    def stream_turn(
        self,
        request: ModelTurnRequest,
        *,
        cancel_check: Callable[[], None] | None = None,
    ) -> Iterator[ModelStreamEvent]:
        self.requests.append(request)
        if self.cancel_signal is not None:
            # 让第一次 AgentLoopRunner 的取消检查顺利通过，再在 Runtime 内触发
            # 取消，验证 Host 确实把同一个回调传递到了 Provider 边界。
            self.cancel_signal.set()
        if cancel_check is not None:
            self.cancel_checks += 1
            cancel_check()

        if self.emit_tool_on_first_turn and len(self.requests) in {1, 2}:
            if len(request.tools) != 2:
                raise AssertionError(
                    "工具往返契约要求子任务只收到 search_tools 和 invoke_tool。"
                )
            if len(self.requests) == 1:
                function_name = request.tools[0].name
                arguments = {"query": "read file", "limit": 4}
                call_id = "provider-call-1"
            else:
                function_name = request.tools[1].name
                arguments = {
                    "tool_name": "read_file",
                    "arguments": {"path": "README.md"},
                }
                call_id = "provider-call-2"
            yield ToolCallStarted(call_id=call_id, name=function_name)
            yield ToolCallCompleted(
                call_id=call_id,
                name=function_name,
                arguments=arguments,
            )
            yield UsageUpdated(input_tokens=7, output_tokens=3, cached_input_tokens=1)
            yield ResponseCompleted(finish_reason="tool_calls")
            return

        if self.reasoning_text:
            yield ReasoningDelta(text=self.reasoning_text)
        yield TextDelta(text=self.response_text)
        yield UsageUpdated(input_tokens=11, output_tokens=7, cached_input_tokens=3)
        yield ResponseCompleted(finish_reason="stop")

    def close(self) -> None:
        self.closed = True


class SubAgentToolIntegrationTest(unittest.TestCase):
    def test_default_disabled_does_not_change_tool_registry(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            with patch("openai.OpenAI", return_value=SimpleNamespace()):
                agent = LocalToolAgent(_agent_config(Path(temp_dir), enabled=False))
                try:
                    self.assertNotIn("subagent", agent._tools)
                    self.assertIsNone(agent._subagent_coordinator)
                finally:
                    agent.close()

    def test_enabled_registers_one_policy_managed_tool_and_builtin_definitions(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            with patch("openai.OpenAI", return_value=SimpleNamespace()):
                agent = LocalToolAgent(_agent_config(Path(temp_dir), enabled=True))
                try:
                    tool = agent._tools["subagent"]
                    schema = json.loads(tool.argument_schema)
                    self.assertFalse(tool.requires_confirmation)
                    self.assertEqual(
                        schema["properties"]["action"]["enum"],
                        [
                            "run",
                            "spawn",
                            "list",
                            "get",
                            "cancel",
                            "apply_worktree",
                            "discard_worktree",
                            "list_worktrees",
                        ],
                    )
                    self.assertEqual(schema["properties"]["tasks"]["maxItems"], 4)
                    task_properties = schema["properties"]["tasks"]["items"]["properties"]
                    self.assertNotIn("maxLength", task_properties["description"])
                    self.assertNotIn("maxLength", task_properties["prompt"])
                    model_schema = schema["properties"]["tasks"]["items"]["properties"]["model"]
                    self.assertIn("省略", model_schema["description"])
                    self.assertIn("default", model_schema["description"])
                    self.assertEqual(
                        schema["properties"]["tasks"]["items"]["properties"]
                        ["subagent_type"]["enum"],
                        ["explore", "plan"],
                    )
                    self.assertEqual(
                        schema["properties"]["max_concurrency"]["maximum"],
                        4,
                    )
                    self.assertIn("fail_fast", schema["properties"])
                    self.assertEqual(
                        {item.name for item in agent._subagent_registry.list_all()},
                        {"explore", "general-purpose", "plan", "verify"},
                    )
                finally:
                    agent.close()

    def test_plugin_definition_refresh_rebuilds_subagent_schema(self) -> None:
        agent = object.__new__(LocalToolAgent)
        manager = object()
        next_tools = {"subagent": object()}
        agent.config = SimpleNamespace(subagents=SimpleNamespace(enabled=True))
        agent._on_plugin_settings_changed = Mock(return_value=manager)
        agent._refresh_subagent_definitions = Mock()
        agent._build_tools = Mock(return_value=next_tools)
        agent._tools = {"stale": object()}

        LocalToolAgent.set_plugin_enabled(agent, True)

        self.assertIs(agent._plugin_manager, manager)
        agent._refresh_subagent_definitions.assert_called_once_with()
        agent._build_tools.assert_called_once_with()
        self.assertIs(agent._tools, next_tools)

    def test_verify_profile_is_opt_in_and_its_fixed_tool_is_child_only(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            with patch("openai.OpenAI", return_value=SimpleNamespace()):
                agent = LocalToolAgent(
                    _agent_config(
                        Path(temp_dir),
                        enabled=True,
                        enable_verify_agent=True,
                    )
                )
                try:
                    definition = agent._subagent_registry.get("verify")
                    self.assertIsNotNone(definition)
                    assert definition is not None
                    child_tools = agent._subagent_coordinator._tools_for_definition(definition)
                    self.assertEqual(
                        set(child_tools),
                        {
                            "list_files",
                            "find_files",
                            "read_file",
                            "read_image",
                            "search_text",
                            "verify_command",
                        },
                    )
                    self.assertNotIn("bash", child_tools)
                    self.assertNotIn("powershell", child_tools)
                    self.assertNotIn("verify_command", agent._tools)
                    schema = json.loads(agent._tools["subagent"].argument_schema)
                    self.assertEqual(
                        schema["properties"]["tasks"]["items"]["properties"]
                        ["subagent_type"]["enum"],
                        ["explore", "plan", "verify"],
                    )
                    protocol = agent._subagent_llm_protocol(definition, child_tools)
                    self.assertEqual(protocol.request_retry_count, 1)
                    self.assertIn(
                        "只可读取、搜索，并调用 Host 提供的 verify_command",
                        protocol.system_prompt_provider(),
                    )
                finally:
                    agent.close()

    def test_should_inherit_skill_snapshot_when_context_is_fresh(self) -> None:
        """fresh 子任务继承父 Skill 能力，但仍使用独立消息列表。"""

        class _SkillManager:
            def list_all(self):
                return [SimpleNamespace(name="web-fetcher")]

            def format_skills_for_prompt(self, _skills):
                return (
                    "<skill><name>web-fetcher</name>"
                    "<description>读取网页</description>"
                    "<location>D:/skills/web-fetcher/SKILL.md</location></skill>"
                )

        active_skill = SimpleNamespace(
            skill=SimpleNamespace(
                meta=SimpleNamespace(
                    name="web-fetcher",
                    scope="user",
                    description="读取网页",
                    source_path=Path("D:/skills/web-fetcher/SKILL.md"),
                ),
                body="使用受限联网工具读取并核验网页来源。",
            )
        )

        with tempfile.TemporaryDirectory() as temp_dir:
            agent = object.__new__(LocalToolAgent)
            agent.workspace_root = Path(temp_dir)
            agent.config = SimpleNamespace(workspace_detection_summary="workspace")
            agent._skill_manager = _SkillManager()
            agent._active_skills = []
            agent._load_agents_instructions = lambda: "项目协作规范"
            agent._agent_temp_dir_display = lambda: ".agent_tmp"
            child_tools = {
                "read_file": ToolDefinition(
                    name="read_file",
                    description="读取文件",
                    argument_schema="{}",
                    requires_confirmation=False,
                    run=lambda _arguments: ToolResult(ok=True, output="ok"),
                )
            }

            indexed_context = SubAgentExecutionContext(
                context="fresh",
                skill_context=agent._freeze_subagent_skill_context(),
            )
            indexed_messages = agent._build_subagent_messages(
                indexed_context,
                child_tools,
                "检查 Skill 继承",
                "使用可用 Skill 完成任务。",
            )
            agent._active_skills = [active_skill]
            active_context = SubAgentExecutionContext(
                context="fresh",
                skill_context=agent._freeze_subagent_skill_context(),
            )
            active_messages = agent._build_subagent_messages(
                active_context,
                child_tools,
                "检查 Skill 继承",
                "使用当前 Skill 完成任务。",
            )

        indexed_rendered = json.dumps(indexed_messages, ensure_ascii=False)
        active_rendered = json.dumps(active_messages, ensure_ascii=False)
        self.assertIn("项目协作规范", indexed_rendered)
        self.assertIn("search_tools", indexed_rendered)
        self.assertIn("invoke_tool", indexed_rendered)
        self.assertNotIn("read_file", indexed_rendered)
        self.assertIn('context=\\"fresh\\"', indexed_rendered)
        self.assertIn("<skill_index", indexed_rendered)
        self.assertIn("web-fetcher", indexed_rendered)
        self.assertIn("<active_skill_instructions", active_rendered)
        self.assertIn("使用受限联网工具读取并核验网页来源。", active_rendered)

    def test_tool_executor_does_not_swallow_cancellation_from_subagent(self) -> None:
        class RunCancelled(RuntimeError):
            pass

        agent = object.__new__(LocalToolAgent)
        agent._dispatch_plugin_hook = lambda _hook, payload, **_kwargs: dict(payload)
        tool = ToolDefinition(
            name="subagent",
            description="delegate",
            argument_schema="{}",
            requires_confirmation=False,
            run=lambda _arguments: (_ for _ in ()).throw(RunCancelled("cancelled")),
        )

        with self.assertRaises(RunCancelled):
            agent._execute_approved_tool(tool, {})

    def test_subagent_tool_is_a_parent_batch_serial_barrier(self) -> None:
        tool = ToolDefinition(
            name="subagent",
            description="delegate",
            argument_schema="{}",
            requires_confirmation=True,
            run=lambda _arguments: ToolResult(ok=True, output="ok"),
        )

        self.assertTrue(LocalToolAgent._tool_call_requires_serial_execution(tool, {}))

    def test_api_default_agent_injects_the_same_subagent_config_loader(self) -> None:
        project_context = SimpleNamespace(
            workspace_root=Path("D:/workspace"),
            detection_summary="workspace",
        )
        plugin_runtime = SimpleNamespace(
            manager=None,
            start=lambda: [],
            notify_app_started=lambda: None,
            close=lambda: None,
        )
        agent = SimpleNamespace(add_close_callback=lambda _callback: None)
        def fake_load_feature_enabled(section: str, default: bool = True, **_kwargs) -> bool:
            # 测试不得依赖本机用户配置，固定索引开关为默认关闭。
            return False if section in {"file_name_index", "content_index"} else bool(default)

        with patch("omnicrawl.api.app.detect_project_context", return_value=project_context):
            with patch("omnicrawl.api.app.load_llm_config", return_value="llm-config"):
                with patch(
                    "omnicrawl.api.app.load_feature_enabled",
                    side_effect=fake_load_feature_enabled,
                ):
                    with patch(
                        "omnicrawl.api.app.load_agent_temp_workspace_config",
                        return_value="temp-config",
                    ):
                        with patch(
                            "omnicrawl.api.app.load_subagent_config",
                            return_value="subagent-config",
                        ):
                            with patch(
                                "omnicrawl.extensions.plugin_manager.PluginRuntime.from_config_data",
                                return_value=plugin_runtime,
                            ):
                                with patch("omnicrawl.api.app.AgentConfig") as config_class:
                                    with patch(
                                        "omnicrawl.api.app.LocalToolAgent",
                                        return_value=agent,
                                    ):
                                        created = create_default_agent()

        self.assertIs(created, agent)
        config_class.assert_called_once_with(
            llm="llm-config",
            workspace_root=Path("D:/workspace"),
            workspace_detection_summary="workspace",
            file_name_index_enabled=False,
            content_index_enabled=False,
            temp_workspace="temp-config",
            subagents="subagent-config",
        )

    def test_api_default_agent_closes_plugin_runtime_when_agent_init_fails(self) -> None:
        project_context = SimpleNamespace(
            workspace_root=Path("D:/workspace"),
            detection_summary="workspace",
        )
        plugin_runtime = SimpleNamespace(
            manager=None,
            start=Mock(),
            close=Mock(),
        )
        with patch("omnicrawl.api.app.detect_project_context", return_value=project_context):
            with patch(
                "omnicrawl.extensions.plugin_manager.PluginRuntime.from_config_data",
                return_value=plugin_runtime,
            ):
                with patch(
                    "omnicrawl.api.app.load_llm_config",
                    side_effect=RuntimeError("invalid llm config"),
                ):
                    with self.assertRaisesRegex(RuntimeError, "invalid llm config"):
                        create_default_agent()

        plugin_runtime.start.assert_called_once_with()
        plugin_runtime.close.assert_called_once_with()

    def test_agent_maps_safe_subagent_events_to_session_and_public_callback(self) -> None:
        agent = object.__new__(LocalToolAgent)
        agent._subagent_event_lock = threading.RLock()
        session_events = []
        public_events = []
        persistent_events = []
        agent._append_session_event = lambda name, payload: session_events.append((name, payload))
        agent._subagent_event_handler = lambda name, payload: persistent_events.append((name, payload))
        agent._subagent_event_callback = lambda name, payload: public_events.append((name, payload))

        agent._handle_subagent_event(
            "subagent.task.completed",
            {
                "task_id": "task-a1b2c3d4e5f6",
                "description": "检查配置",
                "summary": "api_key=very-secret",
            },
        )

        self.assertEqual(session_events[0][0], "subagent_task_completed")
        self.assertEqual(public_events[0][0], "subagent.task.completed")
        self.assertEqual(persistent_events[0][0], "subagent.task.completed")
        self.assertEqual(session_events[0][1], public_events[0][1])
        self.assertEqual(session_events[0][1], persistent_events[0][1])
        self.assertNotIn("very-secret", json.dumps(public_events, ensure_ascii=False))
        self.assertNotIn("very-secret", json.dumps(persistent_events, ensure_ascii=False))
        self.assertIn("api_key=***", public_events[0][1]["summary"])

    def test_public_subagent_observer_failure_does_not_break_session_lifecycle(self) -> None:
        agent = object.__new__(LocalToolAgent)
        agent._subagent_event_lock = threading.RLock()
        session_events = []
        agent._append_session_event = lambda name, payload: session_events.append((name, payload))
        agent._subagent_event_callback = lambda _name, _payload: (_ for _ in ()).throw(
            RuntimeError("UI 已关闭")
        )

        agent._handle_subagent_event(
            "subagent.task.started",
            {
                "task_id": "task-a1b2c3d4e5f6",
                "description": "检查配置",
            },
        )

        self.assertEqual(session_events[0][0], "subagent_task_started")

    def test_agent_public_result_uses_parent_session_artifact_store(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            workspace = Path(temp_dir)
            store = SessionStore(workspace / ".agent_sessions")
            state = store.start_session(workspace)
            agent = object.__new__(LocalToolAgent)
            agent.workspace_root = workspace
            agent.config = SimpleNamespace(
                subagents=SubAgentConfig(enabled=True, result_summary_chars=40),
            )
            agent._session_store = store
            agent._session_state = state
            raw_result = "A" * 60 + " token=very-secret " + "Z" * 60

            prepared = agent._prepare_subagent_public_result(
                "task-a1b2c3d4e5f6",
                "explore",
                "检查配置",
                raw_result,
            )
            artifact_text = store.read_artifact_text(
                state.session_id,
                prepared.artifacts[0]["artifact_path"],
            )

        self.assertNotIn("very-secret", prepared.summary)
        self.assertNotIn("very-secret", artifact_text)
        self.assertIn("token=***", artifact_text)

    def test_subagent_global_timeout_tightens_each_model_request_timeout(self) -> None:
        agent = object.__new__(LocalToolAgent)
        agent.workspace_root = Path.cwd()
        agent.config = SimpleNamespace(
            llm=SimpleNamespace(model="test-model", reasoning_effort=""),
            request_timeout_seconds=180,
            request_retry_count=1,
            subagents=SubAgentConfig(enabled=True, default_timeout_seconds=30),
        )
        agent._llm_client = lambda: None
        agent._runtime_manager_for_protocol = lambda: None
        agent._build_extra_body = lambda: {}
        definition = AgentDefinition(
            name="explore",
            description="explore",
            system_prompt="只读。",
        )

        protocol = agent._subagent_llm_protocol(definition, {})

        self.assertEqual(protocol.request_timeout_seconds, 30)

    def test_model_request_concurrency_is_bounded_across_parallel_tasks(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            workspace = Path(temp_dir)
            agent = object.__new__(LocalToolAgent)
            agent.workspace_root = workspace
            agent.config = SimpleNamespace(
                llm=SimpleNamespace(model="test-model", reasoning_effort=""),
                request_timeout_seconds=30,
                request_retry_count=1,
                max_tool_output_chars=6000,
                workspace_detection_summary="",
                subagents=SubAgentConfig(
                    enabled=True,
                    max_concurrency=2,
                    model_request_concurrency=1,
                    default_timeout_seconds=30,
                ),
            )
            agent._temp_workspace = SimpleNamespace(display_path=".agent_tmp")
            agent._cancel_check = lambda: None
            agent._subagent_model_request_semaphore = threading.BoundedSemaphore(1)
            active = 0
            max_active = 0
            lock = threading.Lock()
            start_barrier = threading.Barrier(2)

            class Protocol:
                runtime_manager = None

                def request_reply(self, *_args, **_kwargs):
                    nonlocal active, max_active
                    with lock:
                        active += 1
                        max_active = max(max_active, active)
                    time.sleep(0.05)
                    with lock:
                        active -= 1
                    return AgentModelReply(
                        message={"role": "assistant", "content": "完成"},
                        content="完成",
                    )

            agent._subagent_llm_protocol = (
                lambda _definition, _tools, **_kwargs: Protocol()
            )
            definition = AgentDefinition(
                name="explore",
                description="explore",
                system_prompt="只读。",
            )

            def run_task(index):
                start_barrier.wait(timeout=2)
                return agent._execute_subagent_task(
                    definition,
                    {},
                    f"任务 {index}",
                    "返回完成。",
                )

            with ThreadPoolExecutor(max_workers=2) as executor:
                results = list(executor.map(run_task, range(2)))

        self.assertEqual([item.final_text for item in results], ["完成", "完成"])
        self.assertEqual(max_active, 1)

    def test_child_execution_uses_independent_messages_snapshot_and_no_session_events(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            workspace = Path(temp_dir)
            (workspace / "AGENTS.md").write_text("# Rules\n只读分析。", encoding="utf-8")
            agent = object.__new__(LocalToolAgent)
            parent_snapshot = object()
            child_snapshot = object()
            agent.workspace_root = workspace
            agent.config = SimpleNamespace(
                llm=SimpleNamespace(model="test-model", reasoning_effort=""),
                request_timeout_seconds=30,
                request_retry_count=1,
                max_tool_output_chars=6000,
                workspace_detection_summary="",
                subagents=SubAgentConfig(enabled=True, default_timeout_seconds=30),
            )
            agent._history = [{"role": "user", "content": "父历史"}]
            agent._pending_user_text = "父任务"
            agent._active_skills = ["parent-skill"]
            agent._active_runtime_snapshot = parent_snapshot
            agent._temp_workspace = SimpleNamespace(display_path=".agent_tmp")
            agent._cancel_check = lambda: None
            session_events = []
            hook_names = []
            agent._append_session_event = lambda event, payload: session_events.append((event, payload))

            def dispatch(hook, payload, **_kwargs):
                hook_names.append(hook)
                return dict(payload)

            agent._dispatch_plugin_hook = dispatch

            class RuntimeManager:
                def __init__(self) -> None:
                    self.acquired = 0
                    self.released = []

                def acquire_turn(self):
                    self.acquired += 1
                    return child_snapshot

                def release_turn(self, snapshot):
                    self.released.append(snapshot)

            runtime_manager = RuntimeManager()
            captured_messages = []
            replies = iter(
                [
                    AgentModelReply(
                        message={"role": "assistant", "content": None},
                        content="",
                        tool_calls=[ToolCall("read_file", {"path": "README.md"}, "call_1")],
                    ),
                    AgentModelReply(
                        message={"role": "assistant", "content": "完成"},
                        content="完成",
                    ),
                ]
            )

            class Protocol:
                def __init__(self, manager) -> None:
                    self.runtime_manager = manager

                def request_reply(self, messages, *args, **kwargs):
                    captured_messages.append([dict(item) for item in messages])
                    usage = args[1]
                    usage(10, 2, 1)
                    snapshot = kwargs.get("runtime_snapshot") if kwargs else args[6]
                    self.assert_snapshot(snapshot)
                    return next(replies)

                @staticmethod
                def assert_snapshot(snapshot):
                    if snapshot is not child_snapshot:
                        raise AssertionError("child runtime snapshot mismatch")

            agent._subagent_llm_protocol = (
                lambda _definition, _tools, **_kwargs: Protocol(runtime_manager)
            )
            child_tools = {
                "read_file": ToolDefinition(
                    name="read_file",
                    description="read",
                    argument_schema='{"path":"README.md"}',
                    requires_confirmation=False,
                    run=lambda _arguments: ToolResult(ok=True, output="README"),
                )
            }
            definition = AgentDefinition(
                name="explore",
                description="explore",
                system_prompt="只读。",
                tools=("read_file",),
            )

            result = agent._execute_subagent_task(
                definition,
                child_tools,
                "检查 README",
                "读取 README 并总结。",
            )

        self.assertIsInstance(result, SubAgentExecutionResult)
        self.assertEqual(result.final_text, "完成")
        self.assertEqual(result.model_turns, 2)
        self.assertEqual(result.tool_calls, 1)
        self.assertEqual(result.input_tokens, 20)
        self.assertEqual(runtime_manager.acquired, 1)
        self.assertEqual(runtime_manager.released, [child_snapshot])
        self.assertIs(agent._active_runtime_snapshot, parent_snapshot)
        self.assertEqual(agent._history, [{"role": "user", "content": "父历史"}])
        self.assertEqual(agent._pending_user_text, "父任务")
        self.assertEqual(agent._active_skills, ["parent-skill"])
        self.assertEqual(session_events, [])
        self.assertIn("tool.call.before", hook_names)
        self.assertIn("tool.execute.after", hook_names)
        self.assertNotIn("父历史", json.dumps(captured_messages, ensure_ascii=False))
        self.assertIn("只读分析", json.dumps(captured_messages, ensure_ascii=False))

    def test_background_notifications_are_injected_before_each_model_request(self) -> None:
        agent = object.__new__(LocalToolAgent)
        notifications = iter(
            [
                [{"task_id": "task-one", "status": "completed"}],
                [{"task_id": "task-two", "status": "failed"}],
                [],
            ]
        )
        agent._subagent_coordinator = SimpleNamespace(
            drain_notifications=lambda **_kwargs: next(notifications)
        )
        agent.__dict__["_session_state"] = None
        messages = [{"role": "user", "content": "继续任务"}]

        agent._inject_subagent_notifications(messages)
        agent._inject_subagent_notifications(messages)
        agent._inject_subagent_notifications(messages)

        content = messages[0]["content"]
        self.assertIn("task-one", content)
        self.assertIn("task-two", content)
        self.assertEqual(content.count("task-one"), 1)
        self.assertEqual(content.count("task-two"), 1)
        self.assertEqual(len(messages), 1)


class SubAgentTaskControlFacadeTest(unittest.TestCase):
    """锁定 API/TUI 使用的 LocalToolAgent 控制门面，不接触 TaskManager 私有状态。"""

    def test_control_methods_delegate_to_coordinator_and_disabled_state_is_explicit(self) -> None:
        class FakeCoordinator:
            def __init__(self) -> None:
                self.cancelled: list[str] = []

            def list_tasks(self):
                return [{"task_id": "task-current", "status": "running"}]

            def get_task(self, task_id: str):
                return {"task_id": task_id, "status": "running"}

            def cancel_task(self, *, task_id: str):
                self.cancelled.append(task_id)
                return {"ok": True, "task_id": task_id, "status": "cancelling"}

        agent = object.__new__(LocalToolAgent)
        coordinator = FakeCoordinator()
        agent._subagent_coordinator = coordinator

        self.assertEqual(
            LocalToolAgent.list_subagent_tasks(agent),
            [{"task_id": "task-current", "status": "running"}],
        )
        self.assertEqual(
            LocalToolAgent.get_subagent_task(agent, "task-current")["task_id"],
            "task-current",
        )
        self.assertEqual(
            LocalToolAgent.cancel_subagent_task(agent, "task-current")["status"],
            "cancelling",
        )
        self.assertEqual(coordinator.cancelled, ["task-current"])

        agent._subagent_coordinator = None
        with self.assertRaisesRegex(AgentError, "SubAgent 功能未启用"):
            LocalToolAgent.list_subagent_tasks(agent)


class SubAgentProviderRuntimeContractTest(unittest.TestCase):
    """验证定义式 SubAgent 对四种统一 Provider Runtime 的共同协议边界。"""

    @staticmethod
    def _definition(*, with_read_file: bool = False) -> AgentDefinition:
        return AgentDefinition(
            name="explore",
            description="只读探索",
            system_prompt="只读取证据并返回结论。",
            tools=("read_file",) if with_read_file else (),
        )

    @staticmethod
    def _runtime(
        provider: str,
        protocol: str,
        *,
        response_text: str,
        reasoning_text: str = "",
        emit_tool_on_first_turn: bool = False,
        cancel_signal: threading.Event | None = None,
    ) -> tuple[ModelIdentity, _ProviderContractFakeRuntime, ModelRuntimeManager]:
        identity = ModelIdentity(
            profile_id=f"{provider}-{protocol}",
            provider=provider,
            protocol=protocol,
            model_id=f"{provider}-fake-model",
        )
        profile = ProviderProfile(
            id=identity.profile_id,
            provider=provider,
            api_key="fake-key",
            default_protocol=protocol,
        )
        descriptor = ModelDescriptor(
            identity=identity,
            display_name=identity.model_id,
            capabilities=ModelCapabilities(streaming=True, tools=True),
        )
        runtime = _ProviderContractFakeRuntime(
            identity,
            response_text=response_text,
            reasoning_text=reasoning_text,
            emit_tool_on_first_turn=emit_tool_on_first_turn,
            cancel_signal=cancel_signal,
        )
        manager = ModelRuntimeManager()
        manager.bootstrap(profile, descriptor, runtime=runtime)
        return identity, runtime, manager

    @staticmethod
    def _agent(workspace: Path, manager: ModelRuntimeManager, model_id: str) -> LocalToolAgent:
        """构造只具备本契约所需依赖的 LocalToolAgent，避免 SDK 与网络边界。"""

        agent = object.__new__(LocalToolAgent)
        agent.workspace_root = workspace
        agent.config = SimpleNamespace(
            llm=SimpleNamespace(model=model_id, reasoning_effort=""),
            request_timeout_seconds=30,
            request_retry_count=1,
            max_tool_output_chars=6000,
            workspace_detection_summary="",
            approval_mode="auto",
            subagents=SubAgentConfig(enabled=True, default_timeout_seconds=20),
        )
        agent._llm_client = lambda: None
        agent._runtime_manager_for_protocol = lambda: manager
        agent._build_extra_body = lambda: {}
        agent._temp_workspace = SimpleNamespace(display_path=".agent_tmp")
        agent._cancel_check = None
        # 工具往返测试仍需要走 Host 的 Plugin guard 链，但不加载真实插件。
        agent._dispatch_plugin_hook = lambda _hook, payload, **_kwargs: dict(payload)
        return agent

    def test_all_provider_fake_runtimes_execute_fresh_subagent_and_report_usage(self) -> None:
        """四种身份都应走统一 Runtime，并保持 fresh 上下文和用量统计。"""

        with tempfile.TemporaryDirectory() as temp_dir:
            workspace = Path(temp_dir)
            for provider, protocol in _PROVIDER_RUNTIME_CONTRACTS:
                with self.subTest(provider=provider, protocol=protocol):
                    response_text = f"{protocol} 的只读报告"
                    identity, runtime, manager = self._runtime(
                        provider,
                        protocol,
                        response_text=response_text,
                    )
                    try:
                        agent = self._agent(workspace, manager, identity.model_id)
                        result = agent._execute_subagent_task(
                            self._definition(),
                            {},
                            "验证 Provider 协议",
                            "返回一份受限的只读报告。",
                            cancel_check=lambda: None,
                        )

                        self.assertEqual(result.final_text, response_text)
                        self.assertEqual(result.model_turns, 1)
                        self.assertEqual(result.tool_calls, 0)
                        self.assertEqual(
                            (result.input_tokens, result.output_tokens, result.cached_input_tokens),
                            (11, 7, 3),
                        )
                        self.assertEqual(runtime.cancel_checks, 1)
                        self.assertEqual(len(runtime.requests), 1)
                        request = runtime.requests[0]
                        self.assertEqual(request.identity, identity)
                        self.assertEqual(request.identity.provider, provider)
                        self.assertEqual(request.identity.protocol, protocol)
                        self.assertIn("<agent_definition name=\"explore\">", request.system_prompt)
                        self.assertIn('context="fresh"', request.messages[-1].text)
                    finally:
                        manager.close()
                    self.assertTrue(runtime.closed)

    def test_hidden_reasoning_never_enters_public_subagent_result(self) -> None:
        """Provider reasoning 只参与私有协议消息，不能进入结果、事件或 artifact。"""

        with tempfile.TemporaryDirectory() as temp_dir:
            workspace = Path(temp_dir)
            identity, runtime, manager = self._runtime(
                "openai",
                PROTOCOL_OPENAI_RESPONSES,
                response_text="仅公开这段结论",
                reasoning_text="HIDDEN_CHAIN token=reasoning-secret",
            )
            try:
                agent = self._agent(workspace, manager, identity.model_id)
                result = agent._execute_subagent_task(
                    self._definition(),
                    {},
                    "验证隐藏推理隔离",
                    "只返回公开结论。",
                    cancel_check=lambda: None,
                )
                public_result = agent._prepare_subagent_public_result(
                    "task-reasoning",
                    "explore",
                    "验证隐藏推理隔离",
                    result.final_text,
                )
            finally:
                manager.close()

        serialized = json.dumps(
            {
                "final_text": result.final_text,
                "summary": public_result.summary,
                "artifacts": public_result.artifacts,
            },
            ensure_ascii=False,
        )
        self.assertEqual(result.final_text, "仅公开这段结论")
        self.assertNotIn("HIDDEN_CHAIN", serialized)
        self.assertNotIn("reasoning-secret", serialized)
        self.assertTrue(runtime.closed)

    def test_all_provider_fake_runtimes_complete_read_only_tool_round_trip(self) -> None:
        """每种身份都能把归一化工具调用映射回 Host 只读工具并提交下一回合。"""

        with tempfile.TemporaryDirectory() as temp_dir:
            workspace = Path(temp_dir)
            for provider, protocol in _PROVIDER_RUNTIME_CONTRACTS:
                with self.subTest(provider=provider, protocol=protocol):
                    identity, runtime, manager = self._runtime(
                        provider,
                        protocol,
                        response_text=f"{protocol} 已整理 README",
                        emit_tool_on_first_turn=True,
                    )
                    tool_invocations: list[dict[str, str]] = []
                    child_tools = {
                        "read_file": ToolDefinition(
                            name="read_file",
                            description="读取 README",
                            argument_schema='{"path": "README.md"}',
                            requires_confirmation=False,
                            run=lambda arguments: (
                                tool_invocations.append(dict(arguments))
                                or ToolResult(ok=True, output="README 内容")
                            ),
                        )
                    }
                    try:
                        agent = self._agent(workspace, manager, identity.model_id)
                        result = agent._execute_subagent_task(
                            self._definition(with_read_file=True),
                            child_tools,
                            "读取 README",
                            "读取 README.md 后给出摘要。",
                            cancel_check=lambda: None,
                        )

                        self.assertEqual(result.final_text, f"{protocol} 已整理 README")
                        self.assertEqual(result.model_turns, 3)
                        self.assertEqual(result.tool_calls, 2)
                        self.assertEqual(
                            (result.input_tokens, result.output_tokens, result.cached_input_tokens),
                            (25, 13, 5),
                        )
                        self.assertEqual(tool_invocations, [{"path": "README.md"}])
                        self.assertEqual(len(runtime.requests), 3)
                        self.assertEqual(runtime.cancel_checks, 3)
                        self.assertEqual(len(runtime.requests[0].tools), 2)
                        tool_results = [
                            block
                            for message in runtime.requests[2].messages
                            if message.role == "tool"
                            for block in message.blocks
                            if isinstance(block, ToolResultBlock)
                        ]
                        self.assertEqual(len(tool_results), 2)
                        tool_result = next(
                            block
                            for block in tool_results
                            if "工具：read_file" in block.content
                        )
                        self.assertEqual(tool_result.call_id, "provider-call-2")
                        # Host 会将工具结果包装为可读的状态上下文；契约应固定该
                        # 包装仍保留原始正文，而不是错误地要求直接透传 output。
                        self.assertIn("状态：成功", tool_result.content)
                        self.assertIn("README 内容", tool_result.content)
                    finally:
                        manager.close()
                    self.assertTrue(runtime.closed)

    def test_all_provider_fake_runtimes_receive_and_propagate_cancellation(self) -> None:
        """Provider Runtime 内检测到的父取消必须中断四种身份的子任务。"""

        with tempfile.TemporaryDirectory() as temp_dir:
            workspace = Path(temp_dir)
            for provider, protocol in _PROVIDER_RUNTIME_CONTRACTS:
                with self.subTest(provider=provider, protocol=protocol):
                    cancel_signal = threading.Event()
                    identity, runtime, manager = self._runtime(
                        provider,
                        protocol,
                        response_text="不应返回",
                        cancel_signal=cancel_signal,
                    )

                    def cancel_check() -> None:
                        if cancel_signal.is_set():
                            raise _ProviderContractCancelled(protocol)

                    try:
                        agent = self._agent(workspace, manager, identity.model_id)
                        with self.assertRaises(_ProviderContractCancelled):
                            agent._execute_subagent_task(
                                self._definition(),
                                {},
                                "验证取消边界",
                                "在 Runtime 检查取消回调。",
                                cancel_check=cancel_check,
                            )
                        self.assertTrue(cancel_signal.is_set())
                        self.assertEqual(runtime.cancel_checks, 1)
                        self.assertEqual(len(runtime.requests), 1)
                    finally:
                        manager.close()
                    self.assertTrue(runtime.closed)


if __name__ == "__main__":
    unittest.main()
