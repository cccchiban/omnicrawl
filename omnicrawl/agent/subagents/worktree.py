"""SubAgent git worktree 隔离生命周期。

该模块只负责：
1. 在父工作区所在 git 仓库中创建临时 worktree
2. 收集 diff / 分支 / 变更文件等父 Agent 可审查的产物
3. 清理 worktree 目录与本地分支
4. 在父 Agent 显式请求时，把分支变更 apply 回主工作区

是否应用结果由父 Agent 通过 ``apply_subagent_worktree`` 一类的显式动作决定。
创建与 apply 前都会检查主工作区是否干净，避免在脏主树上静默覆盖用户未提交改动。
"""

from __future__ import annotations

import logging
import re
import shutil
import subprocess
import uuid
from dataclasses import dataclass
from pathlib import Path

LOGGER = logging.getLogger(__name__)

_BRANCH_SAFE_RE = re.compile(r"[^a-zA-Z0-9._/-]+")
_MAX_DIFF_CHARS = 200_000
_MAX_CHANGED_FILES = 200


class WorktreeError(RuntimeError):
    """worktree 创建、查询或清理失败时的稳定错误。"""


@dataclass(frozen=True)
class WorktreeSession:
    """一次 SubAgent 任务绑定的 worktree 会话。"""

    task_id: str
    repo_root: Path
    worktree_path: Path
    branch_name: str
    base_ref: str


@dataclass(frozen=True)
class WorktreeArtifacts:
    """父 Agent 审查 / 应用前可见的 worktree 产物摘要。"""

    branch_name: str
    worktree_path: str
    base_ref: str
    changed_files: tuple[str, ...]
    diff_stat: str
    diff_text: str
    has_changes: bool


def is_git_repository(path: Path) -> bool:
    """判断路径是否位于 git 工作树中。"""

    result = _run_git(["rev-parse", "--is-inside-work-tree"], cwd=path, check=False)
    return result.returncode == 0 and result.stdout.strip() == "true"


def resolve_repo_root(path: Path) -> Path:
    """解析 path 所属的 git 仓库根目录。"""

    result = _run_git(["rev-parse", "--show-toplevel"], cwd=path, check=True)
    return Path(result.stdout.strip()).resolve()



def main_tree_is_clean(repo_root: Path) -> bool:
    """主工作区工作树与暂存区是否干净。"""

    completed = _run_git(["status", "--porcelain"], cwd=repo_root, check=True)
    return not (completed.stdout or "").strip()


def require_clean_main_tree(repo_root: Path) -> None:
    """主工作区脏时拒绝 create/apply，避免静默覆盖或无法回滚。

    设计约束（单写者 / 脏树门禁）：
    - 不得在主树有未提交变更时静默创建 worktree；
    - 不得在主树脏时静默 checkout/merge 子 Agent 结果；
    - 只检查主仓库工作区与暂存区，不检查目标 worktree 目录本身。
    """

    completed = _run_git(["status", "--porcelain"], cwd=repo_root, check=True)
    dirty = (completed.stdout or "").strip()
    if not dirty:
        return
    preview_lines = dirty.splitlines()[:8]
    preview_text = "; ".join(preview_lines)
    if len(dirty.splitlines()) > 8:
        preview_text += "; ..."
    raise WorktreeError(
        "主工作区存在未提交变更，禁止静默创建或应用 worktree。"
        f"请先提交、暂存或清理后再操作。脏项预览：{preview_text}"
    )


def create_worktree_session(
    *,
    workspace_root: Path,
    task_id: str,
    base_ref: str = "HEAD",
    worktree_parent: Path | None = None,
) -> WorktreeSession:
    """为 task 创建独立 worktree 与本地分支。

    约束：
    - 父工作区必须在 git 仓库内
    - 主工作区必须干净（无未提交变更）
    - worktree 目录放在仓库外的临时父目录，避免污染源树
    - 分支名带 task_id，便于父 Agent 审查和清理
    """

    root = Path(workspace_root).expanduser().resolve()
    if not is_git_repository(root):
        raise WorktreeError("当前工作区不是 git 仓库，无法启用 isolation=worktree。")

    repo_root = resolve_repo_root(root)
    # 脏主树禁止创建：避免后续 apply 时与用户未提交改动互相覆盖。
    require_clean_main_tree(repo_root)
    safe_task = _sanitize_branch_fragment(task_id) or uuid.uuid4().hex[:12]
    branch_name = f"omnicrawl/subagent/{safe_task}"
    parent_dir = (
        Path(worktree_parent).expanduser().resolve()
        if worktree_parent is not None
        else (repo_root.parent / ".omnicrawl-worktrees")
    )
    parent_dir.mkdir(parents=True, exist_ok=True)
    worktree_path = parent_dir / f"wt-{safe_task}-{uuid.uuid4().hex[:8]}"

    # 先解析 base_ref，避免 git worktree 在错误 ref 上创建半成品目录。
    resolved_base = _run_git(
        ["rev-parse", "--verify", base_ref],
        cwd=repo_root,
        check=True,
    ).stdout.strip()

    try:
        _run_git(
            [
                "worktree",
                "add",
                "-b",
                branch_name,
                str(worktree_path),
                resolved_base,
            ],
            cwd=repo_root,
            check=True,
        )
    except WorktreeError:
        # 创建失败时尽量回收可能残留的目录，避免下次撞名。
        if worktree_path.exists():
            shutil.rmtree(worktree_path, ignore_errors=True)
        raise

    return WorktreeSession(
        task_id=task_id,
        repo_root=repo_root,
        worktree_path=worktree_path.resolve(),
        branch_name=branch_name,
        base_ref=resolved_base,
    )


def collect_worktree_artifacts(session: WorktreeSession) -> WorktreeArtifacts:
    """收集 worktree 相对 base_ref 的变更摘要。"""

    if not session.worktree_path.exists():
        raise WorktreeError(f"worktree 目录不存在：{session.worktree_path}")

    # 包含未暂存与未跟踪文件，避免只看 HEAD 时漏掉子任务写入。
    # 若有变更则提交到 worktree 分支，这样父 Agent apply 时能从分支检出文件。
    _run_git(["add", "-A"], cwd=session.worktree_path, check=False)
    status = _run_git(
        ["status", "--porcelain"],
        cwd=session.worktree_path,
        check=True,
    ).stdout
    if status.strip():
        _run_git(
            [
                "commit",
                "-m",
                f"omnicrawl-subagent:{session.task_id}",
                "--allow-empty-message",
            ],
            cwd=session.worktree_path,
            check=False,
        )
    status = _run_git(
        ["diff", "--name-only", session.base_ref, "HEAD"],
        cwd=session.worktree_path,
        check=False,
    ).stdout
    changed_files = tuple(
        line.strip()
        for line in status.splitlines()
        if line.strip()
    )[:_MAX_CHANGED_FILES]
    diff_stat = _run_git(
        ["diff", "--stat", session.base_ref, "HEAD"],
        cwd=session.worktree_path,
        check=False,
    ).stdout.strip()
    diff_text = _run_git(
        ["diff", session.base_ref, "HEAD"],
        cwd=session.worktree_path,
        check=False,
    ).stdout
    if len(diff_text) > _MAX_DIFF_CHARS:
        diff_text = (
            diff_text[:_MAX_DIFF_CHARS]
            + "\n... diff 已截断，完整内容请在 worktree 分支上查看。"
        )
    has_changes = bool(changed_files) or bool(diff_text.strip())
    return WorktreeArtifacts(
        branch_name=session.branch_name,
        worktree_path=str(session.worktree_path),
        base_ref=session.base_ref,
        changed_files=changed_files,
        diff_stat=diff_stat,
        diff_text=diff_text,
        has_changes=has_changes,
    )


def apply_worktree_to_main(
    session: WorktreeSession,
    *,
    strategy: str = "checkout",
) -> str:
    """把 worktree 分支上的变更应用到主工作区。

    默认使用 ``git checkout <branch> -- .``，只取文件内容，不切换当前分支。
    主工作区必须干净，避免覆盖用户未提交改动。
    """

    if strategy not in {"checkout", "merge"}:
        raise WorktreeError(f"不支持的 apply strategy：{strategy}")

    # apply 前再次检查主树：collect 只处理 worktree，不会保护主工作区脏状态。
    require_clean_main_tree(session.repo_root)
    artifacts = collect_worktree_artifacts(session)
    if not artifacts.has_changes:
        return "worktree 无变更，无需应用。"

    if strategy == "checkout":
        # 先确保分支上有最新提交（collect 会 commit；若调用方未 collect 再补一次）。
        collect_worktree_artifacts(session)
        _run_git(
            ["checkout", session.branch_name, "--", "."],
            cwd=session.repo_root,
            check=True,
        )
        return (
            f"已将分支 {session.branch_name} 的文件变更检出到主工作区。"
            f" 变更文件数：{len(artifacts.changed_files)}。"
        )

    _run_git(
        ["merge", "--no-ff", "--no-edit", session.branch_name],
        cwd=session.repo_root,
        check=True,
    )
    return f"已将分支 {session.branch_name} merge 到主工作区当前分支。"


def cleanup_worktree_session(
    session: WorktreeSession,
    *,
    remove_branch: bool = True,
) -> None:
    """清理 worktree 目录，并可选删除本地分支。"""

    # 先尝试 git worktree remove；失败时回退到目录删除。
    # Windows 临时目录可能在测试 teardown 时已失效，所有 git 调用都降级吞掉。
    try:
        if session.repo_root.exists():
            remove = _run_git(
                ["worktree", "remove", "--force", str(session.worktree_path)],
                cwd=session.repo_root,
                check=False,
            )
            if remove.returncode != 0 and session.worktree_path.exists():
                shutil.rmtree(session.worktree_path, ignore_errors=True)
                _run_git(["worktree", "prune"], cwd=session.repo_root, check=False)
            if remove_branch:
                _run_git(
                    ["branch", "-D", session.branch_name],
                    cwd=session.repo_root,
                    check=False,
                )
        elif session.worktree_path.exists():
            shutil.rmtree(session.worktree_path, ignore_errors=True)
    except WorktreeError:
        if session.worktree_path.exists():
            shutil.rmtree(session.worktree_path, ignore_errors=True)


def _sanitize_branch_fragment(value: str) -> str:
    cleaned = _BRANCH_SAFE_RE.sub("-", (value or "").strip()).strip("-./")
    return cleaned[:48]


def _run_git(
    args: list[str],
    *,
    cwd: Path,
    check: bool,
) -> subprocess.CompletedProcess[str]:
    try:
        completed = subprocess.run(
            ["git", *args],
            cwd=str(cwd),
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
            check=False,
        )
    except FileNotFoundError as exc:
        raise WorktreeError("未找到 git 可执行文件。") from exc
    except OSError as exc:
        if not check:
            # check=False 的清理路径允许环境消失；返回伪失败结果。
            return subprocess.CompletedProcess(
                args=["git", *args],
                returncode=1,
                stdout="",
                stderr=str(exc),
            )
        raise WorktreeError(f"执行 git 失败：{exc}") from exc

    if check and completed.returncode != 0:
        detail = (completed.stderr or completed.stdout or "").strip()
        raise WorktreeError(detail or f"git {' '.join(args)} 失败。")
    return completed
