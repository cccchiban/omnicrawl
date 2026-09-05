"""Agent 控制面 API：状态查询、后台任务/监控可见性、生命周期关闭。

保持 ``LocalToolAgent`` 对外门面不变，本 Mixin 只承载状态与控制类入口；
会话/项目存取见 ``session.store``，运行时配置见 ``session.settings``。"""
from __future__ import annotations

import logging
from typing import Any, Callable
from ...session.session_facade import AgentSessionFacade
from ...subagents.recovery import rebuild_task_snapshots_from_session_events
from ....skill import SkillManager
from ....workspace_tools import (
    WorkspaceToolError,
)
from ....workspace.monitor import MonitorPollResult, MonitorTaskSnapshot

from ..shared import (
    AgentError,
    SUBAGENT_LIFECYCLE_WAIT_SECONDS,
)

LOGGER = logging.getLogger(__name__)


class SessionControlMixin:
    """Agent 控制面 API：状态查询、后台任务/监控可见性、生命周期关闭。"""

    def _session_facade(self) -> AgentSessionFacade:
        facade = getattr(self, "_agent_session_facade", None)
        if facade is None:
            facade = AgentSessionFacade(self, AgentError)
            self._agent_session_facade = facade
        return facade

    @property
    def skill_manager(self) -> SkillManager | None:
        """公开 SkillManager 供 main.py 查询 /skills 列表。"""
        return self._skill_manager

    def preload_mcp_tools(self) -> None:
        """发现并注册 MCP 能力，供交互界面在后台启动阶段主动预热。"""

        self._ensure_mcp_tools_ready()

    def format_mcp_status(self) -> str:
        """返回 MCP 子系统状态，供 `/mcp` 斜杠命令展示。"""

        self.preload_mcp_tools()
        return self._mcp_manager.format_status()

    def format_plugins_status(self) -> str:
        """返回插件子系统只读状态，供 `/plugins` 斜杠命令展示。

        安装/更新/卸载不在活跃 Agent 内执行；此处只读 runtime 与配置。
        """

        manager = getattr(self, "_plugin_manager", None)
        if manager is None:
            return (
                "插件子系统：未注入 PluginManager（无插件模式）。\n"
                "管理命令：ocl plugin doctor / list / install ..."
            )
        enabled = bool(getattr(manager, "enabled", False))
        lines = [
            f"插件系统：{'已启用' if enabled else '已关闭（plugins.enabled=false）'}",
        ]
        list_status = getattr(manager, "list_status", None)
        rows = list_status() if callable(list_status) else []
        if not rows:
            lines.append("当前工作区没有已加载的插件 Worker。")
            lines.append("管理命令：ocl plugin list")
            return "\n".join(lines)
        lines.append(f"已加载 Worker：{len(rows)}")
        for row in rows:
            name = row.get("name", "?")
            version = row.get("version", "?")
            scope = row.get("scope", "?")
            active = "active" if row.get("active") else "inactive"
            circuit = " circuit-open" if row.get("circuitOpen") else ""
            dev = " [dev]" if row.get("devMode") else ""
            handlers = row.get("handlers") or []
            lines.append(
                f"  - {name}@{version} ({scope}) {active}{circuit}{dev}"
            )
            if handlers:
                lines.append(f"    handlers: {', '.join(map(str, handlers))}")
            last_error = str(row.get("lastError") or "").strip()
            if last_error:
                lines.append(f"    lastError: {last_error[:160]}")
        lines.append("管理命令：ocl plugin list|info|enable|disable|install|update|rollback|uninstall ...")
        return "\n".join(lines)

    def _ensure_mcp_tools_ready(
        self,
        status: Callable[[str], None] | None = None,
    ) -> None:
        """按需发现 MCP 能力，并在发现后重建工具表。

        启动期只保留内置工具，等首次真正需要模型上下文或用户查看 `/mcp`
        时再拉起 stdio MCP Server。这样不会减少 MCP 功能，只是把昂贵的
        进程启动和能力枚举从 GUI 首屏路径移到首次使用路径。
        """

        manager = getattr(self, "_mcp_manager", None)
        if manager is None or not manager.enabled or manager.discovered:
            return

        if status is not None:
            status("正在加载 MCP 能力")
        manager.discover()
        self._tools = self._build_tools()

    def list_monitor_tasks(self) -> list[MonitorTaskSnapshot]:
        """列出当前 Agent 受管的后台任务，供 TUI 与本地 API 只读展示。"""

        return self._monitor_toolbox().list_snapshots()

    def get_monitor_task(self, monitor_id: str) -> MonitorTaskSnapshot:
        """读取一个后台任务状态，不改变其执行或日志游标。"""

        try:
            return self._monitor_toolbox().get_snapshot(monitor_id)
        except WorkspaceToolError as exc:
            raise AgentError(str(exc)) from exc

    def poll_monitor_events(
        self,
        monitor_id: str,
        *,
        cursor: int = 0,
        max_events: int = 100,
    ) -> MonitorPollResult:
        """按游标读取后台日志，供 UI/API 观察而不触发模型新回合。"""

        try:
            return self._monitor_toolbox().poll_events(
                monitor_id,
                cursor=cursor,
                max_events=max_events,
            )
        except WorkspaceToolError as exc:
            raise AgentError(str(exc)) from exc

    def wait_for_monitor_events(self, monitor_id: str, cursor: int, timeout: float) -> None:
        """等待后台日志或任务终态，供 API SSE 长连接降低轮询开销。"""

        try:
            self._monitor_toolbox().wait_for_events(monitor_id, cursor, timeout)
        except WorkspaceToolError as exc:
            raise AgentError(str(exc)) from exc

    def list_subagent_tasks(self) -> list[dict[str, Any]]:
        """列出当前会话可见的后台 SubAgent 任务安全快照。

        所有 owner/session 过滤都由 Coordinator 固定，调用方不能借由 API 或
        TUI 传入其他会话标识来枚举任务。
        """

        coordinator = getattr(self, "_subagent_coordinator", None)
        if coordinator is None:
            raise AgentError("SubAgent 功能未启用。")
        return coordinator.list_tasks()

    def import_recovered_subagent_tasks(self) -> int:
        """从当前会话 additive 事件导入跨进程 SubAgent 终态快照。

        只恢复控制面 list/get 可见性：中断中的非终态任务会折叠为
        ``failed`` + ``SUBAGENT_INTERRUPTED``，绝不自动重跑，也不注入通知。
        """

        coordinator = getattr(self, "_subagent_coordinator", None)
        store = getattr(self, "_session_store", None)
        state = getattr(self, "_session_state", None)
        if coordinator is None or store is None or state is None:
            return 0
        try:
            events = store.read_session_events(state.session_id)
        except Exception:
            # 恢复是增量能力；会话事件读取失败不得阻断 Agent 启动。
            LOGGER.warning(
                "Failed to read session events for SubAgent recovery",
                exc_info=True,
            )
            return 0
        snapshots = rebuild_task_snapshots_from_session_events(
            events,
            owner_id=f"agent-{id(self)}",
            session_id=state.session_id,
        )
        if not snapshots:
            return 0
        try:
            return int(coordinator.import_recovered_snapshots(snapshots))
        except Exception:
            LOGGER.warning(
                "Failed to import recovered SubAgent snapshots",
                exc_info=True,
            )
            return 0

    def get_subagent_task(self, task_id: str) -> dict[str, Any] | None:
        """读取当前会话的单个后台 SubAgent 任务；跨会话任务不可见。"""

        coordinator = getattr(self, "_subagent_coordinator", None)
        if coordinator is None:
            raise AgentError("SubAgent 功能未启用。")
        return coordinator.get_task(task_id)

    def cancel_active_turn(self, reason: str = "父 Agent 回合已取消。") -> None:
        """主动取消当前回合关联的 SubAgent、审批和后台任务，不等待收尾。"""

        coordinator = getattr(self, "_subagent_coordinator", None)
        if coordinator is not None:
            coordinator.cancel_active(reason)

    def cancel_subagent_task(self, task_id: str) -> dict[str, Any]:
        """请求取消当前会话的后台 SubAgent 任务，不等待其最终退出。"""

        coordinator = getattr(self, "_subagent_coordinator", None)
        if coordinator is None:
            raise AgentError("SubAgent 功能未启用。")
        return coordinator.cancel_task(task_id=task_id)

    def add_close_callback(self, callback: Callable[[], None]) -> None:
        """注册资源关闭后的单次回调，供进程级 PluginRuntime 等外部所有者使用。"""

        if getattr(self, "_closed", False):
            callback()
            return
        self._close_callbacks.append(callback)

    def attach_isolation_session(
        self,
        session: Any,
        *,
        on_finalized: Callable[[str], None] | None = None,
    ) -> None:
        """把主 Agent 隔离工作区会话挂到 Agent 生命周期上，``close()`` 时自动收尾。

        ``close()`` 会先按 ``config.agent_workspace`` 的 ``apply_on_exit`` /
        ``cleanup_on_exit`` 把隔离区变更应用回主工作区并清理，保证 TUI / API /
        连接器各入口共用同一收尾路径（此前只有 TUI 显式收尾）。``on_finalized``
        接收收尾摘要文本，例如 TUI 用于打印到 stderr；回调异常被吞掉。
        """

        self._isolation_session = session
        self._isolation_on_finalized = on_finalized

    def _finalize_attached_isolation(self) -> None:
        """收尾挂载的隔离工作区与 SubAgent worktree 会话；只执行一次。"""

        session = getattr(self, "_isolation_session", None)
        self._isolation_session = None
        on_finalized = getattr(self, "_isolation_on_finalized", None)
        self._isolation_on_finalized = None
        summaries: list[str] = []
        if session is not None:
            try:
                from ....workspace.agent_isolation import finalize_isolation_session

                workspace_config = getattr(self.config, "agent_workspace", None)
                summaries.append(
                    finalize_isolation_session(
                        session,
                        apply_on_exit=bool(
                            getattr(workspace_config, "apply_on_exit", True)
                        ),
                        cleanup_on_exit=str(
                            getattr(workspace_config, "cleanup_on_exit", "auto") or "auto"
                        ),
                    )
                )
            except Exception as exc:  # noqa: BLE001 - 隔离收尾失败不能阻断进程退出
                summaries.append(f"隔离工作区收尾失败：{exc}")
        # SubAgent worktree 会话按 auto 策略收尾：四层门禁通过的（无变更）
        # 清理，有变更的一律保留（成果须父 Agent 显式审查，绝不自动 apply）。
        sub_summary = ""
        try:
            from ....workspace.agent_isolation import finalize_subagent_worktrees

            sub_summary = finalize_subagent_worktrees()
            if sub_summary:
                summaries.append(sub_summary)
        except Exception as exc:  # noqa: BLE001 - 收尾失败不能阻断进程退出
            summaries.append(f"SubAgent worktree 收尾失败：{exc}")
        summary = "；".join(item for item in summaries if item)
        if summary:
            LOGGER.info("Isolation finalize: %s", summary)
        if on_finalized is not None and (session is not None or sub_summary):
            try:
                on_finalized(summary)
            except Exception:  # noqa: BLE001
                pass

    def close(self) -> None:
        """取消子任务并在安全边界内关闭 Agent 持有的外部资源。"""

        if getattr(self, "_closed", False) or getattr(self, "_closing", False):
            return
        coordinator = getattr(self, "_subagent_coordinator", None)
        if coordinator is not None:
            drained = coordinator.cancel_and_wait(
                reason="Agent 正在关闭，当前子任务已取消。",
                timeout_seconds=SUBAGENT_LIFECYCLE_WAIT_SECONDS,
                permanent=True,
            )
            if not drained:
                # Python worker 线程不能被安全强杀。先保持资源可用，再由最后一个
                # 子任务的 Future 收尾回调自动重试关闭，调用方无需轮询或手工重试。
                self._closing = True
                coordinator.call_when_idle(self._finish_deferred_close)
                return
        self._closed = True
        self._closing = False
        # 主 Agent 隔离工作区收尾：按配置把变更应用回主工作区并按策略清理。
        # 放在子任务全部排空之后，保证 apply 时不再有隔离区写入者。
        self._finalize_attached_isolation()
        close_errors: list[Exception] = []
        try:
            self._dispatch_plugin_hook("session.close.before", {})
            self._append_session_closed_event()
            self._dispatch_plugin_hook("session.close.after", {})
        except Exception as exc:
            close_errors.append(exc)

        manager = getattr(self, "_mcp_manager", None)
        if manager is not None:
            try:
                manager.close()
            except Exception as exc:
                close_errors.append(exc)
        monitor_manager = getattr(self, "_monitor_manager", None)
        if monitor_manager is not None:
            try:
                monitor_manager.close()
            except Exception as exc:
                close_errors.append(exc)
        temp_workspace = getattr(self, "_temp_workspace", None)
        if temp_workspace is not None:
            try:
                temp_workspace.close()
            except Exception as exc:
                close_errors.append(exc)
        client = getattr(self, "_client", None)
        if client is not None:
            try:
                close_client = getattr(client, "close", None)
                if callable(close_client):
                    close_client()
            except Exception as exc:
                close_errors.append(exc)
            self._client = None
        runtime_manager = getattr(self, "_runtime_manager", None)
        if runtime_manager is not None:
            try:
                runtime_manager.close()
            except Exception as exc:
                close_errors.append(exc)
            self._runtime_manager = None
        callbacks = tuple(getattr(self, "_close_callbacks", ()))
        self._close_callbacks = []
        for callback in callbacks:
            try:
                callback()
            except Exception as exc:
                close_errors.append(exc)
        if close_errors:
            raise close_errors[0]

    def _finish_deferred_close(self) -> None:
        """最后一个子任务退出后自动完成此前因超时推迟的资源关闭。"""

        self._closing = False
        try:
            self.close()
        except Exception as exc:  # noqa: BLE001 - 后台清理失败只能记录，不能回抛到 worker
            LOGGER.warning(
                "Deferred Agent close failed: %s",
                type(exc).__name__,
            )

    def _append_session_closed_event(self) -> None:
        """正常退出时收尾当前会话，并丢弃没有真实内容的启动占位。"""

        state = getattr(self, "_session_state", None)
        if state is None:
            return
        if state.last_event_type in {"session_closed", "session_interrupted"}:
            if state.last_event_type == "session_closed":
                self._session_facade().discard_current_empty_session()
            return
        self._append_session_event("session_closed", {})
        self._session_facade().discard_current_empty_session()
