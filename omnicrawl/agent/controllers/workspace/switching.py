"""运行中工作区切换：准备新子系统、失败回滚、收尾旧资源。"""
from __future__ import annotations

from pathlib import Path, PurePosixPath
from typing import Any, Callable, Mapping, Sequence
from ....skill import SkillManager, SkillMatchResult
from ....temp_workspace import (
    AgentTempWorkspace,
    AgentTempWorkspaceConfig,
    AgentTempWorkspaceError,
    load_agent_temp_workspace_config,
)
from ....workspace_tools import (
    DEFAULT_COMMAND_TIMEOUT_SECONDS,
    MAX_COMMAND_TIMEOUT_SECONDS,
    WorkspaceToolError,
    WorkspaceTools,
)

from ..shared import (
    AgentError,
    SUBAGENT_LIFECYCLE_WAIT_SECONDS,
)


class WorkspaceSwitchingMixin:
    """运行中工作区切换：准备新子系统、失败回滚、收尾旧资源。"""

    def switch_workspace(self, new_path):
        """在运行中切换到新的工作区目录。

        切换工作区会完整重建 Agent 的子系统（工作区工具、临时目录、项目列表、
        记忆），并清空当前对话上下文。会话已全局化（不绑定工作区），切换时
        保持同一会话。原工作区会被记录到退出事件中，以便从 UI 项目列表恢复。

        参数：
            new_path: 新工作区的绝对或相对路径。

        返回：
            解析后的新工作区绝对路径。

        异常：
            AgentError：路径不存在、不是目录或子系统初始化失败时抛出。
        """

        try:
            candidate = Path(new_path).expanduser().resolve(strict=True)
        except OSError as exc:
            raise AgentError(f"工作区切换失败：{new_path} 无法解析，{exc}") from exc
        if not candidate.is_dir():
            raise AgentError(f"工作区切换失败：{candidate} 不是目录。")

        new_root = candidate.resolve()
        if new_root == self.workspace_root.resolve():
            return new_root

        old_root = self.workspace_root.resolve()
        switch_payload = self._dispatch_plugin_hook(
            "workspace.switch.before",
            {"from": str(old_root), "to": str(new_root)},
        )
        if switch_payload is None:
            raise AgentError("workspace.switch.before 被插件拒绝。")

        coordinator = getattr(self, "_subagent_coordinator", None)
        if coordinator is not None:
            drained = coordinator.cancel_and_wait(
                reason="工作区即将切换，当前子任务已取消。",
                timeout_seconds=SUBAGENT_LIFECYCLE_WAIT_SECONDS,
                permanent=False,
            )
            if not drained:
                # 不合作的 Provider/工具线程仍可能引用旧工作区资源。切换必须
                # 保持旧状态不动；待旧批次真正退出后 Coordinator 自动恢复接单。
                coordinator.resume_accepting_when_idle()
                raise AgentError(
                    "工作区切换失败：仍有 SubAgent 子任务未在期限内退出，"
                    "已保留原工作区和共享资源。"
                )

        # Worktree 是旧项目中的待决写入能力，不能静默带进新工作区：否则新项目
        # 的父 Agent 仍可 list/apply/discard 旧仓库分支，形成跨工作区控制面越权。
        # 这里选择阻止切换而不是自动删除，避免丢失尚未审查或应用的用户改动。
        pending_worktrees = self.list_subagent_worktrees()
        if pending_worktrees:
            if coordinator is not None:
                coordinator.resume_accepting_when_idle()
            branches = ", ".join(
                str(item.get("branch") or item.get("task_id") or "unknown")
                for item in pending_worktrees[:3]
            )
            if len(pending_worktrees) > 3:
                branches += f" ...(+{len(pending_worktrees) - 3})"
            raise AgentError(
                "工作区切换失败：仍有未处理的 SubAgent worktree。"
                "请先 apply_worktree 或 discard_worktree："
                f"{branches}"
            )

        # 1. 子任务全部退出后再准备新工作区，避免准备阶段临时替换 Agent
        #    可变字段时被旧子线程观察到。失败时恢复旧 Coordinator 接单。
        try:
            prepared = self._prepare_workspace_switch(new_root)
        except BaseException:
            if coordinator is not None:
                coordinator.resume_accepting_when_idle()
            raise

        # 2. 收尾旧工作区资源（此时新子系统已就绪）。
        # 会话已全局化且切换保持同一会话，不能在此丢弃/清空当前会话；
        # 只关闭临时目录、Monitor 与 MCP 等旧工作区资源。
        self._teardown_workspace_resources(discard_empty_session=False)

        # 3. 原子替换到新工作区状态。
        self.workspace_root = new_root
        self.__dict__.pop("_workspace_tools", None)
        self.__dict__.pop("_windows_desktop_tools", None)
        self.__dict__.pop("_agent_session_facade", None)
        self.__dict__.pop("_context_compaction_service_instance", None)

        self._workspace_tools = WorkspaceTools(
            new_root,
            command_timeout_seconds=self.config.command_timeout_seconds,
            extra_protection_message=self._workspace_extra_protection_message,
        )
        self._temp_workspace = prepared["temp_workspace"]
        self._session_store = prepared["session_store"]
        self._session_state = prepared["session_state"]
        self._project_store = prepared["project_store"]
        self._project_memory_store = prepared["project_memory_store"]
        self._session_memory_store = prepared["session_memory_store"]
        self._user_memory_store = prepared["user_memory_store"]
        self._memory_store = self._project_memory_store
        self._mcp_manager = prepared["mcp_manager"]
        self._tools = prepared["tools"]
        if "skill_manager" in prepared:
            self._skill_manager = prepared["skill_manager"]

        # 4. 保留对话上下文：会话已全局化且切换保持同一会话，
        #    上下文不因切换而丢失（见 session_design.md）。

        # 5. 先建立不含插件定义的新工作区 Coordinator。即使后续插件 Worker
        #    重建失败，也不会遗留已暂停的旧 Coordinator 或跨工作区定义。
        if self.config.subagents.enabled:
            self._refresh_subagent_definitions(include_plugins=False)

        # 6. 最后重建插件子系统：此前 Agent 状态已与新工作区一致。
        callback = getattr(self, "_on_workspace_switched", None)
        if callable(callback):
            try:
                new_manager = callback(new_root)
                if new_manager is not None:
                    self._plugin_manager = new_manager
            except Exception as exc:
                # 工作区主体已提交，旧 PluginManager 不能继续服务新路径。入口/API
                # 回调会关闭失败 Runtime 的 Manager；Agent 本地同步降级为无插件。
                self._plugin_manager = None
                raise AgentError(f"工作区插件子系统重建失败：{exc}") from exc

        if self.config.subagents.enabled:
            self._refresh_subagent_definitions()

        self._dispatch_plugin_hook(
            "workspace.switch.after",
            {"workspace": str(new_root)},
        )
        # 转录记录跨工作区切换事件（会话未启用时静默跳过）。
        if getattr(self, "_session_state", None) is not None:
            self._append_session_event(
                "workspace_switched",
                {"from": str(old_root), "to": str(new_root)},
            )
        return new_root

    def _prepare_workspace_switch(self, new_root: Path) -> dict[str, Any]:
        """为工作区切换准备新子系统；失败时清理候选资源且不修改当前 Agent。"""

        prepared: dict[str, Any] = {
            "temp_workspace": None,
            "session_store": None,
            "session_state": None,
            "project_store": None,
            "project_memory_store": None,
            "session_memory_store": None,
            "user_memory_store": None,
            "mcp_manager": None,
            "tools": {},
        }
        previous_root = self.workspace_root
        previous_session_store = getattr(self, "_session_store", None)
        previous_session_state = getattr(self, "_session_state", None)
        previous_project_store = getattr(self, "_project_store", None)
        previous_project_memory_store = getattr(self, "_project_memory_store", None)
        previous_session_memory_store = getattr(self, "_session_memory_store", None)
        previous_user_memory_store = getattr(self, "_user_memory_store", None)
        previous_memory_store = getattr(self, "_memory_store", None)
        previous_mcp_manager = getattr(self, "_mcp_manager", None)
        previous_tools = getattr(self, "_tools", None)
        previous_skill_manager = getattr(self, "_skill_manager", None)
        previous_windows_desktop_tools = getattr(self, "_windows_desktop_tools", None)

        try:
            # 临时把 workspace_root 指到新路径，复用现有工厂方法；失败后完整回写。
            self.workspace_root = new_root
            self.__dict__.pop("_workspace_tools", None)
            self.__dict__.pop("_windows_desktop_tools", None)
            self.__dict__.pop("_agent_session_facade", None)

            temp_workspace = AgentTempWorkspace(new_root, self.config.temp_workspace)
            temp_workspace.ensure()
            temp_workspace.clean_if_due()
            temp_workspace.start_scheduler()
            prepared["temp_workspace"] = temp_workspace

            # 会话/项目工厂依赖 facade，而 facade 依赖当前 session_store 槽位。
            self._session_store = None
            self._session_state = None
            self._project_store = None
            if self.config.session_enabled:
                # 会话已全局化（~/.omnicrawl/.agent_sessions，不绑定工作区）：
                # 切换工作区保持同一会话，直接沿用当前 SessionStore/SessionState/
                # ProjectStore，而不是新建会话；项目/用户级记忆仍由 memory 分支
                # 按新工作区重建，会话级记忆按复用的 session id 正确绑定。
                session_store = previous_session_store
                session_state = previous_session_state
                project_store = previous_project_store
                if session_store is None:
                    # 防御兜底：session_enabled 时初始化已创建，此处仅防异常路径。
                    session_store = self._create_session_store()
                    self._session_store = session_store
                if session_state is None:
                    session_state = self._start_session()
                prepared["session_store"] = session_store
                prepared["session_state"] = session_state
                prepared["project_store"] = project_store
                self._session_store = session_store
                self._session_state = session_state
                self._project_store = project_store

            if self.config.memory_enabled:
                (
                    project_memory_store,
                    session_memory_store,
                    user_memory_store,
                ) = self._create_memory_stores()
                prepared["project_memory_store"] = project_memory_store
                prepared["session_memory_store"] = session_memory_store
                prepared["user_memory_store"] = user_memory_store
                self._project_memory_store = project_memory_store
                self._session_memory_store = session_memory_store
                self._user_memory_store = user_memory_store
                self._memory_store = project_memory_store

            mcp_manager = self._create_mcp_manager()
            prepared["mcp_manager"] = mcp_manager
            self._mcp_manager = mcp_manager
            tools = self._build_tools()
            prepared["tools"] = tools
            self._tools = tools

            if self.config.skills_enabled:
                skill_manager = SkillManager()
                skill_manager.discover(
                    cwd=new_root,
                    extra_paths=self.config.skill_paths,
                )
                prepared["skill_manager"] = skill_manager

            # 准备完成：把运行态先还原到旧工作区，真正切换由调用方统一赋值。
            self.workspace_root = previous_root
            self._session_store = previous_session_store
            self._session_state = previous_session_state
            self._project_store = previous_project_store
            self._project_memory_store = previous_project_memory_store
            self._session_memory_store = previous_session_memory_store
            self._user_memory_store = previous_user_memory_store
            self._memory_store = previous_memory_store
            self._mcp_manager = previous_mcp_manager
            if previous_tools is not None:
                self._tools = previous_tools
            if previous_skill_manager is not None:
                self._skill_manager = previous_skill_manager
            self.__dict__.pop("_workspace_tools", None)
            if previous_windows_desktop_tools is not None:
                self._windows_desktop_tools = previous_windows_desktop_tools
            else:
                self.__dict__.pop("_windows_desktop_tools", None)
            self.__dict__.pop("_agent_session_facade", None)
            return prepared
        except Exception as exc:
            # 清理已创建的候选资源，并完整恢复旧 Agent 状态。
            self._discard_prepared_workspace(prepared)
            self.workspace_root = previous_root
            self._session_store = previous_session_store
            self._session_state = previous_session_state
            self._project_store = previous_project_store
            self._project_memory_store = previous_project_memory_store
            self._session_memory_store = previous_session_memory_store
            self._user_memory_store = previous_user_memory_store
            self._memory_store = previous_memory_store
            self._mcp_manager = previous_mcp_manager
            if previous_tools is not None:
                self._tools = previous_tools
            if previous_skill_manager is not None:
                self._skill_manager = previous_skill_manager
            self.__dict__.pop("_workspace_tools", None)
            if previous_windows_desktop_tools is not None:
                self._windows_desktop_tools = previous_windows_desktop_tools
            else:
                self.__dict__.pop("_windows_desktop_tools", None)
            self.__dict__.pop("_agent_session_facade", None)
            self._dispatch_plugin_hook(
                "workspace.switch.error",
                {"workspace": str(new_root), "error": str(exc)},
            )
            if isinstance(exc, AgentError):
                raise
            raise AgentError(f"工作区切换准备失败：{exc}") from exc

    def _discard_prepared_workspace(self, prepared: dict[str, Any]) -> None:
        """关闭工作区切换过程中创建但未提交的候选资源。"""

        for key in ("mcp_manager", "temp_workspace"):
            resource = prepared.get(key)
            if resource is None:
                continue
            closer = getattr(resource, "close", None)
            if callable(closer):
                try:
                    closer()
                except Exception:
                    pass

    def _teardown_workspace_resources(self, *, discard_empty_session: bool) -> None:
        """关闭当前工作区绑定的临时目录、会话、MCP 与 Monitor 资源。"""

        if (
            discard_empty_session
            and self._session_store is not None
            and self._session_state is not None
        ):
            try:
                self._session_facade().discard_current_empty_session()
            except AgentError:
                pass
            self._session_state = None
            self._session_store = None
            self._project_store = None

        old_temp = getattr(self, "_temp_workspace", None)
        if old_temp is not None:
            try:
                old_temp.close()
            except Exception:
                pass
            self.__dict__.pop("_temp_workspace", None)

        old_monitor_manager = getattr(self, "_monitor_manager", None)
        if old_monitor_manager is not None:
            try:
                old_monitor_manager.close()
            except Exception:
                pass
            self.__dict__.pop("_monitor_manager", None)

        old_mcp = getattr(self, "_mcp_manager", None)
        if old_mcp is not None:
            try:
                old_mcp.close()
            except Exception:
                pass
            self.__dict__.pop("_mcp_manager", None)

    def _clear_workspace_root_override(self) -> None:
        """清理当前线程的 WorkspaceTools 根目录覆盖。"""

        local = getattr(self, "_workspace_root_local", None)
        if local is not None and hasattr(local, "root"):
            try:
                delattr(local, "root")
            except Exception:  # noqa: BLE001 - 清理失败不应影响主流程
                local.root = None
