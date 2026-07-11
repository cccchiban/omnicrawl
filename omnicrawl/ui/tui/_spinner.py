"""等待动画（spinner），带预输入支持和持久输入栏。"""

from __future__ import annotations


import os
import shutil
import threading
import time
from typing import Any

from ._capabilities import TerminalCapabilities
from ._colors import ANSI_CLEAR_LINE, ANSI_PREVIOUS_LINE, color_text, strip_ansi
from ._display import _combine_surrogate_pair, _delete_last_display_unit, _split_display_rows
from ._status import StatusLine, _render_prompt_status_rows, _render_styled_status_rows

# Spinner 动画帧
SPINNER_FRAMES = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]

# 轮播状态文字
_STATUS_LABELS = ["正在思考", "正在分析", "正在生成"]
_STATUS_LABEL_INTERVAL_SECONDS = 2.0


class InputBar:
    """终端底部持久输入栏（▸ + token 状态行）。

    在 AI 输出期间以“输出前清除、输出后追加”的方式维护，避免直接在
    模型输出位置原地重绘时覆盖正文。输入栏同时收集用户预输入。
    """

    def __init__(self, ui: Any, *, caps: TerminalCapabilities | None = None) -> None:
        self._ui = ui
        self._caps = caps or ui.capabilities
        self._pre_input: str = ""
        self._submitted_pre_input: str = ""
        # getwch() 在 Windows 上对非 BMP 字符返回两个 UTF-16 代理项；需要跨
        # 非阻塞轮询暂存高代理，等低代理到达后再插入完整显示单元。
        self._pending_high_surrogate: str = ""
        self._visible = False
        # 预输入行数：1 行输入 + (0 或 1) 行 token 状态
        self._info_lines = 1  # 至少输入行

    @property
    def info_lines(self) -> int:
        """当前输入栏占用的终端行数（不含 spinner）。"""
        return self._info_lines

    @property
    def pre_input(self) -> str:
        """当前已收集但未必提交的预输入草稿。"""
        return self._pre_input

    @property
    def submitted_pre_input(self) -> str:
        """用户按 Enter 提交的预输入文本。"""
        return self._submitted_pre_input

    def _pre_input_enabled(self) -> bool:
        return self._caps.ansi and os.name == "nt"

    def show(self, spinner_text: str = "") -> None:
        """渲染输入栏，并按实际终端列数记录其物理行数。"""
        if not self._caps.ansi or not self._pre_input_enabled():
            return

        ui = self._ui
        prompt_prefix = color_text("▸", "primary", self._caps)
        terminal_width = max(1, shutil.get_terminal_size((100, 30)).columns)
        input_width = max(1, terminal_width - ui.prompt_width() - 1)
        input_rows = _split_display_rows(self._pre_input, input_width)
        input_lines = [
            f"{prompt_prefix} {row}" if index == 0 else f"{' ' * ui.prompt_width()}{row}"
            for index, row in enumerate(input_rows)
        ]
        lines: list[str] = []
        if spinner_text:
            indent = " " * ui.prompt_width()
            spinner_rows = _render_styled_status_rows(
                f"{indent}{strip_ansi(spinner_text)}",
                role="muted",
                caps=self._caps,
                max_width=terminal_width,
            )
            lines.extend(f"{ANSI_CLEAR_LINE}{row}" for row in spinner_rows)
        lines.extend(input_lines)
        if ui.model_label:
            lines.extend(
                _render_prompt_status_rows(
                    ui.model_label,
                    ui._input_tokens,
                    ui._output_tokens,
                    ui._cached_input_tokens,
                    caps=self._caps,
                    max_width=terminal_width,
                )
            )

        with ui._lock:
            if self._visible:
                self._clear_visible_locked()
                leading = ""
            else:
                leading = "\n"
            rendered_lines = "\n".join(lines)
            print(f"{leading}{rendered_lines}", end="", flush=True)
            self._visible = True
            self._info_lines = len(lines)

    def push_up(self) -> None:
        """清除输入栏，为 AI 输出腾出空间。"""
        if not self._visible or not self._caps.ansi:
            return
        with self._ui._lock:
            self._clear_visible_locked()

    def pop_down(self) -> None:
        """AI 输出后，在当前位置下方重新追加输入栏。"""
        if not self._pre_input_enabled():
            return
        self.show()

    def clear(self) -> str:
        """清除输入栏，返回已提交的预输入文本。"""
        pre = self._submitted_pre_input
        if self._visible and self._caps.ansi:
            with self._ui._lock:
                self._clear_visible_locked()
        return pre

    def _clear_visible_locked(self) -> None:
        """从输入栏最后一行向上清理，调用方必须持有 UI 锁。"""
        for index in range(max(0, self._info_lines)):
            print(f"\r{ANSI_CLEAR_LINE}", end="")
            if index < self._info_lines - 1:
                print(ANSI_PREVIOUS_LINE, end="")
        self._visible = False
        self._info_lines = 0

    def poll_pre_input(self) -> None:
        """非阻塞轮询键盘输入。"""
        if not self._pre_input_enabled():
            return
        try:
            import msvcrt
        except ImportError:
            return

        while msvcrt.kbhit():
            char = msvcrt.getwch()
            if self._pending_high_surrogate:
                combined = _combine_surrogate_pair(self._pending_high_surrogate, char)
                if combined is not None:
                    self._pre_input += combined
                    self._pending_high_surrogate = ""
                    continue
                self._pre_input += "�"
                self._pending_high_surrogate = ""
            if 0xD800 <= ord(char) <= 0xDBFF:
                self._pending_high_surrogate = char
                continue
            if char in {"\r", "\n"}:
                submitted = self._pre_input.strip()
                if submitted:
                    self._submitted_pre_input = submitted
                continue
            if char == "\x03":
                raise KeyboardInterrupt
            if char in {"\x00", "\xe0"}:
                if msvcrt.kbhit():
                    msvcrt.getwch()
                continue
            if char in {"\b", "\x7f"}:
                if self._pre_input:
                    self._pre_input = _delete_last_display_unit(self._pre_input)
                continue
            if char.isprintable() or char == " ":
                self._pre_input += char


class WaitingIndicator:
    """模型返回前的现代 spinner 等待动画。

    带轮播状态文字、经过时间计数，以及预输入收集。
    spinner 运行时同时显示输入栏（▸ + token 状态行）。
    """

    def __init__(self, status_line: StatusLine, *, caps: TerminalCapabilities | None = None, input_bar: InputBar | None = None) -> None:
        self._status_line = status_line
        self._caps = caps or status_line._ui.capabilities
        self._input_bar = input_bar
        self._stop = threading.Event()
        self._thread: threading.Thread | None = None
        self._frame_index = 0
        self._start_time: float = 0.0
        self._has_info_lines = False
        self._rendered_info_lines = 0
        # 无 InputBar 时的后备存储
        self._pre_input: str = ""
        self._submitted_pre_input: str = ""
        self._pending_high_surrogate: str = ""

    @property
    def input_bar(self) -> InputBar | None:
        return self._input_bar

    def start(self, *, reset_input: bool = False) -> None:
        if self._thread is not None and self._thread.is_alive():
            return
        self._stop.clear()
        self._frame_index = 0
        self._start_time = time.monotonic()
        self._has_info_lines = False
        self._rendered_info_lines = 0
        if reset_input:
            if self._input_bar is not None:
                self._input_bar.clear()
                self._input_bar._pre_input = ""
                self._input_bar._submitted_pre_input = ""
                self._input_bar._pending_high_surrogate = ""
            else:
                self._pre_input = ""
                self._submitted_pre_input = ""
                self._pending_high_surrogate = ""
        self._thread = threading.Thread(target=self._run, daemon=True)
        self._thread.start()

    def stop(self) -> str:
        """停止 spinner，保留输入栏，返回已提交的预输入文本。"""

        self._stop.set()
        if self._thread is not None:
            self._thread.join(timeout=1.0)
            self._thread = None

        pre = self._input_bar._submitted_pre_input if self._input_bar else self._submitted_pre_input

        if self._has_info_lines and self._caps.ansi:
            with self._status_line._ui._lock:
                for index in range(max(0, self._rendered_info_lines)):
                    print(f"\r{ANSI_CLEAR_LINE}", end="")
                    if index < self._rendered_info_lines - 1:
                        print(ANSI_PREVIOUS_LINE, end="")
                print("", end="", flush=True)
            self._has_info_lines = False
            self._rendered_info_lines = 0
        else:
            self._status_line.clear()

        # spinner 停止后，立即以无 spinner 文本的方式重新渲染输入栏
        if self._input_bar is not None and self._input_bar._pre_input_enabled():
            self._input_bar.show()

        return pre

    @property
    def pre_input(self) -> str:
        """当前已收集但未必提交的预输入草稿。"""

        if self._input_bar is not None:
            return self._input_bar.pre_input
        return self._pre_input

    def _pre_input_enabled(self) -> bool:
        """仅在可控的 Windows ANSI 终端中启用预输入布局。"""
        return self._caps.ansi and os.name == "nt"

    def _render_status(self, text: str) -> None:
        """渲染等待状态；预输入模式按物理行数维护动态区域。"""

        if not self._pre_input_enabled():
            self._status_line.show(text)
            return

        ui = self._status_line._ui
        terminal_width = max(1, shutil.get_terminal_size((100, 30)).columns)
        indent = " " * ui.prompt_width()
        # spinner 文本在逻辑层保持纯文本；先换行再为每个物理行独立套样式，
        # 避免 SGR 状态跨行泄漏到输入栏或 token 行。
        status_rows = _render_styled_status_rows(
            f"{indent}{strip_ansi(text)}",
            role="muted",
            caps=self._caps,
            max_width=terminal_width,
        )
        prompt_prefix = color_text("▸", "primary", self._caps)
        pre_input_text = self._input_bar._pre_input if self._input_bar else self._pre_input
        input_rows = _split_display_rows(
            pre_input_text,
            max(1, terminal_width - ui.prompt_width() - 1),
        )
        lines = [f"{ANSI_CLEAR_LINE}{row}" for row in status_rows]
        lines.extend(
            f"{prompt_prefix} {row}" if index == 0 else f"{' ' * ui.prompt_width()}{row}"
            for index, row in enumerate(input_rows)
        )
        if ui.model_label:
            lines.extend(
                _render_prompt_status_rows(
                    ui.model_label,
                    ui._input_tokens,
                    ui._output_tokens,
                    ui._cached_input_tokens,
                    caps=self._caps,
                    max_width=terminal_width,
                )
            )

        with ui._lock:
            if self._has_info_lines:
                for index in range(max(0, self._rendered_info_lines)):
                    print(f"\r{ANSI_CLEAR_LINE}", end="")
                    if index < self._rendered_info_lines - 1:
                        print(ANSI_PREVIOUS_LINE, end="")
                leading = ""
            else:
                leading = "\n"
            rendered_lines = "\n".join(lines)
            print(f"{leading}{rendered_lines}", end="", flush=True)
            self._has_info_lines = True
            self._rendered_info_lines = len(lines)

    def _poll_pre_input(self) -> None:
        """非阻塞轮询键盘输入。"""

        if not self._pre_input_enabled():
            return
        try:
            import msvcrt
        except ImportError:
            return

        while msvcrt.kbhit():
            char = msvcrt.getwch()
            if self._pending_high_surrogate:
                combined = _combine_surrogate_pair(self._pending_high_surrogate, char)
                if combined is not None:
                    if self._input_bar is not None:
                        self._input_bar._pre_input += combined
                    else:
                        self._pre_input += combined
                    self._pending_high_surrogate = ""
                    continue
                if self._input_bar is not None:
                    self._input_bar._pre_input += "�"
                else:
                    self._pre_input += "�"
                self._pending_high_surrogate = ""
            if 0xD800 <= ord(char) <= 0xDBFF:
                self._pending_high_surrogate = char
                continue
            if char in {"\r", "\n"}:
                # Enter 提交预输入
                draft = self._input_bar._pre_input if self._input_bar else self._pre_input
                submitted = draft.strip()
                if submitted:
                    if self._input_bar is not None:
                        self._input_bar._submitted_pre_input = submitted
                    else:
                        self._submitted_pre_input = submitted
                    self._stop.set()
                continue
            if char == "\x03":
                self._stop.set()
                continue
            if char in {"\x00", "\xe0"}:
                if msvcrt.kbhit():
                    msvcrt.getwch()
                continue
            if char in {"\b", "\x7f"}:
                if self._input_bar is not None:
                    if self._input_bar._pre_input:
                        self._input_bar._pre_input = _delete_last_display_unit(
                            self._input_bar._pre_input
                        )
                elif self._pre_input:
                    self._pre_input = _delete_last_display_unit(self._pre_input)
                continue
            if char.isprintable() or char == " ":
                if self._input_bar is not None:
                    self._input_bar._pre_input += char
                else:
                    self._pre_input += char

    def _run(self) -> None:
        while not self._stop.is_set():
            self._poll_pre_input()
            if self._stop.is_set():
                break

            frame = SPINNER_FRAMES[self._frame_index % len(SPINNER_FRAMES)]

            # 轮播状态文字
            elapsed = time.monotonic() - self._start_time
            label_index = int(elapsed / _STATUS_LABEL_INTERVAL_SECONDS) % len(_STATUS_LABELS)
            label = _STATUS_LABELS[label_index]

            # 时间计数
            seconds = int(elapsed)

            # _render_status 会在换行后统一为弱化状态色，不能把已含 SGR 的
            # 片段交给普通显示宽度拆行器，否则颜色状态可能跨越物理行。
            self._render_status(f"{frame} {label} · {seconds}s")
            self._frame_index += 1
            self._stop.wait(0.08)
