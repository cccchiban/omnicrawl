from __future__ import annotations

import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

try:
    import tomllib
except ImportError:  # Python < 3.11
    import tomli as tomllib
from omnicrawl.config.runtime import dump_toml_text

from omnicrawl.config.bootstrap import StartupCheck, initialize_user_configuration, user_config_dir
from omnicrawl.config.channels import (
    ChannelConfig,
    ChannelConfiguration,
    save_channel_configuration,
)


class UserConfigDirectoryTest(unittest.TestCase):
    def test_windows_config_directory_uses_hidden_home_directory(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            home = Path(temp_dir)
            with patch("omnicrawl.config.runtime.Path.home", return_value=home):
                self.assertEqual(
                    user_config_dir({"APPDATA": str(home / "appdata")}, platform_name="win32"),
                    home / ".OmniCrawl",
                )

    def test_unix_config_directory_uses_same_hidden_home_directory(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            home = Path(temp_dir)
            with patch("omnicrawl.config.runtime.Path.home", return_value=home):
                self.assertEqual(
                    user_config_dir({"XDG_CONFIG_HOME": str(home / "config")}, platform_name="linux"),
                    home / ".OmniCrawl",
                )


class FirstRunConfigurationTest(unittest.TestCase):
    def test_normal_initialization_migrates_legacy_config_first(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            config_path = root / "config.toml"
            models_path = root / "models.toml"
            subagents_path = root / "subagents.toml"
            with patch("omnicrawl.config.bootstrap.migrate_legacy_user_config") as migrate:
                with patch(
                    "omnicrawl.config.bootstrap.resolve_config_write_path",
                    return_value=config_path,
                ):
                    with patch(
                        "omnicrawl.config.bootstrap.resolve_models_write_path",
                        return_value=models_path,
                    ):
                        with patch(
                            "omnicrawl.config.bootstrap.resolve_subagents_write_path",
                            return_value=subagents_path,
                        ):
                            result = initialize_user_configuration(
                                prompt=lambda _message: "secret-key",
                            )

            migrate.assert_called_once_with()
            self.assertTrue(result.config_path.is_file())
            self.assertTrue(result.models_path.is_file())
            self.assertTrue(result.subagents_path.is_file())

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

            config = tomllib.loads(result.config_path.read_text(encoding="utf-8"))
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
            config = tomllib.loads(result.config_path.read_text(encoding="utf-8"))
            self.assertNotIn("api_key", config["llm"]["profiles"]["openai-main"])

    def test_existing_config_and_key_are_not_overwritten(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            config_path = Path(temp_dir) / "config.toml"
            config_path.write_text(
                dump_toml_text(
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
                ),
                encoding="utf-8",
            )

            result = initialize_user_configuration(
                Path(temp_dir),
                prompt=lambda _message: self.fail("existing key should not prompt"),
            )

            self.assertFalse(result.config_created)
            self.assertTrue(result.api_key_configured)
            config = tomllib.loads(config_path.read_text(encoding="utf-8"))
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
            config = tomllib.loads(result.config_path.read_text(encoding="utf-8"))
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
