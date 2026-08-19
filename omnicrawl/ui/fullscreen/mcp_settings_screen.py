"""全屏 TUI 的 MCP 设置主界面（P3 自 mcp_settings.py 拆出）。"""

from __future__ import annotations

from dataclasses import replace
from typing import Any, Optional

from textual.app import ComposeResult
from textual.binding import Binding
from textual.containers import Container, VerticalScroll
from textual.screen import ModalScreen
from textual.widgets import Static

from ...agent import AgentError
from ...config.settings import SettingsConfigError
from ...mcp.config import MCPConfig, MCPConfigError
from .mcp_settings import MCPSettingsAction, _apply_and_save, _current_config
from .theme import terminal_css


class MCPSettingsScreen(ModalScreen[Optional[MCPSettingsAction]]):
    """第二级：MCP 全局策略和资源限制。"""

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
    #mcp-settings-list { height: 1fr; }
    .mcp-settings-row { height: 2; padding: 0 1; color: $terminal-text-secondary; }
    .mcp-settings-row.selected { color: $terminal-amber; text-style: bold; }
    #mcp-settings-status { height: 2; color: $terminal-white; margin-top: 1; }
    #mcp-settings-help { height: 1; color: $terminal-white; margin-top: 1; }
    """)

    _ROWS = ("enabled", "network", "write", "command", "audit", "timeout", "servers")

    def __init__(self, agent: Any) -> None:
        super().__init__()
        self._agent = agent
        self._config = _current_config(agent)
        self._selected = 0
        self._busy = False
        self._status = "Enter/空格修改；Server 管理进入下一级。"

    def compose(self) -> ComposeResult:
        with Container(id="mcp-settings-dialog"):
            yield Static("MCP 全局设置", id="mcp-settings-title")
            with VerticalScroll(id="mcp-settings-list"):
                for key in self._ROWS:
                    yield Static(" ", id=f"mcp-settings-row-{key}", classes="mcp-settings-row")
            yield Static(self._status, id="mcp-settings-status")
            yield Static("↑↓ 选择  ←→ 修改  Enter/空格确认  Esc 返回", id="mcp-settings-help")

    def on_mount(self) -> None:
        self.call_after_refresh(self._render_rows)

    def action_cancel(self) -> None:
        if not self._busy:
            self.dismiss(None)

    def action_move_up(self) -> None:
        if not self._busy:
            self._selected = (self._selected - 1) % len(self._ROWS)
            self._render_rows()

    def action_move_down(self) -> None:
        if not self._busy:
            self._selected = (self._selected + 1) % len(self._ROWS)
            self._render_rows()

    def action_previous_value(self) -> None:
        self._change(-1)

    def action_next_value(self) -> None:
        self._change(1)

    def action_confirm(self) -> None:
        self._change(1)

    def _change(self, direction: int) -> None:
        if self._busy:
            return
        key = self._ROWS[self._selected]
        if key == "servers":
            self.dismiss(MCPSettingsAction("servers"))
            return
        if key == "timeout":
            options = (10, 30, 60, 120, 300)
            current = self._config.default_timeout_seconds
            index = min(range(len(options)), key=lambda i: abs(options[i] - current))
            value = options[(index + direction) % len(options)]
            self._config = replace(self._config, default_timeout_seconds=value)
        elif key == "network":
            self._config = replace(self._config, policy=replace(self._config.policy, allow_external_network_tools=not self._config.policy.allow_external_network_tools))
        elif key == "write":
            self._config = replace(self._config, policy=replace(self._config.policy, require_confirmation_for_write=not self._config.policy.require_confirmation_for_write))
        elif key == "command":
            self._config = replace(self._config, policy=replace(self._config.policy, require_confirmation_for_command=not self._config.policy.require_confirmation_for_command))
        elif key == "audit":
            self._config = replace(self._config, policy=replace(self._config.policy, audit_log_enabled=not self._config.policy.audit_log_enabled))
        elif key == "enabled":
            self._config = replace(self._config, enabled=not self._config.enabled)
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
            self._render_rows()

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

    def _render_rows(self) -> None:
        if not self.is_mounted:
            return
        labels = {"enabled": "MCP 总开关", "network": "外部网络工具", "write": "写入操作确认", "command": "命令操作确认", "audit": "审计日志", "timeout": "默认超时", "output": "Tool 输出上限", "servers": "MCP Server"}
        values = self._values()
        for index, key in enumerate(self._ROWS):
            row = self.query_one(f"#mcp-settings-row-{key}", Static)
            row.update(("› " if index == self._selected else "  ") + f"{labels[key]}：{values[key]}")
            row.set_class(index == self._selected, "selected")
            if index == self._selected:
                row.scroll_visible(animate=False)
        self.query_one("#mcp-settings-status", Static).update(self._status)


