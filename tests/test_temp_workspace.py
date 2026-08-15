from __future__ import annotations

import json
import tempfile
import unittest
import os
from datetime import datetime, timedelta
from pathlib import Path

from omnicrawl.temp_workspace import (
    AgentTempWorkspace,
    AgentTempWorkspaceConfig,
    AgentTempWorkspaceError,
    LAST_CLEANUP_FILENAME,
    load_agent_temp_workspace_config,
    resolve_agent_temp_dir,
)


class AgentTempWorkspaceConfigTest(unittest.TestCase):
    def test_load_config_data_uses_defaults_without_section(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            config_path = Path(temp_dir) / "config.toml"
            config_path.write_text("", encoding="utf-8")

            config = load_agent_temp_workspace_config(config_path)

        self.assertTrue(config.enabled)
        self.assertEqual(config.directory, ".omnicrawl/.agent_tmp")
        self.assertTrue(config.cleanup_enabled)
        self.assertEqual(config.cleanup_interval_hours, 24)

    def test_load_config_data_rejects_invalid_cleanup_interval(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            config_path = Path(temp_dir) / "config.toml"
            config_path.write_text(
                json.dumps({"agent_temp": {"cleanup_interval_hours": 0}}),
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

            self.assertTrue((Path(temp_dir) / ".omnicrawl/.agent_tmp" / "files").is_dir())
            self.assertTrue((Path(temp_dir) / ".omnicrawl/.agent_tmp" / "images").is_dir())
            self.assertTrue((Path(temp_dir) / ".omnicrawl/.agent_tmp" / "code").is_dir())
            self.assertTrue((Path(temp_dir) / ".omnicrawl/.agent_tmp" / "videos").is_dir())
            self.assertTrue((Path(temp_dir) / ".omnicrawl/.agent_tmp" / "scripts").is_dir())
            self.assertTrue((Path(temp_dir) / ".omnicrawl/.agent_tmp" / "README.md").is_file())
            self.assertTrue((Path(temp_dir) / ".omnicrawl/.agent_tmp" / ".gitignore").is_file())

    def test_clean_removes_temp_entries_and_preserves_markers(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            workspace = AgentTempWorkspace(Path(temp_dir))
            workspace.ensure()
            root = Path(temp_dir) / ".omnicrawl/.agent_tmp"
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
            self.assertTrue((root / LAST_CLEANUP_FILENAME).is_file())
            self.assertEqual(workspace.last_cleanup_time(), datetime(2026, 6, 10, 4, 0, 0))

    def test_clean_if_due_removes_entries_without_marker(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            workspace = AgentTempWorkspace(Path(temp_dir))
            workspace.ensure()
            root = Path(temp_dir) / ".omnicrawl/.agent_tmp"
            (root / "files" / "scratch.txt").write_text("tmp", encoding="utf-8")

            result = workspace.clean_if_due(datetime(2026, 6, 10, 9, 0, 0))

            self.assertIsNotNone(result)
            self.assertFalse((root / "files" / "scratch.txt").exists())
            self.assertTrue((root / LAST_CLEANUP_FILENAME).is_file())

    def test_clean_if_due_skips_recent_marker(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            workspace = AgentTempWorkspace(Path(temp_dir))
            workspace.clean(datetime(2026, 6, 10, 9, 0, 0))
            root = Path(temp_dir) / ".omnicrawl/.agent_tmp"
            (root / "files" / "scratch.txt").write_text("tmp", encoding="utf-8")

            result = workspace.clean_if_due(datetime(2026, 6, 11, 8, 59, 0))

            self.assertIsNone(result)
            self.assertTrue((root / "files" / "scratch.txt").exists())

    def test_clean_if_due_uses_marker_timestamp_after_restart(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            workspace = AgentTempWorkspace(Path(temp_dir))
            workspace.clean(datetime(2026, 6, 10, 9, 0, 0))
            root = Path(temp_dir) / ".omnicrawl/.agent_tmp"
            (root / "files" / "scratch.txt").write_text("tmp", encoding="utf-8")

            result = workspace.clean_if_due(datetime(2026, 6, 11, 9, 0, 1))

            self.assertIsNotNone(result)
            self.assertFalse((root / "files" / "scratch.txt").exists())
            self.assertEqual(workspace.last_cleanup_time(), datetime(2026, 6, 11, 9, 0, 1))

    def test_last_cleanup_time_reads_marker_file_mtime(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            workspace = AgentTempWorkspace(Path(temp_dir))
            workspace.ensure()
            marker = Path(temp_dir) / ".omnicrawl/.agent_tmp" / LAST_CLEANUP_FILENAME
            marker.write_text("legacy marker\n", encoding="utf-8")
            recorded_at = datetime(2026, 6, 10, 9, 0, 0)
            timestamp = recorded_at.timestamp()
            os.utime(marker, (timestamp, timestamp))

            self.assertEqual(workspace.last_cleanup_time(), recorded_at)

    def test_seconds_until_next_cleanup_uses_cleanup_interval(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            workspace = AgentTempWorkspace(
                Path(temp_dir),
                AgentTempWorkspaceConfig(directory=".omnicrawl/.agent_tmp", cleanup_interval_hours=24),
            )
            workspace.clean(datetime(2026, 6, 10, 9, 0, 0))

            seconds = workspace.seconds_until_next_cleanup(datetime(2026, 6, 10, 9, 30, 0))

        self.assertEqual(seconds, 23.5 * 60 * 60)


if __name__ == "__main__":
    unittest.main()
