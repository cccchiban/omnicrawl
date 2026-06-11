from __future__ import annotations

import json
import tempfile
import unittest
from datetime import datetime
from pathlib import Path

from ai_voice_agent.temp_workspace import (
    AgentTempWorkspace,
    AgentTempWorkspaceConfig,
    AgentTempWorkspaceError,
    load_agent_temp_workspace_config,
    resolve_agent_temp_dir,
)


class AgentTempWorkspaceConfigTest(unittest.TestCase):
    def test_load_config_data_uses_defaults_without_section(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            config_path = Path(temp_dir) / "config.json"
            config_path.write_text("{}", encoding="utf-8")

            config = load_agent_temp_workspace_config(config_path)

        self.assertTrue(config.enabled)
        self.assertEqual(config.directory, ".agent_tmp")
        self.assertTrue(config.cleanup_enabled)
        self.assertEqual(config.cleanup_hour, 4)

    def test_load_config_data_rejects_invalid_cleanup_hour(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            config_path = Path(temp_dir) / "config.json"
            config_path.write_text(
                json.dumps({"agent_temp": {"cleanup_hour": 24}}),
                encoding="utf-8",
            )

            with self.assertRaises(AgentTempWorkspaceError):
                load_agent_temp_workspace_config(config_path)

    def test_resolve_agent_temp_dir_rejects_workspace_escape(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            with self.assertRaises(AgentTempWorkspaceError):
                resolve_agent_temp_dir(Path(temp_dir), "../outside")


class AgentTempWorkspaceTest(unittest.TestCase):
    def test_ensure_creates_classified_directories_and_markers(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            workspace = AgentTempWorkspace(Path(temp_dir))

            workspace.ensure()

            self.assertTrue((Path(temp_dir) / ".agent_tmp" / "files").is_dir())
            self.assertTrue((Path(temp_dir) / ".agent_tmp" / "images").is_dir())
            self.assertTrue((Path(temp_dir) / ".agent_tmp" / "code").is_dir())
            self.assertTrue((Path(temp_dir) / ".agent_tmp" / "videos").is_dir())
            self.assertTrue((Path(temp_dir) / ".agent_tmp" / "scripts").is_dir())
            self.assertTrue((Path(temp_dir) / ".agent_tmp" / "README.md").is_file())
            self.assertTrue((Path(temp_dir) / ".agent_tmp" / ".gitignore").is_file())

    def test_clean_removes_temp_entries_and_preserves_markers(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            workspace = AgentTempWorkspace(Path(temp_dir))
            workspace.ensure()
            root = Path(temp_dir) / ".agent_tmp"
            (root / "files" / "scratch.txt").write_text("tmp", encoding="utf-8")
            (root / "scripts" / "work.ps1").write_text("tmp", encoding="utf-8")

            result = workspace.clean(datetime(2026, 6, 10, 4, 0, 0))

            self.assertIn("files", result.deleted_entries)
            self.assertIn("scripts", result.deleted_entries)
            self.assertTrue((root / "README.md").is_file())
            self.assertTrue((root / ".gitignore").is_file())
            self.assertTrue((root / "files").is_dir())
            self.assertTrue((root / "scripts").is_dir())
            self.assertFalse((root / "files" / "scratch.txt").exists())
            self.assertFalse((root / "scripts" / "work.ps1").exists())

    def test_seconds_until_next_cleanup_uses_next_day_after_cleanup_hour(self) -> None:
        workspace = AgentTempWorkspace(
            Path.cwd(),
            AgentTempWorkspaceConfig(directory=".agent_tmp", cleanup_hour=4),
        )

        seconds = workspace.seconds_until_next_cleanup(datetime(2026, 6, 10, 4, 30, 0))

        self.assertEqual(seconds, 23.5 * 60 * 60)


if __name__ == "__main__":
    unittest.main()
