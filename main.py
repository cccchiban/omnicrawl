from __future__ import annotations

import json
import os
import re
import shutil
import subprocess
import sys
import threading
from pathlib import Path
from typing import Any, Callable

from ai_voice_agent.agent import AgentConfig, AgentError, LocalToolAgent, UserDeclinedOperation
from ai_voice_agent.fullscreen_tui import (
    AssistantPrefixBlinker,
    FullScreenTUI,
    FullScreenWaitingIndicator,
)
from ai_voice_agent.llm import LLMError, load_llm_config
from ai_voice_agent.runtime_config import resolve_config_path
from ai_voice_agent.terminal_ui import (
    USER_PREFIX,
    MarkdownStreamState,
    StatusLine,
    TerminalUI,
    WaitingIndicator,
    _display_width as _terminal_display_width,
    _take_display_width as _terminal_take_display_width,
)
from ai_voice_agent.speech_to_text import MicrophoneInfo, SpeechConfig, SpeechToText, SpeechToTextError
from ai_voice_agent.text_to_speech import TextToSpeech, TextToSpeechError


EXIT_WORDS = {"退出", "结束", "再见"}
POWERSHELL_CHILD_ENV = "AI_VOICE_CHAT_IN_POWERSHELL"
TTS_SENTENCE_PATTERN = re.compile(r"(.+?[。！？!?；;\n])")
INLINE_COMPLETION_LIMIT = 8


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


def _prompt_visible_width(prompt: str, ui: TerminalUI | None) -> int:
    """返回当前输入提示符在终端里实际占用的列数。"""

    if ui is not None:
        return ui.prompt_width()
    return _terminal_display_width(prompt.rsplit("\n", 1)[-1])


def _truncate_menu_text(text: str, max_width: int) -> str:
    """按字符近似截断菜单项，避免长命令把终端行撑乱。"""

    if max_width <= 3:
        return _terminal_take_display_width(text, max(0, max_width))
    if _terminal_display_width(text) <= max_width:
        return text
    return f"{_terminal_take_display_width(text, max_width - 3)}..."


def _slash_command_matches(text: str, commands: list[str]) -> list[str]:
    """只在整行以 / 开头时启用命令候选，避免普通文本中误触发。"""

    if not text.startswith("/"):
        return []
    return [command for command in commands if command.startswith(text)]


def _visible_completion_window(
    matches: list[str],
    selected_index: int,
    *,
    limit: int = INLINE_COMPLETION_LIMIT,
) -> tuple[int, list[str]]:
    """返回当前选中项附近的一段候选，保证选中项始终在可见窗口内。"""

    if not matches:
        return 0, []

    selected_index = max(0, min(selected_index, len(matches) - 1))
    limit = max(1, limit)
    start = 0
    if selected_index >= limit:
        start = selected_index - limit + 1
    return start, matches[start : start + limit]


def _format_completion_menu_lines(
    matches: list[str],
    selected_index: int,
    *,
    terminal_width: int,
) -> list[str]:
    """把候选命令格式化成稳定的菜单行，供行内 TUI 重绘。"""

    if not matches:
        return []

    start, visible = _visible_completion_window(matches, selected_index)
    command_width = max(8, terminal_width - 8)
    lines: list[str] = []
    for offset, command in enumerate(visible):
        actual_index = start + offset
        marker = "> " if actual_index == selected_index else "  "
        lines.append(f"  {marker}{_truncate_menu_text(command, command_width)}")
    return lines


def _is_inline_escape_sequence_complete(sequence: str) -> bool:
    """判断行内输入读取到的 ESC 序列是否完整。"""

    if not sequence:
        return False
    if sequence[0] not in {"[", "O"}:
        return True
    if sequence[0] == "O":
        return len(sequence) >= 2
    return len(sequence) >= 2 and 0x40 <= ord(sequence[-1]) <= 0x7E


class _InlineCompletionMenu:
    """管理输入行下方的临时候选区域。

    旧实现每次清理候选时从输入行向上擦除，菜单可见时继续输入会误删上方对话内容。
    这里把候选区固定为“输入行下方的若干临时行”：绘制、清理都只向下操作，
    最后再把光标放回输入位置，从根上避免错行和残留。
    """

    def __init__(self, ui: TerminalUI | None) -> None:
        self._ui = ui
        self._rendered_lines = 0
        self._allocated_lines = 0

    @property
    def _ansi_enabled(self) -> bool:
        return self._ui is not None and self._ui.capabilities.ansi

    def render(
        self,
        *,
        matches: list[str],
        selected_index: int,
        cursor_column: int,
    ) -> None:
        if not self._ansi_enabled:
            return

        terminal_width = shutil.get_terminal_size((100, 30)).columns
        lines = _format_completion_menu_lines(
            matches,
            selected_index,
            terminal_width=terminal_width,
        )
        if not lines and self._ui is not None and self._ui.model_label:
            lines = [self._ui.muted(f"- {self._ui.model_label}")]

        self._replace_lines(lines, cursor_column)

    def clear(self, *, cursor_column: int) -> None:
        if not self._ansi_enabled:
            return
        self._replace_lines([], cursor_column)

    def _replace_lines(self, lines: list[str], cursor_column: int) -> None:
        assert self._ui is not None

        self._ensure_allocated(len(lines), cursor_column)
        lines_to_clear = max(self._rendered_lines, len(lines))
        if lines_to_clear == 0:
            return

        parts: list[str] = []
        for index in range(lines_to_clear):
            parts.append("\033[1B\r\033[2K")
            if index < len(lines):
                parts.append(lines[index])
        parts.append(f"\033[{lines_to_clear}A")
        parts.append(f"\033[{max(1, cursor_column)}G")

        with self._ui._lock:
            print("".join(parts), end="", flush=True)
        self._rendered_lines = len(lines)

    def _ensure_allocated(self, line_count: int, cursor_column: int) -> None:
        assert self._ui is not None

        missing_lines = max(0, line_count - self._allocated_lines)
        if missing_lines == 0:
            return

        # 在输入行下面预留真实终端行，避免光标已经在窗口底部时 CSI 向下移动失败。
        with self._ui._lock:
            print("\n" * missing_lines, end="")
            print(f"\033[{missing_lines}A\033[{max(1, cursor_column)}G", end="", flush=True)
        self._allocated_lines = line_count


def _read_line_autocomplete(prompt: str, commands: list[str], ui: TerminalUI | None = None) -> str:
    """逐字符读取输入，在输入 / 时实时显示匹配命令。

    Tab 补全当前唯一或最佳匹配，Enter 提交。
    """
    import msvcrt

    if ui is None or not ui.capabilities.ansi:
        return input(prompt).strip()

    prompt_text = prompt.rsplit("\n", 1)[-1]
    prompt_width = _prompt_visible_width(prompt, ui)
    menu = _InlineCompletionMenu(ui)
    print(prompt, end="", flush=True)
    text = ""
    cursor = 0
    matches: list[str] = []
    match_index = 0

    def _cursor_column() -> int:
        return prompt_width + _terminal_display_width(text[:cursor]) + 1

    def _redraw_input() -> None:
        print(f"\r\033[2K{prompt_text}{text}\033[{_cursor_column()}G", end="", flush=True)

    def _render_menu() -> None:
        menu.render(
            matches=matches,
            selected_index=match_index,
            cursor_column=_cursor_column(),
        )

    def _update_matches(*, reset_selection: bool = True) -> None:
        nonlocal matches, match_index
        matches = _slash_command_matches(text, commands)
        if reset_selection:
            match_index = 0
        elif matches:
            match_index = max(0, min(match_index, len(matches) - 1))
        else:
            match_index = 0
        _render_menu()

    def _hide_matches() -> None:
        nonlocal matches, match_index
        matches = []
        match_index = 0
        _render_menu()

    def _move_selection(delta: int) -> None:
        nonlocal match_index
        if not matches:
            return
        match_index = (match_index + delta) % len(matches)
        _render_menu()

    def _move_cursor(delta: int) -> None:
        nonlocal cursor
        cursor = max(0, min(len(text), cursor + delta))
        _redraw_input()
        _render_menu()

    def _handle_navigation_key(key: str) -> None:
        nonlocal text, cursor
        if key == "H":
            _move_selection(-1)
            return
        if key == "P":
            _move_selection(1)
            return
        if key == "K":
            _move_cursor(-1)
            return
        if key == "M":
            _move_cursor(1)
            return
        if key == "G":
            cursor = 0
            _redraw_input()
            _render_menu()
            return
        if key == "O":
            cursor = len(text)
            _redraw_input()
            _render_menu()
            return
        if key == "S" and cursor < len(text):
            text = text[:cursor] + text[cursor + 1 :]
            _redraw_input()
            _update_matches()

    _render_menu()

    while True:
        char = msvcrt.getwch()

        if char in {"\r", "\n"}:
            menu.clear(cursor_column=_cursor_column())
            print()
            return text.strip()

        if char == "\t":
            if matches:
                text = matches[match_index]
                cursor = len(text)
                _redraw_input()
                _hide_matches()
            continue

        if char == "\x1b":
            # 读取转义序列
            seq_parts: list[str] = []
            import time as _time
            deadline = _time.monotonic() + 0.02
            while _time.monotonic() < deadline:
                if msvcrt.kbhit():
                    seq_parts.append(msvcrt.getwch())
                    if _is_inline_escape_sequence_complete("".join(seq_parts)):
                        break
                else:
                    _time.sleep(0.001)
            sequence = "".join(seq_parts)

            if not sequence:
                # 纯 Escape：关闭补全菜单
                _hide_matches()
                continue
            if sequence in {"[A", "OA"}:  # Up
                _move_selection(-1)
                continue
            if sequence in {"[B", "OB"}:  # Down
                _move_selection(1)
                continue
            if sequence in {"[D", "OD"}:  # Left
                _move_cursor(-1)
                continue
            if sequence in {"[C", "OC"}:  # Right
                _move_cursor(1)
                continue
            if sequence in {"[H", "[1~"}:
                cursor = 0
                _redraw_input()
                _render_menu()
                continue
            if sequence in {"[F", "[4~"}:
                cursor = len(text)
                _redraw_input()
                _render_menu()
                continue
            if sequence == "[3~" and cursor < len(text):
                text = text[:cursor] + text[cursor + 1 :]
                _redraw_input()
                _update_matches()
                continue
            continue

        if char in {"\x00", "\xe0"}:
            _handle_navigation_key(msvcrt.getwch())
            continue

        if char == "\x03":
            raise KeyboardInterrupt

        if char == "\b":
            if cursor > 0:
                text = text[:cursor - 1] + text[cursor:]
                cursor -= 1
                _redraw_input()
                _update_matches()
            continue

        if char == "\x7f":
            if cursor < len(text):
                text = text[:cursor] + text[cursor + 1:]
                _redraw_input()
                _update_matches()
            continue

        if char.isprintable() or char.isspace():
            text = text[:cursor] + char + text[cursor:]
            cursor += 1
            _redraw_input()
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

    used_autocomplete = False
    if os.name == "nt" and slash_commands:
        try:
            import msvcrt
        except ImportError:
            pass
        else:
            used_autocomplete = True
            text = _read_line_autocomplete(ui.prompt(), slash_commands, ui)
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
        self._markdown_state = MarkdownStreamState()

    def handle_delta(self, delta: str) -> None:
        """显示增量文本，并在形成完整短句后提交后台播报。"""

        if not self._has_output:
            if self._before_first_output is not None:
                self._before_first_output()
            self._status_line.clear()
            self._ui.print_ai_prefix()
            self._has_output = True
        self._ui.write_markdown_delta(delta, self._markdown_state)
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

        self._ui.flush_markdown(self._markdown_state)
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
    """保留给全屏 TUI 的 AI 输出和语音播报适配器。"""

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

        self._tui.set_status("Enter 发送，空 Enter 录音，Ctrl+C 或输入“退出”结束")
        if input_buffer:
            return False, "".join(input_buffer).strip() or None
        return False, None


def _get_user_text_fullscreen(
    speech_to_text: SpeechToText | None,
    tui: FullScreenTUI,
) -> str:
    """保留给全屏 TUI 的单轮用户输入读取。"""

    text = tui.read_line().strip()
    if text:
        return text

    if speech_to_text is None:
        return tui.read_line().strip()

    tui.set_status("正在录音...")
    try:
        recognized = speech_to_text.listen_once(tui.set_status).strip()
        tui.set_status("Enter 发送，空 Enter 录音，Ctrl+C 或输入“退出”结束")
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
    """运行保留的全屏 TUI 对话主循环。"""

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
    """运行默认的普通终端内联 UI。"""

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
    """保留给全屏 TUI 的 Skill 列表展示。"""
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
    try:
        config = load_llm_config()
    except LLMError as exc:
        print(f"配置加载失败：{exc}")
        return
    ui = TerminalUI(model_label=config.model)

    config_path = resolve_config_path()

    enabled_label = "已启用" if config.thinking_enabled else "已禁用"
    reasoning_info = f"，推理强度：{config.reasoning_effort}" if config.reasoning_effort else ""
    config_label = str(config_path) if config_path.exists() else f"未找到 {config_path.name}，回退到环境变量"
    ui.print_startup_panel(
        "AI 语音 Agent",
        [
            f"thinking: {enabled_label}{reasoning_info}",
            f"config: {config_label}",
            "Ctrl+C 或关闭窗口结束会话",
        ],
    )

    transient_output_marked = ui.mark_transient_output_start()
    speech_to_text = _create_speech_to_text()
    text_to_speech = _create_text_to_speech()
    if transient_output_marked and speech_to_text is not None and text_to_speech is not None:
        ui.clear_transient_output()

    try:
        agent = LocalToolAgent(AgentConfig(llm=config, workspace_root=Path(__file__).resolve().parent))
    except AgentError as exc:
        print(f"Agent 初始化失败：{exc}")
        return

    # 显示 Skill 加载情况
    if agent.skill_manager is not None and agent.skill_manager.count > 0:
        print(ui.muted(f"已加载 {agent.skill_manager.count} 个 Skill，输入 /skills 查看列表。"))

    try:
        _run_inline_chat(agent, speech_to_text, text_to_speech, ui)
    except KeyboardInterrupt:
        print("\n对话结束。")
    finally:
        if text_to_speech is not None:
            text_to_speech.stop()


if __name__ == "__main__":
    if not _launch_in_powershell_window():
        main()
