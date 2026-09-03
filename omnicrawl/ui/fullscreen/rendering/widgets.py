"""全屏工作台可复用的 Textual 展示组件。"""

from __future__ import annotations

import time
import unicodedata
from dataclasses import dataclass
from typing import Any

from rich.console import Console, ConsoleOptions
from rich.markdown import Markdown as RichMarkdown
from rich.segment import Segment
from rich.style import Style
from rich.text import Text
from textual.app import ComposeResult
from textual.containers import Container, Horizontal
from textual.events import Click, Resize
from textual.geometry import Size
from textual.selection import Selection
from textual.screen import ModalScreen
from textual.strip import Strip
from textual.widgets import Button, RichLog, Static

from ....agent.toolkit.tools import ASK_USER_TOOL_NAME
from .latex import latex_to_text
from ..terminal.theme import REASONING_BACKGROUND, REASONING_TEXT, terminal_css
from .tool_diff import tool_disclosure_body, tool_disclosure_title


_SUBAGENT_TERMINAL_STATUSES = frozenset({"completed", "failed", "cancelled"})
_SUBAGENT_STATUS_PRESENTATION = {
    "queued": ("○", "等待中", "dim"),
    "running": ("●", "运行中", "blue"),
    "waiting_approval": ("◆", "等待审批", "yellow"),
    "completed": ("✓", "完成", "green"),
    "failed": ("×", "失败", "red"),
    "cancelled": ("–", "已取消", "dim"),
}


def _apply_selection_style(
    strip: Strip,
    selection: Selection | None,
    row: int,
    selection_style: Style,
) -> Strip:
    """把 Textual 选区样式叠加到自定义 RichLog 行，保留原始文本样式。"""

    if selection is None or (span := selection.get_span(row)) is None:
        return strip
    start, end = span
    if end == -1:
        end = sum(len(segment.text) for segment in strip)

    position = 0
    rendered: list[Segment] = []
    for segment in strip:
        segment_end = position + len(segment.text)
        selected_start = max(start, position)
        selected_end = min(end, segment_end)
        if selected_start >= selected_end or segment.control:
            rendered.append(segment)
        else:
            left = selected_start - position
            right = selected_end - position
            if left:
                rendered.append(Segment(segment.text[:left], segment.style))
            selected_segment_style = (
                segment.style + selection_style
                if segment.style is not None
                else selection_style
            )
            rendered.append(
                Segment(segment.text[left:right], selected_segment_style)
            )
            if right < len(segment.text):
                rendered.append(Segment(segment.text[right:], segment.style))
        position = segment_end
    return Strip(rendered, strip.cell_length)


@dataclass
class _SubAgentProgressItem:
    """进度树中的单个安全任务节点，不保存 prompt、结果或异常详情。"""

    task_id: str
    agent_type: str
    description: str
    status: str
    first_seen_at: float
    started_at: float | None = None
    finished_at: float | None = None


class ConfirmationScreen(ModalScreen[bool]):
    """受限工具的全屏模态确认框。

    挂载后默认聚焦「允许执行」，直接回车即可运行；←/→ 在
    「允许执行」「拒绝」两个按钮间切换焦点（箭头方向与按钮位置一致），
    Enter/Space 触发当前焦点按钮，Esc 取消，无需依赖鼠标点击。
    """

    BINDINGS = [
        ("escape", "cancel_confirmation", "取消"),
        ("left", "focus_approve", "选择允许"),
        ("right", "focus_reject", "选择拒绝"),
    ]

    CSS = terminal_css("""
    ConfirmationScreen { align: center middle; background: $terminal-overlay; }
    #confirmation-dialog {
        width: 78;
        max-width: 92%;
        max-height: 22;
        padding: 1 2;
        border: solid $terminal-white;
        background: $terminal-surface;
    }
    #confirmation-title { color: $terminal-green; text-style: bold; margin-bottom: 1; }
    #confirmation-body { color: $terminal-text; height: auto; max-height: 13; overflow-y: auto; }
    #confirmation-hint { color: ansi_bright_black; margin-top: 1; }
    #confirmation-actions { height: 3; align: right middle; margin-top: 1; }
    #confirmation-actions Button { min-width: 12; background: $terminal-surface; }
    #confirmation-actions #reject { margin-left: 1; }
    #approve { background: $terminal-surface; color: $terminal-green; }
    #approve:focus { border: tall $terminal-white; }
    #reject { background: $terminal-panel; color: $terminal-text-secondary; }
    #reject:focus { border: tall $terminal-border-strong; }
    """)

    def __init__(self, prompt: str) -> None:
        super().__init__()
        self._prompt = prompt

    def compose(self) -> ComposeResult:
        with Container(id="confirmation-dialog"):
            yield Static("需要确认", id="confirmation-title")
            yield Static(self._prompt, id="confirmation-body")
            yield Static("←/→ 选择操作 · Enter 确认 · Esc 取消", id="confirmation-hint")
            with Horizontal(id="confirmation-actions"):
                yield Button("允许执行", id="approve", variant="success")
                yield Button("拒绝", id="reject", variant="default")

    def on_mount(self) -> None:
        """挂载后预聚焦「允许执行」，回车即可直接运行。"""

        self.query_one("#approve", Button).focus()

    def on_button_pressed(self, event: Button.Pressed) -> None:
        self.dismiss(event.button.id == "approve")

    def action_focus_approve(self) -> None:
        """把键盘焦点移到「允许执行」按钮（当前为左键位）。"""

        self.query_one("#approve", Button).focus()

    def action_focus_reject(self) -> None:
        """把键盘焦点移到「拒绝」按钮（当前为右键位）。"""

        self.query_one("#reject", Button).focus()

    def action_cancel_confirmation(self) -> None:
        """将确认页内的取消请求交给应用回合控制器统一处理。"""

        self.app.cancel_pending_turn()
        self.dismiss(False)


class AssistantMessage(RichLog, can_focus=False):
    """支持 Textual 原生鼠标选择且不抢占输入焦点的 AI Markdown 回复。

    LaTeX 公式统一由 ``latex_to_text`` 转为 Unicode 近似文本（行内
    ``$..$``、块级 ``$$..$$``/``\\[..\\]``、数学 fenced block 与整行
    裸公式均覆盖）。

    流式性能设计：``update`` 会全量重解析整条消息的 Markdown，成本随
    消息长度近似线性增长；因此流式阶段由 ``append_stream_chunk`` 按
    换行边界切成小块增量渲染（成本与该块长度成正比），仅在做全量精确
    重绘（流式停顿、收口、回合结束）时调用 ``update``。
    """

    DEFAULT_CSS = """
    AssistantMessage {
        height: auto;
        padding: 0 1;
        background: transparent;
        overflow-x: hidden;
        overflow-y: hidden;
    }
    """

    def __init__(self, markdown: str = "") -> None:
        super().__init__(
            classes="message assistant-message",
            markup=False,
            wrap=True,
            auto_scroll=False,
        )
        self._last_markdown = ""  # 最近一次完整 Markdown，供挂载后重绘
        # ``◇ `` 显示前缀是否已经写入过渲染内容（首次流式分片补前缀，
        # 全量重绘后复位为已写入，避免重复前缀）。
        self._prefix_written = False
        if markdown:
            self.update(markdown)

    def on_mount(self) -> None:
        """挂载后重走渲染管线，保证构造期（app 未绑定）的样式正确。"""

        super().on_mount()
        if self._last_markdown:
            self.update(self._last_markdown)

    def update(self, markdown: str) -> None:
        """用完整 Markdown 重绘当前消息，同时保留 RichLog 的可选区能力。

        全部 LaTeX 公式（行内/块级/裸公式）统一经 ``latex_to_text`` 转为
        Unicode 近似文本，不依赖任何可选图像渲染依赖。

        该方法会替换当前全部行（含此前增量追加的内容），是全量精确渲染。
        """

        self._last_markdown = markdown
        self._prefix_written = markdown.startswith("◇ ")
        self.clear()
        # ``◇ `` 是工作台给 AssistantMessage 加的显示前缀，不属于 Markdown
        # 内容。数学 fenced 必须从行首开始，因此解析前暂时剥离它；普通
        # Markdown 路径仍使用完整字符串，保持原有显示格式。
        render_markdown = markdown
        display_prefix = ""
        if render_markdown.startswith("◇ "):
            display_prefix = "◇ "
            render_markdown = render_markdown[len(display_prefix) :]
        self.write(
            RichMarkdown(display_prefix + latex_to_text(render_markdown)),
            scroll_end=False,
        )

    def append_stream_chunk(self, markdown: str) -> None:
        """追加一段已就绪的流式内容，渲染成本与该段长度成正比。

        流式输出由调用方按换行边界切成小块后逐块调用本方法，避免每 50ms
        全量重解析整条消息的 Markdown（长消息会达到数百毫秒/次）。
        分块之间的跨块 Markdown 结构（如跨块围栏、加粗）在流式期间可能
        显示为近似结果，停顿/收口时的 ``update`` 全量重绘会修正为精确结果。
        """

        if not markdown:
            return
        if not self._prefix_written:
            self._prefix_written = True
            markdown = "◇ " + markdown
        self.write(RichMarkdown(latex_to_text(markdown)), scroll_end=False)

    def _render_line(self, y: int, scroll_x: int, width: int) -> Strip:
        """给 RichLog 行补充文本坐标，并绘制 Textual 原生选择样式。"""

        strip = super()._render_line(y, scroll_x, width)
        strip = _apply_selection_style(
            strip,
            self.text_selection,
            y,
            self.screen.get_component_rich_style("screen--selection"),
        )
        return strip.apply_offsets(scroll_x, y)

    def get_selection(self, selection: Selection) -> tuple[str, str] | None:
        """从 RichLog 的渲染行提取纯文本，供鼠标复制使用。"""

        text = "\n".join(line.text.rstrip() for line in self.lines)
        return selection.extract(text), "\n"


class SubAgentProgressTree(Static):
    """按批次原地更新的 SubAgent 进度树。"""

    can_focus = False

    def __init__(self, batch_id: str) -> None:
        super().__init__(classes="message subagent-tree-message")
        self.batch_id = batch_id
        self._tasks: dict[str, _SubAgentProgressItem] = {}
        self._task_order: list[str] = []
        self._last_rendered_at = time.perf_counter()
        self._refresh_display(self._last_rendered_at)

    @property
    def is_active(self) -> bool:
        """批次中仍有等待、运行或等待审批的任务时返回 True。"""

        return any(
            task.status not in _SUBAGENT_TERMINAL_STATUSES
            for task in self._tasks.values()
        )

    def update_task(
        self,
        *,
        task_id: str,
        agent_type: str,
        description: str,
        status: str,
        now: float | None = None,
    ) -> None:
        """新增或更新任务节点；终态节点不会被迟到的活动事件回退。"""

        if status not in _SUBAGENT_STATUS_PRESENTATION:
            return
        now = time.perf_counter() if now is None else now
        safe_task_id = str(task_id or "task").strip()[:120] or "task"
        safe_agent_type = " ".join(str(agent_type or "subagent").split())[:80]
        safe_description = " ".join(str(description or safe_task_id).split())[:120]
        task = self._tasks.get(safe_task_id)
        if task is None:
            task = _SubAgentProgressItem(
                task_id=safe_task_id,
                agent_type=safe_agent_type or "subagent",
                description=safe_description or safe_task_id,
                status=status,
                first_seen_at=now,
            )
            self._tasks[safe_task_id] = task
            self._task_order.append(safe_task_id)
        elif task.status in _SUBAGENT_TERMINAL_STATUSES:
            # 后台线程的迟到事件不得让已完成节点重新显示为等待或运行。
            return
        else:
            task.agent_type = safe_agent_type or task.agent_type
            task.description = safe_description or task.description
            task.status = status

        if status in {"running", "waiting_approval"} and task.started_at is None:
            task.started_at = now
        if status in _SUBAGENT_TERMINAL_STATUSES:
            if task.started_at is None:
                task.started_at = task.first_seen_at
            task.finished_at = now

        self._refresh_display(now)

    def refresh_elapsed(self, now: float | None = None) -> None:
        """仅在存在活动任务时刷新运行耗时，避免终态树持续重绘。"""

        if not self.is_active:
            return
        self._refresh_display(time.perf_counter() if now is None else now)

    def render_text(self, now: float | None = None) -> Text:
        """构建当前树的 Rich 文本，供 Textual 渲染和测试复核。"""

        now = self._last_rendered_at if now is None else now
        rendered = Text()
        total = len(self._task_order)
        completed = sum(
            self._tasks[task_id].status == "completed"
            for task_id in self._task_order
        )
        root_label = "◇ 并行子任务" if total > 1 else "◇ 子任务进度"
        rendered.append(root_label, style="bold")
        if total:
            rendered.append(f"  {completed}/{total} 完成", style="dim")

        for index, task_id in enumerate(self._task_order):
            task = self._tasks[task_id]
            icon, status_label, status_style = _SUBAGENT_STATUS_PRESENTATION[
                task.status
            ]
            connector = "└─" if index == total - 1 else "├─"
            rendered.append("\n")
            rendered.append(f"{connector} ", style="dim")
            rendered.append(f"{icon} ", style=status_style)
            rendered.append(task.description)
            rendered.append(f"  {task.agent_type}", style="dim")
            rendered.append(f" · {status_label}", style=status_style)
            if task.started_at is not None:
                ended_at = task.finished_at if task.finished_at is not None else now
                rendered.append(
                    f" · {self._format_elapsed(ended_at - task.started_at)}",
                    style="dim",
                )
        return rendered

    def _refresh_display(self, now: float) -> None:
        self._last_rendered_at = now
        self.update(self.render_text(now))

    @staticmethod
    def _format_elapsed(seconds: float) -> str:
        total_seconds = max(0, int(seconds))
        hours, remainder = divmod(total_seconds, 3600)
        minutes, seconds = divmod(remainder, 60)
        if hours:
            return f"{hours:02d}:{minutes:02d}:{seconds:02d}"
        return f"{minutes:02d}:{seconds:02d}"


class TodoPlan(Static):
    """显示 Agent 自动维护的紧凑执行清单。"""

    can_focus = False

    def __init__(self) -> None:
        super().__init__(classes="todo-plan", id="todo-plan")
        self._items: list[tuple[str, bool]] = []
        self._refresh_display()

    @property
    def items(self) -> tuple[tuple[str, bool], ...]:
        """返回当前清单的只读投影，便于布局和测试使用。"""

        return tuple(self._items)

    @property
    def row_count(self) -> int:
        """计划区占用的紧凑行数；每个步骤恰好一行。"""

        return len(self._items)

    def update_items(self, items: Any) -> None:
        """替换计划内容，过滤空步骤并限制单步长度。"""

        normalized: list[tuple[str, bool]] = []
        if isinstance(items, (list, tuple)):
            for item in items[:20]:
                if not isinstance(item, dict):
                    continue
                text = str(
                    item.get("step")
                    or item.get("description")
                    or item.get("title")
                    or ""
                ).strip()
                if not text:
                    continue
                completed = bool(item.get("completed")) or str(
                    item.get("status") or ""
                ).casefold() in {"completed", "done", "complete"}
                normalized.append((" ".join(text.split())[:240], completed))
        self._items = normalized
        self._refresh_display()

    def render_text(self) -> Text:
        rendered = Text(no_wrap=True, overflow="ellipsis")
        for index, (step, completed) in enumerate(self._items):
            if index:
                rendered.append("\n")
            rendered.append("▣" if completed else "▢", style="green" if completed else "dim")
            rendered.append(" " + step)
        return rendered

    def _refresh_display(self) -> None:
        self.display = bool(self._items)
        self.update(self.render_text())


class SubAgentConversation(Static):
    """左侧 │ 竖线 + 底部 ╰ 圆角转角包裹的子代理会话面板。

    /review 等派生评审流程把子代理对话实时渲染在这里：每一行以 ``│ ``
    前缀，末尾以 ``╰`` 圆角转角收口；相对父代理消息左右各缩进两格
    （margin 0 2），从视觉上把子代理会话嵌套在父对话内部。内容过长时按
    显示宽度手动换行（CJK 双宽），每一行（含续行）都带 ``│ `` 前缀，
    保证左侧竖线从上到下连续，不被折行内容覆盖。
    """

    can_focus = False

    DEFAULT_CSS = """
    SubAgentConversation {
        margin: 0 2;
        padding: 0 1;
        height: auto;
        background: transparent;
        overflow-x: hidden;
    }
    """

    def __init__(self, batch_id: str, agent_type: str) -> None:
        super().__init__(classes="message subagent-conversation")
        self.batch_id = batch_id
        self.agent_type = agent_type
        self._rows: list[tuple[str, str]] = []
        self._finished = False
        self._rows.append(
            (f"◇ {agent_type or 'subagent'} 子代理对话", "bold")
        )
        self._refresh_display()

    @property
    def is_active(self) -> bool:
        return not self._finished

    @property
    def logical_text(self) -> str:
        """返回面板原始逻辑行，不包含按终端宽度产生的软折行。"""

        return "\n".join(text for text, _style in self._rows)

    def append(self, text: str, style: str = "") -> None:
        """追加一行内容；终态面板忽略后续行。"""

        if self._finished:
            return
        self._rows.append((text, style))
        self._refresh_display()

    def finish(self, status: str) -> None:
        """收口面板：追加终态状态行并落 ╰ 圆角转角，不再接受新行。"""

        if self._finished:
            return
        self._finished = True
        self._rows.append((status, "green" if "完成" in status or "成功" in status else "red"))
        self._refresh_display()

    def render_text(self, width: int = 60) -> Text:
        """构建当前面板的 Rich 文本，供 Textual 渲染和测试复核。"""

        rendered = Text()
        for text, style in self._rows:
            for line in _wrap_subagent_line(text, width):
                rendered.append("│ ", style="dim")
                rendered.append(line, style=style or None)
                rendered.append("\n")
        rendered.append("╰", style="dim")
        return rendered

    def _refresh_display(self) -> None:
        self.update(_SubAgentConversationRenderable(self._rows))


def _wrap_subagent_line(text: str, width: int) -> list[str]:
    """按显示宽度换行：保留原始空白，CJK 字符占 2 列。"""

    content_width = max(4, int(width) - 2)  # 预留 "│ " 两列
    wrapped: list[str] = []
    for raw in str(text).splitlines() or [""]:
        current = ""
        current_width = 0
        for char in raw:
            char_width = 2 if unicodedata.east_asian_width(char) in ("W", "F") else 1
            if current_width + char_width > content_width and current:
                wrapped.append(current)
                current = char
                current_width = char_width
            else:
                current += char
                current_width += char_width
        wrapped.append(current)
    return wrapped or [""]


class _SubAgentConversationRenderable:
    """按实际可用宽度手动换行并给每一行补 │ 前缀的渲染内容。"""

    __slots__ = ("_rows",)

    def __init__(self, rows: list[tuple[str, str]]) -> None:
        self._rows = rows

    def __rich_console__(self, console: Console, options: ConsoleOptions):
        width = max(6, int(options.max_width or 80))
        rows = list(self._rows)
        for text, style in rows:
            # rich 15 中 Style("bold") 这类位置参数构造不可用，统一走 parse。
            style_obj = Style.parse(style) if style else None
            for line in _wrap_subagent_line(text, width):
                yield Segment("│ ", style=Style(dim=True))
                yield Segment(f"{line}\n", style=style_obj)
        yield Segment("╰", style=Style(dim=True))


class _UniformGrayMarkdown:
    """保留 RichMarkdown 结构，但把全部前景/背景统一为思考区灰阶样式。

    思考区域移除围栏包裹后仍按 Markdown 解析（标题、列表、行内代码等），
    这里在渲染完成后把所有 segment 强制改为灰色前景与代码块同款灰色背景，
    使整块思考内容在视觉上保持“灰色 Maple Mono 等宽字体 + 灰色底”的观感。
    """

    def __init__(self, markdown: str) -> None:
        self.markdown = markdown
        self._style = Style(color=REASONING_TEXT, bgcolor=REASONING_BACKGROUND)

    def __rich_console__(self, console: Console, options: ConsoleOptions) -> Any:
        for segment in console.render(RichMarkdown(self.markdown), options):
            yield Segment(
                segment.text,
                (segment.style or Style()) + self._style,
                segment.control,
            )


class ReasoningDisclosure(RichLog, can_focus=False):
    """可折叠、不抢占输入焦点的单次模型思考记录。

    思考内容按普通 Markdown 渲染（不再包裹围栏代码块），但视觉上统一
    使用代码块同款灰色背景与灰色前景；Maple Mono 等宽字体由终端自身
    提供，TUI 不逐控件切换字体。鼠标复制思考内容时复制的是渲染后的
    Markdown 正文。

    默认折叠：只展示最新的 ``COLLAPSED_HEIGHT`` 行思考内容；思考中流式
    更新时持续滚动到底部（始终看到最新五行）；点击折叠区展开全部思考，
    再次点击回到折叠（双击保留 Textual 原生「全选」手势）。折叠只改变
    组件显示高度与滚动位置，``reasoning_text`` 始终累积完整内容，会话
    投影与复制不受影响。

    流式性能设计（与主回复同一套策略）：
    - append_delta 按换行边界把原始思考切成小块增量渲染，成本与块长
      成正比，不再逐片全量重绘；
    - 折叠态按增量行数轻量更新高度并锚定底部，不依赖全量重绘；
    - 流式停顿 ``STREAM_SETTLE_SECONDS`` 后（或 flush_tail 收口时）做
      一次全量精确重绘，统一灰色样式并修正折叠高度。
    """

    DEFAULT_CSS = """
    ReasoningDisclosure {
        height: auto;
        min-height: 0;
        /* 思考块不显示内部滚动条（垂直与水平均隐藏）：超高时由消息区
           统一滚动，避免右侧滚动条轨道/底部水平条（黑色长条）出现。 */
        overflow-x: hidden;
        overflow-y: hidden;
    }
    """

    STREAM_RENDER_INTERVAL_SECONDS = 0.05
    # 折叠态展示的最新思考行数。
    COLLAPSED_HEIGHT = 5
    # 流式渲染块的最大长度：超过该长度且不含换行时强制落盘一次，
    # 避免模型长时间输出单段文本时界面长时间无更新。
    STREAM_CHUNK_LIMIT = 512
    # 流式停顿多久后做一次全量精确重绘。
    STREAM_SETTLE_SECONDS = 0.5

    def __init__(self) -> None:
        super().__init__(
            classes="message reasoning-message",
            markup=False,
            wrap=True,
            auto_scroll=False,
        )
        self._chunks: list[str] = []  # 全部原始思考分片（惰性拼接）
        self._nl_count = 0  # 全部分片中的换行总数
        self._ends_newline = True  # 当前完整文本是否以换行结尾
        self._render_buffer = ""  # 尚未落盘的流式缓冲（保留跨行完整性）
        self._last_delta_at = 0.0
        self._expanded = False  # 折叠态默认只显示最新五行；点击展开全部
        self._last_render_at: float | None = None
        self._render_timer = None  # 停顿检查定时器（textual Timer）
        self._render_pending = False

    @property
    def reasoning_text(self) -> str:
        """完整累积文本（模型原始思考）。"""

        return "".join(self._chunks)

    @property
    def _line_count(self) -> int:
        """当前思考文本的逻辑行数（增量累计，避免逐片 splitlines）。"""

        return self._nl_count + (0 if self._ends_newline else 1)

    def on_mount(self) -> None:
        """挂载后重走渲染管线，保证构造期（app 未绑定）的样式正确。"""

        super().on_mount()
        if self.reasoning_text:
            self._render_markdown()

    def append_delta(self, delta: str) -> None:
        if not delta:
            return
        self._chunks.append(delta)
        self._nl_count += delta.count("\n")
        self._ends_newline = delta.endswith("\n")
        self._render_buffer += delta
        if "\n" in self._render_buffer or len(self._render_buffer) >= self.STREAM_CHUNK_LIMIT:
            self._flush_stream_chunk()
        self._last_delta_at = time.monotonic()
        if self._render_timer is not None:
            self._render_timer.stop()
        # 停顿 SETTLE 秒后做一次全量精确重绘（统一灰色样式并修正折叠
        # 高度）；持续流式时该定时器会被每个分片取消并重新安排。
        self._render_timer = self.set_timer(
            self.STREAM_SETTLE_SECONDS,
            self._render_markdown_now,
        )
        self._render_pending = True
        if not self._expanded:
            # 折叠态不依赖全量重绘：按当前已渲染行数轻量更新高度并锚定
            # 底部，使思考中流式更新始终落在最新五行（与 _render_markdown
            # 折叠高度的计算口径一致，避免停顿重绘时高度跳变）。
            self.styles.height = min(self.COLLAPSED_HEIGHT, max(1, len(self.lines)))
            self.anchor()
        self.refresh()

    def _flush_stream_chunk(self) -> None:
        """把已就绪的流式缓冲按块增量渲染（保留跨行完整性）。

        缓冲只在遇到换行（或超过 STREAM_CHUNK_LIMIT）时落盘，且只落盘
        到最后一个换行为止的完整行，未换行的尾部留在缓冲中等待后续分片，
        避免同一行文本被切成多段分片渲染而错行。
        """

        if not self._render_buffer:
            return
        buffer = self._render_buffer
        self._render_buffer = ""
        if self.parent is None:
            return
        head, sep, tail = buffer.rpartition("\n")
        if sep:
            self._render_buffer = tail
            chunk = head + sep
        else:
            chunk = buffer
        if chunk:
            self.write(_UniformGrayMarkdown(latex_to_text(chunk)), scroll_end=False)

    def flush_tail(self) -> None:
        """推理阶段结束时取消挂起刷新并渲染最终 Markdown。

        思考块关闭（首个回复分片、工具调用、回合结束）时调用；同时
        取消可能挂起的定时刷新，避免失效的延时渲染。
        """

        if self._render_timer is not None:
            self._render_timer.stop()
            self._render_timer = None
        self._render_pending = False
        if not self.reasoning_text:
            return
        self._render_markdown()

    def _render_markdown_now(self) -> None:
        self._render_timer = None
        self._render_pending = False
        if self.parent is None:
            # 思考块已被移除（如流中断回滚），不再渲染。
            return
        self._render_markdown()

    def on_click(self, event: Click) -> None:
        """点击折叠区展开全部思考，再次点击回到折叠。

        双击（chain == 2）是 Textual 原生「全选」手势，不参与折叠切换，
        避免连续两次单击把展开的思考又立刻折叠回去。
        """

        if event.chain != 1:
            return
        self._expanded = not self._expanded
        if self.reasoning_text:
            self._render_markdown()

    def _render_markdown(self) -> None:
        """用完整 Markdown 重绘思考内容（不再包裹围栏代码块）。

        流式阶段与 flush_tail 渲染同一份原始 Markdown，不会出现
        `` ``` `` 定界行；最终统一为灰色前景与代码块同款灰色背景。

        折叠态把组件高度固定为「最新五行内容」后滚动到底部，使思考中
        流式更新始终落在最新五行；展开态回退到 CSS 的 ``height: auto``，
        由消息区统一滚动展示全部。
        """

        self._last_render_at = time.monotonic()
        if not self.reasoning_text:
            return
        # 全量重绘会替换全部行（含此前增量追加的内容），残留的流式缓冲
        # 文本已包含在 reasoning_text 中，直接清空。
        self._render_buffer = ""
        self.clear()
        self.write(
            _UniformGrayMarkdown(latex_to_text(self.reasoning_text)),
            scroll_end=False,
        )
        if self._expanded:
            self.anchor(False)
            self.styles.height = None
        else:
            content_rows = max(1, len(self.lines))
            self.styles.height = min(self.COLLAPSED_HEIGHT, content_rows)
            # 锚定底部：新高度要在下一次布局才生效，直接 scroll_end 会按旧
            # 高度算出的 max_scroll_y 停在顶部；anchor 的语义是在每次重新
            # 布局后持续跟随底部，流式重绘与高度变更都会自动滚到最新五行
            # （与 _scroll_conversation_if_following 同机制）。
            self.anchor()
        self.refresh()

    def _render_line(self, y: int, scroll_x: int, width: int) -> Strip:
        """渲染行补充文本坐标，供 Screen 命中鼠标拖选位置。

        RichLog 继承版本的行不带 offset meta，导致 compositor 无法把
        鼠标坐标映射到文本位置（思考内容此前因此不可复制）。
        """

        strip = super()._render_line(y, scroll_x, width)
        strip = _apply_selection_style(
            strip,
            self.text_selection,
            y,
            self.screen.get_component_rich_style("screen--selection"),
        )
        return strip.apply_offsets(scroll_x, y)

    def get_selection(self, selection: Selection) -> tuple[str, str] | None:
        """从 RichLog 渲染行提取纯文本，供鼠标复制使用。"""

        text = "\n".join(line.text.rstrip() for line in self.lines)
        return selection.extract(text), "\n"


def _indent_body_lines(body: Text, prefix: str) -> Text:
    """给正文每一行加统一缩进（方案6：正文相对标题缩进）。"""

    parts = body.split("\n")
    rendered = Text()
    for index, part in enumerate(parts):
        if index:
            rendered.append("\n")
        if part.plain.strip():
            rendered.append(prefix)
        rendered.append_text(part)
    return rendered


class ToolDisclosure(Static):
    """工具调用记录；所有工具默认展开，正文始终可见。

    除 write_file、Edit_file 外，所有工具的展开正文做头尾采样：
    不超过五行时原样显示，超出时剥离前导空行后保留首尾各两行有效行，
    中间直接折叠（不显示任何截断提示行），避免大段工具输出刷屏，同时
    让测试汇总、错误栈尾部等关键信息直接可见；write_file 与 Edit_file
    保留完整文件变更预览（Edit_file 的结果区只显示“替换 N 处”摘要），
    read 与写入类记忆工具的正文不展示给终端用户（只保留标题行，且没有
    任何“已隐藏”提示）。鼠标交互已全面禁用，展开/折叠不再提供切换入口。
    """

    can_focus = False

    # 状态 → 语义 class（方案6：状态色点 + 缩进，无边框）。class 不再驱动
    # 任何边框/背景样式，仅保留状态标签语义；颜色由标题行首 ● 点在
    # tool_disclosure_title 内按状态绘制。ask_user 使用专属状态：提问期间
    # 「等待回复」（实时计时），用户回答后收口为「已收到回复」。
    STATUS_CLASS = {
        "调用中": "tool-running",
        "成功": "tool-ok",
        "失败": "tool-fail",
        "等待确认": "tool-pending",
        "已取消": "tool-cancelled",
        "等待回复": "tool-running",
        "已收到回复": "tool-ok",
    }

    # 工具展开正文的行数上限（不含标题行）。
    MAX_EXPANDED_BODY_LINES = 5
    # 正文被截断时首部与尾部各保留的有效行数。
    HEAD_BODY_LINES = 2
    TAIL_BODY_LINES = 2
    # 方案6：正文相对标题的缩进宽度（4 空格）。
    BODY_INDENT = "    "
    # 豁免五行限制的工具：write_file 与 Edit_file 保持完整正文展示；
    # read 与写入类记忆工具已由 tool_disclosure_body 直接隐藏（正文为
    # 空），无需豁免。
    UNLIMITED_TOOL_NAMES = frozenset({"write_file", "Edit_file"})

    def __init__(self, tool_name: str, arguments: Any, started_at: float) -> None:
        super().__init__(classes="message tool-message tool-running")
        self.tool_name = tool_name
        self.arguments = arguments
        self.started_at = started_at
        self.status = "等待回复" if tool_name == ASK_USER_TOOL_NAME else "调用中"
        self.duration_seconds = 0.0
        self.result_text = ""
        # 除 write_file 与 Edit_file 外的所有工具正文受五行上限约束。
        self._limit_body_lines = tool_name not in self.UNLIMITED_TOOL_NAMES
        self._refresh_display()

    def finish(self, *, ok: bool, output: str, finished_at: float) -> None:
        if self.tool_name == ASK_USER_TOOL_NAME:
            self.status = "已收到回复" if ok else "已取消"
        else:
            self.status = "成功" if ok else "失败"
        self.duration_seconds = max(0.0, finished_at - self.started_at)
        self.result_text = output
        self._apply_status_class()
        self._refresh_display()

    def _apply_status_class(self) -> None:
        """按当前状态切换边框语义色 class（tool-running/ok/fail/pending/cancelled）。"""

        target = self.STATUS_CLASS.get(self.status)
        for name in self.STATUS_CLASS.values():
            self.set_class(name == target, name)

    def refresh_elapsed(self, now: float | None = None) -> None:
        """调用期间实时刷新已耗时；终态记录不再重绘。

        与 SubAgentTree.refresh_elapsed 同语义：只有「调用中」「等待回复」
        的工具行参与 tick 刷新，完成后保留 finish() 记录的最终耗时。
        """

        if self.status not in {"调用中", "等待回复"}:
            return
        self.duration_seconds = max(
            0.0,
            (time.perf_counter() if now is None else now) - self.started_at,
        )
        self._refresh_display()

    def _refresh_display(self) -> None:
        # 工作区工具与文件变更工具使用统一的短标识 + 上下文摘要；其他工具
        # 保持「参数 + 结果」正文，避免标题泄露完整参数或内部工具协议。
        title = tool_disclosure_title(
            tool_name=self.tool_name,
            arguments=self.arguments,
            status=self.status,
            duration_seconds=self.duration_seconds,
            expanded=True,
            result_text=self.result_text,
        )
        body = tool_disclosure_body(
            tool_name=self.tool_name,
            arguments=self.arguments,
            result_text=self.result_text,
        )
        if self._limit_body_lines and body.plain:
            body = self._truncate_body_lines(body)
        rendered = Text()
        rendered.append_text(title)
        if body.plain:
            rendered.append("\n")
            rendered.append_text(_indent_body_lines(body, self.BODY_INDENT))
        self.update(rendered)

    def _truncate_body_lines(self, body: Text) -> Text:
        """把展开正文做头尾采样，中间直接折叠，不显示截断提示。

        剥离前导空行后按有效（非空）行计数：不超过上限时原样返回；
        超出时保留首尾各两行有效行，中间直接折叠（不渲染任何提示行）。
        """

        parts = body.split("\n")
        if len(parts) <= self.MAX_EXPANDED_BODY_LINES:
            return body
        # 空行不计入有效行；前导空行自然被排除在采样之外。
        effective = [part for part in parts if part.plain.strip()]
        if len(effective) <= self.MAX_EXPANDED_BODY_LINES:
            return body
        truncated = Text()
        for part in effective[: self.HEAD_BODY_LINES]:
            truncated.append_text(part)
            truncated.append("\n")
        for part in effective[-self.TAIL_BODY_LINES :]:
            truncated.append("\n")
            truncated.append_text(part)
        return truncated
