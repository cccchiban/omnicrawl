"""全屏工作台的核心装配类 ``OmniCrawlApp``（对应 agent/core.py 的组合门面）。

P3 重构从 ``ui/fullscreen/__init__.py`` 拆出（2026-08-21）：类骨架（CSS、
BINDINGS、常量、状态装配、compose/on_mount/on_resize、滚动动作）集中在本
模块，各领域方法按职责拆分到 ``input/``、``turn/``、``screens/``、
``status/``、``conversation/`` 子包的 Mixin 中，由本类按 MRO 组合。
``ui/fullscreen/__init__.py`` 只保留公开 API 与既有测试的模块级 patch 点。
"""

from __future__ import annotations

import random
import sys
import threading
from collections import deque
from typing import Any

from rich.text import Text
from textual import events
from textual.app import App, ComposeResult
from textual.binding import Binding
from textual.containers import Horizontal, Vertical, VerticalScroll
from textual.widgets import Static, TextArea

from ....agent import AskUserRequest, LocalToolAgent
from .._compat import resolve_facade
from ..conversation.view import (
    CONVERSATION_DISPLAY_MAX_LOGICAL_LINES as _CONVERSATION_DISPLAY_MAX_LOGICAL_LINES,
    ConversationViewMixin,
)
from ..input.composer import Composer
from ..input.editing import InputMixin
from ..input.menu import CommandMenuMixin
from ..input.sessions_menu import SessionsMenuMixin
from ..rendering.logo_anim import (
    LOGO_ANIM_FRAME_SECONDS,
    LOGO_ANIM_SECONDS,
    welcome_logo_frame,
)
from ..rendering.pipeline import RenderingMixin
from ..rendering.welcome_logo import welcome_logo_text
from ..rendering.widgets import (
    AssistantMessage,
    ReasoningDisclosure,
    SubAgentProgressTree,
    TodoPlan,
    ToolDisclosure,
)
from ..screens.navigation import SettingsNavigationMixin
from ..status.indicators import StatusMixin
from ..support.commands import CommandDispatcher
from ..support.monitor import MonitorStateAdapter
from ..support.turns import AgentTurnController
from ..terminal.handling import (
    OmniCrawlWindowsDriver,
    TerminalHandlingMixin,
    _restore_windows_raw_input_mode_if_needed,
    _restore_windows_vt_input_mode_if_needed,
)
from ..terminal.theme import TERMINAL_THEME, THEME_NAME, terminal_css
from ..turn.execution import TurnExecutionMixin
from .startup import FullscreenStartup


class OmniCrawlApp(
    TerminalHandlingMixin,
    RenderingMixin,
    InputMixin,
    CommandMenuMixin,
    SessionsMenuMixin,
    TurnExecutionMixin,
    SettingsNavigationMixin,
    StatusMixin,
    ConversationViewMixin,
    App[None],
):
    """可控全屏渲染的 OmniCrawl 工作台。"""

    TITLE = "OmniCrawl"
    SUB_TITLE = "Developer Workspace"
    CSS = terminal_css("""\
    Screen { background: $terminal-canvas; color: $terminal-text; }
    /* 全局细滚动条：所有可滚动容器（对话区、各设置页列表、编辑器表单等）
       的滚动条宽度统一为 1 格，颜色统一为白色（菜单滚动条默认继承主题
       secondary 蓝色，在这里覆盖为白色；#conversation 的 ID 规则优先级更高，
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
    /* 底部单行轮播 HUD：原两行内容（工作区路径 / 遥测+模型状态）合并为
       一行，按遥测 20s、工作区路径 10s 交替显示；切换时以解密扫描特效
       过渡（旧文本被乱码从左到右侵蚀、新文本由乱码从左到右吐出）。
       左侧与输入框左边框对齐（左 margin 同为 2），行高 1 使内容直接
       贴齐屏幕底缘。 */
    #bottom-carousel {
        height: 1;
        margin: 0;
        padding: 0 1 0 0;
        background: $terminal-panel;
        align: left middle;
    }
    #carousel-display {
        width: auto;
        min-width: 0;
        max-width: 100%;
        height: 1;
        padding: 0;
        color: $terminal-text-muted;
        content-align: left middle;
        text-overflow: ellipsis;
        text-wrap: nowrap;
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
       斜体是最接近"换字体"的观感；CJK 字符在多数终端不渲染斜体，
       主要作用于英文/代码部分。 */
    .reasoning-message {
        color: $terminal-reasoning-text;
        padding: 0 1;
        background: $terminal-reasoning-background;
        text-style: italic;
    }
    /* Agent 自动计划区位于输入框上方。它是 composer-wrap 的内容，
       因此高度增加时会直接挤占会话区，而不是覆盖会话消息；每行不留
       额外上下间距，完成图标的绿色由 TodoPlan 的 Rich Text 提供。 */
    #todo-plan {
        display: none;
        height: auto;
        min-height: 0;
        max-height: 20;
        padding: 0 1;
        background: $terminal-surface;
        color: $terminal-text-secondary;
        text-wrap: nowrap;
        text-overflow: ellipsis;
    }
    /* 启动后空会话的欢迎 Logo：覆盖在对话区域内，水平居中、垂直下移四行。
       绝对定位不参与消息列表排版，不产生滚动空白；首个消息出现后由应用隐藏，
       让位给会话内容。offset 水平 0 让容器贴齐对话区内容区左缘，配合
       content-align: center 使 Logo 左右到屏幕边缘间距对称；垂直固定 4 行将
       Logo 下移四行（offset 不参与布局流，不撑高容器，region 高度保持 8，
       下方不会越变越大）。
       行首缩进是字形定位，不能去；nowrap 防止块字被软折行破坏对齐。 */
    #welcome-logo {
        position: absolute;
        offset: 0 4;
        width: 100%;
        height: auto;
        min-height: 0;
        padding: 0 2;
        color: $terminal-white;
        content-align: center top;
        text-wrap: nowrap;
        text-overflow: clip;
    }
    /* 输入区用白色圆角框独立成卡：顶部 HUD 已移到下方，靠边框与下方
        HUD 内容分隔开；圆角边框 + 左右 margin 让输入框成为悬浮卡片。 */
    /* Agent 的 ask_user 面板位于输入框上方：问题和单选项保持同一组
       视觉层级，不写入会话区。所有 kind 都以选项呈现，question/confirm
       同时保留自由文本输入兜底；选项末尾统一追加「but I Think...」
       自定义入口。面板高度按视口比例动态封顶，超出后面板内部滚动，
       保证长问题/多选项不会把下方输入框挤出可视区域。 */
    #ask-user-panel {
        display: none;
        height: auto;
        min-height: 0;
        padding: 0 1;
        color: $terminal-text;
        background: $terminal-surface;
        border: round $terminal-white;
        /* 高度由代码按视口比例封顶（ASK_USER_PANEL_MAX_ROWS_FRACTION），
           内容更高时面板内部滚动，下方输入框始终可见。 */
    }
    #ask-user-panel > Static {
        width: 100%;
    }
    #ask-user-header {
        height: 1;
        min-height: 1;
        padding: 0 1;
        color: $terminal-amber;
        text-style: bold;
    }
    #ask-user-question {
        height: auto;
        min-height: 1;
        padding: 0 1;
        color: $terminal-white;
        text-style: bold;
        text-wrap: wrap;
        text-overflow: ellipsis;
    }
    #ask-user-options {
        display: none;
        height: auto;
        min-height: 0;
        padding: 0 1;
        color: $terminal-amber;
        background: $terminal-surface;
    }
    /* 单选行文本颜色由 AskUserOption 按状态绘制：普通白色「▢」，
       选中/悬停黄色「▣」（bold ansi_yellow）。:focus 兜底色保持
       黄色，与悬停高亮一致。 */
    #ask-user-options AskUserOption {
        display: block;
        height: auto;
        min-height: 1;
        padding: 0;
        border: none;
        color: $terminal-amber;
        background: $terminal-surface;
        text-wrap: wrap;
    }
    #ask-user-options AskUserOption:focus { color: $terminal-amber; }
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
    /* 会话列表预选菜单：位于命令菜单之下、输入框之上，样式与命令菜单
       保持一致（白色左边条 + 暗色背景），让用户感知这是同一层级的交互。 */
    #sessions-menu {
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
    /* 选中即复制后的状态提示：位于输入框上方（排队条之下），显示 2 秒
       后自动隐藏；绿色表示复制成功，内容超出预览上限时省略号截断。 */
    #copy-status {
        display: none;
        height: 1;
        min-height: 1;
        padding: 0 1;
        background: $terminal-surface;
        color: $terminal-green;
        text-wrap: nowrap;
        text-overflow: ellipsis;
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
    # 流式停顿多久后做一次全量精确 Markdown 重绘（< 该间隔时仅增量渲染
    # 已就绪的文本块，避免逐 50ms 全量重解析长消息）。
    STREAM_SETTLE_SECONDS = 0.5
    # 流式渲染块的最大长度：超过该长度且不含换行时强制落盘一次，
    # 避免模型长时间输出单段文本时界面长时间无更新。
    STREAM_CHUNK_LIMIT = 512
    # 流式停顿后全量精确重绘的文本长度上限：超过该长度的消息在停顿时不
    # 全量重绘（保留增量渲染结果），只在回合结束/工具边界等收口处重绘，
    # 避免一次停顿触发数百毫秒的同步重解析。
    STREAM_FULL_RENDER_LIMIT = 30_000
    # 对话区只渲染最近的逻辑文本行；被隐藏的旧消息组件保留在 DOM 中，
    # 以便 /undo 后重新计算窗口并恢复显示。
    CONVERSATION_DISPLAY_MAX_LOGICAL_LINES = _CONVERSATION_DISPLAY_MAX_LOGICAL_LINES
    # 统计平均生成速率（t/s）的待机判定阈值：相邻输出增量间隔超过该值
    # 视为"待机"（工具执行、模型停顿、回合间隙），不计入输出时长；
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
    # 排队预览条最多同时展示的摘要行数（不含"⏳ N 条消息排队"标题行
    # 与"… 还有 N 条"折叠行）。
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
        self._conversation_visibility_dirty = True
        self._conversation_visibility_refresh_pending = False
        self._conversation_visible_logical_lines = 0
        self._pending_inputs: deque[str] = deque()
        self._cancel_requested = threading.Event()
        # Agent 回合协议和取消令牌由非 Textual 控制器持有；本应用仅适配其
        # 回调回到主线程并保留 UI/审批状态。
        self._turn_controller = AgentTurnController(agent, self._cancel_requested)
        # 用 lambda 延迟解析门面模块级委托函数：重构后的 Dispatcher 不依赖
        # Textual，且测试/扩展仍可在 App 创建后 patch
        # ``omnicrawl.ui.fullscreen.<委托名>`` 命令入口（resolve_facade
        # 在调用时按名解析，patch 立即生效）。
        self._command_dispatcher = CommandDispatcher(
            agent,
            format_skills=lambda command_agent: resolve_facade(
                "format_skills_list"
            )(command_agent),
            format_mcp=lambda command_agent: resolve_facade(
                "format_mcp_status"
            )(command_agent),
            format_plugins=lambda command_agent: resolve_facade(
                "format_plugins_status"
            )(command_agent),
            format_memory_clean=lambda command_agent: resolve_facade(
                "format_memory_clean_result"
            )(command_agent),
            handle_session=lambda command_agent, command: resolve_facade(
                "handle_session_command"
            )(command_agent, command),
            handle_subagent_task=lambda command_agent, command: resolve_facade(
                "handle_subagent_task_command"
            )(command_agent, command),
            handle_approval=lambda command_agent, command: resolve_facade(
                "handle_approval_command"
            )(command_agent, command),
            handle_mode=lambda command_agent, command: resolve_facade(
                "handle_mode_command"
            )(command_agent, command),
            handle_reasoning=lambda command_agent, command: resolve_facade(
                "handle_reasoning_command"
            )(command_agent, command),
            handle_review=lambda command_agent, command: resolve_facade(
                "handle_review_command"
            )(
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
        # 尚未落盘的流式缓冲：按换行边界切成小块增量渲染，避免逐分片
        # 全量重解析整条消息的 Markdown（长消息数百毫秒/次）。
        self._stream_render_buffer = ""
        self._stream_nl_count = 0
        self._stream_ends_newline = True
        self._stream_last_delta_at = 0.0
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
        # 会话列表预选菜单状态（SessionsMenuMixin 使用）。
        self._session_menu_items: list[Any] = []
        self._session_menu_selection = 0
        self._interaction_watchdog_signature: tuple[object, ...] | None = None
        self._interaction_watchdog_stable_ticks = 0
        self._paste_sequence = 0
        self._compact_pastes: dict[str, str] = {}
        # 当前回合的自动计划只存在展示层；任务完成后仍保留完成勾选，
        # 下一条用户任务开始时由 Agent 的新计划替换或清空。
        self._todo_plan_items: list[dict[str, Any]] = []
        self._ask_user_request: AskUserRequest | None = None
        self._ask_user_answer: str | None = None
        self._ask_user_event = threading.Event()
        self._ask_user_custom_mode = False
        self._ask_user_selection = 0
        # 底部单行轮播 HUD：遥测页停留 20s、工作区路径页停留 10s 交替；
        # 切换时以解密扫描特效过渡，随机源固定实例便于测试复现。
        self._carousel_page = "telemetry"
        self._carousel_settled_text: Text | None = None
        self._carousel_animating = False
        self._carousel_anim_target: Text | None = None
        self._carousel_anim_target_old: Text | None = None
        self._carousel_anim_frame = 0
        self._carousel_anim_total_frames = 0
        self._carousel_anim_interval: Any = None
        self._carousel_hold_timer: Any = None
        self._carousel_rand = random.Random()
        # 欢迎 Logo 解密扫描入场动画：仅首次挂载播放一次；定时器/游标与
        # 轮播同一模式，隐藏或清空会话时由 _stop_welcome_logo_animation
        # 收口，避免残留回调。
        self._logo_anim_interval: Any = None
        self._logo_anim_frame = 0
        self._logo_anim_total_frames = 0
        self._logo_anim_started = False
        self._logo_rand = random.Random()

    def compose(self) -> ComposeResult:
        with Vertical(id="shell"):
            with VerticalScroll(id="conversation", can_focus=False):
                yield Static(welcome_logo_text(), id="welcome-logo")
            with Vertical(id="composer-wrap"):
                yield TodoPlan()
                # ask_user 面板允许内部滚动：问题过长/选项过多时在面板内
                # 滚动查看，而不是把输入框或 HUD 挤出屏幕。
                with VerticalScroll(id="ask-user-panel"):
                    yield Static("", id="ask-user-header")
                    yield Static("", id="ask-user-question")
                    with Vertical(id="ask-user-options"):
                        pass
                yield Static("", id="command-menu")
                yield Static("", id="sessions-menu")
                yield Static("", id="pending-queue")
                yield Static("", id="copy-status")
                yield Composer(
                    submit_handler=self._submit_composer_text,
                    command_key_handler=self._handle_composer_command_key,
                    sessions_menu_key_handler=self._handle_sessions_menu_key,
                    copy_or_clear_handler=self.action_copy_or_clear_composer,
                    paste_handler=self._compact_paste_if_needed,
                    placeholder="› 输入消息或 / 命令",
                    id="composer",
                    soft_wrap=True,
                    show_line_numbers=False,
                    highlight_cursor_line=False,
                )
            # 底部单行轮播 HUD：原两行内容合并为一行，遥测（20s）与
            # 工作区路径（10s）交替显示，切换时以解密扫描特效过渡。
            with Horizontal(id="bottom-carousel"):
                yield Static(self._carousel_display_text(), id="carousel-display")

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
        # Agent 回合在线程中结束；通过 call_from_thread 安全更新 Textual 控件。
        set_ask_user_handler = getattr(self.agent, "set_ask_user_handler", None)
        if callable(set_ask_user_handler):
            set_ask_user_handler(self._ask_user)
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
        # 底部单行轮播 HUD：开始遥测/工作区两页交替（各停留 20s/10s）。
        self._carousel_start()

        if not self.startup.startup_ready and callable(
            getattr(self.agent, "preload_mcp_tools", None)
        ):
            # 直接启动 App 的兼容路径：真实入口已在 Splash 阶段完成 MCP
            # 预热，因此不会在可见主界面后再锁住输入。
            self.is_generating = True
            self._preload_mcp_tools()
        else:
            self._set_runtime_status("完成", "complete")
        # 欢迎 Logo 入场动画：挂载完成后启动，仅在空会话首屏播放一次。
        self._start_welcome_logo_animation()
        # 启动参数 --resume 已在 Agent 初始化时恢复会话：按原始事件重建
        # 对话组件，避免用户看到空白会话页或被压成纯文本。
        self._replay_session_conversation()
        for message in self.startup.startup_messages:
            if str(message).strip():
                self._append_message("error", str(message))

    def _start_welcome_logo_animation(self) -> None:
        """启动欢迎 Logo 解密扫描入场动画（仅首次挂载播放一次）。

        已有会话重放会立刻隐藏 Logo（``_hide_welcome_logo`` 停表），
        因此不会在非空会话页误播；恢复/清空会话后再次显示 Logo 时
        保持静态，不再重播。
        """

        if self._logo_anim_started:
            return
        try:
            self.query_one("#welcome-logo", Static)
        except Exception:
            return
        self._logo_anim_started = True
        self._logo_anim_frame = 0
        self._logo_anim_total_frames = max(
            1,
            round(LOGO_ANIM_SECONDS / LOGO_ANIM_FRAME_SECONDS),
        )
        if self._logo_anim_interval is None:
            self._logo_anim_interval = self.set_interval(
                LOGO_ANIM_FRAME_SECONDS,
                self._welcome_logo_animation_tick,
            )
        self._welcome_logo_animation_tick()

    def _welcome_logo_animation_tick(self) -> None:
        """推进一帧 Logo 解密扫描动画；播完落定静态白色 Logo 并停表。"""

        self._logo_anim_frame += 1
        if self._logo_anim_frame >= self._logo_anim_total_frames:
            self._stop_welcome_logo_animation()
            return
        progress = self._logo_anim_frame / self._logo_anim_total_frames
        frame = welcome_logo_frame(progress, rand_source=self._logo_rand)
        try:
            self.query_one("#welcome-logo", Static).update(frame)
        except Exception:
            pass

    def _stop_welcome_logo_animation(self) -> None:
        """停止 Logo 入场动画并落定静态文本；幂等（隐藏/清空时调用）。"""

        interval = getattr(self, "_logo_anim_interval", None)
        if interval is not None:
            try:
                interval.stop()
            except Exception:
                pass
        self._logo_anim_interval = None
        try:
            # 隐藏路径已 display=False，这里统一落定静态白色 Logo，
            # 避免清空会话重新显示时停在乱码中间帧。
            self.query_one("#welcome-logo", Static).update(welcome_logo_text())
        except Exception:
            pass

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


__all__ = ["OmniCrawlApp"]
