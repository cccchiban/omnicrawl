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
from textual.containers import Container, Horizontal, Vertical
from textual.events import Click, Resize
from textual.selection import Selection
from textual.screen import ModalScreen
from textual.strip import Strip
from textual.widgets import Button, RichLog, Static

from ....agent.toolkit.tools import ASK_USER_TOOL_NAME
from .latex import latex_to_text
from ..terminal.theme import REASONING_BACKGROUND, REASONING_TEXT, terminal_css
from .tool_diff import (
    FILE_CHANGE_TOOLS,
    tool_disclosure_body,
    tool_disclosure_title,
)


_SUBAGENT_TERMINAL_STATUSES = frozenset({"completed", "failed", "cancelled"})
_SUBAGENT_STATUS_PRESENTATION = {
    "queued": ("○", "等待中", "dim"),
    "running": ("●", "运行中", "blue"),
    "waiting_approval": ("◆", "等待审批", "yellow"),
    "completed": ("✓", "完成", "green"),
    "failed": ("×", "失败", "red"),
    "cancelled": ("–", "已取消", "dim"),
}


def update_static_line(widget: Static, text: Text) -> bool:
    """只重绘高度固定为单行的组件，尺寸不变时不请求重排。

    状态行 spinner、工具行与底部轮播都只换一行文本，而 ``Static.update`` 默认
    ``layout=True``：每次调用都会让 Textual 重排整块消息区，长会话下每次重排的
    成本随已挂载组件数线性增长。这些组件高度恒为一行、宽度由内容决定，因此只在
    显示宽度改变（可能折行）时才请求布局，其余情况只重绘本行；内容与样式跨度都
    未变时连重绘也跳过。返回是否写入了新内容。
    """

    plain = text.plain
    spans = tuple(text.spans)
    if plain == getattr(widget, "_static_line_plain", None) and spans == getattr(
        widget, "_static_line_spans", None
    ):
        return False
    same_width = text.cell_len == getattr(widget, "_static_line_cell_len", -1)
    widget._static_line_plain = plain
    widget._static_line_spans = spans
    widget._static_line_cell_len = text.cell_len
    widget.update(text, layout=not same_width)
    return True


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
        """挂载后重走渲染管线，保证构造期（app 未绑定）的样式正确。

        重绘完成后构造期保存的全文副本不再需要，立即释放以降低长会话
        内存驻留（每条约 24KB，且随会话长度线性累积）。
        """

        super().on_mount()
        if self._last_markdown:
            self.update(self._last_markdown)
        self._last_markdown = ""

    def update(self, markdown: str) -> None:
        """用完整 Markdown 重绘当前消息，同时保留 RichLog 的可选区能力。

        全部 LaTeX 公式（行内/块级/裸公式）统一经 ``latex_to_text`` 转为
        Unicode 近似文本，不依赖任何可选图像渲染依赖。

        该方法会替换当前全部行（含此前增量追加的内容），是全量精确渲染。
        """

        # 只有挂载前需要保留全文副本供 on_mount 重绘；挂载后 update 是即时
        # 渲染，保留副本只会让长会话每条约 24KB 全文线性累积驻留内存。
        if not self.is_mounted:
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
        self._rendered_plain = ""
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
        rendered = self.render_text(now)
        if rendered.plain == self._rendered_plain:
            # 80ms 一次的耗时 tick 大多只有秒级文本会变：渲染结果一致时跳过
            # 重绘，避免长会话下每帧一次的消息区重排。
            return
        self._rendered_plain = rendered.plain
        self.update(rendered)

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
        self._collapsed_height = 0  # 已写入折叠高度的行数（0 = 尚未收口）

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
            # 折叠态不依赖全量重绘：按当前可见行数轻量更新高度并锚定底部，
            # 使思考中流式更新始终落在最新五行（与 _render_markdown 折叠高度
            # 的计算口径一致，避免停顿重绘时高度跳变）。
            self._apply_collapsed_height()
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
            self._collapsed_height = 0
            self.anchor(False)
            self.styles.height = None
        else:
            self._apply_collapsed_height()
        self.refresh()

    def _collapsed_rows(self) -> int:
        """折叠态应占的行数（不超过 ``COLLAPSED_HEIGHT``）。

        ``RichLog.write`` 在组件宽度未知前只把内容入队、并不渲染（Textual
        的 ``_size_known`` 此时为假），``lines`` 因此是空的；若直接按它算
        高度，折叠块会被压成一行——只剩一行背景色——而且首次布局冲刷延迟
        渲染之后也没有人会再把它改回来。宽度未知时退回逻辑行数，等
        ``on_resize`` 拿到真实渲染行后再收口一次。
        """

        rows = len(self.lines) if self._size_known else max(1, self._line_count)
        return min(self.COLLAPSED_HEIGHT, max(1, rows))

    def _apply_collapsed_height(self) -> None:
        """把折叠高度收敛到最新 ``COLLAPSED_HEIGHT`` 行并锚定底部。

        锚定底部（而不是直接 ``scroll_end``）：新高度要到下一次布局才生效，
        直接滚动会按旧高度算出的 ``max_scroll_y`` 停在顶部；``anchor`` 的语义
        是每次重新布局后持续跟随底部，流式重绘与高度变更都会自动滚到最新五行
        （与 ``_scroll_conversation_if_following`` 同机制）。
        """

        if self._expanded:
            return
        target = self._collapsed_rows()
        if self._collapsed_height == target:
            return
        self._collapsed_height = target
        self.styles.height = target
        self.anchor()

    def on_resize(self, event: Resize) -> None:
        """尺寸首次可知时冲刷延迟渲染，并据此重新收口折叠高度。

        ``RichLog.on_resize`` 在第一次拿到非零宽度时才会把 ``write`` 入队的
        内容真正渲染出来。高度必须在这之后重算：思考内容较短、来不及等到
        首次布局就已经 ``flush_tail`` 时，此前按空 ``lines`` 算出的高度会把
        折叠块永远压在「一行背景色」。
        """

        super().on_resize(event)
        self._apply_collapsed_height()

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


class RuntimeStatus(Horizontal):
    """回合运行状态行：左侧动态文本 + 尾部 [ ESC ] 中断提示。

    状态文本（含 Braille 帧）随回合阶段高频重绘，[ ESC ] 是恒定子组件，
    独立接收鼠标悬停与点击；悬停时由 CSS 将 [ ESC ] 置为淡蓝色加粗，
    离开恢复灰色（dim），点击触发与键盘 ESC 相同的宿主取消/聚焦动作，
    让中断入口可发现又不抢占正文注意力。
    """

    can_focus = False

    DEFAULT_CSS = """
    RuntimeStatus {
        height: 1;
        width: auto;
        border-top: none;
        border-bottom: none;
        margin-bottom: 0;
    }
    #runtime-status-label {
        width: auto;
        height: 1;
        text-style: bold;
    }
    #runtime-status-esc-hint {
        width: 8;
        height: 1;
        text-style: bold dim;
        pointer: pointer;
    }
    #runtime-status-esc-hint:hover {
        color: ansi_bright_blue;
        text-style: bold;
    }
    """

    def __init__(self) -> None:
        super().__init__(classes="message runtime-status-message")
        self._label = Static("", id="runtime-status-label")
        # 前导空格是状态正文与提示之间的分隔，挂在提示一侧可避免依赖
        # 动态标签的尾随空格。
        self._hint = Static(" [ ESC ]", id="runtime-status-esc-hint", markup=False)
        self._last_label_plain = ""

    def compose(self) -> ComposeResult:
        yield self._label
        yield self._hint

    def update_status(self, label_text: Text) -> None:
        """更新状态文本（spinner + 状态文字），[ ESC ] 子组件保持不变。

        文本与上一帧一致时直接跳过：``Static.update`` 会使组件布局失效，
        而 spinner 帧由宿主按固定节奏推进，同一帧内的流式分片无需重复重绘；
        文本宽度未变（仅 Braille 帧轮换）时只重绘本行，不重新布局。
        """

        plain = label_text.plain
        if plain == self._last_label_plain:
            return
        self._last_label_plain = plain
        update_static_line(self._label, label_text)

    def on_click(self, event: Click) -> None:
        """点击 [ ESC ] 与键盘 ESC 等价：交给宿主统一的取消/聚焦动作。"""

        if event.chain != 1:
            return
        app = self.app
        action = getattr(app, "action_cancel_or_focus", None)
        if action is not None:
            event.stop()
            action()


class _ToolBodyHint(Static):
    """工具卡省略区中间的可点击提示行（「点击展开 N 行」）。

    默认灰色斜体 + 下划线表示可点击，鼠标悬停变亮蓝（``:hover``）；点击由宿主
    工具卡的 ``expand_body`` 展开被省略的正文。点击事件不再向上冒泡：否则
    同一次点击会继续被卡片的「展开态点击收起」逻辑处理，立刻又折回缩略态。

    下划线只覆盖文字：前导缩进空格单独成段并关闭下划线，否则整行文本样式
    会把下划线画到文字左侧的缩进区。
    """

    can_focus = False

    def __init__(self) -> None:
        super().__init__("", classes="tool-disclosure-hint", markup=False)

    def on_click(self, event: Click) -> None:
        if event.chain != 1:
            return
        expand = getattr(self.parent, "expand_body", None)
        if expand is None:
            return
        event.stop()
        expand()


class ToolDisclosure(Vertical):
    """工具调用记录；正文默认缩略为头尾，省略区留一行可点击提示。

    除 write_file、Edit_file 外，所有工具的正文做头尾采样：不超过五行时
    原样显示；超出时剥离前导空行后保留首尾各两行有效行，中间被省略的行数
    由一行灰色斜体带下划线的「点击展开 N 行」替代（前导缩进不参与下划线）——
    鼠标悬停该行变蓝表示可点击，点击展开完整正文，再点击工具卡即回到缩略态。
    write_file 与 Edit_file 保留完整文件变更预览（Edit_file 的结果区只显示“替换 N 处”
    摘要），read 与写入类记忆工具的正文不展示给终端用户（只保留标题行，
    且没有任何“已隐藏”提示）。

    标题行、正文区与提示行是彼此独立的子组件：展开/收起只重绘正文区，不
    重建标题，因此终态已释放参数与结果原文的工具卡仍能展开（正文源在
    ``finish`` 渲染时已缓存）。
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
    # 正文被缩略时首部与尾部各保留的有效行数。
    HEAD_BODY_LINES = 2
    TAIL_BODY_LINES = 2
    # 方案6：正文相对标题的缩进宽度（4 空格）。
    BODY_INDENT = "    "
    # 省略区提示行文案：占位符是中间被省略的有效行数。缩进不写进文案——
    # 渲染时以单独一段「不参与下划线」的前导空格补上。
    EXPAND_HINT = "点击展开 {lines} 行"
    # 豁免五行限制的工具：write_file 与 Edit_file 保持完整正文展示；
    # read 与写入类记忆工具已由 tool_disclosure_body 直接隐藏（正文为
    # 空），无需豁免。
    UNLIMITED_TOOL_NAMES = frozenset({"write_file", "Edit_file"})

    # 正文流式渲染：工具完成后按块释放终态正文，让编辑内容逐行出现而不是
    # 一次性铺满。块数与间隔共同决定观感（总时长约 0.36 秒）；行数不足阈值
    # 的短正文没有流式价值，直接显示。
    STREAM_BODY_CHUNKS = 12
    STREAM_BODY_INTERVAL = 0.03
    STREAM_BODY_MIN_LINES = 6

    # 提示行是唯一保留鼠标交互的正文元素：悬停点亮表示可点击，点击交给
    # 卡片的 expand_body；正文与标题仍不参与点击（点击卡片其余部分是
    # 展开态的收起动作）。
    DEFAULT_CSS = terminal_css("""
    ToolDisclosure {
        height: auto;
    }
    ToolDisclosure > .tool-disclosure-hint {
        height: 1;
        width: auto;
        color: $terminal-text-gray;
        text-style: underline italic;
        pointer: pointer;
    }
    ToolDisclosure > .tool-disclosure-hint:hover {
        color: ansi_bright_blue;
    }
    """)

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
        self._expanded = False
        self._title_text = Text()
        self._body_source = Text()
        self._display_text = Text()
        # 最近一次真正渲染进组件的标题纯文本（None 表示尚未渲染过）。
        self._shown_title_plain: str | None = None
        # 流式渲染状态：终态正文分块释放，释放完毕后 _stream_full_source 清空。
        self._stream_full_source: Text | None = None
        self._stream_shown_lines = 0
        self._stream_step_lines = 0
        self._stream_timer: Any = None
        self._title_line = Static("", markup=False)
        self._body_line = Static("", markup=False)
        self._hint_line = _ToolBodyHint()
        self._tail_line = Static("", markup=False)
        self._hint_line.display = False
        self._tail_line.display = False
        self._refresh_display()

    def compose(self) -> ComposeResult:
        yield self._title_line
        yield self._body_line
        yield self._hint_line
        yield self._tail_line

    @property
    def content(self) -> Text:
        """当前显示内容（标题 + 正文区 + 省略提示行）。

        与 ``Static.content`` 同口径：工具卡改为容器后，整块内容改由本属性提供。
        """

        return self._display_text

    @property
    def body_streaming(self) -> bool:
        """终态正文是否仍在分块释放（供调用方在释放结束后恢复滚动）。"""

        return self._stream_full_source is not None

    def finish(
        self,
        *,
        ok: bool,
        output: str,
        finished_at: float,
        stream: bool = False,
    ) -> None:
        if self.tool_name == ASK_USER_TOOL_NAME:
            self.status = "已收到回复" if ok else "已取消"
        else:
            self.status = "成功" if ok else "失败"
        self.duration_seconds = max(0.0, finished_at - self.started_at)
        self.result_text = output
        self._apply_status_class()
        self._refresh_display()
        if stream:
            self._start_body_stream()
        # 终态已渲染进子组件，此后 refresh_elapsed 直接 return 不再重绘；
        # 清空参数与结果原文，避免 read 全文/write_file 大 content/bash 大
        # 输出随历时长滞留（单卡可省数 KB~MB）。展开/收起只重绘正文区，
        # 正文源已在 _body_source 中缓存，不受释放影响。
        self.arguments = None
        self.result_text = ""

    def _start_body_stream(self) -> None:
        """把终态正文改为分块释放（显示层流式）。

        正文源此时已由 ``_refresh_display`` 整体算好；这里先只渲染第一块，
        再按 ``STREAM_BODY_INTERVAL`` 逐步补齐，让编辑内容与长输出逐行出现。
        短正文没有流式价值，直接保持完整显示。
        """

        full = self._body_source
        total = len(full.split("\n"))
        if not full.plain.strip() or total <= self.STREAM_BODY_MIN_LINES:
            return
        self._stream_full_source = full
        self._stream_shown_lines = 0
        self._stream_step_lines = max(1, -(-total // self.STREAM_BODY_CHUNKS))
        self._render_body_stream_step()
        self._arm_body_stream_timer()

    def _arm_body_stream_timer(self) -> None:
        if not self.is_mounted or self._stream_timer is not None:
            return
        self._stream_timer = self.set_interval(
            self.STREAM_BODY_INTERVAL,
            self._advance_body_stream,
        )

    def _advance_body_stream(self) -> None:
        self._render_body_stream_step()

    def _render_body_stream_step(self) -> None:
        """按已释放块数切出正文前缀重绘；补齐后收口为完整正文。"""

        full = self._stream_full_source
        if full is None:
            return
        lines = full.split("\n")
        self._stream_shown_lines = min(
            len(lines),
            self._stream_shown_lines + self._stream_step_lines,
        )
        if self._stream_shown_lines >= len(lines):
            self._finish_body_stream()
            return
        self._body_source = Text("\n").join(lines[: self._stream_shown_lines])
        self._render_body()

    def _cancel_body_stream(self) -> None:
        """放弃未完成的流式释放；调用方随后写入完整正文。"""

        timer = self._stream_timer
        self._stream_timer = None
        self._stream_full_source = None
        self._stream_shown_lines = 0
        self._stream_step_lines = 0
        if timer is not None:
            timer.stop()

    def _finish_body_stream(self) -> None:
        """结束流式释放并把正文恢复为完整源（展开或补齐时调用）。"""

        full = self._stream_full_source
        self._cancel_body_stream()
        if full is not None:
            self._body_source = full
            self._render_body()

    def on_mount(self) -> None:
        # finish 早于挂载（工具结果先于组件入列）时在此补上定时器。
        self._arm_body_stream_timer()

    def update_body(self, output: str) -> None:
        """替换正文区文本，保留已渲染的标题与状态。

        工具输出压缩在卡片收口之后才拿到压缩结果，不能复用 ``finish``：它会按
        新参数重建标题并再次释放结果原文。文件变更类工具的正文由调用参数生成，
        替换会丢掉 diff 预览，因此这类工具保持原正文。
        """

        if self.tool_name.rsplit(".", 1)[-1] in FILE_CHANGE_TOOLS:
            return
        self._cancel_body_stream()
        self._body_source = tool_disclosure_body(
            tool_name=self.tool_name,
            arguments=None,
            result_text=output,
        )
        self._render_body()

    def _apply_status_class(self) -> None:
        """按当前状态切换语义 class（tool-running/ok/fail/pending/cancelled）。"""

        target = self.STATUS_CLASS.get(self.status)
        for name in self.STATUS_CLASS.values():
            self.set_class(name == target, name)

    def refresh_elapsed(self, now: float | None = None) -> None:
        """调用期间实时刷新已耗时；终态记录不再重绘。

        与子代理进度树同语义：只有「调用中」「等待回复」的工具行参与 tick
        刷新，完成后保留 finish() 记录的最终耗时。
        """

        if self.status not in {"调用中", "等待回复"}:
            return
        self.duration_seconds = max(
            0.0,
            (time.perf_counter() if now is None else now) - self.started_at,
        )
        self._refresh_display()

    def expand_body(self) -> None:
        """展开被省略的正文（由省略区提示行点击触发）。"""

        if self._expanded:
            return
        self._expanded = True
        self._finish_body_stream()
        self._render_body()

    def on_click(self, event: Click) -> None:
        """展开态点击工具卡回到缩略状态。

        提示行在展开态不可见，其点击由提示行自己消费并停止冒泡，因此这里
        只处理「展开 → 缩略」这一半。
        """

        if event.chain != 1 or not self._expanded:
            return
        self._expanded = False
        self._render_body()

    def _refresh_display(self) -> None:
        # 工作区工具与文件变更工具使用统一的短标识 + 上下文摘要；其他工具
        # 保持「参数 + 结果」正文，避免标题泄露完整参数或内部工具协议。
        self._title_text = tool_disclosure_title(
            tool_name=self.tool_name,
            arguments=self.arguments,
            status=self.status,
            duration_seconds=self.duration_seconds,
            expanded=True,
            result_text=self.result_text,
        )
        if self._title_text.plain == self._shown_title_plain:
            # 80ms 一次的工具耗时 tick 只在秒位跨秒时才改变标题：标题未变即
            # 整卡内容未变，跳过正文重建与重绘，避免长会话下的消息区重排。
            return
        self._shown_title_plain = self._title_text.plain
        self._body_source = tool_disclosure_body(
            tool_name=self.tool_name,
            arguments=self.arguments,
            result_text=self.result_text,
        )
        update_static_line(self._title_line, self._title_text)
        self._render_body()

    def _render_body(self) -> None:
        """按展开状态重绘正文区。

        缩略态是「首部 + 提示行 + 尾部」，展开态是完整正文；两种状态共用
        同一份正文源，因此展开/收起不重建标题，也不依赖已在终态释放的原始
        参数与结果文本。
        """

        head, hidden_lines, tail = self._body_parts(self._body_source)
        head_text = _indent_body_lines(head, self.BODY_INDENT)
        tail_text = (
            _indent_body_lines(tail, self.BODY_INDENT) if tail is not None else None
        )
        self._body_line.update(head_text)
        self._tail_line.update(tail_text if tail_text is not None else "")
        self._tail_line.display = tail_text is not None
        hint_text = None
        if hidden_lines:
            # 缩进单独成段并关闭下划线：提示行整段由 CSS 加下划线，若缩进并进
            # 同一段文本，下划线会向文字左侧多画 4 格。
            hint_text = Text()
            hint_text.append(self.BODY_INDENT, style="not underline")
            hint_text.append(self.EXPAND_HINT.format(lines=hidden_lines))
        self._hint_line.display = hint_text is not None
        if hint_text is not None:
            self._hint_line.update(hint_text)
        displayed = Text()
        displayed.append_text(self._title_text)
        for part in (head_text, hint_text, tail_text):
            if part is None or not part.plain:
                continue
            displayed.append("\n")
            displayed.append_text(part)
        self._display_text = displayed

    def _body_parts(self, body: Text) -> tuple[Text, int, Text | None]:
        """把正文拆成「保留的首部 / 省略的有效行数 / 保留的尾部」。

        展开态与豁免工具返回完整正文（省略 0 行、无尾部）；缩略态按有效
        （非空）行采样：首尾各留 HEAD/TAIL 行，中间只汇报省略行数，展开
        入口由提示行承载。
        """

        if self._expanded or not self._limit_body_lines:
            return body, 0, None
        parts = body.split("\n")
        if len(parts) <= self.MAX_EXPANDED_BODY_LINES:
            return body, 0, None
        effective = [part for part in parts if part.plain.strip()]
        if len(effective) <= self.MAX_EXPANDED_BODY_LINES:
            return body, 0, None
        head = Text("\n").join(effective[: self.HEAD_BODY_LINES])
        tail = Text("\n").join(effective[-self.TAIL_BODY_LINES :])
        hidden_lines = len(effective) - self.HEAD_BODY_LINES - self.TAIL_BODY_LINES
        return head, hidden_lines, tail
