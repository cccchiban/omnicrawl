from __future__ import annotations

import json
import tempfile
import unittest

import yaml
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import Mock, patch

from omnicrawl.llm import LLMConfig
from omnicrawl.llm.registry import DiscoveryModel, DiscoveryResult, ProviderProfile
from omnicrawl.model_catalog import (
    detect_model_options,
    detect_model_provider,
    ensure_current_model_option,
    clear_discovery_cache,
    ModelOption,
    save_llm_model,
)
from omnicrawl.config.model_catalog import _discover_for_profile
from omnicrawl.slash_commands import handle_model_command


class _FakeResponse:
    def __init__(self, payload: dict) -> None:
        self._body = json.dumps(payload).encode("utf-8")

    def __enter__(self) -> "_FakeResponse":
        return self

    def __exit__(self, *_args) -> None:
        return None

    def read(self, _limit: int = -1) -> bytes:
        return self._body


class _FakeAgent:
    def __init__(self) -> None:
        self.config = SimpleNamespace(
            llm=LLMConfig(
                api_key="test-key",
                base_url="https://example.test/v1",
                model="old-model",
            )
        )

    @property
    def current_model(self) -> str:
        return self.config.llm.model

    def set_model(self, model: str) -> None:
        self.config.llm.model = model


class ModelCatalogTest(unittest.TestCase):
    def test_detect_model_options_reads_openai_compatible_models_endpoint(self) -> None:
        captured = {}

        def fake_urlopen(request, timeout):
            captured["url"] = request.full_url
            captured["timeout"] = timeout
            captured["authorization"] = request.headers.get("Authorization")
            return _FakeResponse({"data": [{"id": "gpt-5.2"}, {"id": "deepseek-v4-flash"}]})

        config = LLMConfig(
            api_key="test-key",
            base_url="https://example.test/v1/",
            model="gpt-5.2",
        )

        with patch("omnicrawl.model_catalog.urllib.request.urlopen", side_effect=fake_urlopen):
            options = detect_model_options(config, timeout_seconds=3)

        self.assertEqual(captured["url"], "https://example.test/v1/models")
        self.assertEqual(captured["timeout"], 3)
        self.assertEqual(captured["authorization"], "Bearer test-key")
        self.assertEqual([option.id for option in options], ["gpt-5.2", "deepseek-v4-flash"])
        self.assertEqual(options[0].provider, "gpt")

    def test_should_retry_discovery_when_previous_attempt_was_unavailable(self) -> None:
        profile = ProviderProfile(
            id="migrated-openai",
            provider="openai",
            base_url="https://example.test/v1",
            api_key="test-key",
        )
        unavailable = DiscoveryResult(
            profile_id=profile.id,
            status="unavailable",
            message="temporary connection failure",
        )
        available = DiscoveryResult(
            profile_id=profile.id,
            status="ok",
            models=(
                DiscoveryModel(
                    profile_id=profile.id,
                    provider="openai",
                    protocol="openai_chat_completions",
                    model_id="deepseek-v4-flash",
                ),
            ),
        )
        adapter = SimpleNamespace(discover_models=Mock(side_effect=[unavailable, available]))
        clear_discovery_cache()

        with patch("omnicrawl.config.model_catalog.get_adapter", return_value=adapter):
            first = _discover_for_profile(profile, refresh=False, timeout_seconds=1)
            second = _discover_for_profile(profile, refresh=False, timeout_seconds=1)

        self.assertEqual(first.status, "unavailable")
        self.assertEqual(second.status, "ok")
        self.assertEqual(adapter.discover_models.call_count, 2)

    def test_ensure_current_model_option_prepends_missing_current_model(self) -> None:
        options = ensure_current_model_option([], "custom-model")

        self.assertEqual(options[0].id, "custom-model")
        self.assertEqual(options[0].provider, "other")

    def test_detect_model_provider_supports_common_families(self) -> None:
        self.assertEqual(detect_model_provider("deepseek-v4-flash"), "deepseek")
        self.assertEqual(detect_model_provider("qwen3.6-plus"), "qwen")
        self.assertEqual(detect_model_provider("glm-5.1"), "glm")

    def test_save_llm_model_preserves_existing_config(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            config_path = Path(temp_dir) / "config.yaml"
            config_path.write_text(
                json.dumps(
                    {
                        "llm": {"model": "old-model", "base_url": "https://example.test/v1"},
                        "voice": {"text_to_speech_enabled": False},
                    }
                ),
                encoding="utf-8",
            )

            save_llm_model("new-model", config_path)
            data = yaml.safe_load(config_path.read_text(encoding="utf-8"))

        self.assertEqual(data["llm"]["model"], "new-model")
        self.assertEqual(data["llm"]["base_url"], "https://example.test/v1")
        self.assertFalse(data["voice"]["text_to_speech_enabled"])

    def test_handle_model_command_switches_agent_and_persists(self) -> None:
        agent = _FakeAgent()

        with patch(
            "omnicrawl.slash_commands.detect_model_options",
            return_value=[ModelOption(id="new-model", name="new-model", provider="other")],
        ), patch(
            "omnicrawl.slash_commands.save_llm_model",
            return_value=Path("config.yaml"),
        ):
            message = handle_model_command(agent, "/model new-model")

        self.assertEqual(agent.current_model, "new-model")
        self.assertIn("当前模型已切换为 new-model", message or "")

    def test_handle_model_command_rejects_removed_models_alias(self) -> None:
        agent = _FakeAgent()

        self.assertIsNone(handle_model_command(agent, "/models"))

    def test_handle_model_command_rejects_models_outside_detected_base_url_list(self) -> None:
        agent = _FakeAgent()

        with patch(
            "omnicrawl.slash_commands.detect_model_options",
            return_value=ensure_current_model_option([], "allowed-model"),
        ):
            message = handle_model_command(agent, "/model blocked-model")

        self.assertEqual(agent.current_model, "old-model")
        self.assertIn("不在当前 base_url", message or "")


if __name__ == "__main__":
    unittest.main()
