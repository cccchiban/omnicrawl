"""基于独立 Git 对象库的单轮文件树快照与原子恢复。"""

from __future__ import annotations

import os
import subprocess
from dataclasses import dataclass
from pathlib import Path, PurePosixPath
from typing import Mapping


class SnapshotError(RuntimeError):
    """影子快照创建或恢复失败。"""


class SnapshotConflictError(SnapshotError):
    """当前文件树不再等于轮次结束状态，禁止覆盖用户的新修改。"""


@dataclass(frozen=True)
class SnapshotRoot:
    """一个受影子 Git 管理的根目录及其运行态排除项。"""

    path: Path
    excluded: tuple[str, ...] = ()
    included: tuple[str, ...] = ()

    def __post_init__(self) -> None:
        def normalize(values: tuple[str, ...], *, label: str) -> tuple[str, ...]:
            normalized: list[str] = []
            for raw_value in values:
                value = str(raw_value).replace("\\", "/").strip("/")
                candidate = PurePosixPath(value)
                if not value or candidate.is_absolute() or ".." in candidate.parts:
                    raise SnapshotError(f"快照{label}路径无效：{raw_value}")
                normalized.append(candidate.as_posix())
            return tuple(dict.fromkeys(normalized))

        object.__setattr__(self, "path", Path(self.path).resolve())
        object.__setattr__(
            self,
            "excluded",
            normalize(self.excluded, label="排除"),
        )
        object.__setattr__(
            self,
            "included",
            normalize(self.included, label="包含"),
        )


@dataclass(frozen=True)
class GitTreeSnapshot:
    """单个根目录在某一时刻的 Git tree。"""

    tree_id: str
    root_existed: bool

    def to_payload(self) -> dict[str, object]:
        return {"tree_id": self.tree_id, "root_existed": self.root_existed}

    @classmethod
    def from_payload(cls, payload: Mapping[str, object]) -> GitTreeSnapshot:
        tree_id = str(payload.get("tree_id") or "").strip()
        if len(tree_id) != 40 or any(
            character not in "0123456789abcdef" for character in tree_id
        ):
            raise SnapshotError("快照 tree_id 格式无效。")
        root_existed = payload.get("root_existed")
        if not isinstance(root_existed, bool):
            raise SnapshotError("快照 root_existed 必须是布尔值。")
        return cls(tree_id=tree_id, root_existed=root_existed)


SnapshotSet = dict[str, GitTreeSnapshot]


class GitSnapshotStore:
    """用外置 bare Git 仓库保存多个文件树，不接触用户仓库状态。"""

    # 单条 Git 子命令的最长等待时间。快照只是轮次 undo 的前置，绝不允许
    # 工作区遍历（例如误把用户主目录当根）把整轮对话卡死；超时按失败处理，
    # 由上层（_begin_turn_snapshot）降级为“本轮禁用 undo”。
    GIT_COMMAND_TIMEOUT_SECONDS = 120

    def __init__(self, git_dir: Path) -> None:
        self.git_dir = Path(git_dir).resolve()
        self._ensure_repository()
        self._empty_tree_id = self._git_stdout(["mktree"], input_bytes=b"").strip()

    def capture(self, roots: Mapping[str, SnapshotRoot]) -> SnapshotSet:
        """捕获全部根目录；tree 对象由 bare 仓库统一去重保存。"""

        snapshots: SnapshotSet = {}
        for name, root in roots.items():
            normalized_name = str(name).strip()
            if not normalized_name or normalized_name in snapshots:
                raise SnapshotError(f"快照根名称无效或重复：{name}")
            snapshots[normalized_name] = self._capture_root(normalized_name, root)
        return snapshots

    def transition(
        self,
        *,
        roots: Mapping[str, SnapshotRoot],
        expected: Mapping[str, GitTreeSnapshot],
        target: Mapping[str, GitTreeSnapshot],
    ) -> None:
        """当前状态完全匹配 expected 时，将所有根切换到 target。

        冲突检查和全部 ``git apply --check`` 均在首次内容写入前完成。实际应用
        阶段若某个根失败，会按相反顺序把已完成的根恢复到 expected。
        """

        root_names = tuple(roots)
        if set(root_names) != set(expected) or set(root_names) != set(target):
            raise SnapshotError("快照根集合不一致，无法恢复。")

        current = self.capture(roots)
        conflicts = [name for name in root_names if current[name] != expected[name]]
        if conflicts:
            names = "、".join(conflicts)
            raise SnapshotConflictError(f"以下范围在轮次结束后又被修改：{names}")

        patches = {
            name: self._tree_patch(expected[name].tree_id, target[name].tree_id)
            for name in root_names
        }
        created_roots: list[Path] = []
        try:
            for name in root_names:
                if not patches[name]:
                    continue
                root_path = roots[name].path
                if not root_path.exists():
                    root_path.mkdir(parents=True, exist_ok=False)
                    created_roots.append(root_path)
                self._apply_patch(root_path, patches[name], check_only=True)
        except Exception:
            for root_path in reversed(created_roots):
                self._remove_if_empty(root_path)
            raise

        applied: list[str] = []
        try:
            for name in root_names:
                patch = patches[name]
                if patch:
                    self._apply_patch(roots[name].path, patch, check_only=False)
                applied.append(name)
            for name in root_names:
                if not target[name].root_existed:
                    self._remove_if_empty(roots[name].path)
        except Exception as exc:
            rollback_errors: list[str] = []
            for name in reversed(applied):
                reverse_patch = self._tree_patch(
                    target[name].tree_id,
                    expected[name].tree_id,
                )
                if not reverse_patch:
                    continue
                try:
                    root_path = roots[name].path
                    root_path.mkdir(parents=True, exist_ok=True)
                    self._apply_patch(root_path, reverse_patch, check_only=False)
                except Exception as rollback_exc:  # pragma: no cover - 极端文件系统故障
                    rollback_errors.append(f"{name}: {rollback_exc}")
            for root_path in reversed(created_roots):
                self._remove_if_empty(root_path)
            detail = (
                f"；反向恢复失败：{'；'.join(rollback_errors)}"
                if rollback_errors
                else ""
            )
            raise SnapshotError(f"应用快照失败：{exc}{detail}") from exc

    def _capture_root(self, name: str, root: SnapshotRoot) -> GitTreeSnapshot:
        path = root.path
        if not path.exists():
            return GitTreeSnapshot(self._empty_tree_id, False)
        if not path.is_dir():
            raise SnapshotError(f"快照根不是目录：{path}")

        index_dir = self.git_dir / "omnicrawl-indexes"
        index_dir.mkdir(parents=True, exist_ok=True)
        # 跨轮复用固定 index：首次 read-tree --empty 清空，后续直接增量 add，
        # 保留 stat 与 untracked 缓存，避免对未变化文件反复读盘哈希——大型
        # 项目每轮快照的主要成本是重新哈希全部文件，而非增量扫描。
        index_path = index_dir / f"{name}.index"
        env = os.environ.copy()
        env["GIT_INDEX_FILE"] = str(index_path)
        # index 损坏或与工作树不一致时，删除重建一次并退化为全量捕获。
        for attempt in (1, 2):
            try:
                return self._capture_root_into(index_path, path, root, env)
            except SnapshotError:
                if attempt == 2 or not index_path.exists():
                    raise
                try:
                    index_path.unlink()
                except OSError as exc:
                    raise SnapshotError(f"无法删除损坏的快照 index：{exc}") from exc
        raise SnapshotError("快照 index 重试失败")  # 防御分支，实际不可达

    def _capture_root_into(
        self,
        index_path: Path,
        path: Path,
        root: SnapshotRoot,
        env: dict[str, str],
    ) -> GitTreeSnapshot:
        """把单个根目录捕获进给定 index 的当前状态。"""

        if not index_path.exists():
            self._git(
                ["-c", "core.indexVersion=4", "read-tree", "--empty"],
                env=env,
            )
        pathspecs = list(root.included) or ["."]
        exclusions = tuple(dict.fromkeys((".git", *root.excluded)))
        if os.name == "nt":
            # `-f` 会绕过 .gitignore；Git for Windows 不能把 NUL 设备写入索引。
            exclusions = (*exclusions, "NUL")
        for excluded in exclusions:
            pathspec_magic = (
                "exclude,icase"
                if os.name == "nt" and excluded.casefold() == "nul"
                else "exclude"
            )
            pathspecs.append(f":({pathspec_magic}){excluded}")
            pathspecs.append(f":({pathspec_magic}){excluded}/**")
        # `-f` 保留：快照需要捕获被忽略的受控运行态（.agent_tmp、config.toml、
        # 会话 artifact 等），undo 才能完整回退；巨型依赖/构建目录由上层
        # excluded 名单排除，避免遍历 node_modules 等。
        self._git(
            [
                "-c",
                "core.autocrlf=false",
                "-c",
                "core.indexVersion=4",
                "-c",
                "core.untrackedCache=true",
                "-c",
                "advice.addIgnoredFile=false",
                "--work-tree",
                str(path),
                "add",
                "-A",
                "-f",
                "--",
                *pathspecs,
            ],
            env=env,
            cwd=path,
        )
        tree_id = self._git_stdout(
            ["-c", "core.autocrlf=false", "--work-tree", str(path), "write-tree"],
            env=env,
        ).strip()
        return GitTreeSnapshot(tree_id=tree_id, root_existed=True)

    def _tree_patch(self, source_tree: str, target_tree: str) -> bytes:
        if source_tree == target_tree:
            return b""
        return self._git_bytes(
            [
                "-c",
                "core.autocrlf=false",
                "diff",
                "--binary",
                "--full-index",
                source_tree,
                target_tree,
                "--",
            ]
        )

    def _apply_patch(self, root: Path, patch: bytes, *, check_only: bool) -> None:
        arguments = [
            "-c",
            "core.autocrlf=false",
            "--work-tree",
            str(root),
            "apply",
            "--binary",
            "--whitespace=nowarn",
        ]
        if check_only:
            arguments.append("--check")
        arguments.append("-")
        self._git(arguments, input_bytes=patch, cwd=root)

    def _ensure_repository(self) -> None:
        if (self.git_dir / "HEAD").is_file() and (self.git_dir / "objects").is_dir():
            return
        self.git_dir.parent.mkdir(parents=True, exist_ok=True)
        try:
            result = subprocess.run(
                ["git", "init", "--bare", "--quiet", str(self.git_dir)],
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                timeout=self.GIT_COMMAND_TIMEOUT_SECONDS,
            )
        except subprocess.TimeoutExpired as exc:
            raise SnapshotError(
                f"初始化影子 Git 仓库超时（>{self.GIT_COMMAND_TIMEOUT_SECONDS}s）：{exc}"
            ) from exc
        except OSError as exc:
            raise SnapshotError(f"无法启动 Git：{exc}") from exc
        if result.returncode != 0:
            raise SnapshotError(self._command_error("初始化影子 Git 仓库", result))

    def _git(
        self,
        arguments: list[str],
        *,
        input_bytes: bytes | None = None,
        env: dict[str, str] | None = None,
        cwd: Path | None = None,
    ) -> subprocess.CompletedProcess[bytes]:
        try:
            result = subprocess.run(
                ["git", "--git-dir", str(self.git_dir), *arguments],
                input=input_bytes,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                env=env,
                cwd=cwd,
                timeout=self.GIT_COMMAND_TIMEOUT_SECONDS,
            )
        except subprocess.TimeoutExpired as exc:
            # subprocess.run 超时后已 kill 子进程；这里转为 SnapshotError，
            # 让上层降级而不是让对话永久挂起。
            raise SnapshotError(
                f"影子 Git 命令超时（>{self.GIT_COMMAND_TIMEOUT_SECONDS}s）：{arguments}"
            ) from exc
        except OSError as exc:
            raise SnapshotError(f"无法启动 Git：{exc}") from exc
        if result.returncode != 0:
            raise SnapshotError(self._command_error("影子 Git 命令失败", result))
        return result

    def _git_stdout(
        self,
        arguments: list[str],
        *,
        input_bytes: bytes | None = None,
        env: dict[str, str] | None = None,
    ) -> str:
        return self._git(arguments, input_bytes=input_bytes, env=env).stdout.decode(
            "utf-8", errors="replace"
        )

    def _git_bytes(self, arguments: list[str]) -> bytes:
        return self._git(arguments).stdout

    @staticmethod
    def _command_error(
        prefix: str,
        result: subprocess.CompletedProcess[bytes],
    ) -> str:
        detail = result.stderr.decode("utf-8", errors="replace").strip()
        return f"{prefix}：{detail or f'退出码 {result.returncode}'}"

    @staticmethod
    def _remove_if_empty(path: Path) -> None:
        if not path.is_dir():
            return
        try:
            next(path.iterdir())
        except StopIteration:
            path.rmdir()
        except OSError:
            return
