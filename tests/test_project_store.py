from __future__ import annotations

import json
import tempfile
import unittest
from pathlib import Path

from ai_voice_agent.project import ProjectStore, ProjectStoreError
from ai_voice_agent.session import SessionStore


class ProjectStoreTest(unittest.TestCase):
    def test_scan_projects_persists_workspace_roots_from_sessions(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            workspace = root / "workspace"
            other_workspace = root / "other"
            workspace.mkdir()
            other_workspace.mkdir()
            session_store = SessionStore(workspace / ".agent_sessions")
            session_store.start_session(workspace)
            session_store.start_session(other_workspace)
            project_store = ProjectStore(workspace / ".agent_sessions")

            projects = project_store.scan_projects(
                session_store.list_project_paths(),
                current_workspace=workspace,
            )
            projects_path = workspace / ".agent_sessions" / "projects.json"
            saved = json.loads(projects_path.read_text(encoding="utf-8"))
            projects_file_exists = projects_path.is_file()

        self.assertTrue(projects_file_exists)
        self.assertEqual(
            {project.path for project in projects},
            {str(workspace.resolve()), str(other_workspace.resolve())},
        )
        self.assertEqual(len(saved["projects"]), 2)

    def test_create_import_rename_pin_and_remove_project(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            store = ProjectStore(root / ".agent_sessions")
            created_path = root / "created"
            imported_path = root / "imported"
            imported_path.mkdir()

            created = store.create_project(name="课程 Demo", path=created_path)
            imported = store.import_project(name="外部项目", path=imported_path)
            renamed = store.rename_project(path=created.path, name="课程 Demo 新名")
            pinned = store.pin_project(imported.path)
            unpinned = store.toggle_project_pin(imported.path)
            store.remove_project(renamed.path)
            projects = store.list_projects()
            created_dir_exists = created_path.is_dir()

        self.assertTrue(created_dir_exists)
        self.assertEqual(created.name, "课程 Demo")
        self.assertEqual(imported.path, str(imported_path.resolve()))
        self.assertEqual(renamed.name, "课程 Demo 新名")
        self.assertTrue(pinned.pinned)
        self.assertFalse(unpinned.pinned)
        self.assertEqual([project.path for project in projects], [str(imported_path.resolve())])

    def test_import_project_requires_existing_directory(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            store = ProjectStore(Path(temp_dir) / ".agent_sessions")

            with self.assertRaisesRegex(ProjectStoreError, "导入项目路径不存在"):
                store.import_project(name="缺失项目", path=Path(temp_dir) / "missing")


if __name__ == "__main__":
    unittest.main()
