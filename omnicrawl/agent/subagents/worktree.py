"""SubAgent git worktree 隔离生命周期（沿用项目 Worktree 管理设计）。

与主 Agent 隔离区（``workspace/agent_isolation.py``）同一套管理：
1. 创建：目录放在共享托管根 ``~/.omnicrawl/agent-worktrees/``（``sw-<task>``），
   创建即持久化同格式元数据并登记共享注册表；目录已存在且纯文件系统校验
   通过时直接复用，跳过 ``git worktree add``
2. 进入退出：会话登记在共享注册表（启动清扫 in-use 保护）；apply/discard
   后移除目录、分支、元数据并注销
3. 自动清理：崩溃遗留由启动清扫四层门禁统一回收，退出时按 auto 策略
   收尾；SubAgent 成果必须由父 Agent 显式审查后才 apply，清扫 / 退出
   绝不自动写回主工作区

创建与 apply 前都会检查主工作区是否干净，避免在脏主树上静默覆盖用户未提交改动。
"""

from __future__ import annotations

import logging
import re
import shutil
import subprocess
import time
import uuid
from dataclasses import dataclass
from pathlib import Path

from ...config.features.agent_workspace import AgentWorkspaceConfig
from ...workspace.agent_isolation import (
    DEFAULT_WORKTREES_ROOT,
    IsolationSession,
    _read_isolation_metadata,
    _read_worktree_gitdir,
    _remove_isolation_metadata,
    _worktree_registered,
    _write_isolation_metadata,
    register_isolation_session,
    resolve_worktree_head,
    unregister_isolation_session,
)
from ...workspace.slug import is_safe_slug

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
    instance_id: str = ""        # 托管键（sw-<task> 的安全 slug 部分）


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


@dataclass(frozen=True)
class WorktreeChangeSummary:
    """worktree 相对基线的变更计数（退出 / 丢弃时的变更保护用）。"""

    uncommitted: int = 0    # 未提交 / 未跟踪文件数
    new_commits: int = 0    # 分支上相对 base_ref 的新提交数


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
    """为 task 创建独立 worktree 与本地分支（沿用项目 Worktree 管理设计）。

    - 目录放在共享托管根（默认 ``~/.omnicrawl/agent-worktrees/``），命名
      ``sw-<task>``，与主 Agent 隔离区同根管理；
    - 目录已存在且纯文件系统校验通过时直接复用，跳过 ``git worktree add``；
    - 创建 / 复用后立即持久化元数据（``sw-<task>.json``）并登记共享注册表，
      供启动清扫的 in-use 保护与退出收尾使用；
    - 主工作区必须干净（无未提交变更）。
    """

    root = Path(workspace_root).expanduser().resolve()
    if not is_git_repository(root):
        raise WorktreeError("当前工作区不是 git 仓库，无法启用 isolation=worktree。")

    repo_root = resolve_repo_root(root)
    safe_task = _sanitize_branch_fragment(task_id) or uuid.uuid4().hex[:12]
    # 目录名 / 托管键必须是安全 slug（清扫与元数据按 slug 校验）；分支名可放宽。
    instance_key = safe_task if is_safe_slug(safe_task) else uuid.uuid4().hex[:12]
    branch_name = f"omnicrawl/subagent/{safe_task}"
    parent_dir = (
        Path(worktree_parent).expanduser().resolve()
        if worktree_parent is not None
        else DEFAULT_WORKTREES_ROOT
    )
    parent_dir.mkdir(parents=True, exist_ok=True)
    worktree_path = parent_dir / f"sw-{instance_key}"

    reused = _reuse_subagent_worktree(
        worktree_path=worktree_path,
        repo_root=repo_root,
        task_id=task_id,
        instance_key=instance_key,
    )
    if reused is not None:
        session = reused
    else:
        # 脏主树禁止创建：避免后续 apply 时与用户未提交改动互相覆盖。
        require_clean_main_tree(repo_root)
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
        session = WorktreeSession(
            task_id=task_id,
            instance_id=instance_key,
            repo_root=repo_root,
            worktree_path=worktree_path.resolve(),
            branch_name=branch_name,
            base_ref=resolved_base,
        )

    # 创建 / 复用统一持久化元数据并登记共享注册表。
    _persist_subagent_worktree_metadata(session, parent_dir)
    register_isolation_session(_isolation_view(session))
    return session


def _reuse_subagent_worktree(
    *,
    worktree_path: Path,
    repo_root: Path,
    task_id: str,
    instance_key: str,
) -> WorktreeSession | None:
    """目录已存在时纯文件系统校验并复用（与主 Agent 隔离区同款）。

    fail-closed：任何一步不满足返回 None，由创建路径继续（``git worktree add``
    会因目录已存在而失败，最终以 :class:`WorktreeError` 呈现给父 Agent）。
    """

    if not worktree_path.exists():
        return None
    gitdir = _read_worktree_gitdir(worktree_path)
    if gitdir is None:
        return None
    try:
        gitdir.relative_to((repo_root / ".git" / "worktrees").resolve())
    except ValueError:
        return None
    if resolve_worktree_head(gitdir) is None:
        return None
    if not _worktree_registered(gitdir, worktree_path):
        return None
    meta = _read_isolation_metadata(worktree_path.parent, worktree_path.name)
    if not meta or meta.get("mode") != "subagent":
        return None
    branch_name = str(meta.get("branch_name") or "")
    base_ref = str(meta.get("base_ref") or "")
    if not branch_name or not base_ref:
        return None
    return WorktreeSession(
        task_id=task_id,
        instance_id=instance_key,
        repo_root=repo_root,
        worktree_path=worktree_path,
        branch_name=branch_name,
        base_ref=base_ref,
    )


def _isolation_view(session: WorktreeSession) -> IsolationSession:
    """把 SubAgent 会话投影为共享隔离会话（登记注册表 / 清扫 / 收尾用）。"""

    return IsolationSession(
        instance_id=session.instance_id,
        mode="subagent",
        repo_root=session.repo_root,
        worktree_path=session.worktree_path,
        base_ref=session.base_ref,
        main_workspace=session.repo_root,
        created_at=time.time(),
        branch_name=session.branch_name,
    )


def _persist_subagent_worktree_metadata(
    session: WorktreeSession,
    parent_dir: Path,
) -> None:
    """持久化 SubAgent worktree 会话元数据（崩溃恢复 / 清扫 / 收尾用）。

    与主 Agent 隔离区同格式；SubAgent 成果须父 Agent 审查，故
    ``apply_on_exit`` 固定为 False，清理策略固定为 ``auto``（四层门禁）。
    """

    _write_isolation_metadata(
        _isolation_view(session),
        AgentWorkspaceConfig(apply_on_exit=False, cleanup_on_exit="auto"),
        parent_dir,
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


def summarize_worktree_changes(session: WorktreeSession) -> WorktreeChangeSummary:
    """统计 worktree 相对基线的变更（未提交文件数 + 新提交数）。

    用于退出 / 丢弃时的变更保护：未提交改动或新提交都意味着有尚未 apply
    回主工作区的成果，丢弃前必须显式 force。

    - worktree 目录已不存在时视为无变更（没有可丢失的内容）；
    - git 查询失败时抛 :class:`WorktreeError`（fail-closed，宁可拒绝丢弃也
      不静默丢数据），调用方可用 force 绕过；
    - base_ref 无法解析时按「存在新提交」保守处理。
    """

    if not session.worktree_path.exists():
        return WorktreeChangeSummary()
    status = _run_git(
        ["status", "--porcelain"],
        cwd=session.worktree_path,
        check=True,
    ).stdout
    uncommitted = len([line for line in status.splitlines() if line.strip()])
    base = (session.base_ref or "").strip() or "HEAD"
    try:
        count = _run_git(
            ["rev-list", "--count", f"{base}..HEAD"],
            cwd=session.worktree_path,
            check=True,
        ).stdout.strip()
    except WorktreeError:
        new_commits = 1
    else:
        new_commits = max(0, int(count or "0"))
    return WorktreeChangeSummary(
        uncommitted=uncommitted,
        new_commits=new_commits,
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
    """清理 worktree 目录与本地分支，并注销共享注册表、删除托管元数据。"""

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

    # 注销共享注册表并删除托管元数据（沿用主 Agent 隔离区收尾设计）。
    if session.instance_id:
        unregister_isolation_session(session.instance_id)
    try:
        _remove_isolation_metadata(session)
    except OSError:
        LOGGER.warning(
            "删除 SubAgent worktree 元数据失败：%s", session.worktree_path
        )


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
