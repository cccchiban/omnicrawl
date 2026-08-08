"""全屏工作台可复用的 Textual 展示组件。"""

from __future__ import annotations

import time
from dataclasses import dataclass
from typing import Any

from rich.markdown import Markdown as RichMarkdown
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


_SUBAGENT_TERMINAL_STATUSES = frozenset({"completed", "failed", "cancelled"})
_SUBAGENT_STATUS_PRESENTATION = {
    "queued": ("○", "等待中", "dim"),
    "running": ("●", "运行中", "blue"),
    "waiting_approval": ("◆", "等待审批", "yellow"),
    "completed": ("✓", "完成", "green"),
    "failed": ("×", "失败", "red"),
    "cancelled": ("–", "已取消", "dim"),
}


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
    """支持 Textual 原生鼠标选择且不抢占输入焦点的 AI Markdown 回复。"""

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
        if markdown:
            self.update(markdown)

    def update(self, markdown: str) -> None:
        """用完整 Markdown 重绘当前消息，同时保留 RichLog 的可选区能力。

        渲染前把 LaTeX 公式片段（$..$、$$..$$ 等）转换为终端可读的
        Unicode 数学文本；无公式时走快速路径，不影响流式渲染性能。
        """

        self.clear()
        self.write(RichMarkdown(latex_to_text(markdown)), scroll_end=False)

    def _render_line(self, y: int, scroll_x: int, width: int):
        """给 RichLog 行补充文本坐标，供 Screen 命中鼠标拖选位置。"""

        return super()._render_line(y, scroll_x, width).apply_offsets(scroll_x, y)

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
    """默认展开、可点击折叠且不抢占输入焦点的单次模型思考记录。

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
    }
    """

    STREAM_RENDER_INTERVAL_SECONDS = 0.05
    COLLAPSED_HINT = "思考过程（点击展开）"

    def __init__(self) -> None:
        super().__init__(
            classes="message reasoning-message",
            markup=False,
            wrap=True,
            auto_scroll=False,
        )
        self.reasoning_text = ""  # 完整累积文本（折叠恢复/测试断言依据）
        self.expanded = True
        self._tail = ""  # 尚未以换行结束的未完成行，按节流合并渲染
        self._tail_lines: list[Strip] = []  # 未完成行按当前宽度换行后的行集
        self._last_render_at: float | None = None
        self._render_timer = None  # 挂起的合并刷新定时器（textual Timer）
        self._render_pending = False
        # 挂载前写入会进入 deferred 队列，首次布局完成后自动渲染。
        self.write(Text("思考过程", style="bold"), scroll_end=False)

    def on_resize(self, event: Resize) -> None:
        """首次布局完成后重新计入 deferred 写入和未完成尾行。"""

        super().on_resize(event)
        if self.expanded and (self._tail or self._tail_lines):
            # 重新测量尾行宽度；同时恢复 RichLog.write 对 virtual_size 的覆盖。
            self._render_tail()

    def append_delta(self, delta: str) -> None:
        if not delta:
            return
        self.reasoning_text += delta
        if not self.expanded:
            # 折叠期间不渲染，只累积文本；重新展开时一次补齐。
            self._tail += delta
            return
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

    def on_click(self) -> None:
        self.expanded = not self.expanded
        self.set_class(not self.expanded, "collapsed")
        if self.expanded:
            # 重新展开时立即补齐此前合并挂起的未完成行。
            self._render_tail()
        else:
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
        if y < len(self.lines):
            return super()._render_line(y, scroll_x, width)
        if not self.expanded:
            # 折叠态只显示提示行，不显示思考正文。
            if y == 0:
                return self._to_strip(Text(self.COLLAPSED_HINT)).crop_extend(
                    0, width, self.rich_style
                )
            return Strip.blank(width, self.rich_style)
        tail_index = y - len(self.lines)
        if 0 <= tail_index < len(self._tail_lines):
            return self._tail_lines[tail_index].crop_extend(
                0, width, self.rich_style
            )
        return Strip.blank(width, self.rich_style)


class ToolDisclosure(Static):
    """工具调用记录；写入文件和替换文本默认展开，其他工具默认折叠。"""

    can_focus = False

    def __init__(self, tool_name: str, arguments: Any, started_at: float) -> None:
        expanded_by_default = tool_name in {"write_file", "replace_text"}
        classes = "message tool-message"
        if tool_name == "replace_text":
            classes += " replace-text-message"
        if not expanded_by_default:
            classes += " collapsed"
        super().__init__(classes=classes)
        self.tool_name = tool_name
        self.arguments = arguments
        self.started_at = started_at
        self.status = "调用中"
        self.duration_seconds = 0.0
        self.result_text = ""
        self.expanded = expanded_by_default
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

    def on_click(self) -> None:
        self.expanded = not self.expanded
        self.set_class(not self.expanded, "collapsed")
        self._refresh_display()

    def _refresh_display(self) -> None:
        # 工作区工具与文件变更工具使用统一的短标识 + 上下文摘要；其他工具
        # 保持「参数 + 结果」正文，避免标题泄露完整参数或内部工具协议。
        title = tool_disclosure_title(
            tool_name=self.tool_name,
            arguments=self.arguments,
            status=self.status,
            duration_seconds=self.duration_seconds,
            expanded=self.expanded,
            result_text=self.result_text,
        )
        if not self.expanded:
            self.update(title)
            return
        body = tool_disclosure_body(
            tool_name=self.tool_name,
            arguments=self.arguments,
            result_text=self.result_text,
        )
        rendered = Text()
        rendered.append_text(title)
        if body.plain:
            rendered.append("\n")
            rendered.append_text(body)
        self.update(rendered)
