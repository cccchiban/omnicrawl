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

    def test_build_catalog_without_detected_skips_network_discovery(self) -> None:
        """``include_detected=False`` 只返回本地数据，不触发网络发现。

        两阶段加载的第一步（渠道列）依赖该行为：在模型网络发现完成
        前即可安全地展示本地渠道，且发现函数绝不能被调用。
        """

        from omnicrawl.config.model_catalog import build_catalog

        discovered = Mock(side_effect=AssertionError("不应触发网络发现"))
        with (
            patch("omnicrawl.config.model_catalog._discover_for_profile", discovered),
            patch(
                "omnicrawl.config.model_catalog.load_model_store",
                return_value=SimpleNamespace(models=[]),
            ),
        ):
            catalog = build_catalog(
                config=_FakeAgent().config.llm,
                include_detected=False,
                config_data={"llm": {}},
            )

        self.assertEqual(catalog["custom"], [])
        self.assertEqual(catalog["detected"], [])
        self.assertEqual(catalog["diagnostics"], [])
        discovered.assert_not_called()

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

if __name__ == "__main__":
    unittest.main()
