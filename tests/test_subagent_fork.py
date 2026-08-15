from __future__ import annotations

import json
import tempfile
import threading
import time
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch

from omnicrawl.agent.core import LocalToolAgent
from omnicrawl.agent.types import AgentModelReply
from omnicrawl.agent.subagents.coordinator import (
    SubAgentCoordinator,
    SubAgentExecutionResult,
)
from omnicrawl.agent.subagents.definitions import AgentDefinition, AgentDefinitionRegistry
from omnicrawl.agent.subagents.execution import SubAgentExecutionContext
from omnicrawl.agent.types import ToolResult
from omnicrawl.config.llm import LLMConfig
from omnicrawl.config.subagents import SubAgentConfig
from omnicrawl.llm.capabilities import ModelCapabilities
from omnicrawl.llm.protocol import (
    ModelIdentity,
    ResponseCompleted,
    TextDelta,
)
from omnicrawl.llm.registry import ModelDescriptor, ProviderProfile


class _Registry(AgentDefinitionRegistry):
    """只提供本测试所需的内存定义，避免依赖用户级 Markdown 配置。"""

    def __init__(self, definitions: list[AgentDefinition]) -> None:
        self._definitions = {definition.name: definition for definition in definitions}

    def get(self, name: str):
        return self._definitions.get(name)

    def list_all(self):
        return list(self._definitions.values())


class _CapturingRuntime:
    """记录统一 Runtime 请求，用于验证跨 Provider 前的公共协议输入。"""

    def __init__(self, identity: ModelIdentity) -> None:
        self.identity = identity
        self.capabilities = ModelCapabilities(streaming=True, tools=True)
        self.requests = []
        self.closed = False

    def stream_turn(self, request, *, cancel_check=None):
        self.requests.append(request)
        if cancel_check is not None:
            cancel_check()
        yield TextDelta(text="Fork 完成")
        yield ResponseCompleted(finish_reason="stop")

    def close(self) -> None:
        self.closed = True


class _DedicatedRuntimeManager:
    """模拟每个子任务独立拥有的 RuntimeManager 生命周期。"""

    def __init__(self, descriptor: ModelDescriptor) -> None:
        self.runtime = _CapturingRuntime(descriptor.identity)
        self.snapshot = SimpleNamespace(descriptor=descriptor, runtime=self.runtime)
        self.acquired = 0
        self.released = []
        self.closed = False

    def acquire_turn(self):
        self.acquired += 1
        return self.snapshot

    def release_turn(self, snapshot) -> None:
        self.released.append(snapshot)

    def close(self) -> None:
        self.closed = True
        self.runtime.close()


class SubAgentForkCoordinatorTest(unittest.TestCase):
    def _definition(self, **overrides) -> AgentDefinition:
        values = {
            "name": "explore",
            "description": "只读探索",
            "system_prompt": "只读分析。",
            "tools": (),
            "source": "builtin",
        }
        values.update(overrides)
        return AgentDefinition(**values)

    def _arguments(self, *, context: str = "fresh", model: str | None = None) -> dict:
        task = {
            "description": "检查调用链",
            "prompt": "只返回可复核证据。",
            "subagent_type": "explore",
            "context": context,
        }
        if model is not None:
            task["model"] = model
        return {"action": "run", "tasks": [task]}

    def test_fork_requires_explicit_configuration(self) -> None:
        prepared = []
        executed = []
        coordinator = SubAgentCoordinator(
            config=SubAgentConfig(enabled=True, allow_fork=False),
            registry=_Registry([self._definition()]),
            tools_provider=lambda: {},
            prepare_execution=lambda definition, context, model: prepared.append(
                (definition, context, model)
            ) or SubAgentExecutionContext(context=context),
            execute_task=lambda *_args, **_kwargs: executed.append(True),
        )

        payload = json.loads(coordinator.run(self._arguments(context="fork")).output)

        self.assertEqual(payload["error"]["code"], "SUBAGENT_PERMISSION_DENIED")
        self.assertEqual(prepared, [])
        self.assertEqual(executed, [])

    def test_fork_and_task_model_are_frozen_before_execution(self) -> None:
        prepared = []
        executed = []

        def prepare(definition, context, model):
            prepared.append((definition.model, context, model))
            return SubAgentExecutionContext(context=context)

        def execute(
            _definition,
            _tools,
            _description,
            _prompt,
            _cancel_check,
            execution_context,
        ):
            executed.append(execution_context)
            return SubAgentExecutionResult("完成", 1, 0)

        coordinator = SubAgentCoordinator(
            config=SubAgentConfig(enabled=True, allow_fork=True),
            registry=_Registry([self._definition(model="definition-model")]),
            tools_provider=lambda: {},
            prepare_execution=prepare,
            execute_task=execute,
        )

        result = coordinator.run(self._arguments(context="fork", model="task-model"))
        payload = json.loads(result.output)

        self.assertTrue(result.ok)
        self.assertEqual(prepared, [("definition-model", "fork", "task-model")])
        self.assertEqual(len(executed), 1)
        self.assertEqual(executed[0].context, "fork")
        self.assertEqual(payload["results"][0]["status"], "completed")

    def test_background_fork_uses_context_prepared_before_worker_starts(self) -> None:
        source = {"content": "创建时上下文"}
        gate = threading.Event()
        observed = []

        def prepare(_definition, context, _model):
            return SubAgentExecutionContext(
                context=context,
                fork_messages=({"role": "user", "content": source["content"]},),
            )

        def execute(
            _definition,
            _tools,
            _description,
            _prompt,
            _cancel_check,
            execution_context,
        ):
            gate.wait(timeout=1)
            observed.append(execution_context.fork_messages[0]["content"])
            return SubAgentExecutionResult("完成", 1, 0)

        coordinator = SubAgentCoordinator(
            config=SubAgentConfig(
                enabled=True,
                allow_fork=True,
                allow_background=True,
            ),
            registry=_Registry([self._definition()]),
            tools_provider=lambda: {},
            prepare_execution=prepare,
            execute_task=execute,
        )
        arguments = self._arguments(context="fork")
        arguments["action"] = "spawn"
        task_id = json.loads(coordinator.run(arguments).output)["task_ids"][0]
        source["content"] = "父状态后来变化"
        gate.set()

        for _ in range(100):
            task = json.loads(
                coordinator.run({"action": "get", "task_id": task_id}).output
            )["task"]
            if task["status"] == "completed":
                break
            time.sleep(0.01)

        self.assertEqual(task["status"], "completed")
        self.assertEqual(observed, ["创建时上下文"])
        coordinator.cancel_and_wait(
            reason="test cleanup",
            timeout_seconds=1,
            permanent=True,
        )

    def test_preparation_failure_does_not_expose_sensitive_model_configuration(self) -> None:
        coordinator = SubAgentCoordinator(
            config=SubAgentConfig(enabled=True, allow_fork=True),
            registry=_Registry([self._definition()]),
            tools_provider=lambda: {},
            prepare_execution=lambda *_args, **_kwargs: (_ for _ in ()).throw(
                RuntimeError("api_key=very-secret")
            ),
            execute_task=lambda *_args, **_kwargs: self.fail("准备失败时不得执行"),
        )

        payload = json.loads(coordinator.run(self._arguments(model="chosen")).output)

        self.assertEqual(payload["error"]["code"], "SUBAGENT_MODEL_ERROR")
        self.assertNotIn("very-secret", json.dumps(payload, ensure_ascii=False))

    def test_model_override_must_be_a_bounded_nonempty_string(self) -> None:
        coordinator = SubAgentCoordinator(
            config=SubAgentConfig(enabled=True, allow_fork=True),
            registry=_Registry([self._definition()]),
            tools_provider=lambda: {},
            prepare_execution=lambda *_args, **_kwargs: self.fail("非法模型不得进入 Host 解析"),
            execute_task=lambda *_args, **_kwargs: self.fail("非法模型不得执行"),
        )

        empty = json.loads(coordinator.run(self._arguments(model=" ")).output)
        too_long = json.loads(
            coordinator.run(self._arguments(model="m" * 201)).output
        )

        self.assertEqual(empty["error"]["code"], "AGENT_DEFINITION_INVALID")
        self.assertEqual(too_long["error"]["code"], "AGENT_DEFINITION_INVALID")


class SubAgentForkRunStreamTest(unittest.TestCase):
    def test_parent_turn_freezes_and_clears_redacted_fork_snapshot(self) -> None:
        """真实父回合须在模型首请求前冻结快照，并在结束时删除临时引用。"""

        agent = object.__new__(LocalToolAgent)
        agent.workspace_root = Path.cwd()
        agent.config = SimpleNamespace(
            llm=None,
            max_history_turns=6,
            subagents=SubAgentConfig(enabled=True, allow_fork=True),
        )
        agent._history = [{"role": "assistant", "content": "上一轮 token=old-secret"}]
        agent._pending_user_text = None
        agent._active_skills = []
        agent._cancel_check = None
        agent._reasoning_delta_callback = None
        agent._subagent_event_callback = None
        agent._ensure_mcp_tools_ready = lambda _status: None
        agent._apply_skill_command = lambda text, _status: text
        agent._resolve_continue_request = lambda text: text
        agent._plugin_begin_turn = lambda: None
        agent._plugin_end_turn = lambda: None
        agent._dispatch_plugin_hook = lambda _hook, payload, **_kwargs: dict(payload)
        agent._append_prompt_history = lambda _text: None
        agent._append_session_event = lambda _event, _payload: None
        agent._context_messages = lambda **_kwargs: [
            {"role": "user", "content": "项目规范"}
        ]
        agent._append_history = lambda *_args, **_kwargs: None
        agent._inject_subagent_notifications = lambda _messages: None
        captured = []

        def request_reply(messages, *_args, **_kwargs):
            captured.extend(agent._active_fork_context_messages)
            return AgentModelReply(
                message={"role": "assistant", "content": "完成"},
                content="完成",
            )

        agent._request_agent_reply = request_reply
        result = agent.run_stream("本轮 token=current-secret", lambda _text: None)

        self.assertEqual(result, "完成")
        snapshot_text = "\n".join(str(item.get("content", "")) for item in captured)
        self.assertIn("项目规范", snapshot_text)
        self.assertIn("token=***", snapshot_text)
        self.assertNotIn("old-secret", snapshot_text)
        self.assertNotIn("current-secret", snapshot_text)
        self.assertNotIn("_active_fork_context_messages", agent.__dict__)


class SubAgentForkExecutionTest(unittest.TestCase):
    @staticmethod
    def _llm(model: str) -> LLMConfig:
        return LLMConfig(
            api_key="test-key",
            base_url="https://example.test/v1",
            model=model,
            model_source="legacy",
            profile_id="parent-profile",
            provider="openai",
            protocol="openai_chat_completions",
        )

    @staticmethod
    def _definition(model: str = "inherit") -> AgentDefinition:
        return AgentDefinition(
            name="explore",
            description="只读探索",
            system_prompt="只读分析并给出结论。",
            model=model,
        )

    def _agent(self, workspace: Path, parent_llm: LLMConfig) -> LocalToolAgent:
        agent = object.__new__(LocalToolAgent)
        agent.workspace_root = workspace
        agent.config = SimpleNamespace(
            llm=parent_llm,
            request_timeout_seconds=30,
            request_retry_count=1,
            max_tool_output_chars=6000,
            workspace_detection_summary="",
            subagents=SubAgentConfig(
                enabled=True,
                allow_fork=True,
                default_timeout_seconds=20,
            ),
        )
        agent._cancel_check = None
        agent._subagent_model_request_semaphore = None
        agent._temp_workspace = SimpleNamespace(display_path=".omnicrawl/.agent_tmp")
        agent._llm_client = lambda: None
        agent._dispatch_plugin_hook = lambda _hook, payload, **_kwargs: dict(payload)
        # Fork 必须使用创建时的父系统提示，而不是执行时重新读取可变状态。
        agent._system_prompt = lambda: "父 Agent 系统提示"
        return agent

    def test_fork_uses_only_frozen_skill_context_without_live_parent_lookup(self) -> None:
        """Fork 可继承创建时的公开 Skill 消息，但不能回读之后变化的父状态。"""

        class _ExplodingSkillManager:
            def list_all(self):
                raise AssertionError("Fork 执行阶段不得重新读取父 SkillManager")

        agent = object.__new__(LocalToolAgent)
        agent._skill_manager = _ExplodingSkillManager()
        agent._active_skills = ["LATE_PRIVATE_SKILL"]
        execution_context = SubAgentExecutionContext(
            context="fork",
            fork_messages=(
                {
                    "role": "user",
                    "content": (
                        '<active_skill_instructions source="skill-registry">'
                        "FROZEN_PUBLIC_SKILL"
                        "</active_skill_instructions>"
                    ),
                },
            ),
        )

        messages = agent._build_subagent_messages(
            execution_context,
            {},
            "检查 Fork Skill 快照",
            "只使用创建时冻结的公开上下文。",
        )

        rendered = json.dumps(messages, ensure_ascii=False)
        self.assertIn("FROZEN_PUBLIC_SKILL", rendered)
        self.assertIn('context=\\"fork\\"', rendered)
        self.assertNotIn("LATE_PRIVATE_SKILL", rendered)

    def test_fork_copies_redacted_parent_snapshot_and_uses_task_model_snapshot(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            workspace = Path(temp_dir)
            parent_llm = self._llm("parent-model")
            override_llm = self._llm("override-model")
            agent = self._agent(workspace, parent_llm)
            agent._history = [{"role": "user", "content": "父历史不应被改写"}]
            agent._pending_user_text = "父任务"
            agent._active_skills = ["parent-skill"]
            agent._active_runtime_snapshot = object()
            # 此快照模拟 run_stream 在当前轮开始时冻结的公开消息：包含用户目标和
            # 常见凭据表达，Fork 只可获得其脱敏副本。
            parent_messages = [
                {"role": "user", "content": "当前目标：实现模型覆盖 token=very-secret"},
                {"role": "assistant", "content": "已确认先完成 Fork。"},
            ]
            agent._active_fork_context_messages = tuple(parent_messages)

            with patch(
                "omnicrawl.agent.core.apply_model_selection",
                return_value=override_llm,
            ) as select_model:
                execution_context = agent._prepare_subagent_execution(
                    self._definition(model="definition-model"),
                    "fork",
                    "task-model",
                )

            # 调用参数优先于角色定义；创建后修改父配置或源消息都不能影响子任务。
            select_model.assert_called_once_with(parent_llm, "task-model")
            parent_llm.model = "new-parent-model"
            parent_messages[0]["content"] = "被父回合后续状态改写"

            model_snapshot = execution_context.model_snapshot
            self.assertIsNotNone(model_snapshot)
            assert model_snapshot is not None
            manager = _DedicatedRuntimeManager(model_snapshot.descriptor)
            agent._create_subagent_runtime_manager = lambda _snapshot: manager

            result = agent._execute_subagent_task(
                self._definition(model="definition-model"),
                {},
                "分析 Fork 边界",
                "核对上下文和模型是否冻结。",
                cancel_check=lambda: None,
                execution_context=execution_context,
            )

        self.assertEqual(result.final_text, "Fork 完成")
        self.assertEqual(manager.acquired, 1)
        self.assertEqual(manager.released, [manager.snapshot])
        self.assertTrue(manager.closed)
        self.assertTrue(manager.runtime.closed)
        self.assertEqual(manager.runtime.requests[0].identity.model_id, "override-model")
        request = manager.runtime.requests[0]
        self.assertIn("<fork_boilerplate>", request.system_prompt)
        self.assertIn("父 Agent 系统提示", request.system_prompt)
        fork_text = "\n".join(message.text for message in request.messages)
        self.assertIn("当前目标：实现模型覆盖", fork_text)
        self.assertIn("token=***", fork_text)
        self.assertNotIn("very-secret", fork_text)
        self.assertIn('context="fork"', fork_text)
        self.assertNotIn("被父回合后续状态改写", fork_text)
        self.assertEqual(agent._history, [{"role": "user", "content": "父历史不应被改写"}])
        self.assertEqual(agent._pending_user_text, "父任务")
        self.assertEqual(agent._active_skills, ["parent-skill"])

    def test_dedicated_runtime_is_closed_when_protocol_setup_fails(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            agent = self._agent(Path(temp_dir), self._llm("parent-model"))
            execution_context = agent._prepare_subagent_execution(
                self._definition(),
                "fresh",
                "",
            )
            model_snapshot = execution_context.model_snapshot
            assert model_snapshot is not None
            manager = _DedicatedRuntimeManager(model_snapshot.descriptor)
            agent._create_subagent_runtime_manager = lambda _snapshot: manager
            agent._subagent_llm_protocol = lambda *_args, **_kwargs: (_ for _ in ()).throw(
                RuntimeError("protocol setup failed")
            )

            with self.assertRaisesRegex(RuntimeError, "protocol setup failed"):
                agent._execute_subagent_task(
                    self._definition(),
                    {},
                    "测试清理",
                    "在协议构造失败时关闭独立 Runtime。",
                    cancel_check=lambda: None,
                    execution_context=execution_context,
                )

        self.assertTrue(manager.closed)
        self.assertTrue(manager.runtime.closed)

    def test_parent_model_is_used_when_neither_task_nor_definition_overrides_it(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            parent_llm = self._llm("parent-model")
            agent = self._agent(Path(temp_dir), parent_llm)

            execution_context = agent._prepare_subagent_execution(
                self._definition(),
                "fresh",
                "",
            )

        self.assertEqual(
            execution_context.model_snapshot.descriptor.model_id,
            "parent-model",
        )

    def test_explicit_task_inherit_overrides_definition_model(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            parent_llm = self._llm("parent-model")
            agent = self._agent(Path(temp_dir), parent_llm)

            with patch("omnicrawl.agent.core.apply_model_selection") as select_model:
                execution_context = agent._prepare_subagent_execution(
                    self._definition(model="definition-key"),
                    "fresh",
                    "inherit",
                )

        select_model.assert_not_called()
        self.assertEqual(
            execution_context.model_snapshot.descriptor.model_id,
            "parent-model",
        )

    def test_default_task_placeholder_uses_parent_model(self) -> None:
        """模型为可选字段编造 default 时，不得将其发送到 Provider。"""

        with tempfile.TemporaryDirectory() as temp_dir:
            parent_llm = self._llm("parent-model")
            agent = self._agent(Path(temp_dir), parent_llm)

            with patch("omnicrawl.agent.core.apply_model_selection") as select_model:
                execution_context = agent._prepare_subagent_execution(
                    self._definition(model="definition-key"),
                    "fresh",
                    "DEFAULT",
                )

        select_model.assert_not_called()
        self.assertEqual(
            execution_context.model_snapshot.descriptor.model_id,
            "parent-model",
        )

    def test_definition_model_is_used_when_task_does_not_override_it(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            parent_llm = self._llm("parent-model")
            agent = self._agent(Path(temp_dir), parent_llm)
            agent._active_fork_context_messages = (
                {"role": "user", "content": "上下文"},
            )
            definition_llm = self._llm("definition-model")

            with patch(
                "omnicrawl.agent.core.apply_model_selection",
                return_value=definition_llm,
            ) as select_model:
                execution_context = agent._prepare_subagent_execution(
                    self._definition(model="definition-key"),
                    "fresh",
                    "",
                )

        select_model.assert_called_once_with(parent_llm, "definition-key")
        self.assertEqual(
            execution_context.model_snapshot.descriptor.model_id,
            "definition-model",
        )

    def test_subagents_toml_role_model_overrides_definition_model(self) -> None:
        """subagents.toml 中的角色模型高于 Markdown 定义，低于任务级显式指定。"""

        with tempfile.TemporaryDirectory() as temp_dir:
            parent_llm = self._llm("parent-model")
            agent = self._agent(Path(temp_dir), parent_llm)
            agent.config.subagents = SubAgentConfig(
                enabled=True,
                allow_fork=True,
                model_overrides={"explore": "configured-key"},
            )

            configured_llm = self._llm("configured-model")

            with patch(
                "omnicrawl.agent.core.apply_model_selection",
                return_value=configured_llm,
            ) as select_model:
                execution_context = agent._prepare_subagent_execution(
                    self._definition(model="definition-key"),
                    "fresh",
                    "",
                )

        select_model.assert_called_once_with(parent_llm, "configured-key")
        self.assertEqual(
            execution_context.model_snapshot.descriptor.model_id,
            "configured-model",
        )

    def test_task_model_still_wins_over_subagents_toml_role_config(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            parent_llm = self._llm("parent-model")
            agent = self._agent(Path(temp_dir), parent_llm)
            agent.config.subagents = SubAgentConfig(
                enabled=True,
                allow_fork=True,
                model_overrides={"explore": "configured-key"},
            )

            task_llm = self._llm("task-model")

            with patch(
                "omnicrawl.agent.core.apply_model_selection",
                return_value=task_llm,
            ) as select_model:
                execution_context = agent._prepare_subagent_execution(
                    self._definition(model="definition-key"),
                    "fresh",
                    "task-key",
                )

        select_model.assert_called_once_with(parent_llm, "task-key")
        self.assertEqual(
            execution_context.model_snapshot.descriptor.model_id,
            "task-model",
        )

    def test_inherit_role_config_falls_back_to_definition_model(self) -> None:
        """角色配置显式写 inherit 时，不覆盖定义级模型。"""

        with tempfile.TemporaryDirectory() as temp_dir:
            parent_llm = self._llm("parent-model")
            agent = self._agent(Path(temp_dir), parent_llm)
            agent.config.subagents = SubAgentConfig(
                enabled=True,
                allow_fork=True,
                model_overrides={},
            )

            definition_llm = self._llm("definition-model")

            with patch(
                "omnicrawl.agent.core.apply_model_selection",
                return_value=definition_llm,
            ) as select_model:
                execution_context = agent._prepare_subagent_execution(
                    self._definition(model="definition-key"),
                    "fresh",
                    "",
                )

        select_model.assert_called_once_with(parent_llm, "definition-key")
        self.assertEqual(
            execution_context.model_snapshot.descriptor.model_id,
            "definition-model",
        )


if __name__ == "__main__":
    unittest.main()
