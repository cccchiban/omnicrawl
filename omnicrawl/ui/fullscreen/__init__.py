"""Textual 全屏工作台。"""

from __future__ import annotations

import threading
from dataclasses import dataclass
from typing import Any, Callable

from rich.markdown import Markdown as RichMarkdown
from rich.text import Text
from textual import work
from textual.app import App, ComposeResult
from textual.containers import Container, Horizontal, Vertical, VerticalScroll
from textual.screen import ModalScreen
from textual.widgets import Button, Footer, Input, Static

from ...agent import AgentError, LocalToolAgent
from ...config.approval import approval_mode_label
from ...commands.slash import (
    format_memory_clean_result,
    format_mcp_status,
    format_skills_list,
    format_tool_confirmation,
    handle_approval_command,
    handle_model_command,
    handle_reasoning_command,
    handle_session_command,
)


@dataclass(frozen=True)
class FullscreenStartup:
    """启动阶段提供给左侧状态栏的只读摘要。"""

    thinking_enabled: bool
    reasoning_effort: str
    approval_label: str
    workspace_label: str
    temp_label: str


class ConfirmationScreen(ModalScreen[bool]):
    """受限工具的全屏模态确认框。"""

    BINDINGS = [("ctrl+c", "cancel_confirmation", "取消")]

    CSS = """
    ConfirmationScreen { align: center middle; background: rgba(8, 12, 20, 0.86); }
    #confirmation-dialog { width: 78; max-height: 24; padding: 1 2; border: solid #ffbd6b; background: #151b2d; }
    #confirmation-title { color: #ffbd6b; text-style: bold; margin-bottom: 1; }
    #confirmation-body { color: #d7e0ff; height: auto; max-height: 14; overflow-y: auto; }
    #confirmation-actions { height: 3; align: right middle; margin-top: 1; }
    #confirmation-actions Button { margin-left: 1; }
    #approve { background: #36c3a1; color: #06120f; }
    #reject { background: #313b55; color: #e7ecff; }
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


class OmniCrawlApp(App[None]):
    """可控全屏渲染的 OmniCrawl 工作台。"""

    TITLE = "OmniCrawl"
    SUB_TITLE = "Developer Workspace"
    CSS = """
    Screen { background: #0b0f18; color: #d7e0ff; }
    #shell { height: 1fr; }
    #sidebar { width: 34; min-width: 28; background: #101727; border-right: solid #263554; padding: 1 1; }
    #brand { color: #f1f5ff; text-style: bold; padding: 0 1 1 1; }
    #brand-mark { color: #73a7ff; }
    .section-title { color: #7f9fd9; text-style: bold; padding: 1 1 0 1; }
    .sidebar-value { color: #dce6ff; padding: 0 1; }
    .sidebar-muted { color: #7080a3; padding: 0 1; }
    #main { width: 1fr; background: #0b0f18; }
    #topbar { height: 4; margin-top: 1; background: #101727; border-bottom: solid #263554; padding: 0 2; }
    #active-title { color: #f1f5ff; text-style: bold; width: 1fr; content-align: left middle; }
    #runtime-status { color: #7cceba; width: 22; content-align: right middle; }
    #runtime-status.working { color: #ffbd6b; }
    #runtime-status.warning { color: #ff8aa1; }
    #runtime-status.ready { color: #7cceba; }
    #conversation { height: 1fr; padding: 1 2; scrollbar-color: #5e7db5; scrollbar-color-hover: #80a6ee; }
    .message { margin-bottom: 1; padding: 0 1; }
    .user-message { border-left: thick #72a7ff; background: #121b2d; color: #e7edff; }
    .assistant-message { border-left: thick #42c8a7; background: #101a20; color: #dceee9; }
    .status-message { border-left: thick #7483a7; color: #9ba8c7; background: #101521; }
    .tool-message { border-left: thick #d2a861; background: #1c1820; color: #f4e6c4; }
    .error-message { border-left: thick #ff6d8d; background: #24151d; color: #ffdce4; }
    #composer-wrap { height: 7; min-height: 7; background: #101727; border-top: solid #263554; padding: 1 2; }
    #composer { border: tall #425f97; background: #0d1422; color: #edf3ff; }
    #composer:focus { border: tall #73a7ff; }
    #hint { color: #7686a8; margin-top: 1; }
    Footer { background: #101727; color: #8fa7d6; }
    """

    BINDINGS = [
        ("ctrl+c", "cancel_or_quit", "取消 / 退出"),
        ("ctrl+l", "clear_conversation", "清空视图"),
        ("escape", "focus_composer", "输入框"),
    ]

    STREAM_RENDER_INTERVAL_SECONDS = 0.05
    MAX_TOOL_OUTPUT_CHARS = 3_500

    def __init__(self, agent: LocalToolAgent, startup: FullscreenStartup) -> None:
        super().__init__()
        self.agent = agent
        self.startup = startup
        self.is_generating = False
        self.conversation_text = ""
        self._cancel_requested = threading.Event()
        self._stream_message: Static | None = None
        self._stream_markdown = ""
        self._stream_render_pending = False
        self._tool_message: Static | None = None

    def compose(self) -> ComposeResult:
        with Horizontal(id="shell"):
            with Vertical(id="sidebar"):
                yield Static("◆ OmniCrawl", id="brand")
                yield Static("WORKSPACE", classes="section-title")
                yield Static(self.startup.workspace_label, classes="sidebar-value", id="workspace-summary")
                yield Static("AGENT", classes="section-title")
                thinking = "已启用" if self.startup.thinking_enabled else "已禁用"
                yield Static(f"思考  {thinking}", classes="sidebar-value", id="thinking-summary")
                yield Static(
                    f"深度  {self.startup.reasoning_effort or '默认'}",
                    classes="sidebar-value",
                    id="reasoning-summary",
                )
                yield Static(
                    f"审批  {self.startup.approval_label}",
                    classes="sidebar-value",
                    id="approval-summary",
                )
                yield Static("RUNTIME", classes="section-title")
                yield Static(f"会话  {self.agent.current_session_id}", classes="sidebar-muted", id="session-summary")
                skill_count = self.agent.skill_manager.count if self.agent.skill_manager else 0
                yield Static(f"Skill  {skill_count} 已加载", classes="sidebar-muted", id="skill-summary")
            with Vertical(id="main"):
                with Horizontal(id="topbar"):
                    yield Static("当前对话", id="active-title")
                    yield Static("就绪", id="runtime-status")
                yield VerticalScroll(id="conversation")
                with Vertical(id="composer-wrap"):
                    yield Input(placeholder="输入消息，Enter 发送；/ 可使用命令", id="composer")
                    yield Static("Enter 发送  ·  Ctrl+C 取消当前任务  ·  Ctrl+L 清空视图", id="hint")
        yield Footer()

    def on_mount(self) -> None:
        self.agent.set_confirm_handler(self._confirm_tool)
        self.query_one("#composer", Input).focus()
        self._append_message("status", "全屏工作台已就绪。")

    def on_input_submitted(self, event: Input.Submitted) -> None:
        text = event.value.strip()
        if not text or self.is_generating:
            return
        event.input.value = ""
        self._submit(text)

    def action_focus_composer(self) -> None:
        self.query_one("#composer", Input).focus()

    def action_clear_conversation(self) -> None:
        if self.is_generating:
            return
        self.query_one("#conversation", VerticalScroll).remove_children()
        self.conversation_text = ""
        self._stream_message = None
        self._stream_markdown = ""
        self._stream_render_pending = False
        self._tool_message = None
        self._append_message("status", "已清空当前视图，不影响会话历史。")

    def action_cancel_or_quit(self) -> None:
        if self.is_generating:
            self.cancel_pending_turn()
            return
        self.exit()

    def cancel_pending_turn(self) -> None:
        """标记当前回合已取消，使工作线程在下一个可中断点退出。"""

        self._cancel_requested.set()
        self._set_runtime_status("正在取消", "warning")

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
        try:
            self.agent.run_stream(
                text,
                lambda delta: self.call_from_thread(self._append_delta, delta),
                on_status=lambda status: self.call_from_thread(self._handle_status, status),
                on_tool_start=lambda step, call: self.call_from_thread(self._handle_tool_start, step, call),
                on_tool_result=lambda call, result: self.call_from_thread(self._handle_tool_result, call, result),
                on_token_usage=lambda incoming, outgoing, cached: self.call_from_thread(
                    self._handle_token_usage, incoming, outgoing, cached
                ),
                on_protocol_wait=lambda: self.call_from_thread(self._handle_status, "正在准备工具调用"),
                on_retry_status=lambda status: self.call_from_thread(self._handle_status, status),
                cancel_check=self._raise_if_cancelled,
            )
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
        refresh_sidebar: bool,
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
            if refresh_sidebar:
                self.call_from_thread(self._refresh_sidebar)
        finally:
            self.call_from_thread(self._finish_turn)

    def _start_slow_command(
        self,
        status: str,
        command: Callable[[], str | None],
        *,
        refresh_sidebar: bool = False,
    ) -> None:
        """锁定输入并安排慢命令，避免在 Textual 主事件循环执行 I/O。"""

        self.is_generating = True
        self._cancel_requested.clear()
        self._append_message("status", status)
        self._set_runtime_status(status, "working")
        self._run_slow_command(command, refresh_sidebar=refresh_sidebar)

    def _raise_if_cancelled(self) -> None:
        if self._cancel_requested.is_set():
            raise KeyboardInterrupt("用户取消当前任务")

    def _handle_command(self, text: str) -> bool:
        if text in {"退出", "结束", "再见"}:
            self.exit()
            return True
        if text == "/new":
            self.agent.reset_conversation()
            self._append_message("status", "已开启新对话。")
            self._refresh_sidebar()
            return True
        if text == "/skills":
            self._append_message("status", format_skills_list(self.agent))
            return True
        if text == "/mcp":
            self._start_slow_command("正在读取 MCP 状态", lambda: format_mcp_status(self.agent))
            return True
        if text == "/memory:clean":
            self._append_message("status", format_memory_clean_result(self.agent))
            return True
        if text == "/workspace" or text.startswith("/workspace "):
            parts = text.split(None, 1)
            if len(parts) == 1 or not parts[1].strip():
                self._append_message("status", f"当前工作区：{self.agent.workspace_root}\n用法：/workspace <新工作区路径>")
            else:
                try:
                    self.agent.switch_workspace(parts[1].strip())
                    self._append_message("status", f"已切换工作区：{self.agent.workspace_root}")
                    self._refresh_sidebar()
                except AgentError as exc:
                    self._append_message("error", f"工作区切换失败：{exc}")
            return True
        session_message = handle_session_command(self.agent, text)
        if session_message is not None:
            self._append_message("status", session_message)
            self._refresh_sidebar()
            return True
        normalized = text.strip().lower()
        if normalized in {"/model", "/models"} or normalized.startswith("/model "):
            self._start_slow_command(
                "正在读取模型列表",
                lambda: handle_model_command(self.agent, text),
                refresh_sidebar=True,
            )
            return True
        approval_message = handle_approval_command(self.agent, text)
        if approval_message is not None:
            self._append_message("status", approval_message)
            self._refresh_sidebar()
            return True
        reasoning_message = handle_reasoning_command(self.agent, text)
        if reasoning_message is not None:
            self._append_message("status", reasoning_message)
            self._refresh_sidebar()
            return True
        return False

    def _confirm_tool(self, tool_name: str, arguments: dict[str, Any]) -> bool:
        """在主线程展示确认框，并允许工作线程在用户取消时立即退出等待。"""

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

    def _handle_status(self, message: str) -> None:
        if message:
            self._set_runtime_status(message, "working")

    def _handle_tool_start(self, step: int, tool_call: Any) -> None:
        self._render_stream_markdown()
        details = ""
        if getattr(tool_call, "arguments", None):
            details = f"\n参数：{tool_call.arguments}"
        self._append_message("tool", f"步骤 {step} · {tool_call.name}{details}", track_tool=True)
        self._set_runtime_status(f"正在执行 {tool_call.name}", "working")

    def _handle_tool_result(self, tool_call: Any, result: Any) -> None:
        marker = "成功" if result.ok else "失败"
        output = str(result.output or "无输出")
        if len(output) > self.MAX_TOOL_OUTPUT_CHARS:
            output = (
                f"{output[:self.MAX_TOOL_OUTPUT_CHARS]}\n"
                f"... 界面展示已截断（原始输出 {len(result.output)} 字符）。"
            )
        result_text = f"结果  {marker}\n{output}"
        if self._tool_message is not None:
            self._tool_message.update(Text(f"{self._tool_message.content}\n{result_text}"))
            self.conversation_text += f"{result_text}\n"
            self.query_one("#conversation", VerticalScroll).scroll_end(animate=False)
        else:
            self._append_message("tool", f"{tool_call.name}\n{result_text}", track_tool=True)
        self._tool_message = None

    def _handle_token_usage(self, incoming: int, outgoing: int, cached: int) -> None:
        self._set_runtime_status(f"in {incoming} · cache {cached} · out {outgoing}", "ready")

    def _append_delta(self, delta: str) -> None:
        if not delta:
            return
        conversation = self.query_one("#conversation", VerticalScroll)
        if self._stream_message is None:
            self._stream_message = Static("", classes="message assistant-message")
            self._stream_markdown = ""
            conversation.mount(self._stream_message)
        self._stream_markdown += delta
        self.conversation_text += f"{delta}\n"
        if not self._stream_render_pending:
            self._stream_render_pending = True
            self.set_timer(self.STREAM_RENDER_INTERVAL_SECONDS, self._render_stream_markdown)
        conversation.scroll_end(animate=False)

    def _render_stream_markdown(self) -> None:
        """合并短时间内的流式分片，避免逐片重解析完整 Markdown。"""

        self._stream_render_pending = False
        if self._stream_message is not None:
            self._stream_message.update(RichMarkdown(self._stream_markdown))

    def _append_message(
        self,
        kind: str,
        text: str,
        *,
        merge_with_previous: bool = False,
        track_tool: bool = False,
    ) -> None:
        conversation = self.query_one("#conversation", VerticalScroll)
        if merge_with_previous and self._stream_message is not None:
            self._stream_markdown += text
            self._stream_message.update(RichMarkdown(self._stream_markdown))
        else:
            renderable = RichMarkdown(text) if kind == "assistant" else Text(text)
            widget = Static(renderable, classes=f"message {kind}-message")
            if merge_with_previous:
                self._stream_message = widget
                self._stream_markdown = text
            else:
                self._stream_message = None
                self._stream_markdown = ""
            conversation.mount(widget)
            self._tool_message = widget if track_tool else None
        self.conversation_text += f"{text}\n"
        conversation.scroll_end(animate=False)

    def _finish_turn(self) -> None:
        self._render_stream_markdown()
        self.is_generating = False
        self._set_runtime_status("就绪", "ready")
        self.query_one("#composer", Input).focus()

    def _set_runtime_status(self, text: str, state: str) -> None:
        status = self.query_one("#runtime-status", Static)
        status.update(text)
        status.set_class(state == "working", "working")
        status.set_class(state == "warning", "warning")
        status.set_class(state == "ready", "ready")

    def _refresh_sidebar(self) -> None:
        reasoning_effort = str(getattr(self.agent, "reasoning_effort", "") or "")
        thinking_enabled = reasoning_effort not in {"none", "disabled"}
        if not reasoning_effort:
            thinking_enabled = self.startup.thinking_enabled
        self.query_one("#workspace-summary", Static).update(str(self.agent.workspace_root))
        self.query_one("#thinking-summary", Static).update(
            f"思考  {'已启用' if thinking_enabled else '已禁用'}"
        )
        self.query_one("#reasoning-summary", Static).update(f"深度  {reasoning_effort or '默认'}")
        self.query_one("#approval-summary", Static).update(
            f"审批  {approval_mode_label(str(self.agent.approval_mode))}"
        )
        self.query_one("#session-summary", Static).update(f"会话  {self.agent.current_session_id}")
        skill_count = self.agent.skill_manager.count if self.agent.skill_manager else 0
        self.query_one("#skill-summary", Static).update(f"Skill  {skill_count} 已加载")


def run_fullscreen_tui(agent: LocalToolAgent, startup: FullscreenStartup) -> None:
    """运行默认全屏 TUI。"""

    OmniCrawlApp(agent, startup).run()


__all__ = ["FullscreenStartup", "OmniCrawlApp", "run_fullscreen_tui"]
