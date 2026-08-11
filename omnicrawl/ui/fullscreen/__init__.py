"""Textual 全屏工作台。"""

from __future__ import annotations

import re
import sys
import threading
import time
from collections import deque
from dataclasses import dataclass
from typing import Any, Callable

from rich.text import Text
from textual import events, work
from textual.app import App, ComposeResult
from textual.binding import Binding
from textual.containers import Horizontal, Vertical, VerticalScroll
from textual.geometry import Offset
from textual.scrollbar import ScrollBar
from textual.widgets import Static, TextArea

if sys.platform == "win32":
    from textual.drivers import win32 as _textual_win32
    from textual.drivers.windows_driver import WindowsDriver as _TextualWindowsDriver
else:
    _textual_win32 = None
    _TextualWindowsDriver = object

from ...agent import AgentError, LocalToolAgent
from ...config.runtime import resolve_config_path, resolve_models_path
from ...llm.stream_registry import stream_scope
from ...agent.tools import public_tool_arguments
from ...version_check import current_version
from .hud import (
    SEARCH_INDEX_SPINNER_FRAMES,
    compact_token_count,
    context_summary_text,
    gradient_text,
    pending_queue_text,
    search_index_status_text,
    status_summary_text,
    token_telemetry_text,
)
# 保留这些模块级名称作为既有测试和扩展的 patch 点；实际分派位于 commands.py。
from ...commands.slash import (
    build_slash_command_options,
    format_memory_clean_result,
    format_mcp_status,
    format_plugins_status,
    format_skills_list,
    format_tool_confirmation,
    handle_approval_command,
    handle_reasoning_command,
    handle_session_command,
    handle_subagent_task_command,
)
from .channel_manager import ChannelManagerResult, ChannelManagerScreen
from .commands import CommandDispatcher
from .model_picker import ModelPickerResult, ModelPickerScreen
from .settings import SettingsAction, SettingsScreen
from .mcp_settings import MCPServerListScreen, MCPSettingsAction, MCPSettingsScreen
from .tool_settings import ToolSettingsScreen
from .vision_settings import VisionSettingsResult, VisionSettingsScreen
from .monitor import MonitorStateAdapter, format_monitor_display_batch
from .theme import (
    ACCENT_BLUE,
    BORDER_MUTED,
    TERMINAL_THEME,
    TEXT_MUTED,
    TEXT_SECONDARY,
    THEME_NAME,
    terminal_css,
)
from .turns import AgentTurnCallbacks, AgentTurnController
from .widgets import (
    AssistantMessage,
    ConfirmationScreen,
    ReasoningDisclosure,
    SubAgentProgressTree,
    ToolDisclosure,
)


_MOUSE_REPORTING_DISABLE_SEQUENCE = (
    "\x1b[?1000l"
    "\x1b[?1002l"
    "\x1b[?1003l"
    "\x1b[?1015l"
    "\x1b[?1006l"
)
_PASTE_COMPACT_LINE_THRESHOLD = 5
_PASTE_PLACEHOLDER_PATTERN = re.compile(r"\[粘贴 #\d+ \+\d+ 行\]")


def _normalize_pasted_text(text: str) -> str:
    """把终端粘贴中的 CRLF/CR 统一为 TextArea 使用的 LF。"""

    return text.replace("\r\n", "\n").replace("\r", "\n")


def _count_paste_lines(text: str) -> int:
    """按编辑器语义统计粘贴行数，保留末尾空行。"""

    if not text:
        return 0
    return text.count("\n") + 1


def _disable_terminal_mouse_reporting(output_stream: Any | None = None) -> None:
    """在 Textual Driver 停止后兜底关闭所有可能启用的鼠标报告模式。"""

    output_stream = sys.__stdout__ if output_stream is None else output_stream
    if output_stream is None:
        return
    try:
        # 退出阶段不能继续使用 Driver.write：Windows WriterThread 此时已经停止，
        # 写入只会滞留在无人消费的队列中，因此必须直接写回真实控制台流。
        output_stream.write(_MOUSE_REPORTING_DISABLE_SEQUENCE)
        output_stream.flush()
    except (AttributeError, OSError, ValueError):
        # 关闭窗口或重定向流已提前失效时，不应让兜底清理覆盖原始退出结果。
        return


def _restore_windows_vt_input_mode_if_needed(
    *,
    platform_name: str | None = None,
    input_stream: Any | None = None,
    output_stream: Any | None = None,
    win32_api: Any | None = None,
) -> bool:
    """恢复可能被锁屏或息屏重置的 Windows 控制台 VT 输入模式。"""

    if (platform_name or sys.platform) != "win32":
        return False

    if win32_api is None:
        from textual.drivers import win32 as win32_api

    input_stream = sys.__stdin__ if input_stream is None else input_stream
    output_stream = sys.__stdout__ if output_stream is None else output_stream
    if input_stream is None or output_stream is None:
        return False

    try:
        input_mode = win32_api.get_console_mode(input_stream)
        required_input_mode = win32_api.ENABLE_VIRTUAL_TERMINAL_INPUT
        if input_mode == required_input_mode:
            return False

        # Textual 的 Windows 输入线程只解析 VT 字符序列；普通控制台模式下，
        # 方向键会变成空字符的虚拟键记录，鼠标会变成其未处理的 MOUSE_EVENT。
        if not win32_api.set_console_mode(input_stream, required_input_mode):
            return False

        output_mode = win32_api.get_console_mode(output_stream)
        win32_api.set_console_mode(
            output_stream,
            output_mode | win32_api.ENABLE_VIRTUAL_TERMINAL_PROCESSING,
        )
        return True
    except (AttributeError, OSError, ValueError):
        # 重定向输入、非控制台 Host 或终端关闭期间可能无法读取 ConsoleMode；
        # 此时保留 Textual 当前状态，避免定时器异常打断主事件循环。
        return False


def _restore_windows_raw_input_mode_if_needed(
    *,
    platform_name: str | None = None,
    input_stream: Any | None = None,
    output_stream: Any | None = None,
    win32_api: Any | None = None,
) -> bool:
    """恢复可保留修饰键的 Windows 原始控制台输入模式。"""

    if (platform_name or sys.platform) != "win32":
        return False

    if win32_api is None:
        from textual.drivers import win32 as win32_api

    input_stream = sys.__stdin__ if input_stream is None else input_stream
    output_stream = sys.__stdout__ if output_stream is None else output_stream
    if input_stream is None or output_stream is None:
        return False

    try:
        input_mode = win32_api.get_console_mode(input_stream)
        required_input_mode = (
            input_mode
            | win32_api.ENABLE_MOUSE_INPUT
            | win32_api.ENABLE_WINDOW_INPUT
            | win32_api.ENABLE_EXTENDED_FLAGS
        ) & ~(
            win32_api.ENABLE_QUICK_EDIT_MODE
            | win32_api.ENABLE_VIRTUAL_TERMINAL_INPUT
        )
        if input_mode == required_input_mode:
            return False
        if not win32_api.set_console_mode(input_stream, required_input_mode):
            return False

        output_mode = win32_api.get_console_mode(output_stream)
        win32_api.set_console_mode(
            output_stream,
            output_mode | win32_api.ENABLE_VIRTUAL_TERMINAL_PROCESSING,
        )
        return True
    except (AttributeError, OSError, ValueError):
        return False


@dataclass(frozen=True)
class FullscreenStartup:
    """启动阶段提供给顶部上下文条的只读摘要。"""

    thinking_enabled: bool
    reasoning_effort: str
    approval_label: str
    workspace_label: str
    temp_label: str
    current_version: str = current_version()
    version_check_enabled: bool = False


class Composer(TextArea):
    """多行编辑器：Enter 由应用提交，Shift+Enter 插入真实换行。"""

    def __init__(
        self,
        *,
        submit_handler: Callable[[], None],
        command_key_handler: Callable[[events.Key], bool],
        copy_or_clear_handler: Callable[[], None],
        paste_handler: Callable[[str], str | None],
        **kwargs: Any,
    ) -> None:
        super().__init__(**kwargs)
        self._submit_handler = submit_handler
        self._command_key_handler = command_key_handler
        self._copy_or_clear_handler = copy_or_clear_handler
        self._paste_handler = paste_handler

    def _insert_paste_text(self, text: str) -> None:
        replacement = self._paste_handler(text)
        insert_text = replacement if replacement is not None else _normalize_pasted_text(text)
        if result := self._replace_via_keyboard(insert_text, *self.selection):
            self.move_cursor(result.end_location)
            self.focus()

    async def _on_paste(self, event: events.Paste) -> None:
        self._insert_paste_text(event.text)
        event.prevent_default()
        event.stop()

    def action_paste(self) -> None:
        if self.read_only:
            return
        self._insert_paste_text(self.app.clipboard)

    def on_key(self, event: events.Key) -> None:
        if event.key == "escape":
            self.app.action_cancel_or_focus()
            event.prevent_default()
            event.stop()
            return
        # 某些 Windows Terminal / VS Code 组合会把 Shift+Enter 归一为
        # Key("enter", "\n")；先判断换行事件，避免被普通 Enter 分支提交。
        is_newline_key = event.key in {
            "shift+enter",
            "shift+\r",
            "shift+j",
        } or (event.key == "enter" and event.character == "\n")
        if event.key == "ctrl+c":
            self._copy_or_clear_handler()
            event.prevent_default()
            event.stop()
        elif is_newline_key:
            self.insert("\n")
            event.prevent_default()
            event.stop()
        elif self._command_key_handler(event):
            event.prevent_default()
            event.stop()
        elif event.key in {"up", "down"}:
            scroll_action = getattr(
                self.app,
                f"action_scroll_conversation_{event.key}",
            )
            scroll_action()
            event.prevent_default()
            event.stop()
        elif event.key == "enter":
            self._submit_handler()
            event.prevent_default()
            event.stop()


class OmniCrawlWindowsEventMonitor(
    _textual_win32.EventMonitor if _textual_win32 is not None else object
):
    """将 Windows 原始控制台记录转换为 Textual 事件。"""

    WINDOWS_ALT_PRESSED = 0x0003
    WINDOWS_CTRL_PRESSED = 0x000C
    WINDOWS_SHIFT_PRESSED = 0x0010
    WINDOWS_RETURN_KEY = 0x000D
    WINDOWS_TAB_KEY = 0x0009
    WINDOWS_ESCAPE_KEY = 0x001B
    WINDOWS_MODIFIER_KEYS = {0x0010, 0x0011, 0x0012}
    WINDOWS_SPECIAL_KEYS = {
        0x0021: "pageup",
        0x0022: "pagedown",
        0x0023: "end",
        0x0024: "home",
        0x0025: "left",
        0x0026: "up",
        0x0027: "right",
        0x0028: "down",
        0x002D: "insert",
        0x002E: "delete",
    }
    WINDOWS_MOUSE_BUTTONS = (
        (0x0001, 1),
        (0x0004, 2),
        (0x0002, 3),
        (0x0008, 4),
        (0x0010, 5),
    )
    WINDOWS_MOUSE_BUTTON_MASK = 0x001F
    WINDOWS_MOUSE_MOVED = 0x0001
    WINDOWS_MOUSE_DOUBLE_CLICK = 0x0002
    WINDOWS_MOUSE_WHEELED = 0x0004
    WINDOWS_MOUSE_HWHEELED = 0x0008

    @classmethod
    def _key_with_modifiers(cls, key: str, control_key_state: int) -> str:
        """生成与 Textual XTermParser 一致的修饰键名称。"""

        modifiers: list[str] = []
        if control_key_state & cls.WINDOWS_ALT_PRESSED:
            modifiers.append("alt")
        if control_key_state & cls.WINDOWS_CTRL_PRESSED:
            modifiers.append("ctrl")
        if control_key_state & cls.WINDOWS_SHIFT_PRESSED:
            modifiers.append("shift")
        return "+".join([*modifiers, key])

    @classmethod
    def key_event_to_textual(cls, key_event: Any) -> events.Key | None:
        """转换需依赖虚拟键码的原始 Windows 按键事件。"""

        if not getattr(key_event, "bKeyDown", False):
            return None

        character = getattr(key_event.uChar, "UnicodeChar", "")
        virtual_key = int(getattr(key_event, "wVirtualKeyCode", 0))
        control_key_state = int(getattr(key_event, "dwControlKeyState", 0))
        if virtual_key in cls.WINDOWS_MODIFIER_KEYS:
            return None

        if (
            virtual_key == cls.WINDOWS_RETURN_KEY
            and character in {"\r", "\n"}
            and control_key_state & cls.WINDOWS_SHIFT_PRESSED
        ):
            return events.Key(
                cls._key_with_modifiers("enter", control_key_state), character
            )
        if (
            virtual_key == cls.WINDOWS_TAB_KEY
            and character == "\t"
            and control_key_state & cls.WINDOWS_SHIFT_PRESSED
        ):
            return events.Key(
                cls._key_with_modifiers("tab", control_key_state), None
            )

        key_name = cls.WINDOWS_SPECIAL_KEYS.get(virtual_key)
        if key_name is None and 0x0070 <= virtual_key <= 0x0087:
            key_name = f"f{virtual_key - 0x006F}"
        if key_name is not None and character == "\x00":
            return events.Key(
                cls._key_with_modifiers(key_name, control_key_state), None
            )
        if virtual_key == cls.WINDOWS_ESCAPE_KEY:
            return events.Key("escape", None)
        return None

    @classmethod
    def mouse_events_from_raw(
        cls,
        mouse_event: Any,
        *,
        previous_button_state: int,
        previous_position: tuple[int, int],
    ) -> tuple[list[events.MouseEvent], int, tuple[int, int]]:
        """将原始鼠标记录转换为 Textual 鼠标消息并保留按钮状态。"""

        x = int(mouse_event.dwMousePosition.X)
        y = int(mouse_event.dwMousePosition.Y)
        delta_x = x - previous_position[0]
        delta_y = y - previous_position[1]
        control_key_state = int(mouse_event.dwControlKeyState)
        button_state = int(mouse_event.dwButtonState) & cls.WINDOWS_MOUSE_BUTTON_MASK
        event_flags = int(mouse_event.dwEventFlags)
        shift = bool(control_key_state & cls.WINDOWS_SHIFT_PRESSED)
        meta = bool(control_key_state & cls.WINDOWS_ALT_PRESSED)
        ctrl = bool(control_key_state & cls.WINDOWS_CTRL_PRESSED)

        def make_mouse_event(
            event_type: type[events.MouseEvent], button: int = 0
        ) -> events.MouseEvent:
            return event_type(
                None,
                x,
                y,
                delta_x,
                delta_y,
                button,
                shift,
                meta,
                ctrl,
                screen_x=x,
                screen_y=y,
            )

        messages: list[events.MouseEvent] = []
        if event_flags & cls.WINDOWS_MOUSE_WHEELED:
            wheel_delta = (int(mouse_event.dwButtonState) >> 16) & 0xFFFF
            if wheel_delta & 0x8000:
                wheel_delta -= 0x10000
            if wheel_delta:
                event_type = (
                    events.MouseScrollUp
                    if wheel_delta > 0
                    else events.MouseScrollDown
                )
                messages.append(make_mouse_event(event_type))
        elif event_flags & cls.WINDOWS_MOUSE_HWHEELED:
            wheel_delta = (int(mouse_event.dwButtonState) >> 16) & 0xFFFF
            if wheel_delta & 0x8000:
                wheel_delta -= 0x10000
            if wheel_delta:
                event_type = (
                    events.MouseScrollRight
                    if wheel_delta > 0
                    else events.MouseScrollLeft
                )
                messages.append(make_mouse_event(event_type))
        elif event_flags & cls.WINDOWS_MOUSE_MOVED:
            button = next(
                (
                    button_number
                    for mask, button_number in cls.WINDOWS_MOUSE_BUTTONS
                    if button_state & mask
                ),
                0,
            )
            messages.append(make_mouse_event(events.MouseMove, button))
        else:
            released = previous_button_state & ~button_state
            pressed = button_state & ~previous_button_state
            for mask, button in cls.WINDOWS_MOUSE_BUTTONS:
                if released & mask:
                    messages.append(make_mouse_event(events.MouseUp, button))
            for mask, button in cls.WINDOWS_MOUSE_BUTTONS:
                if pressed & mask:
                    messages.append(make_mouse_event(events.MouseDown, button))

        return messages, button_state, (x, y)

    def run(self) -> None:
        if _textual_win32 is None:
            return

        win32 = _textual_win32
        exit_requested = self.exit_event.is_set
        parser = win32.XTermParser(debug=win32.constants.DEBUG)

        try:
            read_count = win32.wintypes.DWORD(0)
            h_in = win32.GetStdHandle(win32.STD_INPUT_HANDLE)
            max_events = 1024
            key_event_type = 0x0001
            mouse_event_type = 0x0002
            window_buffer_size_event = 0x0004
            focus_event_type = 0x0010
            input_records = (win32.INPUT_RECORD * max_events)()
            read_console_input = win32.KERNEL32.ReadConsoleInputW
            keys: list[str] = []
            mouse_button_state = 0
            mouse_position = (0, 0)

            def flush_keys() -> None:
                if not keys:
                    return
                for parsed_event in parser.feed(
                    "".join(keys)
                    .encode("utf-16", "surrogatepass")
                    .decode("utf-16")
                ):
                    self.process_event(parsed_event)
                del keys[:]

            while not exit_requested():
                for event in parser.tick():
                    self.process_event(event)

                if win32.wait_for_handles([h_in], 100) is None:
                    continue

                read_console_input(
                    h_in,
                    win32.byref(input_records),
                    max_events,
                    win32.byref(read_count),
                )
                read_input_records = input_records[: read_count.value]
                new_size: tuple[int, int] | None = None

                for input_record in read_input_records:
                    event_type = input_record.EventType
                    if event_type == key_event_type:
                        key_event = input_record.Event.KeyEvent
                        normalized_event = self.key_event_to_textual(key_event)
                        if normalized_event is not None:
                            flush_keys()
                            self.process_event(normalized_event)
                        elif key_event.bKeyDown:
                            key = key_event.uChar.UnicodeChar
                            if key and key != "\x00":
                                keys.append(key)
                    elif event_type == mouse_event_type:
                        flush_keys()
                        mouse_messages, mouse_button_state, mouse_position = (
                            self.mouse_events_from_raw(
                                input_record.Event.MouseEvent,
                                previous_button_state=mouse_button_state,
                                previous_position=mouse_position,
                            )
                        )
                        for mouse_message in mouse_messages:
                            self.process_event(mouse_message)
                    elif event_type == window_buffer_size_event:
                        size = input_record.Event.WindowBufferSizeEvent.dwSize
                        new_size = (size.X, size.Y)
                    elif event_type == focus_event_type:
                        flush_keys()
                        focus_event = input_record.Event.FocusEvent
                        self.process_event(
                            events.AppFocus()
                            if focus_event.bSetFocus
                            else events.AppBlur()
                        )

                flush_keys()
                if new_size is not None:
                    self.on_size_change(*new_size)
        except Exception as error:
            self.app.log.error("EVENT MONITOR ERROR", error)


class OmniCrawlWindowsDriver(_TextualWindowsDriver):
    """Windows 全屏输入驱动：使用原始控制台事件保留修饰键。"""

    KEYBOARD_PROTOCOL = "\x1b[>25u"
    RAW_INPUT_PROTOCOL_RESET = (
        _MOUSE_REPORTING_DISABLE_SEQUENCE + "\x1b[?1004l\x1b[?2004l\x1b[<u"
    )

    def start_application_mode(self) -> None:
        original_event_monitor = _textual_win32.EventMonitor
        _textual_win32.EventMonitor = OmniCrawlWindowsEventMonitor
        try:
            super().start_application_mode()
        finally:
            _textual_win32.EventMonitor = original_event_monitor
        _restore_windows_raw_input_mode_if_needed()
        self.flush()


class OmniCrawlApp(App[None]):
    """可控全屏渲染的 OmniCrawl 工作台。"""

    TITLE = "OmniCrawl"
    SUB_TITLE = "Developer Workspace"
    CSS = terminal_css("""
    Screen { background: $terminal-canvas; color: $terminal-text; }
    #shell { height: 1fr; background: $terminal-background; }
    /* 顶部两行紧凑靠左：内容按实际宽度紧排，剩余空间留白在行尾；
       弹性占位把版本号/索引状态推到整行尾部，行首与字段间用 │ 分隔。 */
    #topbar { height: 1; padding: 0 1; background: $terminal-surface; align: left middle; }
    #context-summary {
        width: auto;
        min-width: 0;
        max-width: 1fr;
        color: $terminal-text-muted;
        content-align: left middle;
        text-overflow: ellipsis;
        text-wrap: nowrap;
    }
    #status-summary {
        width: auto;
        min-width: 0;
        max-width: 100%;
        color: $terminal-text-muted;
        content-align: left middle;
        text-overflow: ellipsis;
        text-wrap: nowrap;
    }
    /* 第二行左侧展示 Token 明细，右侧在后台建索引时显示进度。 */
    #telemetry-row {
        height: 2;
        padding: 0 1;
        background: $terminal-panel;
        border-bottom: solid $terminal-border;
    }
    #token-telemetry {
        width: auto;
        min-width: 0;
        max-width: 100%;
        height: 1;
        padding: 0;
        color: $terminal-text-muted;
        content-align: left middle;
        text-overflow: ellipsis;
    }
    #index-status {
        width: auto;
        min-width: 0;
        max-width: 100%;
        height: 1;
        color: $terminal-text-muted;
        content-align: left middle;
        text-overflow: ellipsis;
    }
    .runtime-status-message { color: $terminal-text-muted; text-style: bold; }
    .runtime-status-message.warning { color: $terminal-red; }
    #conversation {
        height: 1fr;
        padding: 0 1;
        background: $terminal-background;
        scrollbar-color: $terminal-scrollbar;
        scrollbar-color-hover: $terminal-green;
        scrollbar-background: $terminal-background;
    }
    .message { margin: 0 0 1 0; padding: 0 1; background: transparent; border: none; }
    #conversation > .message:last-child { margin-bottom: 0; }
    .user-message { color: $terminal-text; background: $terminal-user-background; }
    .assistant-message { color: $terminal-text; }
    .status-message { color: $terminal-text-muted; }
    .subagent-tree-message { color: $terminal-text; padding-left: 2; }
    .tool-message { color: $terminal-amber; padding-left: 2; }
    .tool-message:hover { color: $terminal-amber; background: $terminal-amber-soft; }
    .tool-message:focus { color: $terminal-text; background: $terminal-amber-soft; }
    .replace-text-message,
    .replace-text-message:hover,
    .replace-text-message:focus { background: $terminal-replace-text-background; }
    .error-message { color: $terminal-red; }
    .reasoning-message { color: $terminal-text; padding-left: 2; background: $terminal-reasoning-background; }
    .reasoning-message:hover { color: $terminal-text; background: $terminal-reasoning-hover-background; }
    .reasoning-message:focus { color: $terminal-text; background: $terminal-reasoning-focus-background; border-left: thick $terminal-blue; text-style: bold; }
    .tool-message.shell-tool-message { max-height: 10; overflow-y: hidden; }
    #composer-wrap { height: 2; min-height: 2; background: $terminal-surface; border-top: solid $terminal-border-strong; padding: 0 1; }
    #command-menu {
        display: none;
        height: auto;
        max-height: 8;
        padding: 0 1;
        background: $terminal-surface;
        color: $terminal-text-secondary;
        text-wrap: nowrap;
        text-overflow: ellipsis;
        border-left: solid $terminal-blue;
    }
    #composer {
        height: 1;
        border: none;
        padding: 0 1;
        background: $terminal-surface;
        color: $terminal-text;
        overflow-x: hidden;
    }
    #composer .text-area--cursor-line { background: $terminal-panel; }
    #composer .text-area--cursor {
        color: $input-cursor-foreground;
        background: $input-cursor-background;
        text-style: $input-cursor-text-style;
    }
    #composer:focus { border-left: solid $terminal-green; background: $terminal-panel; }
    #composer:focus .text-area--cursor-line { background: $terminal-panel; }
    """)

    BINDINGS = [
        ("escape", "cancel_or_focus", "取消 / 输入框"),
        ("ctrl+c", "copy_or_clear_composer", "复制 / 清空输入"),
        ("ctrl+l", "clear_conversation", "清空视图"),
        Binding("pageup", "scroll_conversation_page_up", "上翻消息", priority=True),
        Binding("pagedown", "scroll_conversation_page_down", "下翻消息", priority=True),
    ]

    STREAM_RENDER_INTERVAL_SECONDS = 0.05
    # 生成速率统计窗口：只统计最近窗口内的增量，平滑瞬时抖动。
    TOKEN_RATE_WINDOW_SECONDS = 2.0
    # 顶部 tok/s 遥测的刷新间隔（生成期间才触发刷新）。
    TOKEN_RATE_REFRESH_INTERVAL_SECONDS = 0.5
    MONITOR_POLL_INTERVAL_SECONDS = 0.5
    STATUS_SPINNER_INTERVAL_SECONDS = 0.16
    STATUS_SPINNER_FRAMES = (
        "⠋",
        "⠙",
        "⠹",
        "⠸",
        "⠼",
        "⠴",
        "⠦",
        "⠧",
        "⠇",
        "⠏",
    )
    INTERACTION_WATCHDOG_INTERVAL_SECONDS = 0.5
    STALE_INTERACTION_TICKS = 6
    MAX_TOOL_OUTPUT_CHARS = 3_500
    COMMAND_MENU_VISIBLE_OPTIONS = 8
    COMPOSER_MIN_ROWS = 1
    COMPOSER_MAX_ROWS = 5
    COMPOSER_BORDER_ROWS = 1

    def __init__(self, agent: LocalToolAgent, startup: FullscreenStartup) -> None:
        super().__init__(
            driver_class=(
                OmniCrawlWindowsDriver if sys.platform == "win32" else None
            )
        )
        self.register_theme(TERMINAL_THEME)
        self.theme = THEME_NAME
        self.agent = agent
        self.startup = startup
        self.is_generating = False
        self.conversation_text = ""
        self._pending_inputs: deque[str] = deque()
        self._cancel_requested = threading.Event()
        # Agent 回合协议和取消令牌由非 Textual 控制器持有；本应用仅适配其
        # 回调回到主线程并保留 UI/审批状态。
        self._turn_controller = AgentTurnController(agent, self._cancel_requested)
        # 用 lambda 延迟解析模块级委托函数：重构后的 Dispatcher 不依赖
        # Textual，但既有测试和扩展仍可在 App 创建后 patch 本模块的命令入口。
        self._command_dispatcher = CommandDispatcher(
            agent,
            format_skills=lambda command_agent: format_skills_list(command_agent),
            format_mcp=lambda command_agent: format_mcp_status(command_agent),
            format_plugins=lambda command_agent: format_plugins_status(command_agent),
            format_memory_clean=lambda command_agent: format_memory_clean_result(command_agent),
            handle_session=lambda command_agent, command: handle_session_command(
                command_agent,
                command,
            ),
            handle_subagent_task=lambda command_agent, command: handle_subagent_task_command(
                command_agent,
                command,
            ),
            handle_approval=lambda command_agent, command: handle_approval_command(
                command_agent,
                command,
            ),
            handle_reasoning=lambda command_agent, command: handle_reasoning_command(
                command_agent,
                command,
            ),
        )
        self._stream_message: AssistantMessage | None = None
        self._stream_markdown = ""
        self._stream_render_pending = False
        self._stream_start_text_len: int | None = None
        self._tool_messages: dict[str, ToolDisclosure] = {}
        self._subagent_trees: dict[str, SubAgentProgressTree] = {}
        self._reasoning_message: ReasoningDisclosure | None = None
        # UI 私有的 Monitor cursor、暂停状态和失败隔离均由无 Textual 的适配器
        # 持有；本应用只安排定时刷新并渲染它返回的结构化事件批次。
        self._monitor_state = MonitorStateAdapter(agent)
        self._input_tokens = 0
        self._output_tokens = 0
        self._cached_input_tokens = 0
        # 实时生成速率：滑动窗口内 (时刻, 估算 token 数) 采样点。
        # 采样源是 UI 收到的流式文本增量，因此是估算值而非供应商用量。
        self._generation_samples: deque[tuple[float, float]] = deque()
        self._tokens_per_second = 0.0
        self._runtime_status_text = "完成"
        self._runtime_status_state = "complete"
        self._runtime_status_message: Static | None = None
        self._status_spinner_index = 0
        self._command_matches: list[dict[str, str]] = []
        self._command_selection = 0
        self._search_index_frame = 0
        self._interaction_watchdog_signature: tuple[object, ...] | None = None
        self._interaction_watchdog_stable_ticks = 0
        self._paste_sequence = 0
        self._compact_pastes: dict[str, str] = {}

    def compose(self) -> ComposeResult:
        with Vertical(id="shell"):
            with Horizontal(id="topbar"):
                yield Static(self._context_summary_text(), id="context-summary")
                yield Static(self._search_index_status_text(), id="index-status")
            with Horizontal(id="telemetry-row"):
                yield Static(self._token_telemetry_text(), id="token-telemetry")
                yield Static(self._status_summary_text(), id="status-summary")
            yield VerticalScroll(id="conversation", can_focus=False)
            with Vertical(id="composer-wrap"):
                yield Static("", id="command-menu")
                yield Composer(
                    submit_handler=self._submit_composer_text,
                    command_key_handler=self._handle_composer_command_key,
                    copy_or_clear_handler=self.action_copy_or_clear_composer,
                    paste_handler=self._compact_paste_if_needed,
                    placeholder="› 输入消息或 / 命令",
                    id="composer",
                    soft_wrap=True,
                    show_line_numbers=False,
                    highlight_cursor_line=False,
                )

    def action_scroll_conversation_up(self) -> None:
        """在固定输入框获得焦点时向上滚动一行消息。"""

        self.query_one("#conversation", VerticalScroll).scroll_up()

    def action_scroll_conversation_down(self) -> None:
        """在固定输入框获得焦点时向下滚动一行消息。"""

        self.query_one("#conversation", VerticalScroll).scroll_down()

    def action_scroll_conversation_page_up(self) -> None:
        """在固定输入框获得焦点时向上翻动消息区。"""

        self.query_one("#conversation", VerticalScroll).scroll_page_up()

    def action_scroll_conversation_page_down(self) -> None:
        """在固定输入框获得焦点时向下翻动消息区。"""

        self.query_one("#conversation", VerticalScroll).scroll_page_down()

    def on_mount(self) -> None:
        self.agent.set_confirm_handler(self._confirm_tool)
        self.query_one("#composer", TextArea).focus()
        self._resize_composer_to_text()
        self.set_interval(self.STATUS_SPINNER_INTERVAL_SECONDS, self._tick_status_indicator)
        self.set_interval(
            self.STATUS_SPINNER_INTERVAL_SECONDS,
            self._tick_search_index_status,
        )
        self.set_interval(
            self.TOKEN_RATE_REFRESH_INTERVAL_SECONDS,
            self._refresh_token_rate,
        )
        self.set_interval(
            self.INTERACTION_WATCHDOG_INTERVAL_SECONDS,
            self._recover_stale_mouse_interaction,
        )
        if self._monitor_state.can_schedule_refresh:
            self.set_interval(self.MONITOR_POLL_INTERVAL_SECONDS, self._refresh_monitor_events)

        if callable(getattr(self.agent, "preload_mcp_tools", None)):
            # MCP 预加载属于内部初始化：继续锁定输入，但不显示瞬时等待消息。
            self.is_generating = True
            self._preload_mcp_tools()
        else:
            self._set_runtime_status("完成", "complete")

    def on_app_blur(self, _event: events.AppBlur) -> None:
        """窗口失焦时终止可能丢失 MouseUp 的交互。"""

        self._reset_mouse_interaction_state()

    def on_app_focus(self, _event: events.AppFocus) -> None:
        """窗口重新聚焦时恢复输入焦点和终端鼠标报告。"""

        self._reset_mouse_interaction_state(
            focus_composer=True,
            rearm_terminal_protocols=True,
        )

    async def on_event(self, event: events.Event) -> None:
        """在 Textual 标准处理之前补齐滚动条鼠标交互。

        Textual 8.2.7 的 ``ScrollBar`` 只对渲染在 thumb 上的 MouseDown 启动
        拖动（meta ``@mouse.down: grab``）；轨道上按下只触发一次性跳转，
        且无法“按住滑动”。这里让滚动条任意位置按住都能拖动：thumb 上原位
        抓取，轨道上先跳到点击位置对应的比例再抓取。

        拖动中的 MouseMove 由本方法直接换算并滚动容器：Textual 的
        ``ScrollBar._on_mouse_move`` 只把 ``ScrollTo`` 消息发给滚动条自身，
        而滚动条 ``_allow_scroll`` 恒为 False，消息实际不会生效，原生
        拖动因此不可靠。
        """

        captured_scrollbar = self.mouse_captured
        if (
            isinstance(captured_scrollbar, ScrollBar)
            and isinstance(event, (events.MouseMove, events.MouseUp))
            and event.button != 1
        ):
            # Textual 的滚动条鼠标处理不区分按键；非左键事件不能推进或结束
            # 已存在的左键拖动，也不能触发渲染元数据中的 grab 动作。
            return
        if isinstance(event, events.MouseMove) and isinstance(
            captured_scrollbar, ScrollBar
        ):
            self._drag_scrollbar_by_mouse(captured_scrollbar, event)
        elif isinstance(event, events.MouseDown) and not event.is_forwarded:
            try:
                widget, _region = self.get_widget_at(event.x, event.y)
            except Exception:  # noqa: BLE001
                widget = None
            if isinstance(widget, ScrollBar):
                if event.button != 1:
                    # 必须在进入 Textual 默认分发前返回；ScrollBarRender 的
                    # @mouse.down 元数据本身也不会校验鼠标按键。
                    return
                if not widget.grabbed:
                    self._begin_scrollbar_drag(widget, event)
        await super().on_event(event)

    def _drag_scrollbar_by_mouse(
        self,
        scrollbar: ScrollBar,
        event: events.MouseMove,
    ) -> None:
        """拖动中按鼠标位移换算目标位置并滚动容器（立即、无动画）。"""

        if not scrollbar.grabbed:
            return
        parent = scrollbar.parent
        virtual_size = scrollbar.window_virtual_size
        window_size = scrollbar.window_size
        if parent is None or not window_size or virtual_size <= window_size:
            return
        ratio = virtual_size / window_size
        if scrollbar.vertical:
            target = scrollbar.grabbed_position + (
                (event.screen_y - scrollbar.grabbed.y) * ratio
            )
            maximum = float(parent.max_scroll_y)
            parent.scroll_to(y=target, animate=False)
        else:
            target = scrollbar.grabbed_position + (
                (event.screen_x - scrollbar.grabbed.x) * ratio
            )
            maximum = float(parent.max_scroll_x)
            parent.scroll_to(x=target, animate=False)
        target = min(max(0.0, target), maximum)
        # 同步 thumb 位置：滚动容器的异步刷新会回填 position，这里先对齐
        # 保证 grab 起点与下次位移计算连续。
        scrollbar.position = target

    @staticmethod
    def _scrollbar_thumb_bounds(
        scrollbar: ScrollBar,
        bar_size: int,
    ) -> tuple[float, float]:
        """计算滚动条 thumb 在条身内的区间，与 ScrollBarRender 公式一致。"""

        virtual_size = scrollbar.window_virtual_size
        window_size = scrollbar.window_size
        if bar_size <= 0 or virtual_size <= window_size:
            return (0.0, float(bar_size))
        thumb_size = max(1.0, window_size * bar_size / virtual_size)
        position_ratio = scrollbar.position / (virtual_size - window_size)
        start = (bar_size - thumb_size) * position_ratio
        return (start, start + thumb_size)

    def _begin_scrollbar_drag(
        self,
        scrollbar: ScrollBar,
        event: events.MouseDown,
    ) -> None:
        """滚动条按下：thumb 上原位抓取；轨道上先按比例跳转再抓取。"""

        region = scrollbar.region
        if scrollbar.vertical:
            bar_size = region.height
            position_in_bar = event.screen_y - region.y
        else:
            bar_size = region.width
            position_in_bar = event.screen_x - region.x
        parent = scrollbar.parent
        thumb_start, thumb_end = self._scrollbar_thumb_bounds(scrollbar, bar_size)
        on_thumb = thumb_start <= position_in_bar <= thumb_end
        if not on_thumb and parent is not None and bar_size > 0:
            # 轨道点击：先跳到点击位置对应的滚动比例，再进入拖动状态，
            # 用户不移动即停在跳转点，继续移动则从该点拖动。
            ratio = min(1.0, max(0.0, position_in_bar / bar_size))
            if scrollbar.vertical:
                target = ratio * parent.max_scroll_y
                parent.scroll_to(y=target, animate=False)
            else:
                target = ratio * parent.max_scroll_x
                parent.scroll_to(x=target, animate=False)
            # scroll_to 的生效与 scrollbar.position 的同步是异步的；先手动
            # 对齐 thumb 位置，保证抓取后的拖动起点正确。
            scrollbar.position = target
        # 本方法在 App.on_event 更新 mouse_position 之前执行，capture_mouse
        # 会把它写进 MouseCapture.mouse_position（即 grabbed 起点）。不同步
        # 的话拖动偏移全错；且若起点恰好是 Offset(0,0)，ScrollBar._on_mouse_up
        # 的 ``if self.grabbed`` 判定为 False，MouseUp 后永不释放、交互卡死。
        self.mouse_position = Offset(event.screen_x, event.screen_y)
        scrollbar.action_grab()

    def _recover_stale_mouse_interaction(self) -> None:
        """释放长时间没有变化的鼠标按下状态，避免输入事件永久失效。"""

        try:
            driver = self._driver
            if not driver.is_headless:
                restore_input_mode = (
                    _restore_windows_raw_input_mode_if_needed
                    if isinstance(driver, OmniCrawlWindowsDriver)
                    else _restore_windows_vt_input_mode_if_needed
                )
                if restore_input_mode():
                    # 控制台模式被系统重置后不会再产生 Textual 可识别的 AppFocus，
                    # 因此必须由周期看门狗主动恢复，且设置页等模态界面也不能跳过。
                    self._reset_mouse_interaction_state(rearm_terminal_protocols=True)

            if len(self.screen_stack) > 1:
                self._interaction_watchdog_signature = None
                self._interaction_watchdog_stable_ticks = 0
                return

            screen = self.screen
            captured = self.mouse_captured
            down_buttons = getattr(driver, "_down_buttons", [])
            mouse_down_offset = getattr(screen, "_mouse_down_offset", None)
            selecting = bool(getattr(screen, "_selecting", False))
            if captured is None and not selecting and mouse_down_offset is None and not down_buttons:
                self._interaction_watchdog_signature = None
                self._interaction_watchdog_stable_ticks = 0
                return

            select_state = getattr(screen, "_select_state", None)
            last_move = getattr(driver, "_last_move_event", None)
            last_move_signature = None
            if last_move is not None:
                last_move_signature = (
                    last_move.screen_x,
                    last_move.screen_y,
                    last_move.button,
                )
            conversations = self.query("#conversation")
            if not conversations:
                self._interaction_watchdog_signature = None
                self._interaction_watchdog_stable_ticks = 0
                return
            signature = (
                captured,
                mouse_down_offset,
                getattr(select_state, "end", None),
                getattr(captured, "selection", None),
                getattr(captured, "position", None),
                conversations.first(VerticalScroll).scroll_y,
                last_move_signature,
                tuple(down_buttons),
            )
            if signature != self._interaction_watchdog_signature:
                self._interaction_watchdog_signature = signature
                self._interaction_watchdog_stable_ticks = 0
                return

            self._interaction_watchdog_stable_ticks += 1
            if self._interaction_watchdog_stable_ticks >= self.STALE_INTERACTION_TICKS:
                self._reset_mouse_interaction_state(
                    focus_composer=True,
                    rearm_terminal_protocols=True,
                )
        except Exception as exc:  # noqa: BLE001
            # 这是周期自愈边界，而不是业务主流程。锁屏恢复、窗口关闭或 Driver
            # 正在停止时，私有终端状态可能短暂不可读；异常若逃逸，Textual 会
            # 将定时器错误升级为致命退出，正是长时间闲置后窗口自行结束的根因。
            self._interaction_watchdog_signature = None
            self._interaction_watchdog_stable_ticks = 0
            try:
                self.log.debug("终端输入自愈暂时跳过", exc)
            except Exception:  # noqa: BLE001
                pass

    def _reset_mouse_interaction_state(
        self,
        *,
        focus_composer: bool = False,
        rearm_terminal_protocols: bool = False,
    ) -> None:
        """统一清除 Textual 组件、Screen 和 Driver 的未完成鼠标状态。"""

        self.capture_mouse(None)
        screen = self.screen
        screen.clear_selection()
        # Textual 8.2.7 的公开 clear_selection() 不会清理这两个按下状态；
        # MouseUp 丢失后必须同步复位，否则选区自动滚动仍会继续拦截鼠标。
        screen._mouse_down_offset = None
        screen._selecting = False

        driver = self._driver
        down_buttons = getattr(driver, "_down_buttons", None)
        if isinstance(down_buttons, list):
            down_buttons.clear()
        if rearm_terminal_protocols:
            if isinstance(driver, OmniCrawlWindowsDriver):
                if not driver.is_headless:
                    _restore_windows_raw_input_mode_if_needed()
                write = getattr(driver, "write", None)
                if callable(write):
                    write(OmniCrawlWindowsDriver.RAW_INPUT_PROTOCOL_RESET)
            else:
                if not driver.is_headless:
                    _restore_windows_vt_input_mode_if_needed()
                enable_mouse_support = getattr(driver, "_enable_mouse_support", None)
                if callable(enable_mouse_support):
                    enable_mouse_support()
                write = getattr(driver, "write", None)
                if callable(write):
                    write("\033[?1004h")
                    write(OmniCrawlWindowsDriver.KEYBOARD_PROTOCOL)
                enable_bracketed_paste = getattr(driver, "_enable_bracketed_paste", None)
                if callable(enable_bracketed_paste):
                    enable_bracketed_paste()
            flush = getattr(driver, "flush", None)
            if callable(flush):
                flush()

        self._interaction_watchdog_signature = None
        self._interaction_watchdog_stable_ticks = 0
        if focus_composer and len(self.screen_stack) == 1:
            self.query_one("#composer", TextArea).focus()

    def _tick_search_index_status(self) -> None:
        """加载索引时推进与状态指示器同款的旋转动画帧，并刷新 HUD。"""

        self._search_index_frame = (self._search_index_frame + 1) % len(
            SEARCH_INDEX_SPINNER_FRAMES
        )
        self._render_search_index_status()

    def _search_index_status_text(self) -> Text:
        getter = getattr(self.agent, "search_index_status", None)
        status = getter() if callable(getter) else None
        return search_index_status_text(status, self._search_index_frame)

    def _render_search_index_status(self) -> None:
        """轮询只读状态快照，不让后台索引线程直接接触 Textual 组件。

        空闲时索引状态为空文本，隐藏整个组件；非空闲时前置 "⁕ " 分隔
        符（与第二行字段段衔接），隐藏时不会留下悬空分隔符。
        """

        widgets = self.query("#index-status")
        if widgets:
            widget = widgets.first(Static)
            rendered = self._search_index_status_text()
            if rendered:
                # 前置分隔符属于组件内容，随组件一起隐藏/显示。
                rendered = Text.assemble(
                    ("⁕ ", BORDER_MUTED),
                    rendered,
                )
            current = widget.content
            if isinstance(current, Text) and current == rendered:
                # 内容未变时仍需同步隐藏状态（首次渲染两者都为空）。
                widget.display = bool(rendered)
                return
            widget.update(rendered)
            widget.display = bool(rendered)

    @work(thread=True, exclusive=True, group="mcp-preload", exit_on_error=False)
    def _preload_mcp_tools(self) -> None:
        """主界面显示后在后台发现 MCP，避免阻塞 Textual 首屏绘制。"""

        try:
            with self._turn_controller.scope():
                self._turn_controller.preload_mcp_tools()
        except AgentError as exc:
            self.call_from_thread(self._append_message, "error", f"MCP 能力加载失败：{exc}")
        except Exception as exc:
            self.call_from_thread(self._append_message, "error", f"MCP 能力加载异常：{exc}")
        finally:
            self.call_from_thread(self._finish_turn)

    def _compact_paste_if_needed(self, text: str) -> str | None:
        pasted_text = _normalize_pasted_text(text)
        line_count = _count_paste_lines(pasted_text)
        if line_count <= _PASTE_COMPACT_LINE_THRESHOLD:
            return None
        self._paste_sequence += 1
        placeholder = f"[粘贴 #{self._paste_sequence} +{line_count} 行]"
        self._compact_pastes[placeholder] = pasted_text
        return placeholder

    def _expand_compact_paste_placeholders(self, text: str) -> str:
        if not self._compact_pastes:
            return text
        return _PASTE_PLACEHOLDER_PATTERN.sub(
            lambda match: self._compact_pastes.get(match.group(0), match.group(0)),
            text,
        )

    def _prune_compact_paste_placeholders(self, text: str) -> None:
        if not self._compact_pastes:
            return
        self._compact_pastes = {
            placeholder: pasted_text
            for placeholder, pasted_text in self._compact_pastes.items()
            if placeholder in text
        }

    def on_text_area_changed(self, event: TextArea.Changed) -> None:
        if event.text_area.id == "composer":
            self._prune_compact_paste_placeholders(event.text_area.text)
            self._refresh_command_menu(event.text_area.text)
            self._resize_composer_to_text()

    def on_resize(self, _event: events.Resize) -> None:
        """窗口变化时重算输入区软折行高度。"""

        self.call_after_refresh(self._resize_composer_to_text)

    def _resize_composer_to_text(self) -> None:
        """让输入区从一行起步，随软折行增长且不挤占整个消息区。"""

        composer = self.query_one("#composer", TextArea)
        explicit_rows = composer.text.count("\n") + 1 if composer.text else 1
        composer_rows = min(
            self.COMPOSER_MAX_ROWS,
            max(
                self.COMPOSER_MIN_ROWS,
                explicit_rows,
                composer.virtual_size.height,
            ),
        )
        composer.styles.height = composer_rows
        menu_rows = min(len(self._command_matches), self.COMMAND_MENU_VISIBLE_OPTIONS)
        self.query_one("#composer-wrap").styles.height = (
            self.COMPOSER_BORDER_ROWS + composer_rows + menu_rows
        )

    def _handle_composer_command_key(self, event: events.Key) -> bool:
        """菜单打开时消费选择键；Enter/Tab 只补全，不提交命令。"""

        if not self._command_matches:
            return False
        composer = self.query_one("#composer", TextArea)
        if event.key in {"up", "down"}:
            offset = -1 if event.key == "up" else 1
            self._command_selection = (self._command_selection + offset) % len(self._command_matches)
            self._render_command_menu()
            return True
        if event.key in {"enter", "tab"}:
            selected = self._command_matches[self._command_selection]
            target = selected["insert"]
            # 输入已是完整命令时，Enter 必须提交执行；否则会反复“补全”同一文本，
            # 导致 /settings、/new 这类无参数命令永远打不开。
            if composer.text == target or composer.text.strip() == selected["command"]:
                self._hide_command_menu()
                return event.key != "enter"
            composer.text = target
            composer.cursor_location = (0, len(composer.text))
            self._hide_command_menu()
            return True
        return False
    def _submit_composer_text(self) -> None:
        """提交编辑器内容，保留内部换行且忽略纯空白输入。"""

        composer = self.query_one("#composer", TextArea)
        text = self._expand_compact_paste_placeholders(composer.text).strip()
        if not text:
            return
        composer.clear()
        self._compact_pastes.clear()
        if self.is_generating and text == "/quit":
            self._handle_command(text)
            return
        if self.is_generating:
            self._pending_inputs.append(text)
            self._refresh_pending_queue_count()
            self._append_message("status", f"消息已排队（{len(self._pending_inputs)}）")
            return
        self._submit(text)

    def _refresh_command_menu(self, value: str) -> None:
        """根据当前斜杠前缀实时筛选统一命令源，并保留完整候选供上下键选择。"""

        query = value.strip().lower()
        if not query.startswith("/") or any(char.isspace() for char in value):
            self._hide_command_menu()
            return
        matches = [
            option
            for option in build_slash_command_options(self.agent)
            if query in option["search"].lower()
        ]
        # Python 排序稳定：仅把前缀命中提到前面，同级保留统一命令源的产品顺序。
        matches.sort(key=lambda option: not option["command"].lower().startswith(query))
        self._command_matches = matches
        self._command_selection = 0
        if not self._command_matches:
            self._hide_command_menu()
            return
        self._render_command_menu()

    def _render_command_menu(self) -> None:
        menu = self.query_one("#command-menu", Static)
        lines = Text(no_wrap=True, overflow="ellipsis")
        visible_limit = self.COMMAND_MENU_VISIBLE_OPTIONS
        visible_start = max(
            0,
            min(
                self._command_selection - visible_limit + 1,
                len(self._command_matches) - visible_limit,
            ),
        )
        visible_matches = self._command_matches[
            visible_start : visible_start + visible_limit
        ]
        for offset, option in enumerate(visible_matches):
            index = visible_start + offset
            marker = "›" if index == self._command_selection else " "
            style = f"bold {ACCENT_BLUE}" if index == self._command_selection else TEXT_SECONDARY
            lines.append(f"{marker} {option['command']}", style=style)
            description = " ".join(option.get("description", "").split())
            if description:
                lines.append(f"  · {description}", style=TEXT_MUTED)
            if offset < len(visible_matches) - 1:
                lines.append("\n")
        menu.update(lines)
        menu.display = True
        self._resize_composer_to_text()

    def _hide_command_menu(self) -> None:
        self._command_matches = []
        self._command_selection = 0
        menu = self.query_one("#command-menu", Static)
        menu.display = False
        menu.update("")
        self._resize_composer_to_text()

    def action_cancel_or_focus(self) -> None:
        if self.is_generating:
            self.cancel_pending_turn()
        else:
            self._reset_mouse_interaction_state(
                focus_composer=True,
                rearm_terminal_protocols=True,
            )

    def action_copy_or_clear_composer(self) -> None:
        composer = self.query_one("#composer", TextArea)
        if composer.selected_text:
            self.copy_to_clipboard(composer.selected_text)
            return
        selected_text = self.screen.get_selected_text()
        if selected_text:
            self.copy_to_clipboard(selected_text)
            return
        composer.clear()
        self._compact_pastes.clear()

    def action_clear_conversation(self) -> None:
        if self.is_generating:
            return
        self.query_one("#conversation", VerticalScroll).remove_children()
        self.conversation_text = ""
        self._stream_message = None
        self._stream_markdown = ""
        self._stream_render_pending = False
        self._stream_start_text_len = None
        self._tool_messages.clear()
        self._subagent_trees.clear()
        self._reasoning_message = None
        self._runtime_status_message = None
        self._append_message("status", "已清空当前视图，不影响会话历史。")

    def cancel_pending_turn(self) -> None:
        """标记当前回合已取消，使工作线程在下一个可中断点退出。"""

        self._cancel_requested.set()
        closed = self._turn_controller.cancel()
        self._set_runtime_status("已取消", "complete")
        return closed

    def _submit(self, text: str) -> None:
        if self._handle_command(text):
            return
        self.is_generating = True
        self._cancel_requested.clear()
        self._append_message("user", text)
        self._set_runtime_status("正在思考", "working")
        self._run_agent_turn(text)

    @work(thread=True, exclusive=True, group="agent-turn", exit_on_error=False)
    def _run_agent_turn(self, text: str) -> None:
        callbacks = AgentTurnCallbacks(
            on_delta=lambda delta: self.call_from_thread(self._append_delta, delta),
            on_status=lambda status: self.call_from_thread(self._handle_status, status),
            on_tool_start=lambda step, call: self.call_from_thread(
                self._handle_tool_start,
                step,
                call,
            ),
            on_tool_result=lambda call, result: self.call_from_thread(
                self._handle_tool_result,
                call,
                result,
            ),
            on_token_usage=lambda incoming, outgoing, cached: self.call_from_thread(
                self._handle_token_usage,
                incoming,
                outgoing,
                cached,
            ),
            on_protocol_wait=lambda: self.call_from_thread(
                self._handle_status,
                "正在准备工具调用",
            ),
            on_retry_status=lambda status: self.call_from_thread(self._handle_status, status),
            on_stream_rollback=lambda: self.call_from_thread(self._rollback_stream),
            on_reasoning_delta=lambda delta: self.call_from_thread(
                self._append_reasoning_delta,
                delta,
            ),
            on_subagent_event=lambda event_name, payload: self.call_from_thread(
                self._handle_subagent_event,
                event_name,
                payload,
            ),
        )
        try:
            self._turn_controller.run(text, callbacks)
        except KeyboardInterrupt:
            self.call_from_thread(self._append_message, "status", "当前任务已取消。")
        except AgentError as exc:
            self.call_from_thread(self._append_message, "error", f"Agent 请求失败：{exc}")
        except Exception as exc:
            self.call_from_thread(self._append_message, "error", f"界面任务异常：{exc}")
        finally:
            self.call_from_thread(self._finish_turn)

    @work(thread=True, exclusive=True, group="slow-command", exit_on_error=False)
    def _run_slow_command(
        self,
        command: Callable[[], str | None],
        *,
        refresh_context: bool,
        on_success: Callable[[], None] | None = None,
        on_finish: Callable[[], None] | None = None,
    ) -> None:
        """在工作线程执行可能连接网络或启动 MCP Server 的斜杠命令。"""

        try:
            with self._turn_controller.scope():
                message = command()
        except AgentError as exc:
            self.call_from_thread(self._append_message, "error", f"命令执行失败：{exc}")
        except Exception as exc:
            self.call_from_thread(self._append_message, "error", f"命令执行异常：{exc}")
        else:
            if message:
                self.call_from_thread(self._append_message, "status", message)
            if refresh_context:
                self.call_from_thread(self._refresh_context_summary)
            if on_success is not None:
                self.call_from_thread(on_success)
        finally:
            if on_finish is not None:
                self.call_from_thread(on_finish)
            self.call_from_thread(self._finish_turn)

    def _start_slow_command(
        self,
        status: str,
        command: Callable[[], str | None],
        *,
        refresh_context: bool = False,
        on_success: Callable[[], None] | None = None,
        on_finish: Callable[[], None] | None = None,
    ) -> None:
        """锁定输入并安排慢命令，避免在 Textual 主事件循环执行 I/O。"""

        self.is_generating = True
        self._cancel_requested.clear()
        self._append_message("status", status)
        self._set_runtime_status("等待", "waiting")
        self._run_slow_command(
            command,
            refresh_context=refresh_context,
            on_success=on_success,
            on_finish=on_finish,
        )

    def _raise_if_cancelled(self) -> None:
        """兼容已有调用点；实际 Agent 回合取消由控制器传入协议。"""

        self._turn_controller.raise_if_cancelled()

    def _handle_command(self, text: str) -> bool:
        """执行分派结果；Textual 生命周期始终保留在应用层。"""

        outcome = self._command_dispatcher.dispatch(text)
        if not outcome.handled:
            return False
        if outcome.exit_requested:
            self.cancel_pending_turn()
            self._pending_inputs.clear()
            self._refresh_pending_queue_count()
            self.exit()
            return True
        if outcome.open_settings:
            self._open_settings()
            return True
        if outcome.execution == "slow":
            # 工作区切换会重建 Session、MCP、Monitor 和临时目录，必须放在
            # Textual worker 中，避免文件和进程操作阻塞主事件循环。
            if outcome.workspace_switch_requested:
                # 切换请求一经接受，旧工作区的任务 ID 已不再具有语义；不应等
                # 后台 I/O 成功才清除游标，否则新工作区可能跳过首批 Monitor 事件。
                self._monitor_state.suspend_for_workspace_switch()
            self._start_slow_command(
                outcome.message or "正在执行命令",
                outcome.command,
                refresh_context=outcome.refresh_context,
                on_finish=(
                    self._monitor_state.resume_polling
                    if outcome.workspace_switch_requested
                    else None
                ),
            )
            return True
        if outcome.message:
            self._append_message("status", outcome.message)
        if outcome.refresh_context:
            self._refresh_context_summary()
        return True

    def _confirm_tool(self, tool_name: str, arguments: dict[str, Any]) -> bool:
        """在主线程展示确认框，并允许工作线程在用户取消时立即退出等待。"""

        self.call_from_thread(self._set_runtime_status, "等待", "waiting")
        prompt = format_tool_confirmation(tool_name, arguments)
        event = threading.Event()
        result = {"approved": False}
        screen: ConfirmationScreen | None = None

        def receive(value: bool | None) -> None:
            result["approved"] = bool(value)
            event.set()

        def show_confirmation() -> ConfirmationScreen:
            nonlocal screen
            screen = ConfirmationScreen(prompt)
            self.push_screen(screen, receive)
            return screen

        self.call_from_thread(show_confirmation)
        while not event.wait(0.05):
            if self._cancel_requested.is_set():
                def dismiss_confirmation() -> None:
                    if screen is not None and screen.is_active:
                        screen.dismiss(False)

                self.call_from_thread(dismiss_confirmation)
                return False
        return result["approved"]

    @staticmethod
    def _is_conversation_at_end(conversation: VerticalScroll) -> bool:
        """判断用户是否仍在消息流底部，避免后台更新抢回滚动位置。"""

        return conversation.is_vertical_scroll_end

    @staticmethod
    def _scroll_conversation_if_following(
        conversation: VerticalScroll,
        follow_latest: bool,
        *,
        defer_until_refresh: bool = False,
    ) -> None:
        """仅在用户原本位于底部时跟随新增内容。"""

        if follow_latest:
            if conversation.max_scroll_y > 0:
                # Textual 的锚定语义会在内容重新布局后持续跟随底部，并在用户
                # 手动滚动时自动释放；这比跨刷新排队 scroll_end 更能避免流式
                # 更新竞态。
                conversation.anchor()
                if defer_until_refresh:
                    conversation.call_after_refresh(conversation.scroll_end, animate=False)
            else:
                # 内容不足一屏时，Textual 8.2.7 的 anchor 会让 compositor 在
                # 布局时把 scroll_y 设为「内容高度 - 容器高度」的负值（该路径
                # 不经过 validate/clamp），消息被推到视口底部、顶部出现大片
                # 空白。此时无需滚动，取消锚定并保持顶部对齐；布局完成后若
                # 内容已超出一屏（跨屏边界），再恢复底部跟随。
                conversation.anchor(False)
                conversation.scroll_y = 0
                conversation.call_after_refresh(
                    lambda: (
                        conversation.anchor()
                        if conversation.max_scroll_y > 0
                        else None
                    )
                )

    def _handle_status(self, message: str) -> None:
        if message:
            self._set_runtime_status("等待", "waiting")

    def _handle_subagent_event(self, event_name: str, payload: dict[str, Any]) -> None:
        """按批次原地更新子任务树，不暴露 prompt、结果或原始异常。"""

        status_by_event = {
            "subagent.task.queued": "queued",
            "subagent.task.started": "running",
            "subagent.task.running": "running",
            "subagent.task.waiting_approval": "waiting_approval",
            "subagent.task.completed": "completed",
            "subagent.task.failed": "failed",
            "subagent.task.cancelled": "cancelled",
            "subagent.task.approval_cancelled": "cancelled",
        }
        status = status_by_event.get(event_name)
        if status is None:
            return

        task_id = str(payload.get("task_id") or "task")
        batch_id = str(payload.get("batch_id") or f"batch-{task_id}")
        conversation = self.query_one("#conversation", VerticalScroll)
        follow_latest = self._is_conversation_at_end(conversation)
        tree = self._subagent_trees.get(batch_id)
        if tree is None or tree.parent is None:
            tree = SubAgentProgressTree(batch_id)
            self._subagent_trees[batch_id] = tree
            conversation.mount(tree)
        tree.update_task(
            task_id=task_id,
            agent_type=str(payload.get("agent_type") or "subagent"),
            description=str(payload.get("description") or task_id),
            status=status,
        )
        # 运行状态始终保持为消息流末项；树新增或增高后需恢复这一顺序。
        if self._runtime_status_message is not None:
            self._render_status_indicator(follow_latest=follow_latest)
        self._scroll_conversation_if_following(
            conversation,
            follow_latest,
            defer_until_refresh=True,
        )

    def _handle_tool_start(self, step: int, tool_call: Any) -> None:
        del step  # Agent 仍按步骤回调，但极简 HUD 不展示内部步骤编号。
        self._render_stream_markdown()
        # 工具调用是模型 pass 的明确边界。必须封口此前的回复组件，否则工具
        # 返回后的最终回答会继续写入旧组件，在视觉上倒插到工具记录之前。
        if self._reasoning_message is not None:
            # 先补齐未完成行，再封口思考组件。
            self._reasoning_message.flush_tail()
        self._stream_message = None
        self._stream_markdown = ""
        self._stream_start_text_len = None
        self._reasoning_message = None
        conversation = self.query_one("#conversation", VerticalScroll)
        follow_latest = self._is_conversation_at_end(conversation)
        tool_message = ToolDisclosure(
            str(tool_call.name),
            self._public_tool_arguments(tool_call),
            time.perf_counter(),
        )
        self._tool_messages[self._tool_call_key(tool_call)] = tool_message
        conversation.mount(tool_message)
        self.conversation_text += f"{tool_call.name}\n"
        self._set_runtime_status(
            "正在调用",
            "working",
            follow_latest=follow_latest,
        )
        self._scroll_conversation_if_following(conversation, follow_latest)

    def _handle_tool_result(self, tool_call: Any, result: Any) -> None:
        conversation = self.query_one("#conversation", VerticalScroll)
        follow_latest = self._is_conversation_at_end(conversation)
        output = str(getattr(result, "full_output", "") or result.output or "无输出")
        key = self._tool_call_key(tool_call)
        tool_message = self._tool_messages.pop(key, None)
        if tool_message is None:
            # 兼容缺失 start 事件的协议实现，同时仍保持默认折叠交互。
            tool_message = ToolDisclosure(
                str(tool_call.name),
                self._public_tool_arguments(tool_call),
                time.perf_counter(),
            )
            self.query_one("#conversation", VerticalScroll).mount(tool_message)
        tool_message.finish(
            ok=bool(result.ok),
            output=output,
            finished_at=time.perf_counter(),
        )
        self.conversation_text += f"结果  {tool_message.status}\n{output}\n"
        self._scroll_conversation_if_following(conversation, follow_latest)
        self._set_runtime_status("正在思考", "working")

    @staticmethod
    def _public_tool_arguments(tool_call: Any) -> Any:
        """隐藏 SubAgent 完整 prompt，其余工具保持既有参数展示。"""

        arguments = getattr(tool_call, "arguments", None)
        if not isinstance(arguments, dict):
            return arguments
        return public_tool_arguments(str(getattr(tool_call, "name", "")), arguments)

    @staticmethod
    def _tool_call_key(tool_call: Any) -> str:
        """优先以协议 ID 关联并发工具记录；缺失 ID 时退化为对象身份。"""

        tool_call_id = str(getattr(tool_call, "id", "") or "")
        return tool_call_id or f"object:{id(tool_call)}"

    def _handle_token_usage(self, incoming: int, outgoing: int, cached: int) -> None:
        """刷新最近一次模型请求的 Token 遥测与上下文占用进度。"""

        self._input_tokens = max(0, int(incoming))
        self._output_tokens = max(0, int(outgoing))
        self._cached_input_tokens = max(0, int(cached))
        self.query_one("#token-telemetry", Static).update(self._token_telemetry_text())

    @staticmethod
    def _estimate_generation_tokens(text: str) -> float:
        """把流式文本增量粗略估算为 token 数。

        CJK 字符按 1 token、其他字符按 4 字符 1 token 估算。供应商不提供
        流中逐片 token 计数，该估算只用于实时速率展示，不做精确计量。
        """

        cjk = sum(1 for ch in text if ord(ch) > 0x2E7F)
        return cjk + (len(text) - cjk) / 4.0

    def _prune_generation_samples(self) -> None:
        """丢弃窗口之外的采样点，保持滑动窗口有界。"""

        cutoff = time.monotonic() - self.TOKEN_RATE_WINDOW_SECONDS
        while self._generation_samples and self._generation_samples[0][0] < cutoff:
            self._generation_samples.popleft()

    def _record_generation_delta(self, delta: str) -> None:
        """记录思考/正文流增量的时间与估算 token，供 tok/s 实时计算。"""

        if not delta:
            return
        self._generation_samples.append(
            (time.monotonic(), self._estimate_generation_tokens(delta))
        )
        self._prune_generation_samples()

    def _update_token_telemetry(self) -> None:
        """刷新顶部遥测行；widget 不在活动查询树中时静默跳过。

        定时器回调可能在模态屏打开或应用关闭过程中触发，此时主工作台
        组件已不在活动 Screen 的 DOM 中，直接 query_one 会抛 NoMatches。
        """

        try:
            self.query_one("#token-telemetry", Static).update(
                self._token_telemetry_text()
            )
        except Exception:
            pass

    def _refresh_token_rate(self) -> None:
        """重算最近窗口内的生成速率并刷新顶部遥测；无采样时跳过。"""

        if len(self.screen_stack) > 1:
            # 模态审批屏成为活动 Screen 后主工作台不在查询树中，此时跳过。
            return
        if not self._generation_samples:
            if self._tokens_per_second:
                self._tokens_per_second = 0.0
                self._update_token_telemetry()
            return
        self._prune_generation_samples()
        if not self._generation_samples:
            # 窗口过期：速率归零并刷新，避免残留旧速率。
            self._tokens_per_second = 0.0
            self._update_token_telemetry()
            return
        now = time.monotonic()
        span = min(
            now - self._generation_samples[0][0],
            self.TOKEN_RATE_WINDOW_SECONDS,
        )
        # 防御刚收到大量增量立即刷新导致的瞬时尖峰。
        span = max(span, 0.25)
        total = sum(tokens for _, tokens in self._generation_samples)
        rate = total / span if span > 0 else 0.0
        if rate != self._tokens_per_second:
            self._tokens_per_second = rate
            self._update_token_telemetry()

    def _reset_token_rate(self) -> None:
        """回合结束或流回滚时归零速率并刷新遥测，避免残留旧速率。"""

        self._generation_samples.clear()
        self._tokens_per_second = 0.0
        self._update_token_telemetry()

    def _refresh_monitor_events(self) -> None:
        """渲染适配器返回的后台任务增量日志，不影响模型回合。"""

        for batch in self._monitor_state.refresh():
            self._append_message("tool", format_monitor_display_batch(batch))

    def _append_reasoning_delta(self, delta: str) -> None:
        if not delta:
            return
        conversation = self.query_one("#conversation", VerticalScroll)
        follow_latest = self._is_conversation_at_end(conversation)
        if self._reasoning_message is None:
            self._reasoning_message = ReasoningDisclosure()
            conversation.mount(self._reasoning_message)
        self._reasoning_message.append_delta(delta)
        self._record_generation_delta(delta)
        self._set_runtime_status("正在思考", "working")
        self._scroll_conversation_if_following(conversation, follow_latest)

    def _append_delta(self, delta: str) -> None:
        if not delta:
            return
        if self._reasoning_message is not None:
            # 思考阶段结束，同步补齐未完成行，保证推理内容展示完整。
            self._reasoning_message.flush_tail()
        self._reasoning_message = None
        conversation = self.query_one("#conversation", VerticalScroll)
        follow_latest = self._is_conversation_at_end(conversation)
        if self._stream_message is None:
            # 记录本轮流式输出起点，供模型流中断自动重试前回滚已显示内容。
            self._stream_start_text_len = len(self.conversation_text)
            self._stream_message = AssistantMessage(self._stream_markdown)
            self._stream_markdown = "◇ "
            conversation.mount(self._stream_message)
        self._stream_markdown += delta
        self.conversation_text += f"{delta}\n"
        self._record_generation_delta(delta)
        if not self._stream_render_pending:
            self._stream_render_pending = True
            self.set_timer(self.STREAM_RENDER_INTERVAL_SECONDS, self._render_stream_markdown)
        self._set_runtime_status(
            "正在回复",
            "working",
            follow_latest=follow_latest,
        )
        self._scroll_conversation_if_following(conversation, follow_latest)

    def _render_stream_markdown(self) -> None:
        """合并短时间内的流式分片，避免逐片重解析完整 Markdown。"""

        self._stream_render_pending = False
        if self._stream_message is not None:
            conversations = self.query("#conversation")
            if not conversations or self._stream_message.parent is None:
                self._stream_render_pending = False
                return
            conversation = conversations.first(VerticalScroll)
            follow_latest = self._is_conversation_at_end(conversation)
            self._stream_message.update(self._stream_markdown)
            self._scroll_conversation_if_following(
                conversation,
                follow_latest,
                defer_until_refresh=True,
            )

    def _rollback_stream(self) -> None:
        """撤销本轮流式输出已展示的内容，供模型流中断自动重试前调用。

        重试成功后模型会重新生成完整回复；若不清除半截内容，新旧文本会
        在同一消息组件内拼接错乱。推理组件同样移除，因其内容也来自旧请求。
        """

        if self._stream_message is not None:
            try:
                if self._stream_message.parent is not None:
                    self._stream_message.remove()
            except Exception:
                pass
            self._stream_message = None
        self._stream_markdown = ""
        self._stream_render_pending = False
        if self._stream_start_text_len is not None:
            self.conversation_text = self.conversation_text[: self._stream_start_text_len]
            self._stream_start_text_len = None
        if self._reasoning_message is not None:
            try:
                if self._reasoning_message.parent is not None:
                    self._reasoning_message.remove()
            except Exception:
                pass
            self._reasoning_message = None
        self._reset_token_rate()

    def _append_message(
        self,
        kind: str,
        text: str,
        *,
        merge_with_previous: bool = False,
        track_tool: bool = False,
    ) -> None:
        conversation = self.query_one("#conversation", VerticalScroll)
        follow_latest = self._is_conversation_at_end(conversation)
        follow_with_runtime_status = self._runtime_status_message is not None
        if merge_with_previous and self._stream_message is not None:
            self._stream_markdown += text
            self._stream_message.update(self._stream_markdown)
        else:
            prefixes = {
                "user": "$ ",
                "assistant": "◇ ",
                "status": "· ",
                "tool": "⌁ ",
                "error": "△ ",
            }
            prefixed_text = f"{prefixes.get(kind, '· ')}{text}"
            if kind == "assistant":
                widget = AssistantMessage(prefixed_text)
            else:
                widget = Static(Text(prefixed_text), classes=f"message {kind}-message")
            if merge_with_previous:
                self._stream_message = widget
                self._stream_markdown = text
            else:
                self._stream_message = None
                self._stream_markdown = ""
                self._stream_start_text_len = None
            conversation.mount(widget)
            if track_tool:
                self._tool_messages[f"legacy:{id(widget)}"] = widget
        self.conversation_text += f"{text}\n"
        if follow_with_runtime_status:
            self._render_status_indicator(follow_latest=follow_latest)
        self._scroll_conversation_if_following(conversation, follow_latest)

    def _finish_turn(self) -> None:
        self._render_stream_markdown()
        was_cancelled = self._cancel_requested.is_set()
        self.is_generating = False
        if self._reasoning_message is not None:
            # 推理后直接结束回合（无回复/无工具）时，同样补齐未完成行。
            self._reasoning_message.flush_tail()
        self._reasoning_message = None
        self._reset_token_rate()
        # 取消回合的终态不能被 finally 中的通用完成逻辑覆盖为“完成”：
        # 状态文本保持与 cancel_pending_turn 展示的“已取消”一致。
        self._set_runtime_status("已取消" if was_cancelled else "完成", "complete")
        self.query_one("#composer", TextArea).focus()
        self._drain_pending_inputs()

    def _drain_pending_inputs(self) -> None:
        """在当前回合完成后按 FIFO 处理提交内容，避免 worker 重叠。"""

        while (
            self._pending_inputs
            and not self.is_generating
            and len(self.screen_stack) == 1
        ):
            text = self._pending_inputs.popleft()
            self._refresh_pending_queue_count()
            self._submit(text)

    def _set_runtime_status(
        self,
        text: str,
        state: str,
        *,
        follow_latest: bool | None = None,
    ) -> None:
        status_changed = (
            text != self._runtime_status_text or state != self._runtime_status_state
        )
        self._runtime_status_text = text
        self._runtime_status_state = state
        # 流式思考和回复分片会重复上报同一状态；仅在阶段切换时重置，
        # 否则高频事件会把动画持续钉在首帧。
        if status_changed:
            self._status_spinner_index = 0
        self._render_status_indicator(follow_latest=follow_latest)

    def _remove_runtime_status_message(self) -> None:
        status = self._runtime_status_message
        self._runtime_status_message = None
        if status is not None:
            status.remove()

    def _tick_status_indicator(self) -> None:
        """任务运行期间轮换固定宽度的 Braille 状态帧。"""

        # 模态审批成为当前 Screen 后，主工作台组件不在活动查询树中。此时暂停
        # 动画，既避免计时器访问隐藏状态，也不干扰 Esc 的审批取消绑定。
        if len(self.screen_stack) > 1:
            return
        for tree in self._subagent_trees.values():
            if tree.parent is not None:
                tree.refresh_elapsed()
        for tool_message in self._tool_messages.values():
            # 工具行与子代理树同节奏实时跳动；已完成的记录不在 dict 中，
            # 历史回放写入的 legacy 记录由 refresh_elapsed 的状态判断跳过。
            if tool_message.parent is not None:
                tool_message.refresh_elapsed()
        if self._runtime_status_state not in {"working", "waiting"}:
            return
        self._status_spinner_index = (
            self._status_spinner_index + 1
        ) % len(self.STATUS_SPINNER_FRAMES)
        self._render_status_indicator()

    def _render_status_indicator(self, *, follow_latest: bool | None = None) -> None:
        is_active = self._runtime_status_state in {"working", "waiting"}
        if not is_active:
            self._remove_runtime_status_message()
            return

        conversations = self.query("#conversation")
        if not conversations:
            return
        conversation = conversations.first(VerticalScroll)
        if follow_latest is None:
            follow_latest = self._is_conversation_at_end(conversation)
        status = self._runtime_status_message
        if status is None:
            status = Static("", classes="message runtime-status-message")
            self._runtime_status_message = status
            conversation.mount(status)
        spinner_frame = self.STATUS_SPINNER_FRAMES[self._status_spinner_index]
        status.update(f"{spinner_frame} {self._runtime_status_text}")
        if (
            status.parent is conversation
            and conversation.children
            and conversation.children[-1] is not status
        ):
            conversation.move_child(status, after=conversation.children[-1])
        self._scroll_conversation_if_following(conversation, follow_latest)

    def _pending_queue_text(self) -> Text:
        """生成右上角 FIFO 排队消息计数。"""

        return pending_queue_text(len(self._pending_inputs))

    def _refresh_pending_queue_count(self) -> None:
        """同步上下文行中的 FIFO 排队消息计数。"""

        self._refresh_context_summary()

    def _token_telemetry_text(self) -> Text:
        """生成第二行遥测：项目名、CTX 占用、IN/OUT/CA 与 tok/s。"""

        return token_telemetry_text(
            self._input_tokens,
            self._output_tokens,
            self._cached_input_tokens,
            getattr(self.agent, "context_window_tokens", 128_000),
            self._tokens_per_second,
        )

    @staticmethod
    def _compact_token_count(value: int) -> str:
        """兼容原有测试与调用入口。"""

        return compact_token_count(value)

    @staticmethod
    def _gradient_text(text: str) -> Text:
        """兼容原有测试与调用入口。"""

        return gradient_text(text)

    def _mcp_enabled_count(self) -> int:
        """返回当前全局启用的 MCP Server 数量，不触发 MCP 能力发现。"""

        manager = getattr(self.agent, "_mcp_manager", None)
        config = getattr(manager, "config", None)
        if not bool(getattr(config, "enabled", False)):
            return 0
        enabled_servers = getattr(config, "enabled_servers", ())
        try:
            return max(0, len(enabled_servers))
        except TypeError:
            return 0

    def _status_summary_text(self) -> Text:
        """生成第二行左段：模型、推理强度、审批模式、MCP 数量与排队数。

        行尾由 #token-telemetry 自带 “⁕ ” 前置分隔符衔接 CTX 段。
        """

        reasoning_effort = str(getattr(self.agent, "reasoning_effort", "") or "")
        if not reasoning_effort:
            reasoning_effort = self.startup.reasoning_effort or "DEFAULT"
        return status_summary_text(
            approval_mode=str(
                getattr(self.agent, "approval_mode", None) or self.startup.approval_label
            ),
            mcp_enabled_count=self._mcp_enabled_count(),
            pending_count=len(self._pending_inputs),
            model=str(getattr(self.agent, "current_model", "") or "NO MODEL"),
            reasoning_effort=reasoning_effort,
        )

    def _context_summary_text(self) -> Text:
        """渲染第一行左段：项目名；行尾由 #index-status 衔接。"""

        return context_summary_text(
            workspace=str(
                getattr(self.agent, "workspace_root", "") or self.startup.workspace_label
            ),
        )

    def _refresh_context_summary(self) -> None:
        """刷新顶部两段卡片与随 MCP 设置变化的遥测字段。"""

        self.query_one("#context-summary", Static).update(self._context_summary_text())
        self.query_one("#status-summary", Static).update(self._status_summary_text())
        self.query_one("#token-telemetry", Static).update(self._token_telemetry_text())

    def _open_settings(self) -> None:
        """打开中文设置面板；模型项关闭后复用现有模型选择器。"""

        def receive(action: SettingsAction | None) -> None:
            if action is not None and action.name == "model":
                self._open_model_picker(refresh=False)
            elif action is not None and action.name == "channels":
                def apply_channels(configuration) -> None:
                    self.agent.set_model(configuration.default_key)

                def receive_channels(result: ChannelManagerResult | None) -> None:
                    if result is not None:
                        self._input_tokens = 0
                        self._output_tokens = 0
                        self._cached_input_tokens = 0
                        self._append_message(
                            "status",
                            f"模型渠道已保存，当前模型：{self.agent.current_model}",
                        )
                    self._refresh_context_summary()
                    self._open_settings()

                self.push_screen(
                    ChannelManagerScreen(
                        resolve_config_path(),
                        resolve_models_path(),
                        apply_configuration=apply_channels,
                    ),
                    receive_channels,
                )
            elif action is not None and action.name == "subagents_advanced":
                self.push_screen(
                    SettingsScreen(self.agent, advanced=True),
                    lambda _action: self._open_settings(),
                )
            elif action is not None and action.name == "mcp_settings":
                self.push_screen(
                    MCPSettingsScreen(self.agent),
                    self._receive_mcp_settings,
                )
            elif action is not None and action.name == "tools_settings":
                self.push_screen(
                    ToolSettingsScreen(self.agent),
                    lambda _action: self._open_settings(),
                )
            elif action is not None and action.name == "vision":
                def apply_vision(configuration) -> None:
                    self.agent.set_vision_configuration(configuration)

                def receive_vision(result: VisionSettingsResult | None) -> None:
                    if result is not None:
                        state = "已启用" if result.configuration.enabled else "已停用"
                        self._append_message(
                            "status",
                            f"视觉模型代理{state}，已配置 {len(result.configuration.models)} 个故障转移模型。",
                        )
                    self._open_settings()

                self.push_screen(
                    VisionSettingsScreen(
                        self.agent,
                        resolve_config_path(),
                        apply_configuration=apply_vision,
                    ),
                    receive_vision,
                )
            else:
                self._drain_pending_inputs()
            self._refresh_context_summary()

        self.push_screen(SettingsScreen(self.agent), receive)

    def _receive_mcp_settings(self, action: MCPSettingsAction | None) -> None:
        if action is not None and action.name == "servers":
            self.push_screen(
                MCPServerListScreen(self.agent),
                lambda _result: self._open_settings(),
            )
        else:
            self._open_settings()
        self._refresh_context_summary()

    def _open_model_picker(self, *, refresh: bool = False) -> None:
        """打开双列模型选择界面；结束后返回设置面板。

        模型选择器只能从设置面板进入（``/model`` 命令已移除）。退出
        （切换成功或按 ESC 取消）时与设置面板其他选项页面保持一致：
        重新打开设置面板，且不显示取消/切换提示文案。
        """

        def receive(result: ModelPickerResult | None) -> None:
            if result is not None:
                # 切换后旧模型 token 与新模型上下文上限不应混显。
                self._input_tokens = 0
                self._output_tokens = 0
                self._cached_input_tokens = 0
                self._refresh_context_summary()
                self.query_one("#token-telemetry", Static).update(
                    self._token_telemetry_text()
                )
            self._drain_pending_inputs()
            # 参考设置面板其他选项页面（渠道/工具/MCP/视觉/子代理）：
            # 关闭当前页后重新打开设置面板回到主菜单。
            self._open_settings()

        self.push_screen(
            ModelPickerScreen(
                self.agent,
                refresh_on_open=refresh,
            ),
            receive,
        )


def run_fullscreen_tui(agent: LocalToolAgent, startup: FullscreenStartup) -> int:
    """运行默认全屏 TUI。"""

    app = OmniCrawlApp(agent, startup)
    try:
        app.run()
    finally:
        # Driver 正常会关闭鼠标报告，但退出期间的焦点/看门狗重启或 Driver
        # 内部清理异常可能让模式泄漏到后续 PowerShell Read-Host，必须再兜底一次。
        _disable_terminal_mouse_reporting()
    # Textual 会捕获定时器和消息处理异常并通过 return_code 报告，而不会重新抛出。
    # 必须向启动器透传，否则致命退出会被错误显示成“对话已结束”。
    return int(getattr(app, "return_code", 0) or 0)


__all__ = [
    "AgentTurnCallbacks",
    "AgentTurnController",
    "ConfirmationScreen",
    "FullscreenStartup",
    "ModelPickerResult",
    "ModelPickerScreen",
    "OmniCrawlApp",
    "ReasoningDisclosure",
    "ToolDisclosure",
    "run_fullscreen_tui",
]
