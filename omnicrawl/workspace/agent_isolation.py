"""主 Agent 隔离工作区：创建、应用变更、清理。

设计目标：让多个主 Agent 进程（TUI / API / 飞书连接器）各自在独立的
git worktree 或本地目录中读写，互不写穿主工作区，结束时可把变更安全
应用回主工作区，并自动清理不再需要的隔离区。

模式：
- ``worktree``：``~/.omnicrawl/agent-worktrees/<实例ID>/`` 下 git worktree
  （Detached HEAD，不创建临时分支；共享物理 .git，每个实例独立 HEAD/Index/工作目录）。
- ``local``：在 ``~/.omnicrawl/agent-worktrees/<实例ID>/`` 下把主工作区复制为
  普通目录（不依赖 git，排除 .git），退出时把隔离区内容镜像回主工作区。

worktree 目录已存在（上一进程 / 上次运行的残留）且纯文件系统校验通过时，
直接复用而不调用 ``git worktree add``：读 .git 指针、HEAD 与 refs 文件还原
当前提交，按持久化元数据恢复基线与创建时间；校验不通过则回退到正常创建。

创建时可选：
- 把主工作区未提交改动以 ``git diff`` patch 应用到隔离区（sync_uncommitted，
  worktree 模式）。
- 复制 gitignore 的目录/文件（copy_dirs，如 .env、node_modules，worktree 模式）。
- 运行环境脚本（env_scripts，如 npm install）。

结束时可把隔离区相对基线的变更生成 patch 应用回主工作区；冲突时
``git apply --3way`` 写入标准冲突标记并将文件标记为 Unmerged，用户
在 IDE 中按常规 Git 冲突流程解决。

自动清理统一按四层门禁过滤（``cleanup_eligible``），四层全部通过
的 worktree 才是可安全自动清理的：
  第一层：只清理标记为临时的隔离区（目录名以 aw-/sw- 开头）；
  第二层：跳过当前使用中的（in_use 实例 ID）与未过期的（保留期，
          MIN_KEEP_SECONDS / 清扫保留期）隔离区；
  第三层：fail-closed 变更检查——目录内有未提交/未跟踪改动则不删；
  第四层：有未推送远端（origin）的 commit 也不删——即使变更已应用回
          主工作区，未推送的隔离区提交仍可能是有价值的唯一副本。
退出收尾（``cleanup_on_exit=auto``）与启动清扫共用同一门禁；只有
四层全部通过的隔离区才被删除，其余一律保留并在摘要/日志中说明原因。

SubAgent 的 worktree 会话（``agent/subagents/worktree.py``）沿用同一套管理：
目录统一放在本根目录下（``sw-<task>``），创建即持久化同格式元数据并登记
共享注册表，退出 / 崩溃由同一四层门禁清扫与退出收尾处理；区别是 SubAgent
成果必须由父 Agent 显式审查后才 apply，绝不在清扫 / 退出时自动写回。

进程崩溃 / 被强杀（例如连接器随 TUI 退出被终止）留下的孤儿隔离区由
启动清扫 ``sweep_expired_isolation_sessions`` 回收：超过保留期后，按创建时
持久化的配置（``apply_on_exit`` / ``cleanup_on_exit``）先把变更应用回主工作区
（延迟收尾），再走四层门禁决定是否删除。
"""

from __future__ import annotations

import json
import logging
import os
import re
import shutil
import subprocess
import tempfile
import time
import uuid
from dataclasses import dataclass, replace
from pathlib import Path
from typing import Any

from ..config.features.agent_workspace import AgentWorkspaceConfig
from .slug import SlugSafetyError, is_safe_slug, validate_slug

LOGGER = logging.getLogger(__name__)

# 隔离区根目录：仓库外，避免污染 git 状态（用户确认 Q6）
DEFAULT_WORKTREES_ROOT = Path.home() / ".omnicrawl" / "agent-worktrees"
# 完整门禁下的保留期（秒）：隔离区创建后未满此时间不清理
MIN_KEEP_SECONDS = 3600.0
# 启动清扫保留期（秒）：孤儿/过期隔离区超过该时长才会被启动扫描回收
DEFAULT_SWEEP_MAX_AGE_SECONDS = 7 * 24 * 3600.0
_METADATA_SUFFIX = ".json"
_DEFAULT_PATCH_DIR = DEFAULT_WORKTREES_ROOT / "patches"
# SHA-1（40 位）或 SHA-256（64 位）完整提交哈希
_SHA_RE = re.compile(r"^[0-9a-f]{40,64}$")


class AgentIsolationError(RuntimeError):
    """隔离区创建、应用或清理失败时的稳定错误。"""


# 进程级隔离会话注册表：同一进程内多个 Agent 创建点（TUI/API/飞书）共享；
# 键为 instance_id，值为 IsolationSession。退出/归档时据此 apply + cleanup。
_isolation_registry: dict[str, IsolationSession] = {}
_isolation_registry_lock = __import__("threading").Lock()


def register_isolation_session(session: IsolationSession) -> None:
    """登记隔离会话，供同一进程内其他创建点 / 退出回调复用。"""

    with _isolation_registry_lock:
        _isolation_registry[session.instance_id] = session


def registered_isolation_sessions() -> list[IsolationSession]:
    """返回全部已登记隔离会话（副本）。"""

    with _isolation_registry_lock:
        return list(_isolation_registry.values())


def unregister_isolation_session(instance_id: str) -> None:
    """注销隔离会话（清理后调用）。"""

    with _isolation_registry_lock:
        _isolation_registry.pop(instance_id, None)


@dataclass(frozen=True)
class IsolationSession:
    """隔离区会话（主 Agent 隔离区 / SubAgent worktree 共用）。"""

    instance_id: str
    mode: str                     # "worktree" | "local" | "subagent"
    repo_root: Path               # 主工作区 git 仓库根（local 模式为工作区根）
    worktree_path: Path           # 隔离区实际路径
    base_ref: str                 # 基线提交（worktree / subagent 模式）
    main_workspace: Path          # 主工作区路径
    created_at: float = 0.0       # 创建时间戳（清理保留期用）
    branch_name: str = ""         # SubAgent worktree 分支（主隔离区为空）


def _run_git(
    args: list[str],
    *,
    cwd: Path,
    check: bool = True,
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
        raise AgentIsolationError("未找到 git 可执行文件。") from exc
    except OSError as exc:
        if not check:
            return subprocess.CompletedProcess(
                args=["git", *args], returncode=1, stdout="", stderr=str(exc)
            )
        raise AgentIsolationError(f"执行 git 失败：{exc}") from exc

    if check and completed.returncode != 0:
        detail = (completed.stderr or completed.stdout or "").strip()
        raise AgentIsolationError(detail or f"git {' '.join(args)} 失败。")
    return completed


def is_git_repository(path: Path) -> bool:
    """判断路径是否位于 git 工作树中。"""

    result = _run_git(
        ["rev-parse", "--is-inside-work-tree"], cwd=path, check=False
    )
    return result.returncode == 0 and result.stdout.strip() == "true"


def resolve_repo_root(path: Path) -> Path:
    """解析 path 所属的 git 仓库根目录。"""

    result = _run_git(["rev-parse", "--show-toplevel"], cwd=path, check=True)
    return Path(result.stdout.strip()).resolve()


def _resolve_base_ref(config: AgentWorkspaceConfig, repo_root: Path) -> str:
    """解析基线提交：base_branch 优先，其次 base_ref，默认当前 HEAD。"""

    branch = (config.base_branch or "").strip()
    if branch:
        resolved = _run_git(
            ["rev-parse", "--verify", f"refs/heads/{branch}"], cwd=repo_root, check=True
        ).stdout.strip()
        return resolved
    ref = (config.base_ref or "").strip() or "HEAD"
    resolved = _run_git(["rev-parse", "--verify", ref], cwd=repo_root, check=True).stdout.strip()
    return resolved


def _sync_uncommitted(session: IsolationSession) -> IsolationSession:
    """把主工作区当前内容带入新 worktree，并建立新的变更基线。

    ``git diff HEAD`` 同时包含已暂存和未暂存的 tracked 文件；未跟踪但未被
    ``.gitignore`` 排除的文件另行复制。同步完成后在隔离区创建一个内部基线
    commit，避免退出时把主工作区原有改动重复应用回主树。
    """

    diff = _run_git(["diff", "--binary", "HEAD"], cwd=session.repo_root, check=True).stdout
    if diff.strip():
        # 显式 LF 换行：Windows 文本模式默认会把 \\n 转成 \\r\\n，而 git apply
        # 对 CRLF 补丁会整体失败（patch does not apply），导致同步静默丢失。
        with tempfile.NamedTemporaryFile(
            mode="w", encoding="utf-8", newline="\n", suffix=".patch", delete=False
        ) as handle:
            handle.write(diff)
            patch_path = Path(handle.name)
        try:
            result = _run_git(
                ["apply", "--whitespace=nowarn", str(patch_path)],
                cwd=session.worktree_path,
                check=False,
            )
        finally:
            patch_path.unlink(missing_ok=True)
        if result.returncode != 0:
            LOGGER.warning(
                "同步未提交改动到隔离区失败（主工作区改动保留）：%s",
                (result.stderr or result.stdout or "").strip(),
            )

    # git diff 不包含未跟踪文件；只复制未被 .gitignore 排除的项目文件，
    # .env/node_modules 等显式依赖仍由 copy_dirs 负责，避免扩大复制范围。
    untracked = _run_git(
        ["ls-files", "--others", "--exclude-standard"],
        cwd=session.repo_root,
        check=True,
    ).stdout.splitlines()
    for relative_name in untracked:
        relative = Path(relative_name)
        source = session.repo_root / relative
        target = session.worktree_path / relative
        if not source.exists() or source.is_symlink() or not source.is_file():
            continue
        try:
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(source, target)
        except OSError as exc:
            LOGGER.warning("同步未跟踪文件失败：%s：%s", relative_name, exc)

    # 仅在实际同步出内容时创建基线 commit；空 worktree 不制造无意义 commit。
    status = _run_git(["status", "--porcelain"], cwd=session.worktree_path, check=True).stdout
    if status.strip():
        _run_git(["add", "-A"], cwd=session.worktree_path, check=True)
        committed = _run_git(
            ["commit", "-m", f"omnicrawl-sync:{session.instance_id}"],
            cwd=session.worktree_path,
            check=False,
        )
        if committed.returncode != 0:
            LOGGER.warning(
                "记录隔离区同步基线失败：%s",
                (committed.stderr or committed.stdout or "").strip(),
            )
        else:
            # 退出时只回传同步之后的 AI 增量，不能把主工作区原有改动再
            # 应用一遍。基线提交是内部实现细节，不创建分支。
            synced_base = _run_git(
                ["rev-parse", "HEAD"], cwd=session.worktree_path, check=True
            ).stdout.strip()
            session = replace(session, base_ref=synced_base)
    return session


def _copy_dirs(session: IsolationSession, copy_dirs: tuple[str, ...]) -> None:
    """复制 gitignore 的目录/文件（如 .env、node_modules）到隔离区。"""

    for item in copy_dirs:
        src = session.main_workspace / item
        dst = session.worktree_path / item
        if not src.exists():
            LOGGER.warning("copy_dirs 路径不存在，跳过：%s", src)
            continue
        try:
            if src.is_dir():
                if dst.exists():
                    shutil.copytree(src, dst, dirs_exist_ok=True)
                else:
                    shutil.copytree(src, dst)
            else:
                dst.parent.mkdir(parents=True, exist_ok=True)
                shutil.copy2(src, dst)
        except OSError as exc:
            LOGGER.warning("复制 %s 到隔离区失败：%s", src, exc)


def _run_env_scripts(session: IsolationSession, env_scripts: tuple[str, ...]) -> None:
    """在隔离区内运行环境脚本（如 npm install）。"""

    for script in env_scripts:
        try:
            result = subprocess.run(
                script,
                cwd=str(session.worktree_path),
                shell=True,
                capture_output=True,
                text=True,
                encoding="utf-8",
                errors="replace",
                timeout=1800,
                check=False,
            )
        except (OSError, subprocess.TimeoutExpired) as exc:
            LOGGER.warning("运行环境脚本失败：%s → %s", script, exc)
            continue
        if result.returncode != 0:
            LOGGER.warning(
                "环境脚本返回非零：%s → %s",
                script,
                (result.stderr or result.stdout or "").strip()[-500:],
            )


def _create_local_isolation(session: IsolationSession) -> None:
    """local 模式：把主工作区复制为普通目录（排除 .git，不依赖 git）。"""

    session.worktree_path.mkdir(parents=True, exist_ok=True)
    try:
        shutil.copytree(
            session.main_workspace,
            session.worktree_path,
            dirs_exist_ok=True,
            ignore=shutil.ignore_patterns(".git"),
            symlinks=True,
        )
    except OSError as exc:
        raise AgentIsolationError(
            f"复制主工作区到隔离区失败（可改用 mode=worktree 或关闭隔离工作区）：{exc}"
        ) from exc


def _read_worktree_gitdir(worktree_path: Path) -> Path | None:
    """读取 worktree 目录的 .git 指针文件，返回 linked gitdir 路径。

    linked worktree 的 .git 是文件而非目录，内容形如
    ``gitdir: <repo>/.git/worktrees/<name>``；路径可能是绝对路径，也可能
    是相对 worktree 目录的相对路径（相对路径必须以此目录为基准解析，
    不能相对进程 CWD）。读取失败 / 格式不符返回 None。
    """

    try:
        raw = (worktree_path / ".git").read_text(
            encoding="utf-8", errors="replace"
        ).strip()
    except OSError:
        return None
    if not raw.startswith("gitdir:"):
        return None
    gitdir = Path(raw[len("gitdir:"):].strip()).expanduser()
    if not gitdir.is_absolute():
        gitdir = worktree_path / gitdir
    try:
        return gitdir.resolve()
    except OSError:
        return None


def _resolve_common_dir(gitdir: Path) -> Path | None:
    """读取 gitdir/commondir 定位 common git dir（路径可相对 gitdir）。

    linked worktree 的 gitdir 只保存 per-worktree refs（HEAD 等），普通
    分支 ref 与 packed-refs 都在 common git dir 中；标准布局下 commondir
    为 ``..``（即 ``<repo>/.git``），但必须读文件而不是硬编码相对层级。
    """

    try:
        raw = (gitdir / "commondir").read_text(
            encoding="utf-8", errors="replace"
        ).strip()
    except OSError:
        return None
    if not raw:
        return None
    common = Path(raw)
    if not common.is_absolute():
        common = gitdir / common
    try:
        return common.resolve()
    except OSError:
        return None


def _resolve_ref_sha(common_dir: Path, ref_name: str) -> str | None:
    """解析 common git dir 中 ref 的提交 SHA：loose ref，其次 packed-refs。"""

    ref_file = common_dir / ref_name
    try:
        if ref_file.is_file():
            sha = ref_file.read_text(
                encoding="utf-8", errors="replace"
            ).strip()
            if _SHA_RE.match(sha):
                return sha
    except OSError:
        pass
    try:
        packed = (common_dir / "packed-refs").read_text(
            encoding="utf-8", errors="replace"
        )
    except OSError:
        return None
    suffix = f" {ref_name}"
    for line in packed.splitlines():
        line = line.strip()
        if not line or line.startswith("#") or line.startswith("^"):
            continue
        if line.endswith(suffix):
            sha = line[: -len(suffix)].strip()
            if _SHA_RE.match(sha):
                return sha
    return None


def resolve_worktree_head(gitdir: Path) -> str | None:
    """纯文件系统解析 worktree gitdir 的 HEAD 提交 SHA。

    - detached HEAD：HEAD 文件内容就是 commit SHA，直接返回；
    - 符号引用 ``ref: refs/heads/<branch>``：先读 commondir 定位 common
      git dir，再依次查 loose ref 与 packed-refs；
    - reftable 仓库（refs 不是普通文件）无法纯文件解析，返回 None。

    任何一步失败返回 None（fail-closed），调用方可回退 ``git rev-parse``。
    """

    try:
        head_raw = (gitdir / "HEAD").read_text(
            encoding="utf-8", errors="replace"
        ).strip()
    except OSError:
        return None
    if not head_raw:
        return None
    if not head_raw.startswith("ref: "):
        sha = head_raw.strip()
        return sha if _SHA_RE.match(sha) else None
    ref_name = head_raw[len("ref: "):].strip()
    if not ref_name:
        return None
    common_dir = _resolve_common_dir(gitdir)
    if common_dir is None:
        return None
    return _resolve_ref_sha(common_dir, ref_name)


def _worktree_registered(gitdir: Path, worktree_path: Path) -> bool:
    """校验 linked gitdir 在主仓库 worktrees/ 中仍有反向注册。

    ``<commondir>/worktrees/<name>/gitdir`` 应指回 worktree 目录的 .git
    文件；反向链接缺失说明 worktree 已被主仓库侧移除（或目录是拷贝来的），
    后续从主仓库 apply / remove 都会失效。
    """

    common_dir = _resolve_common_dir(gitdir)
    if common_dir is None:
        return False
    try:
        raw = (common_dir / "worktrees" / gitdir.name / "gitdir").read_text(
            encoding="utf-8", errors="replace"
        ).strip()
    except OSError:
        return False
    if not raw:
        return False
    reverse = Path(raw)
    if not reverse.is_absolute():
        reverse = common_dir / reverse
    try:
        return reverse.resolve() == (worktree_path / ".git").resolve()
    except OSError:
        return False


def _reuse_worktree_isolation(
    session: IsolationSession,
    worktrees_root: Path,
) -> IsolationSession | None:
    """复用已存在的 worktree 隔离区目录，全程不调用 git 子进程。

    纯文件系统校验（fail-closed），任何一步不满足返回 None，由调用方
    回退到正常创建路径：
    1. worktree 目录的 .git 是指针文件，能解析出 linked gitdir；
    2. gitdir 位于预期仓库 ``<repo_root>/.git/worktrees/`` 下；
    3. gitdir 的 HEAD 能解析出 commit SHA（detached / 符号引用 / packed-refs）；
    4. 主仓库 ``worktrees/<name>/gitdir`` 反向链接指回本目录（注册完好）；
    5. 元数据存在且为 worktree 模式——base_ref 是退出时 apply 回主工作区的
       基线，缺失时复用会让 diff 基线退化为 HEAD..HEAD（变更丢失）。

    复用成功时按元数据恢复 ``base_ref`` 与 ``created_at``，并跳过同步 /
    目录复制 / 环境脚本（上次运行已完成）。
    """

    worktree_path = session.worktree_path
    gitdir = _read_worktree_gitdir(worktree_path)
    if gitdir is None:
        return None
    try:
        gitdir.relative_to((session.repo_root / ".git" / "worktrees").resolve())
    except ValueError:
        return None
    if resolve_worktree_head(gitdir) is None:
        return None
    if not _worktree_registered(gitdir, worktree_path):
        return None
    meta = _read_isolation_metadata(worktrees_root, session.worktree_path.name)
    if not meta or meta.get("mode") != "worktree":
        return None
    try:
        base_ref = str(meta.get("base_ref") or "")
        created_at = float(meta.get("created_at") or 0.0)
    except (TypeError, ValueError):
        return None
    if not base_ref:
        return None
    return replace(session, base_ref=base_ref, created_at=created_at)


def _create_worktree_isolation(
    session: IsolationSession,
    config: AgentWorkspaceConfig,
) -> IsolationSession:
    """worktree 模式：创建 Detached HEAD worktree（不建分支）。"""

    repo_root = session.repo_root
    base_ref = _resolve_base_ref(config, repo_root)
    session = replace(session, base_ref=base_ref)
    parent_dir = session.worktree_path.parent
    parent_dir.mkdir(parents=True, exist_ok=True)
    try:
        if config.detached:
            _run_git(
                ["worktree", "add", "--detach", str(session.worktree_path), base_ref],
                cwd=repo_root,
                check=True,
            )
        else:
            branch_name = f"omnicrawl/agent/{session.instance_id}"
            _run_git(
                [
                    "worktree",
                    "add",
                    "-b",
                    branch_name,
                    str(session.worktree_path),
                    base_ref,
                ],
                cwd=repo_root,
                check=True,
            )
    except AgentIsolationError:
        if session.worktree_path.exists():
            shutil.rmtree(session.worktree_path, ignore_errors=True)
        raise
    return session


def _isolation_metadata_path(worktrees_root: Path, entry_name: str) -> Path:
    """隔离区元数据文件路径（启动清扫 / 崩溃恢复用）。

    元数据文件名与隔离区目录名一一对应（``aw-<id>.json`` / ``sw-<id>.json``），
    与实例 ID 前缀解耦，主 Agent 隔离区与 SubAgent worktree 共用同一套。
    """

    return worktrees_root / f"{entry_name}{_METADATA_SUFFIX}"


def _write_isolation_metadata(
    session: IsolationSession,
    config: AgentWorkspaceConfig,
    worktrees_root: Path,
) -> None:
    """把创建时的会话信息与收尾策略持久化，供启动清扫恢复孤儿隔离区。"""

    payload = {
        "instance_id": session.instance_id,
        "mode": session.mode,
        "repo_root": str(session.repo_root),
        "main_workspace": str(session.main_workspace),
        "worktree_path": str(session.worktree_path),
        "base_ref": session.base_ref,
        "branch_name": session.branch_name,
        "created_at": session.created_at,
        "apply_on_exit": config.apply_on_exit,
        "cleanup_on_exit": config.cleanup_on_exit,
    }
    try:
        _isolation_metadata_path(worktrees_root, session.worktree_path.name).write_text(
            json.dumps(payload, ensure_ascii=False, indent=2),
            encoding="utf-8",
        )
    except OSError as exc:
        LOGGER.warning("写入隔离区元数据失败：%s", exc)


def _read_isolation_metadata(
    worktrees_root: Path, entry_name: str
) -> dict[str, Any] | None:
    """读取隔离区元数据（按目录名定位）；缺失或损坏返回 None（不阻断清扫）。"""

    try:
        raw = _isolation_metadata_path(worktrees_root, entry_name).read_text(
            encoding="utf-8"
        )
        data = json.loads(raw)
        return data if isinstance(data, dict) else None
    except (OSError, ValueError):
        return None


def _remove_isolation_metadata(session: IsolationSession) -> None:
    """删除隔离区元数据文件（目录清理成功后调用）。"""

    try:
        _isolation_metadata_path(
            session.worktree_path.parent, session.worktree_path.name
        ).unlink(missing_ok=True)
    except OSError as exc:
        LOGGER.warning("删除隔离区元数据失败：%s", exc)


def create_isolation_session(
    *,
    main_workspace: Path,
    instance_id: str | None = None,
    config: AgentWorkspaceConfig | None = None,
    worktrees_root: Path | None = None,
) -> IsolationSession:
    """创建主 Agent 隔离区会话。

    参数：
        main_workspace: 主工作区路径（Agent 本来的 workspace_root）。
        instance_id: 实例 ID（A2：会话稳定 ID；空则随机 UUID）。
        config: 隔离区配置（缺省使用默认配置）。
        worktrees_root: 隔离区根目录（默认 ~/.omnicrawl/agent-worktrees）。

    返回：
        隔离区会话；worktree 模式已创建 Detached worktree 并完成
        未提交改动同步、目录复制与环境脚本执行（目录已存在且纯文件系统
        校验通过时直接复用，跳过 git worktree add 与上述准备步骤）；
        local 模式已复制主工作区。

    异常：
        AgentIsolationError: instance_id 不是安全 slug（含路径分隔符、点、
            空白等）时抛出，避免把外部命名拼进隔离区路径造成路径遍历。
    """

    config = config or AgentWorkspaceConfig()
    main_workspace = Path(main_workspace).expanduser().resolve()
    instance_id = (instance_id or "").strip() or uuid.uuid4().hex[:12]
    try:
        instance_id = validate_slug(instance_id, field="隔离区实例 ID")
    except SlugSafetyError as exc:
        raise AgentIsolationError(str(exc)) from exc
    worktrees_root = (worktrees_root or DEFAULT_WORKTREES_ROOT).expanduser().resolve()
    worktrees_root.mkdir(parents=True, exist_ok=True)

    mode = config.mode
    worktree_path = worktrees_root / f"aw-{instance_id}"

    base_session = IsolationSession(
        instance_id=instance_id,
        mode=mode,
        repo_root=main_workspace,
        worktree_path=worktree_path,
        base_ref="",
        main_workspace=main_workspace,
        created_at=time.time(),
    )

    reused = False
    if mode == "worktree":
        if not is_git_repository(main_workspace):
            raise AgentIsolationError(
                "worktree 模式要求主工作区位于 git 仓库中；当前不是 git 仓库，"
                "请改用 mode=local 或关闭隔离工作区。"
            )
        repo_root = resolve_repo_root(main_workspace)
        base_session = replace(base_session, repo_root=repo_root)
        if worktree_path.exists():
            # 目录已存在（上一进程 / 上次运行的残留）：纯文件系统校验通过后
            # 直接复用，跳过 git worktree add 与同步 / 复制 / 环境脚本。
            session = _reuse_worktree_isolation(base_session, worktrees_root)
            if session is not None:
                reused = True
            else:
                LOGGER.info(
                    "隔离区目录已存在但复用校验未通过，尝试重新创建：%s",
                    worktree_path,
                )
        if not reused:
            session = _create_worktree_isolation(base_session, config)
            # worktree 首次创建后才同步主树；复用既有会话时不重复同步。
            if config.sync_uncommitted:
                session = _sync_uncommitted(session)
            if config.copy_dirs:
                _copy_dirs(session, config.copy_dirs)
    elif mode == "local":
        # local 模式本身就是主工作区的完整复制，无需额外执行 sync/copy_dirs。
        _create_local_isolation(base_session)
        session = base_session
    else:
        raise AgentIsolationError(f"agent_workspace.mode 不受支持：{mode}")

    if config.env_scripts and not reused:
        _run_env_scripts(session, config.env_scripts)

    _write_isolation_metadata(session, config, worktrees_root)
    LOGGER.info(
        "已%s隔离工作区 %s（模式=%s，实例=%s）",
        "复用" if reused else "创建",
        worktree_path,
        mode,
        instance_id,
    )
    return session


def _apply_local_isolation_changes(session: IsolationSession) -> tuple[int, list[str]]:
    """local 模式：把隔离区目录内容镜像回主工作区（只新增/覆盖，不删除）。

    local 模式没有 git 基线，无法生成 patch；镜像方向固定为
    isolation → main，永不删除主工作区中 isolation 里已不存在的文件。
    """

    if not session.worktree_path.exists():
        raise AgentIsolationError(f"隔离区目录不存在：{session.worktree_path}")
    copied = 0
    for root, dirs, files in os.walk(session.worktree_path, followlinks=False):
        source_dir = Path(root)
        rel = source_dir.relative_to(session.worktree_path)
        target_dir = (
            session.main_workspace
            if str(rel) == "."
            else session.main_workspace / rel
        )
        for name in dirs:
            (target_dir / name).mkdir(parents=True, exist_ok=True)
        for name in files:
            source = source_dir / name
            if source.is_symlink():
                continue
            target = target_dir / name
            target.parent.mkdir(parents=True, exist_ok=True)
            try:
                shutil.copy2(source, target)
            except OSError as exc:
                raise AgentIsolationError(
                    f"镜像隔离区文件回主工作区失败：{target}，{exc}"
                ) from exc
            copied += 1
    return copied, []


def apply_isolation_changes(
    session: IsolationSession,
    *,
    conflict_patch_dir: Path | None = None,
) -> tuple[int, list[str]]:
    """把隔离区变更安全应用到主工作区。

    生成相对基线的 patch，在主工作区 ``git apply --3way --check`` 预检；
    冲突时写入冲突标记（Unmerged），用户按标准 Git 冲突流程解决。
    返回 (变更文件数, 冲突文件列表)。
    """

    if not session.worktree_path.exists():
        raise AgentIsolationError(f"隔离区目录不存在：{session.worktree_path}")
    if session.mode == "local":
        return _apply_local_isolation_changes(session)

    # worktree 模式：先把未提交改动 commit 到隔离区分支，确保 diff 基线干净
    _run_git(["add", "-A"], cwd=session.worktree_path, check=False)
    _run_git(
        ["commit", "-m", f"omnicrawl-agent:{session.instance_id}", "--allow-empty-message"],
        cwd=session.worktree_path,
        check=False,
    )
    base = session.base_ref or "HEAD"
    diff = _run_git(
        ["diff", base, "HEAD"], cwd=session.worktree_path, check=False
    ).stdout
    if not diff.strip():
        return 0, []

    patch_file = (
        (conflict_patch_dir or _DEFAULT_PATCH_DIR) / f"agent-{session.instance_id}.patch"
    )
    patch_file.parent.mkdir(parents=True, exist_ok=True)
    # 与 _sync_uncommitted 相同：patch 必须写 LF，Windows 默认文本模式
    # 会转成 CRLF 导致 git apply --3way 失败。
    with patch_file.open("w", encoding="utf-8", newline="\n") as handle:
        handle.write(diff)

    # Git 的三方应用以 index 作为 ours。先把当前主工作区内容纳入 index，
    # 这样主树已有未提交修改会成为 ours；应用成功时保留主修改并叠加 AI
    # 增量，冲突时 Git 原生写入 <<<<<<< / ======= / >>>>>>> 并标记 UU。
    # 这是标准 Git 冲突状态，不自行改写冲突文件或伪造 unmerged 标记。
    _run_git(["add", "-A"], cwd=session.repo_root, check=True)
    result = _run_git(
        ["apply", "--3way", "--whitespace=nowarn", str(patch_file)],
        cwd=session.repo_root,
        check=False,
    )
    if result.returncode != 0:
        conflicts = _conflict_files_from_patch(diff)
        LOGGER.warning(
            "隔离区变更应用冲突：%d 个文件需要手动解决，patch 保留在 %s",
            len(conflicts),
            patch_file,
        )
        return len(conflicts), conflicts

    changed = _count_changed(diff)
    patch_file.unlink(missing_ok=True)
    return changed, []


def _count_changed(diff: str) -> int:
    """统计 patch 涉及的文件数。"""

    count = 0
    for line in diff.splitlines():
        if line.startswith("diff --git "):
            count += 1
    return count


def _conflict_files_from_patch(diff: str) -> list[str]:
    """从 patch 中提取涉及的文件路径（冲突时用）。"""

    files: list[str] = []
    for line in diff.splitlines():
        if line.startswith("diff --git "):
            # diff --git a/path b/path
            parts = line.split(" b/", 1)
            if len(parts) == 2:
                files.append(parts[1].strip())
    return files


def cleanup_eligible(
    session: IsolationSession,
    *,
    in_use: set[str] | None = None,
    now: float | None = None,
    remote_ref: str = "origin",
) -> tuple[bool, str]:
    """四层清理门禁判定（fail-closed）：四层全部通过才可安全自动清理。

    第一层：只清理标记为临时的隔离区（目录名以 aw- 开头）。
    第二层：跳过当前使用中（in_use 实例 ID）与未过期（未到保留期
            MIN_KEEP_SECONDS）的隔离区。
    第三层：fail-closed 变更检查——目录内有未提交/未跟踪改动则不删。
    第四层：有未推送远端 commit（本地领先 origin）也不删——即使变更已
            应用回主工作区，未推送的隔离区提交仍可能是有价值的唯一副本。

    任何一层不满足返回 (False, 原因)；退出收尾与启动清扫共用本判定。
    """

    now = now if now is not None else time.time()
    # 第一层：只清理临时隔离区（aw- 主隔离区 / sw- SubAgent worktree）
    name = session.worktree_path.name
    if not name.startswith(("aw-", "sw-")):
        return False, "非临时隔离区（目录名不以 aw-/sw- 开头）"

    # 第二层：跳过当前使用中
    if in_use and session.instance_id in in_use:
        return False, "隔离区当前正在使用中"

    # 第二层：跳过未过期
    if session.created_at > 0 and now - session.created_at < MIN_KEEP_SECONDS:
        remaining = int(MIN_KEEP_SECONDS - (now - session.created_at))
        return False, f"隔离区未过保留期（还需 {remaining}s）"

    if session.mode in {"worktree", "subagent"} and session.worktree_path.exists():
        # 第三层：fail-closed 变更检查
        status = _run_git(
            ["status", "--porcelain"], cwd=session.worktree_path, check=False
        ).stdout.strip()
        if status:
            return False, "隔离区存在未提交改动"

        # 第四层：有未推送远端 commit（主隔离区）或未审查的新提交（SubAgent）
        # 也不删——未推送 / 未审查的提交可能仍是唯一副本。
        if remote_ref:
            if session.mode == "subagent":
                rev_args = [
                    "rev-list",
                    "--count",
                    f"{session.base_ref or 'HEAD'}..HEAD",
                ]
                keep_reason = "隔离区存在未审查的新提交（尚未 apply 回主工作区）"
            else:
                rev_args = ["rev-list", "--count", f"{remote_ref}..HEAD"]
                keep_reason = "隔离区存在未推送远端的 commit"
            ahead = _run_git(
                rev_args,
                cwd=session.worktree_path,
                check=False,
            ).stdout.strip()
            if ahead and ahead != "0":
                return False, keep_reason

    return True, "可安全清理"


def cleanup_isolation_session(
    session: IsolationSession,
    *,
    in_use: set[str] | None = None,
    force: bool = False,
) -> tuple[bool, str]:
    """清理隔离区目录（四层门禁 + worktree 移除 + 元数据删除）。

    返回 (是否已删除, 说明)；未满足门禁时返回 (False, 原因)。
    ``force=True`` 跳过门禁，仅用于显式手动清理。
    """

    eligible, reason = cleanup_eligible(session, in_use=in_use)
    if not force and not eligible:
        return False, reason

    if (
        session.mode in {"worktree", "subagent"}
        and session.repo_root
        and session.repo_root.exists()
        and session.worktree_path.exists()
    ):
        # 先通过 git 移除 worktree 注册信息，再兜底删除目录并 prune。
        _run_git(
            ["worktree", "remove", "--force", str(session.worktree_path)],
            cwd=session.repo_root,
            check=False,
        )
        _run_git(["worktree", "prune"], cwd=session.repo_root, check=False)

    if session.worktree_path.exists():
        shutil.rmtree(session.worktree_path, ignore_errors=True)
    if session.worktree_path.exists():
        return False, "清理失败：目录仍存在"
    if session.branch_name and session.repo_root and session.repo_root.exists():
        # SubAgent worktree 分支在目录移除后一并删除（门禁只放行无变更 / 无
        # 新提交的会话，此时分支没有独有价值）。
        _run_git(
            ["branch", "-D", session.branch_name],
            cwd=session.repo_root,
            check=False,
        )
    _remove_isolation_metadata(session)
    return True, "已清理隔离工作区"


def process_instance_id(main_workspace: Path) -> str:
    """生成进程内稳定的实例 ID（A2：同一工作区同一进程复用同一隔离区）。

    以「工作区路径哈希 + 进程启动时间戳」组合，保证：
    - 同一进程内多次调用返回同一 ID（同一次启动的 TUI/飞书/API 共享隔离区）；
    - 不同进程启动时间不同 → ID 不同 → 各自独立隔离区，互不写穿。
    """

    main_workspace = Path(main_workspace).expanduser().resolve()
    path_hash = abs(hash(str(main_workspace))) % 0xFFFFF
    # 进程启动时间（秒）来自 /proc 或 GetProcessTimes；不可用时退回随机。
    try:
        if os.name == "nt":
            import ctypes

            creation = ctypes.c_ulonglong(0)
            exit_ = ctypes.c_ulonglong(0)
            kernel = ctypes.c_ulonglong(0)
            user = ctypes.c_ulonglong(0)
            handle = ctypes.windll.kernel32.GetCurrentProcess()
            if ctypes.windll.kernel32.GetProcessTimes(
                handle,
                ctypes.byref(creation),
                ctypes.byref(exit_),
                ctypes.byref(kernel),
                ctypes.byref(user),
            ):
                # FILETIME: 100ns 间隔，转秒
                boot_secs = creation.value / 10_000_000
            else:
                boot_secs = time.time()
        else:
            stat = Path("/proc/self/stat").read_text(encoding="utf-8", errors="replace")
            boot_secs = float(stat.split(")")[1].split()[19]) if ")" in stat else time.time()
    except Exception:
        boot_secs = time.time()
    boot_secs = int(boot_secs)
    return f"w{path_hash:05x}-{boot_secs:x}"


def prepare_isolated_workspace(
    *,
    main_workspace: Path,
    config: AgentWorkspaceConfig | None = None,
    instance_id: str | None = None,
    worktrees_root: Path | None = None,
) -> tuple[Path, IsolationSession | None]:
    """主 Agent 启动时准备隔离工作区。

    返回 (Agent 实际 workspace_root, 隔离会话或 None)。
    隔离功能未启用或创建失败时返回 (main_workspace, None)，不阻断启动。
    """

    config = config or AgentWorkspaceConfig()
    main_workspace = Path(main_workspace).expanduser().resolve()
    if not config.enabled:
        return main_workspace, None
    try:
        session = create_isolation_session(
            main_workspace=main_workspace,
            instance_id=instance_id or process_instance_id(main_workspace),
            config=config,
            worktrees_root=worktrees_root,
        )
    except AgentIsolationError as exc:
        LOGGER.warning("隔离工作区创建失败，回退到主工作区：%s", exc)
        return Path(main_workspace), None
    register_isolation_session(session)
    return session.worktree_path, session


def finalize_isolation_session(
    session: IsolationSession,
    *,
    apply_on_exit: bool = True,
    cleanup_on_exit: str = "auto",
    conflict_patch_dir: Path | None = None,
    in_use: set[str] | None = None,
) -> str:
    """主 Agent 退出时收尾：apply 变更 + 按策略清理。

    返回人类可读的收尾摘要。``cleanup_on_exit=auto`` 时统一走四层门禁
    （``cleanup_eligible``）：四层全部通过的隔离区才删除，其余保留并在
    摘要中说明原因（如未过保留期、存在未提交改动、存在未推送 commit）。
    """

    messages: list[str] = []
    if apply_on_exit:
        try:
            changed, conflicts = apply_isolation_changes(
                session,
                conflict_patch_dir=conflict_patch_dir,
            )
        except AgentIsolationError as exc:
            messages.append(f"应用隔离区变更失败：{exc}")
            changed, conflicts = 0, []
        if changed:
            messages.append(f"已应用 {changed} 个文件的变更到主工作区")
        if conflicts:
            messages.append(f"⚠ {len(conflicts)} 个文件冲突，请手动解决：{', '.join(conflicts[:5])}")
    if cleanup_on_exit == "auto":
        removed, reason = cleanup_isolation_session(session, in_use=in_use)
        if removed:
            messages.append("隔离区已清理")
        else:
            messages.append(f"隔离区保留（{reason}）")
    elif cleanup_on_exit == "keep":
        messages.append("隔离区已保留（cleanup_on_exit=keep）")
    elif cleanup_on_exit == "never":
        messages.append("隔离区已保留（cleanup_on_exit=never）")
    unregister_isolation_session(session.instance_id)
    return "；".join(messages) or "无变更"


def finalize_subagent_worktrees(
    *,
    in_use: set[str] | None = None,
    remote_ref: str = "origin",
) -> str:
    """退出时按 auto 策略收尾 SubAgent worktree 会话。

    与主 Agent 隔离区共用四层门禁：只有无任何变更（未提交 / 未审查新提交）
    的会话才清理，有变更的一律保留，交给启动清扫或父 Agent 后续 apply/
    discard。SubAgent 成果必须由父 Agent 显式审查，绝不在此自动 apply 回
    主工作区。返回人类可读的收尾摘要。
    """

    messages: list[str] = []
    for session in registered_isolation_sessions():
        if session.mode != "subagent":
            continue
        eligible, reason = cleanup_eligible(
            session,
            in_use=in_use,
            remote_ref=remote_ref,
        )
        if not eligible:
            messages.append(f"{session.worktree_path.name} 保留（{reason}）")
            continue
        removed_ok, remove_reason = cleanup_isolation_session(
            session,
            in_use=in_use,
            force=True,
        )
        if removed_ok:
            unregister_isolation_session(session.instance_id)
            messages.append(f"{session.worktree_path.name} 已清理")
        else:
            messages.append(f"{session.worktree_path.name} 清理失败（{remove_reason}）")
    return "；".join(messages)


@dataclass(frozen=True)
class IsolationSweepResult:
    """一次启动清扫的汇总结果。"""

    removed: tuple[str, ...] = ()
    kept: tuple[tuple[str, str], ...] = ()   # (instance_id, 原因)
    applied: tuple[str, ...] = ()            # 已把变更应用回主工作区的实例


def _session_from_sweep_entry(
    worktrees_root: Path,
    entry: Path,
    instance_id: str,
) -> IsolationSession | None:
    """从清扫条目重建会话：优先元数据，其次用 .git 文件推断（仅 worktree）。

    名称与元数据均做 fail-closed 校验：实例 ID 必须是安全 slug；元数据里的
    ``worktree_path`` 必须解析为本次扫描到的条目目录本身（且在隔离区根目录
    内），防止被篡改的元数据把清理（rmtree / git worktree remove）或镜像回写
    重定向到任意路径。不满足的条目返回 None（清扫时按「无法识别」保留）。
    """

    if not is_safe_slug(instance_id):
        return None

    meta = _read_isolation_metadata(worktrees_root, entry.name)
    if meta:
        try:
            worktree_path = Path(str(meta["worktree_path"])).expanduser()
            if not worktree_path.is_absolute():
                worktree_path = worktrees_root / worktree_path
            resolved_path = worktree_path.resolve()
            if resolved_path != entry.resolve() or not _is_relative_to(
                resolved_path, worktrees_root
            ):
                return None
            return IsolationSession(
                instance_id=str(meta.get("instance_id") or instance_id),
                mode=str(meta.get("mode") or "worktree"),
                repo_root=Path(str(meta["repo_root"])).expanduser().resolve(),
                worktree_path=resolved_path,
                base_ref=str(meta.get("base_ref") or ""),
                main_workspace=Path(str(meta["main_workspace"])).expanduser().resolve(),
                created_at=float(meta.get("created_at") or 0.0),
                branch_name=str(meta.get("branch_name") or ""),
            )
        except (KeyError, TypeError, ValueError):
            return None

    # SubAgent worktree 必须带元数据（branch_name 用于安全清分支）；无元数据
    # 的 sw- 目录按「无法识别」保留，不猜测清理。
    if not entry.name.startswith("aw-"):
        return None

    # 旧版本留下的无元数据目录：凡带 .git 指针文件的按 worktree 处理。
    gitdir = _read_worktree_gitdir(entry)
    if gitdir is None:
        return None
    try:
        # common git dir 由 commondir 文件给出（标准布局为 <repo>/.git），
        # 仓库根 = common dir 的父目录；不依赖 gitdir 的具体层级，也兼容
        # gitdir 为相对路径的情况（_read_worktree_gitdir 已按目录解析）。
        common_dir = _resolve_common_dir(gitdir)
        if common_dir is None:
            return None
        repo_root = common_dir.parent
    except OSError:
        return None
    try:
        created_at = entry.stat().st_mtime
    except OSError:
        created_at = 0.0
    return IsolationSession(
        instance_id=instance_id,
        mode="worktree",
        repo_root=repo_root,
        worktree_path=entry.resolve(),
        base_ref="",
        main_workspace=repo_root,
        created_at=created_at,
    )


def sweep_expired_isolation_sessions(
    worktrees_root: Path | None = None,
    *,
    max_age_seconds: float = DEFAULT_SWEEP_MAX_AGE_SECONDS,
    now: float | None = None,
    in_use: set[str] | None = None,
    remote_ref: str = "origin",
) -> IsolationSweepResult:
    """启动清扫：回收超过保留期的孤儿/过期隔离区。

    - 只处理 ``cleanup_on_exit=auto`` 的会话；keep / never 一律跳过（尊重配置）。
    - 创建时 ``apply_on_exit=true`` 的会话先尝试把变更应用回主工作区（进程崩溃 /
      被强杀时的延迟收尾），随后统一走四层门禁决定是否删除；apply 失败或
      ``apply_on_exit=false`` 的会话同样走门禁，宁可保留也不丢数据。
    - 无元数据的旧目录只做可回收判定删除，不做 apply 回写（无法还原基线）。
    """

    root = (worktrees_root or DEFAULT_WORKTREES_ROOT).expanduser().resolve()
    now = now if now is not None else time.time()
    guarded: set[str] = set(in_use or ())
    # 同一进程内仍存活的会话全部视为使用中
    guarded.update(session.instance_id for session in registered_isolation_sessions())
    if not root.exists():
        return IsolationSweepResult()

    removed: list[str] = []
    applied: list[str] = []
    kept: list[tuple[str, str]] = []
    for entry in sorted(root.iterdir(), key=lambda item: item.name):
        if not entry.is_dir() or not entry.name.startswith(("aw-", "sw-")):
            continue
        instance_id = entry.name[3:]
        session = _session_from_sweep_entry(root, entry, instance_id)
        if session is None:
            kept.append((instance_id, "无法识别隔离区"))
            continue
        if guarded and session.instance_id in guarded:
            kept.append((instance_id, "隔离区当前正在使用中"))
            continue
        if session.created_at > 0 and now - session.created_at < max_age_seconds:
            remaining = int(max_age_seconds - (now - session.created_at))
            kept.append((instance_id, f"隔离区未过清扫保留期（还需 {remaining}s）"))
            continue

        meta = _read_isolation_metadata(root, entry.name)
        cleanup_policy = str(meta.get("cleanup_on_exit", "auto")) if meta else "auto"
        if cleanup_policy != "auto":
            kept.append((instance_id, f"cleanup_on_exit={cleanup_policy}，跳过清扫"))
            continue

        apply_on_exit = bool(meta.get("apply_on_exit", True)) if meta else False
        # SubAgent worktree 绝不自动 apply 回主工作区：成果必须由父 Agent
        # 显式审查后 apply/discard，崩溃遗留只按门禁清理或保留。
        if apply_on_exit and session.mode != "subagent":
            try:
                changed, _conflicts = apply_isolation_changes(session)
                if changed:
                    applied.append(instance_id)
            except AgentIsolationError as exc:
                kept.append((instance_id, f"应用变更失败：{exc}"))
                continue

        # 延迟收尾之后仍必须四层门禁全过才能安全自动清理
        #（未推送远端 commit / 未提交改动 / 未过期 / 非临时一律保留）。
        eligible, reason = cleanup_eligible(
            session,
            in_use=guarded,
            now=now,
            remote_ref=remote_ref,
        )
        if not eligible:
            kept.append((instance_id, reason))
            continue
        removed_ok, remove_reason = cleanup_isolation_session(
            session,
            in_use=guarded,
            force=True,
        )
        if removed_ok:
            removed.append(instance_id)
        else:
            kept.append((instance_id, remove_reason))

    if removed or applied:
        LOGGER.info(
            "隔离工作区清扫：应用 %d 个、清理 %d 个、保留 %d 个",
            len(applied),
            len(removed),
            len(kept),
        )
    return IsolationSweepResult(
        removed=tuple(removed),
        kept=tuple(kept),
        applied=tuple(applied),
    )


def _is_relative_to(path: Path, parent: Path) -> bool:
    """判断 path 是否位于 parent 之内（或相等）。"""

    try:
        path.relative_to(parent)
        return True
    except ValueError:
        return False