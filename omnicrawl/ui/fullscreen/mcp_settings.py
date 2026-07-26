"""全屏 TUI 的三级 MCP 设置界面。"""

from __future__ import annotations

from dataclasses import dataclass, replace
from pathlib import Path
import shlex
from typing import Any, Optional

from textual.app import ComposeResult
from textual.renderables.blank import Blank
from textual import work
from textual.binding import Binding
from textual.containers import Container, Horizontal, VerticalScroll
from textual.screen import ModalScreen
from textual.widgets import Button, Input, Select, Static

from ...agent import AgentError
from ...config.settings import SettingsConfigError, save_mcp_config
from ...mcp.client import MCPClientManager
from ...mcp.config import (
    MCPConfig,
    MCPConfigError,
    MCPServerConfig,
    MCP_RISK_EXTERNAL,
    MCP_RISK_RESTRICTED,
    MCP_RISK_TRUSTED,
    MCP_TRANSPORT_STDIO,
    MCP_TRANSPORT_STREAMABLE_HTTP,
    load_mcp_config,
)
from .theme import terminal_css, terminal_select_css


@dataclass(frozen=True)
class MCPSettingsAction:
    name: str


_RISKS = (MCP_RISK_TRUSTED, MCP_RISK_RESTRICTED, MCP_RISK_EXTERNAL)
_TRANSPORTS = (MCP_TRANSPORT_STDIO, MCP_TRANSPORT_STREAMABLE_HTTP)


def _current_config(agent: Any) -> MCPConfig:
    manager = getattr(agent, "_mcp_manager", None)
    config = getattr(agent, "config", None)
    candidate = getattr(config, "mcp_config", None) or getattr(manager, "config", None)
    return candidate if isinstance(candidate, MCPConfig) else load_mcp_config()


def _apply_and_save(screen: ModalScreen[Any], config: MCPConfig) -> str:
    """应用配置并写盘；写盘失败时回滚当前运行态。"""

    previous = _current_config(screen._agent)
    apply_config = getattr(screen._agent, "apply_mcp_config", None)
    if callable(apply_config):
        apply_config(config)
    else:
        screen._agent.config.mcp_config = config
    try:
        return str(save_mcp_config(config))
    except Exception:
        try:
            if callable(apply_config):
                apply_config(previous)
            else:
                screen._agent.config.mcp_config = previous
        except Exception:
            pass
        raise


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
    #mcp-settings-dialog { width: 82; max-width: 95%; height: 27; max-height: 92%; padding: 1 2; border: solid $terminal-border-strong; background: $terminal-surface; }
    #mcp-settings-title { height: 1; margin-bottom: 1; color: $terminal-green; text-style: bold; }
    #mcp-settings-list { height: 1fr; }
    .mcp-settings-row { height: 2; padding: 0 1; color: $terminal-text-secondary; }
    .mcp-settings-row.selected { color: $terminal-text; background: $terminal-blue-soft; text-style: bold; }
    #mcp-settings-status { height: 2; color: $terminal-blue; margin-top: 1; }
    #mcp-settings-help { height: 1; color: $terminal-text-muted; margin-top: 1; }
    """)

    _ROWS = ("enabled", "network", "write", "command", "audit", "timeout", "output", "servers")

    def __init__(self, agent: Any) -> None:
        super().__init__()
        self._agent = agent
        self._config = _current_config(agent)
        self._selected = 0
        self._busy = False
        self._status = "Enter/空格修改；Server 管理进入下一级。"

    def render(self) -> Blank:
        return Blank(self.styles.background)

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
        if key in {"timeout", "output"}:
            options = (10, 30, 60, 120, 300) if key == "timeout" else (2000, 6000, 12000, 30000, 60000)
            current = self._config.default_timeout_seconds if key == "timeout" else self._config.max_tool_output_chars
            index = min(range(len(options)), key=lambda i: abs(options[i] - current))
            value = options[(index + direction) % len(options)]
            self._config = replace(self._config, **({"default_timeout_seconds": value} if key == "timeout" else {"max_tool_output_chars": value}))
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
            "output": f"{self._config.max_tool_output_chars} 字符",
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


class MCPServerListScreen(ModalScreen[Optional[MCPSettingsAction]]):
    """第三级入口：MCP Server 列表。"""

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
    #mcp-servers-dialog { width: 86; max-width: 95%; height: 28; max-height: 92%; padding: 1 2; border: solid $terminal-border-strong; background: $terminal-surface; }
    #mcp-servers-title { height: 1; margin-bottom: 1; color: $terminal-green; text-style: bold; }
    #mcp-servers-list { height: 1fr; }
    .mcp-server-row { height: 2; padding: 0 1; color: $terminal-text-secondary; }
    .mcp-server-row.selected { color: $terminal-text; background: $terminal-blue-soft; text-style: bold; }
    #mcp-servers-status { height: 2; color: $terminal-blue; margin-top: 1; }
    #mcp-servers-help { height: 1; color: $terminal-text-muted; margin-top: 1; }
    """)

    def __init__(self, agent: Any) -> None:
        super().__init__()
        self._agent = agent
        self._config = _current_config(agent)
        self._names = list(self._config.servers)
        self._selected = 0
        self._editing_name = ""
        self._status = "Enter 编辑，Space 启用/禁用，A 添加。"

    def render(self) -> Blank:
        return Blank(self.styles.background)

    def compose(self) -> ComposeResult:
        with Container(id="mcp-servers-dialog"):
            yield Static("MCP Server", id="mcp-servers-title")
            with VerticalScroll(id="mcp-servers-list"):
                for index in range(20):
                    yield Static(" ", id=f"mcp-server-row-{index}", classes="mcp-server-row")
            yield Static(self._status, id="mcp-servers-status")
            yield Static("↑↓ 选择  Enter 编辑  Space 启用/禁用  A 添加  D 删除  Esc 返回", id="mcp-servers-help")

    def on_mount(self) -> None:
        self.call_after_refresh(self._render_rows)

    def action_cancel(self) -> None:
        self.dismiss(None)

    def action_move_up(self) -> None:
        self._selected = (self._selected - 1) % max(1, len(self._names))
        self._render_rows()

    def action_move_down(self) -> None:
        self._selected = (self._selected + 1) % max(1, len(self._names))
        self._render_rows()

    def action_add(self) -> None:
        self._open_editor(None)

    def action_edit(self) -> None:
        if self._names:
            self._open_editor(self._config.servers[self._names[self._selected]])

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

    def _open_editor(self, server: MCPServerConfig | None) -> None:
        existing = set(self._names)
        self._editing_name = server.name if server is not None else ""
        self.app.push_screen(MCPServerEditorScreen(self._agent, server, existing_names=existing), self._receive_editor)

    def _receive_editor(self, result: MCPServerConfig | None) -> None:
        if result is None:
            return
        servers = dict(self._config.servers)
        original_name = self._editing_name
        if result.name not in servers and len(servers) >= 20:
            self._status = "最多管理 20 个 MCP Server。"
            self._render_rows()
            return
        if original_name and original_name != result.name:
            servers.pop(original_name, None)
        servers[result.name] = result
        self._save(replace(self._config, servers=servers))
        self._names = list(self._config.servers)

    def _save(self, config: MCPConfig) -> None:
        try:
            path = _apply_and_save(self, config)
            self._config = config
            self._names = list(config.servers)
            self._selected = min(self._selected, max(0, len(self._names) - 1))
            self._status = f"已保存：{path}"
        except (AgentError, MCPConfigError, SettingsConfigError, OSError) as exc:
            self._status = f"设置未完成：{exc}"
        self._render_rows()

    def _render_rows(self) -> None:
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


class MCPServerEditorScreen(ModalScreen[Optional[MCPServerConfig]]):
    """第三级编辑器：敏感环境变量只显示摘要。"""

    BINDINGS = [("escape", "cancel", "取消"), Binding("ctrl+s", "save", "保存", priority=True)]
    CSS = terminal_css("""
    MCPServerEditorScreen { align: center middle; background: $terminal-overlay; }
    #mcp-editor-dialog { width: 88; max-width: 96%; height: 35; max-height: 95%; padding: 1 2; border: solid $terminal-blue; background: $terminal-surface; }
    #mcp-editor-title { height: 1; margin-bottom: 1; color: $terminal-green; text-style: bold; }
    #mcp-editor-form { height: 1fr; }
    .mcp-editor-control { height: 3; margin-bottom: 1; }
    #mcp-editor-status { height: 2; color: $terminal-amber; }
    #mcp-editor-actions { height: 3; align-horizontal: right; }
    """ + terminal_select_css())

    def __init__(self, agent: Any, server: MCPServerConfig | None, *, existing_names: set[str]) -> None:
        super().__init__()
        self._agent = agent
        self._original = server
        self._existing_names = existing_names
        self._draft = server or MCPServerConfig(name="new-server")

    def render(self) -> Blank:
        return Blank(self.styles.background)

    def compose(self) -> ComposeResult:
        d = self._draft
        with Container(id="mcp-editor-dialog"):
            yield Static("编辑 MCP Server" if self._original else "添加 MCP Server", id="mcp-editor-title")
            with VerticalScroll(id="mcp-editor-form"):
                yield Input(d.name, placeholder="名称：小写字母、数字、_、-", id="mcp-editor-name", classes="mcp-editor-control")
                yield Select([("stdio", MCP_TRANSPORT_STDIO), ("streamable_http", MCP_TRANSPORT_STREAMABLE_HTTP)], value=d.transport, allow_blank=False, id="mcp-editor-transport", classes="mcp-editor-control choice-select")
                yield Input(d.command or "", placeholder="stdio 启动命令，例如 python", id="mcp-editor-command", classes="mcp-editor-control")
                yield Input(shlex.join(d.args), placeholder="stdio 参数，以空格分隔", id="mcp-editor-args", classes="mcp-editor-control")
                yield Input(d.url or "", placeholder="HTTP URL，例如 https://example.com/mcp", id="mcp-editor-url", classes="mcp-editor-control")
                yield Input(str(d.timeout_seconds), placeholder="超时秒数", id="mcp-editor-timeout", classes="mcp-editor-control")
                yield Select([(item, item) for item in _RISKS], value=d.risk_level, allow_blank=False, id="mcp-editor-risk", classes="mcp-editor-control choice-select")
                env_note = f"stdio 环境变量：已配置 {len(d.env)} 项；留空保持原值，输入 KEY=VALUE;KEY2=VALUE 可替换"
                yield Static(env_note, id="mcp-editor-env-note")
                yield Input("", placeholder="仅传给 stdio 子进程；凭据不会回显", password=True, id="mcp-editor-env", classes="mcp-editor-control")
                yield Static(" ", id="mcp-editor-status")
            with Horizontal(id="mcp-editor-actions"):
                yield Button("取消", id="mcp-editor-cancel")
                yield Button("测试连接", id="mcp-editor-test")
                yield Button("保存", variant="primary", id="mcp-editor-save")

    def on_button_pressed(self, event: Button.Pressed) -> None:
        if event.button.id == "mcp-editor-save":
            self.action_save()
        elif event.button.id == "mcp-editor-cancel":
            self.action_cancel()
        elif event.button.id == "mcp-editor-test":
            self.action_test_connection()

    def action_test_connection(self) -> None:
        try:
            server = self._read_server()
        except (ValueError, MCPConfigError) as exc:
            self.query_one("#mcp-editor-status", Static).update(f"测试失败：{exc}")
            return
        self._set_status("正在测试连接…")
        self._test_connection(server)

    @work(thread=True, exclusive=True, group="mcp-server-test", exit_on_error=False)
    def _test_connection(self, server: MCPServerConfig) -> None:
        manager = MCPClientManager(
            MCPConfig(enabled=True, servers={server.name: server}),
            workspace_root=Path(getattr(self._agent, "workspace_root", Path.cwd())),
        )
        try:
            manager.discover()
            status = manager.format_status().splitlines()[0]
        except Exception as exc:
            status = f"测试失败：{exc}"
        finally:
            manager.close()
        self.app.call_from_thread(self._set_status, status)

    def _set_status(self, status: str) -> None:
        self.query_one("#mcp-editor-status", Static).update(status)

    def _read_server(self) -> MCPServerConfig:
        name = self.query_one("#mcp-editor-name", Input).value.strip()
        transport = str(self.query_one("#mcp-editor-transport", Select).value)
        command = self.query_one("#mcp-editor-command", Input).value.strip() or None
        args = shlex.split(self.query_one("#mcp-editor-args", Input).value, posix=True)
        url = self.query_one("#mcp-editor-url", Input).value.strip() or None
        timeout = int(self.query_one("#mcp-editor-timeout", Input).value.strip())
        risk = str(self.query_one("#mcp-editor-risk", Select).value)
        env_text = self.query_one("#mcp-editor-env", Input).value.strip()
        if not name or (name != self._draft.name and name in self._existing_names):
            raise MCPConfigError("Server 名称不能为空且不能重复。")
        env = dict(self._draft.env)
        if env_text:
            env = {}
            for item in env_text.split(";"):
                key, separator, value = item.partition("=")
                if not separator or not key.strip():
                    raise MCPConfigError("环境变量格式必须是 KEY=VALUE。")
                env[key.strip()] = value
        result = MCPServerConfig(name=name, enabled=self._draft.enabled, transport=transport, command=command, args=args, url=url, env=env, timeout_seconds=timeout, risk_level=risk)
        from ...mcp.config import _load_server_config
        return _load_server_config(name, {"enabled": result.enabled, "transport": result.transport, "command": result.command, "args": result.args, "url": result.url, "env": result.env, "timeout_seconds": result.timeout_seconds, "risk_level": result.risk_level}, default_timeout_seconds=30)

    def action_cancel(self) -> None:
        self.dismiss(None)

    def action_save(self) -> None:
        try:
            self.dismiss(self._read_server())
        except (ValueError, MCPConfigError) as exc:
            self.query_one("#mcp-editor-status", Static).update(f"保存失败：{exc}")


__all__ = ["MCPSettingsAction", "MCPSettingsScreen", "MCPServerEditorScreen", "MCPServerListScreen"]
