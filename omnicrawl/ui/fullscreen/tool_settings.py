"""全屏 TUI 的二级工具开关设置面板。

逐工具切换内置工具在 Agent 工具表中的注册状态：默认除 ``powershell``
关闭外其余全部启用；切换后立即重建 Agent 工具表并写回 config.toml。
"""

from __future__ import annotations

from typing import Any, Optional

from textual import work
from textual.app import ComposeResult
from textual.binding import Binding
from textual.containers import Container, VerticalScroll
from textual.screen import ModalScreen
from textual.widgets import Static

from ...agent import AgentError
from ...config.tools import (
    TOOL_SWITCH_KEYS,
    TOOL_SWITCH_LABELS,
    ToolSwitchConfigError,
    save_tool_switch,
)
from .theme import terminal_css


class ToolSettingsScreen(ModalScreen[None]):
    """第二级：逐工具开关列表。Enter/空格/←/→ 切换，Esc 返回。"""

    BINDINGS = [
        ("escape", "cancel", "返回"),
        Binding("up", "move_up", "上一项", priority=True),
        Binding("down", "move_down", "下一项", priority=True),
        ("left", "previous_value", "上一个"),
        ("right", "next_value", "下一个"),
        ("enter", "confirm", "切换"),
        ("space", "confirm", "切换"),
    ]

    CSS = terminal_css("""
    ToolSettingsScreen { align: center middle; background: $terminal-overlay; }
    #tool-settings-dialog { width: 62; max-width: 94%; height: 29; max-height: 92%; padding: 1 2; border: round $terminal-border-strong; background: $terminal-surface; }
    #tool-settings-title { height: 1; margin-bottom: 1; color: $terminal-white; text-style: bold; }
    #tool-settings-list { height: 1fr; }
    .tool-settings-row { height: 1; padding: 0 1; color: $terminal-text-secondary; }
    .tool-settings-row.selected { color: $terminal-amber; text-style: bold; }
    #tool-settings-status { height: 2; color: $terminal-white; margin-top: 1; }
    #tool-settings-help { height: 1; color: $terminal-white; margin-top: 1; }
    """)

    def __init__(self, agent: Any) -> None:
        super().__init__()
        self._agent = agent
        self._selected = 0
        self._busy = False
        self._status = "Enter/空格/←/→ 切换开关；Esc 返回。"
        self._keys = tuple(TOOL_SWITCH_KEYS)
        # 当前已注册到 Agent 工具表的工具名，用于标注条件注册工具。
        self._registered = frozenset(getattr(getattr(agent, "_tools", None), "keys", lambda: ())())

    def compose(self) -> ComposeResult:
        with Container(id="tool-settings-dialog"):
            yield Static("工具开关", id="tool-settings-title")
            with VerticalScroll(id="tool-settings-list"):
                for index, key in enumerate(self._keys):
                    # 首次挂载即渲染真实状态，避免高负载下延迟刷新尚未执行时显示空白行。
                    marker = "› " if index == self._selected else "  "
                    yield Static(
                        marker + self._row_text(key),
                        id=f"tool-settings-row-{key}",
                        classes="tool-settings-row",
                    )
            yield Static(self._status, id="tool-settings-status")
            yield Static("↑↓ 选择  ←→/Enter/空格 切换  Esc 返回", id="tool-settings-help")

    def on_mount(self) -> None:
        self.call_after_refresh(self._render_rows)

    def action_cancel(self) -> None:
        if not self._busy:
            self.dismiss(None)

    def action_move_up(self) -> None:
        if not self._busy:
            self._selected = (self._selected - 1) % len(self._keys)
            self._render_rows()

    def action_move_down(self) -> None:
        if not self._busy:
            self._selected = (self._selected + 1) % len(self._keys)
            self._render_rows()

    def action_previous_value(self) -> None:
        self._toggle_selected()

    def action_next_value(self) -> None:
        self._toggle_selected()

    def action_confirm(self) -> None:
        self._toggle_selected()

    def _tool_enabled(self, key: str) -> bool:
        config = getattr(self._agent, "config", None)
        disabled = frozenset(getattr(config, "disabled_tools", ()))
        return key not in disabled

    def _row_text(self, key: str) -> str:
        label = TOOL_SWITCH_LABELS.get(key, key)
        state = "已启用" if self._tool_enabled(key) else "已关闭"
        if key not in self._registered:
            return f"{label}：{state}（未注册）"
        return f"{label}：{state}"

    def _toggle_selected(self) -> None:
        if self._busy:
            return
        key = self._keys[self._selected]
        enabled = not self._tool_enabled(key)
        self._apply_tool_switch(key, enabled)

    @work(thread=True, exclusive=True, group="tool-settings-apply", exit_on_error=False)
    def _apply_tool_switch(self, key: str, enabled: bool) -> None:
        self.app.call_from_thread(self._set_busy, True, "正在应用工具开关…")
        try:
            self._agent.set_tool_enabled(key, enabled)
            try:
                path = save_tool_switch(key, enabled)
            except Exception:
                self._agent.set_tool_enabled(key, not enabled)
                raise
            message = f"{TOOL_SWITCH_LABELS.get(key, key)}已{'启用' if enabled else '关闭'}，已保存到 {path}。"
        except (AgentError, ToolSwitchConfigError, OSError) as exc:
            message = f"设置未完成：{exc}"
        except Exception as exc:
            message = f"设置未完成：{exc}"
        self.app.call_from_thread(self._set_busy, False, message)

    def _set_busy(self, busy: bool, status: str) -> None:
        self._busy = busy
        self._status = status
        self._render_rows()

    def _render_rows(self) -> None:
        if not self.is_mounted:
            return
        for index, key in enumerate(self._keys):
            marker = "› " if index == self._selected else "  "
            row = self.query_one(f"#tool-settings-row-{key}", Static)
            row.update(marker + self._row_text(key))
            selected = index == self._selected
            row.set_class(selected, "selected")
            if selected:
                row.scroll_visible(animate=False)
        self.query_one("#tool-settings-status", Static).update(self._status)


__all__ = ["ToolSettingsScreen"]
