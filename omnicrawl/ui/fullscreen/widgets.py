"""全屏工作台可复用的 Textual 展示组件。"""

from __future__ import annotations

from typing import Any

from rich.markdown import Markdown as RichMarkdown
from rich.text import Text
from textual.app import ComposeResult
from textual.containers import Container, Horizontal
from textual.selection import Selection
from textual.screen import ModalScreen
from textual.widgets import Button, RichLog, Static

from .theme import terminal_css
from .tool_diff import tool_disclosure_body, tool_disclosure_title


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
        """用完整 Markdown 重绘当前消息，同时保留 RichLog 的可选区能力。"""

        self.clear()
        self.write(RichMarkdown(markdown), scroll_end=False)

    def get_selection(self, selection: Selection) -> tuple[str, str] | None:
        """从 RichLog 的渲染行提取纯文本，供鼠标复制使用。"""

        text = "\n".join(line.text.rstrip() for line in self.lines)
        return selection.extract(text), "\n"


class ReasoningDisclosure(Static):
    """默认展开、可点击折叠且不抢占输入焦点的单次模型思考记录。"""

    can_focus = False

    def __init__(self) -> None:
        super().__init__(classes="message reasoning-message")
        self.reasoning_text = ""
        self.expanded = True
        self._refresh_display()

    def append_delta(self, delta: str) -> None:
        self.reasoning_text += delta
        self._refresh_display()

    def on_click(self) -> None:
        self.expanded = not self.expanded
        self.set_class(not self.expanded, "collapsed")
        self._refresh_display()

    def _refresh_display(self) -> None:
        if self.expanded:
            self.update(RichMarkdown(f"**思考过程**\n\n{self.reasoning_text}"))
        else:
            self.update(Text("思考过程（点击展开）"))


class ToolDisclosure(Static):
    """工具调用记录；写入文件和替换文本默认展开，其他工具默认折叠。"""

    can_focus = False

    def __init__(self, tool_name: str, arguments: Any, started_at: float) -> None:
        expanded_by_default = tool_name in {"write_file", "replace_text"}
        classes = "message tool-message" if expanded_by_default else "message tool-message collapsed"
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
