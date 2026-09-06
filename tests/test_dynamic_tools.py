from __future__ import annotations

import unittest
from types import SimpleNamespace

from omnicrawl.agent.core import LocalToolAgent
from omnicrawl.agent.types import ToolCall, ToolDefinition, ToolResult
from omnicrawl.llm.protocol import (
    ConversationMessage,
    ToolSpec,
    conversation_from_openai_messages,
    tools_from_conversation_messages,
)
from omnicrawl.llm.providers.openai_chat import _to_openai_messages


class DynamicToolProtocolTest(unittest.TestCase):
    def test_conversation_from_openai_messages_parses_system_tools(self) -> None:
        messages = conversation_from_openai_messages(
            [
                {
                    "role": "system",
                    "tools": [
                        {
                            "type": "function",
                            "function": {
                                "name": "read",
                                "description": "读取文件",
                                "parameters": {"type": "object"},
                            },
                        }
                    ],
                }
            ]
        )

        self.assertEqual(len(messages), 1)
        self.assertEqual(messages[0].role, "system")
        self.assertEqual(len(messages[0].tools), 1)
        self.assertEqual(messages[0].tools[0].name, "read")

    def test_openai_chat_emits_system_tools_without_content(self) -> None:
        message = ConversationMessage(
            role="system",
            tools=(
                ToolSpec(
                    name="read",
                    description="读取文件",
                    parameters={"type": "object"},
                ),
            ),
        )

        result = _to_openai_messages("sys", (message,))

        self.assertEqual(
            result,
            [
                {"role": "system", "content": "sys"},
                {
                    "role": "system",
                    "tools": [
                        {
                            "type": "function",
                            "function": {
                                "name": "read",
                                "description": "读取文件",
                                "parameters": {"type": "object"},
                            },
                        }
                    ],
                },
            ],
        )

    def test_openai_chat_dedupes_dynamic_tools_across_messages(self) -> None:
        first = ConversationMessage(
            role="system",
            tools=(ToolSpec("read", "读取文件", {"type": "object"}),),
        )
        second = ConversationMessage(
            role="system",
            tools=(
                ToolSpec("read", "读取文件", {"type": "object"}),
                ToolSpec("bash", "执行命令", {"type": "object"}),
            ),
        )

        result = _to_openai_messages("", (first, second))
        dynamic = [message for message in result if message.get("tools")]
        self.assertEqual(len(dynamic), 2)
        self.assertEqual(
            [tool["function"]["name"] for tool in dynamic[0]["tools"]],
            ["read"],
        )
        self.assertEqual(
            [tool["function"]["name"] for tool in dynamic[1]["tools"]],
            ["bash"],
        )

    def test_tools_from_conversation_messages_dedupes_by_name(self) -> None:
        first = ConversationMessage(
            role="system",
            tools=(ToolSpec("read", "读取文件", {"type": "object"}),),
        )
        second = ConversationMessage(
            role="system",
            tools=(
                ToolSpec("read", "读取文件", {"type": "object"}),
                ToolSpec("bash", "执行命令", {"type": "object"}),
            ),
        )

        names = [
            tool.name for tool in tools_from_conversation_messages((first, second))
        ]
        self.assertEqual(names, ["read", "bash"])


class DynamicToolAgentTest(unittest.TestCase):
    def _agent(self) -> LocalToolAgent:
        agent = object.__new__(LocalToolAgent)
        agent.config = SimpleNamespace()
        agent._tools = {
            "read": ToolDefinition(
                name="read",
                description="读取文件",
                argument_schema=(
                    '{"type":"object","properties":'
                    '{"path":{"type":"string"}},"required":["path"]}'
                ),
                requires_confirmation=False,
                run=lambda arguments: ToolResult(ok=True, output="content"),
            )
        }
        agent._append_session_event = lambda _name, _payload: None  # type: ignore[method-assign]
        agent._approve_tool_for_batch = (  # type: ignore[method-assign]
            lambda _tool, _arguments, **_kwargs: None
        )
        agent._execute_approved_tool = (  # type: ignore[method-assign]
            lambda tool, arguments: tool.run(arguments)
        )
        agent._tool_result_message = (  # type: ignore[method-assign]
            lambda tool_call, result: {
                "role": "tool",
                "tool_call_id": tool_call.id,
                "content": result.output,
            }
        )
        return agent

    def test_top_level_registered_tool_call_dispatches_directly(self) -> None:
        agent = self._agent()

        observations = agent._execute_tool_batch(
            [ToolCall("read", {"path": "README.md"}, "c2")],
            1,
            report_tool_start=lambda _step, _call: None,
            report_tool_result=lambda _call, _result: None,
            check_cancelled=lambda: None,
            status=lambda _message: None,
        )

        self.assertEqual(observations[0].tool_call.name, "read")
        self.assertEqual(observations[0].result.output, "content")
        # 顶层注册不再产生动态声明 followup 消息。
        self.assertEqual(observations[0].followup_messages, ())

    def test_unknown_tool_returns_corrective_error(self) -> None:
        agent = self._agent()

        observations = agent._execute_tool_batch(
            [ToolCall("missing_tool", {}, "c3")],
            1,
            report_tool_start=lambda _step, _call: None,
            report_tool_result=lambda _call, _result: None,
            check_cancelled=lambda: None,
            status=lambda _message: None,
        )

        self.assertFalse(observations[0].result.ok)
        self.assertIn("未知工具：missing_tool", observations[0].result.output)
        self.assertEqual(observations[0].followup_messages, ())


if __name__ == "__main__":
    unittest.main()
