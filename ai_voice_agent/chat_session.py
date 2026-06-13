from __future__ import annotations

import os

from .agent import AgentError, LocalToolAgent
from .inline_input import read_line_autocomplete
from .slash_commands import (
    build_slash_commands,
    format_tool_confirmation,
    handle_approval_command,
    print_memory_clean_result,
    print_mcp_status,
    print_skills_list,
)
from .speech_playback import StreamingSpeechPlayer
from .speech_to_text import SpeechToText, SpeechToTextError
from .terminal_ui import USER_PREFIX, InputBar, StatusLine, TerminalUI, WaitingIndicator
from .text_to_speech import TextToSpeech


EXIT_WORDS = {"退出", "结束", "再见"}
NEW_CHAT_COMMAND = "/new"


def _get_user_text(
    speech_to_text: SpeechToText | None,
    ui: TerminalUI,
    slash_commands: list[str] | None = None,
) -> str:
    """读取一轮用户输入。

    直接回车表示开始录音；输入文字则跳过录音，便于在麦克风不可用时继续调试。
    支持斜杠命令 Tab 补全（Windows 下）。
    """

    if speech_to_text is None:
        if os.name == "nt" and slash_commands:
            try:
                import msvcrt
            except ImportError:
                pass
            else:
                return read_line_autocomplete(ui.prompt(), slash_commands, ui).strip()
        return input(ui.prompt()).strip()

    used_autocomplete = False
    if os.name == "nt" and slash_commands:
        try:
            import msvcrt
        except ImportError:
            pass
        else:
            used_autocomplete = True
            text = read_line_autocomplete(ui.prompt(), slash_commands, ui)
            if text:
                return text
            if speech_to_text is None:
                return input(f"{USER_PREFIX} ").strip()

    if not used_autocomplete:
        command = input(ui.prompt()).strip()
        if command:
            return command

        if speech_to_text is None:
            return input(f"{USER_PREFIX} ").strip()

    try:
        return speech_to_text.listen_once(ui.replace_current_input_with_status).strip()
    except SpeechToTextError as exc:
        ui.replace_current_input_with_status(f"语音识别失败：{exc}")
        return input(f"{USER_PREFIX} ").strip()


def run_inline_chat(
    agent: LocalToolAgent,
    speech_to_text: SpeechToText | None,
    text_to_speech: TextToSpeech | None,
    ui: TerminalUI,
) -> None:
    """运行默认的普通终端内联 UI。"""

    agent.set_confirm_handler(
        lambda tool_name, arguments: ui.prompt_yes_no(
            format_tool_confirmation(tool_name, arguments),
            confirmed_label="",
        )
    )
    pending_user_text: str | None = None
    input_interrupt_count = 0
    while True:
        try:
            if pending_user_text is not None:
                user_text = pending_user_text
                pending_user_text = None
            else:
                user_text = _get_user_text(speech_to_text, ui, build_slash_commands(agent))
        except KeyboardInterrupt:
            input_interrupt_count += 1
            if input_interrupt_count >= 2:
                print("\n对话结束。")
                break
            ui.notice("\n已取消输入，再按一次 Ctrl+C 退出。")
            continue

        input_interrupt_count = 0
        if not user_text:
            continue

        if user_text.strip().lower() in EXIT_WORDS:
            print("对话结束。")
            break

        if user_text.strip() == NEW_CHAT_COMMAND:
            agent.reset_conversation()
            ui.notice("已开启新对话。")
            continue

        if user_text.strip() == "/skills":
            print_skills_list(agent)
            continue

        if user_text.strip() == "/memory:clean":
            print_memory_clean_result(agent)
            continue

        if user_text.strip() == "/mcp":
            print_mcp_status(agent)
            continue

        approval_message = handle_approval_command(agent, user_text)
        if approval_message is not None:
            ui.notice(approval_message)
            continue

        ui.inline_turn_base(user_text)
        status_line = StatusLine(ui)
        input_bar = InputBar(ui)
        waiting_indicator = WaitingIndicator(status_line, input_bar=input_bar)

        def _collect_pre_input() -> None:
            """停止 spinner 并收集预输入到 pending_user_text。"""
            nonlocal pending_user_text
            pre = waiting_indicator.stop()
            if pre and pending_user_text is None:
                pending_user_text = pre

        speech_player = StreamingSpeechPlayer(
            text_to_speech,
            ui,
            status_line,
            before_first_output=_collect_pre_input,
            input_bar=input_bar,
        )
        tool_display_state = None

        def handle_agent_status(message: str) -> None:
            if message:
                had_display_output = speech_player.has_display_output
                speech_player.flush_display()
                _collect_pre_input()
                input_bar.push_up()
                ui.status(message, leading_blank=had_display_output)
                input_bar.pop_down()
            else:
                speech_player.start_new_display_segment()
                waiting_indicator.start()

        def handle_retry_status(message: str) -> None:
            """流式连接可恢复中断时，用弱提示说明自动重试，不进入语音播报。"""

            had_display_output = speech_player.has_display_output
            speech_player.flush_display()
            _collect_pre_input()
            input_bar.push_up()
            ui.status(message, leading_blank=had_display_output, italic=True)
            input_bar.pop_down()

        def handle_tool_start(step: int, tool_call) -> None:
            """工具开始执行时立即展示调用详情和运行态标记。"""

            nonlocal tool_display_state
            had_display_output = speech_player.has_display_output
            speech_player.flush_display()
            _collect_pre_input()
            input_bar.push_up()
            tool_display_state = ui.print_tool_call_start(
                step,
                tool_call.name,
                tool_call.arguments,
                leading_blank=had_display_output,
            )
            input_bar.pop_down()

        def handle_tool_result(_tool_call, result) -> None:
            """工具执行完成时先收起等待动画，再输出执行摘要。"""

            nonlocal tool_display_state
            speech_player.flush_display()
            _collect_pre_input()
            input_bar.push_up()
            ui.print_tool_result_record(
                result.ok,
                result.output,
                tool_name=_tool_call.name,
                display_state=tool_display_state,
            )
            input_bar.pop_down()
            tool_display_state = None

        def handle_protocol_wait() -> None:
            """模型已显示进度、正在继续输出隐藏工具协议时恢复等待动画。"""

            speech_player.flush_display()
            waiting_indicator.start()

        try:
            waiting_indicator.start()
            agent.run_stream(
                user_text,
                speech_player.handle_delta,
                on_status=handle_agent_status,
                on_tool_start=handle_tool_start,
                on_tool_result=handle_tool_result,
                on_token_usage=ui.update_token_usage,
                on_protocol_wait=handle_protocol_wait,
                on_retry_status=handle_retry_status,
            )
            _collect_pre_input()
            input_bar.push_up()
            speech_player.flush()
            ui.newline()
            input_bar.pop_down()
            input_bar.clear()
            interrupted, buffered_text = speech_player.wait_until_done_or_interrupt()
            if interrupted:
                ui.notice("已打断朗读。")
            if buffered_text and pending_user_text is None:
                pending_user_text = buffered_text
        except KeyboardInterrupt:
            _collect_pre_input()
            input_bar.push_up()
            if text_to_speech is not None:
                text_to_speech.interrupt(wait_timeout_seconds=0)
            ui.newline()
            input_bar.pop_down()
            input_bar.clear()
            ui.notice("已取消当前操作。")
            continue
        except AgentError as exc:
            _collect_pre_input()
            input_bar.push_up()
            message = str(exc).strip() or "Agent 请求失败，请检查配置或稍后重试。"
            print(message if message.startswith("Agent ") else f"Agent 请求失败：{message}")
            input_bar.pop_down()
            input_bar.clear()
            continue
