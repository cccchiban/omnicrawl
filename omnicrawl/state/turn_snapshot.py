"""基于 git diff 的工作区轮次快照与原子恢复。

旧实现为每个会话维护一个独立 bare Git 对象库（``shadow.git``），把工作区、
三类记忆与会话文件按 tree 整体捕获。代价是快照成本与工作区大小成正比：
被 .gitignore 忽略的大二进制文件（RAR/ZIP/DLL/视频等）会被 ``git add -f``
强制入库且永不回收，单会话可累积数百 MB，每轮同步阻塞模型请求。

新实现只依赖用户仓库自身的 Git 状态，不创建任何对象库：

- 轮次起点/终点各执行一次 ``git diff HEAD --binary``，把"未提交的已跟踪修改"
  落盘为补丁；同时用 ``git ls-files --others --exclude-standard`` 记录
  未跟踪文件清单（被忽略的 config.toml、.omnicrawl 等不属于"被 Git 记录
  的更改"，不纳入回退范围）。
- ``/undo`` 时先校验当前状态仍等于轮次终点（冲突检查），再
  ``git checkout --force HEAD -- .`` 复位到 HEAD，``git apply`` 轮次起点
  补丁，最后删除本轮新增的未跟踪文件。
- 记忆目录不再参与快照（/undo 放弃记忆回退）；非 Git 工作区禁用事务式
  undo，沿用旧降级逻辑。

已知限制：

- 轮次中被删除的未跟踪文件没有内容副本，无法恢复（只提示）。
- 被 .gitignore 忽略的受控运行态（config.toml、.agent_tmp 等）不回退。
- 旧版 shadow.git 快照事件（version 1）无法解析，/undo 会明确拒绝。
"""

from __future__ import annotations

import os
import subprocess
from dataclasses import dataclass
from pathlib import Path, PurePosixPath


class SnapshotError(RuntimeError):
    """快照创建或恢复失败。"""


class SnapshotConflictError(SnapshotError):
    """当前文件树不再等于轮次结束状态，禁止覆盖用户的新修改。"""


@dataclass(frozen=True)
class WorktreeSnapshot:
    """工作区在某一时刻的状态：HEAD 之上的未提交修改 + 未跟踪文件清单。

    ``patch`` 是 ``git diff HEAD --binary`` 的输出，已包含暂存区与工作区
    修改的合并视图；``untracked`` 是 ``git ls-files --others
    --exclude-standard`` 的文件列表（POSIX 相对路径）。``has_head`` 为假
    表示工作区不是有 HEAD 的 Git 仓库，无法快照。
    """

    patch: bytes
    untracked: tuple[str, ...]
    has_head: bool


class WorktreeSnapshotStore:
    """用 git diff 捕获/恢复单个 Git 工作区，不触碰用户仓库的引用与历史。

    与旧 shadow.git 机制的关键差异：这里只读执行 ``diff``/``ls-files``，
    写操作只有恢复时的 ``checkout --force HEAD`` 与 ``apply``，不会创建
    对象、修改 index 或影响用户分支。
    """

    # 单条 Git 子命令的最长等待时间。快照只是轮次 undo 的前置，绝不允许
    # 工作区遍历把整轮对话卡死；超时按失败处理，由上层降级为"本轮禁用
    # undo"。
    GIT_COMMAND_TIMEOUT_SECONDS = 120

    def capture(self, workspace: Path) -> WorktreeSnapshot:
        """捕获工作区当前状态；非 Git 仓库返回 has_head=False 的空快照。"""

        workspace = Path(workspace).resolve()
        if not workspace.is_dir():
            return WorktreeSnapshot(b"", (), False)
        if not self._has_head(workspace):
            return WorktreeSnapshot(b"", (), False)
        # quotepath=false：非 ASCII 路径输出原生 UTF-8，diff/apply 两侧一致。
        # 注意不能强制 core.autocrlf：Windows 默认 autocrlf=true 下工作区
        # 是 CRLF、HEAD 是 LF，若强制 autocrlf=false，行尾差异会被误判为
        # 修改，导致空修改也生成补丁、apply 时报 patch does not apply。
        patch = self._git_bytes(
            [
                "-c",
                "core.quotepath=false",
                "diff",
                "--binary",
                "--full-index",
                "HEAD",
                "--",
            ],
            cwd=workspace,
        )
        untracked = self._git_stdout(
            ["ls-files", "--others", "--exclude-standard"],
            cwd=workspace,
        )
        lines = tuple(
            line for line in untracked.splitlines() if line.strip()
        )
        return WorktreeSnapshot(patch, lines, True)

    def transition(
        self,
        workspace: Path,
        *,
        expected: WorktreeSnapshot,
        target: WorktreeSnapshot,
    ) -> list[str]:
        """当前状态完全匹配 expected 时，把工作区切换到 target。

        冲突检查和补丁应用在首次内容写入前完成；返回"无法恢复"的提示列表
        （轮次中被删除、且没有内容副本的未跟踪文件路径）。
        """

        workspace = Path(workspace).resolve()
        if not expected.has_head or not target.has_head:
            raise SnapshotError("工作区不是 Git 仓库，无法回退。")

        current = self.capture(workspace)
        if not current.has_head:
            raise SnapshotConflictError("工作区不再是 Git 仓库，拒绝回退。")
        if current.patch != expected.patch or set(current.untracked) != set(
            expected.untracked
        ):
            raise SnapshotConflictError("工作区在轮次结束后又被修改，拒绝回退。")

        # 复位到 HEAD 干净状态。用 reset --hard 而非 checkout --force：
        # 前者会同步清掉 index 中 HEAD 不存在的已暂存文件（它们会在
        # apply 轮次起点补丁时被重新创建，内容一致，仅丢失暂存标记）。
        self._git(["reset", "--hard", "HEAD"], cwd=workspace)
        if target.patch:
            self._git(
                [
                    "-c",
                    "core.quotepath=false",
                    "apply",
                    "--binary",
                    "--whitespace=nowarn",
                    "-",
                ],
                input_bytes=target.patch,
                cwd=workspace,
            )

        created = sorted(set(expected.untracked) - set(target.untracked))
        for relative in created:
            path = self._workspace_path(workspace, relative)
            try:
                if path.is_file() or path.is_symlink():
                    path.unlink()
            except OSError as exc:
                raise SnapshotError(
                    f"无法删除未跟踪文件 {relative}：{exc}"
                ) from exc

        # 轮次中被删除的未跟踪文件没有内容副本，仅提示无法恢复。
        return sorted(set(target.untracked) - set(expected.untracked))

    def _has_head(self, workspace: Path) -> bool:
        try:
            self._git_stdout(
                ["rev-parse", "--verify", "--quiet", "HEAD^{commit}"],
                cwd=workspace,
            )
            return True
        except SnapshotError:
            return False

    @staticmethod
    def _workspace_path(workspace: Path, relative: str) -> Path:
        """把 ls-files 输出的 POSIX 相对路径安全解析为工作区内的绝对路径。"""

        candidate = PurePosixPath(relative)
        if not relative or candidate.is_absolute() or ".." in candidate.parts:
            raise SnapshotError(f"未跟踪文件路径无效：{relative}")
        return workspace.joinpath(*candidate.parts)

    def _git(
        self,
        arguments: list[str],
        *,
        input_bytes: bytes | None = None,
        cwd: Path | None = None,
    ) -> subprocess.CompletedProcess[bytes]:
        try:
            result = subprocess.run(
                ["git", *arguments],
                input=input_bytes,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                cwd=cwd,
                timeout=self.GIT_COMMAND_TIMEOUT_SECONDS,
            )
        except subprocess.TimeoutExpired as exc:
            raise SnapshotError(
                f"Git 命令超时（>{self.GIT_COMMAND_TIMEOUT_SECONDS}s）：{arguments}"
            ) from exc
        except OSError as exc:
            raise SnapshotError(f"无法启动 Git：{exc}") from exc
        if result.returncode != 0:
            raise SnapshotError(self._command_error(arguments, result))
        return result

    def _git_stdout(self, arguments: list[str], *, cwd: Path | None = None) -> str:
        return self._git(arguments, cwd=cwd).stdout.decode(
            "utf-8", errors="replace"
        )

    def _git_bytes(self, arguments: list[str], *, cwd: Path | None = None) -> bytes:
        return self._git(arguments, cwd=cwd).stdout

    @staticmethod
    def _command_error(
        arguments: list[str],
        result: subprocess.CompletedProcess[bytes],
    ) -> str:
        detail = result.stderr.decode("utf-8", errors="replace").strip()
        return f"Git 命令失败（{arguments[0]}）：{detail or f'退出码 {result.returncode}'}"
