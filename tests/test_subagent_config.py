from __future__ import annotations

import json
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
    def test_defaults_are_disabled_and_keep_design_limits(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            config = load_subagent_config(Path(temp_dir) / "missing.yaml")

        self.assertFalse(config.enabled)
        self.assertEqual(config.max_depth, 1)
        self.assertEqual(config.max_concurrency, 2)
        self.assertEqual(config.max_tasks_per_batch, 4)
        self.assertFalse(config.allow_background)
        self.assertFalse(config.allow_fork)
        self.assertFalse(config.allow_shared_workspace_writes)
        self.assertFalse(config.allow_worktree)
        self.assertFalse(config.allow_standard_agent)
        self.assertFalse(config.enable_verify_agent)
        self.assertEqual(config.verify_command_timeout_seconds, 120)

    def test_yaml_loads_phase1b_values(self) -> None:
        payload = {
            "subagents": {
                "enabled": True,
                "max_depth": 1,
                "max_concurrency": 2,
                "max_tasks_per_batch": 4,
                "max_total_tasks": 8,
                "default_max_turns": 12,
                "default_max_tool_calls": 24,
                "default_timeout_seconds": 90,
                "model_request_concurrency": 2,
                "result_summary_chars": 2400,
            }
        }
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            yaml_path = root / "config.yaml"
            yaml_path.write_text(
                "subagents:\n"
                "  enabled: true\n"
                "  max_depth: 1\n"
                "  max_concurrency: 2\n"
                "  max_tasks_per_batch: 4\n"
                "  max_total_tasks: 8\n"
                "  default_max_turns: 12\n"
                "  default_max_tool_calls: 24\n"
                "  default_timeout_seconds: 90\n"
                "  model_request_concurrency: 2\n"
                "  result_summary_chars: 2400\n",
                encoding="utf-8",
            )

            yaml_config = load_subagent_config(yaml_path)

        self.assertTrue(yaml_config.enabled)
        self.assertEqual(yaml_config.default_max_turns, 12)
        self.assertEqual(yaml_config.default_timeout_seconds, 90.0)

    def test_environment_can_only_disable_or_tighten(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            path = Path(temp_dir) / "config.yaml"
            path.write_text(
                json.dumps(
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
                json.dumps(
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
            path = Path(temp_dir) / "config.yaml"
            path.write_text(json.dumps({"subagents": {"allow_background": True}}), encoding="utf-8")
            with patch.dict(os.environ, {"OMNICRAWL_SUBAGENTS_ENABLED": "false"}, clear=False):
                config = load_subagent_config(path)
        self.assertTrue(config.allow_background)
        self.assertFalse(config.enabled)

    def test_current_phase_allows_explicit_fork_but_rejects_recursive_configuration(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            path = Path(temp_dir) / "config.yaml"
            path.write_text(
                json.dumps({"subagents": {"allow_fork": True}}),
                encoding="utf-8",
            )
            self.assertTrue(load_subagent_config(path).allow_fork)

        # max_depth 仍限制为 1；共享写入/worktree/standard 允许显式开启。
        with tempfile.TemporaryDirectory() as temp_dir:
            path = Path(temp_dir) / "config.yaml"
            path.write_text(
                json.dumps({"subagents": {"max_depth": 2}}),
                encoding="utf-8",
            )
            with self.assertRaisesRegex(SubAgentConfigError, "max_depth"):
                load_subagent_config(path)

        with tempfile.TemporaryDirectory() as temp_dir:
            path = Path(temp_dir) / "config.yaml"
            path.write_text(
                json.dumps(
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


if __name__ == "__main__":
    unittest.main()
