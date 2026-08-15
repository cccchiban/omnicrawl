from __future__ import annotations

import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch

from omnicrawl.agent.core import LocalToolAgent
from omnicrawl.agent.execution import AgentLoopObservation, AgentLoopRunner
from omnicrawl.agent.types import (
    AgentModelReply,
    ToolCall,
    ToolDefinition,
    ToolImageAttachment,
    ToolResult,
)
from omnicrawl.agent.vision_proxy import VisionAnalysis, VisionProxyError
from omnicrawl.config.llm import ActiveModelRef
from omnicrawl.config.vision import VisionConfiguration
from omnicrawl.llm.protocol import (
    ConversationMessage,
    ImageBlock,
    TextBlock,
    ToolCallBlock,
    ToolResultBlock,
    conversation_from_openai_messages,
)
from omnicrawl.llm.providers.anthropic import _to_anthropic_messages
from omnicrawl.llm.providers.gemini import _to_gemini_contents
from omnicrawl.llm.providers.openai_chat import _to_openai_messages
from omnicrawl.llm.providers.openai_responses import _messages_to_responses_input


_IMAGE_BASE64 = "iVBORw0KGgpmaXh0dXJl"


class VisionToolImageProtocolTest(unittest.TestCase):
    def test_openai_style_multimodal_message_becomes_image_block(self) -> None:
        messages = conversation_from_openai_messages(
            [
                {
                    "role": "user",
                    "content": [
                        {"type": "text", "text": "观察截图"},
                        {
                            "type": "image_url",
                            "image_url": {
                                "url": f"data:image/png;base64,{_IMAGE_BASE64}",
                                "detail": "high",
                            },
                        },
                    ],
                }
            ]
        )

        self.assertEqual(len(messages), 1)
        self.assertIsInstance(messages[0].blocks[0], TextBlock)
        image = messages[0].blocks[1]
        self.assertIsInstance(image, ImageBlock)
        self.assertEqual(image.media_type, "image/png")
        self.assertEqual(image.data_base64, _IMAGE_BASE64)
        self.assertEqual(image.detail, "high")

    def test_all_provider_mappers_emit_native_image_parts(self) -> None:
        messages = (
            ConversationMessage(
                role="user",
                blocks=(
                    TextBlock("观察截图"),
                    ImageBlock("image/png", _IMAGE_BASE64, "auto"),
                ),
            ),
        )

        openai_chat = _to_openai_messages("system", messages)
        self.assertEqual(openai_chat[1]["content"][1]["type"], "image_url")
        self.assertTrue(openai_chat[1]["content"][1]["image_url"]["url"].startswith("data:image/png"))

        responses = _messages_to_responses_input(messages)
        self.assertEqual(responses[0]["content"][1]["type"], "input_image")
        self.assertTrue(responses[0]["content"][1]["image_url"].startswith("data:image/png"))
        self.assertEqual(responses[0]["content"][1]["detail"], "auto")

        anthropic = _to_anthropic_messages(messages)
        self.assertEqual(anthropic[0]["content"][1]["type"], "image")
        self.assertEqual(anthropic[0]["content"][1]["source"]["data"], _IMAGE_BASE64)

        gemini = _to_gemini_contents(messages)
        self.assertEqual(
            gemini[0]["parts"][1]["inline_data"]["mime_type"],
            "image/png",
        )
        self.assertEqual(gemini[0]["parts"][1]["inline_data"]["data"], _IMAGE_BASE64)

    def test_responses_input_emits_reasoning_item_only_for_tool_call_history(self) -> None:
        # 思考模式 + 工具调用历史：上游（Console Go）要求回传 reasoning，
        # 但 Responses API 不接受 chat 专用字段 reasoning_content，必须用标准
        # reasoning item；无工具调用时不能携带（上游 400 invalid message）。
        with_tools = _messages_to_responses_input(
            (
                ConversationMessage(role="user", blocks=(TextBlock("读取文件"),)),
                ConversationMessage(
                    role="assistant",
                    blocks=(
                        TextBlock("我先查看。"),
                        ToolCallBlock("call_1", "read", {"path": "README.md"}),
                    ),
                    reasoning="先读取文件。",
                ),
                ConversationMessage(
                    role="tool",
                    blocks=(ToolResultBlock("call_1", True, "ok"),),
                ),
            )
        )
        types = [item.get("type") or item.get("role") for item in with_tools]
        self.assertEqual(
            types,
            ["user", "assistant", "reasoning", "function_call", "function_call_output"],
        )
        reasoning_item = with_tools[2]
        self.assertEqual(reasoning_item["summary"][0]["type"], "summary_text")
        self.assertEqual(reasoning_item["summary"][0]["text"], "先读取文件。")
        for item in with_tools:
            self.assertNotIn("reasoning_content", item, "Responses 输入禁止携带 reasoning_content")

        # 无工具调用：不带 reasoning item
        without_tools = _messages_to_responses_input(
            (
                ConversationMessage(role="user", blocks=(TextBlock("hi"),)),
                ConversationMessage(role="assistant", blocks=(TextBlock("hello"),), reasoning="think"),
            )
        )
        flat = [item.get("type") or item.get("role") for item in without_tools]
        self.assertNotIn("reasoning", flat)

        # reasoning 为空但有工具调用：占位文本兜底
        placeholder = _messages_to_responses_input(
            (
                ConversationMessage(role="user", blocks=(TextBlock("hi"),)),
                ConversationMessage(
                    role="assistant",
                    blocks=(ToolCallBlock("call_2", "list", {"path": "."}),),
                    reasoning="",
                ),
            )
        )
        item = [i for i in placeholder if i.get("type") == "reasoning"][0]
        self.assertTrue(item["summary"][0]["text"].strip())

    def test_anthropic_and_gemini_merge_tool_result_with_image_observation(self) -> None:
        messages = (
            ConversationMessage(
                role="assistant",
                blocks=(ToolCallBlock("call_1", "windows_screenshot", {}),),
            ),
            ConversationMessage(
                role="tool",
                blocks=(ToolResultBlock("call_1", True, "saved"),),
            ),
            ConversationMessage(
                role="user",
                blocks=(
                    TextBlock("观察截图"),
                    ImageBlock("image/png", _IMAGE_BASE64),
                ),
            ),
        )

        anthropic = _to_anthropic_messages(messages)
        self.assertEqual([item["role"] for item in anthropic], ["assistant", "user"])
        self.assertEqual(
            [part["type"] for part in anthropic[1]["content"]],
            ["tool_result", "text", "image"],
        )

        gemini = _to_gemini_contents(messages)
        self.assertEqual([item["role"] for item in gemini], ["model", "user"])
        self.assertIn("function_response", gemini[1]["parts"][0])
        self.assertIn("inline_data", gemini[1]["parts"][2])

    def test_host_injects_image_only_when_active_model_supports_vision(self) -> None:
        agent = LocalToolAgent.__new__(LocalToolAgent)
        tool_call = ToolCall("windows_screenshot", {"target": "desktop"}, "call_1")
        result = ToolResult(
            ok=True,
            output='{"path":".omnicrawl/.agent_tmp/images/screenshot.png"}',
            model_images=(
                ToolImageAttachment("image/png", _IMAGE_BASE64, "screenshot.png"),
            ),
        )

        agent._active_runtime_snapshot = SimpleNamespace(
            runtime=SimpleNamespace(capabilities=SimpleNamespace(vision=False))
        )
        self.assertEqual(agent._tool_result_followup_messages(tool_call, result), ())

        agent._active_runtime_snapshot = SimpleNamespace(
            runtime=SimpleNamespace(capabilities=SimpleNamespace(vision=True))
        )
        followups = agent._tool_result_followup_messages(tool_call, result)
        self.assertEqual(len(followups), 1)
        self.assertEqual(followups[0]["role"], "user")
        image_url = followups[0]["content"][1]["image_url"]["url"]
        self.assertEqual(image_url, f"data:image/png;base64,{_IMAGE_BASE64}")

    def test_non_visual_main_model_routes_image_to_text_vision_proxy(self) -> None:
        agent = object.__new__(LocalToolAgent)
        agent.config = SimpleNamespace(
            llm=SimpleNamespace(),
            max_tool_output_chars=6000,
            vision=VisionConfiguration(
                enabled=True,
                models=(ActiveModelRef(source="custom", key="vision-model"),),
            ),
        )
        agent.workspace_root = Path.cwd()
        agent._active_runtime_snapshot = SimpleNamespace(
            runtime=SimpleNamespace(capabilities=SimpleNamespace(vision=False))
        )
        result = ToolResult(
            ok=True,
            output='{"path":"screen.png","media_type":"image/png"}',
            full_output="读取完成",
            model_images=(ToolImageAttachment("image/png", _IMAGE_BASE64),),
        )
        proxy = SimpleNamespace(
            analyze=lambda images, **kwargs: VisionAnalysis(
                text="画面中显示一个设置窗口。",
                model="vision-model",
            )
        )
        with patch("omnicrawl.agent.core.VisionModelProxy", return_value=proxy):
            prepared, followups = agent._prepare_tool_result_for_model(
                ToolCall("read_image", {"path": "screen.png"}, "call_1"),
                result,
            )

        self.assertTrue(prepared.ok)
        self.assertEqual(prepared.model_images, ())
        self.assertIn("读取完成", prepared.full_output)
        self.assertIn("画面中显示一个设置窗口。", prepared.full_output)
        self.assertEqual(len(followups), 1)
        self.assertEqual(followups[0]["role"], "user")
        self.assertIn("vision_observation", followups[0]["content"])
        self.assertIn("画面中显示一个设置窗口。", followups[0]["content"])

    def test_visual_proxy_failure_becomes_tool_error_without_image_leak(self) -> None:
        agent = object.__new__(LocalToolAgent)
        agent.config = SimpleNamespace(
            llm=SimpleNamespace(),
            max_tool_output_chars=6000,
            vision=VisionConfiguration(
                enabled=True,
                models=(ActiveModelRef(source="custom", key="vision-model"),),
            ),
        )
        agent.workspace_root = Path.cwd()
        agent._active_runtime_snapshot = SimpleNamespace(
            runtime=SimpleNamespace(capabilities=SimpleNamespace(vision=False))
        )
        result = ToolResult(
            ok=True,
            output='{"path":"screen.png"}',
            model_images=(ToolImageAttachment("image/png", _IMAGE_BASE64),),
        )
        proxy = SimpleNamespace(
            analyze=lambda _images, **_kwargs: (_ for _ in ()).throw(
                VisionProxyError("两个视觉模型都不可用")
            )
        )
        with patch("omnicrawl.agent.core.VisionModelProxy", return_value=proxy):
            prepared, followups = agent._prepare_tool_result_for_model(
                ToolCall("windows_screenshot", {"target": "desktop"}, "call_1"),
                result,
            )

        self.assertFalse(prepared.ok)
        self.assertIn("两个视觉模型都不可用", prepared.output)
        self.assertEqual(prepared.model_images, ())
        self.assertEqual(followups, ())
        self.assertNotIn(_IMAGE_BASE64, prepared.output)

    def test_session_projection_still_omits_image_data_after_proxy(self) -> None:
        agent = LocalToolAgent.__new__(LocalToolAgent)
        result = ToolResult(
            True,
            '{"path":".omnicrawl/.agent_tmp/images/screenshot.png"}',
            model_images=(ToolImageAttachment("image/png", _IMAGE_BASE64),),
        )
        tool = ToolDefinition(
            "windows_screenshot",
            "",
            "{}",
            True,
            lambda _arguments: result,
        )
        agent._tools = {tool.name: tool}
        agent._active_runtime_snapshot = SimpleNamespace(
            runtime=SimpleNamespace(capabilities=SimpleNamespace(vision=True))
        )
        events: list[tuple[str, dict]] = []
        agent._append_session_event = lambda event_type, payload: events.append((event_type, payload))
        agent._approve_tool_for_batch = lambda _tool, _arguments: None
        agent._execute_approved_tool = lambda _tool, _arguments: result

        observations = agent._execute_tool_batch(
            [ToolCall("windows_screenshot", {"target": "desktop"}, "call_1")],
            1,
            report_tool_start=lambda _step, _call: None,
            report_tool_result=lambda _call, _result: None,
            check_cancelled=lambda: None,
            status=lambda _text: None,
        )

        tool_result_payload = next(payload for name, payload in events if name == "tool_result")
        self.assertNotIn(_IMAGE_BASE64, str(tool_result_payload))
        self.assertNotIn("model_images", tool_result_payload)
        self.assertIn(_IMAGE_BASE64, str(observations[0].followup_messages))

    def test_runner_appends_all_tool_results_before_visual_followups(self) -> None:
        first = ToolCall("windows_screenshot", {}, "call_1")
        second = ToolCall("read", {}, "call_2")
        replies = iter(
            [
                AgentModelReply(
                    message={"role": "assistant", "content": None, "tool_calls": []},
                    content="",
                    tool_calls=[first, second],
                ),
                AgentModelReply(
                    message={"role": "assistant", "content": "完成"},
                    content="完成",
                ),
            ]
        )

        def execute_batch(calls, _first_step):
            return [
                AgentLoopObservation(
                    tool_call=calls[0],
                    result=ToolResult(True, "screenshot"),
                    message={"role": "tool", "tool_call_id": "call_1", "content": "screenshot"},
                    followup_messages=(
                        {"role": "user", "content": [{"type": "text", "text": "image"}]},
                    ),
                ),
                AgentLoopObservation(
                    tool_call=calls[1],
                    result=ToolResult(True, "file"),
                    message={"role": "tool", "tool_call_id": "call_2", "content": "file"},
                ),
            ]

        result = AgentLoopRunner().run(
            messages=[{"role": "user", "content": "检查"}],
            request_reply=lambda _messages: next(replies),
            execute_tool_batch=execute_batch,
        )
        roles = [message["role"] for message in result.messages]
        self.assertEqual(roles, ["user", "assistant", "tool", "tool", "user"])


if __name__ == "__main__":
    unittest.main()
