from __future__ import annotations

import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch

from omnicrawl.agent.types import AgentModelReply, ToolImageAttachment
from omnicrawl.agent.runtime.vision_proxy import VisionModelProxy, VisionProxyError
from omnicrawl.config.models.llm import ActiveModelRef, LLMConfig
from omnicrawl.config.models.vision import VisionConfiguration


class _FakeManager:
    instances: list["_FakeManager"] = []

    def __init__(self) -> None:
        self.snapshot = SimpleNamespace(
            descriptor=SimpleNamespace(model_id="vision-model"),
            runtime=SimpleNamespace(identity=SimpleNamespace(model_id="vision-model")),
        )
        self.released = False
        self.closed = False
        self.__class__.instances.append(self)

    def bootstrap(self, _profile, _descriptor):
        return self.snapshot

    def acquire_turn(self):
        return self.snapshot

    def release_turn(self, _snapshot) -> None:
        self.released = True

    def close(self) -> None:
        self.closed = True


class _FakeProtocol:
    responses: list[object] = []
    requests: list[dict] = []

    def __init__(self, **kwargs) -> None:
        self.kwargs = kwargs

    def request_reply(self, messages, _on_delta, on_usage, *_args):
        self.__class__.requests.append(
            {
                "messages": messages,
                "tools": self.kwargs["tools_provider"](),
                "system": self.kwargs["system_prompt_provider"](),
            }
        )
        on_usage(11, 7, 2)
        response = self.__class__.responses.pop(0)
        if isinstance(response, BaseException):
            raise response
        return response


class _CancelledError(RuntimeError):
    pass


class VisionModelProxyTests(unittest.TestCase):
    def setUp(self) -> None:
        _FakeManager.instances = []
        _FakeProtocol.responses = []
        _FakeProtocol.requests = []

    def _proxy(self, refs: tuple[ActiveModelRef, ...]) -> VisionModelProxy:
        return VisionModelProxy(
            base_llm=SimpleNamespace(
                model="main-model",
                profile_id="main-profile",
                request_timeout_seconds=30,
                request_retry_count=3,
            ),
            configuration=VisionConfiguration(enabled=True, models=refs),
            workspace_root=Path.cwd(),
            max_output_chars=200,
        )

    def test_failover_uses_next_model_and_sends_only_image_request(self) -> None:
        refs = (
            ActiveModelRef(source="custom", key="vision-first"),
            ActiveModelRef(
                source="detected",
                profile="vision-profile",
                model_id="vision-second",
                protocol="openai_responses",
            ),
        )
        _FakeProtocol.responses = [
            RuntimeError("first model unavailable"),
            AgentModelReply(message={"role": "assistant"}, content="屏幕上有一个设置窗口。"),
        ]
        selections: list[str] = []
        selected_protocols: list[str] = []
        usage: list[tuple[int, int, int]] = []

        def select(_base, token):
            selections.append(token)
            return LLMConfig(
                api_key="test-key",
                base_url="https://example.test/v1",
                model=token,
                profile_id="vision-profile",
                provider="openai",
                protocol="openai_chat_completions",
                model_source="detected",
                request_timeout_seconds=20,
                request_retry_count=4,
            )

        def build_descriptor(config):
            selected_protocols.append(config.protocol)
            return SimpleNamespace(), SimpleNamespace()

        with patch("omnicrawl.agent.runtime.vision_proxy.apply_model_selection", side_effect=select), patch(
            "omnicrawl.agent.runtime.vision_proxy.llm_config_to_profile_and_descriptor",
            side_effect=build_descriptor,
        ), patch(
            "omnicrawl.agent.runtime.vision_proxy.ModelRuntimeManager",
            _FakeManager,
        ), patch(
            "omnicrawl.agent.runtime.vision_proxy.AgentLLMProtocol",
            _FakeProtocol,
        ):
            result = self._proxy(refs).analyze(
                (
                    ToolImageAttachment(
                        "image/png",
                        "aW1hZ2U=",
                        "screen.png",
                    ),
                ),
                prompt="请判断设置窗口是否已打开。",
                on_token_usage=lambda input_count, output_count, cached_count: usage.append(
                    (input_count, output_count, cached_count)
                ),
            )

        self.assertEqual(result.text, "屏幕上有一个设置窗口。")
        self.assertEqual(result.model, "vision-profile/vision-second")
        self.assertEqual(selections, ["vision-first", "vision-profile/vision-second"])
        self.assertEqual(
            selected_protocols,
            ["openai_chat_completions", "openai_responses"],
        )
        self.assertEqual(usage, [(11, 7, 2), (11, 7, 2)])
        self.assertEqual(len(_FakeProtocol.requests), 2)
        self.assertEqual(_FakeProtocol.requests[1]["tools"], [])
        self.assertEqual(
            _FakeProtocol.requests[1]["messages"][0]["content"][0]["text"],
            "请判断设置窗口是否已打开。",
        )
        self.assertEqual(_FakeProtocol.requests[1]["system"], "")
        self.assertNotIn("windows_screenshot", _FakeProtocol.requests[1]["messages"][0]["content"][0]["text"])
        self.assertIn("data:image/png;base64,aW1hZ2U=", str(_FakeProtocol.requests[1]))
        self.assertTrue(all(item.released and item.closed for item in _FakeManager.instances))

    def test_all_models_failed_returns_explicit_error(self) -> None:
        refs = (
            ActiveModelRef(source="custom", key="vision-first"),
            ActiveModelRef(source="custom", key="vision-second"),
        )
        _FakeProtocol.responses = [RuntimeError("timeout"), ""]

        with patch(
            "omnicrawl.agent.runtime.vision_proxy.apply_model_selection",
            side_effect=lambda base, _token: base,
        ), patch(
            "omnicrawl.agent.runtime.vision_proxy.llm_config_to_profile_and_descriptor",
            return_value=(SimpleNamespace(), SimpleNamespace()),
        ), patch(
            "omnicrawl.agent.runtime.vision_proxy.ModelRuntimeManager",
            _FakeManager,
        ), patch(
            "omnicrawl.agent.runtime.vision_proxy.AgentLLMProtocol",
            _FakeProtocol,
        ):
            with self.assertRaisesRegex(VisionProxyError, "视觉模型全部调用失败"):
                self._proxy(refs).analyze(
                    (ToolImageAttachment("image/png", "aA=="),),
                    prompt="请读取图片中的文字。",
                )

    def test_cancellation_does_not_failover_to_next_model(self) -> None:
        refs = (
            ActiveModelRef(source="custom", key="vision-first"),
            ActiveModelRef(source="custom", key="vision-second"),
        )
        _FakeProtocol.responses = [_CancelledError("cancelled by parent turn")]

        with patch(
            "omnicrawl.agent.runtime.vision_proxy.apply_model_selection",
            side_effect=lambda base, _token: base,
        ), patch(
            "omnicrawl.agent.runtime.vision_proxy.llm_config_to_profile_and_descriptor",
            return_value=(SimpleNamespace(), SimpleNamespace()),
        ), patch(
            "omnicrawl.agent.runtime.vision_proxy.ModelRuntimeManager",
            _FakeManager,
        ), patch(
            "omnicrawl.agent.runtime.vision_proxy.AgentLLMProtocol",
            _FakeProtocol,
        ):
            with self.assertRaises(_CancelledError):
                self._proxy(refs).analyze(
                    (ToolImageAttachment("image/png", "aA=="),),
                    prompt="请读取图片中的文字。",
                )

        self.assertEqual(len(_FakeProtocol.requests), 1)

    def test_disabled_proxy_does_not_attempt_a_model(self) -> None:
        proxy = VisionModelProxy(
            base_llm=SimpleNamespace(),
            configuration=VisionConfiguration(enabled=False),
            workspace_root=Path.cwd(),
        )
        with self.assertRaisesRegex(VisionProxyError, "未启用"):
            proxy.analyze(
                (ToolImageAttachment("image/png", "aA=="),),
                prompt="请确认图片是否可读。",
            )


if __name__ == "__main__":
    unittest.main()
