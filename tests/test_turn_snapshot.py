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
    SnapshotConflictError,
    SnapshotError,
    WorktreeSnapshotStore,
)


def _git(root: Path, *arguments: str) -> str:
    return subprocess.run(
        ["git", *arguments],
        cwd=root,
        check=True,
        text=True,
        stdout=subprocess.PIPE,
    ).stdout


def _init_git(root: Path) -> None:
    subprocess.run(["git", "init", "-q"], cwd=root, check=True)
    subprocess.run(
        ["git", "config", "user.name", "OmniCrawl Test"], cwd=root, check=True
    )
    subprocess.run(
        ["git", "config", "user.email", "test@example.invalid"], cwd=root, check=True
    )


class WorktreeSnapshotStoreTest(unittest.TestCase):
    def test_capture_non_git_workspace_has_no_head(self) -> None:
        """非 Git 工作区返回 has_head=False，由上层降级禁用 undo。"""

        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir) / "workspace"
            root.mkdir()
            (root / "a.txt").write_text("a\n", encoding="utf-8")
            snapshot = WorktreeSnapshotStore().capture(root)
            self.assertFalse(snapshot.has_head)
            self.assertEqual(snapshot.patch, b"")
            self.assertEqual(snapshot.untracked, ())

    def test_restores_workspace_without_touching_user_git_state(self) -> None:
        """diff 快照回退不修改用户仓库的 HEAD 与已暂存内容。"""

        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir) / "workspace"
            root.mkdir()
            _init_git(root)
            tracked = root / "tracked.txt"
            tracked.write_text("initial\n", encoding="utf-8")
            subprocess.run(["git", "add", "tracked.txt"], cwd=root, check=True)
            subprocess.run(["git", "commit", "-qm", "initial"], cwd=root, check=True)

            staged = root / "staged.txt"
            staged.write_text("already staged\n", encoding="utf-8")
            subprocess.run(["git", "add", "staged.txt"], cwd=root, check=True)
            head_before = _git(root, "rev-parse", "HEAD")

            store = WorktreeSnapshotStore()
            before = store.capture(root)
            tracked.write_text("changed\n", encoding="utf-8")
            created = root / "created.txt"
            created.write_text("new\n", encoding="utf-8")
            after = store.capture(root)

            store.transition(root, expected=after, target=before)

            self.assertEqual(tracked.read_text(encoding="utf-8"), "initial\n")
            self.assertFalse(created.exists())
            # 已暂存文件内容保留（apply 后变为未暂存，但内容一致）。
            self.assertEqual(staged.read_text(encoding="utf-8"), "already staged\n")
            self.assertEqual(_git(root, "rev-parse", "HEAD"), head_before)

    def test_restores_crlf_files_even_when_attributes_normalize_text(self) -> None:
        """gitattributes 声明 text 时，diff/apply 往返仍保留原始 CRLF 内容。"""

        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir) / "workspace"
            root.mkdir()
            _init_git(root)
            (root / ".gitattributes").write_text("*.txt text\n", encoding="utf-8")
            subprocess.run(["git", "add", ".gitattributes"], cwd=root, check=True)
            subprocess.run(["git", "commit", "-qm", "attrs"], cwd=root, check=True)
            target = root / "notes.txt"
            target.write_bytes(b"before\r\n")
            subprocess.run(["git", "add", "notes.txt"], cwd=root, check=True)
            subprocess.run(["git", "commit", "-qm", "notes"], cwd=root, check=True)

            store = WorktreeSnapshotStore()
            before = store.capture(root)
            target.write_bytes(b"after\r\n")
            after = store.capture(root)

            store.transition(root, expected=after, target=before)

            self.assertEqual(target.read_bytes(), b"before\r\n")

    def test_conflict_rejects_when_workspace_changed_after_turn(self) -> None:
        """轮次结束后外部修改工作区，回退被拒绝且不覆盖用户的新修改。"""

        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir) / "workspace"
            root.mkdir()
            _init_git(root)
            target = root / "app.py"
            target.write_text("before workspace\n", encoding="utf-8")
            subprocess.run(["git", "add", "app.py"], cwd=root, check=True)
            subprocess.run(["git", "commit", "-qm", "initial"], cwd=root, check=True)

            store = WorktreeSnapshotStore()
            before = store.capture(root)
            target.write_text("after workspace\n", encoding="utf-8")
            after = store.capture(root)
            target.write_text("external edit\n", encoding="utf-8")

            with self.assertRaisesRegex(SnapshotConflictError, "被修改"):
                store.transition(root, expected=after, target=before)

            self.assertEqual(target.read_text(encoding="utf-8"), "external edit\n")

    def test_untracked_created_in_turn_removed_on_undo(self) -> None:
        """本轮新增的未跟踪文件在回退时删除。"""

        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir) / "workspace"
            root.mkdir()
            _init_git(root)
            (root / "keep.txt").write_text("tracked\n", encoding="utf-8")
            subprocess.run(["git", "add", "keep.txt"], cwd=root, check=True)
            subprocess.run(["git", "commit", "-qm", "initial"], cwd=root, check=True)

            store = WorktreeSnapshotStore()
            before = store.capture(root)
            (root / "new.txt").write_text("new\n", encoding="utf-8")
            (root / "dir").mkdir()
            (root / "dir" / "nested.txt").write_text("nested\n", encoding="utf-8")
            after = store.capture(root)

            store.transition(root, expected=after, target=before)

            self.assertFalse((root / "new.txt").exists())
            self.assertFalse((root / "dir" / "nested.txt").exists())

    def test_untracked_removed_in_turn_reported_unrestorable(self) -> None:
        """轮次中被删除的未跟踪文件没有内容副本，transition 返回提示。"""

        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir) / "workspace"
            root.mkdir()
            _init_git(root)
            (root / "keep.txt").write_text("tracked\n", encoding="utf-8")
            subprocess.run(["git", "add", "keep.txt"], cwd=root, check=True)
            subprocess.run(["git", "commit", "-qm", "initial"], cwd=root, check=True)
            removed = root / "gone.txt"
            removed.write_text("gone\n", encoding="utf-8")

            store = WorktreeSnapshotStore()
            before = store.capture(root)
            removed.unlink()
            after = store.capture(root)

            unrestorable = store.transition(root, expected=after, target=before)
            self.assertEqual(unrestorable, ["gone.txt"])
            self.assertFalse(removed.exists())

    def test_ignored_files_not_included_in_untracked(self) -> None:
        """被 .gitignore 忽略的受控运行态（config.toml 等）不纳入回退范围。"""

        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir) / "workspace"
            root.mkdir()
            _init_git(root)
            (root / ".gitignore").write_text("config.toml\n.agent_tmp/\n", encoding="utf-8")
            subprocess.run(["git", "add", ".gitignore"], cwd=root, check=True)
            subprocess.run(["git", "commit", "-qm", "gitignore"], cwd=root, check=True)
            (root / "config.toml").write_text("secret\n", encoding="utf-8")
            (root / ".agent_tmp").mkdir()
            (root / ".agent_tmp" / "draft.txt").write_text("draft\n", encoding="utf-8")

            snapshot = WorktreeSnapshotStore().capture(root)
            self.assertNotIn("config.toml", snapshot.untracked)
            self.assertNotIn(".agent_tmp/draft.txt", snapshot.untracked)

    def test_has_head_cached_within_ttl(self) -> None:
        """has_head 在 TTL 内缓存，避免每轮重复支付 rev-parse 子进程成本。"""

        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir) / "workspace"
            root.mkdir(parents=True)
            _init_git(root)
            (root / "base.txt").write_text("base\n", encoding="utf-8")
            subprocess.run(["git", "add", "base.txt"], cwd=root, check=True)
            subprocess.run(["git", "commit", "-qm", "base"], cwd=root, check=True)
            store = WorktreeSnapshotStore()
            WorktreeSnapshotStore._head_cache.clear()
            real_has_head = WorktreeSnapshotStore._has_head

            calls: list[Path] = []

            def counting_has_head(_self: WorktreeSnapshotStore, workspace: Path) -> bool:
                calls.append(Path(workspace).resolve())
                return real_has_head(_self, workspace)

            WorktreeSnapshotStore._has_head = counting_has_head  # type: ignore[method-assign]
            try:
                self.assertTrue(store.has_head(root))
                self.assertTrue(store.has_head(root))
                self.assertTrue(store.has_head(root))
            finally:
                WorktreeSnapshotStore._has_head = real_has_head  # type: ignore[method-assign]
                WorktreeSnapshotStore._head_cache.clear()
            # 3 次调用只触发 1 次底层 rev-parse。
            self.assertEqual(len(calls), 1)

    def test_has_head_cache_cleared_for_new_workspace(self) -> None:
        """不同工作区使用独立缓存项，互不污染。"""

        with tempfile.TemporaryDirectory() as temp_dir:
            git_root = Path(temp_dir) / "git_ws"
            git_root.mkdir(parents=True)
            _init_git(git_root)
            (git_root / "base.txt").write_text("base\n", encoding="utf-8")
            subprocess.run(["git", "add", "base.txt"], cwd=git_root, check=True)
            subprocess.run(["git", "commit", "-qm", "base"], cwd=git_root, check=True)
            plain_root = Path(temp_dir) / "plain_ws"
            plain_root.mkdir(parents=True)
            store = WorktreeSnapshotStore()
            WorktreeSnapshotStore._head_cache.clear()
            self.assertTrue(store.has_head(git_root))
            self.assertFalse(store.has_head(plain_root))
            self.assertTrue(store.has_head(git_root))
            self.assertFalse(store.has_head(plain_root))
            WorktreeSnapshotStore._head_cache.clear()


class AgentUndoTest(unittest.TestCase):
    def test_agent_undo_restores_workspace_only(self) -> None:
        """/undo 回退工作区 Git 记录的文件，记忆与 Session artifact 不回退。"""

        with tempfile.TemporaryDirectory() as temp_dir:
            agent, workspace_file = self._build_agent(Path(temp_dir))
            project_memory = agent._project_memory_store
            session_memory = agent._session_memory_store
            user_memory = agent._user_memory_store

            agent._append_session_event("user_message", {"content": "修改文件和记忆"})
            snapshot = agent._begin_turn_snapshot()
            self.assertIsNotNone(snapshot)
            agent._append_prompt_history("修改文件和记忆")
            created_file = agent.workspace_root / "created.txt"
            temp_file = agent.workspace_root / ".omnicrawl" / ".agent_tmp" / "files" / "draft.txt"
            session_artifact = (
                agent._session_store.artifacts_dir
                / agent._session_state.session_id
                / "tool-output.txt"
            )
            project_memory = agent._project_memory_store
            session_memory = agent._session_memory_store
            user_memory = agent._user_memory_store
            # 模拟真实时序：先记录工具执行（首个写工具在此触发惰性起点捕获），
            # 之后才产生文件/记忆副作用。
            for name in (
                "write_file",
                "project_memory_write",
                "session_memory_write",
                "user_memory_write",
            ):
                agent._record_turn_tool_execution(snapshot, ToolCall(name=name))
            workspace_file.write_text("after\n", encoding="utf-8")
            created_file.write_text("created\n", encoding="utf-8")
            temp_file.parent.mkdir(parents=True)
            temp_file.write_text("temporary\n", encoding="utf-8")
            session_artifact.parent.mkdir(parents=True, exist_ok=True)
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

            # 工作区 Git 记录的变化被回退。
            self.assertEqual(workspace_file.read_text(encoding="utf-8"), "before\n")
            self.assertFalse(created_file.exists())
            self.assertFalse(temp_file.exists())
            # 会话 artifact、三类记忆与提示历史不回退（1B 决策：/undo 只
            # 回退工作区 Git 记录的更改，放弃对会话/记忆数据的回退）。
            self.assertTrue(session_artifact.exists())
            self.assertTrue((project_memory.root / "project" / "general").is_dir())
            self.assertTrue(session_memory.root.exists())
            self.assertTrue(user_memory.root.exists())
            self.assertEqual(
                len(
                    agent._session_store.search_prompt_history(
                        workspace_root=agent.workspace_root,
                        session_id=agent._session_state.session_id,
                    )
                ),
                1,
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

    def test_agent_undo_conflict_keeps_workspace(self) -> None:
        """轮次后外部修改工作区，/undo 拒绝且不覆盖用户修改。"""

        with tempfile.TemporaryDirectory() as temp_dir:
            agent, workspace_file = self._build_agent(Path(temp_dir))
            agent._append_session_event("user_message", {"content": "修改文件"})
            snapshot = agent._begin_turn_snapshot()
            agent._record_turn_tool_execution(snapshot, ToolCall(name="write_file"))
            agent._record_turn_tool_execution(
                snapshot, ToolCall(name="session_memory_write")
            )
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

            with self.assertRaisesRegex(AgentError, "被修改"):
                agent.undo_last_turn()

            self.assertEqual(workspace_file.read_text(encoding="utf-8"), "external edit\n")
            self.assertTrue(agent._session_memory_store.root.exists())
            self.assertEqual(len(agent._history), 2)
            events = agent._session_store.read_session_events(
                agent._session_state.session_id
            )
            self.assertNotEqual(events[-1].type, "turn_undone")

    def test_session_commit_failure_reapplies_turn_end_snapshot(self) -> None:
        """Session 提交失败时反向恢复，工作区回到轮次终点状态。"""

        with tempfile.TemporaryDirectory() as temp_dir:
            agent, workspace_file = self._build_agent(Path(temp_dir))
            agent._append_session_event("user_message", {"content": "修改后模拟提交失败"})
            snapshot = agent._begin_turn_snapshot()
            agent._record_turn_tool_execution(snapshot, ToolCall(name="write_file"))
            workspace_file.write_text("turn result\n", encoding="utf-8")
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

    def test_agent_undo_rejects_irreversible_tool(self) -> None:
        """执行不可逆工具（bash 等）的轮次拒绝事务式 /undo。"""

        with tempfile.TemporaryDirectory() as temp_dir:
            agent, workspace_file = self._build_agent(Path(temp_dir))
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

    def test_agent_undo_rejects_non_git_turn_with_file_side_effect(self) -> None:
        """非 Git 工作区的写文件轮次没有快照，/undo 明确拒绝。"""

        with tempfile.TemporaryDirectory() as temp_dir:
            base = Path(temp_dir)
            workspace = base / "workspace"
            workspace.mkdir(parents=True)
            workspace_file = workspace / "app.txt"
            workspace_file.write_text("legacy result\n", encoding="utf-8")
            session_store = SessionStore(base / ".agent_sessions")
            state = session_store.start_session(workspace)

            agent = object.__new__(LocalToolAgent)
            agent.workspace_root = workspace
            agent.config = SimpleNamespace(max_history_turns=6)
            agent._session_store = session_store
            agent._session_state = state
            agent._history = []
            agent._pending_user_text = None
            agent._active_skills = []
            agent._append_session_event("user_message", {"content": "写文件"})
            self.assertIsNone(agent._begin_turn_snapshot())
            agent._append_session_event(
                "tool_result", {"tool": "write_file", "ok": True, "output": "ok"}
            )
            agent._append_session_event("assistant_message", {"content": "已完成"})
            agent._history = [
                {"role": "user", "content": "写文件"},
                {"role": "assistant", "content": "已完成"},
            ]

            with self.assertRaisesRegex(AgentError, "没有 Git 快照"):
                agent.undo_last_turn()
            self.assertEqual(
                workspace_file.read_text(encoding="utf-8"), "legacy result\n"
            )
            self.assertEqual(len(agent._history), 2)

    def test_agent_undo_rejects_legacy_version_snapshot(self) -> None:
        """旧版影子对象库快照（version 1）无法解析，/undo 明确拒绝。"""

        with tempfile.TemporaryDirectory() as temp_dir:
            agent, _workspace_file = self._build_agent(Path(temp_dir))
            agent._append_session_event("user_message", {"content": "旧格式轮次"})
            agent._append_session_event(
                "turn_snapshot",
                {
                    "version": 1,
                    "snapshot_id": "deadbeef",
                    "roots": {"workspace": {"before": {}, "after": {}}},
                    "irreversible_tools": [],
                },
            )
            agent._append_session_event("assistant_message", {"content": "完成"})
            agent._history = [
                {"role": "user", "content": "旧格式轮次"},
                {"role": "assistant", "content": "完成"},
            ]

            with self.assertRaisesRegex(AgentError, "version 1"):
                agent.undo_last_turn()
            self.assertEqual(len(agent._history), 2)

    def test_read_only_turn_never_captures_snapshot(self) -> None:
        """纯读/纯对话轮次全程 0 次 git 捕获、不落盘、不产生 turn_snapshot。

        惰性快照的核心收益：只读工具（read/grep/memory 等）不需要 begin
        diff，/undo 也无需快照即可安全逻辑回退。
        """

        with tempfile.TemporaryDirectory() as temp_dir:
            agent, workspace_file = self._build_agent(Path(temp_dir))
            agent._append_session_event("user_message", {"content": "只读查询"})
            snapshot = agent._begin_turn_snapshot()
            self.assertIsNotNone(snapshot)
            # 只读工具执行：不触发惰性捕获。
            for name in ("read", "grep", "project_memory_search"):
                agent._record_turn_tool_execution(snapshot, ToolCall(name=name))
            agent._append_session_event("assistant_message", {"content": "查询完成"})
            agent._complete_turn_snapshot(snapshot)
            self.assertIsNone(snapshot.before)
            self.assertTrue(snapshot.completed)

            # 没有快照事件、没有 undo 目录落盘。
            events = agent._session_store.read_session_events(
                agent._session_state.session_id
            )
            self.assertFalse(
                [event for event in events if event.type == "turn_snapshot"]
            )
            undo_dir = (
                agent._session_store.artifacts_dir
                / agent._session_state.session_id
                / "undo"
            )
            self.assertFalse(undo_dir.exists())
            # 工作区没被动过，文件保持原样。
            self.assertEqual(workspace_file.read_text(encoding="utf-8"), "before\n")

    def test_write_turn_captures_lazily_on_first_write_tool(self) -> None:
        """写文件轮在首个可回退写工具执行时才捕获起点，且能正常回退。"""

        with tempfile.TemporaryDirectory() as temp_dir:
            agent, workspace_file = self._build_agent(Path(temp_dir))
            agent._append_session_event("user_message", {"content": "写文件"})
            snapshot = agent._begin_turn_snapshot()
            self.assertIsNotNone(snapshot)
            self.assertIsNone(snapshot.before)
            # 先跑只读工具：仍不捕获。
            agent._record_turn_tool_execution(snapshot, ToolCall(name="read"))
            self.assertIsNone(snapshot.before)
            # 首个写工具：触发捕获，且发生在文件副作用之前。
            agent._record_turn_tool_execution(
                snapshot, ToolCall(name="write_file")
            )
            self.assertIsNotNone(snapshot.before)
            self.assertEqual(
                snapshot.before.patch, b""
            )  # 起点无修改 → 空补丁
            workspace_file.write_text("lazy after\n", encoding="utf-8")
            agent._append_session_event(
                "tool_result", {"tool": "write_file", "ok": True, "output": "ok"}
            )
            agent._append_session_event("assistant_message", {"content": "已完成"})
            agent._complete_turn_snapshot(snapshot)
            agent._history = [
                {"role": "user", "content": "写文件"},
                {"role": "assistant", "content": "已完成"},
            ]

            agent.undo_last_turn()
            self.assertEqual(
                workspace_file.read_text(encoding="utf-8"), "before\n"
            )

    def test_concurrent_write_tools_capture_once(self) -> None:
        """并发写工具同时到达时，起点只捕获一次（线程安全单飞）。"""

        with tempfile.TemporaryDirectory() as temp_dir:
            agent, _workspace_file = self._build_agent(Path(temp_dir))
            agent._append_session_event("user_message", {"content": "并发写"})
            snapshot = agent._begin_turn_snapshot()
            self.assertIsNotNone(snapshot)
            # 用线程模拟同一批次的多个并发写工具。
            import threading

            barrier = threading.Barrier(4)
            errors: list[Exception] = []

            def record(_name: str) -> None:
                try:
                    barrier.wait(timeout=5)
                    agent._record_turn_tool_execution(
                        snapshot, ToolCall(name="write_file")
                    )
                except Exception as exc:  # noqa: BLE001
                    errors.append(exc)

            threads = [
                threading.Thread(target=record, args=(f"w{i}",)) for i in range(4)
            ]
            for thread in threads:
                thread.start()
            for thread in threads:
                thread.join(timeout=10)
            self.assertEqual(errors, [])
            # 捕获恰好一次：before 非 None 且 capture_attempted 只置位一次
            # （真实 git 子进程调用次数由 store.capture 保证，这里验证状态）。
            self.assertIsNotNone(snapshot.before)
            self.assertTrue(snapshot.capture_attempted)
            self.assertFalse(snapshot.capture_failed)

    @staticmethod
    def _build_agent(base: Path) -> tuple[LocalToolAgent, Path]:
        workspace = base / "workspace"
        workspace.mkdir(parents=True)
        _init_git(workspace)
        workspace_file = workspace / "app.txt"
        workspace_file.write_text("before\n", encoding="utf-8")
        subprocess.run(["git", "add", "app.txt"], cwd=workspace, check=True)
        subprocess.run(["git", "commit", "-qm", "initial"], cwd=workspace, check=True)
        # 会话数据放在工作区外（生产环境在 ~/.omnicrawl），避免 undo 补丁
        # 目录自身被当作未跟踪文件捕获。
        session_store = SessionStore(base / ".agent_sessions")
        state = session_store.start_session(workspace)

        agent = object.__new__(LocalToolAgent)
        agent.workspace_root = workspace
        agent.config = SimpleNamespace(max_history_turns=6)
        agent._session_store = session_store
        agent._session_state = state
        agent._project_memory_store = MemoryStore(base / ".omnicrawl" / ".oclmemory")
        agent._session_memory_store = MemoryStore(
            base / "Session_memory" / state.session_id
        )
        agent._user_memory_store = MemoryStore(base / "User_memory")
        agent._memory_store = agent._project_memory_store
        agent._history = []
        agent._pending_user_text = None
        agent._active_skills = []
        return agent, workspace_file


if __name__ == "__main__":
    unittest.main()
