from __future__ import annotations

import json
import threading
import time
import unittest
from pathlib import Path

from omnicrawl.agent.execution import AgentLoopBudgetExceeded
from omnicrawl.agent.llm_protocol import AgentProtocolError
from omnicrawl.agent.subagents.coordinator import (
    SubAgentCancelled,
    SubAgentCoordinator,
    SubAgentExecutionResult,
    SubAgentPublicResult,
    _build_failure_diagnostics,
)
from omnicrawl.llm.errors import ModelError, ModelErrorCode
from omnicrawl.agent.subagents.approval import (
    ApprovalBroker,
    current_subagent_approval_scope,
)
from omnicrawl.agent.subagents.definitions import AgentDefinition, AgentDefinitionRegistry
from omnicrawl.agent.types import ToolDefinition, ToolResult
from omnicrawl.config.subagents import SubAgentConfig


def _tool(name: str, *, confirmation: bool = True) -> ToolDefinition:
    return ToolDefinition(
        name=name,
        description=name,
        argument_schema="{}",
        requires_confirmation=confirmation,
        run=lambda _arguments: ToolResult(ok=True, output=name),
    )


class _Registry(AgentDefinitionRegistry):
    def __init__(self, definitions: list[AgentDefinition]) -> None:
        self._definitions = {item.name: item for item in definitions}

    def get(self, name: str):
        return self._definitions.get(name)

    def list_all(self):
        return sorted(self._definitions.values(), key=lambda item: item.name)


class SubAgentCoordinatorTest(unittest.TestCase):
    def test_model_failure_exposes_safe_classification_and_no_raw_message(self) -> None:
        root_error = ModelError(
            code=ModelErrorCode.AUTHENTICATION_FAILED,
            message="api_key=secret-value",
            status_code=401,
            provider="openai",
            retryable=False,
        )
        wrapped_error = AgentProtocolError("Agent 模型请求中断：api_key=secret-value")
        wrapped_error.__cause__ = root_error

        diagnostic = _build_failure_diagnostics(
            wrapped_error,
            model="migrated-default",
            wire_model="gpt-5.6-luna",
        )

        self.assertEqual(diagnostic["category"], "AUTHENTICATION_FAILED")
        self.assertEqual(diagnostic["model_selection"], "migrated-default")
        self.assertEqual(diagnostic["wire_model"], "gpt-5.6-luna")
        self.assertEqual(diagnostic["exception_type"], "ModelError")
        self.assertEqual(diagnostic["provider"], "openai")
        self.assertEqual(diagnostic["status_code"], 401)
        self.assertFalse(diagnostic["retryable"])
        self.assertNotIn("secret-value", str(diagnostic))

        generic_diagnostic = _build_failure_diagnostics(
            AgentProtocolError("HTTP 401 unauthorized"),
        )
        self.assertEqual(generic_diagnostic["category"], "AUTHENTICATION_FAILED")
        self.assertEqual(generic_diagnostic["status_code"], 401)

    def _arguments(self, **task_overrides):
        task = {
            "description": "检查调用链",
            "prompt": "定位 Session 恢复逻辑并给出证据。",
            "subagent_type": "explore",
            "context": "fresh",
        }
        task.update(task_overrides)
        return {"action": "run", "tasks": [task]}

    def _definition(self, **overrides) -> AgentDefinition:
        values = {
            "name": "explore",
            "description": "只读探索",
            "system_prompt": "只读。",
            "tools": ("read_file", "search_text", "memory_read", "write_file", "bash", "subagent"),
            "disallowed_tools": (),
            "source_path": Path("explore.md"),
            "source": "builtin",
        }
        values.update(overrides)
        return AgentDefinition(**values)

    def test_disabled_feature_fails_without_starting_execution(self) -> None:
        calls = []
        coordinator = SubAgentCoordinator(
            config=SubAgentConfig(enabled=False),
            registry=_Registry([self._definition()]),
            tools_provider=lambda: {},
            execute_task=lambda *_args: calls.append("called"),
        )

        result = coordinator.run(self._arguments())
        payload = json.loads(result.output)

        self.assertFalse(result.ok)
        self.assertEqual(payload["error"]["code"], "SUBAGENT_DISABLED")
        self.assertEqual(calls, [])

    def test_phase1b_rejects_task_overflow(self) -> None:
        coordinator = SubAgentCoordinator(
            config=SubAgentConfig(enabled=True, max_tasks_per_batch=2),
            registry=_Registry([self._definition()]),
            tools_provider=lambda: {},
            execute_task=lambda *_args: self.fail("invalid request must not execute"),
        )

        overflow = self._arguments()
        overflow["tasks"].extend(
            [dict(overflow["tasks"][0]), dict(overflow["tasks"][0])]
        )
        overflow_result = json.loads(coordinator.run(overflow).output)
        self.assertEqual(overflow_result["error"]["code"], "SUBAGENT_LIMIT_EXCEEDED")



    def test_accepts_unbounded_task_description_and_prompt_text(self) -> None:
        received: list[tuple[str, str]] = []

        def execute(_definition, _tools, description, prompt, _cancel_check):
            received.append((description, prompt))
            return SubAgentExecutionResult("完成", 1, 0)

        coordinator = SubAgentCoordinator(
            config=SubAgentConfig(enabled=True),
            registry=_Registry([self._definition()]),
            tools_provider=lambda: {},
            execute_task=execute,
        )
        description = "任务" * 121
        prompt = "提示" * 6_001
        result = coordinator.run(self._arguments(description=description, prompt=prompt))

        self.assertTrue(result.ok)
        self.assertEqual(received, [(description, prompt)])

        barrier = threading.Barrier(2)
        lock = threading.Lock()
        active = 0
        max_active = 0

        def execute(_definition, _tools, description, _prompt, _cancel_check):
            nonlocal active, max_active
            with lock:
                active += 1
                max_active = max(max_active, active)
            barrier.wait(timeout=2)
            with lock:
                active -= 1
            return SubAgentExecutionResult(description, 1, 0)

        coordinator = SubAgentCoordinator(
            config=SubAgentConfig(enabled=True, max_concurrency=2),
            registry=_Registry([self._definition()]),
            tools_provider=lambda: {},
            execute_task=execute,
        )
        arguments = self._arguments()
        arguments["tasks"] = [
            {**arguments["tasks"][0], "description": "任务一"},
            {**arguments["tasks"][0], "description": "任务二"},
        ]
        arguments["max_concurrency"] = 4

        payload = json.loads(coordinator.run(arguments).output)

        self.assertEqual(payload["status"], "completed")
        self.assertEqual(max_active, 2)
        self.assertEqual(
            [item["description"] for item in payload["results"]],
            ["任务一", "任务二"],
        )
        self.assertEqual(
            [item["summary"] for item in payload["results"]],
            ["任务一", "任务二"],
        )

    def test_lifecycle_events_share_batch_and_task_ids_with_ordered_results(self) -> None:
        events = []
        coordinator = SubAgentCoordinator(
            config=SubAgentConfig(enabled=True, max_concurrency=1),
            registry=_Registry([self._definition()]),
            tools_provider=lambda: {},
            execute_task=lambda _definition, _tools, description, _prompt, _cancel_check: (
                SubAgentExecutionResult(description, 1, 0)
            ),
            event_sink=lambda name, payload: events.append((name, payload)),
        )
        arguments = self._arguments()
        arguments["tasks"] = [
            {**arguments["tasks"][0], "description": "任务一"},
            {**arguments["tasks"][0], "description": "任务二"},
        ]

        payload = json.loads(coordinator.run(arguments).output)

        self.assertEqual(events[0][0], "subagent.batch.created")
        self.assertEqual(events[0][1]["batch_id"], payload["batch_id"])
        for result in payload["results"]:
            task_events = [
                (name, event)
                for name, event in events
                if event.get("task_id") == result["task_id"]
            ]
            self.assertEqual(
                [name for name, _event in task_events],
                [
                    "subagent.task.queued",
                    "subagent.task.started",
                    "subagent.task.completed",
                ],
            )
            self.assertTrue(
                all(event["batch_id"] == payload["batch_id"] for _name, event in task_events)
            )

    def test_result_processor_controls_public_summary_and_artifacts(self) -> None:
        coordinator = SubAgentCoordinator(
            config=SubAgentConfig(enabled=True),
            registry=_Registry([self._definition()]),
            tools_provider=lambda: {},
            execute_task=lambda *_args: SubAgentExecutionResult("raw secret", 1, 0),
            result_processor=lambda task_id, _agent, _description, text: SubAgentPublicResult(
                summary=f"safe:{text.split()[0]}",
                artifacts=({"artifact_path": f"artifacts/session/{task_id}.json"},),
            ),
        )

        payload = json.loads(coordinator.run(self._arguments()).output)

        self.assertEqual(payload["results"][0]["summary"], "safe:raw")
        self.assertTrue(payload["results"][0]["artifacts"][0]["artifact_path"].endswith(".json"))
        self.assertNotIn("secret", json.dumps(payload, ensure_ascii=False))

    def test_task_failure_is_isolated_and_batch_becomes_partial(self) -> None:
        calls = []

        def execute(_definition, _tools, description, _prompt, _cancel_check):
            calls.append(description)
            if description == "失败任务":
                raise RuntimeError("provider unavailable")
            return SubAgentExecutionResult(description, 1, 0)

        coordinator = SubAgentCoordinator(
            config=SubAgentConfig(enabled=True, max_concurrency=1),
            registry=_Registry([self._definition()]),
            tools_provider=lambda: {},
            execute_task=execute,
        )
        arguments = self._arguments()
        arguments["tasks"] = [
            {**arguments["tasks"][0], "description": "成功任务"},
            {**arguments["tasks"][0], "description": "失败任务"},
            {**arguments["tasks"][0], "description": "后续任务"},
        ]

        result = coordinator.run(arguments)
        payload = json.loads(result.output)

        self.assertFalse(result.ok)
        self.assertEqual(payload["status"], "partial")
        self.assertEqual(calls, ["成功任务", "失败任务", "后续任务"])
        self.assertEqual(
            [item["status"] for item in payload["results"]],
            ["completed", "failed", "completed"],
        )

    def test_fail_fast_stops_unscheduled_tasks(self) -> None:
        calls = []

        def execute(_definition, _tools, description, _prompt, _cancel_check):
            calls.append(description)
            raise RuntimeError("first failure")

        coordinator = SubAgentCoordinator(
            config=SubAgentConfig(enabled=True, max_concurrency=1),
            registry=_Registry([self._definition()]),
            tools_provider=lambda: {},
            execute_task=execute,
        )
        arguments = self._arguments()
        arguments["tasks"] = [
            {**arguments["tasks"][0], "description": "任务一"},
            {**arguments["tasks"][0], "description": "任务二"},
            {**arguments["tasks"][0], "description": "任务三"},
        ]
        arguments["fail_fast"] = True

        payload = json.loads(coordinator.run(arguments).output)

        self.assertEqual(calls, ["任务一"])
        self.assertEqual(payload["status"], "failed")
        self.assertEqual(
            [item["status"] for item in payload["results"]],
            ["failed", "cancelled", "cancelled"],
        )

    def test_background_spawn_list_get_cancel_and_default_denial(self) -> None:
        gate = threading.Event()
        coordinator = SubAgentCoordinator(
            config=SubAgentConfig(enabled=True, allow_background=True),
            registry=_Registry([self._definition()]),
            tools_provider=lambda: {},
            execute_task=lambda _definition, _tools, _description, _prompt, _cancel: (
                gate.wait(2),
                SubAgentExecutionResult("safe result", 1, 0),
            )[1],
        )
        arguments = self._arguments()
        arguments["action"] = "spawn"
        payload = json.loads(coordinator.run(arguments).output)
        self.assertEqual(payload["status"], "queued")
        task_id = payload["task_ids"][0]
        listed = json.loads(coordinator.run({"action": "list"}).output)
        self.assertEqual(listed["tasks"][0]["task_id"], task_id)
        self.assertTrue(json.loads(coordinator.run({"action": "cancel", "task_id": task_id}).output)["ok"])
        gate.set()
        for _ in range(100):
            current = json.loads(coordinator.run({"action": "get", "task_id": task_id}).output)["task"]
            if current["status"] == "cancelled":
                break
            time.sleep(0.01)
        self.assertEqual(current["status"], "cancelled")

    def test_public_task_control_methods_are_limited_to_current_session(self) -> None:
        """API/TUI 门面不能借由 Coordinator 查看或取消其他 Session 的后台任务。"""

        gate = threading.Event()
        session = {"id": "session-a"}
        coordinator = SubAgentCoordinator(
            config=SubAgentConfig(enabled=True, allow_background=True),
            registry=_Registry([self._definition()]),
            tools_provider=lambda: {},
            execute_task=lambda _definition, _tools, _description, _prompt, _cancel: (
                gate.wait(2),
                SubAgentExecutionResult("safe result", 1, 0),
            )[1],
            session_id_provider=lambda: session["id"],
        )
        arguments = self._arguments()
        arguments["action"] = "spawn"
        task_id = json.loads(coordinator.run(arguments).output)["task_ids"][0]
        try:
            self.assertEqual(coordinator.list_tasks()[0]["task_id"], task_id)
            self.assertEqual(coordinator.get_task(task_id)["task_id"], task_id)

            session["id"] = "session-b"
            self.assertEqual(coordinator.list_tasks(), [])
            self.assertIsNone(coordinator.get_task(task_id))
            self.assertFalse(coordinator.cancel_task(task_id=task_id)["ok"])

            session["id"] = "session-a"
            self.assertTrue(coordinator.cancel_task(task_id=task_id)["ok"])
        finally:
            gate.set()
            coordinator.cancel_and_wait(
                reason="test cleanup",
                timeout_seconds=1,
                permanent=True,
            )

    def test_background_freezes_parent_cancel_check_at_spawn(self) -> None:
        release = threading.Event()
        parent_calls = {"spawn": 0, "later": 0}

        def spawn_parent_check() -> None:
            parent_calls["spawn"] += 1

        def later_parent_check() -> None:
            parent_calls["later"] += 1
            raise RuntimeError("later run cancelled")

        def execute(_definition, _tools, _description, _prompt, cancel_check):
            release.wait(2)
            cancel_check()
            return SubAgentExecutionResult("done", 1, 0)

        current_check = {"value": spawn_parent_check}
        coordinator = SubAgentCoordinator(
            config=SubAgentConfig(enabled=True, allow_background=True),
            registry=_Registry([self._definition()]),
            tools_provider=lambda: {},
            execute_task=execute,
        )
        coordinator.set_cancel_check_provider(lambda: current_check["value"])
        arguments = self._arguments()
        arguments["action"] = "spawn"
        payload = json.loads(coordinator.run(arguments).output)
        task_id = payload["task_ids"][0]

        current_check["value"] = later_parent_check
        release.set()
        for _ in range(100):
            current = json.loads(
                coordinator.run({"action": "get", "task_id": task_id}).output
            )["task"]
            if current["status"] == "completed":
                break
            time.sleep(0.01)

        self.assertEqual(current["status"], "completed")
        self.assertGreater(parent_calls["spawn"], 0)
        self.assertEqual(parent_calls["later"], 0)
        coordinator.cancel_and_wait(
            reason="test cleanup",
            timeout_seconds=1,
            permanent=True,
        )

    def test_background_honors_requested_batch_concurrency(self) -> None:
        lock = threading.Lock()
        active = 0
        max_active = 0

        def execute(_definition, _tools, _description, _prompt, _cancel_check):
            nonlocal active, max_active
            with lock:
                active += 1
                max_active = max(max_active, active)
            time.sleep(0.03)
            with lock:
                active -= 1
            return SubAgentExecutionResult("done", 1, 0)

        coordinator = SubAgentCoordinator(
            config=SubAgentConfig(
                enabled=True,
                allow_background=True,
                max_concurrency=2,
            ),
            registry=_Registry([self._definition()]),
            tools_provider=lambda: {},
            execute_task=execute,
        )
        arguments = self._arguments()
        arguments["action"] = "spawn"
        arguments["max_concurrency"] = 1
        arguments["tasks"] = [
            {**arguments["tasks"][0], "description": "任务一"},
            {**arguments["tasks"][0], "description": "任务二"},
        ]
        payload = json.loads(coordinator.run(arguments).output)
        for _ in range(100):
            listed = json.loads(coordinator.run({"action": "list"}).output)[
                "tasks"
            ]
            if listed and all(item["status"] == "completed" for item in listed):
                break
            time.sleep(0.01)

        self.assertEqual(len(payload["task_ids"]), 2)
        self.assertEqual(max_active, 1)
        coordinator.cancel_and_wait(
            reason="test cleanup",
            timeout_seconds=1,
            permanent=True,
        )

    def test_lifecycle_cancel_covers_background_tasks_from_previous_session(self) -> None:
        session_id = {"value": "session-a"}
        started = threading.Event()

        def execute(_definition, _tools, _description, _prompt, cancel_check):
            started.set()
            while True:
                cancel_check()
                time.sleep(0.01)

        coordinator = SubAgentCoordinator(
            config=SubAgentConfig(enabled=True, allow_background=True),
            registry=_Registry([self._definition()]),
            tools_provider=lambda: {},
            execute_task=execute,
            session_id_provider=lambda: session_id["value"],
        )
        arguments = self._arguments()
        arguments["action"] = "spawn"
        coordinator.run(arguments)
        self.assertTrue(started.wait(1))
        session_id["value"] = "session-b"

        self.assertTrue(
            coordinator.cancel_and_wait(
                reason="Agent close",
                timeout_seconds=1,
                permanent=True,
            )
        )

    def test_background_fail_fast_is_rejected_until_supported(self) -> None:
        coordinator = SubAgentCoordinator(
            config=SubAgentConfig(enabled=True, allow_background=True),
            registry=_Registry([self._definition()]),
            tools_provider=lambda: {},
            execute_task=lambda *_args: self.fail("invalid spawn must not execute"),
        )
        arguments = self._arguments()
        arguments["action"] = "spawn"
        arguments["fail_fast"] = True

        payload = json.loads(coordinator.run(arguments).output)

        self.assertEqual(payload["error"]["code"], "SUBAGENT_PERMISSION_DENIED")

    def test_background_requires_explicit_config(self) -> None:
        coordinator = SubAgentCoordinator(
            config=SubAgentConfig(enabled=True),
            registry=_Registry([self._definition()]),
            tools_provider=lambda: {},
            execute_task=lambda *_args: self.fail("后台默认关闭时不得执行"),
        )
        arguments = self._arguments()
        arguments["action"] = "spawn"
        payload = json.loads(coordinator.run(arguments).output)
        self.assertEqual(payload["error"]["code"], "SUBAGENT_BACKGROUND_DISABLED")

    def test_unknown_agent_type_has_stable_error(self) -> None:
        coordinator = SubAgentCoordinator(
            config=SubAgentConfig(enabled=True),
            registry=_Registry([]),
            tools_provider=lambda: {},
            execute_task=lambda *_args: self.fail("unknown agent must not execute"),
        )

        result = json.loads(
            coordinator.run(self._arguments(subagent_type="missing")).output
        )

        self.assertEqual(result["error"]["code"], "AGENT_TYPE_NOT_FOUND")

    def test_read_only_profile_intersects_parent_and_definition_tools(self) -> None:
        captured = {}
        parent_tools = {
            name: _tool(name)
            for name in (
                "read_file",
                "search_text",
                "memory_read",
                "write_file",
                "replace_text",
                "bash",
                "powershell",
                "monitor",
                "display_html",
                "load_skill",
                "trusted.read_resource",
                "restricted.mutate_resource",
                "memory_write",
                "subagent",
            )
        }

        def execute(definition, tools, description, prompt, _cancel_check):
            captured["definition"] = definition
            captured["tools"] = tools
            captured["description"] = description
            captured["prompt"] = prompt
            return SubAgentExecutionResult(
                final_text="找到证据",
                model_turns=2,
                tool_calls=3,
                input_tokens=100,
                output_tokens=20,
                cached_input_tokens=5,
            )

        coordinator = SubAgentCoordinator(
            config=SubAgentConfig(enabled=True),
            registry=_Registry([self._definition(tools=())]),
            tools_provider=lambda: parent_tools,
            execute_task=execute,
        )

        result = coordinator.run(self._arguments())
        payload = json.loads(result.output)

        self.assertTrue(result.ok)
        self.assertEqual(
            set(captured["tools"]),
            {
                "read_file",
                "search_text",
                "memory_read",
                "bash",
                "powershell",
                "monitor",
                "display_html",
                "load_skill",
                "trusted.read_resource",
                "restricted.mutate_resource",
            },
        )
        self.assertFalse(captured["tools"]["read_file"].requires_confirmation)
        self.assertFalse(captured["tools"]["search_text"].requires_confirmation)
        self.assertFalse(captured["tools"]["memory_read"].requires_confirmation)
        self.assertFalse(captured["tools"]["bash"].requires_confirmation)
        self.assertFalse(captured["tools"]["powershell"].requires_confirmation)
        self.assertFalse(captured["tools"]["monitor"].requires_confirmation)
        self.assertTrue(captured["tools"]["display_html"].requires_confirmation)
        self.assertTrue(captured["tools"]["restricted.mutate_resource"].requires_confirmation)
        self.assertEqual(payload["status"], "completed")
        self.assertTrue(payload["batch_id"].startswith("batch-"))
        self.assertTrue(payload["results"][0]["task_id"].startswith("task-"))
        self.assertEqual(payload["results"][0]["usage"]["model_turns"], 2)
        self.assertEqual(payload["results"][0]["usage"]["input_tokens"], 100)

    def test_definition_can_only_narrow_read_only_profile(self) -> None:
        definition = self._definition(disallowed_tools=("search_text", "memory_read"))
        captured = {}

        def execute(_definition, tools, _description, _prompt, _cancel_check):
            captured["tools"] = tools
            return SubAgentExecutionResult("ok", 1, 0)

        coordinator = SubAgentCoordinator(
            config=SubAgentConfig(enabled=True),
            registry=_Registry([definition]),
            tools_provider=lambda: {
                "read_file": _tool("read_file"),
                "search_text": _tool("search_text"),
                "memory_read": _tool("memory_read"),
                "write_file": _tool("write_file"),
            },
            execute_task=execute,
        )

        result = coordinator.run(self._arguments())

        self.assertTrue(result.ok)
        self.assertEqual(set(captured["tools"]), {"read_file"})

    def test_model_cannot_inject_tool_skill_or_mcp_capabilities(self) -> None:
        """任务参数只能选择 Host 定义，不能携带自定义权限声明。"""

        coordinator = SubAgentCoordinator(
            config=SubAgentConfig(enabled=True),
            registry=_Registry([self._definition()]),
            tools_provider=lambda: {},
            execute_task=lambda *_args: self.fail(
                "非法能力字段必须在准备执行前被拒绝"
            ),
        )

        forbidden_fields = {
            "tools": ["write_file"],
            "skills": ["privileged-skill"],
            "mcpServers": ["external-server"],
            "permissionMode": "standard",
        }
        for field, value in forbidden_fields.items():
            with self.subTest(field=field):
                payload = json.loads(
                    coordinator.run(self._arguments(**{field: value})).output
                )
                self.assertEqual(
                    payload["error"]["code"],
                    "SUBAGENT_PERMISSION_DENIED",
                )
                self.assertIn(field, payload["error"]["message"])

    def test_should_inherit_external_capabilities_when_profile_is_read_only(self) -> None:
        """read_only 继承父能力，只排除本地文件与父控制面写能力。"""

        allowed_tools = {
            "list_files",
            "read_file",
            "search_text",
            "memory_search",
            "memory_read",
            "memory_expand_related",
            "bash",
            "powershell",
            "monitor",
            "display_html",
            "load_skill",
            "search_skills",
            "switch_skill",
            "trusted.read_resource",
            "restricted.mutate_resource",
        }
        denied_tools = {
            "memory_write",
            "write_file",
            "replace_text",
            "subagent",
        }
        definition = self._definition(tools=())
        captured = {}

        def execute(_definition, tools, _description, _prompt, _cancel_check):
            captured["tools"] = tools
            return SubAgentExecutionResult("ok", 1, 0)

        coordinator = SubAgentCoordinator(
            config=SubAgentConfig(enabled=True),
            registry=_Registry([definition]),
            tools_provider=lambda: {
                name: _tool(name) for name in allowed_tools | denied_tools
            },
            execute_task=execute,
        )

        result = coordinator.run(self._arguments())

        self.assertTrue(result.ok)
        self.assertEqual(set(captured["tools"]), allowed_tools)
        self.assertTrue(denied_tools.isdisjoint(captured["tools"]))

    def test_should_allow_queries_and_reject_mutations_when_running_read_only_commands(self) -> None:
        captured = {}
        executed: list[dict] = []

        def command_tool(name: str) -> ToolDefinition:
            return ToolDefinition(
                name=name,
                description=name,
                argument_schema='{"command":"..."}',
                requires_confirmation=True,
                run=lambda arguments: executed.append(dict(arguments))
                or ToolResult(ok=True, output="executed"),
            )

        def execute(_definition, tools, _description, _prompt, _cancel_check):
            captured["tools"] = tools
            return SubAgentExecutionResult("ok", 1, 0)

        coordinator = SubAgentCoordinator(
            config=SubAgentConfig(enabled=True),
            registry=_Registry([self._definition(tools=())]),
            tools_provider=lambda: {
                "bash": command_tool("bash"),
                "powershell": command_tool("powershell"),
                "monitor": command_tool("monitor"),
            },
            execute_task=execute,
        )

        self.assertTrue(coordinator.run(self._arguments()).ok)
        bash = captured["tools"]["bash"]
        powershell = captured["tools"]["powershell"]
        monitor = captured["tools"]["monitor"]

        for arguments in (
            {"command": "git status --short"},
            {"command": "rg --files | head -20"},
            {"command": "curl -fsSL https://example.com"},
            {"command": "agent-browser-cli tabs"},
        ):
            with self.subTest(arguments=arguments):
                self.assertTrue(bash.run(arguments).ok)

        self.assertTrue(
            powershell.run({"command": "Get-ChildItem | Select-Object -First 5"}).ok
        )
        self.assertTrue(
            monitor.run(
                {
                    "action": "start",
                    "command": "agent-browser-cli scan --text-only",
                }
            ).ok
        )
        self.assertTrue(monitor.run({"action": "list"}).ok)

        blocked_calls = (
            (bash, {"command": "echo changed > main.py"}),
            (bash, {"command": "sed -i 's/a/b/' main.py"}),
            (bash, {"command": "git checkout -- main.py"}),
            (bash, {"command": "curl https://example.com -o news.html"}),
            (bash, {"command": "curl -X POST https://example.com/api"}),
            (bash, {"command": "curl --data key=value https://example.com/api"}),
            (powershell, {"command": "Set-Content -Path main.py -Value changed"}),
            (
                powershell,
                {
                    "command": (
                        "Invoke-RestMethod -Method POST "
                        "-Uri https://example.com/api"
                    )
                },
            ),
            (monitor, {"action": "start", "command": "python update_files.py"}),
        )
        for tool, arguments in blocked_calls:
            with self.subTest(arguments=arguments):
                result = tool.run(arguments)
                self.assertFalse(result.ok)
                self.assertIn("read-only", result.output)

        self.assertEqual(len(executed), 7)

    def test_standard_profile_does_not_open_mcp_skill_or_memory_write(self) -> None:
        """通用写 Agent 只能写工作区，不能顺带获得全局 Memory/MCP/Skill 控制面。"""

        allowed_tools = {
            "list_files",
            "read_file",
            "search_text",
            "memory_search",
            "memory_read",
            "memory_expand_related",
            "write_file",
            "replace_text",
            "bash",
            "powershell",
        }
        denied_tools = {
            "memory_write",
            "load_skill",
            "search_skills",
            "switch_skill",
            "trusted.read_resource",
            "restricted.mutate_resource",
            "subagent",
        }
        definition = self._definition(
            permission_mode="standard",
            isolation="shared",
            tools=tuple(sorted(allowed_tools | denied_tools)),
        )
        captured = {}

        def execute(_definition, tools, _description, _prompt, _cancel_check):
            captured["tools"] = tools
            return SubAgentExecutionResult("ok", 1, 0)

        coordinator = SubAgentCoordinator(
            config=SubAgentConfig(
                enabled=True,
                allow_standard_agent=True,
                allow_shared_workspace_writes=True,
            ),
            registry=_Registry([definition]),
            tools_provider=lambda: {
                name: _tool(name) for name in allowed_tools | denied_tools
            },
            execute_task=execute,
        )

        result = coordinator.run(self._arguments())

        self.assertTrue(result.ok)
        self.assertEqual(set(captured["tools"]), allowed_tools)
        self.assertTrue(denied_tools.isdisjoint(captured["tools"]))

    def test_unsupported_definition_capabilities_are_rejected(self) -> None:
        definitions = (
            self._definition(background=True),
            self._definition(permission_mode="standard"),
            self._definition(isolation="worktree"),
            self._definition(skills=("some-skill",)),
            self._definition(mcp_servers=("external",)),
        )
        for definition in definitions:
            with self.subTest(definition=definition):
                coordinator = SubAgentCoordinator(
                    config=SubAgentConfig(enabled=True),
                    registry=_Registry([definition]),
                    tools_provider=lambda: {},
                    execute_task=lambda *_args: self.fail("unsupported definition must not execute"),
                )
                payload = json.loads(coordinator.run(self._arguments()).output)
                self.assertEqual(payload["error"]["code"], "AGENT_DEFINITION_INVALID")

    def test_parent_cancellation_is_re_raised_instead_of_becoming_tool_failure(self) -> None:
        class RunCancelled(RuntimeError):
            pass

        def execute(*_args):
            raise RunCancelled("cancelled")

        events = []
        coordinator = SubAgentCoordinator(
            config=SubAgentConfig(enabled=True),
            registry=_Registry([self._definition()]),
            tools_provider=lambda: {"read_file": _tool("read_file")},
            execute_task=execute,
            event_sink=lambda name, payload: events.append((name, payload)),
        )

        with self.assertRaises(RunCancelled):
            coordinator.run(self._arguments())

        self.assertEqual(events[-1][0], "subagent.task.cancelled")
        self.assertEqual(events[-1][1]["error"]["code"], "SUBAGENT_CANCELLED")

    def test_keyboard_interrupt_emits_cancelled_terminal_before_propagation(self) -> None:
        events = []

        def execute(*_args):
            raise KeyboardInterrupt("用户取消当前任务")

        coordinator = SubAgentCoordinator(
            config=SubAgentConfig(enabled=True),
            registry=_Registry([self._definition()]),
            tools_provider=lambda: {},
            execute_task=execute,
            event_sink=lambda name, payload: events.append((name, payload)),
        )

        with self.assertRaises(KeyboardInterrupt):
            coordinator.run(self._arguments())

        terminal = [
            (name, payload)
            for name, payload in events
            if name in {
                "subagent.task.completed",
                "subagent.task.failed",
                "subagent.task.cancelled",
            }
        ]
        self.assertEqual(len(terminal), 1)
        self.assertEqual(terminal[0][0], "subagent.task.cancelled")
        self.assertEqual(terminal[0][1]["error"]["message"], "用户取消当前任务")

    def test_keyboard_interrupt_is_not_replaced_by_cancel_event_failure(self) -> None:
        def execute(*_args):
            raise KeyboardInterrupt("用户取消当前任务")

        def event_sink(name, _payload):
            if name == "subagent.task.cancelled":
                raise OSError("session disk unavailable")

        coordinator = SubAgentCoordinator(
            config=SubAgentConfig(enabled=True),
            registry=_Registry([self._definition()]),
            tools_provider=lambda: {},
            execute_task=execute,
            event_sink=event_sink,
        )

        with self.assertRaisesRegex(KeyboardInterrupt, "用户取消当前任务"):
            coordinator.run(self._arguments())

    def test_batch_event_failure_cleans_all_unsubmitted_tasks(self) -> None:
        def event_sink(name, _payload):
            if name == "subagent.batch.created":
                raise OSError("session disk unavailable")

        coordinator = SubAgentCoordinator(
            config=SubAgentConfig(enabled=True),
            registry=_Registry([self._definition()]),
            tools_provider=lambda: {},
            execute_task=lambda *_args: self.fail("batch event failure must not execute"),
            event_sink=event_sink,
        )
        arguments = self._arguments()
        arguments["tasks"] = [
            {**arguments["tasks"][0], "description": f"任务 {index}"}
            for index in range(3)
        ]

        with self.assertRaisesRegex(OSError, "session disk unavailable"):
            coordinator.run(arguments)

        self.assertEqual(coordinator._active_batches, {})
        self.assertTrue(
            coordinator.cancel_and_wait(
                reason="确认清理完成。",
                timeout_seconds=0.1,
                permanent=False,
            )
        )

    def test_fail_fast_keyboard_interrupt_cancels_unsubmitted_tasks(self) -> None:
        events = []

        def execute(*_args):
            raise KeyboardInterrupt("用户取消当前任务")

        coordinator = SubAgentCoordinator(
            config=SubAgentConfig(enabled=True, max_concurrency=1),
            registry=_Registry([self._definition()]),
            tools_provider=lambda: {},
            execute_task=execute,
            event_sink=lambda name, payload: events.append((name, payload)),
        )
        arguments = self._arguments()
        arguments["tasks"] = [
            {**arguments["tasks"][0], "description": f"任务 {index}"}
            for index in range(3)
        ]
        arguments["fail_fast"] = True
        arguments["max_concurrency"] = 1

        with self.assertRaises(KeyboardInterrupt):
            coordinator.run(arguments)

        terminal = [
            payload
            for name, payload in events
            if name in {
                "subagent.task.completed",
                "subagent.task.failed",
                "subagent.task.cancelled",
            }
        ]
        self.assertEqual(len(terminal), 3)
        self.assertTrue(all(item["status"] == "cancelled" for item in terminal))
        self.assertEqual(len({item["task_id"] for item in terminal}), 3)

    def test_public_model_error_does_not_echo_provider_request_body(self) -> None:
        def execute(*_args):
            raise RuntimeError(
                'provider rejected request messages=[{"content":"完整子任务 prompt"}]'
            )

        coordinator = SubAgentCoordinator(
            config=SubAgentConfig(enabled=True),
            registry=_Registry([self._definition()]),
            tools_provider=lambda: {},
            execute_task=execute,
        )

        payload = json.loads(coordinator.run(self._arguments()).output)

        self.assertEqual(
            payload["results"][0]["error"]["message"],
            "子任务模型请求失败。",
        )
        self.assertNotIn("完整子任务 prompt", json.dumps(payload, ensure_ascii=False))
        self.assertEqual(
            payload["results"][0]["error"]["diagnostic"]["category"],
            "UNKNOWN",
        )
        self.assertEqual(
            payload["results"][0]["error"]["diagnostic"]["exception_type"],
            "RuntimeError",
        )

    def test_failure_messages_are_redacted_before_publication(self) -> None:
        def execute(*_args):
            raise RuntimeError("provider failed api_key=very-secret")

        coordinator = SubAgentCoordinator(
            config=SubAgentConfig(enabled=True),
            registry=_Registry([self._definition()]),
            tools_provider=lambda: {},
            execute_task=execute,
        )

        payload = json.loads(coordinator.run(self._arguments()).output)

        self.assertNotIn("very-secret", json.dumps(payload, ensure_ascii=False))
        self.assertEqual(
            payload["results"][0]["error"]["message"],
            "子任务模型请求失败。",
        )

    def test_budget_error_returns_failed_structured_result(self) -> None:
        def execute(*_args):
            raise AgentLoopBudgetExceeded("达到模型回合预算")

        coordinator = SubAgentCoordinator(
            config=SubAgentConfig(enabled=True),
            registry=_Registry([self._definition()]),
            tools_provider=lambda: {"read_file": _tool("read_file")},
            execute_task=execute,
        )

        result = coordinator.run(self._arguments())
        payload = json.loads(result.output)

        self.assertFalse(result.ok)
        self.assertEqual(payload["status"], "failed")
        self.assertEqual(payload["results"][0]["error"]["code"], "SUBAGENT_LIMIT_EXCEEDED")

    def test_cancel_and_wait_cascades_to_running_child(self) -> None:
        started = threading.Event()
        run_error = []
        events = []

        def execute(_definition, _tools, _description, _prompt, cancel_check):
            started.set()
            while True:
                cancel_check()
                time.sleep(0.01)

        coordinator = SubAgentCoordinator(
            config=SubAgentConfig(enabled=True),
            registry=_Registry([self._definition()]),
            tools_provider=lambda: {},
            execute_task=execute,
            event_sink=lambda name, payload: events.append((name, payload)),
        )

        def run_batch() -> None:
            try:
                coordinator.run(self._arguments())
            except BaseException as exc:  # 测试线程必须保留取消异常供主线程断言。
                run_error.append(exc)

        worker = threading.Thread(target=run_batch)
        worker.start()
        self.assertTrue(started.wait(timeout=1))

        self.assertTrue(
            coordinator.cancel_and_wait(
                reason="Agent 正在关闭。",
                timeout_seconds=1,
                permanent=True,
            )
        )
        worker.join(timeout=1)

        self.assertFalse(worker.is_alive())
        self.assertEqual(len(run_error), 1)
        self.assertIn("cancel", type(run_error[0]).__name__.casefold())
        terminal = [name for name, _payload in events if name.endswith(".cancelled")]
        self.assertEqual(terminal, ["subagent.task.cancelled"])

    def test_cancel_and_wait_uses_bounded_deadline_for_uncooperative_child(self) -> None:
        started = threading.Event()
        release = threading.Event()
        run_error = []

        def execute(_definition, _tools, _description, _prompt, _cancel_check):
            started.set()
            release.wait(timeout=2)
            return SubAgentExecutionResult("late result", 1, 0)

        coordinator = SubAgentCoordinator(
            config=SubAgentConfig(enabled=True),
            registry=_Registry([self._definition()]),
            tools_provider=lambda: {},
            execute_task=execute,
        )

        def run_batch() -> None:
            try:
                coordinator.run(self._arguments())
            except BaseException as exc:  # 测试线程必须保留取消异常供主线程断言。
                run_error.append(exc)

        worker = threading.Thread(target=run_batch)
        worker.start()
        self.assertTrue(started.wait(timeout=1))

        started_at = time.monotonic()
        drained = coordinator.cancel_and_wait(
            reason="工作区即将切换。",
            timeout_seconds=0.05,
            permanent=False,
        )
        elapsed = time.monotonic() - started_at

        self.assertFalse(drained)
        self.assertLess(elapsed, 0.3)
        rejected = json.loads(coordinator.run(self._arguments()).output)
        self.assertEqual(rejected["error"]["code"], "SUBAGENT_CANCELLED")

        coordinator.resume_accepting_when_idle()
        release.set()
        worker.join(timeout=1)
        self.assertFalse(worker.is_alive())
        self.assertEqual(len(run_error), 1)

    def test_slow_cancel_event_sink_cannot_break_wait_deadline(self) -> None:
        running_started = threading.Event()
        running_release = threading.Event()
        sink_started = threading.Event()
        sink_release = threading.Event()
        run_error = []

        def execute(_definition, _tools, description, _prompt, _cancel_check):
            if description == "运行任务":
                running_started.set()
                running_release.wait(timeout=2)
            return SubAgentExecutionResult(description, 1, 0)

        def event_sink(name, payload):
            if name == "subagent.task.cancelled" and payload["description"] == "排队任务":
                sink_started.set()
                sink_release.wait(timeout=2)

        coordinator = SubAgentCoordinator(
            config=SubAgentConfig(enabled=True, max_concurrency=1),
            registry=_Registry([self._definition()]),
            tools_provider=lambda: {},
            execute_task=execute,
            event_sink=event_sink,
        )
        arguments = self._arguments()
        arguments["tasks"] = [
            {**arguments["tasks"][0], "description": "运行任务"},
            {**arguments["tasks"][0], "description": "排队任务"},
        ]
        arguments["max_concurrency"] = 1

        def run_batch() -> None:
            try:
                coordinator.run(arguments)
            except BaseException as exc:
                run_error.append(exc)

        worker = threading.Thread(target=run_batch)
        worker.start()
        self.assertTrue(running_started.wait(timeout=1))

        started_at = time.monotonic()
        drained = coordinator.cancel_and_wait(
            reason="Agent 正在关闭。",
            timeout_seconds=0.05,
            permanent=False,
        )
        elapsed = time.monotonic() - started_at

        self.assertFalse(drained)
        self.assertLess(elapsed, 0.2)
        self.assertTrue(sink_started.wait(timeout=1))

        coordinator.resume_accepting_when_idle()
        sink_release.set()
        running_release.set()
        worker.join(timeout=1)
        self.assertFalse(worker.is_alive())
        self.assertEqual(len(run_error), 1)

    def test_lifecycle_cancellation_revokes_active_broker_approval(self) -> None:
        approval_visible = threading.Event()
        release_approval = threading.Event()
        run_error = []

        def approve(_request) -> bool:
            approval_visible.set()
            release_approval.wait(timeout=1)
            return True

        broker = ApprovalBroker(approve=approve)

        def execute(_definition, _tools, _description, _prompt, cancel_check):
            scope = current_subagent_approval_scope()
            self.assertIsNotNone(scope)
            approved = scope.broker.request(
                origin=scope.origin,
                tool_name="powershell",
                public_arguments={"command": "git commit -m demo"},
                risk_summary="Git 变更操作",
                cancel_check=cancel_check,
            )
            self.assertFalse(approved)
            return SubAgentExecutionResult("cancelled approval", 1, 1)

        coordinator = SubAgentCoordinator(
            config=SubAgentConfig(enabled=True),
            registry=_Registry([self._definition()]),
            tools_provider=lambda: {},
            execute_task=execute,
            approval_broker=broker,
        )

        def run_batch() -> None:
            try:
                coordinator.run(self._arguments())
            except BaseException as exc:
                run_error.append(exc)

        worker = threading.Thread(target=run_batch)
        worker.start()
        self.assertTrue(approval_visible.wait(timeout=1))

        self.assertTrue(
            coordinator.cancel_and_wait(
                reason="父任务已取消。",
                timeout_seconds=1,
                permanent=False,
            )
        )
        worker.join(timeout=1)
        self.assertFalse(worker.is_alive())
        self.assertEqual(len(run_error), 1)
        self.assertIn("cancel", type(run_error[0]).__name__.casefold())

        release_approval.set()
        deadline = time.monotonic() + 1
        while broker.pending_count and time.monotonic() < deadline:
            time.sleep(0.01)
        self.assertEqual(broker.pending_count, 0)

    def test_cancel_event_failure_does_not_leak_remaining_batch_tasks(self) -> None:
        running_started = threading.Event()
        running_release = threading.Event()
        run_error = []
        failed_once = {"value": False}

        def execute(_definition, _tools, description, _prompt, _cancel_check):
            if description == "运行任务":
                running_started.set()
                running_release.wait(timeout=2)
            return SubAgentExecutionResult(description, 1, 0)

        def event_sink(name, payload):
            if (
                name == "subagent.task.cancelled"
                and payload["description"] == "排队任务一"
                and not failed_once["value"]
            ):
                failed_once["value"] = True
                raise OSError("session disk unavailable")

        coordinator = SubAgentCoordinator(
            config=SubAgentConfig(enabled=True, max_concurrency=1),
            registry=_Registry([self._definition()]),
            tools_provider=lambda: {},
            execute_task=execute,
            event_sink=event_sink,
        )
        arguments = self._arguments()
        arguments["tasks"] = [
            {**arguments["tasks"][0], "description": "运行任务"},
            {**arguments["tasks"][0], "description": "排队任务一"},
            {**arguments["tasks"][0], "description": "排队任务二"},
        ]
        arguments["max_concurrency"] = 1

        def run_batch() -> None:
            try:
                coordinator.run(arguments)
            except BaseException as exc:
                run_error.append(exc)

        worker = threading.Thread(target=run_batch)
        worker.start()
        self.assertTrue(running_started.wait(timeout=1))
        self.assertFalse(
            coordinator.cancel_and_wait(
                reason="Agent 正在关闭。",
                timeout_seconds=0.05,
                permanent=False,
            )
        )

        coordinator.resume_accepting_when_idle()
        running_release.set()
        worker.join(timeout=1)
        self.assertFalse(worker.is_alive())
        self.assertTrue(
            coordinator.cancel_and_wait(
                reason="确认批次已清理。",
                timeout_seconds=1,
                permanent=False,
            )
        )
        self.assertEqual(coordinator._active_batches, {})
        self.assertEqual(len(run_error), 1)


if __name__ == "__main__":
    unittest.main()
