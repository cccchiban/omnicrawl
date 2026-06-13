from __future__ import annotations

import os
import re
import threading
from typing import Callable

from .terminal_ui import USER_PREFIX, InputBar, MarkdownStreamState, StatusLine, TerminalUI
from .text_to_speech import TextToSpeech


TTS_SENTENCE_PATTERN = re.compile(r"(.+?[。！？!?；;\n])")


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
        if char in {"\x00", "\xe0"} and msvcrt.kbhit():
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

    return ""  # unreachable; satisfies the type checker that this returns str


class StreamingSpeechPlayer:
    """把流式文本按句子切分后送入 TTS 队列，实现边显示边播报。"""

    def __init__(
        self,
        text_to_speech: TextToSpeech | None,
        ui: TerminalUI,
        status_line: StatusLine,
        before_first_output: Callable[[], None] | None = None,
        input_bar: InputBar | None = None,
    ) -> None:
        self._text_to_speech = text_to_speech
        self._ui = ui
        self._status_line = status_line
        self._before_first_output = before_first_output
        self._input_bar = input_bar
        self._buffer = ""
        self._has_output = False
        self._markdown_state = MarkdownStreamState()

    def handle_delta(self, delta: str) -> None:
        """显示增量文本，并在形成完整短句后提交后台播报。"""

        if not self._has_output:
            if self._before_first_output is not None:
                self._before_first_output()
            self._status_line.clear()
            if self._input_bar is not None:
                self._input_bar.push_up()
            self._ui.newline()
            self._ui.print_ai_prefix()
            self._has_output = True
        if self._input_bar is not None:
            self._input_bar.push_up()
        self._ui.write_markdown_delta(delta, self._markdown_state)
        if self._input_bar is not None:
            self._input_bar.pop_down()
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

        if self._input_bar is not None:
            self._input_bar.push_up()
        self._ui.flush_markdown(self._markdown_state)
        if self._input_bar is not None:
            self._input_bar.pop_down()
        if self._text_to_speech is None:
            return

        tail = self._buffer.strip()
        self._buffer = ""
        if tail:
            self._text_to_speech.enqueue(tail)

    def flush_display(self) -> None:
        """只提交当前终端显示，不触发语音播报。

        模型可能先流式输出一句说明，随后才请求工具。工具状态行写入前必须把
        这句说明从预览态落成真实行，否则后续清预览会回退到工具状态行，
        造成长任务日志里中文重复、状态错位或残留。
        """

        if self._input_bar is not None:
            self._input_bar.push_up()
        self._ui.flush_markdown(self._markdown_state)
        if self._input_bar is not None:
            self._input_bar.pop_down()

    @property
    def has_display_output(self) -> bool:
        """当前 Agent 轮次里是否已经显示过模型文本。"""

        return self._has_output

    def start_new_display_segment(self) -> None:
        """让下一段模型文本重新清理等待状态并打印 AI 前缀。

        一轮 Agent 可能经历多段输出。每次工具执行后都会重新显示等待动画，
        因此下一段模型文本不能沿用上一段的 _has_output=True，否则文本会
        直接追加到等待动画所在行。
        """

        if self._has_output:
            self.flush_display()
            self._markdown_state = MarkdownStreamState()
            self._has_output = False

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
