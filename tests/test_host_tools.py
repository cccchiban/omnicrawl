from __future__ import annotations

import json
import unittest
from types import SimpleNamespace

from omnicrawl.agent.core import LocalToolAgent
from omnicrawl.agent.host_tools import (
    _bounded_toml_result,
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
        catalog = HostToolCatalog({"read": self._tool("read")})
        provider_tools = build_provider_tools(catalog)

        self.assertEqual(set(provider_tools), {SEARCH_TOOLS_NAME, INVOKE_TOOL_NAME})
        self.assertNotIn("read", provider_tools)
        self.assertFalse(provider_tools[SEARCH_TOOLS_NAME].requires_confirmation)
        self.assertFalse(provider_tools[INVOKE_TOOL_NAME].requires_confirmation)

    def test_search_returns_compact_contract_for_matching_tool(self) -> None:
        catalog = HostToolCatalog({"learning_create_cards": self._tool()})

        result = catalog.search({"query": "create flashcards", "limit": 4})
        payload = json.loads(result.output)

        self.assertTrue(result.ok)
        self.assertEqual(len(payload["tools"]), 1)
        entry = payload["tools"][0]
        self.assertEqual(entry["name"], "learning_create_cards")
        self.assertEqual(entry["parameters"]["required"], ["notes", "mode"])
        self.assertNotIn("description", json.dumps(entry["parameters"], ensure_ascii=False))
        self.assertTrue(entry["requires_confirmation"])

    def test_search_omits_redundant_metadata_and_false_flags(self) -> None:
        """搜索结果只保留调用工具所需信息，假值和可推导元数据不占 token。"""

        catalog = HostToolCatalog(
            {"read": self._simple_tool("read", "读取文件内容")}
        )

        result = catalog.search({"query": "read", "limit": 4})
        payload = json.loads(result.output)

        self.assertEqual(set(payload), {"tools"})
        self.assertEqual(
            set(payload["tools"][0]),
            {"name", "description", "parameters"},
        )
        self.assertNotIn("\n", result.output)
        self.assertNotIn(": ", result.output)

    def test_search_emits_true_flags_only_when_actionable(self) -> None:
        """只有确实还有候选或需要确认时，才发送对应布尔标记。"""

        catalog = HostToolCatalog(
            {
                "read_one": self._simple_tool("read_one", "读取文件"),
                "read_two": ToolDefinition(
                    name="read_two",
                    description="读取另一个文件",
                    argument_schema='{"type":"object","properties":{}}',
                    requires_confirmation=True,
                    run=lambda _arguments: ToolResult(ok=True, output="ok"),
                ),
            }
        )

        payload = json.loads(catalog.search({"query": "read", "limit": 1}).output)

        self.assertTrue(payload["truncated"])
        self.assertNotIn("requires_confirmation", payload["tools"][0])

        confirmed = json.loads(
            catalog.search({"query": "read_two", "limit": 1}).output
        )["tools"][0]
        self.assertTrue(confirmed["requires_confirmation"])

    def test_search_normalizes_and_limits_description(self) -> None:
        """候选说明折叠空白且严格限制长度，不让 MCP 长描述淹没参数契约。"""

        description = "  第一行\n\n" + "能力说明 " * 80
        catalog = HostToolCatalog(
            {"dense_tool": self._simple_tool("dense_tool", description)}
        )

        entry = json.loads(
            catalog.search({"query": "dense_tool", "limit": 1}).output
        )["tools"][0]

        self.assertLessEqual(len(entry["description"]), 160)
        self.assertNotIn("\n", entry["description"])
        self.assertNotIn("  ", entry["description"])
        self.assertTrue(entry["description"].endswith("…"))

    def test_search_result_is_substantially_smaller_than_pretty_json(self) -> None:
        """紧凑序列化应显著小于旧版缩进 JSON，防止格式回退增加 token。"""

        catalog = HostToolCatalog(
            {
                f"read_{index}": self._simple_tool(
                    f"read_{index}",
                    f"读取第 {index} 类文件并返回关键内容。",
                    properties={
                        "path": {"type": "string", "minLength": 1},
                        "max_lines": {
                            "type": "integer",
                            "minimum": 1,
                            "maximum": 2000,
                        },
                    },
                )
                for index in range(4)
            }
        )

        result = catalog.search({"query": "read", "limit": 4})
        pretty_size = len(
            json.dumps(json.loads(result.output), ensure_ascii=False, indent=2)
        )

        self.assertLess(len(result.output), pretty_size * 0.7)

    def _simple_tool(
        self, name: str, description: str, properties: dict | None = None
    ) -> ToolDefinition:
        return ToolDefinition(
            name=name,
            description=description,
            argument_schema=json.dumps(
                {
                    "type": "object",
                    "properties": properties or {"path": {"type": "string"}},
                    "required": [],
                    "additionalProperties": False,
                },
                ensure_ascii=False,
            ),
            requires_confirmation=False,
            run=lambda _arguments: ToolResult(ok=True, output="ok"),
        )

    def test_search_matches_parameter_names(self) -> None:
        """参数名参与匹配：查询命中参数名时工具应进入候选。"""

        catalog = HostToolCatalog(
            {
                "run_pipeline": self._simple_tool(
                    "run_pipeline",
                    "执行数据流水线",
                    properties={"pattern": {"type": "string"}},
                )
            }
        )

        result = catalog.search({"query": "pattern", "limit": 4})
        payload = json.loads(result.output)
        self.assertEqual(len(payload["tools"]), 1)
        self.assertEqual(payload["tools"][0]["name"], "run_pipeline")

    def test_search_matches_tool_name_alias(self) -> None:
        """工具别名参与搜索：readimage 应命中 read_image 且排在无关工具之前。"""

        catalog = HostToolCatalog(
            {
                "read_image": self._simple_tool("read_image", "读取图片内容"),
                "list": self._simple_tool("list", "列出目录文件"),
            }
        )

        result = catalog.search({"query": "readimage", "limit": 4})
        payload = json.loads(result.output)
        self.assertGreaterEqual(len(payload["tools"]), 1)
        self.assertEqual(payload["tools"][0]["name"], "read_image")

    def test_search_normalizes_fullwidth_query(self) -> None:
        """全角/半角归一化：全角查询与工具名等价命中。"""

        catalog = HostToolCatalog(
            {"read": self._simple_tool("read", "读取文件内容")}
        )

        result = catalog.search({"query": "ＲＥＡＤ", "limit": 4})
        payload = json.loads(result.output)
        self.assertEqual(len(payload["tools"]), 1)
        self.assertEqual(payload["tools"][0]["name"], "read")

    def test_search_fuzzy_matches_misspelled_tool_name(self) -> None:
        """拼写错误兜底：generate_repor 以模糊相似度命中 generate_report。"""

        catalog = HostToolCatalog(
            {
                "generate_report": self._simple_tool(
                    "generate_report",
                    "生成报表",
                )
            }
        )

        result = catalog.search({"query": "generate_repor", "limit": 4})
        payload = json.loads(result.output)
        self.assertEqual(len(payload["tools"]), 1)
        self.assertEqual(payload["tools"][0]["name"], "generate_report")

    def test_search_chinese_bigram_ignores_stop_words(self) -> None:
        """中文 bigram 分词 + 停止词：整段查询含虚词时仍按相邻两字命中描述。"""

        catalog = HostToolCatalog(
            {
                "read": self._simple_tool("read", "读取内容"),
                "list": self._simple_tool("list", "列出目录"),
            }
        )

        result = catalog.search({"query": "请帮我读取文件", "limit": 4})
        payload = json.loads(result.output)
        self.assertGreaterEqual(len(payload["tools"]), 1)
        self.assertEqual(payload["tools"][0]["name"], "read")

    def test_prepare_invocation_normalizes_and_validates_arguments(self) -> None:
        catalog = HostToolCatalog({"read": ToolDefinition(
            name="read",
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
                "tool_name": "read",
                "arguments": {"path": "README.md", "startline": 2},
            }
        )

        self.assertFalse(isinstance(prepared, ToolResult))
        assert not isinstance(prepared, ToolResult)
        self.assertEqual(prepared.tool_name, "read")
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

    def test_prepare_invocation_drops_blank_optional_string(self) -> None:
        """空串归一化：可选字符串字段传空串或纯空白视为未提供，不再报参数错误。"""

        catalog = HostToolCatalog(
            {
                "read": ToolDefinition(
                    name="read",
                    description="读取文件",
                    argument_schema=(
                        '{"type":"object","properties":'
                        '{"path":{"type":"string"},'
                        '"mode":{"type":"string","minLength":1}},'
                        '"required":["path"],"additionalProperties":false}'
                    ),
                    requires_confirmation=True,
                    run=lambda _arguments: ToolResult(ok=True, output="ok"),
                )
            }
        )

        for blank in ("", "   "):
            prepared = catalog.prepare_invocation(
                {
                    "tool_name": "read",
                    "arguments": {"path": "a.txt", "mode": blank},
                }
            )
            self.assertFalse(isinstance(prepared, ToolResult))
            assert not isinstance(prepared, ToolResult)
            self.assertEqual(prepared.arguments, {"path": "a.txt"})

    def test_prepare_invocation_keeps_blank_required_string_for_error(self) -> None:
        """必填字段的空串保留给 Schema 校验，不会因归一化静默通过。"""

        catalog = HostToolCatalog(
            {
                "read": ToolDefinition(
                    name="read",
                    description="读取文件",
                    argument_schema=(
                        '{"type":"object","properties":'
                        '{"path":{"type":"string","minLength":1}},'
                        '"required":["path"],"additionalProperties":false}'
                    ),
                    requires_confirmation=True,
                    run=lambda _arguments: ToolResult(ok=True, output="ok"),
                )
            }
        )

        result = catalog.prepare_invocation(
            {"tool_name": "read", "arguments": {"path": ""}}
        )
        self.assertIsInstance(result, ToolResult)
        assert isinstance(result, ToolResult)
        payload = json.loads(result.output)
        self.assertFalse(result.ok)
        self.assertEqual(payload["error"]["code"], "invalid_arguments")
        self.assertTrue(payload["error"]["issues"])

    def test_prepare_invocation_accepts_blank_diagnostic_command(self) -> None:
        """放宽约束：bash 的 diagnostic_command 允许空串（minLength 0），校验通过。"""

        catalog = HostToolCatalog(
            {
                "bash": ToolDefinition(
                    name="bash",
                    description="执行命令",
                    argument_schema=(
                        '{"type":"object","properties":'
                        '{"command":{"type":"string","minLength":1},'
                        '"diagnostic_command":{"type":"string","minLength":0}},'
                        '"required":["command"],"additionalProperties":false}'
                    ),
                    requires_confirmation=True,
                    run=lambda _arguments: ToolResult(ok=True, output="ok"),
                )
            }
        )

        for blank in ("", "   "):
            prepared = catalog.prepare_invocation(
                {
                    "tool_name": "bash",
                    "arguments": {
                        "command": "echo ok",
                        "diagnostic_command": blank,
                    },
                }
            )
            self.assertFalse(isinstance(prepared, ToolResult))
            assert not isinstance(prepared, ToolResult)
            self.assertEqual(prepared.arguments["command"], "echo ok")

    def test_prepare_invocation_still_requires_command(self) -> None:
        """回归：放宽只针对可选 diagnostic_command，必填 command 仍然强制。"""

        catalog = HostToolCatalog(
            {
                "bash": ToolDefinition(
                    name="bash",
                    description="执行命令",
                    argument_schema=(
                        '{"type":"object","properties":'
                        '{"command":{"type":"string","minLength":1},'
                        '"diagnostic_command":{"type":"string","minLength":0}},'
                        '"required":["command"],"additionalProperties":false}'
                    ),
                    requires_confirmation=True,
                    run=lambda _arguments: ToolResult(ok=True, output="ok"),
                )
            }
        )

        result = catalog.prepare_invocation(
            {"tool_name": "bash", "arguments": {"diagnostic_command": "tail x"}}
        )
        self.assertIsInstance(result, ToolResult)
        assert isinstance(result, ToolResult)
        payload = json.loads(result.output)
        self.assertEqual(payload["error"]["code"], "invalid_arguments")

    def test_search_contract_adds_min_properties_for_example_style_tools(self) -> None:
        """示例值风格工具契约应带 minProperties=1，标准 Schema 工具保持 required 声明。"""

        example_style = ToolDefinition(
            name="list",
            description="列出工作区内的文件和目录",
            argument_schema='{"path": ".", "recursive": false}',
            requires_confirmation=False,
            run=lambda _arguments: ToolResult(ok=True, output="ok"),
        )
        standard_style = ToolDefinition(
            name="bash",
            description="执行命令",
            argument_schema=(
                '{"type":"object","properties":{"command":{"type":"string"}},'
                '"required":["command"]}'
            ),
            requires_confirmation=False,
            run=lambda _arguments: ToolResult(ok=True, output="ok"),
        )
        catalog = HostToolCatalog({"list": example_style, "bash": standard_style})

        example_contract = json.loads(
            catalog.search({"query": "list", "limit": 1}).output
        )["tools"][0]["parameters"]
        standard_contract = json.loads(
            catalog.search({"query": "bash", "limit": 1}).output
        )["tools"][0]["parameters"]

        self.assertEqual(example_contract["minProperties"], 1)
        self.assertNotIn("required", example_contract)
        self.assertEqual(standard_contract["required"], ["command"])
        self.assertNotIn("minProperties", standard_contract)

    def test_prepare_invocation_rejects_empty_arguments_for_example_style(self) -> None:
        """回归：示例值风格工具的空 arguments 必须被拦截，而不是静默进入执行器。"""

        catalog = HostToolCatalog(
            {
                "list": ToolDefinition(
                    name="list",
                    description="列出工作区内的文件和目录",
                    argument_schema='{"path": ".", "recursive": false}',
                    requires_confirmation=False,
                    run=lambda _arguments: ToolResult(ok=True, output="ok"),
                )
            }
        )

        result = catalog.prepare_invocation(
            {"tool_name": "list", "arguments": {}}
        )
        self.assertIsInstance(result, ToolResult)
        assert isinstance(result, ToolResult)
        payload = json.loads(result.output)
        self.assertFalse(result.ok)
        self.assertEqual(payload["error"]["code"], "invalid_arguments")
        self.assertTrue(payload["error"]["retryable"])
        self.assertTrue(any("属性至少为" in issue["message"] for issue in payload["error"]["issues"]))

    def test_prepare_invocation_accepts_empty_arguments_for_no_argument_tool(self) -> None:
        """无参数工具的空调用仍然合法，不受 minProperties 兜底影响。"""

        catalog = HostToolCatalog(
            {
                "list_worktrees": ToolDefinition(
                    name="list_worktrees",
                    description="列出工作树",
                    argument_schema="{}",
                    requires_confirmation=False,
                    run=lambda _arguments: ToolResult(ok=True, output="ok"),
                )
            }
        )

        prepared = catalog.prepare_invocation(
            {"tool_name": "list_worktrees", "arguments": {}}
        )
        self.assertFalse(isinstance(prepared, ToolResult))
        assert not isinstance(prepared, ToolResult)
        self.assertEqual(prepared.arguments, {})

    def test_search_full_output_uses_readable_toml(self) -> None:
        """展示通道 full_output 为分节 TOML，模型通道 output 保持紧凑 JSON。"""

        catalog = HostToolCatalog({"learning_create_cards": self._tool()})

        result = catalog.search({"query": "create flashcards", "limit": 4})

        self.assertTrue(result.ok)
        # 模型通道：仍是紧凑 JSON，与旧行为一致。
        payload = json.loads(result.output)
        self.assertEqual(payload["tools"][0]["name"], "learning_create_cards")
        # 展示通道：TOML 分节，包含工具数组表与参数表头。
        display = result.full_output
        self.assertTrue(display.startswith("[[tools]]"))
        self.assertIn("[tools.parameters]", display)
        self.assertIn("[tools.parameters.properties.notes]", display)
        self.assertIn("name = \"learning_create_cards\"", display)
        self.assertIn("requires_confirmation = true", display)
        self.assertNotEqual(display, result.output)

    def test_search_full_output_truncation_keeps_toml_valid(self) -> None:
        """展示通道超限时按整段删除 [[tools]]，剩余 TOML 结构完整。"""

        catalog = HostToolCatalog(
            {
                f"read_{index}": self._simple_tool(
                    f"read_{index}",
                    "读取文件并返回内容。",
                    properties={
                        "path": {"type": "string", "minLength": 1},
                        "mode": {"type": "string", "enum": ["a", "b", "c"]},
                    },
                )
                for index in range(6)
            }
        )

        result = catalog.search({"query": "read", "limit": 6})
        display = result.full_output

        # 6 个简单工具远低于展示上限，全量输出且每段结构完整。
        segments = display.split("[[tools]]")[1:]
        self.assertEqual(len(segments), 6)
        for segment in segments:
            self.assertIn("name = ", segment)
            self.assertIn("[tools.parameters]", segment)
        self.assertNotIn("truncated = true", display)

        # 收紧上限验证裁剪：整段删除、剩余 TOML 语法完整、带 truncated 标记。
        payload = {
            "tools": [
                {
                    "name": f"read_{index}",
                    "description": "读取文件并返回内容。",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "path": {"type": "string", "minLength": 1},
                            "mode": {"type": "string", "enum": ["a", "b", "c"]},
                        },
                        "required": ["path"],
                    },
                }
                for index in range(6)
            ]
        }
        tight = _bounded_toml_result(payload, max_chars=200)
        self.assertIn("truncated = true", tight)
        tight_segments = tight.split("[[tools]]")[1:]
        self.assertEqual(len(tight_segments), 1)
        self.assertIn("name = \"read_0\"", tight_segments[0])
        self.assertIn("[tools.parameters]", tight_segments[0])


class HostToolDispatchTest(unittest.TestCase):
    def test_invoke_tool_resolves_real_tool_before_approval_and_execution(self) -> None:
        executed: list[dict[str, object]] = []
        approvals: list[str] = []
        agent = object.__new__(LocalToolAgent)
        agent.config = SimpleNamespace(max_tool_output_chars=6000)
        agent._tools = {
            "read": ToolDefinition(
                name="read",
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
                    {"tool_name": "read", "arguments": {"path": "README.md"}},
                    "call-1",
                )
            ],
            1,
            report_tool_start=lambda _step, _call: None,
            report_tool_result=lambda _call, _result: None,
            check_cancelled=lambda: None,
            status=lambda _message: None,
        )

        self.assertEqual(approvals, ["read"])
        self.assertEqual(executed, [{"path": "README.md"}])
        self.assertEqual(observations[0].tool_call.name, "read")
        self.assertEqual(observations[0].message["tool_call_id"], "call-1")
        self.assertIn("file content", observations[0].message["content"])

    def test_should_return_structured_error_when_invoke_arguments_are_invalid(self) -> None:
        events: list[tuple[str, dict[str, object]]] = []
        agent = object.__new__(LocalToolAgent)
        agent.config = SimpleNamespace(max_tool_output_chars=6000)
        agent._tools = {
            "read": ToolDefinition(
                name="read",
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
                        "tool_name": "read",
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
        self.assertEqual(events[0][1]["arguments"]["tool_name"], "read")

    def test_plugin_argument_rewrite_is_revalidated_before_approval(self) -> None:
        executed: list[dict[str, object]] = []
        agent = object.__new__(LocalToolAgent)
        agent.config = SimpleNamespace(max_tool_output_chars=6000)
        agent._tools = {
            "read": ToolDefinition(
                name="read",
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
            [ToolCall("read", {"path": "README.md"}, "call-2")],
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
