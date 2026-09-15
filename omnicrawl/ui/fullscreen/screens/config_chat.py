"""配置对话 UI 组件：无上下文、本地写回与恢复友好。"""

from __future__ import annotations

from typing import Any, Callable, List, Optional

from rich.text import Text
from textual.app import ComposeResult
from textual.binding import Binding
from textual.containers import Vertical, VerticalScroll
from textual.screen import ModalScreen
from textual.widget import Widget
from textual.widgets import Input, Static

from ....config_chat.service import ConfigChatError, ConfigChatService
from .panes import SettingsPane


class ConfigChatView(Widget, can_focus=True):
    """简陋的本地配置聊天视图；历史只存在于控件，不送入模型。"""

    DEFAULT_CSS = """
    ConfigChatView { height: 100%; width: 100%; }
    #config-chat-history { height: 1fr; padding: 0 1; }
    #config-chat-input { height: 3; margin-top: 1; border: solid ansi_blue; background: ansi_default; }
    #config-chat-input:focus { border: tall ansi_bright_blue; background: ansi_default; }
    #config-chat-hint { height: 1; color: ansi_bright_black; padding: 0 1; }
    """

    def __init__(
        self,
        service: ConfigChatService,
        *,
        on_exit: Optional[Callable[[], None]] = None,
        on_changed: Optional[Callable[[List[Any]], None]] = None,
    ) -> None:
        super().__init__()
        self.service = service
        self._on_exit_callback = on_exit
        self._on_changed_callback = on_changed
        self._history = Text()
        self._worker = None

    def compose(self) -> ComposeResult:
        with Vertical():
            yield VerticalScroll(id="config-chat-history")
            yield Input(
                placeholder="输入配置修改，例如：把接口端口改成 9000",
                id="config-chat-input",
            )
            yield Static("Enter 应用 · Esc 返回", id="config-chat-hint")

    def on_mount(self) -> None:
        self.query_one("#config-chat-input", Input).focus()
        self._render_history()

    def _render_history(self) -> None:
        history = self.query_one("#config-chat-history", VerticalScroll)
        history.remove_children()
        history.mount(Static(self._history))
        history.scroll_end(animate=False)

    def _append_user(self, text: str) -> None:
        if self._history.plain:
            self._history.append("\n")
        self._history.append("$ ", style="bright_black italic")
        self._history.append(text)
        self._render_history()

    def _append_change(self, changes: List[Any]) -> None:
        if self._history.plain:
            self._history.append("\n")
        self._history.append("· ", style="bright_black italic")
        self._history.append("已修改：", style="bright_black italic")
        self._history.append(
            "、".join(f"{change.path} = {change.value}" for change in changes),
            style="bright_black italic",
        )
        self._render_history()

    def _append_error(self, message: str) -> None:
        if self._history.plain:
            self._history.append("\n")
        self._history.append("△ ", style="red")
        self._history.append(message, style="red")
        self._render_history()

    def on_input_submitted(self, event: Input.Submitted) -> None:
        text = event.value.strip()
        if not text or self._worker is not None:
            return
        event.input.value = ""
        self._append_user(text)
        self._worker = self.run_worker(
            self._apply_in_worker,
            text,
            thread=True,
            exclusive=True,
        )

    def _apply_in_worker(self, text: str) -> None:
        try:
            changes = self.service.apply_text(text)
        except ConfigChatError as exc:
            self.app.call_from_thread(self._finish_result, None, str(exc))
        except Exception as exc:  # noqa: BLE001 - worker 错误转成可读提示
            self.app.call_from_thread(self._finish_result, None, f"配置修改失败：{exc}")
        else:
            self.app.call_from_thread(self._finish_result, changes, None)

    def _finish_result(self, changes: Optional[List[Any]], error: Optional[str]) -> None:
        self._worker = None
        if error:
            self._append_error(error)
        elif changes:
            self._append_change(changes)
            if self._on_changed_callback is not None:
                self._on_changed_callback(changes)
        self.query_one("#config-chat-input", Input).focus()

    def action_exit(self) -> None:
        if self._on_exit_callback is not None:
            self._on_exit_callback()


class ConfigChatPane(SettingsPane):
    """设置页右侧的简化配置对话面板。"""

    DEFAULT_CSS = ConfigChatView.DEFAULT_CSS

    def __init__(
        self,
        service: ConfigChatService,
        *,
        on_changed: Optional[Callable[[List[Any]], None]] = None,
    ) -> None:
        super().__init__(agent=service.agent)
        self.service = service
        self._on_changed = on_changed
        self._view: Optional[ConfigChatView] = None

    def compose_pane(self) -> ComposeResult:
        self._view = ConfigChatView(
            self.service,
            on_exit=self.request_back,
            on_changed=self._on_changed,
        )
        yield self._view

    def activate(self) -> None:
        if self._view is not None:
            self._view.query_one("#config-chat-input", Input).focus()
        else:
            self.focus()


class ConfigChatScreen(ModalScreen[Any]):
    """全屏配置对话模式；关闭后底层主 TUI 原样恢复。"""

    BINDINGS = [Binding("escape", "exit_chat", "退出配置对话", priority=True)]

    DEFAULT_CSS = """
    ConfigChatScreen { background: ansi_default; }
    #config-chat-screen-body { width: 100%; height: 100%; padding: 1 2; }
    #config-chat-screen-title { height: 1; color: ansi_bright_blue; text-style: bold; }
    """

    def __init__(self, service: ConfigChatService) -> None:
        super().__init__()
        self.service = service
        self.view: Optional[ConfigChatView] = None

    def compose(self) -> ComposeResult:
        with Vertical(id="config-chat-screen-body"):
            yield Static("通过对话修改设置", id="config-chat-screen-title")
            self.view = ConfigChatView(self.service, on_exit=self.action_exit)
            yield self.view

    def action_exit(self) -> None:
        self.dismiss(None)


__all__ = ["ConfigChatPane", "ConfigChatScreen", "ConfigChatView"]
