from __future__ import annotations

import json
import tempfile
import unittest
from types import SimpleNamespace

import yaml
from pathlib import Path
from unittest.mock import patch

from omnicrawl.agent.core import LocalToolAgent
from omnicrawl.config.model_store import CustomModelRecord, ModelStore
from omnicrawl.llm import (
    LLMConfig,
    LLMError,
    OpenAIResponseLLM,
    normalize_reasoning_effort,
    save_reasoning_effort,
)
from omnicrawl.llm.providers.openai_common import create_openai_client
from omnicrawl.llm.registry import ProviderProfile


class LLMConfigTest(unittest.TestCase):
    def test_should_disable_system_proxy_when_creating_openai_client(self) -> None:
        profile = ProviderProfile(
            id="openai",
            provider="openai",
            api_key="test-key",
            base_url="https://example.test/v1",
        )

        with patch("httpx.Client") as http_client, patch("openai.OpenAI") as openai_client:
            result = create_openai_client(profile)

        http_client.assert_called_once_with(trust_env=False, follow_redirects=True)
        openai_client.assert_called_once_with(
            api_key="test-key",
            base_url="https://example.test/v1",
            http_client=http_client.return_value,
        )
        self.assertIs(result, openai_client.return_value)

    def test_should_disable_system_proxy_in_legacy_responses_client(self) -> None:
        config = LLMConfig(
            api_key="test-key",
            base_url="https://example.test/v1",
            model="demo-model",
        )

        with patch("httpx.Client") as http_client, patch("openai.OpenAI") as openai_client:
            result = OpenAIResponseLLM(config)

        http_client.assert_called_once_with(trust_env=False, follow_redirects=True)
        openai_client.assert_called_once_with(
            api_key="test-key",
            base_url="https://example.test/v1",
            http_client=http_client.return_value,
        )
        self.assertIs(result._client, openai_client.return_value)

    def test_should_disable_system_proxy_in_legacy_agent_client(self) -> None:
        agent = object.__new__(LocalToolAgent)
        agent.config = SimpleNamespace(
            llm=SimpleNamespace(
                api_key="test-key",
                base_url="https://example.test/v1",
            )
        )

        with patch("httpx.Client") as http_client, patch("openai.OpenAI") as openai_client:
            result = agent._llm_client()

        http_client.assert_called_once_with(trust_env=False, follow_redirects=True)
        openai_client.assert_called_once_with(
            api_key="test-key",
            base_url="https://example.test/v1",
            http_client=http_client.return_value,
        )
        self.assertIs(result, openai_client.return_value)

    def test_reasoning_effort_supports_xhigh_and_common_depths(self) -> None:
        self.assertEqual(normalize_reasoning_effort("low"), "low")
        self.assertEqual(normalize_reasoning_effort("medium"), "medium")
        self.assertEqual(normalize_reasoning_effort("X-HIGH"), "xhigh")
        self.assertEqual(normalize_reasoning_effort("x_high"), "xhigh")

    def test_reasoning_effort_rejects_unknown_value(self) -> None:
        with self.assertRaisesRegex(LLMError, "reasoning_effort"):
            normalize_reasoning_effort("turbo")

    def test_load_llm_config_reads_context_window_tokens(self) -> None:
        from omnicrawl.config.llm import load_llm_config

        with patch(
            "omnicrawl.config.llm.load_config_data",
            return_value={
                "llm": {
                    "api_key": "test-key",
                    "base_url": "https://example.test/v1",
                    "model": "demo-model",
                    "context_window_tokens": 200_000,
                }
            },
        ):
            config = load_llm_config()

        self.assertEqual(config.context_window_tokens, 200_000)

    def test_should_prefer_saved_default_over_profile_default_for_detected_model(self) -> None:
        from omnicrawl.config.llm_multi import load_multi_model_llm_config

        with patch(
            "omnicrawl.config.llm_multi.load_model_store",
            return_value=ModelStore(version=1, models=()),
        ):
            config = load_multi_model_llm_config(
                {
                    "defaults": {"context_window_tokens": 256_000},
                    "profiles": {
                        "openai": {
                            "provider": "openai",
                            "api_key": "test-key",
                            "default_context_window_tokens": 128_000,
                        }
                    },
                    "active_model": {
                        "source": "detected",
                        "profile": "openai",
                        "model_id": "demo-model",
                    },
                }
            )

        self.assertEqual(config.context_window_tokens, 256_000)

    def test_should_keep_explicit_custom_context_when_defaults_exist(self) -> None:
        from omnicrawl.config.llm_multi import load_multi_model_llm_config

        custom = CustomModelRecord(
            key="demo",
            display_name="Demo",
            profile="openai",
            model_id="demo-model",
            protocol="openai_chat_completions",
            context_window_tokens=128_000,
        )
        with patch(
            "omnicrawl.config.llm_multi.load_model_store",
            return_value=ModelStore(version=1, models=(custom,)),
        ):
            config = load_multi_model_llm_config(
                {
                    "defaults": {"context_window_tokens": 256_000},
                    "profiles": {
                        "openai": {
                            "provider": "openai",
                            "api_key": "test-key",
                            "default_context_window_tokens": 64_000,
                        }
                    },
                    "active_model": {"source": "custom", "key": "demo"},
                }
            )

        self.assertEqual(config.context_window_tokens, 128_000)

    def test_context_window_tokens_must_be_positive_integer(self) -> None:
        with self.assertRaisesRegex(LLMError, "context_window_tokens"):
            LLMConfig(
                api_key="test-key",
                base_url="https://example.test/v1",
                model="demo-model",
                context_window_tokens=0,
            )

    def test_extra_body_sends_xhigh_reasoning_effort(self) -> None:
        config = LLMConfig(
            api_key="test-key",
            base_url="https://example.test/v1",
            model="demo-model",
            thinking_type="disabled",
            reasoning_effort="xhigh",
        )
        llm = object.__new__(OpenAIResponseLLM)
        llm.config = config

        body = OpenAIResponseLLM._build_extra_body(llm)

        self.assertEqual(body["thinking"]["type"], "enabled")
        self.assertEqual(body["reasoning_effort"], "xhigh")

    def test_reasoning_effort_none_disables_thinking_even_when_type_enabled(self) -> None:
        config = LLMConfig(
            api_key="test-key",
            base_url="https://example.test/v1",
            model="demo-model",
            thinking_type="enabled",
            reasoning_effort="none",
        )
        llm = object.__new__(OpenAIResponseLLM)
        llm.config = config

        body = OpenAIResponseLLM._build_extra_body(llm)

        self.assertEqual(body, {"thinking": {"type": "disabled"}})

    def test_save_reasoning_effort_preserves_config_and_syncs_thinking_type(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            config_path = Path(temp_dir) / "config.yaml"
            config_path.write_text(
                json.dumps({"llm": {"model": "demo"}, "agent_temp": {"enabled": True}}),
                encoding="utf-8",
            )

            save_reasoning_effort("high", config_path)
            data = yaml.safe_load(config_path.read_text(encoding="utf-8"))

        self.assertEqual(data["llm"]["model"], "demo")
        self.assertEqual(data["llm"]["reasoning_effort"], "high")
        self.assertEqual(data["llm"]["thinking_type"], "enabled")
        self.assertTrue(data["agent_temp"]["enabled"])

    def test_extract_token_usage_supports_stream_event_shapes(self) -> None:
        self.assertEqual(
            OpenAIResponseLLM.extract_token_usage(
                {"response": {"usage": {"input_tokens": 12, "output_tokens": 5}}}
            ),
            (12, 5, 0),
        )
        self.assertEqual(
            OpenAIResponseLLM.extract_token_usage(
                {
                    "usage": {
                        "prompt_tokens": 7,
                        "completion_tokens": 3,
                        "prompt_tokens_details": {"cached_tokens": 4},
                    }
                }
            ),
            (7, 3, 4),
        )
        self.assertEqual(
            OpenAIResponseLLM.extract_token_usage(
                {
                    "usage": {
                        "completion_tokens": 2,
                        "prompt_cache_hit_tokens": 9,
                        "prompt_cache_miss_tokens": 5,
                    }
                }
            ),
            (14, 2, 9),
        )

    def test_context_length_gateway_error_is_classified_without_exposing_raw_body(self) -> None:
        class ContextLengthGatewayError(RuntimeError):
            status_code = 422

            def __init__(self) -> None:
                super().__init__("422 Unprocessable Entity")
                self.response = SimpleNamespace(
                    status_code=422,
                    json=lambda: {
                        "error": {
                            "message": "maximum context length is 8192 tokens; received 9000",
                            "type": "context_length_exceeded",
                        }
                    },
                )

        from omnicrawl.llm.errors import ModelErrorCode, map_openai_exception

        mapped = map_openai_exception(ContextLengthGatewayError())

        self.assertEqual(mapped.code, ModelErrorCode.CONTEXT_LENGTH_EXCEEDED)
        self.assertEqual(mapped.status_code, 422)
        self.assertIn("输入上下文超过", mapped.message)
        self.assertNotIn("8192", mapped.message)
        self.assertNotIn("9000", mapped.message)

    def test_should_classify_rate_limit_before_token_markers(self) -> None:
        class RateLimitError(RuntimeError):
            status_code = 429

        from omnicrawl.llm.errors import ModelErrorCode, map_openai_exception

        mapped = map_openai_exception(
            RateLimitError("Rate limit reached: token limit exceeded for this minute")
        )

        self.assertEqual(mapped.code, ModelErrorCode.RATE_LIMITED)

    def test_format_request_error_hides_html_gateway_body(self) -> None:
        raw_error = RuntimeError(
            "<html>\n"
            "<head><title>504 Gateway Time-out</title></head>\n"
            "<body><center><h1>504 Gateway Time-out</h1></center><hr><center>openresty</center></body>\n"
            "</html>"
        )

        message = OpenAIResponseLLM.format_request_error(raw_error)

        self.assertIn("HTTP 504", message)
        self.assertIn("模型服务网关暂时不可用", message)
        self.assertNotIn("<html>", message)
        self.assertNotIn("openresty", message)

    def test_format_request_error_normalizes_connection_disconnect(self) -> None:
        message = OpenAIResponseLLM.format_request_error(
            RuntimeError("peer closed connection without sending complete message body (incomplete chunked read)")
        )

        self.assertIn("模型服务连接提前断开", message)
        self.assertNotIn("peer closed connection", message)
        self.assertNotIn("incomplete chunked read", message)

    def test_format_request_error_normalizes_timeout(self) -> None:
        message = OpenAIResponseLLM.format_request_error(RuntimeError("ReadTimeout: request timed out"))

        self.assertIn("模型服务请求超时", message)
        self.assertIn("AGENT_REQUEST_TIMEOUT_SECONDS", message)

    def test_format_request_error_uses_status_code_attribute(self) -> None:
        class FakeHTTPError(Exception):
            status_code = 429

        message = OpenAIResponseLLM.format_request_error(FakeHTTPError("raw rate limit body"))

        self.assertIn("HTTP 429", message)
        self.assertIn("限流", message)
        self.assertNotIn("raw rate limit body", message)


if __name__ == "__main__":
    unittest.main()
