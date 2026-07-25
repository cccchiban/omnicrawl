from __future__ import annotations

import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

import yaml

from omnicrawl.config.bootstrap import StartupCheck, initialize_user_configuration, user_config_dir
from omnicrawl.config.channels import (
    ChannelConfig,
    ChannelConfiguration,
    save_channel_configuration,
)


class UserConfigDirectoryTest(unittest.TestCase):
    def test_windows_config_directory_uses_appdata(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            self.assertEqual(
                user_config_dir({"APPDATA": str(root)}, platform_name="win32"),
                root / "OmniCrawl",
            )

    def test_unix_config_directory_uses_xdg_config_home(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            self.assertEqual(
                user_config_dir({"XDG_CONFIG_HOME": str(root)}, platform_name="linux"),
                root / "omnicrawl",
            )


class FirstRunConfigurationTest(unittest.TestCase):
    def test_initialize_creates_templates_and_saves_api_key(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            result = initialize_user_configuration(
                Path(temp_dir),
                prompt=lambda _message: "secret-key",
            )

            self.assertTrue(result.config_created)
            self.assertTrue(result.models_created)
            self.assertTrue(result.api_key_configured)
            self.assertTrue(result.api_key_prompted)
            self.assertTrue(result.config_path.is_file())
            self.assertTrue(result.models_path.is_file())

            config = yaml.safe_load(result.config_path.read_text(encoding="utf-8"))
            self.assertEqual(
                config["llm"]["profiles"]["openai-main"]["api_key"],
                "secret-key",
            )

    def test_missing_api_key_is_reported_without_writing_empty_value(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            result = initialize_user_configuration(
                Path(temp_dir),
                prompt=lambda _message: "",
            )

            self.assertFalse(result.api_key_configured)
            self.assertTrue(result.api_key_prompted)
            config = yaml.safe_load(result.config_path.read_text(encoding="utf-8"))
            self.assertNotIn("api_key", config["llm"]["profiles"]["openai-main"])

    def test_existing_config_and_key_are_not_overwritten(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            config_path = Path(temp_dir) / "config.yaml"
            config_path.write_text(
                yaml.safe_dump(
                    {
                        "version": 2,
                        "llm": {
                            "profiles": {
                                "openai-main": {
                                    "provider": "openai",
                                    "api_key_env": "OPENAI_API_KEY",
                                    "api_key": "existing-key",
                                }
                            },
                            "active_model": {"source": "custom", "key": "default-chat"},
                        },
                    },
                    sort_keys=False,
                ),
                encoding="utf-8",
            )

            result = initialize_user_configuration(
                Path(temp_dir),
                prompt=lambda _message: self.fail("existing key should not prompt"),
            )

            self.assertFalse(result.config_created)
            self.assertTrue(result.api_key_configured)
            config = yaml.safe_load(config_path.read_text(encoding="utf-8"))
            self.assertEqual(config["llm"]["profiles"]["openai-main"]["api_key"], "existing-key")

    def test_first_run_uses_channel_setup_and_reloads_saved_key(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            calls: list[tuple[Path, Path]] = []

            def channel_setup(config_path: Path, models_path: Path) -> bool:
                calls.append((config_path, models_path))
                save_channel_configuration(
                    ChannelConfiguration(
                        channels=(
                            ChannelConfig(
                                key="anthropic-main",
                                name="Anthropic 主渠道",
                                profile_id="anthropic-main",
                                provider="anthropic",
                                protocol="anthropic_messages",
                                base_url="https://api.anthropic.com",
                                api_key="anthropic-secret",
                                model_id="claude-sonnet-4-5",
                            ),
                        ),
                        default_key="anthropic-main",
                    ),
                    config_path,
                    models_path,
                )
                return True

            result = initialize_user_configuration(root, channel_setup=channel_setup)

            self.assertEqual(calls, [(result.config_path, result.models_path)])
            self.assertTrue(result.api_key_prompted)
            self.assertTrue(result.api_key_configured)
            config = yaml.safe_load(result.config_path.read_text(encoding="utf-8"))
            self.assertEqual(config["llm"]["active_model"]["key"], "anthropic-main")

    def test_plugin_check_is_non_blocking_when_node_is_missing(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            with patch(
                "omnicrawl.config.bootstrap._check_node",
                return_value=StartupCheck("Node.js", "warning", "missing"),
            ):
                result = initialize_user_configuration(
                    Path(temp_dir),
                    prompt=lambda _message: "secret-key",
                )

            self.assertTrue(result.api_key_configured)
            self.assertTrue(any(check.name == "Node.js" for check in result.checks))


if __name__ == "__main__":
    unittest.main()
