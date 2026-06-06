from __future__ import annotations

import os

from .agent import AgentError, LocalToolAgent
from .fullscreen_tui import (
    AssistantPrefixBlinker,
    FullScreenTUI,
    FullScreenWaitingIndicator,
)
from .inline_input import read_line_autocomplete
from .slash_commands import (
    build_slash_commands,
    clean_memory_in_tui,
    format_tool_confirmation,
    format_tool_confirmation_compact,
    print_memory_clean_result,
    print_skills_list,
    show_skills_in_tui,
)
from .speech_playback import FullScreenSpeechPlayer, StreamingSpeechPlayer
from .speech_to_text import SpeechToText, SpeechToTextError
from .terminal_ui import USER_PREFIX, StatusLine, TerminalUI, WaitingIndicator
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


def _get_user_text_fullscreen(
    speech_to_text: SpeechToText | None,
    tui: FullScreenTUI,
) -> str:
    """保留给全屏 TUI 的单轮用户输入读取。"""

    text = tui.read_line()
    if text.strip():
        return text

    if speech_to_text is None:
        return tui.read_line()

    tui.set_status("正在录音...")
    try:
        recognized = speech_to_text.listen_once(tui.set_status).strip()
        tui.set_status("Enter 发送，空 Enter 录音，Ctrl+C 两次退出")
        return recognized
    except SpeechToTextError as exc:
        tui.add_system_message(f"语音识别失败：{exc}")
        tui.set_status("语音识别失败，请改用键盘输入")
        return tui.read_line()


def run_fullscreen_chat(
    agent: LocalToolAgent,
    speech_to_text: SpeechToText | None,
    text_to_speech: TextToSpeech | None,
    *,
    model: str,
    thinking_type: str,
    reasoning_effort: str,
    config_label: str,
) -> None:
    """运行保留的全屏 TUI 对话主循环。"""

    with FullScreenTUI(
        model=model,
        thinking_type=thinking_type,
        reasoning_effort=reasoning_effort,
        config_label=config_label,
        slash_commands=build_slash_commands(agent),
    ) as tui:
        agent.set_confirm_handler(
            lambda tool_name, arguments: tui.confirm_yes_no(
                format_tool_confirmation_compact(tool_name, arguments)
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
                    user_text = _get_user_text_fullscreen(speech_to_text, tui)
            except KeyboardInterrupt:
                input_interrupt_count += 1
                tui.set_input("")
                if input_interrupt_count >= 2:
                    tui.add_system_message("对话结束。")
                    break
                tui.add_system_message("已取消输入，再按一次 Ctrl+C 退出。")
                tui.set_status("已取消输入，再按一次 Ctrl+C 退出")
                continue

            input_interrupt_count = 0
            if not user_text:
                continue

            tui.add_user_message(user_text)
            if user_text.strip().lower() in EXIT_WORDS:
                tui.add_system_message("对话结束。")
                break

            if user_text.strip() == NEW_CHAT_COMMAND:
                agent.reset_conversation()
                tui.add_system_message("已开启新对话。")
                continue

            if user_text.strip() == "/skills":
                show_skills_in_tui(agent, tui)
                continue

            if user_text.strip() == "/memory:clean":
                clean_memory_in_tui(agent, tui)
                continue

            waiting_indicator = FullScreenWaitingIndicator(tui, "AI 正在思考")
            prefix_blinker = AssistantPrefixBlinker(tui)
            speech_player = FullScreenSpeechPlayer(
                text_to_speech,
                tui,
                prefix_blinker,
                before_first_output=lambda: waiting_indicator.stop("AI 正在回复"),
            )

            def handle_agent_status(message: str) -> None:
                if not message:
                    waiting_indicator.start()
                    return
                waiting_indicator.stop()
                tui.add_status_message(message)
                tui.set_status(message)

            try:
                waiting_indicator.start()
                tui.show_thinking_indicator()
                prefix_blinker.start()
                agent.run_stream(
                    user_text,
                    speech_player.handle_delta,
                    on_status=handle_agent_status,
                    on_tool_result=lambda _tool_call, result: tui.add_tool_result_message(result.ok),
                )
                waiting_indicator.stop("正在朗读回复" if text_to_speech is not None else "回复完成")
                speech_player.flush()
                interrupted, buffered_text = speech_player.wait_until_done_or_interrupt()
                if interrupted:
                    tui.add_status_message("已打断朗读。")
                if buffered_text:
                    pending_user_text = buffered_text
            except KeyboardInterrupt:
                waiting_indicator.stop("已取消当前操作")
                prefix_blinker.stop()
                tui.hide_thinking_indicator()
                tui.finish_assistant_message()
                if text_to_speech is not None:
                    text_to_speech.interrupt(wait_timeout_seconds=0)
                tui.add_system_message("已取消当前操作。")
                tui.set_status("Enter 发送，空 Enter 录音，Ctrl+C 两次退出")
                continue
            except AgentError as exc:
                waiting_indicator.stop("Agent 请求失败")
                prefix_blinker.stop()
                tui.hide_thinking_indicator()
                tui.add_system_message(f"Agent 请求失败：{exc}")
                continue


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

        status_line = StatusLine(ui, ui.inline_turn_base(user_text))
        waiting_indicator = WaitingIndicator(status_line)
        speech_player = StreamingSpeechPlayer(
            text_to_speech,
            ui,
            status_line,
            before_first_output=waiting_indicator.stop,
        )

        def handle_agent_status(message: str) -> None:
            if message:
                waiting_indicator.stop()
                ui.status(message)
            else:
                ui.newline()
                waiting_indicator.start()

        try:
            waiting_indicator.start()
            agent.run_stream(
                user_text,
                speech_player.handle_delta,
                on_status=handle_agent_status,
                on_tool_result=lambda _tool_call, result: ui.print_tool_result_record(result.ok),
            )
            waiting_indicator.stop()
            speech_player.flush()
            ui.newline()
            interrupted, buffered_text = speech_player.wait_until_done_or_interrupt()
            if interrupted:
                ui.notice("已打断朗读。")
            if buffered_text:
                pending_user_text = buffered_text
        except KeyboardInterrupt:
            waiting_indicator.stop()
            if text_to_speech is not None:
                text_to_speech.interrupt(wait_timeout_seconds=0)
            ui.newline()
            ui.notice("已取消当前操作。")
            continue
        except AgentError as exc:
            waiting_indicator.stop()
            print(f"Agent 请求失败：{exc}")
            continue
