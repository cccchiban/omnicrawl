"""全屏 TUI 的 MCP Server 列表界面（可内嵌右侧的 Pane + 整屏薄壳）。"""

from __future__ import annotations

from dataclasses import replace
from typing import Any, Optional

from textual.app import ComposeResult
from textual.binding import Binding
from textual.containers import Container, VerticalScroll
from textual.screen import ModalScreen
from textual.widgets import Static

from ....agent import AgentError
from ....config.core.settings import SettingsConfigError
from ....mcp.config import MCPConfig, MCPConfigError, MCPServerConfig
from .mcp_settings import MCPSettingsAction, _apply_and_save, _current_config
from ..terminal.theme import terminal_css
from .panes import SettingsPane

_PANE_CSS = """
#mcp-servers-list { height: 1fr; }
.mcp-server-row { height: 2; padding: 0 1; color: $terminal-text-secondary; }
.mcp-server-row.selected { color: $terminal-amber; text-style: bold; }
#mcp-servers-status { height: 2; color: $terminal-white; margin-top: 1; }
#mcp-servers-help { height: 1; color: $terminal-white; }
"""


class MCPServerListPane(SettingsPane):
    """MCP Server 列表二级面板：增删改/启停，即时保存。"""

    BINDINGS = [
        Binding("escape", "cancel", "返回", priority=True),
        Binding("up", "move_up", "上一项", priority=True),
        Binding("down", "move_down", "下一项", priority=True),
        Binding("enter", "edit", "编辑", priority=True),
        Binding("space", "toggle", "启用/禁用", priority=True),
        ("a", "add", "添加"),
        ("d", "delete", "删除"),
    ]

    DEFAULT_CSS = terminal_css(_PANE_CSS)

    def __init__(self, agent: Any) -> None:
        super().__init__(agent=agent)
        self._config = _current_config(agent)
        self._names = list(self._config.servers)
        self._selected = 0
        self._editing_name = ""
        self._status = "Enter 编辑，Space 启用/禁用，A 添加。"
        self._pending_editor_result: Optional[MCPServerConfig] = None

    def compose_pane(self) -> ComposeResult:
        with VerticalScroll(id="mcp-servers-list"):
            for index in range(20):
                yield Static(" ", id=f"mcp-server-row-{index}", classes="mcp-server-row")
        yield Static(self._status, id="mcp-servers-status")
        yield Static("↑↓ 选择  Enter 编辑  Space 启用/禁用  A 添加  D 删除  Esc 返回", id="mcp-servers-help")

    def refresh_pane(self) -> None:
        if not self.is_mounted:
            return
        rows = list(self._names)
        for index in range(20):
            name = rows[index] if index < len(rows) else ""
            row = self.query_one(f"#mcp-server-row-{index}", Static)
            if not name:
                text = "  （暂无 Server，按 A 添加）" if index == 0 and not rows else ""
            else:
                server = self._config.servers[name]
                state = "启用" if server.enabled else "禁用"
                text = f"{'› ' if index == self._selected else '  '}{name}：{state} · {server.transport} · {server.risk_level}"
            row.update(text)
            row.set_class(bool(name) and index == self._selected, "selected")
        self.query_one("#mcp-servers-status", Static).update(self._status)

    def action_cancel(self) -> None:
        self.request_back()

    def action_move_up(self) -> None:
        self._selected = (self._selected - 1) % max(1, len(self._names))
        self.refresh_pane()

    def action_move_down(self) -> None:
        self._selected = (self._selected + 1) % max(1, len(self._names))
        self.refresh_pane()

    def action_add(self) -> None:
        self.request_navigate("mcp_editor", None)

    def action_edit(self) -> None:
        if self._names:
            server = self._config.servers[self._names[self._selected]]
            self.request_navigate("mcp_editor", server)

    def action_toggle(self) -> None:
        if not self._names:
            return
        name = self._names[self._selected]
        server = self._config.servers[name]
        self._save(replace(self._config, servers={**self._config.servers, name: replace(server, enabled=not server.enabled)}))

    def action_delete(self) -> None:
        if not self._names:
            return
        name = self._names[self._selected]
        servers = dict(self._config.servers)
        servers.pop(name, None)
        self._save(replace(self._config, servers=servers))

    def receive_editor_result(self, result: MCPServerConfig | None) -> None:
        """编辑器弹层关闭后回调：应用并保存新增/修改。"""
        if result is None:
            return
        servers = dict(self._config.servers)
        original_name = self._editing_name
        if result.name not in servers and len(servers) >= 20:
            self._status = "最多管理 20 个 MCP Server。"
            self.refresh_pane()
            return
        if original_name and original_name != result.name:
            servers.pop(original_name, None)
        servers[result.name] = result
        self._save(replace(self._config, servers=servers))

    def _save(self, config: MCPConfig) -> None:
        try:
            path = _apply_and_save(self, config)
            self._config = config
            self._names = list(config.servers)
            self._selected = min(self._selected, max(0, len(self._names) - 1))
            self._status = f"已保存：{path}"
        except (AgentError, MCPConfigError, SettingsConfigError, OSError) as exc:
            self._status = f"设置未完成：{exc}"
        self.refresh_pane()


class MCPServerListScreen(ModalScreen[Optional[MCPSettingsAction]]):
    """第三级整屏薄壳：内嵌 MCPServerListPane；编辑用独立编辑器弹层。"""

    BINDINGS = [
        ("escape", "cancel", "返回"),
        Binding("up", "move_up", "上一项", priority=True),
        Binding("down", "move_down", "下一项", priority=True),
        Binding("enter", "edit", "编辑", priority=True),
        Binding("space", "toggle", "启用/禁用", priority=True),
        ("a", "add", "添加"),
        ("d", "delete", "删除"),
    ]

    CSS = terminal_css("""
    MCPServerListScreen { align: center middle; background: $terminal-overlay; }
    #mcp-servers-dialog { width: 86; max-width: 95%; height: 28; max-height: 92%; padding: 1 2; border: round $terminal-border-strong; background: $terminal-surface; }
    #mcp-servers-title { height: 1; margin-bottom: 1; color: $terminal-white; text-style: bold; }
    """ + _PANE_CSS)

    def __init__(self, agent: Any) -> None:
        super().__init__()
        self._agent = agent
        self._pane: Optional[MCPServerListPane] = None

    def compose(self) -> ComposeResult:
        with Container(id="mcp-servers-dialog"):
            yield Static("MCP Server", id="mcp-servers-title")
            self._pane = MCPServerListPane(self._agent)
            self._pane.bind_pane_events(
                on_back=lambda: self.dismiss(None),
                on_navigate=self._open_editor,
            )
            yield self._pane

    def on_mount(self) -> None:
        if self._pane is not None:
            self._pane.focus()

    def _open_editor(self, target: str, payload: Any) -> None:
        from .mcp_server_editor_screen import MCPServerEditorScreen

        server = payload if isinstance(payload, MCPServerConfig) else None
        self._pane._editing_name = server.name if server is not None else ""
        existing = set(self._pane._names)
        self.app.push_screen(
            MCPServerEditorScreen(self._agent, server, existing_names=existing),
            self._pane.receive_editor_result,
        )

    def action_cancel(self) -> None:
        self.dismiss(None)

    def action_move_up(self) -> None:
        if self._pane is not None:
            self._pane.action_move_up()

    def action_move_down(self) -> None:
        if self._pane is not None:
            self._pane.action_move_down()

    def action_edit(self) -> None:
        if self._pane is not None:
            self._pane.action_edit()

    def action_toggle(self) -> None:
        if self._pane is not None:
            self._pane.action_toggle()

    def action_add(self) -> None:
        if self._pane is not None:
            self._pane.action_add()

    def action_delete(self) -> None:
        if self._pane is not None:
            self._pane.action_delete()


__all__ = ["MCPServerListPane", "MCPServerListScreen"]
