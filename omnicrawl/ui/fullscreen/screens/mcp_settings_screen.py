"""全屏 TUI 的 MCP 设置主界面（可内嵌右侧的 Pane + 整屏薄壳）。"""

from __future__ import annotations

from typing import Any, Optional

from textual.app import ComposeResult
from textual.binding import Binding
from textual.containers import Container, VerticalScroll
from textual.screen import ModalScreen
from textual.widgets import Static

from ....agent import AgentError
from ....config.core.settings import SettingsConfigError
from ....mcp.config import MCPConfig, MCPConfigError
from .mcp_settings import (
    MCPSettingsAction,
    _apply_and_save,
    _current_config,
    toggle_mcp_row,
)
from ..terminal.theme import terminal_css
from .panes import SettingsPane

_PANE_CSS = """
#mcp-pane-list { height: 1fr; }
.mcp-pane-row { height: 2; padding: 0 1; color: $terminal-text-secondary; }
.mcp-pane-row.selected { color: $terminal-amber; text-style: bold; }
#mcp-pane-status { height: 2; color: $terminal-white; margin-top: 1; }
#mcp-pane-help { height: 1; color: $terminal-white; }
"""


class MCPSettingsPane(SettingsPane):
    """MCP 全局设置二级面板：策略与资源限制；Server 项进入 Server 列表。"""

    BINDINGS = [
        Binding("escape", "cancel", "返回", priority=True),
        Binding("up", "move_up", "上一项", priority=True),
        Binding("down", "move_down", "下一项", priority=True),
        ("left", "change", "修改"),
        ("right", "change", "修改"),
        ("enter", "change", "修改"),
        ("space", "change", "修改"),
    ]

    DEFAULT_CSS = terminal_css(_PANE_CSS)

    _ROWS = ("enabled", "network", "write", "command", "audit", "timeout", "servers")

    def __init__(self, agent: Any) -> None:
        super().__init__(agent=agent)
        self._config = _current_config(agent)
        self._selected = 0
        self._busy = False
        self._status = "Enter/空格修改；Server 管理进入下一级。"

    def compose_pane(self) -> ComposeResult:
        with VerticalScroll(id="mcp-pane-list"):
            for key in self._ROWS:
                yield Static(" ", id=f"mcp-pane-row-{key}", classes="mcp-pane-row")
        yield Static(self._status, id="mcp-pane-status")
        yield Static("↑↓ 选择  ←→/Enter 修改  Esc 返回", id="mcp-pane-help")

    def refresh_pane(self) -> None:
        if not self._can_refresh():
            return
        labels = {"enabled": "MCP 总开关", "network": "外部网络工具", "write": "写入操作确认", "command": "命令操作确认", "audit": "审计日志", "timeout": "默认超时", "servers": "MCP Server"}
        values = self._values()
        for index, key in enumerate(self._ROWS):
            row = self.query_one(f"#mcp-pane-row-{key}", Static)
            row.update(("› " if index == self._selected else "  ") + f"{labels[key]}：{values[key]}")
            row.set_class(index == self._selected, "selected")
            if index == self._selected:
                row.scroll_visible(animate=False)
        self.query_one("#mcp-pane-status", Static).update(self._status)

    def action_cancel(self) -> None:
        if not self._busy:
            self.request_back()

    def action_move_up(self) -> None:
        if not self._busy:
            self._selected = (self._selected - 1) % len(self._ROWS)
            self.refresh_pane()

    def action_move_down(self) -> None:
        if not self._busy:
            self._selected = (self._selected + 1) % len(self._ROWS)
            self.refresh_pane()

    def action_change(self) -> None:
        self._change(1)

    def _change(self, direction: int) -> None:
        if self._busy:
            return
        key = self._ROWS[self._selected]
        if key == "servers":
            self.request_navigate("mcp_servers")
            return
        self._config = toggle_mcp_row(self._config, key, direction)
        self._save()

    def _save(self) -> None:
        self._busy = True
        candidate = self._config
        try:
            path = _apply_and_save(self, candidate)
            self._status = f"MCP 设置已保存：{path}"
        except (AgentError, MCPConfigError, SettingsConfigError, OSError) as exc:
            self._config = _current_config(self._agent)
            self._status = f"设置未完成：{exc}"
        finally:
            self._busy = False
            self.refresh_pane()

    def _values(self) -> dict[str, str]:
        policy = self._config.policy
        return {
            "enabled": "已开启" if self._config.enabled else "已关闭",
            "network": "已允许" if policy.allow_external_network_tools else "已禁止",
            "write": "需要确认" if policy.require_confirmation_for_write else "免确认",
            "command": "需要确认" if policy.require_confirmation_for_command else "免确认",
            "audit": "已开启" if policy.audit_log_enabled else "已关闭",
            "timeout": f"{self._config.default_timeout_seconds} 秒",
            "servers": f"管理（{len(self._config.servers)} 个）",
        }


class MCPSettingsScreen(ModalScreen[Optional[MCPSettingsAction]]):
    """第二级整屏薄壳：内嵌 MCPSettingsPane；servers 分支 dismiss Action。"""

    BINDINGS = [
        ("escape", "cancel", "返回"),
        Binding("up", "move_up", "上一项", priority=True),
        Binding("down", "move_down", "下一项", priority=True),
        ("left", "previous_value", "上一个"),
        ("right", "next_value", "下一个"),
        ("enter", "confirm", "选择"),
        ("space", "confirm", "切换"),
    ]

    CSS = terminal_css("""
    MCPSettingsScreen { align: center middle; background: $terminal-overlay; }
    #mcp-settings-dialog { width: 82; max-width: 95%; height: 27; max-height: 92%; padding: 1 2; border: round $terminal-border-strong; background: $terminal-surface; }
    #mcp-settings-title { height: 1; margin-bottom: 1; color: $terminal-white; text-style: bold; }
    """ + _PANE_CSS)

    def __init__(self, agent: Any) -> None:
        super().__init__()
        self._agent = agent
        self._pane: Optional[MCPSettingsPane] = None

    def compose(self) -> ComposeResult:
        with Container(id="mcp-settings-dialog"):
            yield Static("MCP 全局设置", id="mcp-settings-title")
            self._pane = MCPSettingsPane(self._agent)
            self._pane.bind_pane_events(
                on_back=lambda: self.dismiss(None),
                on_navigate=lambda target, payload: self.dismiss(MCPSettingsAction("servers")),
            )
            yield self._pane

    def on_mount(self) -> None:
        if self._pane is not None:
            self._pane.focus()

    def action_cancel(self) -> None:
        if self._pane is not None and not self._pane._busy:
            self.dismiss(None)

    def action_move_up(self) -> None:
        if self._pane is not None:
            self._pane.action_move_up()

    def action_move_down(self) -> None:
        if self._pane is not None:
            self._pane.action_move_down()

    def action_previous_value(self) -> None:
        # 左/右键在档位式循环修改中语义相同：均切换下一档。
        self._change_value()

    def action_next_value(self) -> None:
        self._change_value()

    def _change_value(self) -> None:
        if self._pane is not None:
            self._pane.action_change()

    def action_confirm(self) -> None:
        if self._pane is not None:
            self._pane.action_change()


__all__ = ["MCPSettingsPane", "MCPSettingsScreen"]
