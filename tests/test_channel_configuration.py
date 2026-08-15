from __future__ import annotations

import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

try:
    import tomllib
except ImportError:  # Python < 3.11
    import tomli as tomllib

from omnicrawl.config.llm import LLMConfig, LLMError
from omnicrawl.config.llm_multi import apply_model_selection
from omnicrawl.config.model_store import CustomModelRecord, ModelStore
from omnicrawl.llm.registry import ProviderProfile

import omnicrawl.config.runtime as runtime_module

from omnicrawl.config.channels import (
    ChannelConfig,
    ChannelConfiguration,
    load_channel_configuration,
    save_channel_configuration,
)


class ChannelConfigurationTests(unittest.TestCase):
    def test_should_read_project_config_but_write_default_to_user_directory(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            project_dir = root / "project"
            user_dir = root / "user"
            project_dir.mkdir()
            user_dir.mkdir()
            project_config = project_dir / "config.toml"
            project_models = project_dir / "models.toml"
            project_config.write_text("version = 2\n\n[llm]\n", encoding="utf-8")
            project_models.write_text("version = 1\n\n[models]\n", encoding="utf-8")
            channels = ChannelConfiguration(
                channels=(
                    ChannelConfig(
                        key="demo",
                        name="Demo",
                        profile_id="demo",
                        provider="openai",
                        protocol="openai_chat_completions",
                        base_url="https://api.example/v1",
                        api_key="secret",
                        model_id="demo-model",
                    ),
                ),
                default_key="demo",
            )

            with patch("omnicrawl.config.runtime.user_config_dir", return_value=user_dir):
                with patch("omnicrawl.config.runtime.Path.cwd", return_value=project_dir):
                    with patch.object(runtime_module, "_is_development_environment", return_value=True):
                        written_config, written_models = save_channel_configuration(channels)

            self.assertEqual(written_config, user_dir / "config.toml")
            self.assertEqual(written_models, user_dir / "models.toml")
            self.assertEqual(
                tomllib.loads((project_dir / "config.toml").read_text(encoding="utf-8")),
                {"version": 2, "llm": {}},
            )
            self.assertEqual(
                tomllib.loads((user_dir / "config.toml").read_text(encoding="utf-8"))["llm"]["active_model"]["key"],
                "demo",
            )

    def test_should_persist_multiple_channels_when_protocol_is_shared(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            config_path = root / "config.toml"
            models_path = root / "models.toml"
            config_path.write_text("version = 2\n\n[llm]\n", encoding="utf-8")
            models_path.write_text("version = 1\n\n[models]\n", encoding="utf-8")

            channels = ChannelConfiguration(
                channels=(
                    ChannelConfig(
                        key="openai-official",
                        name="OpenAI 官方",
                        profile_id="openai-official",
                        provider="openai",
                        protocol="openai_chat_completions",
                        base_url="https://api.openai.com/v1",
                        api_key="official-key",
                        model_id="gpt-5.2",
                    ),
                    ChannelConfig(
                        key="openai-proxy",
                        name="OpenAI 代理",
                        profile_id="openai-proxy",
                        provider="openai",
                        protocol="openai_chat_completions",
                        base_url="https://proxy.example/v1",
                        api_key="proxy-key",
                        model_id="gpt-5.2",
                    ),
                ),
                default_key="openai-proxy",
            )

            save_channel_configuration(channels, config_path, models_path)

            config = tomllib.loads(config_path.read_text(encoding="utf-8"))
            models = tomllib.loads(models_path.read_text(encoding="utf-8"))
            self.assertEqual(
                set(config["llm"]["profiles"]),
                {"openai-official", "openai-proxy"},
            )
            self.assertEqual(
                config["llm"]["profiles"]["openai-proxy"]["api_key"],
                "proxy-key",
            )
            self.assertEqual(config["llm"]["active_model"]["key"], "openai-proxy")
            self.assertNotIn("api_key", models["models"]["openai-official"])
            self.assertNotIn("api_key", models["models"]["openai-proxy"])

            loaded = load_channel_configuration(config_path, models_path)
            self.assertEqual(loaded.default_key, "openai-proxy")
            self.assertEqual(
                [item.name for item in loaded.channels],
                ["OpenAI 官方", "OpenAI 代理"],
            )

    def test_should_persist_user_agent_in_profile_only(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            config_path = root / "config.toml"
            models_path = root / "models.toml"
            config_path.write_text("version = 2\n\n[llm]\n", encoding="utf-8")
            models_path.write_text("version = 1\n\n[models]\n", encoding="utf-8")
            channel = ChannelConfig(
                key="proxy",
                name="代理渠道",
                profile_id="proxy",
                provider="openai",
                protocol="openai_chat_completions",
                base_url="https://proxy.example/v1",
                api_key="secret",
                user_agent="OmniCrawl-Test/1.0",
                model_id="demo-model",
            )

            save_channel_configuration(
                ChannelConfiguration((channel,), "proxy"), config_path, models_path
            )

            config = tomllib.loads(config_path.read_text(encoding="utf-8"))
            models = tomllib.loads(models_path.read_text(encoding="utf-8"))
            self.assertEqual(
                config["llm"]["profiles"]["proxy"]["user_agent"],
                "OmniCrawl-Test/1.0",
            )
            self.assertNotIn("user_agent", models["models"]["proxy"])
            self.assertEqual(
                load_channel_configuration(config_path, models_path).channels[0].user_agent,
                "OmniCrawl-Test/1.0",
            )

    def test_should_reject_user_agent_with_newline(self) -> None:
        channel = ChannelConfig(
            key="proxy",
            name="代理渠道",
            profile_id="proxy",
            provider="openai",
            protocol="openai_chat_completions",
            base_url="https://proxy.example/v1",
            api_key="secret",
            user_agent="bad\nvalue",
            model_id="demo-model",
        )
        with self.assertRaisesRegex(Exception, "User-Agent"):
            from omnicrawl.config.channels import _validate_channels

            _validate_channels((channel,))

        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            config_path = root / "config.toml"
            models_path = root / "models.toml"
            original_config = "version = 2\n\n[llm.profiles]\n"
            config_path.write_text(original_config, encoding="utf-8")
            models_path.write_text("version = 1\n\n[models]\n", encoding="utf-8")
            channels = ChannelConfiguration(
                channels=(
                    ChannelConfig(
                        key="openai-main",
                        name="OpenAI",
                        profile_id="openai-main",
                        provider="openai",
                        protocol="openai_chat_completions",
                        base_url="https://api.openai.com/v1",
                        api_key="secret",
                        model_id="gpt-5.2",
                    ),
                ),
                default_key="openai-main",
            )

            from omnicrawl.config import channels as channel_module

            real_atomic_write = channel_module.atomic_write_text

            def fail_models(path: Path, text: str) -> None:
                if Path(path) == models_path:
                    raise OSError("models write failed")
                real_atomic_write(Path(path), text)

            with patch.object(
                channel_module,
                "atomic_write_text",
                side_effect=fail_models,
            ):
                with self.assertRaisesRegex(Exception, "models write failed"):
                    save_channel_configuration(channels, config_path, models_path)

            self.assertEqual(config_path.read_text(encoding="utf-8"), original_config)

    def test_should_not_reuse_previous_key_when_selected_profile_has_no_key(self) -> None:
        config = LLMConfig(
            api_key="old-provider-secret",
            base_url="https://old-provider.example/v1",
            model="old-model",
        )
        record = CustomModelRecord(
            key="new-channel",
            display_name="New Channel",
            profile="new-channel",
            model_id="new-model",
            protocol="openai_chat_completions",
        )
        store = ModelStore(version=1, models=(record,))
        profile = ProviderProfile(
            id="new-channel",
            provider="openai",
            base_url="https://new-provider.example/v1",
            api_key="",
            api_key_env="MISSING_NEW_CHANNEL_KEY",
            default_protocol="openai_chat_completions",
        )

        with patch("omnicrawl.config.llm_multi.load_model_store", return_value=store):
            with patch(
                "omnicrawl.config.llm_multi._profiles_from_disk",
                return_value={"new-channel": profile},
            ):
                with patch.dict("os.environ", {}, clear=True):
                    with self.assertRaisesRegex(LLMError, "缺少 API Key"):
                        apply_model_selection(config, "new-channel")

    def test_should_choose_enabled_default_when_requested_default_is_disabled(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            config_path = root / "config.toml"
            models_path = root / "models.toml"
            config_path.write_text("version = 2\n\n[llm]\n", encoding="utf-8")
            models_path.write_text("version = 1\n\n[models]\n", encoding="utf-8")
            channels = ChannelConfiguration(
                channels=(
                    ChannelConfig(
                        key="disabled",
                        name="停用渠道",
                        profile_id="disabled",
                        provider="anthropic",
                        protocol="anthropic_messages",
                        base_url="https://api.anthropic.com",
                        api_key="disabled-key",
                        model_id="claude-sonnet-4-5",
                        enabled=False,
                    ),
                    ChannelConfig(
                        key="gemini-main",
                        name="Gemini",
                        profile_id="gemini-main",
                        provider="gemini",
                        protocol="gemini_generate_content",
                        base_url="https://generativelanguage.googleapis.com",
                        api_key="gemini-key",
                        model_id="gemini-2.5-pro",
                    ),
                ),
                default_key="disabled",
            )

            save_channel_configuration(channels, config_path, models_path)
            loaded = load_channel_configuration(config_path, models_path)

            self.assertEqual(loaded.default_key, "gemini-main")


if __name__ == "__main__":
    unittest.main()
