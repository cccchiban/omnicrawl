"""多模型 Runtime / Agent 协议门面的基础回归。"""

from __future__ import annotations

import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from omnicrawl.agent.llm_protocol import AgentLLMProtocol
from omnicrawl.llm.capabilities import (
    ModelCapabilities,
    conservative_openai_chat_capabilities,
    merge_capabilities,
)
from omnicrawl.llm.protocol import (
    ModelIdentity,
    ResponseCompleted,
    TextDelta,
    ToolCallCompleted,
    UsageUpdated,
)
from omnicrawl.llm.registry import ModelDescriptor, ProviderProfile
from omnicrawl.llm.runtime import ModelRuntimeManager


class _FakeRuntime:
    def __init__(self, identity: ModelIdentity, *, with_tool: bool = False) -> None:
        self.identity = identity
        self.capabilities = ModelCapabilities(streaming=True, tools=True)
        self.with_tool = with_tool
        self.closed = False

    def stream_turn(self, request, *, cancel_check=None):
        if self.with_tool:
            yield ToolCallCompleted(
                call_id="call_1",
                name="tool_demo",
                arguments={"path": "a.txt"},
            )
            yield UsageUpdated(input_tokens=5, output_tokens=1, cached_input_tokens=0)
            yield ResponseCompleted(finish_reason="tool_calls")
            return
        yield TextDelta(text="你好")
        yield TextDelta(text="世界")
        yield UsageUpdated(input_tokens=4, output_tokens=2, cached_input_tokens=1)
        yield ResponseCompleted(finish_reason="stop")

    def close(self) -> None:
        self.closed = True


class _CapturingRuntime(_FakeRuntime):
    def __init__(self, identity: ModelIdentity) -> None:
        super().__init__(identity)
        self.last_request = None

    def stream_turn(self, request, *, cancel_check=None):
        self.last_request = request
        yield from super().stream_turn(request, cancel_check=cancel_check)


class ModelRuntimeTests(unittest.TestCase):
    def _descriptor(
        self,
        model_id: str = "demo-model",
        *,
        max_output_tokens: int = 0,
        temperature: float | None = None,
        provider_options: dict | None = None,
    ) -> tuple[ProviderProfile, ModelDescriptor]:
        identity = ModelIdentity(
            profile_id="p1",
            provider="openai",
            protocol="openai_chat_completions",
            model_id=model_id,
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
            display_name=model_id,
            capabilities=ModelCapabilities(
                streaming=True,
                tools=True,
                max_output_tokens=max_output_tokens,
            ),
            context_window_tokens=128000,
            max_output_tokens=max_output_tokens,
            temperature=temperature,
            provider_options=provider_options or {},
        )
        return profile, descriptor

    def test_runtime_manager_bootstrap_and_switch(self) -> None:
        profile, descriptor = self._descriptor("m1")
        manager = ModelRuntimeManager()
        snap1 = manager.bootstrap(profile, descriptor, runtime=_FakeRuntime(descriptor.identity))
        self.assertEqual(manager.current_model_id(), "m1")
        self.assertEqual(snap1.generation, 1)

        profile2, descriptor2 = self._descriptor("m2")
        snap2 = manager.switch(
            profile2,
            descriptor2,
            runtime_factory=lambda p, d: _FakeRuntime(d.identity),
        )
        self.assertEqual(manager.current_model_id(), "m2")
        self.assertEqual(snap2.generation, 2)
        manager.close()

    def test_should_update_runtime_context_window_when_setting_changes(self) -> None:
        profile, descriptor = self._descriptor()
        manager = ModelRuntimeManager()
        manager.bootstrap(profile, descriptor, runtime=_FakeRuntime(descriptor.identity))

        manager.set_context_window_tokens(256_000)

        self.assertEqual(manager.current_context_window(), 256_000)
        self.assertEqual(manager.active_snapshot.context_window_tokens, 256_000)
        manager.close()

    def test_switch_persist_failure_keeps_old_snapshot_and_closes_candidate(self) -> None:
        profile, descriptor = self._descriptor("m1")
        manager = ModelRuntimeManager()
        manager.bootstrap(profile, descriptor, runtime=_FakeRuntime(descriptor.identity))
        profile2, descriptor2 = self._descriptor("m2")
        candidate = _FakeRuntime(descriptor2.identity)

        with self.assertRaisesRegex(RuntimeError, "disk failure"):
            manager.switch(
                profile2,
                descriptor2,
                persist=lambda: (_ for _ in ()).throw(RuntimeError("disk failure")),
                runtime_factory=lambda _p, _d: candidate,
            )

        self.assertEqual(manager.current_model_id(), "m1")
        self.assertEqual(manager.generation, 1)
        self.assertTrue(candidate.closed)
        manager.close()

    def test_agent_protocol_applies_model_default_generation_options(self) -> None:
        profile, descriptor = self._descriptor(
            max_output_tokens=2048,
            temperature=0.2,
            provider_options={"top_p": 0.9},
        )
        manager = ModelRuntimeManager()
        runtime = _CapturingRuntime(descriptor.identity)
        manager.bootstrap(profile, descriptor, runtime=runtime)

        protocol = AgentLLMProtocol(
            client=None,
            model="demo-model",
            request_timeout_seconds=30,
            request_retry_count=1,
            workspace_root=Path("."),
            system_prompt_provider=lambda: "sys",
            prompt_cache_identity_provider=lambda: {"workspace": "w"},
            tools_provider=lambda: [],
            extra_body_provider=lambda: {},
            tool_name_from_function_name=lambda name: name,
            function_name_for_tool=lambda name: name,
            runtime_manager=manager,
        )
        protocol.request_reply(
            [{"role": "user", "content": "hi"}],
            on_delta=lambda _t: None,
            on_token_usage=lambda _i, _o, _c: None,
            on_protocol_wait=lambda: None,
            on_retry_status=lambda _msg: None,
        )
        options = runtime.last_request.generation_options
        self.assertEqual(options.max_output_tokens, 2048)
        self.assertEqual(options.temperature, 0.2)
        self.assertEqual(options.provider_options.get("top_p"), 0.9)
        manager.close()

    def test_agent_protocol_extra_body_overrides_model_defaults(self) -> None:
        profile, descriptor = self._descriptor(
            max_output_tokens=2048,
            temperature=0.2,
        )
        manager = ModelRuntimeManager()
        runtime = _CapturingRuntime(descriptor.identity)
        manager.bootstrap(profile, descriptor, runtime=runtime)

        protocol = AgentLLMProtocol(
            client=None,
            model="demo-model",
            request_timeout_seconds=30,
            request_retry_count=1,
            workspace_root=Path("."),
            system_prompt_provider=lambda: "sys",
            prompt_cache_identity_provider=lambda: {"workspace": "w"},
            tools_provider=lambda: [],
            extra_body_provider=lambda: {
                "max_output_tokens": 512,
                "temperature": 0.9,
            },
            tool_name_from_function_name=lambda name: name,
            function_name_for_tool=lambda name: name,
            runtime_manager=manager,
        )
        protocol.request_reply(
            [{"role": "user", "content": "hi"}],
            on_delta=lambda _t: None,
            on_token_usage=lambda _i, _o, _c: None,
            on_protocol_wait=lambda: None,
            on_retry_status=lambda _msg: None,
        )
        options = runtime.last_request.generation_options
        self.assertEqual(options.max_output_tokens, 512)
        self.assertEqual(options.temperature, 0.9)
        manager.close()

    def test_agent_protocol_uses_runtime_stream(self) -> None:
        profile, descriptor = self._descriptor()
        manager = ModelRuntimeManager()
        manager.bootstrap(profile, descriptor, runtime=_FakeRuntime(descriptor.identity))

        deltas: list[str] = []
        usages: list[tuple[int, int, int]] = []
        protocol = AgentLLMProtocol(
            client=None,
            model="demo-model",
            request_timeout_seconds=30,
            request_retry_count=1,
            workspace_root=Path("."),
            system_prompt_provider=lambda: "sys",
            prompt_cache_identity_provider=lambda: {"workspace": "w"},
            tools_provider=lambda: [],
            extra_body_provider=lambda: {},
            tool_name_from_function_name=lambda name: name,
            function_name_for_tool=lambda name: name,
            runtime_manager=manager,
        )
        reply = protocol.request_reply(
            [{"role": "user", "content": "hi"}],
            on_delta=deltas.append,
            on_token_usage=lambda i, o, c: usages.append((i, o, c)),
            on_protocol_wait=lambda: None,
            on_retry_status=lambda _msg: None,
        )
        self.assertEqual(reply.content, "你好世界")
        self.assertEqual(deltas, ["你好", "世界"])
        self.assertEqual(usages, [(4, 2, 1)])
        self.assertFalse(reply.tool_calls)
        manager.close()

    def test_agent_protocol_maps_runtime_tool_calls(self) -> None:
        profile, descriptor = self._descriptor()
        manager = ModelRuntimeManager()
        manager.bootstrap(
            profile,
            descriptor,
            runtime=_FakeRuntime(descriptor.identity, with_tool=True),
        )
        protocol = AgentLLMProtocol(
            client=None,
            model="demo-model",
            request_timeout_seconds=30,
            request_retry_count=1,
            workspace_root=Path("."),
            system_prompt_provider=lambda: "sys",
            prompt_cache_identity_provider=lambda: {},
            tools_provider=lambda: [],
            extra_body_provider=lambda: {},
            tool_name_from_function_name=lambda name: "read_file" if name == "tool_demo" else name,
            function_name_for_tool=lambda name: "tool_demo",
            runtime_manager=manager,
        )
        reply = protocol.request_reply(
            [{"role": "user", "content": "hi"}],
            on_delta=lambda _t: None,
            on_token_usage=lambda *_args: None,
            on_protocol_wait=lambda: None,
            on_retry_status=lambda _msg: None,
        )
        self.assertEqual(len(reply.tool_calls), 1)
        self.assertEqual(reply.tool_calls[0].name, "read_file")
        self.assertEqual(reply.tool_calls[0].function_name, "tool_demo")
        self.assertEqual(reply.tool_calls[0].arguments, {"path": "a.txt"})
        manager.close()

class MergeCapabilitiesTests(unittest.TestCase):
    def test_sparse_context_window_does_not_clear_tools(self) -> None:
        """Adapter 仅补 context_window 时不得把 tools 打成 False。"""

        base = conservative_openai_chat_capabilities()
        self.assertTrue(base.tools)
        merged = merge_capabilities(
            base,
            ModelCapabilities(tools=True, parallel_tool_calls=True, streaming=True),
            ModelCapabilities(context_window_tokens=1_000_000),
        )
        self.assertTrue(merged.tools)
        self.assertTrue(merged.parallel_tool_calls)
        self.assertEqual(merged.context_window_tokens, 1_000_000)

    def test_full_layer_can_disable_tools(self) -> None:
        """完整能力层仍可显式关闭 tools。"""

        merged = merge_capabilities(
            conservative_openai_chat_capabilities(),
            ModelCapabilities(streaming=True, tools=False),
        )
        self.assertFalse(merged.tools)


if __name__ == "__main__":
    unittest.main()
