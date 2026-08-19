"""全屏 TUI 的 MCP Server 编辑器界面（P3 自 mcp_settings.py 拆出）。"""

from __future__ import annotations

from pathlib import Path
import shlex
from typing import Any, Optional

from textual.app import ComposeResult
from textual import work
from textual.binding import Binding
from textual.containers import Container, Horizontal, VerticalScroll
from textual.screen import ModalScreen
from textual.widgets import Button, Input, Select, Static

from ...mcp.client import MCPClientManager
from ...mcp.config import (
    MCPConfig,
    MCPConfigError,
    MCPServerConfig,
    MCP_TRANSPORT_STDIO,
    MCP_TRANSPORT_STREAMABLE_HTTP,
)
from .mcp_settings import _RISKS
from .theme import terminal_css, terminal_select_css


class MCPServerEditorScreen(ModalScreen[Optional[MCPServerConfig]]):
    """第三级编辑器：敏感环境变量和 HTTP 请求头只显示摘要。"""

    BINDINGS = [("escape", "cancel", "取消"), Binding("ctrl+s", "save", "保存", priority=True)]
    CSS = terminal_css("""
    MCPServerEditorScreen { align: center middle; background: $terminal-overlay; }
    #mcp-editor-dialog { width: 88; max-width: 96%; height: 39; max-height: 95%; padding: 1 2; border: round $terminal-white; background: $terminal-surface; }
    #mcp-editor-title { height: 1; margin-bottom: 1; color: $terminal-white; text-style: bold; }
    #mcp-editor-form { height: 1fr; }
    .mcp-editor-control { height: 3; margin-bottom: 1; }
    #mcp-editor-status { height: 2; color: $terminal-white; }
    #mcp-editor-actions { height: 3; align-horizontal: right; }
    """ + terminal_select_css())

    def __init__(self, agent: Any, server: MCPServerConfig | None, *, existing_names: set[str]) -> None:
        super().__init__()
        self._agent = agent
        self._original = server
        self._existing_names = existing_names
        self._draft = server or MCPServerConfig(name="new-server")

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
                headers_note = f"HTTP 请求头：已配置 {len(d.headers)} 项；仅 streamable_http 使用，留空保持原值"
                yield Static(headers_note, id="mcp-editor-headers-note")
                yield Input("", placeholder="例如 Authorization=Bearer TOKEN;X-API-Key=KEY", password=True, id="mcp-editor-headers", classes="mcp-editor-control")
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
        headers_text = self.query_one("#mcp-editor-headers", Input).value.strip()
        headers = dict(self._draft.headers)
        if headers_text:
            headers = {}
            for item in headers_text.split(";"):
                key, separator, value = item.partition("=")
                if not separator or not key.strip():
                    raise MCPConfigError("请求头格式必须是 Header=Value。")
                headers[key.strip()] = value.strip()
        result = MCPServerConfig(name=name, enabled=self._draft.enabled, transport=transport, command=command, args=args, url=url, env=env, headers=headers, timeout_seconds=timeout, risk_level=risk)
        from ...mcp.config import _load_server_config
        return _load_server_config(name, {"enabled": result.enabled, "transport": result.transport, "command": result.command, "args": result.args, "url": result.url, "env": result.env, "headers": result.headers, "timeout_seconds": result.timeout_seconds, "risk_level": result.risk_level}, default_timeout_seconds=30)

    def action_cancel(self) -> None:
        self.dismiss(None)

    def action_save(self) -> None:
        try:
            self.dismiss(self._read_server())
        except (ValueError, MCPConfigError) as exc:
            self.query_one("#mcp-editor-status", Static).update(f"保存失败：{exc}")
