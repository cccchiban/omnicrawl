from __future__ import annotations

import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from ai_voice_agent.project_context import (
    LAUNCH_CWD_ENV,
    WORKSPACE_ROOT_ENV,
    ProjectContextError,
    detect_project_context,
    find_project_root,
    project_context_status_label,
)


class ProjectContextTest(unittest.TestCase):
    def test_find_project_root_prefers_nearest_parent_marker(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            workspace = Path(temp_dir)
            nested = workspace / "src" / "feature"
            nested.mkdir(parents=True)
            (workspace / "pyproject.toml").write_text("[project]\n", encoding="utf-8")

            detected = find_project_root(nested)

        self.assertIsNotNone(detected)
        root, marker = detected or (Path(), "")
        self.assertEqual(root, workspace.resolve())
        self.assertEqual(marker, "pyproject.toml")

    def test_find_project_root_ignores_too_broad_parent_marker(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            fake_home = Path(temp_dir) / "home"
            nested = fake_home / "scratch" / "child"
            nested.mkdir(parents=True)
            (fake_home / "AGENTS.md").write_text("# broad home marker\n", encoding="utf-8")

            with patch("ai_voice_agent.project_context.Path.home", return_value=fake_home):
                detected = find_project_root(nested)

        self.assertIsNone(detected)

    def test_detect_project_context_uses_launch_cwd_marker(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            app_root = Path(temp_dir) / "agent_app"
            workspace = Path(temp_dir) / "actual_project"
            nested = workspace / "src"
            app_root.mkdir()
            nested.mkdir(parents=True)
            (workspace / ".git").mkdir()

            with patch.dict(
                "os.environ",
                {
                    LAUNCH_CWD_ENV: str(nested),
                    WORKSPACE_ROOT_ENV: "",
                },
                clear=False,
            ):
                context = detect_project_context(app_root=app_root)

        self.assertEqual(context.workspace_root, workspace.resolve())
        self.assertEqual(context.start_path, nested.resolve())
        self.assertEqual(context.source, "marker")
        self.assertEqual(context.marker, ".git")
        self.assertIn(str(context.workspace_root), context.detection_summary)
        self.assertIn(".git", project_context_status_label(context))

    def test_detect_project_context_uses_start_directory_when_no_marker(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            app_root = Path(temp_dir) / "agent_app"
            workspace = Path(temp_dir) / "scratch_project"
            app_root.mkdir()
            workspace.mkdir()

            with patch.dict(
                "os.environ",
                {
                    LAUNCH_CWD_ENV: "",
                    WORKSPACE_ROOT_ENV: "",
                },
                clear=False,
            ):
                context = detect_project_context(app_root=app_root, start_path=workspace)

        self.assertEqual(context.workspace_root, workspace.resolve())
        self.assertEqual(context.source, "fallback_start")
        self.assertIn("未发现项目标记", context.detection_summary)

    def test_detect_project_context_environment_override_must_exist(self) -> None:
        missing = Path(tempfile.gettempdir()) / "missing-agent-workspace"
        with patch.dict(
            "os.environ",
            {
                WORKSPACE_ROOT_ENV: str(missing),
                LAUNCH_CWD_ENV: "",
            },
            clear=False,
        ):
            with self.assertRaises(ProjectContextError):
                detect_project_context(app_root=Path(tempfile.gettempdir()))


if __name__ == "__main__":
    unittest.main()
