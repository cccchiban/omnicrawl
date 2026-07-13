"""全屏工作台可复用的 Textual 展示组件。"""

from __future__ import annotations

from typing import Any

from rich.markdown import Markdown as RichMarkdown
from rich.text import Text
from textual.app import ComposeResult
from textual.containers import Container, Horizontal
from textual.screen import ModalScreen
from textual.widgets import Button, Static

from .tool_diff import tool_disclosure_body, tool_disclosure_title


class ConfirmationScreen(ModalScreen[bool]):
    """受限工具的全屏模态确认框。"""

    BINDINGS = [("ctrl+c", "cancel_confirmation", "取消")]

    CSS = """
    ConfirmationScreen { align: center middle; background: rgba(5, 8, 10, 0.92); }
    #confirmation-dialog {
        width: 78;
        max-width: 92%;
        max-height: 22;
        padding: 1 2;
        border: solid #39a7ff;
        background: #0b1014;
    }
    #confirmation-title { color: #00e5c3; text-style: bold; margin-bottom: 1; }
    #confirmation-body { color: #d9e4e8; height: auto; max-height: 13; overflow-y: auto; }
    #confirmation-actions { height: 3; align: right middle; margin-top: 1; }
    #confirmation-actions Button { margin-left: 1; min-width: 12; }
    #approve { background: #00bfa5; color: #04100e; }
    #approve:focus { border: tall #68f7df; }
    #reject { background: #151c21; color: #aab8bd; }
    #reject:focus { border: tall #59676d; }
    """

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


class ReasoningDisclosure(Static):
    """默认折叠的单次模型思考记录。"""

    can_focus = True

    def __init__(self) -> None:
        super().__init__(classes="message reasoning-message collapsed")
        self.reasoning_text = ""
        self.expanded = False
        self._refresh_display()

    def append_delta(self, delta: str) -> None:
        self.reasoning_text += delta
        self._refresh_display()

    def on_click(self) -> None:
        self.expanded = not self.expanded
        self.set_class(not self.expanded, "collapsed")
        self._refresh_display()

    def _refresh_display(self) -> None:
        marker = "▾" if self.expanded else "▸"
        if self.expanded:
            self.update(RichMarkdown(f"{marker} **思考过程**\n\n{self.reasoning_text}"))
        else:
            self.update(Text(f"{marker} 思考过程（点击展开）"))


class ToolDisclosure(Static):
    """默认折叠的工具调用记录。"""

    can_focus = True

    def __init__(self, tool_name: str, arguments: Any, started_at: float) -> None:
        super().__init__(classes="message tool-message collapsed")
        self.tool_name = tool_name
        self.arguments = arguments
        self.started_at = started_at
        self.status = "调用中"
        self.duration_seconds = 0.0
        self.result_text = ""
        self.expanded = False
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
        # 文件变更工具（write_file / replace_text）走 git 旁注行号 diff 样式；
        # 其他工具保持原有「参数 + 结果」摘要，避免扩大展示协议。
        title = tool_disclosure_title(
            tool_name=self.tool_name,
            arguments=self.arguments,
            status=self.status,
            duration_seconds=self.duration_seconds,
            expanded=self.expanded,
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
