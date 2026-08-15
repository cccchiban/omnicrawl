"""全屏工作台可复用的 Textual 展示组件。"""

from __future__ import annotations

import time
from dataclasses import dataclass
from typing import Any

from rich.markdown import Markdown as RichMarkdown
from rich.segment import Segment
from rich.style import Style
from rich.text import Text
from textual.app import ComposeResult
from textual.containers import Container, Horizontal
from textual.events import Resize
from textual.geometry import Size
from textual.selection import Selection
from textual.screen import ModalScreen
from textual.strip import Strip
from textual.widgets import Button, RichLog, Static

from .latex import latex_to_text
from .theme import terminal_css
from .tool_diff import tool_disclosure_body, tool_disclosure_title
from .theme import TOOL_TEXT


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
    """受限工具的全屏模态确认框。"""

    BINDINGS = [("escape", "cancel_confirmation", "取消")]

    CSS = terminal_css("""
    ConfirmationScreen { align: center middle; background: $terminal-overlay; }
    #confirmation-dialog {
        width: 78;
        max-width: 92%;
        max-height: 22;
        padding: 1 2;
        border: solid $terminal-blue;
        background: $terminal-surface;
    }
    #confirmation-title { color: $terminal-green; text-style: bold; margin-bottom: 1; }
    #confirmation-body { color: $terminal-text; height: auto; max-height: 13; overflow-y: auto; }
    #confirmation-actions { height: 3; align: right middle; margin-top: 1; }
    #confirmation-actions Button { margin-left: 1; min-width: 12; background: $terminal-surface; }
    #approve { background: $terminal-surface; color: $terminal-green; }
    #approve:focus { border: tall $terminal-blue; }
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
            with Horizontal(id="confirmation-actions"):
                yield Button("拒绝", id="reject", variant="default")
                yield Button("允许执行", id="approve", variant="success")

    def on_button_pressed(self, event: Button.Pressed) -> None:
        self.dismiss(event.button.id == "approve")

    def action_cancel_confirmation(self) -> None:
        """将确认页内的取消请求交给应用回合控制器统一处理。"""

        self.app.cancel_pending_turn()
        self.dismiss(False)


class AssistantMessage(RichLog, can_focus=False):
    """支持 Textual 原生鼠标选择且不抢占输入焦点的 AI Markdown 回复。

    LaTeX 公式统一由 ``latex_to_text`` 转为 Unicode 近似文本（行内
    ``$..$``、块级 ``$$..$$``/``\\[..\\]``、数学 fenced block 与整行
    裸公式均覆盖）。
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
        """

        self._last_markdown = markdown
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


class ReasoningDisclosure(RichLog, can_focus=False):
    """始终展开、不抢占输入焦点的单次模型思考记录（无折叠功能）。

    流式性能设计（与主回复流同一套合并节流策略）：
    - 完整行增量提交：append_delta 按换行切分，已完成的行直接写入
      RichLog 只追加新行；历史行不重解析、不重绘（惰性渲染），避免
      旧实现"每个分片都全量重解析整段文本"的 O(N·L) 开销；
    - 未完成行（tail）合并刷新：最多每 STREAM_RENDER_INTERVAL_SECONDS
      渲染一次，分片突发时合并到一次刷新；
    - 渲染用纯 Text 而非 RichMarkdown：思考内容无需 Markdown 解析，
      进一步消除逐分片全量解析瓶颈。
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

    def __init__(self) -> None:
        super().__init__(
            classes="message reasoning-message",
            markup=False,
            wrap=True,
            auto_scroll=False,
        )
        self.reasoning_text = ""  # 完整累积文本（测试断言依据）
        self._tail = ""  # 尚未以换行结束的未完成行，按节流合并渲染
        self._tail_lines: list[Strip] = []  # 未完成行按当前宽度换行后的行集
        self._last_render_at: float | None = None
        self._render_timer = None  # 挂起的合并刷新定时器（textual Timer）
        self._render_pending = False

    def on_resize(self, event: Resize) -> None:
        """首次布局完成后重新计入 deferred 写入和未完成尾行。"""

        super().on_resize(event)
        if self._tail or self._tail_lines:
            # 重新测量尾行宽度；同时恢复 RichLog.write 对 virtual_size 的覆盖。
            self._render_tail()

    def append_delta(self, delta: str) -> None:
        if not delta:
            return
        self.reasoning_text += delta
        self._tail += delta
        while "\n" in self._tail:
            line, self._tail = self._tail.split("\n", 1)
            self.write(Text(line), scroll_end=False)
        if self._tail:
            self._schedule_tail_render()

    def flush_tail(self) -> None:
        """推理阶段结束时同步补齐未完成行，保证展示内容完整。

        思考块关闭（首个回复分片、工具调用、回合结束）时调用；同时
        取消可能挂起的定时刷新，避免失效的延时渲染。
        """

        if self._render_timer is not None:
            self._render_timer.stop()
            self._render_timer = None
        self._render_pending = False
        if self._tail:
            self._render_tail()
        elif self._tail_lines:
            self._tail_lines = []
            self._sync_virtual_size()
            self.refresh()

    def _schedule_tail_render(self) -> None:
        """前缘节流：空闲时立即渲染，忙碌时合并到 50ms 后的延时刷新。"""

        now = time.monotonic()
        if (
            self._last_render_at is None
            or now - self._last_render_at >= self.STREAM_RENDER_INTERVAL_SECONDS
        ):
            self._render_tail()
        elif not self._render_pending:
            self._render_pending = True
            self._render_timer = self.set_timer(
                self.STREAM_RENDER_INTERVAL_SECONDS,
                self._render_tail_now,
            )

    def _render_tail_now(self) -> None:
        self._render_timer = None
        self._render_pending = False
        if self.parent is None:
            # 思考块已被移除（如流中断回滚），不再渲染。
            return
        self._render_tail()

    def _render_tail(self) -> None:
        self._last_render_at = time.monotonic()
        width = self.scrollable_content_region.width
        if width <= 0:
            width = self.min_width  # 尚未完成布局时按最小宽度占位，布局后自动修正
        if self._tail:
            wrapped = Text(self._tail).wrap(self.app.console, width, overflow="fold")
            self._tail_lines = [self._to_strip(line) for line in wrapped]
        else:
            self._tail_lines = []
        self._sync_virtual_size()
        self.refresh()

    def _to_strip(self, text: Text) -> Strip:
        """把 Text 转成与 RichLog 已提交行一致的 Strip，供 _render_line 裁剪。"""

        return Strip(
            list(text.render(self.app.console)),
            cell_length=text.cell_len,
        )

    def _sync_virtual_size(self) -> None:
        """让容器按"已完成行 + 未完成行"计算自然高度，否则 tail 行不可见。"""

        height = len(self.lines) + (len(self._tail_lines) if self._tail_lines else 0)
        self.virtual_size = Size(self._widest_line_width, max(1, height))

    def _render_line(self, y: int, scroll_x: int, width: int) -> Strip:
        """渲染行补充文本坐标，供 Screen 命中鼠标拖选位置。

        RichLog 继承版本的行不带 offset meta，导致 compositor 无法把
        鼠标坐标映射到文本位置（思考内容此前因此不可复制）。
        已提交行与未完成尾行分支统一补 offsets。
        """

        if y < len(self.lines):
            strip = super()._render_line(y, scroll_x, width)
        else:
            tail_index = y - len(self.lines)
            if 0 <= tail_index < len(self._tail_lines):
                strip = self._tail_lines[tail_index].crop_extend(
                    0, width, self.rich_style
                )
            else:
                strip = Strip.blank(width, self.rich_style)
        strip = _apply_selection_style(
            strip,
            self.text_selection,
            y,
            self.screen.get_component_rich_style("screen--selection"),
        )
        return strip.apply_offsets(scroll_x, y)

    def get_selection(self, selection: Selection) -> tuple[str, str] | None:
        """从已提交行 + 未完成尾行提取纯文本，供鼠标复制使用。

        RichLog 继承版本依赖 _render() 返回 Text/Content，而思考块
        _render() 返回 Panel 包装的 RichVisual，提取必然返回 None；
        这里与 AssistantMessage 一致，基于行文本自行拼装。
        """

        lines = [line.text.rstrip() for line in self.lines]
        lines.extend(line.text.rstrip() for line in self._tail_lines)
        return selection.extract("\n".join(lines)), "\n"


class ToolDisclosure(Static):
    """工具调用记录；所有工具默认展开，正文始终可见。

    除 write_file、replace_text 与 search_tools 外，所有工具的展开正文
    做头尾采样：不超过五行时原样显示，超出时剥离前导空行后保留首尾
    各两行有效行，中间以灰色提示行折叠（提示携带有效总行数），避免
    大段工具输出刷屏，同时让测试汇总、错误栈尾部等关键信息直接可见；
    write_file 与 replace_text 保留完整文件变更预览，search_tools 的
    候选工具清单不展示给终端用户（正文完全隐藏，只保留标题行）。鼠标
    交互已全面禁用，展开/折叠不再提供切换入口。
    """

    can_focus = False

    # 工具展开正文的行数上限（不含标题行）。
    MAX_EXPANDED_BODY_LINES = 5
    # 正文被截断时首部与尾部各保留的有效行数。
    HEAD_BODY_LINES = 2
    TAIL_BODY_LINES = 2
    # 正文被截断时替换中间内容的提示行模板（{total} 为有效总行数）。
    TRUNCATION_HINT = "…（共 {total} 行，仅显示首尾各 2 行）"
    # 豁免五行限制的工具：write_file 与 replace_text 保持完整正文展示；
    # search_tools 已由 tool_disclosure_body 直接隐藏（正文为空），无需豁免。
    UNLIMITED_TOOL_NAMES = frozenset({"write_file", "replace_text"})

    def __init__(self, tool_name: str, arguments: Any, started_at: float) -> None:
        super().__init__(classes="message tool-message")
        self.tool_name = tool_name
        self.arguments = arguments
        self.started_at = started_at
        self.status = "调用中"
        self.duration_seconds = 0.0
        self.result_text = ""
        # 除 write_file 与 replace_text 外的所有工具正文受五行上限约束。
        self._limit_body_lines = tool_name not in self.UNLIMITED_TOOL_NAMES
        self._refresh_display()

    def finish(self, *, ok: bool, output: str, finished_at: float) -> None:
        self.status = "成功" if ok else "失败"
        self.duration_seconds = max(0.0, finished_at - self.started_at)
        self.result_text = output
        self._refresh_display()

    def refresh_elapsed(self, now: float | None = None) -> None:
        """调用期间实时刷新已耗时；终态记录不再重绘。

        与 SubAgentTree.refresh_elapsed 同语义：只有「调用中」的工具行
        参与 tick 刷新，完成后保留 finish() 记录的最终耗时。
        """

        if self.status != "调用中":
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
            rendered.append_text(body)
        self.update(rendered)

    def _truncate_body_lines(self, body: Text) -> Text:
        """把展开正文做头尾采样，并追加一行灰色折叠提示。

        剥离前导空行后按有效（非空）行计数：不超过上限时原样返回；
        超出时保留首尾各两行有效行，中间以灰色提示行折叠，提示行携带
        有效总行数，让用户知道还有多少输出被折叠且尾部关键信息可见。
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
        truncated.append(
            self.TRUNCATION_HINT.format(total=len(effective)),
            style=TOOL_TEXT,
        )
        for part in effective[-self.TAIL_BODY_LINES :]:
            truncated.append("\n")
            truncated.append_text(part)
        return truncated
