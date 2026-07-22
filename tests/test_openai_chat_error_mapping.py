from __future__ import annotations

from types import SimpleNamespace
import unittest

from omnicrawl.llm.capabilities import ModelCapabilities
from omnicrawl.llm.errors import ModelError, ModelErrorCode
from omnicrawl.llm.protocol import (
    ConversationMessage,
    GenerationOptions,
    ModelIdentity,
    ModelTurnRequest,
    TextBlock,
)
from omnicrawl.llm.providers.openai_chat import OpenAIChatCompletionsRuntime
from omnicrawl.llm.registry import ModelDescriptor, ProviderProfile


class OpenAIChatErrorMappingTest(unittest.TestCase):
    def test_should_preserve_model_not_found_when_gateway_rejects_unknown_model(self) -> None:
        class InvalidModelGatewayError(RuntimeError):
            status_code = 422

            def __init__(self) -> None:
                super().__init__("Client error '422 Unprocessable Entity'")
                self.response = SimpleNamespace(
                    status_code=422,
                    json=lambda: {
                        "error": {
                            "message": "model not found: default",
                            "type": "invalid_model_error",
                        }
                    },
                )

        def reject_request(**_kwargs):
            raise InvalidModelGatewayError()

        identity = ModelIdentity(
            profile_id="openai",
            provider="openai",
            protocol="openai_chat_completions",
            model_id="default",
        )
        profile = ProviderProfile(
            id="openai",
            provider="openai",
            api_key="test-key",
            default_protocol="openai_chat_completions",
        )
        descriptor = ModelDescriptor(
            identity=identity,
            capabilities=ModelCapabilities(streaming=True, tools=True),
        )
        client = SimpleNamespace(
            chat=SimpleNamespace(
                completions=SimpleNamespace(create=reject_request),
            )
        )
        runtime = OpenAIChatCompletionsRuntime(
            identity=identity,
            capabilities=ModelCapabilities(streaming=True, tools=True),
            client=client,
            profile=profile,
            descriptor=descriptor,
            _owns_client=False,
        )
        request = ModelTurnRequest(
            identity=identity,
            system_prompt="system",
            messages=(ConversationMessage(role="user", blocks=(TextBlock("hello"),)),),
            generation_options=GenerationOptions(),
        )

        with self.assertRaises(ModelError) as caught:
            list(runtime.stream_turn(request))

        self.assertEqual(caught.exception.code, ModelErrorCode.MODEL_NOT_FOUND)
        self.assertEqual(caught.exception.status_code, 422)
        self.assertEqual(caught.exception.provider, "openai")


if __name__ == "__main__":
    unittest.main()
