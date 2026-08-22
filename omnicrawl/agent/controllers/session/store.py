"""会话/项目/归档存取与存储工厂。

薄包装 ``AgentSessionFacade`` 的会话门面，加上会话、项目与 MCP 资源工厂；
跨工作区切换复用的 ``_start_session`` 等也收在这里。"""
from __future__ import annotations

import logging
from pathlib import Path, PurePosixPath
from typing import Any, Callable, Mapping, Sequence
from ....mcp import MCPClientManager, MCPConfig, MCPConfigError, MCPToolMeta, load_mcp_config
from ....project import ProjectEntry, ProjectStore
from ....session import (
    COMPACT_SUMMARY_PREFIX,
    PromptHistoryEntry,
    SessionIndexEntry,
    SessionEvent,
    SessionEventReadResult,
    SessionState,
    SessionStore,
    SessionUndoPlan,
)

from ..shared import (
    AgentError,
    SUBAGENT_LIFECYCLE_WAIT_SECONDS,
)

LOGGER = logging.getLogger(__name__)


class SessionStoreMixin:
    """会话/项目/归档存取与存储工厂。"""

    def reset_conversation(self) -> None:
        """开启新对话：清空对话历史并创建新会话，保留工具、记忆和 Skill 配置。"""

        self._history.clear()
        self._pending_user_text = None
        self._active_skills = []
        if self._session_store is not None:
            self._session_facade().discard_current_empty_session()
            self._session_state = self._start_session()
            self._bind_current_session_memory_store()

    @property
    def current_session_id(self) -> str:
        """当前会话 ID；会话系统关闭时返回空字符串。"""

        return self._session_facade().current_session_id()

    def current_session_messages(self) -> list[dict[str, str]]:
        """返回当前会话投影后的模型上下文消息，供非事件型客户端读取。

        与 `resume_session` 重建的 `_history` 同源（来自 JSONL 转录投影），
        但保留完整会话内容而不受 `max_history_turns` 窗口裁剪；会话系统
        关闭或尚未创建会话时返回空列表。TUI 需要保留工具卡和子任务树时，
        应优先使用 `current_session_events()`。
        """

        state = getattr(self, "_session_state", None)
        if state is None:
            return []
        return list(state.messages)

    def current_session_events(self) -> list[SessionEvent]:
        """返回当前会话的有效事件流，供 UI 按原始事件恢复展示层。

        `SessionState.messages` 是面向模型的投影，会把工具请求/结果压成
        assistant 文本，无法据此恢复工具卡、耗时和 SubAgent 进度树；事件流
        才是 TUI 历史页面的权威渲染输入。没有启用会话系统时返回空列表。
        """

        state = getattr(self, "_session_state", None)
        if state is None or getattr(self, "_session_store", None) is None:
            return []
        return self.load_session_events(state.session_id)

    def list_sessions(
        self,
        limit: int = 10,
        *,
        project_path: str | Path | None = None,
    ) -> list[SessionIndexEntry]:
        """列出指定项目或当前工作区最近会话，供 `/sessions` 和项目侧栏展示。"""

        return self._session_facade().list_sessions(limit=limit, project_path=project_path)

    def scan_projects(self) -> list[ProjectEntry]:
        """从会话索引扫描项目路径并写入 `.agent_sessions/projects.json`。"""

        return self._session_facade().scan_projects()

    def list_projects(self) -> list[ProjectEntry]:
        """列出已保存项目；每次读取前先扫描会话索引补齐缺失项目。"""

        return self._session_facade().list_projects()

    def create_project(self, name: str, path: str = "") -> ProjectEntry:
        """创建项目目录并持久化到项目列表。"""

        return self._session_facade().create_project(name, path)

    def import_project(self, name: str, path: str) -> ProjectEntry:
        """导入已有项目目录并持久化到项目列表。"""

        return self._session_facade().import_project(name, path)

    def rename_project(self, project_path: str, name: str) -> ProjectEntry:
        """修改项目展示名，不改动磁盘目录。"""

        return self._session_facade().rename_project(project_path, name)

    def pin_project(self, project_path: str, *, pinned: bool = True) -> ProjectEntry:
        """设置项目置顶状态。"""

        return self._session_facade().pin_project(project_path, pinned=pinned)

    def toggle_project_pin(self, project_path: str) -> ProjectEntry:
        """切换项目置顶状态。"""

        return self._session_facade().toggle_project_pin(project_path)

    def remove_project(self, project_path: str) -> None:
        """从项目列表移除项目记录，不删除目录和会话。"""

        self._session_facade().remove_project(project_path)

    def list_archived_sessions(self, limit: int = 10) -> list[SessionIndexEntry]:
        """列出当前工作区已归档会话，供 `/archives` 展示。"""

        return self._session_facade().list_archived_sessions(limit)

    def load_session_events(self, session_id: str) -> list[SessionEvent]:
        """读取指定会话的原始事件流，供客户端恢复完整消息列表。

        `_history` 只保留模型上下文窗口；客户端需要完整转录，因此这里通过
        明确方法暴露只读事件，而不是让 UI 层直接访问 `.agent_sessions/` 文件。
        """

        return self._session_facade().load_session_events(session_id)

    def load_session_events_with_diagnostics(
        self,
        session_id: str,
    ) -> SessionEventReadResult:
        """读取事件流并返回版本/损坏诊断。"""

        return self._session_facade().load_session_events_with_diagnostics(session_id)

    def load_session_diagnostics(
        self,
        session_id: str | None = None,
    ) -> dict[str, Any]:
        """汇总会话与提示历史诊断，供 API 最小可见入口使用。"""

        return self._session_facade().load_session_diagnostics(session_id)

    def read_session_artifact_text(self, session_id: str, artifact_path: str) -> str:
        """读取会话 artifact 文本，供 API 客户端恢复 HTML 预览。"""

        return self._session_facade().read_session_artifact_text(session_id, artifact_path)

    def undo_last_turn(self) -> SessionState:
        """持久化回退最近一轮对话，并重建当前模型上下文。"""

        return self._session_facade().undo_last_turn()

    def rename_current_session(self, title: str) -> SessionState:
        """重命名当前会话，并同步更新内存中的 `SessionState`。"""

        return self._session_facade().rename_current_session(title)

    def archive_current_session(self) -> SessionState:
        """归档当前会话，并立即开启一个新的空会话。

        当前会话一旦归档，就不应继续接收新的用户输入；因此这里保留已归档
        state 作为返回值，同时把 Agent 切到新会话，避免下一轮消息写到归档文件。
        """

        return self._session_facade().archive_current_session()

    def delete_session(self, session_id: str) -> None:
        """删除指定会话。当前活跃会话不允许删除。"""

        self._session_facade().delete_session(session_id)

    def export_current_session_markdown(self, markdown_text: str) -> Path:
        """导出当前会话 Markdown 到 `.agent_sessions/exports/`。"""

        return self._session_facade().export_current_session_markdown(markdown_text)

    def search_prompt_history(
        self,
        *,
        query: str = "",
        limit: int = 20,
        current_session_only: bool = False,
    ) -> list[PromptHistoryEntry]:
        """查询当前工作区的用户提示历史，供输入复用和 `/history` 展示。"""

        return self._session_facade().search_prompt_history(
            query=query,
            limit=limit,
            current_session_only=current_session_only,
        )

    def prompt_history_texts(self, limit: int = 100) -> list[str]:
        """返回按时间正序排列的提示文本，作为 TUI 上箭头历史种子。"""

        return self._session_facade().prompt_history_texts(limit)

    def resume_session(self, session_id: str) -> SessionState:
        """恢复指定会话，并用转录消息重建 `_history`。"""

        state = self._session_facade().resume_session(session_id)
        self._bind_current_session_memory_store()
        return state

    def _cancel_subagents_for_session_transition(self, reason: str) -> None:
        """在归档/恢复父 Session 前取消旧会话的全部子任务。"""

        coordinator = getattr(self, "_subagent_coordinator", None)
        if coordinator is None:
            return
        try:
            drained = coordinator.cancel_and_wait(
                reason=reason,
                timeout_seconds=SUBAGENT_LIFECYCLE_WAIT_SECONDS,
                permanent=False,
            )
        except BaseException:
            # Session 尚未切换，旧 Coordinator 必须恢复接单能力；否则一次取消
            # 异常会让当前会话永久停在 paused 状态。
            coordinator.resume_accepting_when_idle()
            raise
        if not drained:
            coordinator.resume_accepting_when_idle()
            raise AgentError(
                "父 Session 切换失败：仍有 SubAgent 子任务未在期限内退出，"
                "已保留当前会话和共享资源。"
            )
        # Session 切换复用同一 Coordinator/TaskManager；与工作区切换不同，
        # 不会创建新实例，因此成功取消后也必须显式恢复后续任务接收。
        coordinator.resume_accepting_when_idle()

    def _create_session_store(self) -> SessionStore:
        """创建会话存储，并限制在工作区内。"""

        return self._session_facade().create_session_store()

    def _create_project_store(self) -> ProjectStore:
        """创建项目列表存储，复用会话目录作为持久化根。"""

        return self._session_facade().create_project_store()

    def _start_session(self) -> SessionState:
        return self._session_facade().start_session()

    def _start_or_resume_session(self) -> SessionState:
        """按启动参数恢复指定会话；未指定时创建新会话。"""

        return self._session_facade().start_or_resume_session()

    def _create_mcp_manager(self) -> MCPClientManager:
        """加载并初始化 MCP Client Manager。

        MCP 是增量能力：配置关闭时不影响内置工具。这里仅校验配置并创建
        Manager，能力发现延后到首次对话或用户查看 `/mcp` 时执行，避免
        stdio Server 启动阻塞交互入口。
        """

        try:
            mcp_config = self.config.mcp_config or load_mcp_config()
            manager = MCPClientManager(
                mcp_config,
                workspace_root=self.workspace_root,
                approval_mode_getter=lambda: self.config.approval_mode,
            )
            return manager
        except MCPConfigError as exc:
            raise AgentError(str(exc)) from exc

    def _require_session_store(self) -> SessionStore:
        return self._session_facade().require_session_store()

    def _require_project_store(self) -> ProjectStore:
        return self._session_facade().require_project_store()

    def _append_session_event(self, event_type: str, payload: dict[str, Any]) -> None:
        """追加会话事件；持久化失败时中断当前任务，避免误以为会话可恢复。"""

        self._session_facade().append_session_event(event_type, payload)
