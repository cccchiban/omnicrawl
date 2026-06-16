"""Qt GUI 对话主循环 — 完整复刻 TUI 全部功能。"""

from __future__ import annotations

import threading

from .agent import AgentError, LocalToolAgent
from .session import COMPACT_SUMMARY_PREFIX, SessionEvent, SessionIndexEntry
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


def _session_entry_to_ui(entry: SessionIndexEntry, current_session_id: str) -> dict[str, object]:
    """把会话索引转换为 Qt Web 前端使用的轻量 JSON。

    Qt 前端只需要渲染列表和高亮当前会话，不直接读取 `index.json`。
    后端在这里裁剪字段，可以避免把本地完整路径等不必要细节暴露给 UI。
    """

    return {
        "id": entry.session_id,
        "title": entry.title or "未命名会话",
        "updatedAt": entry.updated_at.astimezone().strftime("%Y-%m-%d %H:%M"),
        "messageCount": entry.message_count,
        "current": entry.session_id == current_session_id,
    }


def _session_events_to_ui(events: list[SessionEvent]) -> list[dict[str, object]]:
    """把 JSONL 事件流转换为 Qt 可回放的消息列表。"""

    messages: list[dict[str, object]] = []
    tool_step = 1
    for event in events:
        payload = event.payload
        created_at = event.created_at.astimezone().strftime("%H:%M")
        if event.type == "user_message":
            content = payload.get("content", "")
            if isinstance(content, str) and content.strip():
                messages.append({"type": "user", "content": content, "time": created_at})
        elif event.type == "assistant_message":
            content = payload.get("content", "")
            if isinstance(content, str) and content.strip():
                messages.append({"type": "assistant", "content": content, "time": created_at})
        elif event.type == "compact_summary":
            content = payload.get("content", "")
            if isinstance(content, str) and content.strip():
                messages.append(
                    {
                        "type": "assistant",
                        "content": f"{COMPACT_SUMMARY_PREFIX}{content}",
                        "time": created_at,
                    }
                )
        elif event.type == "tool_call_requested":
            tool = payload.get("tool", "")
            arguments = payload.get("arguments", {})
            if isinstance(tool, str) and tool.strip():
                messages.append(
                    {
                        "type": "tool_start",
                        "step": tool_step,
                        "tool": tool,
                        "arguments": arguments if isinstance(arguments, dict) else {},
                    }
                )
                tool_step += 1
        elif event.type == "tool_result":
            tool = payload.get("tool", "")
            output = payload.get("model_output")
            if not isinstance(output, str) or not output.strip():
                output = payload.get("output_preview")
            if not isinstance(output, str) or not output.strip():
                output = payload.get("output", "")
            artifact_path = payload.get("artifact_path", "")
            if isinstance(artifact_path, str) and artifact_path.strip():
                artifact_hint = f"\n完整输出 artifact：{artifact_path.strip()}"
                output = f"{output}{artifact_hint}" if isinstance(output, str) else artifact_hint.strip()
            ok = payload.get("ok", False)
            if isinstance(tool, str) and isinstance(output, str):
                messages.append(
                    {
                        "type": "tool_result",
                        "tool": tool,
                        "ok": bool(ok),
                        "output": output,
                    }
                )
    return messages


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

    def refresh_session_list() -> None:
        """刷新 Qt 侧边栏会话列表和当前会话标题。"""

        try:
            entries = agent.list_sessions(limit=20)
        except AgentError as exc:
            ui.show_session_list_error(str(exc))
            return

        current_id = agent.current_session_id
        ui.update_session_list([_session_entry_to_ui(entry, current_id) for entry in entries])
        current_entry = next((entry for entry in entries if entry.session_id == current_id), None)
        if current_entry is not None:
            ui.set_current_session(current_entry.session_id, current_entry.title or "未命名会话")

    def render_session(session_id: str) -> None:
        """从 JSONL 事件流重建 Qt 消息区。"""

        events = agent.load_session_events(session_id)
        ui.render_session_messages(_session_events_to_ui(events))

    def resume_session_for_qt(session_id: str) -> None:
        """恢复会话并同步 Qt 消息列表、标题和侧边栏高亮。"""

        state = agent.resume_session(session_id)
        render_session(state.session_id)
        ui.set_current_session(state.session_id, state.title or "未命名会话")
        refresh_session_list()
        ui.notice(f"已恢复会话：{state.title or state.session_id}")

    def handle_session_control_text(user_text: str) -> bool:
        """处理 Qt 会话控制指令，避免它们进入模型请求。"""

        text = user_text.strip()
        normalized = text.lower()
        if text == "__REFRESH_SESSIONS__":
            refresh_session_list()
            return True

        if text.startswith("__RESUME_SESSION__ "):
            session_id = text.split(None, 1)[1].strip()
            try:
                resume_session_for_qt(session_id)
            except AgentError as exc:
                ui.show_session_list_error(str(exc))
            return True

        if text.startswith("__RENAME_SESSION__ "):
            title = text.split(None, 1)[1].strip()
            try:
                state = agent.rename_current_session(title)
            except AgentError as exc:
                ui.notice(f"会话重命名失败：{exc}")
                return True
            ui.set_current_session(state.session_id, state.title or "未命名会话")
            refresh_session_list()
            ui.notice(f"当前会话已重命名为：{state.title}")
            return True

        if normalized == NEW_CHAT_COMMAND:
            agent.reset_conversation()
            ui.render_session_messages([])
            refresh_session_list()
            ui.notice("已开启新对话。")
            return True

        if normalized == "/resume" or normalized.startswith("/resume "):
            parts = text.split(None, 1)
            if len(parts) == 1 or not parts[1].strip():
                ui.notice("用法：/resume <session_id>。可先用 /sessions 查看最近会话。")
                return True
            try:
                resume_session_for_qt(parts[1].strip())
            except AgentError as exc:
                ui.notice(f"会话恢复失败：{exc}")
            return True

        if normalized == "/rename" or normalized.startswith("/rename "):
            message = handle_session_command(agent, text)
            if message is None:
                return False
            refresh_session_list()
            ui.notice(message)
            return True

        if normalized == "/compact":
            message = handle_session_command(agent, text)
            if message is None:
                return False
            refresh_session_list()
            ui.notice(message)
            return True

        if normalized == "/sessions":
            refresh_session_list()
            message = handle_session_command(agent, text)
            if message is not None:
                ui.write(message)
                ui.flush_markdown(None)
            return True

        return False

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
            path = agent.export_current_session_markdown(markdown_text)
        except AgentError as exc:
            ui.notice(f"导出失败：{exc}")
            return
        refresh_session_list()
        ui.notice(f"当前会话已导出：{path}")

    ui.export_requested.connect(handle_export_request)
    refresh_session_list()

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

        if handle_session_control_text(user_text):
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
            refresh_session_list()
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
            refresh_session_list()

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
