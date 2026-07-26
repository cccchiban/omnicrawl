"""Textual 全屏工作台。"""

from __future__ import annotations

import sys
import threading
import time
from collections import deque
from dataclasses import dataclass
from typing import Any, Callable

from rich.text import Text
from textual import events, work
from textual.app import App, ComposeResult
from textual.containers import Horizontal, Vertical, VerticalScroll
from textual.widgets import Static, TextArea

from ...agent import AgentError, LocalToolAgent
from ...config.runtime import resolve_config_path, resolve_models_path
from ...agent.tools import public_tool_arguments
from ...version_check import (
    VersionCheckResult,
    check_latest_version,
    current_version,
)
from .hud import (
    compact_token_count,
    context_summary_text,
    gradient_text,
    pending_queue_text,
    token_telemetry_text,
    version_status_text,
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
    handle_model_command,
    handle_reasoning_command,
    handle_session_command,
    handle_subagent_task_command,
)
from .channel_manager import ChannelManagerResult, ChannelManagerScreen
from .commands import CommandDispatcher
from .model_picker import ModelPickerResult, ModelPickerScreen
from .settings import SettingsAction, SettingsScreen
from .mcp_settings import MCPServerListScreen, MCPSettingsAction, MCPSettingsScreen
from .monitor import MonitorStateAdapter, format_monitor_display_batch
from .theme import (
    ACCENT_BLUE,
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
    """多行编辑器：Enter 由应用提交，Ctrl+Enter 插入真实换行。"""

    def __init__(
        self,
        *,
        submit_handler: Callable[[], None],
        command_key_handler: Callable[[events.Key], bool],
        copy_or_clear_handler: Callable[[], None],
        **kwargs: Any,
    ) -> None:
        super().__init__(**kwargs)
        self._submit_handler = submit_handler
        self._command_key_handler = command_key_handler
        self._copy_or_clear_handler = copy_or_clear_handler

    def on_key(self, event: events.Key) -> None:
        if event.key == "ctrl+c":
            self._copy_or_clear_handler()
            event.prevent_default()
            event.stop()
        elif self._command_key_handler(event):
            event.prevent_default()
            event.stop()
        elif event.key == "enter":
            self._submit_handler()
            event.prevent_default()
            event.stop()
        elif event.key in {"ctrl+enter", "ctrl+\r", "ctrl+j"}:
            # Windows Terminal / VS Code 在 Kitty 扩展按键协议不可用时，可能将
            # Ctrl+Enter 上报为 ctrl+\r 或 ctrl+j，而非 ctrl+enter。
            self.insert("\n")
            event.prevent_default()
            event.stop()


class OmniCrawlApp(App[None]):
    """可控全屏渲染的 OmniCrawl 工作台。"""

    TITLE = "OmniCrawl"
    SUB_TITLE = "Developer Workspace"
    CSS = terminal_css("""
    Screen { background: $terminal-canvas; color: $terminal-text; }
    #shell { height: 1fr; background: $terminal-background; }
    /* 顶部第一行由稳态上下文占满；瞬时运行态放入对话流。 */
    #topbar { height: 1; padding: 0 1; background: $terminal-surface; align: left middle; }
    #context-summary {
        width: 1fr;
        min-width: 0;
        color: $terminal-text-muted;
        content-align: left middle;
        text-overflow: ellipsis;
    }
    #version-status {
        width: 22;
        min-width: 22;
        max-width: 22;
        color: $terminal-text;
        content-align: right middle;
        text-overflow: ellipsis;
    }
    /* 第二行 Token 使用与第一行上下文摘要相同的左边距。
       height 必须至少为 2：Textual 的 border-bottom 会占用 1 行布局高度，
       若 height=1 则内容区高度被压成 0，导致 IN/OUT/CA/CTX 有 content 但不渲染。 */
    #token-telemetry {
        height: 2;
        padding: 0 1;
        background: $terminal-panel;
        color: $terminal-text-muted;
        content-align: left middle;
        text-overflow: ellipsis;
        border-bottom: solid $terminal-border;
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
    .reasoning-message.collapsed { height: 1; }
    #composer-wrap { height: 2; min-height: 2; background: $terminal-surface; border-top: solid $terminal-border-strong; padding: 0 1; }
    #command-menu {
        display: none;
        height: auto;
        max-height: 8;
        padding: 0 1;
        background: $terminal-surface;
        color: $terminal-text-secondary;
        border-left: thick $terminal-blue;
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
    #composer:focus { border-left: thick $terminal-green; background: $terminal-panel; }
    #composer:focus .text-area--cursor-line { background: $terminal-panel; }
    """)

    BINDINGS = [
        ("escape", "cancel_or_focus", "取消 / 输入框"),
        ("ctrl+c", "copy_or_clear_composer", "复制 / 清空输入"),
        ("ctrl+l", "clear_conversation", "清空视图"),
    ]

    STREAM_RENDER_INTERVAL_SECONDS = 0.05
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
        super().__init__()
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
            handle_model=lambda command_agent, command: handle_model_command(
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
        self._tool_messages: dict[str, ToolDisclosure] = {}
        self._subagent_trees: dict[str, SubAgentProgressTree] = {}
        self._reasoning_message: ReasoningDisclosure | None = None
        # UI 私有的 Monitor cursor、暂停状态和失败隔离均由无 Textual 的适配器
        # 持有；本应用只安排定时刷新并渲染它返回的结构化事件批次。
        self._monitor_state = MonitorStateAdapter(agent)
        self._input_tokens = 0
        self._output_tokens = 0
        self._cached_input_tokens = 0
        self._runtime_status_text = "完成"
        self._runtime_status_state = "complete"
        self._runtime_status_message: Static | None = None
        self._status_spinner_index = 0
        self._command_matches: list[dict[str, str]] = []
        self._command_selection = 0
        self._announced_update_version: str | None = None
        self._version_checking = False
        self._version_check_frame = 0
        self._interaction_watchdog_signature: tuple[object, ...] | None = None
        self._interaction_watchdog_stable_ticks = 0

    def compose(self) -> ComposeResult:
        with Vertical(id="shell"):
            with Horizontal(id="topbar"):
                yield Static(self._context_summary_text(), id="context-summary")
                yield Static(
                    version_status_text(self.startup.current_version),
                    id="version-status",
                )
            yield Static(self._token_telemetry_text(), id="token-telemetry")
            yield VerticalScroll(id="conversation", can_focus=False)
            with Vertical(id="composer-wrap"):
                yield Static("", id="command-menu")
                yield Composer(
                    submit_handler=self._submit_composer_text,
                    command_key_handler=self._handle_composer_command_key,
                    copy_or_clear_handler=self.action_copy_or_clear_composer,
                    placeholder="› 输入消息或 / 命令",
                    id="composer",
                    soft_wrap=True,
                    show_line_numbers=False,
                    highlight_cursor_line=False,
                )

    def on_mount(self) -> None:
        self.agent.set_confirm_handler(self._confirm_tool)
        self.query_one("#composer", TextArea).focus()
        self._resize_composer_to_text()
        self.set_interval(self.STATUS_SPINNER_INTERVAL_SECONDS, self._tick_status_indicator)
        self.set_interval(self.STATUS_SPINNER_INTERVAL_SECONDS, self._tick_version_check)
        self.set_interval(
            self.INTERACTION_WATCHDOG_INTERVAL_SECONDS,
            self._recover_stale_mouse_interaction,
        )
        if self._monitor_state.can_schedule_refresh:
            self.set_interval(self.MONITOR_POLL_INTERVAL_SECONDS, self._refresh_monitor_events)
        if self.startup.version_check_enabled:
            self._version_checking = True
            self._version_check_frame = 0
            self._render_version_status()
            self._check_for_updates()

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

    def _recover_stale_mouse_interaction(self) -> None:
        """释放长时间没有变化的鼠标按下状态，避免输入事件永久失效。"""

        try:
            driver = self._driver
            if not driver.is_headless and _restore_windows_vt_input_mode_if_needed():
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
            if not driver.is_headless:
                _restore_windows_vt_input_mode_if_needed()
            enable_mouse_support = getattr(driver, "_enable_mouse_support", None)
            if callable(enable_mouse_support):
                enable_mouse_support()
            write = getattr(driver, "write", None)
            if callable(write):
                write("\033[?1004h")
                write("\x1b[>1u")
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

    @work(thread=True, exclusive=True, group="version-check", exit_on_error=False)
    def _check_for_updates(self) -> None:
        """首屏完成后在后台检查 PyPI，避免网络状态影响 TUI 启动。"""

        result = check_latest_version(self.startup.current_version)
        self.call_from_thread(self._apply_version_check, result)

    def _tick_version_check(self) -> None:
        """检查更新期间让版本号右侧的四点指示器循环移动。"""

        if not self._version_checking:
            return
        self._version_check_frame = (self._version_check_frame + 1) % 4
        self._render_version_status()

    def _render_version_status(self, latest_version: str | None = None) -> None:
        """在主线程绘制版本号、升级提示或检查更新动画。"""

        self.query_one("#version-status", Static).update(
            version_status_text(
                self.startup.current_version,
                latest_version,
                checking=self._version_checking,
                animation_frame=self._version_check_frame,
            )
        )

    def _apply_version_check(self, result: VersionCheckResult) -> None:
        """在 Textual 主线程更新版本区域，并只提示一次可用升级。"""

        self._version_checking = False
        latest = result.latest_version if result.update_available else None
        self._render_version_status(latest)
        if latest is None or latest == self._announced_update_version:
            return
        self._announced_update_version = latest
        self._append_message(
            "status",
            (
                f"发现新版本 {latest}（当前 {result.current_version}）。"
                "可执行 pip install --upgrade omnicrawl-agent 更新。"
            ),
        )

    @work(thread=True, exclusive=True, group="mcp-preload", exit_on_error=False)
    def _preload_mcp_tools(self) -> None:
        """主界面显示后在后台发现 MCP，避免阻塞 Textual 首屏绘制。"""

        try:
            self._turn_controller.preload_mcp_tools()
        except AgentError as exc:
            self.call_from_thread(self._append_message, "error", f"MCP 能力加载失败：{exc}")
        except Exception as exc:
            self.call_from_thread(self._append_message, "error", f"MCP 能力加载异常：{exc}")
        finally:
            self.call_from_thread(self._finish_turn)

    def on_text_area_changed(self, event: TextArea.Changed) -> None:
        if event.text_area.id == "composer":
            self._refresh_command_menu(event.text_area.text)
            self._resize_composer_to_text()

    def on_resize(self, _event: events.Resize) -> None:
        """窗口宽度变化后，按 Textual 重算的软折行高度更新输入区。"""

        self.call_after_refresh(self._resize_composer_to_text)

    def _resize_composer_to_text(self) -> None:
        """让输入区从一行起步，随软折行增长且不挤占整个消息区。"""

        composer = self.query_one("#composer", TextArea)
        composer_rows = min(
            self.COMPOSER_MAX_ROWS,
            max(self.COMPOSER_MIN_ROWS, composer.virtual_size.height),
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
        text = composer.text.strip()
        if not text:
            return
        composer.clear()
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
        lines = Text()
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
            description = option.get("description", "").strip()
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
        self._reset_mouse_interaction_state(
            focus_composer=not self.is_generating,
            rearm_terminal_protocols=True,
        )
        if self.is_generating:
            self.cancel_pending_turn()

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

    def action_clear_conversation(self) -> None:
        if self.is_generating:
            return
        self.query_one("#conversation", VerticalScroll).remove_children()
        self.conversation_text = ""
        self._stream_message = None
        self._stream_markdown = ""
        self._stream_render_pending = False
        self._tool_messages.clear()
        self._subagent_trees.clear()
        self._reasoning_message = None
        self._runtime_status_message = None
        self._append_message("status", "已清空当前视图，不影响会话历史。")

    def cancel_pending_turn(self) -> None:
        """标记当前回合已取消，使工作线程在下一个可中断点退出。"""

        self._cancel_requested.set()
        self._set_runtime_status("等待", "waiting")

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
            self.exit()
            return True
        if outcome.open_model_picker:
            self._open_model_picker(refresh=outcome.model_picker_refresh)
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
            # Textual 的锚定语义会在内容重新布局后持续跟随底部，并在用户手动
            # 滚动时自动释放；这比跨刷新排队 scroll_end 更能避免流式更新竞态。
            conversation.anchor()
            if defer_until_refresh:
                conversation.call_after_refresh(conversation.scroll_end, animate=False)

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
        self._stream_message = None
        self._stream_markdown = ""
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
        output = str(result.output or "无输出")
        if len(output) > self.MAX_TOOL_OUTPUT_CHARS:
            output = (
                f"{output[:self.MAX_TOOL_OUTPUT_CHARS]}\n"
                f"... 界面展示已截断（原始输出 {len(result.output)} 字符）。"
            )
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
        self._set_runtime_status("正在思考", "working")
        self._scroll_conversation_if_following(conversation, follow_latest)

    def _append_delta(self, delta: str) -> None:
        if not delta:
            return
        self._reasoning_message = None
        conversation = self.query_one("#conversation", VerticalScroll)
        follow_latest = self._is_conversation_at_end(conversation)
        if self._stream_message is None:
            self._stream_message = AssistantMessage(self._stream_markdown)
            self._stream_markdown = "◇ "
            conversation.mount(self._stream_message)
        self._stream_markdown += delta
        self.conversation_text += f"{delta}\n"
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
            conversation.mount(widget)
            if track_tool:
                self._tool_messages[f"legacy:{id(widget)}"] = widget
        self.conversation_text += f"{text}\n"
        if follow_with_runtime_status:
            self._render_status_indicator(follow_latest=follow_latest)
        self._scroll_conversation_if_following(conversation, follow_latest)

    def _finish_turn(self) -> None:
        self._render_stream_markdown()
        self.is_generating = False
        self._reasoning_message = None
        self._set_runtime_status("完成", "complete")
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
        """生成紧凑 Token 遥测；CTX 使用最近请求输入量表示当前上下文占用。"""

        return token_telemetry_text(
            self._input_tokens,
            self._output_tokens,
            self._cached_input_tokens,
            getattr(self.agent, "context_window_tokens", 128_000),
            self._mcp_enabled_count(),
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

    def _context_summary_text(self) -> Text:
        """用短键值字段渲染项目、模型、推理、审批和排队状态。"""

        reasoning_effort = str(getattr(self.agent, "reasoning_effort", "") or "")
        if not reasoning_effort:
            reasoning_effort = self.startup.reasoning_effort or "DEFAULT"
        return context_summary_text(
            workspace=str(
                getattr(self.agent, "workspace_root", "") or self.startup.workspace_label
            ),
            model=str(getattr(self.agent, "current_model", "") or "NO MODEL"),
            reasoning_effort=reasoning_effort,
            approval_mode=str(
                getattr(self.agent, "approval_mode", None) or self.startup.approval_label
            ),
            pending_count=len(self._pending_inputs),
        )

    def _refresh_context_summary(self) -> None:
        """刷新顶部上下文和随 MCP 设置变化的遥测字段。"""

        self.query_one("#context-summary", Static).update(self._context_summary_text())
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
        """打开双列模型选择界面；切换成功后刷新 HUD 并清零最近 Token 显示。"""

        def receive(result: ModelPickerResult | None) -> None:
            if result is None:
                self._append_message("status", "已取消模型切换。")
            else:
                # 切换后旧模型 token 与新模型上下文上限不应混显。
                self._input_tokens = 0
                self._output_tokens = 0
                self._cached_input_tokens = 0
                self._refresh_context_summary()
                self.query_one("#token-telemetry", Static).update(self._token_telemetry_text())
                message = result.message or f"当前模型已切换为 {result.model}"
                self._append_message("status", message)
            self._drain_pending_inputs()

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
