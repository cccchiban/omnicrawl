from __future__ import annotations

import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path

from omnicrawl.agent.toolkit.git_tools import git_result
from omnicrawl.agent.types import ToolResult


def _git(root: Path, *arguments: str) -> str:
    return subprocess.run(
        ["git", *arguments],
        cwd=root,
        check=True,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    ).stdout


def _init_git(root: Path) -> None:
    subprocess.run(["git", "init", "-q"], cwd=root, check=True)
    subprocess.run(
        ["git", "config", "user.name", "OmniCrawl Test"], cwd=root, check=True
    )
    subprocess.run(
        ["git", "config", "user.email", "test@example.invalid"], cwd=root, check=True
    )


@unittest.skipIf(shutil.which("git") is None, "git 不在 PATH 中")
class GitToolTest(unittest.TestCase):
    def _workspace(self) -> Path:
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        root = Path(temp.name) / "workspace"
        root.mkdir()
        _init_git(root)
        return root

    def test_status_readonly(self) -> None:
        root = self._workspace()
        (root / "a.txt").write_text("a\n", encoding="utf-8")

        result = git_result(root, {"action": "status", "args": ["--short"]})

        self.assertTrue(result.ok)
        self.assertIn("?? a.txt", result.output)

    def test_add_and_commit_with_message(self) -> None:
        root = self._workspace()
        (root / "a.txt").write_text("a\n", encoding="utf-8")

        added = git_result(root, {"action": "add", "paths": ["a.txt"]})
        self.assertTrue(added.ok, added.output)

        committed = git_result(
            root, {"action": "commit", "message": "initial commit"}
        )
        self.assertTrue(committed.ok, committed.output)

        log = git_result(root, {"action": "log", "args": ["--oneline"]})
        self.assertTrue(log.ok)
        self.assertIn("initial commit", log.output)

    def test_commit_without_message_rejected(self) -> None:
        root = self._workspace()

        result = git_result(root, {"action": "commit"})

        self.assertFalse(result.ok)
        self.assertIn("message", result.output)

    def test_forbidden_arguments_rejected(self) -> None:
        root = self._workspace()
        for args in (["--git-dir=/etc/git"], ["--work-tree=/tmp"], ["--no-verify"]):
            result = git_result(root, {"action": "commit", "args": args})
            self.assertFalse(result.ok, args)
            self.assertIn("不允许参数", result.output, args)

    def test_config_global_rejected(self) -> None:
        root = self._workspace()

        result = git_result(
            root,
            {"action": "config", "args": ["--global", "user.name", "x"]},
        )

        self.assertFalse(result.ok)
        self.assertIn("全局/系统", result.output)

    def test_archive_output_rejected(self) -> None:
        root = self._workspace()
        (root / "a.txt").write_text("a\n", encoding="utf-8")
        _git(root, "add", "a.txt")
        _git(root, "commit", "-qm", "initial")

        result = git_result(root, {"action": "archive", "args": ["-o", "out.tar", "HEAD"]})

        self.assertFalse(result.ok)
        self.assertIn("archive", result.output)

    def test_paths_cannot_escape_workspace(self) -> None:
        root = self._workspace()

        result = git_result(
            root,
            {"action": "add", "paths": ["../../etc/passwd"]},
        )

        self.assertFalse(result.ok)
        self.assertIn("越界", result.output)

    def test_clone_target_outside_workspace_rejected(self) -> None:
        root = self._workspace()

        result = git_result(
            root,
            {"action": "clone", "args": ["https://example.invalid/repo.git", "../outside"]},
        )

        self.assertFalse(result.ok)
        self.assertIn("越界", result.output)

    def test_non_git_workspace_returns_failure(self) -> None:
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        root = Path(temp.name) / "plain"
        root.mkdir()
        (root / "x.txt").write_text("x\n", encoding="utf-8")

        result = git_result(root, {"action": "status"})

        self.assertFalse(result.ok)
        self.assertIn("git", result.output.casefold())

    def test_unknown_action_rejected(self) -> None:
        root = self._workspace()

        result = git_result(root, {"action": "pushd"})

        self.assertFalse(result.ok)
        self.assertIn("不支持的 git 子命令", result.output)

    def test_long_output_is_bounded(self) -> None:
        root = self._workspace()
        (root / "big.txt").write_text("line\n" * 20_000, encoding="utf-8")
        _git(root, "add", "big.txt")
        _git(root, "commit", "-qm", "big file")

        result = git_result(root, {"action": "show", "args": ["HEAD"]})

        self.assertTrue(result.ok)
        self.assertIn("已截断", result.output)
        self.assertLess(len(result.output), 20_000)

    def test_branch_create_local_mutation(self) -> None:
        root = self._workspace()
        _git(root, "commit", "--allow-empty", "-qm", "initial")

        result = git_result(root, {"action": "branch", "args": ["feature"]})

        self.assertTrue(result.ok, result.output)
        branches = git_result(root, {"action": "branch"})
        self.assertIn("feature", branches.output)

    def test_stash_round_trip(self) -> None:
        root = self._workspace()
        (root / "a.txt").write_text("a\n", encoding="utf-8")
        _git(root, "add", "a.txt")
        _git(root, "commit", "-qm", "initial")
        (root / "a.txt").write_text("changed\n", encoding="utf-8")

        pushed = git_result(root, {"action": "stash", "args": ["push"]})
        self.assertTrue(pushed.ok, pushed.output)

        popped = git_result(root, {"action": "stash", "args": ["pop"]})
        self.assertTrue(popped.ok, popped.output)
        self.assertEqual(
            (root / "a.txt").read_text(encoding="utf-8"), "changed\n"
        )

    def test_output_is_plain_tool_result(self) -> None:
        self.assertIsInstance(git_result(self._workspace(), {"action": "status"}), ToolResult)


if __name__ == "__main__":
    unittest.main()
