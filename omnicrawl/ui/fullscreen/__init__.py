"""Textual 全屏工作台。"""

from __future__ import annotations

import re
import sys
import threading
from collections import deque
from dataclasses import dataclass
from typing import Any, Callable

from rich.text import Text
from textual import events, work
from textual.app import App, ComposeResult
from textual.binding import Binding
from textual.containers import Horizontal, Vertical, VerticalScroll
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
from ...version_check import current_version
from .hud import (
    compact_token_count,
    context_summary_text,
    gradient_text,
    pending_queue_text,
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
    handle_review_command,
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
from .image_gen_settings import ImageGenSettingsResult, ImageGenSettingsScreen
from .monitor import MonitorStateAdapter, format_monitor_display_batch
from .theme import (
    ACCENT_AMBER,
    ACCENT_BLUE,
    BORDER_MUTED,
    TERMINAL_THEME,
    TEXT_MUTED,
    TEXT_SECONDARY,
    THEME_NAME,
    terminal_css,
)
from .turns import AgentTurnCallbacks, AgentTurnController
from .terminal_handling import (
    OmniCrawlWindowsDriver,
    OmniCrawlWindowsEventMonitor,
    TerminalHandlingMixin,
    _MOUSE_REPORTING_DISABLE_SEQUENCE,
    _disable_terminal_mouse_reporting,
    _restore_windows_raw_input_mode_if_needed,
    _restore_windows_vt_input_mode_if_needed,
)

from .rendering import RenderingMixin
from .widgets import (
    AssistantMessage,
    ConfirmationScreen,
    ReasoningDisclosure,
    SubAgentProgressTree,
    ToolDisclosure,
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


class OmniCrawlApp(TerminalHandlingMixin, RenderingMixin, App[None]):
    """可控全屏渲染的 OmniCrawl 工作台。"""

    TITLE = "OmniCrawl"
    SUB_TITLE = "Developer Workspace"
    CSS = terminal_css("""
    Screen { background: $terminal-canvas; color: $terminal-text; }
    /* 全局细滚动条：所有可滚动容器（对话区、各设置页列表、编辑器表单等）
       的滚动条宽度统一为 1 格，颜色统一为白色（菜单滚动条默认继承主题
       secondary 蓝色，在此覆盖为白色；#conversation 的 ID 规则优先级更高，
       保留其原有默认前景色与绿色 hover）。 */
    * {
        scrollbar-size: 1 1;
        scrollbar-color: $terminal-white;
        scrollbar-color-hover: $terminal-white;
    }
    #shell { height: 1fr; background: $terminal-background; }
    /* 顶部两行紧凑靠左：内容按实际宽度紧排，剩余空间留白在行尾；
       弹性占位把版本号推到整行尾部，行首与字段间用 │ 分隔。
       底部 HUD 整体左移对齐输入框左边框（左 margin 同为 2）。 */
    #topbar { height: 1; margin: 0; padding: 0 1 0 0; background: $terminal-surface; align: left middle; }
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
    /* 第二行展示 Token 明细。底部不再画横线分隔，行高 1 使内容直接
        贴齐屏幕底缘（原第 2 行用于承载底边框，删线后留空会形成
        1 行视觉空隙）；左侧与输入框左边框对齐（左 margin 同为 2）。 */
    #telemetry-row {
        height: 1;
        margin: 0;
        padding: 0 1 0 0;
        background: $terminal-panel;
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
    .message.runtime-status-message {
        color: $terminal-text-muted;
        text-style: bold;
        /* 临时状态行（⠧ 正在思考…）不占用上下空行：
           双 class 特异性高于 .message 的 blank 边框与底部 margin，
           避免流式更新中频繁出现/消失引起布局跳动。 */
        border-top: none;
        border-bottom: none;
        margin-bottom: 0;
    }
    .runtime-status-message.warning { color: $terminal-red; }
    #conversation {
        height: 1fr;
        padding: 0 1;
        background: $terminal-background;
        scrollbar-color: $terminal-scrollbar;
        scrollbar-color-hover: $terminal-green;
        scrollbar-background: $terminal-background;
    }
    .message {
        margin: 0 0 1 0;
        padding: 0 1;
        background: transparent;
        /* 消息间仅保留一行间隔：由 margin-bottom 1 提供，不再用 blank 边框额外撑高。 */
        border: none;
    }
    #conversation > .message:last-child { margin-bottom: 0; }
    /* 用户消息：无背景色，左侧青色细竖条强调；正文显式白色，
       user： 标签行保持灰色斜体。 */
    .user-message { color: $terminal-white; border-left: solid $terminal-cyan; }
    .assistant-message { color: $terminal-text; }
    .status-message { color: $terminal-text-muted; }
    .subagent-tree-message { color: $terminal-text; padding-left: 2; }
    /* 方案6：状态色点 + 缩进，最克制。工具卡不再使用边框/背景色块，
       状态由标题行首的状态色点（●）表达；正文缩进由 ToolDisclosure
       渲染时完成（BODY_INDENT）。工具背景透明，不再需要 .message 的
       blank 上下边框空行撑高：无输出正文的工具只占标题一行，避免两个
       无输出工具之间出现大段空白；行间距由 margin-bottom 正常提供。 */
    .tool-message {
        color: $terminal-tool-text;
        padding: 0 1;
        background: $terminal-tool-background;
        border-top: none;
        border-bottom: none;
    }
    .tool-message:focus { color: $terminal-tool-text; background: $terminal-tool-focus-background; }
    .error-message { color: $terminal-red; }
    /* 思考块：暗背景 + 灰前景 + 斜体（方案A）。终端字体无法逐控件切换，
       斜体是最接近“换字体”的观感；CJK 字符在多数终端不渲染斜体，
       主要作用于英文/代码部分。 */
    .reasoning-message {
        color: $terminal-reasoning-text;
        padding: 0 1;
        background: $terminal-reasoning-background;
        text-style: italic;
    }
    /* 输入区用白色圆角框独立成卡：顶部 HUD 已移到下方，靠边框与下方
        HUD 内容分隔开；圆角边框 + 左右 margin 让输入框成为悬浮卡片。 */
    #composer-wrap {
        height: 3;
        min-height: 3;
        /* 上侧留 1 行与对话区分隔，左右贴齐屏幕边缘（0 margin），
           下侧贴紧底部 HUD（0 margin）。 */
        margin: 1 0 0 0;
        padding: 0 2;
        background: $terminal-surface;
        border: round $terminal-white;
    }
    #command-menu {
        display: none;
        height: auto;
        max-height: 8;
        padding: 0 1;
        background: $terminal-surface;
        color: $terminal-text-secondary;
        text-wrap: nowrap;
        text-overflow: ellipsis;
        border-left: solid $terminal-white;
    }
    /* 生成期间排队的用户消息预览条：位于命令菜单之下、输入框之上，
       黄色左边条与命令菜单的白色区分；默认隐藏，有排队时由
       _render_pending_queue 动态显示并按摘要行数撑开高度。 */
    #pending-queue {
        display: none;
        height: auto;
        max-height: 5;
        padding: 0 1;
        background: $terminal-surface;
        color: $terminal-text-secondary;
        text-wrap: nowrap;
        text-overflow: ellipsis;
        border-left: solid $terminal-amber;
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
    # 统计平均生成速率（t/s）的待机判定阈值：相邻输出增量间隔超过该值
    # 视为“待机”（工具执行、模型停顿、回合间隙），不计入输出时长；
    # 间隔内的时长才累计为输出时间，避免空闲等待稀释平均速率。
    GENERATION_STANDBY_GAP_SECONDS = 2.0
    # 顶部 t/s 遥测的刷新间隔（累计统计值变化后最多延迟一个周期显示）。
    TOKEN_RATE_REFRESH_INTERVAL_SECONDS = 0.5
    MONITOR_POLL_INTERVAL_SECONDS = 0.5
    STATUS_SPINNER_INTERVAL_SECONDS = 0.08
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
    MOUSE_WHEEL_SCROLL_LINES = 5.0
    COMMAND_MENU_VISIBLE_OPTIONS = 8
    # 排队预览条最多同时展示的摘要行数（不含“⏳ N 条消息排队”标题行
    # 与“… 还有 N 条”折叠行）。
    QUEUE_PREVIEW_MAX_ROWS = 3
    # 每条排队消息首行摘要的最大字符数，超出用省略号截断。
    QUEUE_PREVIEW_SUMMARY_LIMIT = 40
    COMPOSER_MIN_ROWS = 1
    COMPOSER_MAX_ROWS = 5
    # composer-wrap 上下两侧各 1 行圆角边框，总计入 2 行。
    COMPOSER_BORDER_ROWS = 2

    def __init__(self, agent: LocalToolAgent, startup: FullscreenStartup) -> None:
        super().__init__(
            driver_class=(
                OmniCrawlWindowsDriver if sys.platform == "win32" else None
            )
        )
        self.scroll_sensitivity_y = self.MOUSE_WHEEL_SCROLL_LINES
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
            handle_review=lambda command_agent, command: handle_review_command(
                command_agent,
                command,
                on_subagent_event=lambda event_name, payload: self.call_from_thread(
                    self._handle_subagent_event,
                    event_name,
                    payload,
                ),
            ),
        )
        self._stream_message: AssistantMessage | None = None
        self._stream_markdown = ""
        self._stream_render_pending = False
        self._stream_start_text_len: int | None = None
        self._tool_messages: dict[str, ToolDisclosure] = {}
        self._subagent_trees: dict[str, SubAgentProgressTree] = {}
        # /review 等派生评审流程：子代理对话面板（│ 包裹 + 左右缩进）与
        # 活动开关。活动期间子代理事件渲染到对话面板而非进度树。
        self._subagent_conversations: dict[str, Any] = {}
        self._conversation_stream_active = False
        self._reasoning_message: ReasoningDisclosure | None = None
        # UI 私有的 Monitor cursor、暂停状态和失败隔离均由无 Textual 的适配器
        # 持有；本应用只安排定时刷新并渲染它返回的结构化事件批次。
        self._monitor_state = MonitorStateAdapter(agent)
        self._input_tokens = 0
        self._output_tokens = 0
        self._cached_input_tokens = 0
        # 统计平均生成速率（会话累计）：总输出 token ÷ 总输出时长。
        # token 来自流式文本增量的字符估算（非供应商用量）；时长只累计
        # 相邻增量间隔内的连续输出，待机间隔（见 GENERATION_STANDBY_GAP_SECONDS）
        # 不计入，因此输出停止后平均值保留不归零。
        self._generation_total_tokens = 0.0
        self._generation_total_seconds = 0.0
        self._last_generation_at: float | None = None
        # 回合开始前的累计快照：模型流中断回滚时撤销本回合已累计的量，
        # 避免重试生成的完整回复与半截输出重复计数。
        self._generation_stats_snapshot: tuple[float, float, float | None] | None = None
        self._tokens_per_second = 0.0
        self._runtime_status_text = "完成"
        self._runtime_status_state = "complete"
        self._runtime_status_message: Static | None = None
        self._status_spinner_index = 0
        self._command_matches: list[dict[str, str]] = []
        self._command_selection = 0
        self._interaction_watchdog_signature: tuple[object, ...] | None = None
        self._interaction_watchdog_stable_ticks = 0
        self._paste_sequence = 0
        self._compact_pastes: dict[str, str] = {}

    def compose(self) -> ComposeResult:
        with Vertical(id="shell"):
            yield VerticalScroll(id="conversation", can_focus=False)
            with Vertical(id="composer-wrap"):
                yield Static("", id="command-menu")
                yield Static("", id="pending-queue")
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
            # 顶部 HUD 内容整体移到输入框下方：内容本身不修改，仅调整位置。
            with Horizontal(id="topbar"):
                yield Static(self._context_summary_text(), id="context-summary")
            with Horizontal(id="telemetry-row"):
                yield Static(self._token_telemetry_text(), id="token-telemetry")
                yield Static(self._status_summary_text(), id="status-summary")

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
        # 缩放窗口时 conhost 可能重置控制台输入模式（鼠标记录因此消失）；
        # 立即核对并恢复，不等看门狗周期，避免缩小窗口后点击失效。
        driver = self._driver
        if not driver.is_headless:
            restore_input_mode = (
                _restore_windows_raw_input_mode_if_needed
                if isinstance(driver, OmniCrawlWindowsDriver)
                else _restore_windows_vt_input_mode_if_needed
            )
            try:
                restore_input_mode()
            except Exception:  # noqa: BLE001
                pass

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
            self.COMPOSER_BORDER_ROWS
            + composer_rows
            + menu_rows
            + self._pending_queue_rows()
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
        if self.is_generating:
            if self._command_dispatcher.is_immediate(text):
                # 即时命令（/settings、/skills、只读查询等）不占用回合线程，
                # 生成期间直接执行；有 I/O 或修改回合状态的命令仍按 FIFO 排队。
                self._handle_command(text)
                return
            self._pending_inputs.append(text)
            self._refresh_pending_queue_count()
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
            style = f"bold {ACCENT_AMBER}" if index == self._command_selection else TEXT_SECONDARY
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

    def copy_to_clipboard(self, text: str) -> None:
        """覆盖 Textual 的 OSC52 剪贴板实现。

        Textual 默认把文本作为 OSC52 转义序列写入终端（\x1b]52;c;<base64>\a），
        该协议仅 Windows Terminal / VS Code 等现代终端支持；传统 conhost
        （旧版 PowerShell / cmd 窗口）会直接忽略，导致 Ctrl+C 后系统剪贴板为空。
        这里在 Windows 上用 Win32 API 直接写入系统剪贴板（CF_UNICODETEXT），
        失败时回退到父类 OSC52 实现（兼容 Windows Terminal）。
        """
        if sys.platform == "win32" and text:
            try:
                import ctypes
                from ctypes import wintypes

                CF_UNICODETEXT = 13
                GMEM_MOVEABLE = 0x0002
                GMEM_ZEROINIT = 0x0040
                user32 = ctypes.windll.user32
                kernel32 = ctypes.windll.kernel32
                # 64 位系统上 HGLOBAL 是指针，必须显式声明 restype/argtypes，
                # 否则 ctypes 默认按 32 位 int 截断句柄导致 GlobalLock 失败
                kernel32.GlobalAlloc.restype = wintypes.HGLOBAL
                kernel32.GlobalAlloc.argtypes = [wintypes.UINT, ctypes.c_size_t]
                kernel32.GlobalLock.restype = ctypes.c_void_p
                kernel32.GlobalLock.argtypes = [wintypes.HGLOBAL]
                kernel32.GlobalFree.argtypes = [wintypes.HGLOBAL]
                kernel32.GlobalUnlock.argtypes = [wintypes.HGLOBAL]
                user32.SetClipboardData.restype = wintypes.HANDLE
                user32.SetClipboardData.argtypes = [wintypes.UINT, wintypes.HANDLE]
                user32.OpenClipboard.argtypes = [wintypes.HWND]
                # UTF-16LE 并以 NUL 结尾，供 CF_UNICODETEXT 使用
                data = (text + "\0").encode("utf-16-le")
                if not user32.OpenClipboard(None):
                    # 剪贴板被其他进程占用，回退 OSC52
                    return super().copy_to_clipboard(text)
                try:
                    user32.EmptyClipboard()
                    handle = kernel32.GlobalAlloc(
                        GMEM_MOVEABLE | GMEM_ZEROINIT, len(data)
                    )
                    if not handle:
                        return super().copy_to_clipboard(text)
                    locked = kernel32.GlobalLock(handle)
                    if not locked:
                        kernel32.GlobalFree(handle)
                        return super().copy_to_clipboard(text)
                    try:
                        ctypes.memmove(locked, data, len(data))
                    finally:
                        kernel32.GlobalUnlock(handle)
                    # 成功后句柄归系统所有，不可 GlobalFree
                    if not user32.SetClipboardData(CF_UNICODETEXT, handle):
                        kernel32.GlobalFree(handle)
                        return super().copy_to_clipboard(text)
                finally:
                    user32.CloseClipboard()
                return
            except Exception:
                # 任何异常都回退到 OSC52
                return super().copy_to_clipboard(text)
        super().copy_to_clipboard(text)

    def _clear_conversation_view(self) -> None:
        """移除对话区域的可见消息并复位流式渲染状态（不追加提示）。"""

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

    def action_clear_conversation(self) -> None:
        if self.is_generating:
            return
        self._clear_conversation_view()
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
        # 回合开始前快照累计统计；模型流中断自动重试触发回滚时据此恢复。
        self._generation_stats_snapshot = (
            self._generation_total_tokens,
            self._generation_total_seconds,
            self._last_generation_at,
        )
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
        status: str | None,
        command: Callable[[], str | None],
        *,
        refresh_context: bool = False,
        on_success: Callable[[], None] | None = None,
        on_finish: Callable[[], None] | None = None,
        working_status: str | None = None,
        stream_subagent_conversation: bool = False,
    ) -> None:
        """锁定输入并安排慢命令，避免在 Textual 主事件循环执行 I/O。

        ``status`` 为空时不追加静态提示（进度由事件实时渲染，如 /review 的
        评审进度树）；``working_status`` 覆盖 HUD 状态行文本。
        """

        self.is_generating = True
        self._cancel_requested.clear()
        if status:
            self._append_message("status", status)
        self._set_runtime_status(working_status or "等待", "waiting")
        self._conversation_stream_active = stream_subagent_conversation
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
                outcome.message,
                outcome.command,
                refresh_context=outcome.refresh_context,
                working_status=outcome.working_status,
                stream_subagent_conversation=outcome.stream_subagent_conversation,
                on_finish=(
                    self._monitor_state.resume_polling
                    if outcome.workspace_switch_requested
                    else None
                ),
            )
            return True
        if outcome.clear_conversation:
            self._clear_conversation_view()
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

    def _refresh_monitor_events(self) -> None:
        """渲染适配器返回的后台任务增量日志，不影响模型回合。"""

        for batch in self._monitor_state.refresh():
            self._append_message("tool", format_monitor_display_batch(batch))

    def _pending_queue_text(self) -> Text:
        """生成右上角 FIFO 排队消息计数。"""

        return pending_queue_text(len(self._pending_inputs))

    def _refresh_pending_queue_count(self) -> None:
        """同步 HUD 排队计数与输入区上方的排队预览条。"""

        self._refresh_context_summary()
        self._render_pending_queue()

    def _pending_queue_rows(self) -> int:
        """排队预览条当前应占用的行数（无排队时为 0）。

        行数 = 1 行标题 + 每条摘要一行（最多 QUEUE_PREVIEW_MAX_ROWS）
        + 超出部分折叠提示行（仅当队列超过最大展示行数时）。
        """

        if not self._pending_inputs:
            return 0
        rows = 1 + min(len(self._pending_inputs), self.QUEUE_PREVIEW_MAX_ROWS)
        if len(self._pending_inputs) > self.QUEUE_PREVIEW_MAX_ROWS:
            rows += 1
        return rows

    def _render_pending_queue(self) -> None:
        """在 composer 上方渲染 FIFO 排队消息预览条；空队列时隐藏。

        标题行显示排队总数，随后按 FIFO 顺序展示每条消息的首行摘要
        （超出 QUEUE_PREVIEW_MAX_ROWS 的部分折叠为一行计数），并在每次
        队列变化（排队、逐条发送、清空）后同步高度。
        """

        queue = self.query_one("#pending-queue", Static)
        if not self._pending_inputs:
            queue.display = False
            queue.update("")
            return
        lines = Text(no_wrap=True, overflow="ellipsis")
        lines.append(
            f"⏳ {len(self._pending_inputs)} 条消息排队",
            style=f"bold {ACCENT_AMBER}",
        )
        for index, text in enumerate(
            list(self._pending_inputs)[: self.QUEUE_PREVIEW_MAX_ROWS], start=1
        ):
            first_line = text.splitlines()[0] if text else ""
            summary = " ".join(first_line.split())[: self.QUEUE_PREVIEW_SUMMARY_LIMIT]
            lines.append("\n")
            lines.append(f"  {index}. {summary}", style=TEXT_SECONDARY)
        if len(self._pending_inputs) > self.QUEUE_PREVIEW_MAX_ROWS:
            lines.append("\n")
            lines.append(
                f"  … 还有 {len(self._pending_inputs) - self.QUEUE_PREVIEW_MAX_ROWS} 条",
                style=TEXT_MUTED,
            )
        queue.update(lines)
        queue.display = True
        self._resize_composer_to_text()

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
        """渲染第一行左段：项目路径。"""

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
            elif action is not None and action.name == "image_gen":
                def apply_image_gen(configuration) -> None:
                    self.agent.set_image_gen_configuration(configuration)

                def receive_image_gen(result: ImageGenSettingsResult | None) -> None:
                    if result is not None:
                        state = "已启用" if result.configuration.enabled else "已停用"
                        self._append_message(
                            "status",
                            f"图像生成{state}（{result.configuration.model}，"
                            f"{result.configuration.base_url}）。",
                        )
                    self._open_settings()

                self.push_screen(
                    ImageGenSettingsScreen(
                        resolve_config_path(),
                        apply_configuration=apply_image_gen,
                    ),
                    receive_image_gen,
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
