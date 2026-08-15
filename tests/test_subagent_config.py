from __future__ import annotations

import json
from omnicrawl.config.runtime import dump_toml_text
import os
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from omnicrawl.config.subagents import (
    SubAgentConfigError,
    load_subagent_config,
)


class SubAgentConfigTest(unittest.TestCase):
    def test_defaults_are_disabled_with_unlimited_turns_and_calls(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            config = load_subagent_config(Path(temp_dir) / "missing.toml")

        self.assertFalse(config.enabled)
        self.assertEqual(config.max_depth, 1)
        self.assertEqual(config.max_concurrency, 2)
        self.assertEqual(config.max_tasks_per_batch, 4)
        self.assertEqual(config.default_timeout_seconds, 3600.0)
        self.assertFalse(config.allow_background)
        self.assertFalse(config.allow_fork)
        self.assertFalse(config.allow_shared_workspace_writes)
        self.assertFalse(config.allow_worktree)
        self.assertFalse(config.allow_standard_agent)
        self.assertFalse(config.enable_verify_agent)
        self.assertEqual(config.verify_command_timeout_seconds, 120)

    def test_toml_loads_supported_subagent_values(self) -> None:
        payload = {
            "subagents": {
                "enabled": True,
                "max_depth": 1,
                "max_concurrency": 2,
                "max_tasks_per_batch": 4,
                "default_timeout_seconds": 90,
                "model_request_concurrency": 2,
                "result_summary_chars": 2400,
            }
        }
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            subagents_path = root / "subagents.toml"
            subagents_path.write_text(
                "[subagents]\nenabled = true\nmax_depth = 1\nmax_concurrency = 2\nmax_tasks_per_batch = 4\ndefault_timeout_seconds = 90\nmodel_request_concurrency = 2\nresult_summary_chars = 2400\n",
                encoding="utf-8",
            )

            yaml_config = load_subagent_config(subagents_path)

        self.assertTrue(yaml_config.enabled)
        self.assertEqual(yaml_config.default_timeout_seconds, 90.0)

    def test_legacy_execution_budget_settings_are_ignored(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            path = Path(temp_dir) / "subagents.toml"
            path.write_text(
                dump_toml_text(
                    {
                        "subagents": {
                            "max_total_tasks": 8,
                            "default_max_turns": 12,
                            "default_max_tool_calls": 24,
                        }
                    }
                ),
                encoding="utf-8",
            )
            config = load_subagent_config(path)

        self.assertEqual(config.default_timeout_seconds, 3600.0)
        self.assertFalse(hasattr(config, "max_total_tasks"))
        self.assertFalse(hasattr(config, "default_max_turns"))
        self.assertFalse(hasattr(config, "default_max_tool_calls"))

    def test_environment_can_only_disable_or_tighten(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            path = Path(temp_dir) / "subagents.toml"
            path.write_text(
                dump_toml_text(
                    {
                        "subagents": {
                            "enabled": True,
                            "max_concurrency": 4,
                            "default_timeout_seconds": 300,
                        }
                    }
                ),
                encoding="utf-8",
            )
            env = {
                "OMNICRAWL_SUBAGENTS_ENABLED": "false",
                "OMNICRAWL_SUBAGENT_MAX_CONCURRENCY": "2",
                "OMNICRAWL_SUBAGENT_TIMEOUT_SECONDS": "120",
            }
            with patch.dict(os.environ, env, clear=False):
                config = load_subagent_config(path)

            self.assertFalse(config.enabled)
            self.assertEqual(config.max_concurrency, 2)
            self.assertEqual(config.default_timeout_seconds, 120.0)

            path.write_text(
                dump_toml_text(
                    {
                        "subagents": {
                            "enabled": False,
                            "max_concurrency": 1,
                            "default_timeout_seconds": 30,
                        }
                    }
                ),
                encoding="utf-8",
            )
            widening_env = {
                "OMNICRAWL_SUBAGENTS_ENABLED": "true",
                "OMNICRAWL_SUBAGENT_MAX_CONCURRENCY": "4",
                "OMNICRAWL_SUBAGENT_TIMEOUT_SECONDS": "300",
            }
            with patch.dict(os.environ, widening_env, clear=False):
                tightened = load_subagent_config(path)

        self.assertFalse(tightened.enabled)
        self.assertEqual(tightened.max_concurrency, 1)
        self.assertEqual(tightened.default_timeout_seconds, 30.0)

    def test_background_requires_explicit_config_and_is_not_widened_by_environment(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            path = Path(temp_dir) / "subagents.toml"
            path.write_text(dump_toml_text({"subagents": {"allow_background": True}}), encoding="utf-8")
            with patch.dict(os.environ, {"OMNICRAWL_SUBAGENTS_ENABLED": "false"}, clear=False):
                config = load_subagent_config(path)
        self.assertTrue(config.allow_background)
        self.assertFalse(config.enabled)

    def test_current_phase_allows_explicit_fork_but_rejects_recursive_configuration(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            path = Path(temp_dir) / "subagents.toml"
            path.write_text(
                dump_toml_text({"subagents": {"allow_fork": True}}),
                encoding="utf-8",
            )
            self.assertTrue(load_subagent_config(path).allow_fork)

        # max_depth 仍限制为 1；共享写入/worktree/standard 允许显式开启。
        with tempfile.TemporaryDirectory() as temp_dir:
            path = Path(temp_dir) / "subagents.toml"
            path.write_text(
                dump_toml_text({"subagents": {"max_depth": 2}}),
                encoding="utf-8",
            )
            with self.assertRaisesRegex(SubAgentConfigError, "max_depth"):
                load_subagent_config(path)

        with tempfile.TemporaryDirectory() as temp_dir:
            path = Path(temp_dir) / "subagents.toml"
            path.write_text(
                dump_toml_text(
                    {
                        "subagents": {
                            "allow_shared_workspace_writes": True,
                            "allow_worktree": True,
                            "allow_standard_agent": True,
                        }
                    }
                ),
                encoding="utf-8",
            )
            config = load_subagent_config(path)
            self.assertTrue(config.allow_shared_workspace_writes)
            self.assertTrue(config.allow_worktree)
            self.assertTrue(config.allow_standard_agent)

    def test_model_overrides_parsed_from_subagents_toml(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            path = Path(temp_dir) / "subagents.toml"
            path.write_text(
                dump_toml_text(
                    {
                        "subagents": {
                            "enabled": True,
                            "models": {
                                "explore": {"model": "default-chat"},
                                "plan": {"model": "inherit"},
                                "verify": {"model": "claude-sonnet"},
                            },
                        }
                    }
                ),
                encoding="utf-8",
            )
            config = load_subagent_config(subagents_path=path)

        self.assertTrue(config.enabled)
        self.assertEqual(
            config.model_overrides,
            {"explore": "default-chat", "verify": "claude-sonnet"},
        )
        # inherit 和空值不进入覆盖表，避免与任务级 inherit 语义混淆。
        self.assertNotIn("plan", config.model_overrides)

    def test_explicit_subagents_path_reads_only_that_file(self) -> None:
        # 显式传入 subagents_path 时只从该文件读取 subagents 段，
        # 不被默认位置的 subagents.toml 干扰。
        with tempfile.TemporaryDirectory() as temp_dir:
            subagents_file = Path(temp_dir) / "subagents.toml"
            subagents_file.write_text(
                dump_toml_text({"subagents": {"enabled": True}}),
                encoding="utf-8",
            )
            config = load_subagent_config(subagents_file)
            self.assertTrue(config.enabled)
            self.assertEqual(config.model_overrides, {})

    def test_model_overrides_require_object_entries(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            path = Path(temp_dir) / "subagents.toml"
            path.write_text(
                dump_toml_text(
                    {"subagents": {"models": {"explore": "default-chat"}}}
                ),
                encoding="utf-8",
            )
            with self.assertRaisesRegex(SubAgentConfigError, "models.explore"):
                load_subagent_config(subagents_path=path)


if __name__ == "__main__":
    unittest.main()
