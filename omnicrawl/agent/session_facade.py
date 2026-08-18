"""Agent 子系统内部模块。

本文件由原合并入口按既有模块边界恢复，职责说明见模块内公开对象。
"""

from __future__ import annotations

import re
from pathlib import Path
from typing import Any

from .history import restore_history_window
from ..project import ProjectEntry, ProjectStore, ProjectStoreError
from ..session import (
    PromptHistoryEntry,
    SessionEvent,
    SessionEventReadResult,
    SessionIndexEntry,
    SessionState,
    SessionStore,
    SessionStoreError,
)


def project_directory_name(name: str) -> str:
    """把项目展示名转换为适合创建目录的保守名称。"""

    cleaned = re.sub(r"[<>:\"/\\|?*\x00-\x1f]+", "-", name.strip())
    cleaned = re.sub(r"\s+", "-", cleaned).strip(" .-")
    return cleaned or "new-project"


class AgentSessionFacade:
    """会话、项目和提示历史门面。

    这个类仍然直接操作 `LocalToolAgent` 的运行态字段，是阶段性拆分的兼容层：
    对外 API 继续留在 `LocalToolAgent`，但会话存取、项目列表和提示历史的薄包装逻辑
    集中到这里，后续再逐步收紧为更明确的状态对象。
    """

    def __init__(self, owner: Any, error_type: type[Exception] = RuntimeError) -> None:
        self._owner = owner
        self._error_type = error_type

    @property
    def workspace_root(self) -> Path:
        return self._owner.workspace_root

    def create_session_store(self) -> SessionStore:
        """创建全局会话存储，统一位于用户数据根目录，不绑定工作区。

        相对路径相对于 ``~/.omnicrawl`` 解析（默认 ``.agent_sessions``
        即 ``~/.omnicrawl/.agent_sessions``）；绝对路径直接使用。
        会话目录不再位于工作区内，因此切换工作区时无需重建会话存储。
        """

        raw_directory = self._owner.config.session_directory.strip()
        candidate = Path(raw_directory).expanduser()
        if not candidate.is_absolute():
            candidate = self._owner._memory_user_data_root() / candidate
        store = SessionStore(candidate.resolve())
        try:
            store.ensure()
        except SessionStoreError as exc:
            raise self._error_type(str(exc)) from exc
        return store

    def create_project_store(self) -> ProjectStore:
        """创建项目列表存储，复用会话目录作为持久化根。"""

        store = self.require_session_store()
        project_store = ProjectStore(store.root)
        try:
            project_store.ensure()
        except ProjectStoreError as exc:
            raise self._error_type(str(exc)) from exc
        return project_store

    def current_session_id(self) -> str:
        """当前会话 ID；会话系统关闭时返回空字符串。"""

        state = getattr(self._owner, "_session_state", None)
        return state.session_id if state is not None else ""

    def require_session_store(self) -> SessionStore:
        store = getattr(self._owner, "_session_store", None)
        if store is None:
            raise self._error_type("会话系统未启用。")
        return store

    def require_project_store(self) -> ProjectStore:
        store = getattr(self._owner, "_project_store", None)
        if store is None:
            raise self._error_type("项目列表需要启用会话系统。")
        return store

    def start_session(self) -> SessionState:
        store = self.require_session_store()
        try:
            return store.start_session(self.workspace_root)
        except SessionStoreError as exc:
            raise self._error_type(str(exc)) from exc

    def start_or_resume_session(self) -> SessionState:
        """按启动参数恢复指定会话；未指定时创建新会话。"""

        resume_session_id = self._owner.config.resume_session_id.strip()
        if not resume_session_id:
            return self.start_session()
        return self.resume_session(resume_session_id)

    def list_sessions(
        self,
        limit: int = 10,
        *,
        project_path: str | Path | None = None,
    ) -> list[SessionIndexEntry]:
        """列出全部会话（不再绑定当前工作区）；可按项目路径过滤。"""

        store = self.require_session_store()
        try:
            return store.list_sessions(
                project_path=project_path,
                limit=limit,
            )
        except SessionStoreError as exc:
            raise self._error_type(str(exc)) from exc

    def scan_projects(self) -> list[ProjectEntry]:
        """从会话索引扫描项目路径并写入项目列表。"""

        session_store = self.require_session_store()
        project_store = self.require_project_store()
        try:
            return project_store.scan_projects(
                session_store.list_project_paths(include_archived=True),
                current_workspace=self.workspace_root,
            )
        except (ProjectStoreError, SessionStoreError) as exc:
            raise self._error_type(str(exc)) from exc

    def list_projects(self) -> list[ProjectEntry]:
        """列出已保存项目；读取前先扫描会话索引补齐缺失项目。"""

        self.scan_projects()
        project_store = self.require_project_store()
        try:
            return project_store.list_projects()
        except ProjectStoreError as exc:
            raise self._error_type(str(exc)) from exc

    def create_project(self, name: str, path: str = "") -> ProjectEntry:
        """创建项目目录并持久化到项目列表。"""

        project_store = self.require_project_store()
        project_path = (
            Path(path.strip())
            if path.strip()
            else self.workspace_root / project_directory_name(name)
        )
        try:
            return project_store.create_project(name=name, path=project_path)
        except ProjectStoreError as exc:
            raise self._error_type(str(exc)) from exc

    def import_project(self, name: str, path: str) -> ProjectEntry:
        """导入已有项目目录并持久化到项目列表。"""

        project_store = self.require_project_store()
        try:
            return project_store.import_project(name=name, path=path)
        except ProjectStoreError as exc:
            raise self._error_type(str(exc)) from exc

    def rename_project(self, project_path: str, name: str) -> ProjectEntry:
        """修改项目展示名，不改动磁盘目录。"""

        project_store = self.require_project_store()
        try:
            return project_store.rename_project(path=project_path, name=name)
        except ProjectStoreError as exc:
            raise self._error_type(str(exc)) from exc

    def pin_project(self, project_path: str, *, pinned: bool = True) -> ProjectEntry:
        """设置项目置顶状态。"""

        project_store = self.require_project_store()
        try:
            return project_store.pin_project(project_path, pinned=pinned)
        except ProjectStoreError as exc:
            raise self._error_type(str(exc)) from exc

    def toggle_project_pin(self, project_path: str) -> ProjectEntry:
        """切换项目置顶状态。"""

        project_store = self.require_project_store()
        try:
            return project_store.toggle_project_pin(project_path)
        except ProjectStoreError as exc:
            raise self._error_type(str(exc)) from exc

    def remove_project(self, project_path: str) -> None:
        """从项目列表移除项目记录，不删除目录和会话。"""

        project_store = self.require_project_store()
        try:
            project_store.remove_project(project_path)
        except ProjectStoreError as exc:
            raise self._error_type(str(exc)) from exc

    def list_archived_sessions(self, limit: int = 10) -> list[SessionIndexEntry]:
        """列出全部已归档会话（不再绑定当前工作区）。"""

        store = self.require_session_store()
        try:
            return store.list_sessions(
                limit=limit,
                archived_only=True,
            )
        except SessionStoreError as exc:
            raise self._error_type(str(exc)) from exc

    def load_session_events(self, session_id: str) -> list[SessionEvent]:
        """读取指定会话的原始事件流，供 UI 恢复完整转录。"""

        return list(self.load_session_events_with_diagnostics(session_id).events)

    def load_session_events_with_diagnostics(
        self,
        session_id: str,
    ) -> SessionEventReadResult:
        """读取事件流并返回损坏/版本诊断，供 API 与管理入口展示。"""

        store = self.require_session_store()
        try:
            store.load_session(session_id)
            return store.read_session_events_with_diagnostics(session_id)
        except SessionStoreError as exc:
            raise self._error_type(str(exc)) from exc

    def load_session_diagnostics(
        self,
        session_id: str | None = None,
    ) -> dict[str, Any]:
        """汇总会话转录与提示历史诊断，供最小可见入口使用。"""

        store = self.require_session_store()
        try:
            prompt_diagnostics = store.read_prompt_history_diagnostics()
            if session_id is None or not str(session_id).strip():
                return {
                    "session_id": None,
                    "event_diagnostics": [],
                    "prompt_history_diagnostics": [item.to_dict() for item in prompt_diagnostics],
                }

            result = self.load_session_events_with_diagnostics(session_id)
            return {
                "session_id": session_id.strip(),
                "event_count": len(result.events),
                "event_diagnostics": [item.to_dict() for item in result.diagnostics],
                "prompt_history_diagnostics": [item.to_dict() for item in prompt_diagnostics],
                "has_errors": result.has_errors,
            }
        except SessionStoreError as exc:
            raise self._error_type(str(exc)) from exc

    def read_session_artifact_text(self, session_id: str, artifact_path: str) -> str:
        """读取当前会话存储根目录下的文本 artifact。"""

        store = self.require_session_store()
        try:
            return store.read_artifact_text(session_id, artifact_path)
        except SessionStoreError as exc:
            raise self._error_type(str(exc)) from exc

    def prepare_subagent_result(
        self,
        *,
        task_id: str,
        agent_type: str,
        description: str,
        result_text: str,
        summary_chars: int,
    ) -> dict[str, Any]:
        """使用当前父 Session 的 artifact 策略处理子任务完整结果。"""

        state = self._require_session_state()
        store = self.require_session_store()
        try:
            return store.prepare_subagent_result(
                state.session_id,
                task_id=task_id,
                agent_type=agent_type,
                description=description,
                result_text=result_text,
                summary_chars=summary_chars,
            )
        except SessionStoreError as exc:
            raise self._error_type(str(exc)) from exc

    def undo_last_turn(self) -> SessionState:
        """原子恢复最近轮次副作用，成功后再提交会话逻辑回退。"""

        state = self._require_session_state()
        store = self.require_session_store()
        try:
            plan = store.prepare_undo_last_turn(state.session_id)
        except SessionStoreError as exc:
            raise self._error_type(str(exc)) from exc

        restore_side_effects = getattr(self._owner, "_restore_turn_side_effects", None)
        rollback_side_effects = None
        if callable(restore_side_effects):
            rollback_side_effects = restore_side_effects(plan)
        try:
            restored = store.commit_undo_plan(
                plan,
                side_effects_reverted=rollback_side_effects is not None,
            )
        except SessionStoreError as exc:
            if rollback_side_effects is not None:
                try:
                    rollback_side_effects()
                except Exception as rollback_exc:
                    raise self._error_type(
                        f"会话回退提交失败，且副作用反向恢复失败：{rollback_exc}"
                    ) from exc
            raise self._error_type(str(exc)) from exc

        self._owner._session_state = restored
        self._owner._history = restore_history_window(
            restored.messages,
            max_history_turns=self._owner.config.max_history_turns,
        )
        self._owner._pending_user_text = None
        self._owner._active_skills = []
        return restored

    def rename_current_session(self, title: str) -> SessionState:
        """重命名当前会话，并同步更新内存中的 `SessionState`。"""

        state = self._require_session_state()
        store = self.require_session_store()
        try:
            renamed_state = store.rename_session(state.session_id, title)
        except SessionStoreError as exc:
            raise self._error_type(str(exc)) from exc
        self._owner._session_state = SessionState(
            session_id=renamed_state.session_id,
            title=renamed_state.title,
            workspace_root=renamed_state.workspace_root,
            path=renamed_state.path,
            created_at=renamed_state.created_at,
            updated_at=renamed_state.updated_at,
            messages=self._owner._history,
            last_event_type=renamed_state.last_event_type,
            event_count=renamed_state.event_count,
            archived_at=renamed_state.archived_at,
        )
        return self._owner._session_state

    def archive_current_session(self) -> SessionState:
        """归档当前会话，并立即开启一个新的空会话。"""

        state = self._require_session_state()
        store = self.require_session_store()
        cancel_subagents = getattr(
            self._owner,
            "_cancel_subagents_for_session_transition",
            None,
        )
        if callable(cancel_subagents):
            cancel_subagents("父 Session 即将归档，当前子任务已取消。")
        try:
            archived_state = store.archive_session(state.session_id)
        except SessionStoreError as exc:
            raise self._error_type(str(exc)) from exc
        self._clear_runtime_context()
        self._owner._session_state = self.start_session()
        bind_memory = getattr(self._owner, "_bind_current_session_memory_store", None)
        if callable(bind_memory):
            bind_memory()
        return archived_state

    def delete_session(self, session_id: str) -> None:
        """删除指定会话。当前活跃会话不允许删除。"""

        state = self._require_session_state()
        if session_id == state.session_id:
            raise self._error_type("不能删除当前活跃会话，请先切换到其他会话。")
        store = self.require_session_store()
        try:
            store.delete_session(session_id)
            delete_memory = getattr(self._owner, "_delete_session_memory", None)
            if callable(delete_memory):
                delete_memory(session_id)
        except SessionStoreError as exc:
            raise self._error_type(str(exc)) from exc

    def export_current_session_markdown(self, markdown_text: str) -> Path:
        """导出当前会话 Markdown 到 `.agent_sessions/exports/`。"""

        state = self._require_session_state()
        store = self.require_session_store()
        try:
            path = store.export_session_markdown(state.session_id, markdown_text)
            self._owner._session_state = store.load_session(state.session_id)
        except SessionStoreError as exc:
            raise self._error_type(str(exc)) from exc
        return path

    def search_prompt_history(
        self,
        *,
        query: str = "",
        limit: int = 20,
        current_session_only: bool = False,
    ) -> list[PromptHistoryEntry]:
        """查询全部用户提示历史（不再绑定当前工作区），供输入复用和 `/history` 展示。"""

        store = self.require_session_store()
        session_id = self.current_session_id() if current_session_only else None
        try:
            return store.search_prompt_history(
                session_id=session_id,
                query=query,
                limit=limit,
            )
        except SessionStoreError as exc:
            raise self._error_type(str(exc)) from exc

    def prompt_history_texts(self, limit: int = 100) -> list[str]:
        """返回按时间正序排列的提示文本，作为 TUI 上箭头历史种子。"""

        entries = self.search_prompt_history(limit=limit)
        return [entry.display for entry in reversed(entries)]

    def resume_session(self, session_id: str) -> SessionState:
        """恢复指定会话，并用转录消息重建 `_history`。

        会话已解除工作区绑定：任意工作区下均可恢复任何会话。
        """

        store = self.require_session_store()
        try:
            state = store.load_session(session_id)
        except SessionStoreError as exc:
            raise self._error_type(str(exc)) from exc

        # 先证明目标会话有效，再取消当前会话子任务；无效 ID 不应打断仍合法
        # 运行的后台任务。取消完成后才清理启动占位会话。
        cancel_subagents = getattr(
            self._owner,
            "_cancel_subagents_for_session_transition",
            None,
        )
        if callable(cancel_subagents):
            cancel_subagents("父 Session 即将切换，当前子任务已取消。")
        self.discard_current_empty_session()

        if state.archived_at is not None:
            try:
                state = store.unarchive_session(session_id)
            except SessionStoreError as exc:
                raise self._error_type(str(exc)) from exc
        self._owner._session_state = state
        self._owner._history = restore_history_window(
            state.messages,
            max_history_turns=self._owner.config.max_history_turns,
        )
        self._owner._pending_user_text = None
        self._owner._active_skills = []
        bind_memory = getattr(self._owner, "_bind_current_session_memory_store", None)
        if callable(bind_memory):
            bind_memory()
        # 运行时 /session resume 也要恢复跨进程 SubAgent 控制面快照。
        importer = getattr(self._owner, "import_recovered_subagent_tasks", None)
        if callable(importer):
            try:
                importer()
            except Exception:
                # 恢复失败不得阻断会话切换；任务列表可为空，用户仍能继续对话。
                pass
        return state

    def append_session_event(self, event_type: str, payload: dict[str, Any]) -> None:
        """追加会话事件；持久化失败时中断当前任务，避免误以为会话可恢复。"""

        store = getattr(self._owner, "_session_store", None)
        state = getattr(self._owner, "_session_state", None)
        if store is None or state is None:
            return
        try:
            event = store.append_event(state.session_id, event_type, payload)
            self._owner._session_state = SessionState(
                session_id=state.session_id,
                title=state.title,
                workspace_root=state.workspace_root,
                path=state.path,
                created_at=state.created_at,
                updated_at=event.created_at,
                messages=state.messages,
                last_event_type=event.type,
                event_count=state.event_count + 1,
                archived_at=state.archived_at,
            )
        except SessionStoreError as exc:
            raise self._error_type(str(exc)) from exc

    def discard_current_empty_session(self) -> bool:
        """清理启动后未产生真实内容的占位会话。"""

        store = getattr(self._owner, "_session_store", None)
        state = getattr(self._owner, "_session_state", None)
        if store is None or state is None:
            return False
        try:
            discarded = store.discard_empty_session(state.session_id)
        except SessionStoreError as exc:
            raise self._error_type(str(exc)) from exc
        if discarded:
            self._owner._session_state = None
        return discarded

    def append_prompt_history(self, text: str) -> None:
        """记录用户提交的真实提示，用于跨会话输入复用。"""

        store = getattr(self._owner, "_session_store", None)
        state = getattr(self._owner, "_session_state", None)
        if store is None or state is None:
            return
        try:
            store.append_prompt_history(
                display=text,
                workspace_root=self.workspace_root,
                session_id=state.session_id,
            )
        except SessionStoreError as exc:
            raise self._error_type(str(exc)) from exc

    def _require_session_state(self) -> SessionState:
        state = getattr(self._owner, "_session_state", None)
        if state is None:
            raise self._error_type("会话系统未启用。")
        return state

    def _clear_runtime_context(self) -> None:
        self._owner._history.clear()
        self._owner._pending_user_text = None
        self._owner._active_skills = []


def _is_relative_to(path: Path, parent: Path) -> bool:
    try:
        path.relative_to(parent)
        return True
    except ValueError:
        return False
