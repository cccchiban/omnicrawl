from __future__ import annotations

import json
import sys
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch

from ai_voice_agent.agent import LocalToolAgent, ToolCall, ToolDefinition
from ai_voice_agent.agent_tools import normalize_tool_call
from ai_voice_agent.mcp.client import MCPClientManager, _resolve_stdio_command
from ai_voice_agent.mcp.config import MCPConfig, MCPConfigError, MCPServerConfig, load_mcp_config
from ai_voice_agent.mcp.registry import MCPPromptMeta, MCPResourceMeta, MCPToolMeta, namespace_capability_name
from ai_voice_agent.mcp.server import LocalMCPServer
from ai_voice_agent.mcp.security import mcp_tool_requires_confirmation
from ai_voice_agent.slash_commands import build_slash_commands, format_mcp_status


class MCPConfigTest(unittest.TestCase):
    def test_load_mcp_config_defaults_to_disabled(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            config_path = Path(temp_dir) / "config.json"

            config = load_mcp_config(config_path)

        self.assertFalse(config.enabled)
        self.assertEqual(config.default_timeout_seconds, 30)
        self.assertEqual(config.servers, {})

    def test_load_mcp_config_validates_stdio_server(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            config_path = Path(temp_dir) / "config.json"
            config_path.write_text(
                json.dumps(
                    {
                        "mcp": {
                            "enabled": True,
                            "servers": {
                                "demo_server": {
                                    "enabled": True,
                                    "transport": "stdio",
                                    "command": "python",
                                    "args": ["-m", "demo"],
                                    "env": {"SAFE_FLAG": "1"},
                                    "timeout_seconds": 3,
                                    "risk_level": "trusted",
                                }
                            },
                        }
                    }
                ),
                encoding="utf-8",
            )

            config = load_mcp_config(config_path)

        server = config.servers["demo_server"]
        self.assertTrue(config.enabled)
        self.assertEqual(server.command, "python")
        self.assertEqual(server.args, ["-m", "demo"])
        self.assertEqual(server.timeout_seconds, 3)
        self.assertEqual(server.risk_level, "trusted")

    def test_load_mcp_config_rejects_invalid_name_and_missing_command(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            config_path = Path(temp_dir) / "config.json"
            config_path.write_text(
                json.dumps(
                    {
                        "mcp": {
                            "enabled": True,
                            "servers": {
                                "Bad.Name": {
                                    "enabled": True,
                                    "transport": "stdio",
                                    "command": "python",
                                }
                            },
                        }
                    }
                ),
                encoding="utf-8",
            )

            with self.assertRaisesRegex(MCPConfigError, "名称只能包含"):
                load_mcp_config(config_path)

            config_path.write_text(
                json.dumps(
                    {
                        "mcp": {
                            "enabled": True,
                            "servers": {
                                "demo": {
                                    "enabled": True,
                                    "transport": "stdio",
                                }
                            },
                        }
                    }
                ),
                encoding="utf-8",
            )

            with self.assertRaisesRegex(MCPConfigError, "command"):
                load_mcp_config(config_path)


class MCPSecurityTest(unittest.TestCase):
    def test_namespace_capability_name(self) -> None:
        self.assertEqual(namespace_capability_name("local_project", "read_file"), "local_project.read_file")

    def test_write_like_tool_requires_confirmation(self) -> None:
        config = MCPConfig(enabled=True)
        meta = MCPToolMeta(
            logical_name="trusted.write_file",
            server_name="trusted",
            tool_name="write_file",
            description="write",
            risk_level="trusted",
        )

        self.assertTrue(mcp_tool_requires_confirmation(meta, config.policy))

    def test_trusted_read_tool_can_skip_confirmation(self) -> None:
        config = MCPConfig(enabled=True)
        meta = MCPToolMeta(
            logical_name="trusted.read_file",
            server_name="trusted",
            tool_name="read_file",
            description="read",
            risk_level="trusted",
        )

        self.assertFalse(mcp_tool_requires_confirmation(meta, config.policy))


class MCPManagerTest(unittest.TestCase):
    def test_enabled_without_servers_records_degraded_diagnostic(self) -> None:
        manager = MCPClientManager(MCPConfig(enabled=True))

        manager.discover()

        self.assertIn("没有启用的 Server", manager.format_status())
        self.assertEqual(manager.registry.tools, {})

    def test_call_tool_normalizes_manager_result(self) -> None:
        manager = MCPClientManager(MCPConfig(enabled=True))
        manager.registry.add_tool(
            MCPToolMeta(
                logical_name="demo.echo",
                server_name="demo",
                tool_name="echo",
                description="echo",
            )
        )

        result = manager.call_tool("demo.echo", {"text": "hi"})

        self.assertFalse(result.ok)
        self.assertEqual(result.error_code, "SERVER_UNAVAILABLE")
        self.assertTrue(result.retryable)

    def test_resolve_stdio_command_uses_path_lookup(self) -> None:
        with patch(
            "ai_voice_agent.mcp.client.shutil.which",
            return_value=r"C:\Program Files\nodejs\npx.CMD",
        ) as which:
            resolved = _resolve_stdio_command("npx")

        which.assert_called_once_with("npx")
        self.assertEqual(resolved, r"C:\Program Files\nodejs\npx.CMD")

    def test_stdio_local_server_discovers_and_calls_capabilities(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            workspace = Path(temp_dir)
            (workspace / "README.md").write_text("hello mcp", encoding="utf-8")
            config = MCPConfig(
                enabled=True,
                servers={
                    "local_project": MCPServerConfig(
                        name="local_project",
                        enabled=True,
                        transport="stdio",
                        command="python",
                        args=["-m", "ai_voice_agent.mcp.server"],
                        timeout_seconds=5,
                        risk_level="trusted",
                    )
                },
            )
            manager = MCPClientManager(config, workspace_root=workspace, approval_mode_getter=lambda: "manual")

            manager.discover()
            tool_result = manager.call_tool(
                "local_project.workspace.read_file",
                {"path": "README.md", "max_lines": 5},
            )
            resource_result = manager.read_resource("local_project:project://README.md")
            prompt_result = manager.get_prompt(
                "local_project.code_review",
                {"path": "README.md", "focus": "bug"},
            )
            manager.close()

        self.assertTrue(tool_result.ok)
        self.assertIn("hello mcp", tool_result.output)
        self.assertTrue(resource_result.ok)
        self.assertIn("hello mcp", resource_result.output)
        self.assertTrue(prompt_result.ok)
        self.assertIn("代码审查", prompt_result.output)

    def test_schema_validation_and_audit_redaction(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            workspace = Path(temp_dir)
            config = MCPConfig(enabled=True)
            manager = MCPClientManager(config, workspace_root=workspace, approval_mode_getter=lambda: "manual")
            manager.registry.add_tool(
                MCPToolMeta(
                    logical_name="demo.echo",
                    server_name="demo",
                    tool_name="echo",
                    description="echo",
                    input_schema={
                        "type": "object",
                        "properties": {"text": {"type": "string", "maxLength": 3}},
                        "required": ["text"],
                    },
                    requires_confirmation=False,
                )
            )

            result = manager.call_tool("demo.echo", {"text": "too-long", "api_key": "secret"})

            audit_path = workspace / "logs" / "mcp-audit.jsonl"
            audit_event = json.loads(audit_path.read_text(encoding="utf-8").splitlines()[-1])

        self.assertFalse(result.ok)
        self.assertEqual(result.error_code, "SCHEMA_INVALID")
        self.assertEqual(audit_event["arguments_redacted"]["api_key"], "***")


class MCPAgentCommandTest(unittest.TestCase):
    def test_slash_commands_include_mcp_and_status_uses_agent(self) -> None:
        class FakeAgent:
            skill_manager = None

            def format_mcp_status(self) -> str:
                return "MCP 已关闭。"

        agent = FakeAgent()

        self.assertIn("/mcp", build_slash_commands(agent))
        self.assertEqual(format_mcp_status(agent), "MCP 已关闭。")

    def test_agent_exposes_mcp_tool_definition(self) -> None:
        config = MCPConfig(enabled=True)
        manager = MCPClientManager(config)
        manager.registry.add_tool(
            MCPToolMeta(
                logical_name="trusted.echo",
                server_name="trusted",
                tool_name="echo",
                description="Echo text",
                input_schema={"type": "object"},
                requires_confirmation=False,
                risk_level="trusted",
            )
        )
        manager.registry.add_resource(
            MCPResourceMeta(
                logical_uri="trusted:project://README.md",
                server_name="trusted",
                uri="project://README.md",
                name="README.md",
            )
        )
        manager.registry.add_prompt(
            MCPPromptMeta(
                logical_name="trusted.code_review",
                server_name="trusted",
                prompt_name="code_review",
                description="Review",
            )
        )
        agent = object.__new__(LocalToolAgent)
        agent._mcp_manager = manager

        tools = {tool.name: tool for tool in LocalToolAgent._build_mcp_tools(agent)}

        self.assertIn("trusted.echo", tools)
        self.assertIn("mcp_read_resource__trusted:project://README.md", tools)
        self.assertIn("mcp_get_prompt__trusted.code_review", tools)
        self.assertFalse(tools["trusted.echo"].requires_confirmation)

    def test_mcp_resource_tool_name_falls_back_to_workspace_read_file(self) -> None:
        tools = {
            "local_project.workspace.read_file": ToolDefinition(
                name="local_project.workspace.read_file",
                description="读取文件。",
                argument_schema=(
                    '{"type":"object","properties":{"path":{"type":"string"},'
                    '"start_line":{"type":"integer"},"max_lines":{"type":"integer"}}}'
                ),
                requires_confirmation=False,
                run=lambda _arguments: None,  # type: ignore[arg-type,return-value]
            )
        }

        call = normalize_tool_call(
            ToolCall(
                name="mcp_read_resource__local_project:project://docs/SKILL_INSTALLATION.md",
                arguments={},
            ),
            tools,
        )

        self.assertEqual(call.name, "local_project.workspace.read_file")
        self.assertEqual(call.arguments["path"], "docs/SKILL_INSTALLATION.md")

    def test_agent_prioritizes_mcp_tools_in_prompt_order(self) -> None:
        manager = MCPClientManager(MCPConfig(enabled=True))
        manager.registry.add_tool(
            MCPToolMeta(
                logical_name="trusted.workspace.read_file",
                server_name="trusted",
                tool_name="workspace.read_file",
                description="Read file through MCP",
                input_schema={"type": "object"},
                requires_confirmation=False,
                risk_level="trusted",
            )
        )
        agent = object.__new__(LocalToolAgent)
        agent._mcp_manager = manager
        agent._memory_store = None

        tools = LocalToolAgent._build_tools(agent)
        tool_names = list(tools)

        self.assertLess(
            tool_names.index("trusted.workspace.read_file"),
            tool_names.index("read_file"),
        )

    def test_system_prompt_mentions_mcp_progressive_docs(self) -> None:
        manager = MCPClientManager(MCPConfig(enabled=False))
        agent = object.__new__(LocalToolAgent)
        agent.workspace_root = Path.cwd()
        agent.config = SimpleNamespace(workspace_detection_summary="")
        agent._tools = {}
        agent._memory_store = None
        agent._skill_manager = None
        agent._active_skills = []
        agent._temp_workspace = SimpleNamespace(display_path=".agent_tmp")
        agent._mcp_manager = manager
        agent._system_prompt_template = LocalToolAgent._load_system_prompt_template(agent)

        runtime_context = "\n".join(
            [
                "运行环境：",
                "- 操作系统：Windows 11 (AMD64)",
                "- Python：3.12.0",
                f"- 工作区根目录：{agent.workspace_root}",
                "- Agent 运行窗口：Shell=CMD",
                "- run_command 默认 Shell：cmd.exe（默认按 CMD 语法解析）",
                "- 终端环境变量：WT_SESSION",
            ]
        )
        with patch("ai_voice_agent.agent_prompt_context.runtime_environment_context", return_value=runtime_context):
            prompt = LocalToolAgent._system_prompt(agent)
            context_messages = LocalToolAgent._context_messages(agent)

        self.assertNotIn("运行环境：", prompt)
        self.assertIn("docs/MCP_USAGE.md", prompt)
        self.assertIn("docs/MCP_DESIGN_TECHNICAL.md", prompt)
        self.assertIn("优先调用 MCP 能力", prompt)
        self.assertIn("AGENTS.md", prompt)
        self.assertIn("docs/SKILL_INSTALLATION.md", prompt)
        self.assertIn("Skill 多协作原则", prompt)
        self.assertIn("主 Skill 和辅助 Skill", prompt)
        self.assertIn("天气、新闻、价格", prompt)
        self.assertNotIn("run_command 默认 Shell：cmd.exe", prompt)
        self.assertNotIn("risk_level=trusted", prompt)
        runtime_message = context_messages[-1]["content"]
        self.assertIn("运行环境：", runtime_message)
        self.assertIn("操作系统", runtime_message)
        self.assertIn("Python", runtime_message)
        self.assertIn("工作区根目录", runtime_message)
        self.assertIn("Agent 运行窗口：Shell=CMD", runtime_message)
        self.assertIn("run_command 默认 Shell：cmd.exe", runtime_message)
        self.assertIn("终端环境变量：WT_SESSION", runtime_message)


class LocalMCPServerTest(unittest.TestCase):
    def test_local_server_blocks_protected_paths_and_supports_tools(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            workspace = Path(temp_dir)
            (workspace / "README.md").write_text("alpha\nbeta\n", encoding="utf-8")
            (workspace / "config.json").write_text("secret", encoding="utf-8")
            server = LocalMCPServer(workspace)

            read_response = server.handle_message(
                {
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "tools/call",
                    "params": {
                        "name": "workspace.read_file",
                        "arguments": {"path": "README.md", "max_lines": 1},
                    },
                }
            )
            blocked_response = server.handle_message(
                {
                    "jsonrpc": "2.0",
                    "id": 2,
                    "method": "tools/call",
                    "params": {
                        "name": "workspace.read_file",
                        "arguments": {"path": "config.json"},
                    },
                }
            )

        read_result = read_response["result"]
        blocked_result = blocked_response["result"]
        self.assertFalse(read_result["isError"])
        self.assertIn("alpha", read_result["content"][0]["text"])
        self.assertTrue(blocked_result["isError"])
        self.assertIn("受保护路径", blocked_result["content"][0]["text"])

    def test_local_server_resources_and_prompts(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            workspace = Path(temp_dir)
            (workspace / "README.md").write_text("resource text", encoding="utf-8")
            docs_dir = workspace / "docs"
            docs_dir.mkdir()
            (docs_dir / "SKILL_INSTALLATION.md").write_text("skill docs", encoding="utf-8")
            server = LocalMCPServer(workspace)

            resources = server.handle_message({"jsonrpc": "2.0", "id": 1, "method": "resources/list"})
            resource = server.handle_message(
                {
                    "jsonrpc": "2.0",
                    "id": 2,
                    "method": "resources/read",
                    "params": {"uri": "project://README.md"},
                }
            )
            prompt = server.handle_message(
                {
                    "jsonrpc": "2.0",
                    "id": 3,
                    "method": "prompts/get",
                    "params": {
                        "name": "safe_change_plan",
                        "arguments": {"goal": "升级 MCP", "constraints": "可回滚"},
                    },
                }
            )

        self.assertTrue(any(item["uri"] == "project://README.md" for item in resources["result"]["resources"]))
        self.assertTrue(
            any(
                item["uri"] == "project://docs/SKILL_INSTALLATION.md"
                for item in resources["result"]["resources"]
            )
        )
        self.assertIn("resource text", resource["result"]["contents"][0]["text"])
        self.assertIn("高风险改动", prompt["result"]["messages"][0]["content"]["text"])

    def test_local_server_command_nonzero_exit_is_error(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            server = LocalMCPServer(Path(temp_dir))

            response = server.handle_message(
                {
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "tools/call",
                    "params": {
                        "name": "workspace.run_command",
                        "arguments": {
                            "command": f'"{sys.executable}" -c "import sys; sys.exit(7)"'
                        },
                    },
                }
            )

        result = response["result"]
        self.assertTrue(result["isError"])
        self.assertIn("退出码：7", result["content"][0]["text"])


if __name__ == "__main__":
    unittest.main()
