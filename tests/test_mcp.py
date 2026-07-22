from __future__ import annotations

import json
import tempfile
import unittest
from pathlib import Path

import yaml
from types import SimpleNamespace
from unittest.mock import patch

from omnicrawl.agent import LocalToolAgent
from omnicrawl.mcp.client import MCPClientManager, _resolve_stdio_command
from omnicrawl.mcp.config import MCPConfig, MCPConfigError, MCPServerConfig, load_mcp_config
from omnicrawl.mcp.registry import MCPPromptMeta, MCPResourceMeta, MCPToolMeta, namespace_capability_name
from omnicrawl.mcp.server import LocalMCPServer
from omnicrawl.mcp.security import mcp_tool_requires_confirmation
from omnicrawl.slash_commands import build_slash_commands, format_mcp_status


class MCPConfigTest(unittest.TestCase):
    def test_load_mcp_config_defaults_to_disabled(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            config_path = Path(temp_dir) / "config.yaml"

            config = load_mcp_config(config_path)

        self.assertFalse(config.enabled)
        self.assertEqual(config.default_timeout_seconds, 30)
        self.assertEqual(config.servers, {})

    def test_load_mcp_config_validates_stdio_server(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            config_path = Path(temp_dir) / "config.yaml"
            config_path.write_text(
                yaml.safe_dump(
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
                    },
                    allow_unicode=True,
                    sort_keys=False,
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
            config_path = Path(temp_dir) / "config.yaml"
            config_path.write_text(
                yaml.safe_dump(
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
                    },
                    allow_unicode=True,
                    sort_keys=False,
                ),
                encoding="utf-8",
            )

            with self.assertRaisesRegex(MCPConfigError, "名称只能包含"):
                load_mcp_config(config_path)

            config_path.write_text(
                yaml.safe_dump(
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
                    },
                    allow_unicode=True,
                    sort_keys=False,
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

    def test_trusted_unknown_tool_requires_confirmation(self) -> None:
        config = MCPConfig(enabled=True)
        meta = MCPToolMeta(
            logical_name="trusted.send_email",
            server_name="trusted",
            tool_name="send_email",
            description="send email",
            risk_level="trusted",
        )

        self.assertTrue(mcp_tool_requires_confirmation(meta, config.policy))

    def test_trusted_tool_requires_confirmation(self) -> None:
        config = MCPConfig(enabled=True)
        meta = MCPToolMeta(
            logical_name="trusted.read_file",
            server_name="trusted",
            tool_name="read_file",
            description="read",
            risk_level="trusted",
        )

        self.assertTrue(mcp_tool_requires_confirmation(meta, config.policy))

    def test_trusted_tool_with_read_substring_and_side_effect_requires_confirmation(self) -> None:
        config = MCPConfig(enabled=True)
        meta = MCPToolMeta(
            logical_name="trusted.read_file_and_send_email",
            server_name="trusted",
            tool_name="read_file_and_send_email",
            description="read then send",
            risk_level="trusted",
        )

        self.assertTrue(mcp_tool_requires_confirmation(meta, config.policy))


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
            "omnicrawl.mcp.client.shutil.which",
            return_value=r"C:\Program Files\nodejs\npx.CMD",
        ) as which:
            resolved = _resolve_stdio_command("npx")

        which.assert_called_once_with("npx")
        self.assertEqual(resolved, r"C:\Program Files\nodejs\npx.CMD")

    def test_stdio_local_server_discovers_resources_and_prompts_without_tools(self) -> None:
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
                        args=["-m", "omnicrawl.mcp.server"],
                        timeout_seconds=5,
                        risk_level="trusted",
                    )
                },
            )
            manager = MCPClientManager(config, workspace_root=workspace, approval_mode_getter=lambda: "manual")

            manager.discover()
            resource_result = manager.read_resource("local_project:project://README.md")
            prompt_result = manager.get_prompt(
                "local_project.code_review",
                {"path": "README.md", "focus": "bug"},
            )
            manager.close()

        self.assertEqual(manager.registry.tools, {})
        self.assertTrue(resource_result.ok)
        self.assertIn("hello mcp", resource_result.output)
        self.assertTrue(prompt_result.ok)
        self.assertIn("代码审查", prompt_result.output)

    def test_audit_output_preview_redacts_sensitive_text(self) -> None:
        from omnicrawl.mcp.audit import MCPAuditLogger

        with tempfile.TemporaryDirectory() as temp_dir:
            workspace = Path(temp_dir)
            logger = MCPAuditLogger(workspace, enabled=True)
            logger.record_tool_call(
                session_id="session",
                audit_id="audit",
                server_name="demo",
                tool_name="echo",
                arguments={},
                approval_mode="manual",
                approval_result="approved",
                duration_ms=1,
                ok=True,
                error_code=None,
                output="api_key=output-secret; Authorization: Bearer abcdefghijklmnop",
            )
            event = json.loads((workspace / "logs" / "mcp-audit.jsonl").read_text(encoding="utf-8"))

        self.assertNotIn("output-secret", event["output_preview"])
        self.assertNotIn("abcdefghijklmnop", event["output_preview"])
        self.assertIn("api_key=***", event["output_preview"])
        self.assertIn("Bearer ***", event["output_preview"])

    def test_audit_redacts_x_api_key_header(self) -> None:
        from omnicrawl.mcp.audit import MCPAuditLogger

        with tempfile.TemporaryDirectory() as temp_dir:
            workspace = Path(temp_dir)
            logger = MCPAuditLogger(workspace, enabled=True)
            logger.record_tool_call(
                session_id="session",
                audit_id="audit",
                server_name="demo",
                tool_name="echo",
                arguments={"headers": {"X-API-Key": "header-secret"}},
                approval_mode="manual",
                approval_result="approved",
                duration_ms=1,
                ok=True,
                error_code=None,
                output="ok",
            )
            event = json.loads((workspace / "logs" / "mcp-audit.jsonl").read_text(encoding="utf-8"))

        self.assertNotIn("header-secret", event["arguments_redacted"]["headers"]["X-API-Key"])
        self.assertEqual(event["arguments_redacted"]["headers"]["X-API-Key"], "***")

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

    def test_agent_builds_resource_and_prompt_tools_without_workspace_fallback(self) -> None:
        manager = MCPClientManager(MCPConfig(enabled=True))
        manager.registry.add_resource(
            MCPResourceMeta(
                logical_uri="local_project:project://README.md",
                server_name="local_project",
                uri="project://README.md",
                name="README.md",
            )
        )
        manager.registry.add_prompt(
            MCPPromptMeta(
                logical_name="local_project.code_review",
                server_name="local_project",
                prompt_name="code_review",
                description="Review",
            )
        )
        agent = object.__new__(LocalToolAgent)
        agent._mcp_manager = manager

        tools = {tool.name: tool for tool in LocalToolAgent._build_mcp_tools(agent)}

        self.assertIn("mcp_read_resource__local_project:project://README.md", tools)
        self.assertIn("mcp_get_prompt__local_project.code_review", tools)
        self.assertNotIn("workspace.read_file", tools)

    def test_agent_prioritizes_mcp_tools_in_prompt_order(self) -> None:
        manager = MCPClientManager(MCPConfig(enabled=True))
        manager.registry.add_tool(
            MCPToolMeta(
                logical_name="trusted.read_file",
                server_name="trusted",
                tool_name="read_file",
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
            tool_names.index("trusted.read_file"),
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
                "- 终端环境变量：WT_SESSION",
            ]
        )
        with patch("omnicrawl.agent.prompt_context.runtime_environment_context", return_value=runtime_context):
            prompt = LocalToolAgent._system_prompt(agent)
            context_messages = LocalToolAgent._context_messages(agent)

        self.assertNotIn("运行环境：", prompt)
        self.assertIn("docs/MCP_USAGE.md", prompt)
        self.assertIn("omnicrawl/mcp/", prompt)
        self.assertIn("优先调用 MCP 能力", prompt)
        self.assertIn("AGENTS.md", prompt)
        self.assertIn("docs/SKILL_INSTALLATION.md", prompt)
        self.assertIn("Skill 多协作原则", prompt)
        self.assertIn("主 Skill 和辅助 Skill", prompt)
        self.assertIn("天气、新闻、价格", prompt)
        self.assertNotIn("risk_level=trusted", prompt)
        runtime_message = context_messages[-1]["content"]
        self.assertIn("运行环境：", runtime_message)
        self.assertIn("操作系统", runtime_message)
        self.assertIn("Python", runtime_message)
        self.assertIn("工作区根目录", runtime_message)
        self.assertIn("Agent 运行窗口：Shell=CMD", runtime_message)
        self.assertIn("终端环境变量：WT_SESSION", runtime_message)


class LocalMCPServerTest(unittest.TestCase):
    def test_local_server_exposes_no_workspace_tools(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            server = LocalMCPServer(Path(temp_dir))
            tools = server.handle_message({"jsonrpc": "2.0", "id": 1, "method": "tools/list"})
            unknown_tool = server.handle_message(
                {
                    "jsonrpc": "2.0",
                    "id": 2,
                    "method": "tools/call",
                    "params": {
                        "name": "workspace.read_file",
                        "arguments": {"path": "README.md"},
                    },
                }
            )

        self.assertEqual(tools["result"]["tools"], [])
        self.assertIn("error", unknown_tool)
        self.assertIn("未知工具", unknown_tool["error"]["message"])

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


if __name__ == "__main__":
    unittest.main()
