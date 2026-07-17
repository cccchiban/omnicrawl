from __future__ import annotations

import json
import os
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from omnicrawl.agent import LocalToolAgent
from omnicrawl.agent.subagents.coordinator import (
    SubAgentCoordinator,
    SubAgentExecutionResult,
)
from omnicrawl.agent.subagents.definitions import AgentDefinition
from omnicrawl.agent.subagents.verify import (
    VERIFY_COMMAND_TOOL_NAME,
    build_verify_command_tool,
)
from omnicrawl.agent.types import ToolDefinition, ToolResult
from omnicrawl.config.subagents import SubAgentConfig, load_subagent_config
from omnicrawl.workspace.tools import WorkspaceCommandResult, WorkspaceToolError, WorkspaceTools


class _Registry:
    """为 Coordinator 测试提供最小、确定的定义查询入口。"""

    def __init__(self, definition: AgentDefinition) -> None:
        self._definition = definition

    def get(self, name: str) -> AgentDefinition | None:
        return self._definition if name == self._definition.name else None

    def list_all(self) -> list[AgentDefinition]:
        return [self._definition]


def _tool(name: str) -> ToolDefinition:
    return ToolDefinition(
        name=name,
        description=name,
        argument_schema="{}",
        requires_confirmation=True,
        run=lambda _arguments: ToolResult(ok=True, output=name),
    )


class _FakeWorkspaceTools:
    """记录受控命令底座收到的 argv，避免测试真正执行项目测试集。"""

    def __init__(self) -> None:
        self.invocations: list[tuple[tuple[str, ...], int, str]] = []

    def run_argv_command(
        self,
        arguments: tuple[str, ...],
        *,
        timeout_seconds: int,
        label: str,
    ) -> WorkspaceCommandResult:
        self.invocations.append((arguments, timeout_seconds, label))
        return WorkspaceCommandResult(ok=True, output="验证完成")


class VerifyCommandToolTest(unittest.TestCase):
    def test_fixed_check_is_resolved_to_static_argv_without_shell_input(self) -> None:
        workspace_tools = _FakeWorkspaceTools()
        tool = build_verify_command_tool(
            workspace_tools,
            max_timeout_seconds=120,
        )

        result = tool.run({"check": "unit_tests"})

        self.assertTrue(result.ok)
        self.assertEqual(tool.name, VERIFY_COMMAND_TOOL_NAME)
        self.assertFalse(tool.requires_confirmation)
        self.assertEqual(len(workspace_tools.invocations), 1)
        command, timeout, label = workspace_tools.invocations[0]
        self.assertEqual(
            command,
            (
                sys.executable,
                "-m",
                "unittest",
                "discover",
                "-s",
                "tests",
                "-q",
            ),
        )
        self.assertEqual(timeout, 120)
        self.assertIn("单元测试", label)

    def test_fixed_check_is_a_serial_tool_batch_barrier(self) -> None:
        tool = build_verify_command_tool(_FakeWorkspaceTools(), max_timeout_seconds=120)

        self.assertTrue(
            LocalToolAgent._tool_call_requires_serial_execution(
                tool,
                {"check": "compileall"},
            )
        )

    def test_model_cannot_supply_raw_command_shell_or_unknown_check(self) -> None:
        workspace_tools = _FakeWorkspaceTools()
        tool = build_verify_command_tool(
            workspace_tools,
            max_timeout_seconds=90,
        )

        for arguments, message in (
            ({"check": "python -c 'import os'"}, "check"),
            ({"check": "unit_tests", "command": "Remove-Item ."}, "不支持参数"),
            ({"check": "unit_tests", "shell": "powershell"}, "不支持参数"),
            ({"check": "unit_tests", "timeout_seconds": 91}, "timeout_seconds"),
        ):
            with self.subTest(arguments=arguments):
                result = tool.run(arguments)
                self.assertFalse(result.ok)
                self.assertIn(message, result.output)

        self.assertEqual(workspace_tools.invocations, [])


class VerifyProfileCoordinatorTest(unittest.TestCase):
    def _arguments(self) -> dict:
        return {
            "action": "run",
            "tasks": [
                {
                    "description": "验证当前工作区",
                    "prompt": "运行需要的固定验证检查并汇总结论。",
                    "subagent_type": "verify",
                    "context": "fresh",
                }
            ],
        }

    def _definition(self) -> AgentDefinition:
        return AgentDefinition(
            name="verify",
            description="受控验证",
            system_prompt="只运行 Host 提供的固定检查。",
            # 定义即使错误地要求 Bash/写文件，Coordinator 也只能把权限收窄为
            # read-only + verify_command，不能借 definition 反向扩大 Host 权限。
            tools=("read_file", VERIFY_COMMAND_TOOL_NAME, "bash", "write_file"),
            disallowed_tools=(),
            permission_mode="explicit-command-allowlist",
            background=True,
            source="builtin",
        )

    def test_verify_profile_is_rejected_until_explicitly_enabled(self) -> None:
        coordinator = SubAgentCoordinator(
            config=SubAgentConfig(enabled=True, enable_verify_agent=False),
            registry=_Registry(self._definition()),
            tools_provider=lambda: {},
            execute_task=lambda *_args: self.fail("disabled verify must not execute"),
            verify_tools_provider=lambda: {},
        )

        result = coordinator.run(self._arguments())
        payload = json.loads(result.output)

        self.assertFalse(result.ok)
        self.assertEqual(payload["error"]["code"], "AGENT_DEFINITION_INVALID")
        self.assertIn("enable_verify_agent", payload["error"]["message"])

    def test_verify_profile_only_receives_read_tools_and_fixed_check_tool(self) -> None:
        captured: dict[str, dict[str, ToolDefinition]] = {}
        verify_tool = build_verify_command_tool(_FakeWorkspaceTools(), max_timeout_seconds=120)

        def execute(_definition, tools, _description, _prompt, _cancel_check):
            captured["tools"] = dict(tools)
            return SubAgentExecutionResult("验证通过", 1, 1)

        coordinator = SubAgentCoordinator(
            config=SubAgentConfig(enabled=True, enable_verify_agent=True),
            registry=_Registry(self._definition()),
            tools_provider=lambda: {
                "read_file": _tool("read_file"),
                "bash": _tool("bash"),
                "powershell": _tool("powershell"),
                "write_file": _tool("write_file"),
            },
            execute_task=execute,
            verify_tools_provider=lambda: {VERIFY_COMMAND_TOOL_NAME: verify_tool},
        )

        result = coordinator.run(self._arguments())

        self.assertTrue(result.ok)
        self.assertEqual(set(captured["tools"]), {"read_file", VERIFY_COMMAND_TOOL_NAME})
        self.assertTrue(
            all(not tool.requires_confirmation for tool in captured["tools"].values())
        )


class VerifyConfigTest(unittest.TestCase):
    def test_verify_config_is_default_off_and_environment_can_only_tighten(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            path = Path(temp_dir) / "config.json"
            path.write_text(
                json.dumps(
                    {
                        "subagents": {
                            "enabled": True,
                            "enable_verify_agent": True,
                            "verify_command_timeout_seconds": 120,
                        }
                    }
                ),
                encoding="utf-8",
            )
            enabled = load_subagent_config(path)
            with patch.dict(
                os.environ,
                {
                    "OMNICRAWL_SUBAGENT_VERIFY_AGENT_ENABLED": "false",
                    "OMNICRAWL_SUBAGENT_VERIFY_TIMEOUT_SECONDS": "30",
                },
                clear=False,
            ):
                tightened = load_subagent_config(path)

        self.assertTrue(enabled.enable_verify_agent)
        self.assertEqual(enabled.verify_command_timeout_seconds, 120)
        self.assertFalse(tightened.enable_verify_agent)
        self.assertEqual(tightened.verify_command_timeout_seconds, 30)

    def test_environment_cannot_enable_or_widen_verify_capability(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            path = Path(temp_dir) / "config.json"
            path.write_text(
                json.dumps(
                    {
                        "subagents": {
                            "enable_verify_agent": False,
                            "verify_command_timeout_seconds": 45,
                        }
                    }
                ),
                encoding="utf-8",
            )
            with patch.dict(
                os.environ,
                {
                    "OMNICRAWL_SUBAGENT_VERIFY_AGENT_ENABLED": "true",
                    "OMNICRAWL_SUBAGENT_VERIFY_TIMEOUT_SECONDS": "360",
                },
                clear=False,
            ):
                config = load_subagent_config(path)

        self.assertFalse(config.enable_verify_agent)
        self.assertEqual(config.verify_command_timeout_seconds, 45)


class WorkspaceArgvCommandTest(unittest.TestCase):
    def test_direct_argv_command_runs_without_a_shell_interpreter(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            result = WorkspaceTools(Path(temp_dir)).run_argv_command(
                (sys.executable, "-c", "print('argv-ok')"),
                timeout_seconds=10,
                label="argv-test",
            )

        self.assertTrue(result.ok)
        self.assertIn("命令：argv-test", result.output)
        self.assertIn("argv-ok", result.output)

    def test_direct_argv_command_rejects_empty_or_non_string_arguments(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            tools = WorkspaceTools(Path(temp_dir))
            with self.assertRaisesRegex(WorkspaceToolError, "不能为空"):
                tools.run_argv_command((), timeout_seconds=10, label="empty")
            with self.assertRaisesRegex(WorkspaceToolError, "字符串"):
                tools.run_argv_command((sys.executable, 1), timeout_seconds=10, label="bad")  # type: ignore[arg-type]


if __name__ == "__main__":
    unittest.main()
