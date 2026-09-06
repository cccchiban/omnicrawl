"""主 Agent 隔离工作区（agent_isolation）回归测试。

重点覆盖 ``sync_uncommitted``：创建隔离 worktree 时应把主工作区未提交改动
带入隔离区。回归背景：Windows 上 patch 文件若被文本模式写成 CRLF，
``git apply`` 会对所有文件报 ``patch does not apply``，导致同步静默失败
（此前只有未跟踪文件被复制、基线 commit 只含新增文件）。

后续补充的回归点：
- ``mode=local``：真正复制主工作区为普通目录（排除 .git），退出时镜像回主工作区；
- 收尾清理：apply 成功后隔离区作为冗余副本立即回收（跳过保留期/未推送拦截），
  apply 失败或关闭自动应用时走完整门禁，宁可保留也不丢数据；
- 启动清扫：崩溃/被强杀遗留的过期孤儿隔离区按元数据 apply 回写并回收。
"""
from __future__ import annotations

import os
import subprocess
import tempfile
import time
import unittest
from pathlib import Path
from types import SimpleNamespace

import json

from omnicrawl.config.features.agent_workspace import AgentWorkspaceConfig
from omnicrawl.workspace.agent_isolation import (
    AgentIsolationError,
    apply_isolation_changes,
    cleanup_eligible,
    cleanup_isolation_session,
    create_isolation_session,
    finalize_isolation_session,
    sweep_expired_isolation_sessions,
)


def _run_git(args: list[str], cwd: Path) -> None:
    c = subprocess.run(
        ["git", *args], cwd=str(cwd), capture_output=True, text=True, check=False
    )
    if c.returncode != 0:
        raise RuntimeError(c.stderr or c.stdout or "git failed")


def _init_repo(path: Path) -> None:
    """初始化测试仓库：.gitattributes 统一 LF（与真实项目一致），

    否则 Windows 上 autocrlf=true 会让 worktree 检出 CRLF，混淆测试焦点。
    """
    path.mkdir(parents=True, exist_ok=True)
    _run_git(["init"], path)
    _run_git(["config", "user.email", "test@example.com"], path)
    _run_git(["config", "user.name", "Test"], path)
    (path / ".gitattributes").write_text("* text eol=lf\n", encoding="utf-8")
    (path / "README.md").write_text("hello\n", encoding="utf-8")
    _run_git(["add", ".gitattributes", "README.md"], path)
    _run_git(["commit", "-m", "init"], path)


def _init_bare_origin(root: Path, name: str = "orig") -> Path:
    """初始化带 origin 远端的仓库并返回克隆出的工作仓库。

    带 origin 的仓库才会触发清理门禁第四层（未推送远端 commit），
    用于验证四层门禁中「未推送远端 commit 不删」的判定。
    """
    bare = root / "bare.git"
    bare.mkdir(parents=True)
    _run_git(["init", "--bare", "-q"], bare)
    orig = root / name
    orig.mkdir(parents=True)
    _init_repo(root / name)
    _run_git(["remote", "add", "origin", str(bare)], orig)
    _run_git(["push", "-q", "-u", "origin", "HEAD"], orig)
    repo = root / "repo"
    _run_git(["clone", "-q", str(bare), str(repo)], root)
    return repo


class SyncUncommittedTests(unittest.TestCase):
    def test_sync_uncommitted_brings_modified_and_untracked_files(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            repo = Path(tmp) / "repo"
            _init_repo(repo)

            # 主工作区：一个已跟踪文件的未提交修改 + 一个未跟踪新文件
            (repo / "README.md").write_text("hello\nchanged\n", encoding="utf-8")
            (repo / "notes.txt").write_text("untracked\n", encoding="utf-8")

            session = create_isolation_session(
                main_workspace=repo,
                instance_id="sync-test",
                config=AgentWorkspaceConfig(base_ref="HEAD", sync_uncommitted=True),
                worktrees_root=Path(tmp) / "wts",
            )

            readme = (session.worktree_path / "README.md").read_text(encoding="utf-8")
            self.assertIn("changed", readme)
            notes = (session.worktree_path / "notes.txt").read_text(encoding="utf-8")
            self.assertEqual(notes, "untracked\n")

    def test_sync_uncommitted_disabled_leaves_worktree_at_base(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            repo = Path(tmp) / "repo"
            _init_repo(repo)

            (repo / "README.md").write_text("hello\nchanged\n", encoding="utf-8")

            session = create_isolation_session(
                main_workspace=repo,
                instance_id="sync-off",
                config=AgentWorkspaceConfig(base_ref="HEAD", sync_uncommitted=False),
                worktrees_root=Path(tmp) / "wts",
            )

            readme = (session.worktree_path / "README.md").read_text(encoding="utf-8")
            self.assertEqual(readme, "hello\n")


class LocalModeTests(unittest.TestCase):
    def test_local_mode_copies_workspace_and_mirrors_back(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            workspace = Path(tmp) / "workspace"
            (workspace / "sub").mkdir(parents=True)
            (workspace / "a.txt").write_text("one\n", encoding="utf-8")
            (workspace / "sub" / "b.txt").write_text("two\n", encoding="utf-8")

            session = create_isolation_session(
                main_workspace=workspace,
                instance_id="local1",
                config=AgentWorkspaceConfig(mode="local"),
                worktrees_root=Path(tmp) / "wts",
            )

            # 隔离区是完整复制，且不包含 .git（普通目录，不依赖 git）
            self.assertTrue((session.worktree_path / "a.txt").exists())
            self.assertTrue((session.worktree_path / "sub" / "b.txt").exists())
            self.assertFalse((session.worktree_path / ".git").exists())

            # Agent 在隔离区里修改 + 新建文件
            (session.worktree_path / "a.txt").write_text("one\nchanged\n", encoding="utf-8")
            (session.worktree_path / "new.txt").write_text("x\n", encoding="utf-8")

            # 镜像回写
            changed, conflicts = apply_isolation_changes(session)
            self.assertEqual(conflicts, [])
            self.assertGreaterEqual(changed, 2)
            self.assertIn(
                "changed", (workspace / "a.txt").read_text(encoding="utf-8")
            )
            self.assertTrue((workspace / "new.txt").exists())

            # local 模式无 git 层检查；过保留期后四层门禁通过即可回收
            from dataclasses import replace

            expired = replace(session, created_at=time.time() - 7200)
            removed, _ = cleanup_isolation_session(expired)
            self.assertTrue(removed)
            self.assertFalse(session.worktree_path.exists())

    def test_local_mode_cleanup_without_apply_respects_gate(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            workspace = Path(tmp) / "workspace"
            workspace.mkdir(parents=True)
            (workspace / "a.txt").write_text("one\n", encoding="utf-8")

            session = create_isolation_session(
                main_workspace=workspace,
                instance_id="local2",
                config=AgentWorkspaceConfig(mode="local"),
                worktrees_root=Path(tmp) / "wts",
            )
            # 未 apply（例如关闭自动应用）时不能直接回收，否则 Agent 产物丢失：
            # 走完整门禁，保留期内拒绝删除。
            eligible, reason = cleanup_eligible(session, now=time.time())
            self.assertFalse(eligible)
            self.assertIn("保留期", reason)


class FinalizeCleanupTests(unittest.TestCase):
    def test_finalize_applies_but_keeps_with_unpushed_commits(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            repo = _init_bare_origin(Path(tmp))
            # 主工作区脏 → 隔离区会留下内部的 omnicrawl-sync 基线 commit
            (repo / "README.md").write_text("hello\nuser-change\n", encoding="utf-8")

            session = create_isolation_session(
                main_workspace=repo,
                instance_id="fin1",
                config=AgentWorkspaceConfig(base_ref="HEAD", sync_uncommitted=True),
                worktrees_root=Path(tmp) / "wts",
            )
            (session.worktree_path / "README.md").write_text(
                "hello\nuser-change\nagent-edit\n", encoding="utf-8"
            )
            (session.worktree_path / "NEW.txt").write_text("x\n", encoding="utf-8")

            summary = finalize_isolation_session(
                session, apply_on_exit=True, cleanup_on_exit="auto"
            )

            # 变更已应用回主工作区
            self.assertIn("agent-edit", (repo / "README.md").read_text(encoding="utf-8"))
            self.assertTrue((repo / "NEW.txt").exists())
            # 第四层门禁：隔离区存在未推送远端的 commit（omnicrawl-sync /
            # omnicrawl-agent）→ 即使已应用回主工作区也保留，不自动删除
            self.assertTrue(session.worktree_path.exists())
            self.assertIn("已应用", summary)
            self.assertIn("隔离区保留", summary)

    def test_finalize_cleans_when_all_four_layers_pass(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            repo = _init_bare_origin(Path(tmp))
            session = create_isolation_session(
                main_workspace=repo,
                instance_id="fin-gate",
                config=AgentWorkspaceConfig(
                    base_ref="HEAD", sync_uncommitted=False, apply_on_exit=True
                ),
                worktrees_root=Path(tmp) / "wts",
            )
            # 已过期（模拟长会话退出，保留期已过且无任何提交）
            from dataclasses import replace

            session = replace(session, created_at=time.time() - 7200)
            summary = finalize_isolation_session(
                session, apply_on_exit=True, cleanup_on_exit="auto"
            )
            self.assertFalse(session.worktree_path.exists())
            self.assertIn("已清理", summary)

    def test_finalize_keeps_when_apply_disabled(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            repo = _init_bare_origin(Path(tmp))
            (repo / "README.md").write_text("hello\nuser-change\n", encoding="utf-8")

            session = create_isolation_session(
                main_workspace=repo,
                instance_id="fin2",
                config=AgentWorkspaceConfig(base_ref="HEAD", sync_uncommitted=True),
                worktrees_root=Path(tmp) / "wts",
            )
            (session.worktree_path / "AGENT.md").write_text("agent-only\n", encoding="utf-8")

            summary = finalize_isolation_session(
                session, apply_on_exit=False, cleanup_on_exit="auto"
            )

            # 关闭自动应用时不要回写，也不要删除唯一副本
            self.assertTrue(session.worktree_path.exists())
            self.assertIn("隔离区保留", summary)
            self.assertFalse((repo / "AGENT.md").exists())
            self.assertTrue((session.worktree_path / "AGENT.md").exists())

    def test_finalize_respects_keep_policy(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            repo = _init_bare_origin(Path(tmp))
            session = create_isolation_session(
                main_workspace=repo,
                instance_id="fin3",
                config=AgentWorkspaceConfig(base_ref="HEAD"),
                worktrees_root=Path(tmp) / "wts",
            )
            (session.worktree_path / "NEW.txt").write_text("x\n", encoding="utf-8")

            summary = finalize_isolation_session(
                session, apply_on_exit=True, cleanup_on_exit="keep"
            )

            self.assertTrue((repo / "NEW.txt").exists())
            self.assertTrue(session.worktree_path.exists())
            self.assertIn("已保留", summary)

    def test_cleanup_gate_requires_all_four_layers(self) -> None:
        from dataclasses import replace

        with tempfile.TemporaryDirectory() as tmp:
            repo = _init_bare_origin(Path(tmp))
            session = create_isolation_session(
                main_workspace=repo,
                instance_id="gate1",
                config=AgentWorkspaceConfig(base_ref="HEAD", sync_uncommitted=False),
                worktrees_root=Path(tmp) / "wts",
            )
            # 第二层：刚创建未过期 → 拒绝
            eligible, reason = cleanup_eligible(session, now=time.time())
            self.assertFalse(eligible)
            self.assertIn("保留期", reason)

            # 第二层过期 + 第三/四层通过（干净、无提交）→ 可安全清理
            eligible, reason = cleanup_eligible(
                session, now=time.time() + 7200
            )
            self.assertTrue(eligible, reason)
            expired = replace(session, created_at=time.time() - 7200)
            removed, _ = cleanup_isolation_session(expired)
            self.assertTrue(removed)

    def test_cleanup_gate_layer4_blocks_unpushed_commits(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            repo = _init_bare_origin(Path(tmp))
            # 主工作区脏 → 同步产生未推送的内部基线 commit
            (repo / "README.md").write_text("hello\ndirty\n", encoding="utf-8")
            session = create_isolation_session(
                main_workspace=repo,
                instance_id="gate2",
                config=AgentWorkspaceConfig(base_ref="HEAD", sync_uncommitted=True),
                worktrees_root=Path(tmp) / "wts",
            )
            # 已过期也拒绝：第四层，存在未推送远端 commit
            eligible, reason = cleanup_eligible(session, now=time.time() + 7200)
            self.assertFalse(eligible)
            self.assertIn("未推送", reason)


class SlugSafetyIntegrationTests(unittest.TestCase):
    """instance_id → 隔离区路径 的 slug 安全验证（路径遍历防护）。"""

    def test_create_rejects_path_traversal_instance_ids(self) -> None:
        payloads = ("../../evil", "..\\evil", "a/b", "a b", "..", "a.b", "aw-../../evil")
        for bad in payloads:
            with tempfile.TemporaryDirectory() as tmp:
                repo = Path(tmp) / "repo"
                _init_repo(repo)
                with self.assertRaises(AgentIsolationError):
                    create_isolation_session(
                        main_workspace=repo,
                        instance_id=bad,
                        config=AgentWorkspaceConfig(base_ref="HEAD"),
                        worktrees_root=Path(tmp) / "wts",
                    )
                # 未在根目录外部创建任何目录
                self.assertFalse((Path(tmp) / "evil").exists())
                self.assertFalse((Path(tmp) / "repo" / "evil").exists())

    def test_sweep_skips_tampered_metadata_path(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp) / "wts"
            root.mkdir()
            # 元数据把 worktree_path 指向隔离区根目录之外的真实目录
            outside = Path(tmp) / "outside"
            outside.mkdir()
            (outside / "marker.txt").write_text("keep me\n", encoding="utf-8")
            (root / "aw-evil").mkdir()
            payload = {
                "instance_id": "evil",
                "mode": "worktree",
                "repo_root": str(outside),
                "main_workspace": str(outside),
                "worktree_path": str(outside),
                "base_ref": "HEAD",
                "created_at": time.time() - 8 * 24 * 3600,
                "apply_on_exit": True,
                "cleanup_on_exit": "auto",
            }
            (root / "aw-evil.json").write_text(
                json.dumps(payload, ensure_ascii=False), encoding="utf-8"
            )

            future = time.time()
            result = sweep_expired_isolation_sessions(
                worktrees_root=root,
                now=future,
            )

            self.assertEqual(result.removed, ())
            self.assertEqual(result.applied, ())
            self.assertIn("evil", dict(result.kept))
            # 外部目录未被 rmtree / git worktree remove 触碰
            self.assertTrue((outside / "marker.txt").read_text(encoding="utf-8") == "keep me\n")


class AgentAttachTests(unittest.TestCase):
    """验证 TUI/API/连接器共用收尾路径：attach 到 Agent 后 close() 自动收尾。

    回归背景：``_finalize_attached_isolation`` 里相对导入写少了一层（..）
    会静默导入失败并吞掉异常，导致隔离区既不回写也不清理。
    """

    def _attach_stub(self, session, config):
        from omnicrawl.agent.controllers.session.control import SessionControlMixin

        summary_calls: list[str] = []
        stub = SimpleNamespace(
            config=SimpleNamespace(agent_workspace=config),
        )
        stub._isolation_session = session
        stub._isolation_on_finalized = summary_calls.append
        SessionControlMixin._finalize_attached_isolation(stub)
        return summary_calls

    def test_attached_session_finalizes_on_close(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            repo = _init_bare_origin(Path(tmp))
            (repo / "README.md").write_text("hello\nuser-change\n", encoding="utf-8")

            session = create_isolation_session(
                main_workspace=repo,
                instance_id="attach1",
                config=AgentWorkspaceConfig(base_ref="HEAD", sync_uncommitted=True),
                worktrees_root=Path(tmp) / "wts",
            )
            (session.worktree_path / "README.md").write_text(
                "hello\nuser-change\nagent-edit\n", encoding="utf-8"
            )
            (session.worktree_path / "NEW.txt").write_text("x\n", encoding="utf-8")

            summaries = self._attach_stub(
                session, SimpleNamespace(apply_on_exit=True, cleanup_on_exit="auto")
            )

            self.assertTrue(summaries)
            self.assertIn("已应用", summaries[0])
            self.assertIn("agent-edit", (repo / "README.md").read_text(encoding="utf-8"))
            self.assertTrue((repo / "NEW.txt").exists())
            # 第四层门禁：存在未推送远端 commit → 保留，不自动删除
            self.assertTrue(session.worktree_path.exists())
            self.assertIn("隔离区保留", summaries[0])

    def test_attached_session_respects_disabled_apply(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            repo = _init_bare_origin(Path(tmp))
            session = create_isolation_session(
                main_workspace=repo,
                instance_id="attach2",
                config=AgentWorkspaceConfig(base_ref="HEAD", apply_on_exit=False),
                worktrees_root=Path(tmp) / "wts",
            )
            (session.worktree_path / "KEEP.txt").write_text("z\n", encoding="utf-8")

            summaries = self._attach_stub(
                session, SimpleNamespace(apply_on_exit=False, cleanup_on_exit="auto")
            )

            self.assertTrue(summaries)
            self.assertFalse((repo / "KEEP.txt").exists())
            self.assertTrue(session.worktree_path.exists())


class SweepTests(unittest.TestCase):
    def test_sweep_applies_and_keeps_orphan_with_unpushed_commits(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            repo = _init_bare_origin(Path(tmp))
            (repo / "README.md").write_text("hello\nuser-change\n", encoding="utf-8")

            session = create_isolation_session(
                main_workspace=repo,
                instance_id="orphan1",
                config=AgentWorkspaceConfig(base_ref="HEAD", sync_uncommitted=True),
                worktrees_root=Path(tmp) / "wts",
            )
            (session.worktree_path / "NEW.txt").write_text("x\n", encoding="utf-8")

            # 模拟进程崩溃：不调用 finalize，让清扫按「超过保留期」回收
            future = time.time() + 8 * 24 * 3600
            result = sweep_expired_isolation_sessions(
                worktrees_root=Path(tmp) / "wts",
                now=future,
            )

            # 延迟收尾已把变更应用回主工作区，但第四层门禁（未推送远端 commit）
            # 仍然拦截删除——未推送提交可能是有价值的唯一副本
            self.assertIn("orphan1", result.applied)
            self.assertEqual(result.removed, ())
            self.assertTrue((repo / "NEW.txt").exists())
            self.assertTrue(session.worktree_path.exists())

    def test_sweep_cleans_orphan_passing_all_four_layers(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            repo = _init_bare_origin(Path(tmp))
            # 干净主工作区 + sync 关闭 + 无任何 Agent 改动：无提交、无改动
            session = create_isolation_session(
                main_workspace=repo,
                instance_id="orphan2",
                config=AgentWorkspaceConfig(
                    base_ref="HEAD", sync_uncommitted=False, apply_on_exit=True
                ),
                worktrees_root=Path(tmp) / "wts",
            )

            future = time.time() + 8 * 24 * 3600
            result = sweep_expired_isolation_sessions(
                worktrees_root=Path(tmp) / "wts",
                now=future,
            )

            self.assertIn("orphan2", result.removed)
            self.assertFalse(session.worktree_path.exists())

    def test_sweep_skips_fresh_sessions(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            repo = _init_bare_origin(Path(tmp))

            fresh = create_isolation_session(
                main_workspace=repo,
                instance_id="fresh1",
                config=AgentWorkspaceConfig(base_ref="HEAD"),
                worktrees_root=Path(tmp) / "wts",
            )

            # 实时清扫：刚创建的隔离区未过清扫保留期，不回收
            result = sweep_expired_isolation_sessions(
                worktrees_root=Path(tmp) / "wts",
                now=time.time(),
            )

            self.assertEqual(result.applied, ())
            self.assertEqual(result.removed, ())
            self.assertTrue(fresh.worktree_path.exists())
            reasons = dict(result.kept)
            self.assertIn("fresh1", reasons)
            self.assertIn("保留期", reasons["fresh1"])

    def test_sweep_skips_non_auto_policy_sessions(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            repo = _init_bare_origin(Path(tmp))

            kept = create_isolation_session(
                main_workspace=repo,
                instance_id="kept1",
                config=AgentWorkspaceConfig(
                    base_ref="HEAD", apply_on_exit=True, cleanup_on_exit="keep"
                ),
                worktrees_root=Path(tmp) / "wts",
            )

            # 即使过期，cleanup_on_exit=keep 的会话也跳过清扫
            future = time.time() + 8 * 24 * 3600
            result = sweep_expired_isolation_sessions(
                worktrees_root=Path(tmp) / "wts",
                now=future,
            )

            self.assertEqual(result.applied, ())
            self.assertEqual(result.removed, ())
            self.assertTrue(kept.worktree_path.exists())
            reasons = dict(result.kept)
            self.assertIn("kept1", reasons)
            self.assertIn("cleanup_on_exit=keep", reasons["kept1"])


if __name__ == "__main__":
    unittest.main()