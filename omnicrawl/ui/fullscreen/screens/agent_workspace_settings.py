"""主 Agent 隔离工作区设置（可内嵌右侧的 Pane + 整屏薄壳）。"""

from __future__ import annotations

from dataclasses import replace
from pathlib import Path
from typing import Any, Optional

from textual.app import ComposeResult
from textual.binding import Binding
from textual.containers import Container, Horizontal, VerticalScroll
from textual.screen import ModalScreen
from textual.widgets import Button, Input, Select, Static

from ....config.features.agent_workspace import (
    AgentWorkspaceConfig,
    AgentWorkspaceConfigError,
    load_agent_workspace_config,
    save_agent_workspace_config,
)
from ..terminal.theme import terminal_css, terminal_select_css
from .panes import SettingsPane

_PANE_CSS = """
#agent-workspace-form { height: 1fr; }
.agent-workspace-label { height: 1; color: $terminal-text-muted; }
.agent-workspace-control { height: 3; margin-bottom: 1; }
#agent-workspace-status { height: 2; color: $terminal-white; }
#agent-workspace-actions { height: 3; align-horizontal: right; }
"""


class AgentWorkspaceSettingsPane(SettingsPane):
    """隔离工作区二级面板：编辑配置，保存后返回左侧。"""

    BINDINGS = [
        Binding("escape", "cancel", "返回", priority=True),
        Binding("ctrl+s", "save", "保存", priority=True),
    ]

    DEFAULT_CSS = terminal_css(_PANE_CSS + terminal_select_css())

    def __init__(
        self,
        config_path: str | Path | None = None,
        *,
        agent: Any | None = None,
        configuration: AgentWorkspaceConfig | None = None,
        apply_configuration: Any | None = None,
    ) -> None:
        super().__init__(agent=agent)
        self._config_path = Path(config_path) if config_path is not None else None
        self._configuration = configuration or (
            load_agent_workspace_config(self._config_path)
            if self._config_path is not None
            else AgentWorkspaceConfig()
        )
        self._apply_configuration = apply_configuration

    def compose_pane(self) -> ComposeResult:
        c = self._configuration
        with VerticalScroll(id="agent-workspace-form"):
            yield Static("隔离功能总开关", classes="agent-workspace-label")
            yield Select(
                [("停用（直接使用主工作区）", False), ("启用（每个进程独立隔离区）", True)],
                value=c.enabled,
                allow_blank=False,
                id="agent-workspace-enabled",
                classes="agent-workspace-control choice-select",
            )
            yield Static("隔离模式", classes="agent-workspace-label")
            yield Select(
                [("worktree（Git 工作树，推荐）", "worktree"), ("local（普通目录复制）", "local")],
                value=c.mode,
                allow_blank=False,
                id="agent-workspace-mode",
                classes="agent-workspace-control choice-select",
            )
            for label, widget_id, value in (
                ("基线引用（默认 HEAD）", "agent-workspace-base-ref", c.base_ref),
                ("复制的目录（逗号分隔，如 .env,node_modules）", "agent-workspace-copy-dirs", ", ".join(c.copy_dirs)),
                ("环境脚本（分号分隔，如 npm install；bash 执行）", "agent-workspace-env-scripts", "; ".join(c.env_scripts)),
            ):
                yield Static(label, classes="agent-workspace-label")
                yield Input(str(value), id=widget_id, classes="agent-workspace-control")
            yield Static("Detached HEAD（不创建临时分支）", classes="agent-workspace-label")
            yield Select(
                [("停用", False), ("启用", True)],
                value=c.detached,
                allow_blank=False,
                id="agent-workspace-detached",
                classes="agent-workspace-control choice-select",
            )
            yield Static("带入主工作区未提交变更（git diff 补丁）", classes="agent-workspace-label")
            yield Select(
                [("停用", False), ("启用", True)],
                value=c.sync_uncommitted,
                allow_blank=False,
                id="agent-workspace-sync-uncommitted",
                classes="agent-workspace-control choice-select",
            )
            yield Static("退出时自动应用变更回主工作区", classes="agent-workspace-label")
            yield Select(
                [("停用", False), ("启用", True)],
                value=c.apply_on_exit,
                allow_blank=False,
                id="agent-workspace-apply-on-exit",
                classes="agent-workspace-control choice-select",
            )
            yield Static("退出时清理策略", classes="agent-workspace-label")
            yield Select(
                [("auto（自动清理可回收项）", "auto"), ("keep（保留）", "keep"), ("never（永不清理）", "never")],
                value=c.cleanup_on_exit,
                allow_blank=False,
                id="agent-workspace-cleanup",
                classes="agent-workspace-control choice-select",
            )
            yield Static(
                "保存后从下一次启动生效；当前正在运行的进程不受影响。",
                id="agent-workspace-status",
            )
        with Horizontal(id="agent-workspace-actions"):
            yield Static("", classes="pane-save-hint")
            yield Button("取消", id="agent-workspace-cancel")
            yield Button("保存", variant="primary", id="agent-workspace-save")

    def on_button_pressed(self, event: Button.Pressed) -> None:
        if event.button.id == "agent-workspace-save":
            self.action_save()
        elif event.button.id == "agent-workspace-cancel":
            self.action_cancel()

    def activate(self) -> None:
        self.query_one("#agent-workspace-enabled", Select).focus()

    def action_cancel(self) -> None:
        self.request_back()

    def action_save(self) -> None:
        try:
            old = self._configuration
            copy_dirs = tuple(
                item.strip()
                for item in self.query_one("#agent-workspace-copy-dirs", Input).value.split(",")
                if item.strip()
            )
            env_scripts = tuple(
                item.strip()
                for item in self.query_one("#agent-workspace-env-scripts", Input).value.split(";")
                if item.strip()
            )
            configuration = AgentWorkspaceConfig(
                enabled=self._read_bool("agent-workspace-enabled"),
                mode=self._read_select("agent-workspace-mode"),
                base_ref=self.query_one("#agent-workspace-base-ref", Input).value.strip() or "HEAD",
                detached=self._read_bool("agent-workspace-detached"),
                sync_uncommitted=self._read_bool("agent-workspace-sync-uncommitted"),
                copy_dirs=copy_dirs,
                env_scripts=env_scripts,
                apply_on_exit=self._read_bool("agent-workspace-apply-on-exit"),
                cleanup_on_exit=self._read_select("agent-workspace-cleanup"),
            )
            if self._config_path is not None:
                save_agent_workspace_config(configuration, self._config_path)
            if self._apply_configuration is not None:
                try:
                    self._apply_configuration(configuration)
                except Exception:
                    if self._config_path is not None:
                        save_agent_workspace_config(old, self._config_path)
                    raise
        except (AgentWorkspaceConfigError, OSError, ValueError) as exc:
            self.query_one("#agent-workspace-status", Static).update(f"保存失败：{exc}")
            return
        self.flash_save_hint()
        self.commit(configuration)

    def _read_select(self, widget_id: str) -> str:
        return str(self.query_one(f"#{widget_id}", Select).value)

    def _read_bool(self, widget_id: str) -> bool:
        return bool(self.query_one(f"#{widget_id}", Select).value)


class AgentWorkspaceSettingsScreen(ModalScreen[Optional[AgentWorkspaceConfig]]):
    """整屏薄壳：内嵌 AgentWorkspaceSettingsPane。"""

    BINDINGS = [
        Binding("escape", "cancel", "取消"),
        Binding("ctrl+s", "save", "保存", priority=True),
    ]

    CSS = terminal_css("""
    AgentWorkspaceSettingsScreen { align: center middle; background: $terminal-overlay; }
    #agent-workspace-dialog { width: 96; max-width: 96%; height: 46; max-height: 95%; padding: 1 2; border: round $terminal-border-strong; background: $terminal-surface; }
    #agent-workspace-title { height: 1; margin-bottom: 1; color: $terminal-white; text-style: bold; }
    """ + _PANE_CSS + terminal_select_css())

    def __init__(
        self,
        config_path: str | Path,
        *,
        configuration: AgentWorkspaceConfig | None = None,
        apply_configuration: Any | None = None,
    ) -> None:
        super().__init__()
        self._config_path = Path(config_path)
        self._apply_configuration = apply_configuration
        self._configuration = configuration
        self._pane: Optional[AgentWorkspaceSettingsPane] = None

    def compose(self) -> ComposeResult:
        with Container(id="agent-workspace-dialog"):
            yield Static("主 Agent 隔离工作区", id="agent-workspace-title")
            self._pane = AgentWorkspaceSettingsPane(
                self._config_path,
                configuration=self._configuration,
                apply_configuration=self._apply_configuration,
            )
            self._pane.bind_pane_events(
                on_back=lambda: self.dismiss(None),
                on_commit=lambda result: self.dismiss(result),
            )
            yield self._pane

    def on_mount(self) -> None:
        if self._pane is not None:
            self._pane.activate()

    def action_cancel(self) -> None:
        self.dismiss(None)

    def action_save(self) -> None:
        if self._pane is not None:
            self._pane.action_save()


__all__ = ["AgentWorkspaceSettingsPane", "AgentWorkspaceSettingsScreen"]
