from __future__ import annotations

import unittest

from ai_voice_agent.llm import LLMConfig, LLMError, OpenAIResponseLLM, normalize_reasoning_effort


class LLMConfigTest(unittest.TestCase):
    def test_reasoning_effort_supports_xhigh_and_common_depths(self) -> None:
        self.assertEqual(normalize_reasoning_effort("low"), "low")
        self.assertEqual(normalize_reasoning_effort("medium"), "medium")
        self.assertEqual(normalize_reasoning_effort("X-HIGH"), "xhigh")
        self.assertEqual(normalize_reasoning_effort("x_high"), "xhigh")

    def test_reasoning_effort_rejects_unknown_value(self) -> None:
        with self.assertRaisesRegex(LLMError, "reasoning_effort"):
            normalize_reasoning_effort("turbo")

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

    def test_format_request_error_normalizes_stream_disconnect(self) -> None:
        message = OpenAIResponseLLM.format_request_error(
            RuntimeError("peer closed connection without sending complete message body (incomplete chunked read)")
        )

        self.assertIn("模型服务流式连接提前断开", message)
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
