"""模型流截断（finish_reason）与工具执行超时的回归测试。

覆盖两类"会话静默停止"缺陷：
1. Provider 以 length/incomplete/max_tokens/content_filter 截断输出时，
   协议层此前把截断当正常完成：半截回复直接结束回合、无任何提示；
2. 工具执行线程无超时保护：工具挂起时回合无限等待、无提示。
"""

from __future__ import annotations

import threading
import time
import unittest
from pathlib import Path
from types import SimpleNamespace

from omnicrawl.agent import LocalToolAgent
from omnicrawl.agent.toolkit.host_tools import INVOKE_TOOL_NAME
from omnicrawl.agent.runtime.llm_protocol import (
    AgentLLMProtocol,
    RetryableAgentRequestError,
    StreamInterruptedAfterOutputError,
)
from omnicrawl.agent.types import ToolCall, ToolDefinition, ToolResult
from omnicrawl.llm.capabilities import ModelCapabilities
from omnicrawl.llm.errors import ModelError, ModelErrorCode
from omnicrawl.llm.protocol import (
    ModelIdentity,
    ResponseCompleted,
    TextDelta,
    ToolCallArgumentsDelta,
    ToolCallCompleted,
    ToolCallStarted,
)
from omnicrawl.llm.providers.openai_chat import OpenAIChatCompletionsRuntime
from omnicrawl.llm.providers.openai_responses import OpenAIResponsesRuntime
from omnicrawl.llm.registry import ModelDescriptor, ProviderProfile
from omnicrawl.llm.runtime import ModelRuntimeManager


class _TruncationRuntime:
    """模拟 Provider 流在产出部分/全部事件后以截断原因结束。"""

    def __init__(
        self,
        identity,
        *,
        events=(),
        finish_reason: str = "length",
    ) -> None:
        self.identity = identity
        self.capabilities = ModelCapabilities(streaming=True, tools=True)
        self.events = list(events)
        self.finish_reason = finish_reason
        self.turn_count = 0

    def stream_turn(self, request, *, cancel_check=None):
        self.turn_count += 1
        yield from self.events
        yield ResponseCompleted(finish_reason=self.finish_reason)
        return


class _Identity:
    def __init__(self, model_id: str = "demo-model") -> None:
        self.profile_id = "p1"
        self.provider = "openai"
        self.protocol = "openai_chat_completions"
        self.model_id = model_id


class ProtocolTruncationTests(unittest.TestCase):
    """协议层：截断必须进入重试/回滚路径，而不是静默当正常完成。"""

    def _protocol(
        self,
        runtime,
        *,
        request_retry_count: int = 1,
        client=None,
    ) -> AgentLLMProtocol:
        profile = ProviderProfile(
            id="p1",
            provider="openai",
            base_url="https://example.test/v1",
            api_key="test-key",
            default_protocol="openai_chat_completions",
        )
        descriptor = ModelDescriptor(
            identity=runtime.identity if runtime is not None else _Identity(),
            display_name="demo",
            capabilities=ModelCapabilities(streaming=True, tools=True),
            context_window_tokens=128000,
        )
        manager = None
        if runtime is not None:
            manager = ModelRuntimeManager()
            manager.bootstrap(profile, descriptor, runtime=runtime)
            self.addCleanup(manager.close)
        return AgentLLMProtocol(
            client=client,
            model=descriptor.identity.model_id,
            request_timeout_seconds=30,
            request_retry_count=request_retry_count,
            workspace_root=Path("."),
            system_prompt_provider=lambda: "sys",
            prompt_cache_identity_provider=lambda: {"workspace": "w"},
            tools_provider=lambda: [],
            extra_body_provider=lambda: {},
            tool_name_from_function_name=lambda name: name,
            function_name_for_tool=lambda name: name,
            runtime_manager=manager,
        )

    def _request_once(
        self,
        protocol: AgentLLMProtocol,
        messages: list[dict],
    ):
        return protocol.request_reply_once(
            messages,
            on_delta=lambda _t: None,
            on_token_usage=lambda _i, _o, _c: None,
            on_protocol_wait=lambda: None,
        )

    def test_runtime_truncation_after_text_raises_stream_interrupted(self) -> None:
        """已有可见内容后被截断：必须抛 StreamInterruptedAfterOutputError，
        由外层撤销已显示内容并重试，而不是把半截回复当正常完成。"""

        runtime = _TruncationRuntime(
            _Identity(),
            events=[TextDelta(text="回复说到一半")],
            finish_reason="length",
        )
        protocol = self._protocol(runtime)
        with self.assertRaises(StreamInterruptedAfterOutputError):
            self._request_once(protocol, [{"role": "user", "content": "hi"}])

    def test_runtime_truncation_before_text_is_retryable(self) -> None:
        """无任何可见内容即被截断：必须进入可重试路径而不是静默空响应。"""

        for finish_reason in ("length", "incomplete", "max_tokens", "content_filter"):
            with self.subTest(finish_reason=finish_reason):
                runtime = _TruncationRuntime(
                    _Identity(),
                    finish_reason=finish_reason,
                )
                protocol = self._protocol(runtime)
                with self.assertRaises(RetryableAgentRequestError):
                    self._request_once(
                        protocol,
                        [{"role": "user", "content": "hi"}],
                    )

    def test_runtime_normal_stop_passes_through(self) -> None:
        """正常 finish_reason=stop 不受影响。"""

        runtime = _TruncationRuntime(
            _Identity(),
            events=[TextDelta(text="完整回复")],
            finish_reason="stop",
        )
        protocol = self._protocol(runtime)
        reply = self._request_once(protocol, [{"role": "user", "content": "hi"}])
        self.assertEqual(reply.content, "完整回复")
        self.assertEqual(runtime.turn_count, 1)

    def test_request_reply_retries_truncation_then_fails_with_message(self) -> None:
        """截断重试耗尽后必须给出明确错误信息，而不是静默结束。"""

        runtime = _TruncationRuntime(_Identity(), finish_reason="length")
        protocol = self._protocol(runtime, request_retry_count=2)
        retry_messages: list[str] = []
        with self.assertRaises(Exception) as ctx:
            protocol.request_reply(
                [{"role": "user", "content": "hi"}],
                on_delta=lambda _t: None,
                on_token_usage=lambda _i, _o, _c: None,
                on_protocol_wait=lambda: None,
                on_retry_status=lambda msg: retry_messages.append(msg),
            )
        self.assertEqual(runtime.turn_count, 2)
        self.assertTrue(retry_messages, "截断重试必须向 UI 报告状态")
        self.assertIn("中断", str(ctx.exception))

    def test_openai_client_truncation_after_text_raises_stream_interrupted(self) -> None:
        """旧 client 路径同样必须识别截断。"""

        class _FakeCompletions:
            def create(self, **kwargs):
                return iter(
                    [
                        {
                            "choices": [
                                {
                                    "delta": {"content": "旧路径半截回复"},
                                    "finish_reason": None,
                                }
                            ]
                        },
                        {"choices": [{"delta": {}, "finish_reason": "length"}]},
                    ]
                )

        class _FakeClient:
            chat = SimpleNamespace(completions=_FakeCompletions())

        protocol = self._protocol(None, client=_FakeClient())
        with self.assertRaises(StreamInterruptedAfterOutputError):
            self._request_once(protocol, [{"role": "user", "content": "hi"}])

    def test_openai_client_truncation_before_text_is_retryable(self) -> None:
        """旧 client 路径无内容截断必须进入可重试路径。"""

        class _FakeCompletions:
            def create(self, **kwargs):
                return iter(
                    [{"choices": [{"delta": {}, "finish_reason": "length"}]}]
                )

        class _FakeClient:
            chat = SimpleNamespace(completions=_FakeCompletions())

        protocol = self._protocol(None, client=_FakeClient())
        with self.assertRaises(RetryableAgentRequestError):
            self._request_once(protocol, [{"role": "user", "content": "hi"}])

    def test_chat_runtime_tool_call_name_missing_raises_stream_interrupted(self) -> None:
        """Chat Runtime：文字后工具调用名称从未到达（finish_reason=stop）时，
        必须识别为截断而不是静默当正常完成。"""

        class _FakeCompletions:
            def create(self, **kwargs):
                return iter(
                    [
                        {
                            "choices": [
                                {"delta": {"content": "半截文字"}, "finish_reason": None}
                            ]
                        },
                        {
                            "choices": [
                                {
                                    "delta": {
                                        "tool_calls": [
                                            {
                                                "index": 0,
                                                "id": "call_1",
                                                "function": {"arguments": "{\"path\":"},
                                            }
                                        ]
                                    },
                                    "finish_reason": None,
                                }
                            ]
                        },
                        {"choices": [{"delta": {}, "finish_reason": "stop"}]},
                    ]
                )

        class _FakeClient:
            chat = SimpleNamespace(completions=_FakeCompletions())

        identity = ModelIdentity(
            profile_id="p1",
            provider="openai",
            protocol="openai_chat_completions",
            model_id="demo-model",
        )
        profile = ProviderProfile(
            id="p1",
            provider="openai",
            base_url="https://example.test/v1",
            api_key="test-key",
            default_protocol="openai_chat_completions",
        )
        descriptor = ModelDescriptor(
            identity=identity,
            display_name="demo",
            capabilities=ModelCapabilities(streaming=True, tools=True),
            context_window_tokens=128000,
        )
        runtime = OpenAIChatCompletionsRuntime(
            identity=identity,
            capabilities=ModelCapabilities(streaming=True, tools=True),
            client=_FakeClient(),
            profile=profile,
            descriptor=descriptor,
            _owns_client=False,
        )
        protocol = self._protocol(runtime)
        with self.assertRaises(StreamInterruptedAfterOutputError):
            self._request_once(protocol, [{"role": "user", "content": "hi"}])

    def test_chat_runtime_tool_call_truncated_json_raises_stream_interrupted(self) -> None:
        """Chat Runtime：name 已到达但 arguments 是半截 JSON（finish_reason=stop）
        同样必须识别为截断，不能以空参数误执行。"""

        class _FakeCompletions:
            def create(self, **kwargs):
                return iter(
                    [
                        {
                            "choices": [
                                {"delta": {"content": "半截文字"}, "finish_reason": None}
                            ]
                        },
                        {
                            "choices": [
                                {
                                    "delta": {
                                        "tool_calls": [
                                            {
                                                "index": 0,
                                                "id": "call_1",
                                                "function": {
                                                    "name": "write_file",
                                                    "arguments": "{\"path\":\"a\"",
                                                },
                                            }
                                        ]
                                    },
                                    "finish_reason": None,
                                }
                            ]
                        },
                        {"choices": [{"delta": {}, "finish_reason": "stop"}]},
                    ]
                )

        class _FakeClient:
            chat = SimpleNamespace(completions=_FakeCompletions())

        identity = ModelIdentity(
            profile_id="p1",
            provider="openai",
            protocol="openai_chat_completions",
            model_id="demo-model",
        )
        profile = ProviderProfile(
            id="p1",
            provider="openai",
            base_url="https://example.test/v1",
            api_key="test-key",
            default_protocol="openai_chat_completions",
        )
        descriptor = ModelDescriptor(
            identity=identity,
            display_name="demo",
            capabilities=ModelCapabilities(streaming=True, tools=True),
            context_window_tokens=128000,
        )
        runtime = OpenAIChatCompletionsRuntime(
            identity=identity,
            capabilities=ModelCapabilities(streaming=True, tools=True),
            client=_FakeClient(),
            profile=profile,
            descriptor=descriptor,
            _owns_client=False,
        )
        protocol = self._protocol(runtime)
        with self.assertRaises(StreamInterruptedAfterOutputError):
            self._request_once(protocol, [{"role": "user", "content": "hi"}])

    def test_responses_runtime_text_eof_without_completed_is_accepted(self) -> None:
        """兼容中转站：普通文本已经完整输出但 EOF 丢失 completed 时，不应误报流中断。"""

        class _FakeResponses:
            def create(self, **kwargs):
                return iter(
                    [
                        {"type": "response.output_text.delta", "delta": "普通回复"},
                        # 部分中转站省略 response.completed，但会保留文本分片收尾事件。
                        {"type": "response.output_text.done", "text": "普通回复"},
                    ]
                )

        class _FakeClient:
            responses = SimpleNamespace(create=_FakeResponses().create)

        identity = ModelIdentity(
            profile_id="p1",
            provider="openai",
            protocol="openai_responses",
            model_id="demo-model",
        )
        profile = ProviderProfile(
            id="p1",
            provider="openai",
            base_url="https://example.test/v1",
            api_key="test-key",
            default_protocol="openai_responses",
        )
        descriptor = ModelDescriptor(
            identity=identity,
            display_name="demo",
            capabilities=ModelCapabilities(streaming=True, tools=True),
            context_window_tokens=128000,
        )
        runtime = OpenAIResponsesRuntime(
            identity=identity,
            capabilities=ModelCapabilities(streaming=True, tools=True),
            client=_FakeClient(),
            profile=profile,
            descriptor=descriptor,
            _owns_client=False,
        )
        protocol = self._protocol(runtime)
        reply = self._request_once(protocol, [{"role": "user", "content": "hi"}])
        self.assertEqual(reply.content, "普通回复")

    def test_responses_runtime_tool_call_name_missing_raises_stream_interrupted(self) -> None:
        """Responses Runtime：文字后工具调用 name 缺失但收到 response.completed，
        必须识别为截断而不是静默当正常完成。"""

        class _FakeResponses:
            def create(self, **kwargs):
                return iter(
                    [
                        {"type": "response.output_text.delta", "delta": "半截文字"},
                        {
                            "type": "response.function_call_arguments.delta",
                            "item_id": "call_1",
                            "delta": "{\"path\":",
                        },
                        {
                            "type": "response.completed",
                            "response": {"status": "completed", "output": []},
                        },
                    ]
                )

        class _FakeClient:
            responses = SimpleNamespace(create=_FakeResponses().create)

        identity = ModelIdentity(
            profile_id="p1",
            provider="openai",
            protocol="openai_responses",
            model_id="demo-model",
        )
        profile = ProviderProfile(
            id="p1",
            provider="openai",
            base_url="https://example.test/v1",
            api_key="test-key",
            default_protocol="openai_responses",
        )
        descriptor = ModelDescriptor(
            identity=identity,
            display_name="demo",
            capabilities=ModelCapabilities(streaming=True, tools=True),
            context_window_tokens=128000,
        )
        runtime = OpenAIResponsesRuntime(
            identity=identity,
            capabilities=ModelCapabilities(streaming=True, tools=True),
            client=_FakeClient(),
            profile=profile,
            descriptor=descriptor,
            _owns_client=False,
        )
        protocol = self._protocol(runtime)
        with self.assertRaises(StreamInterruptedAfterOutputError):
            self._request_once(protocol, [{"role": "user", "content": "hi"}])

    def test_responses_runtime_tool_call_name_from_done_event_completes(self) -> None:
        """Responses Runtime：中转站仅在 function_call_arguments.done 顶层携带
        name，也必须能完整落地工具调用，而不是误判为“名称截断”。"""

        class _FakeResponses:
            def create(self, **kwargs):
                return iter(
                    [
                        {
                            "type": "response.function_call_arguments.delta",
                            "item_id": "call_1",
                            "delta": '{"path":"a.txt"}',
                        },
                        {
                            "type": "response.function_call_arguments.done",
                            "item_id": "call_1",
                            "name": "write_file",
                            "arguments": '{"path":"a.txt"}',
                        },
                        {
                            "type": "response.completed",
                            "response": {"status": "completed", "output": []},
                        },
                    ]
                )

        class _FakeClient:
            responses = SimpleNamespace(create=_FakeResponses().create)

        identity = ModelIdentity(
            profile_id="p1",
            provider="openai",
            protocol="openai_responses",
            model_id="demo-model",
        )
        profile = ProviderProfile(
            id="p1",
            provider="openai",
            base_url="https://example.test/v1",
            api_key="test-key",
            default_protocol="openai_responses",
        )
        descriptor = ModelDescriptor(
            identity=identity,
            display_name="demo",
            capabilities=ModelCapabilities(streaming=True, tools=True),
            context_window_tokens=128000,
        )
        runtime = OpenAIResponsesRuntime(
            identity=identity,
            capabilities=ModelCapabilities(streaming=True, tools=True),
            client=_FakeClient(),
            profile=profile,
            descriptor=descriptor,
            _owns_client=False,
        )
        protocol = self._protocol(runtime)
        reply = self._request_once(protocol, [{"role": "user", "content": "hi"}])
        self.assertEqual(len(reply.tool_calls), 1)
        self.assertEqual(reply.tool_calls[0].name, "write_file")
        self.assertEqual(reply.tool_calls[0].arguments, {"path": "a.txt"})

    def test_responses_runtime_complete_tool_call_eof_without_completed_is_accepted(self) -> None:
        """兼容中转站：完整 arguments.done 后 EOF 丢失 completed 时，工具调用仍应交给上层。"""

        class _FakeResponses:
            def create(self, **kwargs):
                return iter(
                    [
                        {
                            "type": "response.function_call_arguments.delta",
                            "item_id": "call_1",
                            "delta": '{"path":"a.txt"}',
                        },
                        {
                            "type": "response.function_call_arguments.done",
                            "item_id": "call_1",
                            "name": "read_file",
                            "arguments": '{"path":"a.txt"}',
                        },
                        # 网关在这里直接 EOF，不发送 response.completed。
                    ]
                )

        class _FakeClient:
            responses = SimpleNamespace(create=_FakeResponses().create)

        identity = ModelIdentity(
            profile_id="p1",
            provider="openai",
            protocol="openai_responses",
            model_id="demo-model",
        )
        profile = ProviderProfile(
            id="p1",
            provider="openai",
            base_url="https://example.test/v1",
            api_key="test-key",
            default_protocol="openai_responses",
        )
        descriptor = ModelDescriptor(
            identity=identity,
            display_name="demo",
            capabilities=ModelCapabilities(streaming=True, tools=True),
            context_window_tokens=128000,
        )
        runtime = OpenAIResponsesRuntime(
            identity=identity,
            capabilities=ModelCapabilities(streaming=True, tools=True),
            client=_FakeClient(),
            profile=profile,
            descriptor=descriptor,
            _owns_client=False,
        )
        protocol = self._protocol(runtime)
        reply = self._request_once(protocol, [{"role": "user", "content": "hi"}])
        self.assertEqual(len(reply.tool_calls), 1)
        self.assertEqual(reply.tool_calls[0].name, "read_file")
        self.assertEqual(reply.tool_calls[0].arguments, {"path": "a.txt"})

    def test_responses_runtime_incomplete_tool_call_eof_still_raises(self) -> None:
        """工具参数未形成完整 JSON 时，即使已收到名称也不能接受 EOF。"""

        class _FakeResponses:
            def create(self, **kwargs):
                return iter(
                    [
                        {
                            "type": "response.function_call_arguments.delta",
                            "item_id": "call_1",
                            "delta": '{"path":',
                        },
                        {
                            "type": "response.function_call_arguments.done",
                            "item_id": "call_1",
                            "name": "read_file",
                            "arguments": '{"path":',
                        },
                        # 没有 response.completed，且参数本身不完整。
                    ]
                )

        class _FakeClient:
            responses = SimpleNamespace(create=_FakeResponses().create)

        identity = ModelIdentity(
            profile_id="p1",
            provider="openai",
            protocol="openai_responses",
            model_id="demo-model",
        )
        profile = ProviderProfile(
            id="p1",
            provider="openai",
            base_url="https://example.test/v1",
            api_key="test-key",
            default_protocol="openai_responses",
        )
        descriptor = ModelDescriptor(
            identity=identity,
            display_name="demo",
            capabilities=ModelCapabilities(streaming=True, tools=True),
            context_window_tokens=128000,
        )
        runtime = OpenAIResponsesRuntime(
            identity=identity,
            capabilities=ModelCapabilities(streaming=True, tools=True),
            client=_FakeClient(),
            profile=profile,
            descriptor=descriptor,
            _owns_client=False,
        )
        protocol = self._protocol(runtime)
        with self.assertRaises(RetryableAgentRequestError):
            self._request_once(protocol, [{"role": "user", "content": "hi"}])

    def test_responses_runtime_tool_call_name_from_added_event_completes(self) -> None:
        """Responses Runtime：中转站仅在 output_item.added 事件携带 name，
        也必须能完整落地工具调用，而不是误判为“名称截断”。"""

        class _FakeResponses:
            def create(self, **kwargs):
                return iter(
                    [
                        {
                            "type": "response.output_item.added",
                            "item": {
                                "type": "function_call",
                                "id": "call_1",
                                "call_id": "call_1",
                                "name": "write_file",
                                "arguments": "",
                            },
                        },
                        {
                            "type": "response.function_call_arguments.delta",
                            "item_id": "call_1",
                            "delta": '{"path":"a.txt"}',
                        },
                        {
                            "type": "response.completed",
                            "response": {"status": "completed", "output": []},
                        },
                    ]
                )

        class _FakeClient:
            responses = SimpleNamespace(create=_FakeResponses().create)

        identity = ModelIdentity(
            profile_id="p1",
            provider="openai",
            protocol="openai_responses",
            model_id="demo-model",
        )
        profile = ProviderProfile(
            id="p1",
            provider="openai",
            base_url="https://example.test/v1",
            api_key="test-key",
            default_protocol="openai_responses",
        )
        descriptor = ModelDescriptor(
            identity=identity,
            display_name="demo",
            capabilities=ModelCapabilities(streaming=True, tools=True),
            context_window_tokens=128000,
        )
        runtime = OpenAIResponsesRuntime(
            identity=identity,
            capabilities=ModelCapabilities(streaming=True, tools=True),
            client=_FakeClient(),
            profile=profile,
            descriptor=descriptor,
            _owns_client=False,
        )
        protocol = self._protocol(runtime)
        reply = self._request_once(protocol, [{"role": "user", "content": "hi"}])
        self.assertEqual(len(reply.tool_calls), 1)
        self.assertEqual(reply.tool_calls[0].name, "write_file")
        self.assertEqual(reply.tool_calls[0].arguments, {"path": "a.txt"})

    def test_responses_runtime_tool_call_eof_with_distinct_item_and_call_ids_is_accepted(self) -> None:
        """真实网关格式：item_id 与 call_id 不同且 EOF 丢失所有收尾事件时，仍应执行工具。"""

        class _FakeResponses:
            def create(self, **kwargs):
                return iter(
                    [
                        {
                            "type": "response.created",
                        },
                        {
                            "type": "response.output_item.added",
                            "item": {
                                "type": "function_call",
                                "id": "item_1",
                                "name": "read_file",
                                "arguments": "",
                            },
                        },
                        {
                            "type": "response.function_call_arguments.delta",
                            "item_id": "item_1",
                            "call_id": "fc_1",
                            "name": "read_file",
                            "delta": '{"path":"README.md"}',
                        },
                        # 真实网关在这里直接 EOF：没有 arguments.done/completed。
                    ]
                )

        class _FakeClient:
            responses = SimpleNamespace(create=_FakeResponses().create)

        identity = ModelIdentity(
            profile_id="p1",
            provider="openai",
            protocol="openai_responses",
            model_id="demo-model",
        )
        profile = ProviderProfile(
            id="p1",
            provider="openai",
            base_url="https://example.test/v1",
            api_key="test-key",
            default_protocol="openai_responses",
        )
        descriptor = ModelDescriptor(
            identity=identity,
            display_name="demo",
            capabilities=ModelCapabilities(streaming=True, tools=True),
            context_window_tokens=128000,
        )
        runtime = OpenAIResponsesRuntime(
            identity=identity,
            capabilities=ModelCapabilities(streaming=True, tools=True),
            client=_FakeClient(),
            profile=profile,
            descriptor=descriptor,
            _owns_client=False,
        )
        protocol = self._protocol(runtime)
        reply = self._request_once(protocol, [{"role": "user", "content": "hi"}])
        self.assertEqual(len(reply.tool_calls), 1)
        self.assertEqual(reply.tool_calls[0].id, "fc_1")
        self.assertEqual(reply.tool_calls[0].name, "read_file")
        self.assertEqual(reply.tool_calls[0].arguments, {"path": "README.md"})

    def test_openai_client_tool_call_name_missing_raises_stream_interrupted(self) -> None:
        """旧 client 路径：tool_calls 到达但名称缺失且 finish_reason=stop，
        同样必须进入回滚路径而不是静默当正常完成。"""

        class _FakeCompletions:
            def create(self, **kwargs):
                return iter(
                    [
                        {
                            "choices": [
                                {"delta": {"content": "半截文字"}, "finish_reason": None}
                            ]
                        },
                        {
                            "choices": [
                                {
                                    "delta": {
                                        "tool_calls": [
                                            {
                                                "index": 0,
                                                "id": "call_1",
                                                "function": {"arguments": "{\"path\":"},
                                            }
                                        ]
                                    },
                                    "finish_reason": None,
                                }
                            ]
                        },
                        {"choices": [{"delta": {}, "finish_reason": "stop"}]},
                    ]
                )

        class _FakeClient:
            chat = SimpleNamespace(completions=_FakeCompletions())

        protocol = self._protocol(None, client=_FakeClient())
        with self.assertRaises(StreamInterruptedAfterOutputError):
            self._request_once(protocol, [{"role": "user", "content": "hi"}])


def _slow_tool(name: str, sleep_seconds: float, *, serial: bool = False) -> ToolDefinition:
    """构造一个执行 sleep_seconds 的工具；serial=True 时模拟写入类工具。"""

    return ToolDefinition(
        name=name,
        description="测试工具",
        argument_schema='{"type":"object","properties":{},"required":[]}',
        requires_confirmation=False,
        run=lambda _arguments: (
            time.sleep(sleep_seconds) or ToolResult(ok=True, output="done")
        ),
    )


class ToolExecutionTimeoutTests(unittest.TestCase):
    """工具执行超时：挂起工具必须在限时内返回错误结果，而不是无限等待。"""

    def _agent(self, tools: dict[str, ToolDefinition], timeout: int) -> LocalToolAgent:
        agent = object.__new__(LocalToolAgent)
        agent._tools = dict(tools)
        agent.config = SimpleNamespace(
            tool_timeout_seconds=timeout,
        )
        # 绕过审批/会话/模型回填依赖，聚焦超时行为本身。
        agent._approve_tool_for_batch = (
            lambda tool, arguments, **kwargs: None
        )
        agent._prepare_tool_result_for_model = (
            lambda tool_call, tool_result, **kwargs: (tool_result, [])
        )
        agent._tool_result_message = (
            lambda tool_call, tool_result: {
                "role": "tool",
                "content": str(tool_result.output),
            }
        )
        return agent

    def _call(self, agent: LocalToolAgent, name: str) -> list[ToolResult]:
        observations = agent._execute_tool_batch(
            [ToolCall(name=name, arguments={}, id="call_1")],
            1,
            report_tool_start=lambda _step, _call: None,
            report_tool_result=lambda _call, _result: None,
            check_cancelled=lambda: None,
            status=lambda _msg: None,
            tools=agent._tools,
            persist_session_events=False,
        )
        return [observation.result for observation in observations]

    def test_parallel_tool_timeout_returns_error_result(self) -> None:
        """并行工具超过限时：返回超时错误结果，不再无限等待。"""

        agent = self._agent(
            {"slow_tool": _slow_tool("slow_tool", 0.6)},
            timeout=0.2,
        )
        started = time.monotonic()
        results = self._call(agent, "slow_tool")
        elapsed = time.monotonic() - started
        self.assertEqual(len(results), 1)
        self.assertFalse(results[0].ok)
        self.assertIn("超时", results[0].output)
        self.assertLess(elapsed, 0.5, "超时必须在限时附近返回，而不是等待工具自然结束")

    def test_serial_tool_timeout_returns_error_result(self) -> None:
        """串行（写入类）工具同样受超时保护。"""

        agent = self._agent(
            {"write_file": _slow_tool("write_file", 0.6, serial=True)},
            timeout=0.2,
        )
        started = time.monotonic()
        results = self._call(agent, "write_file")
        elapsed = time.monotonic() - started
        self.assertEqual(len(results), 1)
        self.assertFalse(results[0].ok)
        self.assertIn("超时", results[0].output)
        self.assertLess(elapsed, 0.5)

    def test_tool_within_timeout_succeeds(self) -> None:
        """限时内完成的工具不受影响。"""

        agent = self._agent(
            {"fast_tool": _slow_tool("fast_tool", 0.05)},
            timeout=2,
        )
        results = self._call(agent, "fast_tool")
        self.assertEqual(len(results), 1)
        self.assertTrue(results[0].ok)

    def test_timeout_from_config_when_argument_omitted(self) -> None:
        """未显式传限时时使用 AgentConfig.tool_timeout_seconds。"""

        agent = self._agent(
            {"slow_tool": _slow_tool("slow_tool", 0.6)},
            timeout=0.2,
        )
        started = time.monotonic()
        observations = agent._execute_tool_batch(
            [ToolCall(name="slow_tool", arguments={}, id="call_1")],
            1,
            report_tool_start=lambda _step, _call: None,
            report_tool_result=lambda _call, _result: None,
            check_cancelled=lambda: None,
            status=lambda _msg: None,
            tools=agent._tools,
            persist_session_events=False,
        )
        elapsed = time.monotonic() - started
        self.assertFalse(observations[0].result.ok)
        self.assertLess(elapsed, 0.5)


if __name__ == "__main__":
    unittest.main()
