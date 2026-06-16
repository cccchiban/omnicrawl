"""Qt GUI 对话主循环 — 完整复刻 TUI 全部功能。"""

from __future__ import annotations

import threading

from .agent import AgentError, LocalToolAgent
from .ui.qt.export import save_chat_export
from .slash_commands import (
    format_memory_clean_result,
    format_mcp_status,
    format_skills_list,
    format_tool_confirmation,
    handle_approval_command,
    handle_model_command,
    handle_reasoning_command,
    handle_session_command,
)
from .model_catalog import (
    ModelCatalogError,
    detect_model_options,
    ensure_current_model_option,
    model_options_to_ui,
)
from .speech_playback import StreamingSpeechPlayer
from .text_to_speech import TextToSpeech
from .ui.qt import QtUI


EXIT_WORDS = {"退出", "结束", "再见"}
NEW_CHAT_COMMAND = "/new"


class _QtChatStopped(RuntimeError):
    """Qt 窗口关闭后用于中断本轮后台对话的内部信号。"""


class _QtChatCancelled(RuntimeError):
    """用户点击停止按钮后用于中断当前生成的内部信号。"""


class _QtStatusLine:
    """Qt UI 的 StatusLine 适配器，满足 StreamingSpeechPlayer 接口。"""

    def __init__(self, ui: QtUI) -> None:
        self._ui = ui

    def show(self, text: str) -> None:
        self._ui.status(text)

    def clear(self) -> None:
        self._ui.status("")

    def new_line_for_input(self, prefix: str = "▸") -> None:
        pass


def run_qt_chat(
    agent: LocalToolAgent,
    text_to_speech: TextToSpeech | None,
    ui: QtUI,
    stop_event: threading.Event | None = None,
    cancel_event: threading.Event | None = None,
) -> None:
    """运行 Qt GUI 对话循环，完整复刻 TUI 功能。"""

    stop_event = stop_event or threading.Event()
    cancel_event = cancel_event or threading.Event()

    def stop_requested() -> bool:
        return stop_event.is_set() or ui.is_closed()

    def cancel_requested() -> bool:
        return cancel_event.is_set() or stop_requested()

    def raise_if_stopped() -> None:
        if stop_requested():
            raise _QtChatStopped()
        if cancel_event.is_set():
            raise _QtChatCancelled()

    def raise_if_cancelled() -> None:
        if cancel_event.is_set():
            raise _QtChatCancelled()

    def confirm_tool_call(tool_name, arguments) -> bool:
        raise_if_stopped()
        approved = ui.prompt_yes_no(
            format_tool_confirmation(tool_name, arguments),
            confirmed_label="",
        )
        raise_if_stopped()
        return approved

    agent.set_confirm_handler(confirm_tool_call)

    pending_user_text: str | None = None

    def handle_model_control_text(user_text: str) -> bool:
        """处理 Qt 前端产生的模型控制指令，避免它们进入聊天请求。"""

        if user_text.strip() == "__REFRESH_MODELS__":
            try:
                model_options = ensure_current_model_option(
                    detect_model_options(agent.config.llm),
                    agent.current_model,
                )
                ui.update_model_list(model_options_to_ui(model_options), agent.current_model)
            except ModelCatalogError as exc:
                ui.show_model_list_error(str(exc))
            return True

        reasoning_message = handle_reasoning_command(agent, user_text)
        if reasoning_message is not None:
            ui.notice(reasoning_message)
            return True

        before_model = agent.current_model
        model_message = handle_model_command(agent, user_text)
        if model_message is None:
            return False

        if agent.current_model != before_model:
            ui.set_model_label(agent.current_model)
            ui.set_current_model(agent.current_model)
        else:
            ui.set_current_model(agent.current_model)
        ui.notice(model_message)
        return True

    def handle_export_request(markdown_text: str) -> None:
        """保存前端导出的 Markdown 对话，并给用户明确反馈。"""

        try:
            path = save_chat_export(markdown_text, workspace_root=agent.workspace_root)
        except OSError as exc:
            ui.notice(f"导出失败：{exc}")
            return
        ui.notice(f"对话已导出：{path}")

    ui.export_requested.connect(handle_export_request)

    while True:
        if stop_requested():
            break

        try:
            if pending_user_text is not None:
                user_text = pending_user_text
                pending_user_text = None
            else:
                # 启用输入框，等待用户输入
                ui.set_input_enabled(True)
                ui.set_input_placeholder("输入消息，Enter 发送 · 退出词结束对话")
                user_text = None
                while user_text is None and not stop_requested():
                    user_text = ui.wait_for_input(timeout=0.1)
        except Exception:
            continue

        if user_text is None:
            continue

        # 窗口关闭信号
        if user_text == "__WINDOW_CLOSED__":
            stop_event.set()
            break

        if not user_text:
            continue

        if user_text.strip().lower() in EXIT_WORDS:
            ui.notice("对话结束。")
            break

        if user_text.strip() == NEW_CHAT_COMMAND:
            agent.reset_conversation()
            ui.notice("已开启新对话。")
            continue

        if user_text.strip() == "/skills":
            ui.write(format_skills_list(agent))
            ui.flush_markdown(None)
            continue

        if user_text.strip() == "/memory:clean":
            ui.write(format_memory_clean_result(agent))
            ui.flush_markdown(None)
            continue

        if user_text.strip() == "/mcp":
            ui.write(format_mcp_status(agent))
            ui.flush_markdown(None)
            continue

        session_message = handle_session_command(agent, user_text)
        if session_message is not None:
            ui.write(session_message)
            ui.flush_markdown(None)
            continue

        if handle_model_control_text(user_text):
            continue

        approval_message = handle_approval_command(agent, user_text)
        if approval_message is not None:
            ui.notice(approval_message)
            continue

        # 禁用输入框，防止在模型响应期间重复发送
        ui.set_input_enabled(False)
        ui.set_input_placeholder("等待响应中...")

        ui.inline_turn_base(user_text)
        ui.status("正在思考")

        speech_player = StreamingSpeechPlayer(
            text_to_speech,
            ui,
            status_line=_QtStatusLine(ui),
            input_bar=None,  # Qt 不需要终端 InputBar
        )

        def handle_delta(delta: str) -> None:
            raise_if_cancelled()
            speech_player.handle_delta(delta)

        def handle_agent_status(message: str) -> None:
            raise_if_stopped()
            if message:
                speech_player.flush_display()
                ui.status(message)
            else:
                speech_player.start_new_display_segment()
                ui.status("正在思考")

        def handle_retry_status(message: str) -> None:
            raise_if_stopped()
            speech_player.flush_display()
            ui.status(message, italic=True)

        def handle_tool_start(step: int, tool_call) -> None:
            raise_if_stopped()
            speech_player.flush_display()
            ui.print_tool_call_start(
                step,
                tool_call.name,
                tool_call.arguments,
            )

        def handle_tool_result(_tool_call, result) -> None:
            raise_if_stopped()
            speech_player.flush_display()
            ui.print_tool_result_record(
                result.ok,
                result.output,
                tool_name=_tool_call.name,
            )

        def handle_protocol_wait() -> None:
            speech_player.flush_display()
            ui.status("正在继续")

        try:
            agent.run_stream(
                user_text,
                handle_delta,
                on_status=handle_agent_status,
                on_tool_start=handle_tool_start,
                on_tool_result=handle_tool_result,
                on_token_usage=ui.update_token_usage,
                on_protocol_wait=handle_protocol_wait,
                on_retry_status=handle_retry_status,
            )
            speech_player.flush()
            ui.newline()

            # 朗读完成后的处理
            if text_to_speech is not None:
                ui.set_speaking(True)
                ui.set_input_placeholder("按 Enter 打断朗读，或等待结束...")

                # 简化版等待：在 Qt 中用后台线程等待语音完成
                speech_done = threading.Event()

                def _wait_speech() -> None:
                    text_to_speech.wait_until_done()
                    speech_done.set()

                waiter = threading.Thread(target=_wait_speech, daemon=True)
                waiter.start()

                # 在等待语音的同时，允许用户提前输入下一条消息
                ui.set_input_enabled(True)
                while not speech_done.wait(timeout=0.1):
                    # 非阻塞检查是否有预输入
                    pre_input = ui.wait_for_input(timeout=0.05)
                    if pre_input is not None and pre_input != "__WINDOW_CLOSED__":
                        if handle_model_control_text(pre_input):
                            continue
                        # 用户在朗读期间输入了内容，打断朗读
                        text_to_speech.interrupt(wait_timeout_seconds=0)
                        speech_done.wait(timeout=5.0)
                        ui.notice("已打断朗读。")
                        pending_user_text = pre_input
                        break
                    elif pre_input == "__WINDOW_CLOSED__":
                        stop_event.set()
                        text_to_speech.interrupt(wait_timeout_seconds=0)
                        return

                ui.set_speaking(False)

        except _QtChatStopped:
            if text_to_speech is not None:
                text_to_speech.interrupt(wait_timeout_seconds=0)
            break
        except _QtChatCancelled:
            if text_to_speech is not None:
                text_to_speech.interrupt(wait_timeout_seconds=0)
            cancel_event.clear()
            ui.notice("已取消当前生成。")
            continue
        except KeyboardInterrupt:
            if text_to_speech is not None:
                text_to_speech.interrupt(wait_timeout_seconds=0)
            ui.set_speaking(False)
            ui.notice("已取消当前操作。")
            continue
        except AgentError as exc:
            ui.set_speaking(False)
            message = str(exc).strip() or "Agent 请求失败，请检查配置或稍后重试。"
            ui.notice(message if message.startswith("Agent ") else f"Agent 请求失败：{message}")
            continue
        finally:
            # 清除等待状态
            ui.status("")
            if not stop_requested():
                ui.set_input_enabled(True)
                ui.set_input_placeholder("输入消息，Enter 发送 · 退出词结束对话")
