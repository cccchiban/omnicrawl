"""/undo 回合快照与副作用回滚。"""
from __future__ import annotations

import logging
import uuid
from pathlib import Path, PurePosixPath
from typing import Any, Callable, Mapping, Sequence
from ..types import AgentModelReply, ToolCall, ToolDefinition, ToolResult
from ...session import (
    COMPACT_SUMMARY_PREFIX,
    PromptHistoryEntry,
    SessionIndexEntry,
    SessionEvent,
    SessionEventReadResult,
    SessionState,
    SessionStore,
    SessionUndoPlan,
)
from ...state.turn_snapshot import (
    SnapshotConflictError,
    SnapshotError,
    WorktreeSnapshot,
    WorktreeSnapshotStore,
)

from .shared import (
    AgentError,
    _ActiveTurnSnapshot,
    _MEMORY_UNDO_EXEMPT_TOOLS,
    _READ_ONLY_UNDO_TOOLS,
    _REVERSIBLE_UNDO_TOOLS,
)

LOGGER = logging.getLogger(__name__)


class UndoMixin:
    """/undo 回合快照与副作用回滚。"""

    def _begin_turn_snapshot(self) -> _ActiveTurnSnapshot | None:
        """登记本轮占位快照；工作区非 Git 或会话未启用时返回 None。

        不再立即执行 ``git diff``/``ls-files``：只有本轮出现可回退写工具
        （Edit_file/write_file）时，才在首个写工具执行前由
        ``_ensure_turn_captured`` 补捕获起点。纯读/纯对话轮次全程 0 次
        diff 快照与 0 次落盘。此处只保留一次 rev-parse 探测，用于在
        非 Git 工作区尽早返回 None（保持旧契约：写文件轮次 /undo 拒绝）。
        """

        session_store = getattr(self, "_session_store", None)
        session_state = getattr(self, "_session_state", None)
        if not isinstance(session_store, SessionStore) or session_state is None:
            return None
        try:
            store = WorktreeSnapshotStore()
            if not store.has_head(self.workspace_root):
                # 工作区不是有 HEAD 的 Git 仓库：diff 补丁无从谈起。本轮
                # 禁用事务式 undo（纯对话/只读轮次仍可逻辑回退，写文件
                # 轮次会被 _restore_turn_side_effects 明确拒绝）。
                LOGGER.warning(
                    "工作区不是 Git 仓库，本轮禁用事务式 undo（%s）",
                    self.workspace_root,
                )
                return None
            return _ActiveTurnSnapshot(
                snapshot_id=uuid.uuid4().hex,
                store=store,
                workspace=self.workspace_root.resolve(),
            )
        except SnapshotError as exc:
            # 快照失败仅禁用本轮 undo，不中止回合：Git 环境异常时若直接
            # 抛错，整轮对话会在模型请求前就失败。降级后本轮失去 undo，
            # 但对话与工具执行不受影响。
            LOGGER.warning("无法创建本轮 Git 快照，本轮禁用事务式 undo：%s", exc)
            return None

    def _ensure_turn_captured(self, snapshot: _ActiveTurnSnapshot) -> None:
        """首个可回退写工具执行前，捕获工作区起点（线程安全单飞）。

        捕获失败只把本轮降级为“无事务式 undo”（账本仍记录），不中止
        回合；多个并发写工具同时到达时由 capture_lock 保证只捕获一次。
        """

        if snapshot.before is not None or snapshot.capture_attempted:
            return
        with snapshot.capture_lock:
            if snapshot.before is not None or snapshot.capture_attempted:
                return
            snapshot.capture_attempted = True
            try:
                snapshot.before = snapshot.store.capture(snapshot.workspace)
            except SnapshotError as exc:
                snapshot.capture_failed = True
                LOGGER.warning(
                    "无法创建本轮 Git 起点快照，本轮禁用事务式 undo：%s", exc
                )

    def _record_turn_tool_execution(
        self,
        snapshot: _ActiveTurnSnapshot | None,
        tool_call: ToolCall,
    ) -> None:
        """记录实际执行过的工具；未知或外部工具会阻止事务式 undo。"""

        if snapshot is None:
            return
        name = tool_call.name
        snapshot.executed_tools.append(name)
        if name in _REVERSIBLE_UNDO_TOOLS:
            # 可回退写工具需要真实 diff 快照才能安全回退：在工具副作用
            # 发生前补捕获起点。其它可逆工具（读/记忆等）与不可逆工具
            # （bash 等）都不需要快照——前者无副作用，后者整轮会被拒绝。
            self._ensure_turn_captured(snapshot)
            return
        if self._tool_is_undo_safe(name, tool_call.arguments):
            return
        snapshot.irreversible_tools.append(name)

    @staticmethod
    def _tool_is_undo_safe(name: str, arguments: Mapping[str, Any]) -> bool:
        if (
            name in _READ_ONLY_UNDO_TOOLS
            or name in _REVERSIBLE_UNDO_TOOLS
            or name in _MEMORY_UNDO_EXEMPT_TOOLS
        ):
            return True
        if name == "subagent":
            return str(arguments.get("action") or "run").strip() in {
                "list",
                "get",
                "list_worktrees",
            }
        if name == "monitor":
            return str(arguments.get("action") or "list").strip() in {"list", "poll"}
        if name == "windows_window":
            return str(arguments.get("action") or "list").strip() in {"list", "get"}
        if name == "windows_clipboard":
            return str(arguments.get("action") or "read_text").strip() == "read_text"
        if name == "windows_screenshot":
            return True
        return False

    def _complete_turn_snapshot(self, snapshot: _ActiveTurnSnapshot | None) -> None:
        """捕获轮次终点并持久化 undo 补丁，作为 Session 事件记录。

        纯读/纯对话轮次没有起点快照（before 为 None），此处直接标记完成、
        不落盘也不产生事件：/undo 会走“无快照且无副作用”的安全逻辑路径。
        只有捕获过起点的写文件轮次才执行终点 diff 并落盘 4 个快照文件。
        """

        if snapshot is None or snapshot.completed:
            return
        session_store = getattr(self, "_session_store", None)
        session_state = getattr(self, "_session_state", None)
        before = snapshot.before
        if before is None:
            # 无起点快照（纯读/纯对话轮，或起点捕获失败降级）：不落盘。
            snapshot.completed = True
            return
        if not isinstance(session_store, SessionStore) or session_state is None:
            raise AgentError("Session 未启用，无法持久化轮次快照。")
        try:
            after = snapshot.store.capture(snapshot.workspace)
            undo_dir = (
                session_store.artifacts_dir / session_state.session_id / "undo"
            )
            undo_dir.mkdir(parents=True, exist_ok=True)
            self._write_snapshot_file(undo_dir / "begin.patch", before.patch)
            self._write_snapshot_untracked(
                undo_dir / "begin.untracked.txt", before.untracked
            )
            self._write_snapshot_file(undo_dir / "end.patch", after.patch)
            self._write_snapshot_untracked(
                undo_dir / "end.untracked.txt", after.untracked
            )
        except (SnapshotError, OSError) as exc:
            raise AgentError(
                f"本轮结束 Git 快照失败，副作用无法安全回退：{exc}"
            ) from exc
        if snapshot.workspace is None:  # 防御：占位快照不应走到落盘分支。
            snapshot.completed = True
            return
        self._append_session_event(
            "turn_snapshot",
            {
                "version": 2,
                "snapshot_id": snapshot.snapshot_id,
                "workspace": str(snapshot.workspace),
                "begin_patch": "undo/begin.patch",
                "begin_untracked": "undo/begin.untracked.txt",
                "end_patch": "undo/end.patch",
                "end_untracked": "undo/end.untracked.txt",
                "executed_tools": list(snapshot.executed_tools),
                "irreversible_tools": list(dict.fromkeys(snapshot.irreversible_tools)),
            },
        )
        snapshot.completed = True

    @staticmethod
    def _write_snapshot_file(path: Path, content: bytes) -> None:
        """原子写入补丁文件，避免中途崩溃留下半截 undo 状态。"""

        temporary = path.with_name(path.name + ".tmp")
        temporary.write_bytes(content)
        temporary.replace(path)

    @staticmethod
    def _write_snapshot_untracked(path: Path, untracked: tuple[str, ...]) -> None:
        temporary = path.with_name(path.name + ".tmp")
        temporary.write_text("\n".join(untracked), encoding="utf-8")
        temporary.replace(path)

    def _restore_turn_side_effects(
        self,
        plan: SessionUndoPlan,
    ) -> Callable[[], None] | None:
        """预检并恢复计划中的快照，返回在 Session 提交失败时使用的反向恢复。"""

        snapshot_events = [event for event in plan.events if event.type == "turn_snapshot"]
        if not snapshot_events:
            potential_side_effects = []
            requested_calls = {
                str(event.payload.get("tool_call_id") or ""): event.payload
                for event in plan.events
                if event.type == "tool_call_requested"
                and str(event.payload.get("tool_call_id") or "")
            }
            for event in plan.events:
                if event.type == "compact_summary":
                    potential_side_effects.append("context_compaction")
                if event.type != "tool_result" or event.payload.get("ok") is False:
                    continue
                tool_name = str(event.payload.get("tool") or "").strip()
                request_payload = requested_calls.get(
                    str(event.payload.get("tool_call_id") or ""),
                    {},
                )
                arguments = request_payload.get("arguments", {})
                if not isinstance(arguments, dict):
                    arguments = {}
                if tool_name and not self._tool_is_undo_safe(tool_name, arguments):
                    potential_side_effects.append(tool_name)
                elif tool_name in _REVERSIBLE_UNDO_TOOLS:
                    potential_side_effects.append(tool_name)
            if potential_side_effects:
                names = "、".join(dict.fromkeys(potential_side_effects))
                raise AgentError(
                    "该旧轮次存在副作用但没有 Git 快照，已拒绝回退：" + names
                )
            return None
        if len(snapshot_events) != 1:
            raise AgentError("当前轮次包含多个 Git 快照事件，无法安全回退。")

        payload = snapshot_events[0].payload
        if payload.get("version") != 2:
            raise AgentError(
                "该轮次使用旧版影子对象库快照（version 1），已随 git diff "
                "重构移除，无法自动回退；请手工还原文件后重试。"
            )
        irreversible = payload.get("irreversible_tools", [])
        if not isinstance(irreversible, list):
            raise AgentError("轮次快照的不可逆工具账本格式无效。")
        blocker_names = [str(name).strip() for name in irreversible if str(name).strip()]
        if blocker_names:
            raise AgentError(
                "该轮执行了无法由 Git 证明可逆的操作，已拒绝整轮回退："
                + "、".join(dict.fromkeys(blocker_names))
            )

        # 快照与工作区绑定：会话跨工作区恢复时，禁止把补丁应用到别的目录。
        recorded_workspace = str(payload.get("workspace") or "").strip()
        current_workspace = self.workspace_root.resolve()
        if recorded_workspace:
            try:
                same_workspace = Path(recorded_workspace).resolve() == current_workspace
            except OSError:
                same_workspace = False
            if not same_workspace:
                raise AgentError(
                    "该轮次的工作区与当前工作区不一致，已拒绝回退。"
                )

        try:
            session_store = self._session_facade().require_session_store()
            artifact_root = session_store.artifacts_dir.resolve()
            before = self._load_workspace_snapshot(
                artifact_root, plan.session_id, payload, "begin"
            )
            after = self._load_workspace_snapshot(
                artifact_root, plan.session_id, payload, "end"
            )
        except SnapshotError as exc:
            raise AgentError(f"副作用回退失败：{exc}") from exc

        snapshot_store = WorktreeSnapshotStore()
        try:
            unrestorable = snapshot_store.transition(
                current_workspace, expected=after, target=before
            )
        except SnapshotError as exc:
            raise AgentError(f"副作用回退冲突或失败：{exc}") from exc
        if unrestorable:
            LOGGER.warning(
                "本轮被删除的未跟踪文件无内容副本，无法恢复：%s",
                "、".join(unrestorable),
            )

        def rollback() -> None:
            snapshot_store.transition(
                current_workspace, expected=before, target=after
            )

        return rollback

    def _load_workspace_snapshot(
        self,
        artifact_root: Path,
        session_id: str,
        payload: Mapping[str, Any],
        prefix: str,
    ) -> WorktreeSnapshot:
        """从 turn_snapshot 事件读取补丁与未跟踪清单文件。

        payload 中存的是相对 artifacts 根的 POSIX 路径（如
        ``undo/begin.patch``）；读取前校验其解析结果仍位于
        ``artifacts/<session_id>`` 内，防止被篡改的事件用 ``..`` 越界。
        """

        patch_relative = str(payload.get(f"{prefix}_patch") or "").strip()
        untracked_relative = str(payload.get(f"{prefix}_untracked") or "").strip()
        if not patch_relative or not untracked_relative:
            raise SnapshotError(f"轮次快照缺少 {prefix} 补丁文件引用。")
        session_artifacts = (artifact_root / session_id).resolve()
        patch_path = self._resolve_artifact_path(session_artifacts, patch_relative)
        untracked_path = self._resolve_artifact_path(
            session_artifacts, untracked_relative
        )
        try:
            patch = patch_path.read_bytes()
            untracked = tuple(
                line
                for line in untracked_path.read_text(encoding="utf-8").splitlines()
                if line.strip()
            )
        except OSError as exc:
            raise SnapshotError(f"读取轮次快照文件失败：{exc}") from exc
        return WorktreeSnapshot(patch, untracked, True)

    @staticmethod
    def _resolve_artifact_path(root: Path, relative: str) -> Path:
        """把事件中的相对路径解析为 root 内的绝对路径（防目录穿越）。"""

        value = relative.replace("\\", "/").strip("/")
        candidate = PurePosixPath(value)
        if not value or candidate.is_absolute() or ".." in candidate.parts:
            raise SnapshotError(f"快照文件路径无效：{relative}")
        path = (root / candidate.as_posix()).resolve()
        if not UndoMixin._is_relative_to(path, root):
            raise SnapshotError(f"快照文件越出会话目录：{relative}")
        return path

    def _is_session_path(self, path: Path) -> bool:
        """普通文件工具不直接访问会话目录，避免模型误写转录文件。"""

        if getattr(self, "_session_store", None) is None:
            return False
        try:
            resolved = path.resolve()
        except OSError:
            resolved = path
        return resolved == self._session_store.root or self._is_relative_to(resolved, self._session_store.root)

    @staticmethod
    def _is_relative_to(path: Path, parent: Path) -> bool:
        try:
            path.relative_to(parent)
            return True
        except ValueError:
            return False
