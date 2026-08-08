from __future__ import annotations

import json
import unittest
from types import SimpleNamespace

from omnicrawl.agent.core import LocalToolAgent
from omnicrawl.agent.host_tools import (
    HostToolCatalog,
    INVOKE_TOOL_NAME,
    SEARCH_TOOLS_NAME,
    build_provider_tools,
)
from omnicrawl.agent.types import ToolCall, ToolDefinition, ToolResult
from omnicrawl.agent.tools import public_tool_arguments


class HostToolCatalogTest(unittest.TestCase):
    def _tool(self, name: str = "learning_create_cards") -> ToolDefinition:
        return ToolDefinition(
            name=name,
            description="从学习笔记生成闪卡。" * 20,
            argument_schema=json.dumps(
                {
                    "type": "object",
                    "properties": {
                        "notes": {"type": "string", "minLength": 1, "description": "笔记正文"},
                        "mode": {"type": "string", "enum": ["qa", "cloze"]},
                    },
                    "required": ["notes", "mode"],
                    "additionalProperties": False,
                },
                ensure_ascii=False,
            ),
            requires_confirmation=True,
            run=lambda _arguments: ToolResult(ok=True, output="created"),
        )

    def test_provider_surface_contains_only_fixed_tools(self) -> None:
        catalog = HostToolCatalog({"read_file": self._tool("read_file")})
        provider_tools = build_provider_tools(catalog)

        self.assertEqual(set(provider_tools), {SEARCH_TOOLS_NAME, INVOKE_TOOL_NAME})
        self.assertNotIn("read_file", provider_tools)
        self.assertFalse(provider_tools[SEARCH_TOOLS_NAME].requires_confirmation)
        self.assertFalse(provider_tools[INVOKE_TOOL_NAME].requires_confirmation)

    def test_search_returns_compact_contract_for_matching_tool(self) -> None:
        catalog = HostToolCatalog({"learning_create_cards": self._tool()})

        result = catalog.search({"query": "create flashcards", "limit": 4})
        payload = json.loads(result.output)

        self.assertTrue(result.ok)
        self.assertEqual(payload["count"], 1)
        entry = payload["tools"][0]
        self.assertEqual(entry["name"], "learning_create_cards")
        self.assertEqual(entry["parameters"]["required"], ["notes", "mode"])
        self.assertNotIn("description", json.dumps(entry["parameters"], ensure_ascii=False))

    def test_prepare_invocation_normalizes_and_validates_arguments(self) -> None:
        catalog = HostToolCatalog({"read_file": ToolDefinition(
            name="read_file",
            description="读取文件",
            argument_schema=(
                '{"type":"object","properties":'
                '{"path":{"type":"string"},"start_line":{"type":"integer"}},'
                '"required":["path"],"additionalProperties":false}'
            ),
            requires_confirmation=True,
            run=lambda _arguments: ToolResult(ok=True, output="ok"),
        )})

        prepared = catalog.prepare_invocation(
            {
                "tool_name": "readfile",
                "arguments": {"path": "README.md", "startline": 2},
            }
        )

        self.assertFalse(isinstance(prepared, ToolResult))
        assert not isinstance(prepared, ToolResult)
        self.assertEqual(prepared.tool_name, "read_file")
        self.assertEqual(prepared.arguments, {"path": "README.md", "start_line": 2})

    def test_prepare_invocation_returns_structured_retryable_error(self) -> None:
        catalog = HostToolCatalog({"learning_create_cards": self._tool()})

        result = catalog.prepare_invocation(
            {
                "tool_name": "learning_create_cards",
                "arguments": {"notes": "only notes", "unexpected": True},
            }
        )
        self.assertIsInstance(result, ToolResult)
        assert isinstance(result, ToolResult)
        payload = json.loads(result.output)
        self.assertFalse(result.ok)
        self.assertEqual(payload["error"]["code"], "invalid_arguments")
        self.assertTrue(payload["error"]["retryable"])
        self.assertIn("contract", payload["error"])
        self.assertTrue(payload["error"]["issues"])


class HostToolDispatchTest(unittest.TestCase):
    def test_invoke_tool_resolves_real_tool_before_approval_and_execution(self) -> None:
        executed: list[dict[str, object]] = []
        approvals: list[str] = []
        agent = object.__new__(LocalToolAgent)
        agent.config = SimpleNamespace(max_tool_output_chars=6000)
        agent._tools = {
            "read_file": ToolDefinition(
                name="read_file",
                description="读取文件",
                argument_schema=(
                    '{"type":"object","properties":{"path":{"type":"string"}},'
                    '"required":["path"]}'
                ),
                requires_confirmation=True,
                run=lambda arguments: (
                    executed.append(dict(arguments))
                    or ToolResult(ok=True, output="file content")
                ),
            )
        }
        agent._append_session_event = lambda _name, _payload: None  # type: ignore[method-assign]
        agent._approve_tool_for_batch = (  # type: ignore[method-assign]
            lambda tool, _arguments, **_kwargs: approvals.append(tool.name) or None
        )
        agent._execute_approved_tool = (  # type: ignore[method-assign]
            lambda tool, arguments: tool.run(arguments)
        )

        observations = agent._execute_tool_batch(
            [
                ToolCall(
                    INVOKE_TOOL_NAME,
                    {"tool_name": "read_file", "arguments": {"path": "README.md"}},
                    "call-1",
                )
            ],
            1,
            report_tool_start=lambda _step, _call: None,
            report_tool_result=lambda _call, _result: None,
            check_cancelled=lambda: None,
            status=lambda _message: None,
        )

        self.assertEqual(approvals, ["read_file"])
        self.assertEqual(executed, [{"path": "README.md"}])
        self.assertEqual(observations[0].tool_call.name, "read_file")
        self.assertEqual(observations[0].message["tool_call_id"], "call-1")
        self.assertIn("file content", observations[0].message["content"])

    def test_should_return_structured_error_when_invoke_arguments_are_invalid(self) -> None:
        events: list[tuple[str, dict[str, object]]] = []
        agent = object.__new__(LocalToolAgent)
        agent.config = SimpleNamespace(max_tool_output_chars=6000)
        agent._tools = {
            "read_file": ToolDefinition(
                name="read_file",
                description="读取文件",
                argument_schema=(
                    '{"type":"object","properties":{"path":{"type":"string"}},'
                    '"required":["path"],"additionalProperties":false}'
                ),
                requires_confirmation=True,
                run=lambda _arguments: ToolResult(ok=True, output="should not run"),
            )
        }
        agent._append_session_event = (  # type: ignore[method-assign]
            lambda name, payload: events.append((name, payload))
        )

        observations = agent._execute_tool_batch(
            [
                ToolCall(
                    INVOKE_TOOL_NAME,
                    {
                        "tool_name": "read_file",
                        "arguments": {"unexpected": True},
                    },
                    "call-invalid",
                )
            ],
            1,
            report_tool_start=lambda _step, _call: None,
            report_tool_result=lambda _call, _result: None,
            check_cancelled=lambda: None,
            status=lambda _message: None,
        )

        self.assertEqual(len(observations), 1)
        self.assertFalse(observations[0].result.ok)
        payload = json.loads(observations[0].result.output)
        self.assertEqual(payload["error"]["code"], "invalid_arguments")
        self.assertEqual(events[0][1]["tool"], INVOKE_TOOL_NAME)
        self.assertEqual(events[0][1]["arguments"]["tool_name"], "read_file")

    def test_plugin_argument_rewrite_is_revalidated_before_approval(self) -> None:
        executed: list[dict[str, object]] = []
        agent = object.__new__(LocalToolAgent)
        agent.config = SimpleNamespace(max_tool_output_chars=6000)
        agent._tools = {
            "read_file": ToolDefinition(
                name="read_file",
                description="读取文件",
                argument_schema=(
                    '{"type":"object","properties":{"path":{"type":"string"}},'
                    '"required":["path"],"additionalProperties":false}'
                ),
                requires_confirmation=False,
                run=lambda arguments: (
                    executed.append(dict(arguments))
                    or ToolResult(ok=True, output="should not run")
                ),
            )
        }
        agent._append_session_event = lambda _name, _payload: None  # type: ignore[method-assign]

        def rewrite_arguments(hook: str, payload: dict[str, object], **_kwargs):
            if hook == "tool.call.before":
                return {**payload, "arguments": {"unexpected": True}}
            return payload

        agent._dispatch_plugin_hook = rewrite_arguments  # type: ignore[method-assign]
        observations = agent._execute_tool_batch(
            [ToolCall("read_file", {"path": "README.md"}, "call-2")],
            1,
            report_tool_start=lambda _step, _call: None,
            report_tool_result=lambda _call, _result: None,
            check_cancelled=lambda: None,
            status=lambda _message: None,
        )

        self.assertEqual(executed, [])
        self.assertFalse(observations[0].result.ok)
        payload = json.loads(observations[0].result.output)
        self.assertEqual(payload["error"]["code"], "invalid_arguments")


if __name__ == "__main__":
    unittest.main()
