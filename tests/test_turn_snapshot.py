from __future__ import annotations

import json
import subprocess
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace

from omnicrawl.agent.core import AgentError, LocalToolAgent
from omnicrawl.agent.types import ToolCall
from omnicrawl.state.memory import MemoryStore, MemoryWriteRequest
from omnicrawl.state.session import SessionStore
from omnicrawl.state.session_models import SessionStoreError
from omnicrawl.state.turn_snapshot import (
    GitSnapshotStore,
    SnapshotConflictError,
    SnapshotError,
    SnapshotRoot,
)


class GitSnapshotStoreTest(unittest.TestCase):
    def test_restores_workspace_without_touching_user_git_state(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir) / "workspace"
            root.mkdir()
            subprocess.run(["git", "init", "-q"], cwd=root, check=True)
            subprocess.run(
                ["git", "config", "user.name", "OmniCrawl Test"], cwd=root, check=True
            )
            subprocess.run(
                ["git", "config", "user.email", "test@example.invalid"], cwd=root, check=True
            )
            tracked = root / "tracked.txt"
            tracked.write_text("initial\n", encoding="utf-8")
            subprocess.run(["git", "add", "tracked.txt"], cwd=root, check=True)
            subprocess.run(["git", "commit", "-qm", "initial"], cwd=root, check=True)

            staged = root / "staged.txt"
            staged.write_text("already staged\n", encoding="utf-8")
            subprocess.run(["git", "add", "staged.txt"], cwd=root, check=True)
            head_before = self._git(root, "rev-parse", "HEAD")
            index_before = self._git(root, "diff", "--cached", "--binary")

            snapshots = GitSnapshotStore(root / ".agent_sessions" / "shadow.git")
            roots = {
                "workspace": SnapshotRoot(root, excluded=(".git", ".agent_sessions"))
            }
            before = snapshots.capture(roots)
            tracked.write_text("changed\n", encoding="utf-8")
            created = root / "created.txt"
            created.write_text("new\n", encoding="utf-8")
            after = snapshots.capture(roots)

            snapshots.transition(roots=roots, expected=after, target=before)

            self.assertEqual(tracked.read_text(encoding="utf-8"), "initial\n")
            self.assertFalse(created.exists())
            self.assertEqual(staged.read_text(encoding="utf-8"), "already staged\n")
            self.assertEqual(self._git(root, "rev-parse", "HEAD"), head_before)
            self.assertEqual(self._git(root, "diff", "--cached", "--binary"), index_before)

    def test_apply_failure_removes_roots_created_during_preflight(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir) / "memory"
            root.mkdir()
            (root / "entry.md").write_text("target\n", encoding="utf-8")
            snapshots = GitSnapshotStore(Path(temp_dir) / "shadow.git")
            roots = {"memory": SnapshotRoot(root)}
            target = snapshots.capture(roots)
            (root / "entry.md").unlink()
            root.rmdir()
            expected = snapshots.capture(roots)
            original_apply = snapshots._apply_patch

            def fail_apply(path: Path, patch: bytes, *, check_only: bool) -> None:
                if check_only:
                    original_apply(path, patch, check_only=True)
                    return
                raise SnapshotError("模拟应用失败")

            snapshots._apply_patch = fail_apply  # type: ignore[method-assign]

            with self.assertRaisesRegex(SnapshotError, "模拟应用失败"):
                snapshots.transition(roots=roots, expected=expected, target=target)
            self.assertFalse(root.exists())

    def test_restores_crlf_files_even_when_attributes_normalize_text(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir) / "workspace"
            root.mkdir()
            (root / ".gitattributes").write_text("*.txt text\n", encoding="utf-8")
            target = root / "notes.txt"
            target.write_bytes(b"before\r\n")
            snapshots = GitSnapshotStore(Path(temp_dir) / "shadow.git")
            roots = {"workspace": SnapshotRoot(root)}
            before = snapshots.capture(roots)
            target.write_bytes(b"after\r\n")
            after = snapshots.capture(roots)

            snapshots.transition(roots=roots, expected=after, target=before)

            self.assertEqual(target.read_bytes(), b"before\r\n")

    def test_conflict_rejects_all_roots_without_partial_restore(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            base = Path(temp_dir)
            workspace = base / "workspace"
            memory = base / "memory"
            workspace.mkdir()
            memory.mkdir()
            workspace_file = workspace / "app.py"
            memory_file = memory / "index.json"
            workspace_file.write_text("before workspace\n", encoding="utf-8")
            memory_file.write_text("before memory\n", encoding="utf-8")

            snapshots = GitSnapshotStore(base / "shadow.git")
            roots = {
                "workspace": SnapshotRoot(workspace),
                "user_memory": SnapshotRoot(memory),
            }
            before = snapshots.capture(roots)
            workspace_file.write_text("after workspace\n", encoding="utf-8")
            memory_file.write_text("after memory\n", encoding="utf-8")
            after = snapshots.capture(roots)
            memory_file.write_text("external edit\n", encoding="utf-8")

            with self.assertRaisesRegex(SnapshotConflictError, "user_memory"):
                snapshots.transition(roots=roots, expected=after, target=before)

            self.assertEqual(
                workspace_file.read_text(encoding="utf-8"), "after workspace\n"
            )
            self.assertEqual(memory_file.read_text(encoding="utf-8"), "external edit\n")

    def test_agent_undo_restores_workspace_and_all_memory_scopes(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            agent, workspace_file = self._build_agent(Path(temp_dir))
            project_memory = agent._project_memory_store
            session_memory = agent._session_memory_store
            user_memory = agent._user_memory_store
            project_memory.write(
                [
                    MemoryWriteRequest(
                        "既有项目事实",
                        [],
                        storage_directory="project/general",
                    )
                ]
            )
            project_before = self._tree_bytes(project_memory.root)

            agent._append_session_event("user_message", {"content": "修改文件和记忆"})
            snapshot = agent._begin_turn_snapshot()
            agent._append_prompt_history("修改文件和记忆")
            workspace_file.write_text("after\n", encoding="utf-8")
            created_file = agent.workspace_root / "created.txt"
            created_file.write_text("created\n", encoding="utf-8")
            temp_file = agent.workspace_root / ".agent_tmp" / "files" / "draft.txt"
            temp_file.parent.mkdir(parents=True)
            temp_file.write_text("temporary\n", encoding="utf-8")
            session_artifact = (
                agent._session_store.artifacts_dir
                / agent._session_state.session_id
                / "tool-output.txt"
            )
            session_artifact.write_text("artifact\n", encoding="utf-8")
            project_memory.write(
                [
                    MemoryWriteRequest(
                        "既有项目事实",
                        [],
                        storage_directory="project/general",
                    )
                ]
            )
            session_memory.write(
                [
                    MemoryWriteRequest(
                        "本轮会话事项",
                        [],
                        storage_directory="session/general",
                    )
                ]
            )
            user_memory.write(
                [
                    MemoryWriteRequest(
                        "用户稳定偏好",
                        [],
                        storage_directory="user/general",
                    )
                ]
            )
            for name in (
                "write_file",
                "project_memory_write",
                "session_memory_write",
                "user_memory_write",
            ):
                agent._record_turn_tool_execution(snapshot, ToolCall(name=name))
                agent._append_session_event(
                    "tool_result", {"tool": name, "ok": True, "output": "ok"}
                )
            agent._append_session_event("assistant_message", {"content": "已完成"})
            agent._complete_turn_snapshot(snapshot)
            agent._history = [
                {"role": "user", "content": "修改文件和记忆"},
                {"role": "assistant", "content": "已完成"},
            ]

            agent.undo_last_turn()

            self.assertEqual(workspace_file.read_text(encoding="utf-8"), "before\n")
            self.assertFalse(created_file.exists())
            self.assertFalse(temp_file.exists())
            self.assertFalse(session_artifact.exists())
            self.assertEqual(self._tree_bytes(project_memory.root), project_before)
            self.assertFalse(session_memory.root.exists())
            self.assertFalse(user_memory.root.exists())
            self.assertEqual(
                agent._session_store.search_prompt_history(
                    workspace_root=agent.workspace_root,
                    session_id=agent._session_state.session_id,
                ),
                [],
            )
            self.assertEqual(agent._history, [])
            persisted_events = [
                json.loads(line)
                for line in agent._session_state.path.read_text(encoding="utf-8").splitlines()
                if line.strip()
            ]
            self.assertTrue(
                persisted_events[-1]["payload"].get("side_effects_reverted")
            )

    def test_agent_undo_conflict_keeps_conversation_files_and_memory(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            agent, workspace_file = self._build_agent(Path(temp_dir))
            agent._append_session_event("user_message", {"content": "修改文件"})
            snapshot = agent._begin_turn_snapshot()
            workspace_file.write_text("turn result\n", encoding="utf-8")
            agent._session_memory_store.write(
                [
                    MemoryWriteRequest(
                        "本轮记忆",
                        [],
                        storage_directory="session/general",
                    )
                ]
            )
            for name in ("write_file", "session_memory_write"):
                agent._record_turn_tool_execution(snapshot, ToolCall(name=name))
                agent._append_session_event(
                    "tool_result", {"tool": name, "ok": True, "output": "ok"}
                )
            agent._append_session_event("assistant_message", {"content": "已完成"})
            agent._complete_turn_snapshot(snapshot)
            agent._history = [
                {"role": "user", "content": "修改文件"},
                {"role": "assistant", "content": "已完成"},
            ]
            workspace_file.write_text("external edit\n", encoding="utf-8")

            with self.assertRaisesRegex(AgentError, "workspace"):
                agent.undo_last_turn()

            self.assertEqual(workspace_file.read_text(encoding="utf-8"), "external edit\n")
            self.assertTrue(agent._session_memory_store.root.exists())
            self.assertEqual(len(agent._history), 2)
            events = agent._session_store.read_session_events(
                agent._session_state.session_id
            )
            self.assertNotEqual(events[-1].type, "turn_undone")

    def test_session_commit_failure_reapplies_turn_end_snapshot(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            agent, workspace_file = self._build_agent(Path(temp_dir))
            agent._append_session_event("user_message", {"content": "修改后模拟提交失败"})
            snapshot = agent._begin_turn_snapshot()
            workspace_file.write_text("turn result\n", encoding="utf-8")
            agent._record_turn_tool_execution(snapshot, ToolCall(name="write_file"))
            agent._append_session_event(
                "tool_result", {"tool": "write_file", "ok": True, "output": "ok"}
            )
            agent._append_session_event("assistant_message", {"content": "已完成"})
            agent._complete_turn_snapshot(snapshot)
            agent._history = [
                {"role": "user", "content": "修改后模拟提交失败"},
                {"role": "assistant", "content": "已完成"},
            ]

            def fail_commit(*_args, **_kwargs):
                raise SessionStoreError("模拟 Session 提交失败")

            agent._session_store.commit_undo_plan = fail_commit  # type: ignore[method-assign]

            with self.assertRaisesRegex(AgentError, "模拟 Session 提交失败"):
                agent.undo_last_turn()

            self.assertEqual(
                workspace_file.read_text(encoding="utf-8"), "turn result\n"
            )
            self.assertEqual(len(agent._history), 2)

    def test_agent_undo_rejects_irreversible_tool_and_legacy_side_effect(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            base = Path(temp_dir)
            agent, workspace_file = self._build_agent(base / "snapshotted")
            agent._append_session_event("user_message", {"content": "执行外部命令"})
            snapshot = agent._begin_turn_snapshot()
            workspace_file.write_text("shell result\n", encoding="utf-8")
            agent._record_turn_tool_execution(
                snapshot,
                ToolCall(name="bash", arguments={"command": "echo changed"}),
            )
            agent._append_session_event(
                "tool_result", {"tool": "bash", "ok": True, "output": "ok"}
            )
            agent._append_session_event("assistant_message", {"content": "已执行"})
            agent._complete_turn_snapshot(snapshot)
            agent._history = [
                {"role": "user", "content": "执行外部命令"},
                {"role": "assistant", "content": "已执行"},
            ]

            with self.assertRaisesRegex(AgentError, "bash"):
                agent.undo_last_turn()
            self.assertEqual(
                workspace_file.read_text(encoding="utf-8"), "shell result\n"
            )
            self.assertEqual(len(agent._history), 2)

            legacy_agent, legacy_file = self._build_agent(base / "legacy")
            legacy_agent._append_session_event("user_message", {"content": "旧轮次写文件"})
            legacy_file.write_text("legacy result\n", encoding="utf-8")
            legacy_agent._append_session_event(
                "tool_result", {"tool": "write_file", "ok": True, "output": "ok"}
            )
            legacy_agent._append_session_event("assistant_message", {"content": "已完成"})
            legacy_agent._history = [
                {"role": "user", "content": "旧轮次写文件"},
                {"role": "assistant", "content": "已完成"},
            ]

            with self.assertRaisesRegex(AgentError, "没有 Git 快照"):
                legacy_agent.undo_last_turn()
            self.assertEqual(
                legacy_file.read_text(encoding="utf-8"), "legacy result\n"
            )
            self.assertEqual(len(legacy_agent._history), 2)

            external_agent, _external_file = self._build_agent(base / "external")
            external_agent._append_session_event(
                "user_message", {"content": "旧轮次启动监控"}
            )
            external_agent._append_session_event(
                "tool_call_requested",
                {
                    "tool": "monitor",
                    "tool_call_id": "legacy-monitor",
                    "arguments": {"action": "start"},
                },
            )
            external_agent._append_session_event(
                "tool_result",
                {
                    "tool": "monitor",
                    "tool_call_id": "legacy-monitor",
                    "ok": True,
                    "output": "started",
                },
            )
            external_agent._append_session_event(
                "assistant_message", {"content": "已启动"}
            )
            external_agent._history = [
                {"role": "user", "content": "旧轮次启动监控"},
                {"role": "assistant", "content": "已启动"},
            ]

            with self.assertRaisesRegex(AgentError, "monitor"):
                external_agent.undo_last_turn()
            self.assertEqual(len(external_agent._history), 2)

    @staticmethod
    def _build_agent(base: Path) -> tuple[LocalToolAgent, Path]:
        workspace = base / "workspace"
        workspace.mkdir(parents=True)
        workspace_file = workspace / "app.txt"
        workspace_file.write_text("before\n", encoding="utf-8")
        session_store = SessionStore(workspace / ".agent_sessions")
        state = session_store.start_session(workspace)

        agent = object.__new__(LocalToolAgent)
        agent.workspace_root = workspace
        agent.config = SimpleNamespace(max_history_turns=6)
        agent._session_store = session_store
        agent._session_state = state
        agent._project_memory_store = MemoryStore(workspace / ".oclmemory")
        agent._session_memory_store = MemoryStore(
            base / "Session_memory" / state.session_id
        )
        agent._user_memory_store = MemoryStore(base / "User_memory")
        agent._memory_store = agent._project_memory_store
        agent._history = []
        agent._pending_user_text = None
        agent._active_skills = []
        agent._search_index = None
        return agent, workspace_file

    @staticmethod
    def _tree_bytes(root: Path) -> dict[str, bytes]:
        if not root.exists():
            return {}
        return {
            path.relative_to(root).as_posix(): path.read_bytes()
            for path in root.rglob("*")
            if path.is_file()
        }

    @staticmethod
    def _git(root: Path, *arguments: str) -> str:
        return subprocess.run(
            ["git", *arguments],
            cwd=root,
            check=True,
            text=True,
            stdout=subprocess.PIPE,
        ).stdout


if __name__ == "__main__":
    unittest.main()
