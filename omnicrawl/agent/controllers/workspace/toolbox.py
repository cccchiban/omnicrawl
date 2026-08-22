"""工作区/后台监控/Windows 桌面工具对象的惰性访问器与路径助手。"""
from __future__ import annotations

from pathlib import Path, PurePosixPath
from ...toolkit.windows_desktop import WindowsDesktopTools
from ....workspace_tools import (
    DEFAULT_COMMAND_TIMEOUT_SECONDS,
    MAX_COMMAND_TIMEOUT_SECONDS,
    WorkspaceToolError,
    WorkspaceTools,
)
from ....workspace.monitor import BackgroundMonitorManager, MonitorPollResult, MonitorTaskSnapshot


class WorkspaceToolboxMixin:
    """工作区/后台监控/Windows 桌面工具对象的惰性访问器与路径助手。"""

    def _workspace_toolbox(self) -> WorkspaceTools:
        """返回当前线程可见的工作区工具箱。

        SubAgent worktree 任务通过 ``_workspace_root_local`` 覆盖根目录，避免
        并发子任务与父工作区互相写穿。覆盖存在时不复用缓存的父 toolbox。
        """

        override_root = getattr(getattr(self, "_workspace_root_local", None), "root", None)
        command_timeout = getattr(
            getattr(self, "config", None),
            "command_timeout_seconds",
            DEFAULT_COMMAND_TIMEOUT_SECONDS,
        )
        if override_root:
            return WorkspaceTools(
                override_root,
                command_timeout_seconds=command_timeout,
                extra_protection_message=self._workspace_extra_protection_message,
            )
        toolbox = getattr(self, "_workspace_tools", None)
        if toolbox is not None:
            return toolbox
        toolbox = WorkspaceTools(
            self.workspace_root,
            command_timeout_seconds=command_timeout,
            extra_protection_message=self._workspace_extra_protection_message,
        )
        self._workspace_tools = toolbox
        return toolbox

    def _monitor_toolbox(self) -> BackgroundMonitorManager:
        manager = getattr(self, "_monitor_manager", None)
        if manager is None:
            manager = BackgroundMonitorManager(self._workspace_toolbox())
            self._monitor_manager = manager
        return manager

    def _create_windows_desktop_tools(self) -> WindowsDesktopTools:
        # 以 AgentConfig 作为启用状态和路径的权威来源，避免测试替身或切换期间的
        # TempWorkspace 门面缺少 config/root 属性时破坏 Agent 工具表构建。
        config = getattr(self, "config", None)
        workspace_root = Path(getattr(self, "workspace_root", Path.cwd())).resolve()
        temp_config = getattr(config, "temp_workspace", None)
        screenshot_directory = None
        if bool(getattr(temp_config, "enabled", False)):
            directory = str(
                getattr(temp_config, "directory", ".omnicrawl/.agent_tmp") or ".omnicrawl/.agent_tmp"
            )
            screenshot_directory = workspace_root / directory / "images"
        return WindowsDesktopTools(
            screenshot_directory=screenshot_directory,
            workspace_root=workspace_root,
        )

    def _windows_desktop_toolbox(self) -> WindowsDesktopTools | None:
        """返回当前 Host 的 Windows 桌面工具箱；其他平台不注册该能力。"""

        toolbox = getattr(self, "_windows_desktop_tools", None)
        if toolbox is not None:
            return toolbox
        if not WindowsDesktopTools.is_supported():
            return None
        toolbox = self._create_windows_desktop_tools()
        self._windows_desktop_tools = toolbox
        return toolbox

    def _workspace_extra_protection_message(self, path: Path) -> str | None:
        """为 Agent 内置工具补充内部目录保护，MCP Server 不共享这条业务限制。"""

        if self._is_memory_path(path):
            return f"请使用 memory_* 工具访问记忆目录：{self._relative_path(path)}"
        if self._is_session_path(path):
            return f"请使用会话命令访问会话目录：{self._relative_path(path)}"
        return None

    def _relative_path(self, path: Path) -> str:
        return self._workspace_toolbox().relative_path(path)

    def _is_memory_path(self, path: Path) -> bool:
        """普通文件工具不直接访问记忆目录，统一走 memory_* 工具。"""

        if self._memory_store is None:
            return False
        try:
            resolved = path.resolve()
        except OSError:
            resolved = path
        return resolved == self._memory_store.root or self._is_relative_to(resolved, self._memory_store.root)
