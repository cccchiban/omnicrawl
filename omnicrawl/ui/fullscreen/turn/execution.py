"""回合执行：提交、流式运行、慢命令、审批确认、取消与命令分派。

P3 重构从 ``ui/fullscreen/__init__.py`` 拆出（2026-08-21）：``TurnExecutionMixin``
的方法名、签名与行为与原 ``OmniCrawlApp`` 逐字一致；回合协议回调由
``AgentTurnController``（``ui/fullscreen/turns.py``）转发，事件渲染与状态
指示仍通过 ``self`` 在 MRO 上解析（``RenderingMixin`` 等）。本模块不依赖
Textual 之外的状态，``ui/fullscreen/turn/__init__.py`` 只做再导出。
"""

from __future__ import annotations

import threading
from typing import Any, Callable

from textual import work

from ....agent import AgentError
from ....commands.slash import format_tool_confirmation
from ..input.composer import Composer
from ..rendering.widgets import ConfirmationScreen
from ..support.turns import AgentTurnCallbacks


class TurnExecutionMixin:
    """原 ``OmniCrawlApp`` 的回合执行与命令分派方法。"""

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

    def cancel_pending_turn(self) -> None:
        """标记当前回合已取消，使工作线程在下一个可中断点退出。"""

        self._cancel_requested.set()
        closed = self._turn_controller.cancel()
        self._set_runtime_status("已取消", "complete")
        return closed

    def _submit(self, text: str) -> None:
        if self._handle_command(text):
            return
        # 记录为可回看的发送历史（斜杠命令不记录），供输入框上下键浏览。
        self.query_one("#composer", Composer).history_record(text)
        self.is_generating = True
        self._cancel_requested.clear()
        self._clear_todo_plan()
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
            on_todo_update=lambda payload: self.call_from_thread(
                self._handle_todo_update,
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

        previous_session_id = self.agent.current_session_id
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
        # 会话切换（/resume 成功）后重放新会话的历史消息，再显示命令反馈；
        # 顺序不能颠倒，否则重放内的清空会抹掉刚追加的状态消息。
        if self.agent.current_session_id != previous_session_id:
            self._replay_session_conversation()
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

    def action_cancel_or_focus(self) -> None:
        if self.is_generating:
            self.cancel_pending_turn()
        else:
            self._reset_mouse_interaction_state(
                focus_composer=True,
                rearm_terminal_protocols=True,
            )


__all__ = ["TurnExecutionMixin"]
