"""Textual 全屏工作台。"""

from __future__ import annotations

import threading
import time
from dataclasses import dataclass
from typing import Any, Callable

from rich.markdown import Markdown as RichMarkdown
from rich.text import Text
from textual import events, work
from textual.app import App, ComposeResult
from textual.containers import Horizontal, Vertical, VerticalScroll
from textual.widgets import Input, Static

from ...agent import AgentError, LocalToolAgent
from .hud import (
    compact_token_count,
    context_summary_text,
    gradient_text,
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
    handle_model_command,
    handle_reasoning_command,
    handle_session_command,
)
from .commands import CommandDispatcher
from .model_picker import ModelPickerResult, ModelPickerScreen
from .monitor import MonitorStateAdapter, format_monitor_display_batch
from .turns import AgentTurnCallbacks, AgentTurnController
from .widgets import ConfirmationScreen, ReasoningDisclosure, ToolDisclosure


@dataclass(frozen=True)
class FullscreenStartup:
    """启动阶段提供给顶部上下文条的只读摘要。"""

    thinking_enabled: bool
    reasoning_effort: str
    approval_label: str
    workspace_label: str
    temp_label: str


class OmniCrawlApp(App[None]):
    """可控全屏渲染的 OmniCrawl 工作台。"""

    TITLE = "OmniCrawl"
    SUB_TITLE = "Developer Workspace"
    CSS = """
    Screen { background: #080b0e; color: #d9e4e8; }
    #shell { height: 1fr; background: #080b0e; }
    /* 顶部第一行：左品牌 / 中上下文 / 右运行态 三栏对齐。
       运行态仅任务活动时显示（方案 3A），空闲不占位。 */
    #topbar { height: 1; padding: 0 1; background: #0a0e12; align: left middle; }
    #brand { width: 18; min-width: 18; max-width: 18; text-style: bold; content-align: left middle; }
    #context-summary {
        width: 1fr;
        min-width: 0;
        color: #66757b;
        content-align: left middle;
        text-overflow: ellipsis;
    }
    #runtime-status {
        display: none;
        width: 16;
        min-width: 16;
        max-width: 16;
        color: #00e5c3;
        content-align: right middle;
        text-style: bold;
        text-overflow: ellipsis;
    }
    /* 第二行 Token 与 topbar 同左边距左对齐。
       height 必须至少为 2：Textual 的 border-bottom 会占用 1 行布局高度，
       若 height=1 则内容区高度被压成 0，导致 IN/OUT/CA/CTX 有 content 但不渲染。 */
    #token-telemetry {
        height: 2;
        padding: 0 1;
        background: #0a0e12;
        color: #72858c;
        content-align: left middle;
        text-overflow: ellipsis;
        border-bottom: solid #16232a;
    }
    #runtime-status.working { color: #39a7ff; }
    #runtime-status.warning { color: #ff5470; }
    #runtime-status.ready { color: #00e5c3; }
    #conversation {
        height: 1fr;
        padding: 1 1;
        scrollbar-color: #1e3943;
        scrollbar-color-hover: #00a9a0;
        scrollbar-background: #080b0e;
    }
    .message { margin: 0; padding: 0 1; background: transparent; border: none; }
    .user-message { color: #a9c7d3; }
    .assistant-message { color: #d9e4e8; }
    .status-message { color: #59676d; }
    .tool-message { color: #f4b860; padding-left: 2; }
    .tool-message:hover { color: #ffd28a; background: #11100d; }
    .tool-message:focus { color: #ffe0a6; }
    .error-message { color: #ff6b82; }
    .reasoning-message { color: #72858c; padding-left: 2; }
    .reasoning-message:hover { color: #39a7ff; background: #0b1115; }
    .reasoning-message:focus { color: #00e5c3; }
    .reasoning-message.collapsed { height: 1; }
    #composer-wrap { height: 3; min-height: 3; background: #0a0e12; border-top: solid #16333b; padding: 0 1; }
    #command-menu {
        display: none;
        height: auto;
        max-height: 8;
        padding: 0 1;
        background: #0b1115;
        color: #8fa4ad;
        border-left: thick #167da3;
    }
    #composer { height: 3; border: none; padding: 0 1; background: #0a0e12; color: #d9e4e8; }
    #composer:focus { border-left: thick #00e5c3; background: #0b1115; }
    """

    BINDINGS = [
        ("ctrl+c", "cancel_or_quit", "取消 / 退出"),
        ("ctrl+l", "clear_conversation", "清空视图"),
        ("escape", "focus_composer", "输入框"),
    ]

    STREAM_RENDER_INTERVAL_SECONDS = 0.05
    MONITOR_POLL_INTERVAL_SECONDS = 0.5
    STATUS_BLINK_INTERVAL_SECONDS = 0.45
    MAX_TOOL_OUTPUT_CHARS = 3_500

    def __init__(self, agent: LocalToolAgent, startup: FullscreenStartup) -> None:
        super().__init__()
        self.agent = agent
        self.startup = startup
        self.is_generating = False
        self.conversation_text = ""
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
        self._stream_message: Static | None = None
        self._stream_markdown = ""
        self._stream_render_pending = False
        self._tool_messages: dict[str, ToolDisclosure] = {}
        self._reasoning_message: ReasoningDisclosure | None = None
        # UI 私有的 Monitor cursor、暂停状态和失败隔离均由无 Textual 的适配器
        # 持有；本应用只安排定时刷新并渲染它返回的结构化事件批次。
        self._monitor_state = MonitorStateAdapter(agent)
        self._input_tokens = 0
        self._output_tokens = 0
        self._cached_input_tokens = 0
        self._runtime_status_text = "完成"
        self._runtime_status_state = "complete"
        self._status_dot_visible = True
        self._command_matches: list[dict[str, str]] = []
        self._command_selection = 0

    def compose(self) -> ComposeResult:
        with Vertical(id="shell"):
            with Horizontal(id="topbar"):
                yield Static(self._gradient_text("◆ OMNICRAWL"), id="brand")
                yield Static(self._context_summary_text(), id="context-summary")
                yield Static("", id="runtime-status")
            yield Static(self._token_telemetry_text(), id="token-telemetry")
            yield VerticalScroll(id="conversation")
            with Vertical(id="composer-wrap"):
                yield Static("", id="command-menu")
                yield Input(placeholder="› 输入消息或 / 命令", id="composer")

    def on_mount(self) -> None:
        self.agent.set_confirm_handler(self._confirm_tool)
        self.query_one("#composer", Input).focus()
        self.set_interval(self.STATUS_BLINK_INTERVAL_SECONDS, self._tick_status_indicator)
        if self._monitor_state.can_schedule_refresh:
            self.set_interval(self.MONITOR_POLL_INTERVAL_SECONDS, self._refresh_monitor_events)

        if callable(getattr(self.agent, "preload_mcp_tools", None)):
            self.is_generating = True
            self._set_runtime_status("等待", "waiting")
            self._preload_mcp_tools()
        else:
            self._set_runtime_status("完成", "complete")

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

    def on_input_changed(self, event: Input.Changed) -> None:
        if event.input.id == "composer":
            self._refresh_command_menu(event.value)

    def on_key(self, event: events.Key) -> None:
        """菜单打开时接管选择键；Enter/Tab 只补全，绝不触发提交。"""

        composer = self.query_one("#composer", Input)
        if not composer.has_focus or not self._command_matches:
            return
        if event.key in {"up", "down"}:
            offset = -1 if event.key == "up" else 1
            self._command_selection = (self._command_selection + offset) % len(self._command_matches)
            self._render_command_menu()
            event.prevent_default()
            event.stop()
        elif event.key in {"enter", "tab"}:
            selected = self._command_matches[self._command_selection]
            composer.value = selected["insert"]
            composer.cursor_position = len(composer.value)
            self._hide_command_menu()
            event.prevent_default()
            event.stop()

    def on_input_submitted(self, event: Input.Submitted) -> None:
        if self._command_matches:
            return
        text = event.value.strip()
        if not text or self.is_generating:
            return
        event.input.value = ""
        self._submit(text)

    def _refresh_command_menu(self, value: str) -> None:
        """根据当前斜杠前缀实时筛选统一命令源，最多展示八项。"""

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
        self._command_matches = matches[:8]
        self._command_selection = 0
        if not self._command_matches:
            self._hide_command_menu()
            return
        self._render_command_menu()

    def _render_command_menu(self) -> None:
        menu = self.query_one("#command-menu", Static)
        lines = Text()
        for index, option in enumerate(self._command_matches):
            marker = "›" if index == self._command_selection else " "
            style = "bold #00e5c3" if index == self._command_selection else "#8fa4ad"
            lines.append(f"{marker} {option['command']}", style=style)
            description = option.get("description", "").strip()
            if description:
                lines.append(f"  · {description}", style="#526872")
            if index < len(self._command_matches) - 1:
                lines.append("\n")
        menu.update(lines)
        menu.display = True
        self.query_one("#composer-wrap").styles.height = 3 + len(self._command_matches)

    def _hide_command_menu(self) -> None:
        self._command_matches = []
        self._command_selection = 0
        menu = self.query_one("#command-menu", Static)
        menu.display = False
        menu.update("")
        self.query_one("#composer-wrap").styles.height = 3

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
        self._tool_messages.clear()
        self._reasoning_message = None
        self._append_message("status", "已清空当前视图，不影响会话历史。")

    def action_cancel_or_quit(self) -> None:
        if self.is_generating:
            self.cancel_pending_turn()
            return
        self.exit()

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

    def _handle_status(self, message: str) -> None:
        if message:
            self._set_runtime_status("等待", "waiting")

    def _handle_tool_start(self, step: int, tool_call: Any) -> None:
        del step  # Agent 仍按步骤回调，但极简 HUD 不展示内部步骤编号。
        self._render_stream_markdown()
        # 工具调用是模型 pass 的明确边界。必须封口此前的回复组件，否则工具
        # 返回后的最终回答会继续写入旧组件，在视觉上倒插到工具记录之前。
        self._stream_message = None
        self._stream_markdown = ""
        self._reasoning_message = None
        conversation = self.query_one("#conversation", VerticalScroll)
        tool_message = ToolDisclosure(
            str(tool_call.name),
            getattr(tool_call, "arguments", None),
            time.perf_counter(),
        )
        self._tool_messages[self._tool_call_key(tool_call)] = tool_message
        conversation.mount(tool_message)
        self.conversation_text += f"{tool_call.name}\n"
        conversation.scroll_end(animate=False)
        self._set_runtime_status("正在调用", "working")

    def _handle_tool_result(self, tool_call: Any, result: Any) -> None:
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
                getattr(tool_call, "arguments", None),
                time.perf_counter(),
            )
            self.query_one("#conversation", VerticalScroll).mount(tool_message)
        tool_message.finish(
            ok=bool(result.ok),
            output=output,
            finished_at=time.perf_counter(),
        )
        self.conversation_text += f"结果  {tool_message.status}\n{output}\n"
        self.query_one("#conversation", VerticalScroll).scroll_end(animate=False)
        self._set_runtime_status("正在思考", "working")

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
        if self._reasoning_message is None:
            self._reasoning_message = ReasoningDisclosure()
            conversation.mount(self._reasoning_message)
        self._reasoning_message.append_delta(delta)
        self._set_runtime_status("正在思考", "working")
        conversation.scroll_end(animate=False)

    def _append_delta(self, delta: str) -> None:
        if not delta:
            return
        self._reasoning_message = None
        self._set_runtime_status("正在回复", "working")
        conversation = self.query_one("#conversation", VerticalScroll)
        if self._stream_message is None:
            self._stream_message = Static("", classes="message assistant-message")
            self._stream_markdown = "◇ "
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
            prefixes = {
                "user": "▸ ",
                "assistant": "◇ ",
                "status": "· ",
                "tool": "⌁ ",
                "error": "△ ",
            }
            prefixed_text = f"{prefixes.get(kind, '· ')}{text}"
            renderable = RichMarkdown(prefixed_text) if kind == "assistant" else Text(prefixed_text)
            widget = Static(renderable, classes=f"message {kind}-message")
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
        conversation.scroll_end(animate=False)

    def _finish_turn(self) -> None:
        self._render_stream_markdown()
        self.is_generating = False
        self._reasoning_message = None
        self._set_runtime_status("完成", "complete")
        self.query_one("#composer", Input).focus()

    def _set_runtime_status(self, text: str, state: str) -> None:
        self._runtime_status_text = text
        self._runtime_status_state = state
        self._status_dot_visible = True
        status = self.query_one("#runtime-status", Static)
        status.set_class(state in {"working", "waiting"}, "working")
        status.set_class(state == "warning", "warning")
        status.set_class(state in {"complete", "ready"}, "ready")
        self._render_status_indicator()

    def _tick_status_indicator(self) -> None:
        """任务运行期间只闪烁状态点，正文和布局保持稳定。"""

        # 模态审批成为当前 Screen 后，主工作台组件不在活动查询树中。此时暂停
        # 闪烁，既避免计时器访问隐藏状态栏，也不干扰 Ctrl+C 的审批取消绑定。
        if len(self.screen_stack) > 1:
            return
        if self._runtime_status_state not in {"working", "waiting"}:
            if not self._status_dot_visible:
                self._status_dot_visible = True
                self._render_status_indicator()
            return
        self._status_dot_visible = not self._status_dot_visible
        self._render_status_indicator()

    def _render_status_indicator(self) -> None:
        status_widgets = self.query("#runtime-status")
        if not status_widgets:
            return
        status = status_widgets.first(Static)
        is_active = self._runtime_status_state in {"working", "waiting"}
        status.display = is_active
        if not is_active:
            # 完成、就绪及其他非活动状态不展示，也不保留固定宽度占位。
            status.update("")
            return
        dot = "●" if self._status_dot_visible else " "
        status.update(f"{dot} {self._runtime_status_text}")

    def _token_telemetry_text(self) -> Text:
        """生成紧凑 Token 遥测；CTX 使用最近请求输入量表示当前上下文占用。"""

        return token_telemetry_text(
            self._input_tokens,
            self._output_tokens,
            self._cached_input_tokens,
            getattr(self.agent, "context_window_tokens", 128_000),
        )

    @staticmethod
    def _compact_token_count(value: int) -> str:
        """兼容原有测试与调用入口。"""

        return compact_token_count(value)

    @staticmethod
    def _gradient_text(text: str) -> Text:
        """兼容原有测试与调用入口。"""

        return gradient_text(text)

    def _context_summary_text(self) -> Text:
        """用短键值字段渲染项目、模型、推理强度和审批模式。"""

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
        )

    def _refresh_context_summary(self) -> None:
        """刷新取消侧栏后的顶部运行上下文。"""

        self.query_one("#context-summary", Static).update(self._context_summary_text())

    def _open_model_picker(self, *, refresh: bool = False) -> None:
        """打开双列模型选择界面；切换成功后刷新 HUD 并清零最近 Token 显示。"""

        def receive(result: ModelPickerResult | None) -> None:
            if result is None:
                self._append_message("status", "已取消模型切换。")
                return
            # 切换后旧模型 token 与新模型上下文上限不应混显。
            self._input_tokens = 0
            self._output_tokens = 0
            self._cached_input_tokens = 0
            self._refresh_context_summary()
            self.query_one("#token-telemetry", Static).update(self._token_telemetry_text())
            message = result.message or f"当前模型已切换为 {result.model}"
            self._append_message("status", message)

        self.push_screen(
            ModelPickerScreen(
                self.agent,
                refresh_on_open=refresh,
            ),
            receive,
        )


def run_fullscreen_tui(agent: LocalToolAgent, startup: FullscreenStartup) -> None:
    """运行默认全屏 TUI。"""

    OmniCrawlApp(agent, startup).run()


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
