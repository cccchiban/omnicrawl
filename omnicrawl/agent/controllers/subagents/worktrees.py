"""SubAgent worktree 会话控制面：登记、查询、应用与丢弃。"""
from __future__ import annotations

from typing import Any, Callable, Mapping, Sequence
from ...subagents.execution import (
    FORK_BOILERPLATE,
    SubAgentExecutionContext,
    SubAgentModelSnapshot,
)
from ...subagents.worktree import (
    WorktreeError,
    WorktreeSession,
    apply_worktree_to_main,
    cleanup_worktree_session,
    collect_worktree_artifacts,
    create_worktree_session,
)

from ..shared import (
    AgentError,
)


class SubAgentWorktreeMixin:
    """SubAgent worktree 会话控制面：登记、查询、应用与丢弃。"""

    def _register_subagent_worktree_session(self, session: WorktreeSession) -> None:
        """登记 worktree 会话，供父 Agent 后续 apply / discard。"""

        sessions = getattr(self, "_subagent_worktree_sessions", None)
        if sessions is None:
            self._subagent_worktree_sessions = {}
            sessions = self._subagent_worktree_sessions
        lock = getattr(self, "_subagent_worktree_lock", None)
        if lock is None:
            sessions[session.branch_name] = session
            sessions[session.task_id] = session
            return
        with lock:
            sessions[session.branch_name] = session
            sessions[session.task_id] = session

    def _lookup_subagent_worktree_session(self, key: str) -> WorktreeSession | None:
        """按 task_id 或 branch_name 查找 worktree 会话。"""

        sessions = getattr(self, "_subagent_worktree_sessions", {}) or {}
        lock = getattr(self, "_subagent_worktree_lock", None)
        token = str(key or "").strip()
        if lock is None:
            return sessions.get(token)
        with lock:
            return sessions.get(token)

    def _collect_subagent_worktree_artifacts(
        self,
        execution_context: SubAgentExecutionContext | None,
    ) -> tuple[str, ...]:
        """收集 worktree 变更摘要，供父 Agent 审查。"""

        if execution_context is None:
            return ()
        session = getattr(execution_context, "worktree_session", None)
        if session is None:
            return ()
        try:
            artifacts = collect_worktree_artifacts(session)
        except WorktreeError as exc:
            return (f"worktree 产物收集失败：{exc}",)
        lines = [
            f"branch={artifacts.branch_name}",
            f"worktree={artifacts.worktree_path}",
            f"base_ref={artifacts.base_ref}",
            f"has_changes={artifacts.has_changes}",
        ]
        if artifacts.changed_files:
            preview = ", ".join(artifacts.changed_files[:20])
            if len(artifacts.changed_files) > 20:
                preview += f" ...(+{len(artifacts.changed_files) - 20})"
            lines.append(f"changed_files={preview}")
        if artifacts.diff_stat:
            lines.append(f"diff_stat={artifacts.diff_stat}")
        if artifacts.diff_text:
            preview = artifacts.diff_text[:4000]
            if len(artifacts.diff_text) > 4000:
                preview += "\n... diff 已截断 ..."
            lines.append("diff_preview:")
            lines.append(preview)
        return tuple(lines)

    def list_subagent_worktrees(self) -> list[dict[str, Any]]:
        """列出当前进程内登记的 SubAgent worktree 会话（去重）。"""

        sessions = getattr(self, "_subagent_worktree_sessions", {}) or {}
        lock = getattr(self, "_subagent_worktree_lock", None)
        if lock is None:
            values = list(sessions.values())
        else:
            with lock:
                values = list(sessions.values())
        seen: set[str] = set()
        items: list[dict[str, Any]] = []
        for session in values:
            branch = getattr(session, "branch_name", "")
            if not branch or branch in seen:
                continue
            seen.add(branch)
            items.append(
                {
                    "task_id": getattr(session, "task_id", ""),
                    "branch": branch,
                    "worktree_path": str(getattr(session, "worktree_path", "")),
                    "base_ref": getattr(session, "base_ref", ""),
                    "repo_root": str(getattr(session, "repo_root", "")),
                }
            )
        return items

    def apply_subagent_worktree(
        self,
        key: str,
        *,
        strategy: str = "checkout",
        cleanup: bool = False,
    ) -> str:
        """把指定 SubAgent worktree 分支变更应用到主工作区。"""

        session = self._lookup_subagent_worktree_session(key)
        if session is None:
            raise AgentError(f"未找到 SubAgent worktree 会话：{key}")
        try:
            message = apply_worktree_to_main(session, strategy=strategy)
        except WorktreeError as exc:
            raise AgentError(f"应用 worktree 失败：{exc}") from exc
        if cleanup:
            self.discard_subagent_worktree(key)
        return message

    def discard_subagent_worktree(self, key: str, *, remove_branch: bool = True) -> str:
        """丢弃 worktree 会话并清理目录/分支。"""

        session = self._lookup_subagent_worktree_session(key)
        if session is None:
            raise AgentError(f"未找到 SubAgent worktree 会话：{key}")
        try:
            cleanup_worktree_session(session, remove_branch=remove_branch)
        except WorktreeError as exc:
            raise AgentError(f"清理 worktree 失败：{exc}") from exc
        sessions = getattr(self, "_subagent_worktree_sessions", {})
        lock = getattr(self, "_subagent_worktree_lock", None)
        if lock is None:
            sessions.pop(session.branch_name, None)
            sessions.pop(session.task_id, None)
        else:
            with lock:
                sessions.pop(session.branch_name, None)
                sessions.pop(session.task_id, None)
        return f"已清理 worktree 会话：{session.branch_name}"
