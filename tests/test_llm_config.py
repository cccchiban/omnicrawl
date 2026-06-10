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
            (12, 5),
        )
        self.assertEqual(
            OpenAIResponseLLM.extract_token_usage(
                {"usage": {"prompt_tokens": 7, "completion_tokens": 3}}
            ),
            (7, 3),
        )


if __name__ == "__main__":
    unittest.main()
