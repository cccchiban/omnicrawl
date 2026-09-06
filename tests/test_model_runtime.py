"""多模型 Runtime / Agent 协议门面的基础回归。"""

from __future__ import annotations

import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from omnicrawl.agent.core import LocalToolAgent
from omnicrawl.agent.runtime.llm_protocol import AgentLLMProtocol
from omnicrawl.llm.capabilities import (
    ModelCapabilities,
    conservative_openai_chat_capabilities,
    merge_capabilities,
)
from omnicrawl.llm.errors import ModelError
from omnicrawl.llm.protocol import (
    ConversationMessage,
    GenerationOptions,
    ModelIdentity,
    ModelTurnRequest,
    ResponseCompleted,
    TextBlock,
    TextDelta,
    ToolCallBlock,
    ToolCallCompleted,
    ToolResultBlock,
    UsageUpdated,
)
from omnicrawl.llm.providers.openai_common import build_prompt_cache_key
from omnicrawl.llm.providers.openai_responses import OpenAIResponsesRuntime
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
    def test_plan_mode_prompt_is_loaded_and_appended_last(self) -> None:
        """模式提示词应加载自模板，并位于基础 system prompt 之后。"""

        agent = object.__new__(LocalToolAgent)
        agent._system_prompt_template = "基础系统规则"

        self.assertEqual(agent.activate_mode("plan"), "plan")
        prompt = agent._system_prompt()
        mode_text = Path(__file__).resolve().parents[1].joinpath(
            "omnicrawl", "templates", "plan.md"
        ).read_text(encoding="utf-8").strip()

        self.assertIn(mode_text, prompt)
        self.assertTrue(
            prompt.endswith(
                '<active_mode_prompt name="plan">\n'
                f"{mode_text}\n"
                "</active_mode_prompt>"
            )
        )
        self.assertLess(prompt.index("基础系统规则"), prompt.index("<active_mode_prompt"))

    def test_plan_mode_rejects_invalid_template_names(self) -> None:
        """模式名称只允许安全文件名，不能越出 templates 目录。"""

        agent = object.__new__(LocalToolAgent)

        with self.assertRaisesRegex(Exception, "模式名称"):
            agent.activate_mode("../system_prompt")

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

    def test_switch_during_turn_allowed_with_flag_next_request_uses_new_model(self) -> None:
        """回合中切换（allow_during_turn=True）：当前回合继续用旧快照，
        下一次 acquire_turn 拿到新快照；旧 runtime 在引用归零后才 close。"""

        profile, descriptor = self._descriptor("m1")
        manager = ModelRuntimeManager()
        old_runtime = _FakeRuntime(descriptor.identity)
        manager.bootstrap(profile, descriptor, runtime=old_runtime)

        # 模拟回合中：持有旧快照引用
        turn_snapshot = manager.acquire_turn()
        self.assertEqual(manager.current_model_id(), "m1")

        # 回合中切换（不带 allow_during_turn → 应抛错）
        profile2, descriptor2 = self._descriptor("m2")
        with self.assertRaises(ModelError):
            manager.switch(
                profile2,
                descriptor2,
                runtime_factory=lambda p, d: _FakeRuntime(d.identity),
            )
        # 旧模型仍有效
        self.assertEqual(manager.current_model_id(), "m1")

        # 带 allow_during_turn=True → 切换成功
        new_runtime = _FakeRuntime(descriptor2.identity)
        snap2 = manager.switch(
            profile2,
            descriptor2,
            allow_during_turn=True,
            runtime_factory=lambda p, d: new_runtime,
        )
        self.assertEqual(manager.current_model_id(), "m2")
        self.assertEqual(snap2.generation, 2)
        # 当前回合仍持旧快照，旧 runtime 不应被 close
        self.assertFalse(old_runtime.closed)

        # 当前回合结束时释放旧快照 → 旧 runtime 才被 close
        manager.release_turn(turn_snapshot)
        self.assertTrue(old_runtime.closed)
        # 下一次 acquire_turn 拿到新快照（m2）
        next_snapshot = manager.acquire_turn()
        self.assertEqual(next_snapshot.generation, 2)
        manager.release_turn(next_snapshot)
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

    def test_should_send_stable_prompt_cache_key_for_gpt_responses_runtime(self) -> None:
        """GPT Responses 请求必须带稳定的缓存路由键。"""

        calls: list[dict] = []

        def create(**kwargs):
            calls.append(kwargs)
            return iter(
                [
                    type(
                        "ResponseTextDelta",
                        (),
                        {"type": "response.output_text.delta", "delta": "完成"},
                    )(),
                    type(
                        "ResponseCompleted",
                        (),
                        {"type": "response.completed"},
                    )()
                ]
            )

        identity = ModelIdentity(
            profile_id="p1",
            provider="openai",
            protocol="openai_responses",
            model_id="gpt-5.4-mini",
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
            display_name=identity.model_id,
            capabilities=ModelCapabilities(
                streaming=True,
                tools=True,
                reasoning=True,
            ),
            context_window_tokens=128_000,
        )
        runtime = OpenAIResponsesRuntime(
            identity=identity,
            capabilities=descriptor.capabilities.resolved(),
            client=type(
                "FakeClient",
                (),
                {"responses": type("FakeResponses", (), {"create": staticmethod(create)})()},
            )(),
            profile=profile,
            descriptor=descriptor,
            _owns_client=False,
        )
        request = ModelTurnRequest(
            identity=identity,
            system_prompt="stable system prompt",
            messages=(),
            generation_options=GenerationOptions(request_timeout_seconds=30),
            prompt_cache_identity={"workspace": "w", "prompt": "stable"},
        )

        list(runtime.stream_turn(request))

        self.assertEqual(len(calls), 1)
        self.assertEqual(
            calls[0].get("prompt_cache_key"),
            build_prompt_cache_key(
                request.prompt_cache_identity,
                model=identity.model_id,
            ),
        )

    def test_responses_tool_result_does_not_force_final_summary(self) -> None:
        """工具结果后应保持协议观察，不得把当前任务改写成强制总结。"""

        calls: list[dict] = []

        def create(**kwargs):
            calls.append(kwargs)
            return iter(
                [
                    type(
                        "ResponseTextDelta",
                        (),
                        {"type": "response.output_text.delta", "delta": "继续处理"},
                    )(),
                    type(
                        "ResponseCompleted",
                        (),
                        {"type": "response.completed"},
                    )(),
                ]
            )

        identity = ModelIdentity(
            profile_id="p1",
            provider="openai",
            protocol="openai_responses",
            model_id="gpt-5.4-mini",
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
            display_name=identity.model_id,
            capabilities=ModelCapabilities(streaming=True, tools=True, reasoning=True),
            context_window_tokens=128_000,
        )
        runtime = OpenAIResponsesRuntime(
            identity=identity,
            capabilities=descriptor.capabilities.resolved(),
            client=type(
                "FakeClient",
                (),
                {"responses": type("FakeResponses", (), {"create": staticmethod(create)})()},
            )(),
            profile=profile,
            descriptor=descriptor,
            _owns_client=False,
        )
        request = ModelTurnRequest(
            identity=identity,
            system_prompt="stable system prompt",
            messages=(
                ConversationMessage(
                    role="assistant",
                    blocks=(
                        ToolCallBlock(
                            call_id="call_1",
                            name="read",
                            arguments={"path": "a.py"},
                        ),
                    ),
                ),
                ConversationMessage(
                    role="tool",
                    blocks=(
                        ToolResultBlock(
                            call_id="call_1",
                            ok=True,
                            content="继续检查依赖",
                        ),
                    ),
                ),
            ),
            generation_options=GenerationOptions(request_timeout_seconds=30),
        )

        list(runtime.stream_turn(request))

        self.assertEqual(len(calls), 1)
        input_items = calls[0]["input"]
        input_text = str(input_items)
        self.assertNotIn("最终总结", input_text)
        self.assertNotIn("开场问候", input_text)
        self.assertEqual(input_items[-1]["type"], "function_call_output")

    def test_should_retry_responses_without_prompt_cache_key_when_gateway_rejects_it(self) -> None:
        """不支持 prompt_cache_key 的网关应降级重试一次。"""

        calls: list[dict] = []

        def create(**kwargs):
            calls.append(kwargs)
            if len(calls) == 1:
                raise RuntimeError("unknown parameter prompt_cache_key")
            return iter(
                [
                    type(
                        "ResponseTextDelta",
                        (),
                        {"type": "response.output_text.delta", "delta": "完成"},
                    )(),
                    type(
                        "ResponseCompleted",
                        (),
                        {"type": "response.completed"},
                    )()
                ]
            )

        identity = ModelIdentity(
            profile_id="p1",
            provider="openai",
            protocol="openai_responses",
            model_id="gpt-5.4-mini",
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
            display_name=identity.model_id,
            capabilities=ModelCapabilities(streaming=True, tools=True, reasoning=True),
            context_window_tokens=128_000,
        )
        runtime = OpenAIResponsesRuntime(
            identity=identity,
            capabilities=descriptor.capabilities.resolved(),
            client=type(
                "FakeClient",
                (),
                {"responses": type("FakeResponses", (), {"create": staticmethod(create)})()},
            )(),
            profile=profile,
            descriptor=descriptor,
            _owns_client=False,
        )
        request = ModelTurnRequest(
            identity=identity,
            system_prompt="stable system prompt",
            messages=(),
            generation_options=GenerationOptions(request_timeout_seconds=30),
            prompt_cache_identity={"workspace": "w", "prompt": "stable"},
        )

        events = list(runtime.stream_turn(request))

        self.assertEqual(len(calls), 2)
        self.assertIn("prompt_cache_key", calls[0])
        self.assertNotIn("prompt_cache_key", calls[1])
        self.assertEqual(events[0].text, "完成")

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
            tool_name_from_function_name=lambda name: "read" if name == "tool_demo" else name,
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
        self.assertEqual(reply.tool_calls[0].name, "read")
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
