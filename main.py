from __future__ import annotations

import json
import os
import re
import subprocess
import sys
import threading
from pathlib import Path
from typing import Any, Callable

from agent import AgentConfig, AgentError, LocalToolAgent, UserDeclinedOperation
from fullscreen_tui import (
    AssistantPrefixBlinker,
    FullScreenTUI,
    FullScreenWaitingIndicator,
    supports_fullscreen_tui,
)
from llm import LLMError, load_llm_config
from runtime_config import resolve_config_path
from terminal_ui import USER_PREFIX, StatusLine, TerminalUI, WaitingIndicator
from speech_to_text import MicrophoneInfo, SpeechConfig, SpeechToText, SpeechToTextError
from text_to_speech import TextToSpeech, TextToSpeechError


EXIT_WORDS = {"q", "quit", "exit", "退出", "结束", "再见"}
POWERSHELL_CHILD_ENV = "AI_VOICE_CHAT_IN_POWERSHELL"
TTS_SENTENCE_PATTERN = re.compile(r"(.+?[。！？!?；;\n])")


def _configure_console_encoding() -> None:
    """尽量使用 UTF-8 输出，减少 Windows 命令行中文乱码概率。"""

    for stream in (sys.stdout, sys.stderr):
        if hasattr(stream, "reconfigure"):
            stream.reconfigure(encoding="utf-8")


def _format_tool_confirmation(tool_name: str, arguments: dict[str, Any]) -> str:
    """把工具名和完整参数展示给用户，作为执行前确认依据。"""

    return (
        "Agent 请求执行操作\n"
        f"工具：{tool_name}\n"
        "参数：\n"
        f"{json_dumps(arguments)}"
    )


def json_dumps(value: Any) -> str:
    """统一生成中文友好的 JSON，避免确认弹窗里转义中文。"""

    return json.dumps(value, ensure_ascii=False, indent=2)


def _running_in_powershell_child() -> bool:
    """判断当前进程是否已经是弹出窗口中的真实对话进程。"""

    return os.getenv(POWERSHELL_CHILD_ENV) == "1"


def _launch_in_powershell_window() -> bool:
    """从 IDE 或测试窗口启动时，弹出独立 PowerShell 运行本脚本。

    返回 True 表示已成功启动新窗口，当前进程可以退出；返回 False 表示启动失败，
    主流程会退回到当前窗口继续运行，避免程序直接不可用。
    """

    if os.name != "nt" or _running_in_powershell_child():
        return False

    script_path = Path(__file__).resolve()
    workdir = str(script_path.parent)
    command = (
        f"$env:{POWERSHELL_CHILD_ENV}='1'; "
        "[Console]::OutputEncoding=[System.Text.Encoding]::UTF8; "
        f"& {sys.executable!r} {str(script_path)!r}; "
        "Write-Host ''; "
        "Read-Host '对话已结束，按 Enter 关闭窗口'"
    )

    try:
        subprocess.Popen(
            [
                "powershell.exe",
                "-NoExit",
                "-ExecutionPolicy",
                "Bypass",
                "-Command",
                command,
            ],
            cwd=workdir,
            creationflags=subprocess.CREATE_NEW_CONSOLE,
        )
    except OSError as exc:
        print(f"弹出 PowerShell 窗口失败，将在当前窗口继续运行：{exc}")
        return False

    print("已弹出独立 PowerShell 窗口，请在新窗口中进行语音对话。")
    return True


def _read_mic_device_index_from_env() -> int | None:
    """读取环境变量中的底层麦克风编号，适合高级排查时固定使用同一设备。"""

    raw_index = os.getenv("MIC_DEVICE_INDEX", "").strip()
    if not raw_index:
        return None

    try:
        return int(raw_index)
    except ValueError as exc:
        raise SpeechToTextError("MIC_DEVICE_INDEX 必须是底层麦克风设备编号，例如 22。") from exc


def _print_microphones(microphones: list[MicrophoneInfo]) -> None:
    """展示可录音输入设备，帮助用户避开虚拟设备和错误默认设备。"""

    print("\n检测到多个录音输入设备：")
    for display_index, microphone in enumerate(microphones, start=1):
        print(f"  {display_index}. {microphone.name}")


def _choose_microphone_index() -> int | None:
    """首次运行时让用户选择麦克风；直接回车则沿用系统默认设备。"""

    env_index = _read_mic_device_index_from_env()
    if env_index is not None:
        return env_index

    if os.getenv("MIC_DEVICE_KEYWORD", "").strip():
        return None

    microphones = SpeechToText.list_microphones()
    if len(microphones) <= 1:
        return None

    _print_microphones(microphones)
    choice_to_device_index = {
        display_index: microphone.index
        for display_index, microphone in enumerate(microphones, start=1)
    }

    while True:
        choice = input("请输入要使用的麦克风序号；直接回车使用系统默认麦克风：").strip()
        if not choice:
            return None

        try:
            selected_choice = int(choice)
        except ValueError:
            print("请输入列表中的数字序号。")
            continue

        if selected_choice in choice_to_device_index:
            return choice_to_device_index[selected_choice]

        print("该序号不是可录音输入设备，请重新选择。")


def _create_speech_to_text() -> SpeechToText | None:
    """初始化语音识别；失败时返回 None，让主流程继续支持键盘输入。"""

    try:
        config = SpeechConfig(
            device_index=_choose_microphone_index(),
            device_name_keyword=os.getenv("MIC_DEVICE_KEYWORD") or None,
        )
        speech_to_text = SpeechToText(config)
        print(f"录音设备：{speech_to_text.selected_microphone_label}")
        return speech_to_text
    except SpeechToTextError as exc:
        print(f"语音识别初始化失败：{exc}")
        print("本次会话将改用键盘输入。")
        return None


def _create_text_to_speech() -> TextToSpeech | None:
    """初始化语音播报；失败时返回 None，不影响命令行文字对话。"""

    try:
        text_to_speech = TextToSpeech()
        print(f"语音后端：{text_to_speech.backend_name}")
        return text_to_speech
    except TextToSpeechError as exc:
        print(f"语音播报初始化失败：{exc}")
        print("AI 回复仍会显示在命令行。")
        return None


def _read_line_autocomplete(prompt: str, commands: list[str]) -> str:
    """逐字符读取输入，在输入 / 时实时显示匹配命令。

    Tab 补全当前唯一或最佳匹配，Enter 提交。
    """
    import msvcrt

    print(prompt, end="", flush=True)
    text = ""
    cursor = 0
    matches: list[str] = []
    match_index = 0
    shown_lines = 0

    def _clear_matches() -> None:
        nonlocal shown_lines
        for _ in range(shown_lines):
            print(f"\033[1A\033[2K", end="")
        shown_lines = 0

    def _show_matches() -> None:
        nonlocal shown_lines, match_index
        _clear_matches()
        if matches:
            match_index = min(match_index, len(matches) - 1)
            display = [f"  > {m}" if i == match_index else f"    {m}"
                       for i, m in enumerate(matches[:8])]
            for line in display:
                print(f"\n\033[2K{line}", end="")
            shown_lines = len(display)
            if shown_lines:
                # 移动光标回输入行
                print(f"\033[{shown_lines}A", end="")
                cursor_offset = len(prompt) + cursor
                if cursor_offset > 0:
                    print(f"\033[{cursor_offset}G", end="")
            print(flush=True)

    def _update_matches() -> None:
        nonlocal matches, match_index
        if text.startswith("/") and len(text) >= 1:
            matches = [c for c in commands if c.startswith(text)]
        else:
            matches.clear()
        match_index = 0
        _show_matches()

    while True:
        char = msvcrt.getwch()

        if char in {"\r", "\n"}:
            _clear_matches()
            print()
            return text.strip()

        if char == "\t":
            if matches:
                text = matches[match_index]
                cursor = len(text)
                match_index = 0
                matches.clear()
                print(f"\r\033[2K{prompt}{text}", end="", flush=True)
                _show_matches()
            continue

        if char == "\x1b":
            # 读取转义序列
            seq_parts: list[str] = []
            import time as _time
            deadline = _time.monotonic() + 0.02
            while _time.monotonic() < deadline:
                if msvcrt.kbhit():
                    seq_parts.append(msvcrt.getwch())
                    if seq_parts[0] in {"[", "O"} and len(seq_parts) >= 2:
                        break
                    if seq_parts[0] not in {"[", "O"}:
                        break
                else:
                    _time.sleep(0.001)
            sequence = "".join(seq_parts)

            if not sequence:
                # 纯 Escape：关闭补全菜单
                matches.clear()
                match_index = 0
                _show_matches()
                continue
            if sequence in {"[A", "OA"}:  # Up
                if matches and match_index > 0:
                    match_index -= 1
                    _show_matches()
                continue
            if sequence in {"[B", "OB"}:  # Down
                if matches and match_index < len(matches) - 1:
                    match_index += 1
                    _show_matches()
                continue
            continue

        if char == "\x03":
            raise KeyboardInterrupt

        if char == "\b":
            if cursor > 0:
                text = text[:cursor - 1] + text[cursor:]
                cursor -= 1
                print(f"\b \b", end="", flush=True)
                # 重绘后续文本
                remaining = text[cursor:]
                print(remaining, end="")
                print(" " * 1, end="")
                print(f"\b" * (len(remaining) + 1), end="", flush=True)
                _update_matches()
            continue

        if char == "\x7f":
            if cursor < len(text):
                text = text[:cursor] + text[cursor + 1:]
                remaining = text[cursor:]
                print(remaining + " ", end="")
                back = len(remaining) + 1
                print(f"\033[{back}D", end="", flush=True)
                _update_matches()
            continue

        if char.isprintable() or char.isspace():
            text = text[:cursor] + char + text[cursor:]
            cursor += 1
            print(char, end="", flush=True)
            remaining = text[cursor:]
            if remaining:
                print(remaining, end="")
                print(f"\033[{len(remaining)}D", end="", flush=True)
            _update_matches()


def _get_user_text(
    speech_to_text: SpeechToText | None,
    ui: TerminalUI,
    slash_commands: list[str] | None = None,
) -> str:
    """读取一轮用户输入。

    直接回车表示开始录音；输入文字则跳过录音，便于在麦克风不可用时继续调试。
    支持斜杠命令 Tab 补全（Windows 下）。
    """

    if os.name == "nt" and slash_commands:
        try:
            import msvcrt
        except ImportError:
            pass
        else:
            text = _read_line_autocomplete(ui.prompt(), slash_commands)
            if text:
                return text

    command = input(ui.prompt()).strip()
    if command:
        return command

    if speech_to_text is None:
        return input(USER_PREFIX).strip()

    try:
        return speech_to_text.listen_once().strip()
    except SpeechToTextError as exc:
        print(f"语音识别失败：{exc}")
        return input(f"{USER_PREFIX}").strip()


def _read_speech_interrupt_keypress(
    input_buffer: list[str],
    before_input: Callable[[], None] | None = None,
    on_empty_enter: Callable[[], None] | None = None,
) -> tuple[bool, str | None]:
    """读取朗读期间的 Windows 控制台按键，Enter 表示打断并提交已输入文本。"""

    if os.name != "nt":
        return False, None

    try:
        import msvcrt
    except ImportError:
        return False, None

    while msvcrt.kbhit():
        char = msvcrt.getwch()
        if char in {"\r", "\n"}:
            text = "".join(input_buffer).strip()
            if text or on_empty_enter is None:
                print()
            else:
                on_empty_enter()
            input_buffer.clear()
            return True, text or None
        elif char in {"\x00", "\xe0"} and msvcrt.kbhit():
            msvcrt.getwch()
        elif char == "\x03":
            raise KeyboardInterrupt
        elif char == "\b":
            if input_buffer:
                input_buffer.pop()
                print("\b \b", end="", flush=True)
        elif char.isprintable() or char.isspace():
            if not input_buffer and before_input is not None:
                before_input()
            input_buffer.append(char)
            print(char, end="", flush=True)

    return False, None


def _read_buffered_console_line(input_buffer: list[str]) -> str:
    """朗读自然结束但用户已开始输入时，继续读取到 Enter，避免吞掉半句输入。"""

    if os.name != "nt":
        text = "".join(input_buffer).strip()
        input_buffer.clear()
        return text

    try:
        import msvcrt
    except ImportError:
        text = "".join(input_buffer).strip()
        input_buffer.clear()
        return text

    while True:
        char = msvcrt.getwch()
        if char in {"\r", "\n"}:
            print()
            text = "".join(input_buffer).strip()
            input_buffer.clear()
            return text
        if char in {"\x00", "\xe0"}:
            msvcrt.getwch()
            continue
        if char == "\x03":
            raise KeyboardInterrupt
        if char == "\b":
            if input_buffer:
                input_buffer.pop()
                print("\b \b", end="", flush=True)
            continue
        if char.isprintable() or char.isspace():
            input_buffer.append(char)
            print(char, end="", flush=True)
            continue

    return ""  # unreachable — satisfies the type checker that the function always returns str


class StreamingSpeechPlayer:
    """把流式文本按句子切分后送入 TTS 队列，实现边显示边播报。"""

    def __init__(
        self,
        text_to_speech: TextToSpeech | None,
        ui: TerminalUI,
        status_line: StatusLine,
        before_first_output: Callable[[], None] | None = None,
    ) -> None:
        self._text_to_speech = text_to_speech
        self._ui = ui
        self._status_line = status_line
        self._before_first_output = before_first_output
        self._buffer = ""
        self._has_output = False

    def handle_delta(self, delta: str) -> None:
        """显示增量文本，并在形成完整短句后提交后台播报。"""

        if not self._has_output:
            if self._before_first_output is not None:
                self._before_first_output()
            self._status_line.clear()
            self._ui.print_ai_prefix()
            self._has_output = True
        self._ui.write(delta)
        if self._text_to_speech is None:
            return

        self._buffer += delta
        while True:
            match = TTS_SENTENCE_PATTERN.match(self._buffer)
            if match is None:
                break

            sentence = match.group(1).strip()
            self._buffer = self._buffer[match.end() :]
            if sentence:
                self._text_to_speech.enqueue(sentence)

    def flush(self) -> None:
        """本轮回复结束后，把没有标点结尾的尾句也提交播报。"""

        if self._text_to_speech is None:
            return

        tail = self._buffer.strip()
        self._buffer = ""
        if tail:
            self._text_to_speech.enqueue(tail)

    def wait_until_done(self) -> None:
        """等待本轮已经提交的语音播报完成。"""

        if self._text_to_speech is not None:
            self._text_to_speech.wait_until_done()

    def wait_until_done_or_interrupt(self) -> tuple[bool, str | None]:
        """等待朗读结束；等待期间按 Enter 会停止朗读并保留已输入的问题。"""

        if self._text_to_speech is None:
            return False, None

        done = threading.Event()
        input_buffer: list[str] = []

        def wait_for_queue() -> None:
            self._text_to_speech.wait_until_done()
            done.set()

        waiter = threading.Thread(target=wait_for_queue, daemon=True)
        waiter.start()

        self._status_line.show("按 Enter 打断")
        while not done.wait(timeout=0.05):
            pressed_enter, buffered_text = _read_speech_interrupt_keypress(
                input_buffer,
                before_input=lambda: self._status_line.new_line_for_input(USER_PREFIX),
                on_empty_enter=self._status_line.clear,
            )
            if pressed_enter:
                self._text_to_speech.interrupt(wait_timeout_seconds=0)
                done.wait(timeout=5.0)
                self._status_line.clear()
                return True, buffered_text

        self._status_line.clear()
        if input_buffer:
            buffered_text = _read_buffered_console_line(input_buffer)
            return False, buffered_text or None

        return False, None


class FullScreenSpeechPlayer:
    """全屏 TUI 下的 AI 输出和语音播报适配器。"""

    def __init__(
        self,
        text_to_speech: TextToSpeech | None,
        tui: FullScreenTUI,
        prefix_blinker: AssistantPrefixBlinker,
        before_first_output: Callable[[], None] | None = None,
    ) -> None:
        self._text_to_speech = text_to_speech
        self._tui = tui
        self._prefix_blinker = prefix_blinker
        self._before_first_output = before_first_output
        self._buffer = ""
        self._has_output = False

    def handle_delta(self, delta: str) -> None:
        if not self._has_output:
            if self._before_first_output is not None:
                self._before_first_output()
            self._tui.start_assistant_message()
            self._prefix_blinker.start()
            self._has_output = True

        self._tui.append_assistant(delta)
        if self._text_to_speech is None:
            return

        self._buffer += delta
        while True:
            match = TTS_SENTENCE_PATTERN.match(self._buffer)
            if match is None:
                break

            sentence = match.group(1).strip()
            self._buffer = self._buffer[match.end() :]
            if sentence:
                self._text_to_speech.enqueue(sentence)

    def flush(self) -> None:
        if self._text_to_speech is not None:
            tail = self._buffer.strip()
            self._buffer = ""
            if tail:
                self._text_to_speech.enqueue(tail)
        self._prefix_blinker.stop()
        self._tui.finish_assistant_message()

    def wait_until_done_or_interrupt(self) -> tuple[bool, str | None]:
        if self._text_to_speech is None:
            return False, None

        done = threading.Event()
        input_buffer: list[str] = []

        def wait_for_queue() -> None:
            self._text_to_speech.wait_until_done()
            done.set()

        waiter = threading.Thread(target=wait_for_queue, daemon=True)
        waiter.start()

        self._tui.set_status("按 Enter 打断朗读；输入文字后 Enter 可直接提交下一句")
        while not done.wait(timeout=0.05):
            submitted = self._tui.poll_submitted_line(input_buffer)
            if submitted is not None:
                self._text_to_speech.interrupt(wait_timeout_seconds=0)
                done.wait(timeout=5.0)
                self._tui.set_status("已打断朗读")
                return True, submitted or None

        self._tui.set_status("Enter 发送，空 Enter 录音，q 退出")
        if input_buffer:
            return False, "".join(input_buffer).strip() or None
        return False, None


def _get_user_text_fullscreen(
    speech_to_text: SpeechToText | None,
    tui: FullScreenTUI,
) -> str:
    """全屏 TUI 下读取一轮用户输入。"""

    text = tui.read_line().strip()
    if text:
        return text

    if speech_to_text is None:
        return tui.read_line().strip()

    tui.set_status("正在录音...")
    try:
        recognized = speech_to_text.listen_once().strip()
        tui.set_status("Enter 发送，空 Enter 录音，q 退出")
        return recognized
    except SpeechToTextError as exc:
        tui.add_system_message(f"语音识别失败：{exc}")
        tui.set_status("语音识别失败，请改用键盘输入")
        return tui.read_line().strip()


def _run_fullscreen_chat(
    agent: LocalToolAgent,
    speech_to_text: SpeechToText | None,
    text_to_speech: TextToSpeech | None,
    *,
    model: str,
    thinking_type: str,
    reasoning_effort: str,
    config_label: str,
) -> None:
    """运行全屏 TUI 对话主循环。"""

    with FullScreenTUI(
        model=model,
        thinking_type=thinking_type,
        reasoning_effort=reasoning_effort,
        config_label=config_label,
        slash_commands=_build_slash_commands(agent),
    ) as tui:
        agent.set_confirm_handler(
            lambda tool_name, arguments: tui.confirm_yes_no(
                _format_tool_confirmation(tool_name, arguments)
            )
        )
        pending_user_text: str | None = None

        while True:
            if pending_user_text is not None:
                user_text = pending_user_text
                pending_user_text = None
            else:
                user_text = _get_user_text_fullscreen(speech_to_text, tui)

            if not user_text:
                continue

            tui.add_user_message(user_text)
            if user_text.lower() in EXIT_WORDS:
                tui.add_system_message("对话结束。")
                break

            if user_text.strip() == "/skills":
                _show_skills_in_tui(agent, tui)
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
                )
                waiting_indicator.stop("正在朗读回复" if text_to_speech is not None else "回复完成")
                speech_player.flush()
                interrupted, buffered_text = speech_player.wait_until_done_or_interrupt()
                if interrupted:
                    tui.add_status_message("已打断朗读。")
                if buffered_text:
                    pending_user_text = buffered_text
            except UserDeclinedOperation as exc:
                waiting_indicator.stop("操作已取消")
                prefix_blinker.stop()
                tui.hide_thinking_indicator()
                tui.add_system_message(str(exc))
                break
            except AgentError as exc:
                waiting_indicator.stop("Agent 请求失败")
                prefix_blinker.stop()
                tui.hide_thinking_indicator()
                tui.add_system_message(f"Agent 请求失败：{exc}")
                continue


def _run_inline_chat(
    agent: LocalToolAgent,
    speech_to_text: SpeechToText | None,
    text_to_speech: TextToSpeech | None,
    ui: TerminalUI,
) -> None:
    """运行旧版行内 UI，对不支持全屏 TUI 的终端自动降级。"""

    agent.set_confirm_handler(
        lambda tool_name, arguments: ui.prompt_yes_no(
            _format_tool_confirmation(tool_name, arguments)
        )
    )
    pending_user_text: str | None = None
    while True:
        if pending_user_text is not None:
            user_text = pending_user_text
            pending_user_text = None
        else:
            user_text = _get_user_text(speech_to_text, ui, _build_slash_commands(agent))

        if not user_text:
            continue

        if user_text.lower() in EXIT_WORDS:
            print("对话结束。")
            break

        if user_text.strip() == "/skills":
            _print_skills_list(agent)
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
            waiting_indicator.stop()
            ui.status(message)

        try:
            waiting_indicator.start()
            agent.run_stream(
                user_text,
                speech_player.handle_delta,
                on_status=handle_agent_status,
            )
            waiting_indicator.stop()
            speech_player.flush()
            ui.newline()
            interrupted, buffered_text = speech_player.wait_until_done_or_interrupt()
            if interrupted:
                ui.notice("已打断朗读。")
            if buffered_text:
                pending_user_text = buffered_text
        except UserDeclinedOperation as exc:
            waiting_indicator.stop()
            print(str(exc))
            break
        except AgentError as exc:
            waiting_indicator.stop()
            print(f"Agent 请求失败：{exc}")
            continue


def _format_skills_list(agent: LocalToolAgent) -> str:
    """格式化 Skill 列表为可展示文本。"""
    sm = agent.skill_manager
    if sm is None:
        return "Skill 子系统未启用。"

    metas = sm.list_all()
    if not metas:
        return "当前没有已加载的 Skill。在 .claude/skills/ 或 ~/.tui-agent/skills/ 下创建 SKILL.md 来添加。"

    lines = [f"已加载 {sm.count} 个 Skill："]
    for meta in metas:
        suffix = " [手动]" if meta.disable_model_invocation else ""
        lines.append(f"  {meta.name}{suffix}  ({meta.scope})")
        lines.append(f"    {meta.description}")
    return "\n".join(lines)


def _print_skills_list(agent: LocalToolAgent) -> None:
    """行内 UI 打印 Skill 列表。"""
    print(_format_skills_list(agent))


def _show_skills_in_tui(agent: LocalToolAgent, tui) -> None:
    """全屏 TUI 展示 Skill 列表。"""
    tui.add_system_message(_format_skills_list(agent))


def _build_slash_commands(agent: LocalToolAgent) -> list[str]:
    """构建所有可用的斜杠命令列表（含内置命令和动态 Skill 命令）。"""
    commands = ["/skills"]
    sm = agent.skill_manager
    if sm is not None:
        for meta in sm.list_all():
            commands.append(f"/skill:{meta.name}")
    return commands


def main() -> None:
    """命令行语音 AI Agent 入口。"""

    _configure_console_encoding()
    ui = TerminalUI()
    try:
        config = load_llm_config()
    except LLMError as exc:
        print(f"配置加载失败：{exc}")
        return

    config_path = resolve_config_path()

    print("AI 语音 Agent 已启动。")
    if config_path.exists():
        print(f"配置文件：{config_path}")
    else:
        print(f"配置文件：未找到 {config_path.name}，将回退到环境变量。")
    print(f"当前模型：{config.model}，可在 config.json 的 llm.model 中修改。")
    enabled_label = "已启用" if config.thinking_enabled else "已禁用"
    reasoning_info = f"，推理强度：{config.reasoning_effort}" if config.reasoning_effort else ""
    print(f"思考模式：{enabled_label}{reasoning_info}，可在 config.json 中修改。")
    print("接口使用 OpenAI Responses API 兼容格式。")
    print("Agent 可长任务工作并调用本地工具；工具执行前会由程序弹出确认。")
    print("命令工具已开放为弹窗确认后执行任意命令；确认界面默认 YES，右箭头/N 选择 NO。")

    speech_to_text = _create_speech_to_text()
    text_to_speech = _create_text_to_speech()

    try:
        agent = LocalToolAgent(AgentConfig(llm=config, workspace_root=Path(__file__).resolve().parent))
    except AgentError as exc:
        print(f"Agent 初始化失败：{exc}")
        return

    # 显示 Skill 加载情况
    if agent.skill_manager is not None and agent.skill_manager.count > 0:
        print(f"已加载 {agent.skill_manager.count} 个 Skill，输入 /skills 查看列表。")
        for diag in agent.skill_manager.get_diagnostics():
            print(f"  [诊断] {diag.message}（{diag.path}）")

    try:
        if supports_fullscreen_tui():
            _run_fullscreen_chat(
                agent,
                speech_to_text,
                text_to_speech,
                model=config.model,
                thinking_type=config.thinking_type,
                reasoning_effort=config.reasoning_effort,
                config_label=str(config_path),
            )
        else:
            print("当前终端不支持全屏 TUI，已自动切换到行内界面。")
            _run_inline_chat(agent, speech_to_text, text_to_speech, ui)
    finally:
        if text_to_speech is not None:
            text_to_speech.stop()


if __name__ == "__main__":
    if not _launch_in_powershell_window():
        main()
