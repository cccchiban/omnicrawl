from __future__ import annotations

import os
import random
import shutil
import sys
import threading
import ctypes
import unicodedata
from dataclasses import dataclass


AI_PREFIX = "^"
USER_PREFIX = ">"
ANSI_CLEAR_LINE = "\033[2K"
ANSI_PREVIOUS_LINE = "\033[1A"
ANSI_MUTED = "\033[2;90m"
ANSI_GRAY = "\033[90m"
ANSI_LIGHT_BLUE = "\033[94m"
ANSI_RESET = "\033[0m"
WAITING_KAOMOJI = (
    "(｡･ω･｡)",
    "(｀・ω・´)",
    "(´･ω･`)",
    "(。-ω-)zzz",
    "(っ˘ω˘ς)",
    "(๑•̀ㅂ•́)و",
)
WAITING_DOTS = ("", ".", "..", "...", "..", ".")


def _char_display_width(char: str) -> int:
    if unicodedata.combining(char):
        return 0
    return 2 if unicodedata.east_asian_width(char) in {"F", "W"} else 1


def _display_width(text: str) -> int:
    return sum(_char_display_width(char) for char in text)


def _take_display_width(text: str, max_width: int) -> str:
    if max_width <= 0:
        return ""

    width = 0
    chars: list[str] = []
    for char in text:
        char_width = _char_display_width(char)
        if width + char_width > max_width:
            break
        chars.append(char)
        width += char_width
    return "".join(chars)


@dataclass(frozen=True)
class TerminalCapabilities:
    """当前终端可用能力。

    终端样式本质由终端模拟器决定。这里仅判断是否适合输出 ANSI 控制序列，
    不尝试模拟真正的小字体或复杂 TUI。
    """

    ansi: bool


def detect_capabilities() -> TerminalCapabilities:
    """根据环境判断是否启用 ANSI 样式和行重绘。"""

    if os.getenv("NO_COLOR"):
        return TerminalCapabilities(ansi=False)
    if os.name == "nt":
        return TerminalCapabilities(
            ansi=bool(
                os.getenv("WT_SESSION")
                or os.getenv("TERM_PROGRAM")
                or os.getenv("ANSICON")
                or os.getenv("ConEmuANSI") == "ON"
                or "xterm" in os.getenv("TERM", "").lower()
                or _enable_windows_virtual_terminal()
            )
        )

    return TerminalCapabilities(ansi=sys.stdout.isatty() and os.getenv("TERM") != "dumb")


def _enable_windows_virtual_terminal() -> bool:
    """在 Windows 控制台中开启 ANSI/VT 控制序列支持。"""

    if not sys.stdout.isatty():
        return False

    kernel32 = ctypes.windll.kernel32
    handle = kernel32.GetStdHandle(-11)
    if handle == -1:
        return False

    mode = ctypes.c_uint32()
    if not kernel32.GetConsoleMode(handle, ctypes.byref(mode)):
        return False

    ENABLE_VIRTUAL_TERMINAL_PROCESSING = 0x0004
    updated_mode = mode.value | ENABLE_VIRTUAL_TERMINAL_PROCESSING
    if not kernel32.SetConsoleMode(handle, updated_mode):
        return False

    return True


class TerminalUI:
    """集中管理终端输出样式，避免多个调用点各自拼 ANSI。"""

    def __init__(self, capabilities: TerminalCapabilities | None = None) -> None:
        self.capabilities = capabilities or detect_capabilities()
        self._lock = threading.Lock()

    def muted(self, text: str) -> str:
        if not self.capabilities.ansi:
            return text
        return f"{ANSI_MUTED}{text}{ANSI_RESET}"

    def print_startup_panel(self, title: str, lines: list[str]) -> None:
        """打印普通终端内的启动面板。

        这里不接管屏幕缓冲区，只输出一次带灰色边框的配置摘要，保持终端历史可滚动；
        面板宽度会按终端宽度收缩，避免长配置路径把右侧边框挤出可视范围。
        """

        terminal_width = shutil.get_terminal_size((100, 30)).columns
        max_box_width = max(24, terminal_width - 2)
        desired_content_width = max(_display_width(title), *(_display_width(line) for line in lines), 36)
        content_width = min(max_box_width - 4, desired_content_width)

        def render_row(text: str) -> str:
            content = _take_display_width(text, content_width)
            padding = " " * max(0, content_width - _display_width(content))
            if not self.capabilities.ansi:
                return f"| {content}{padding} |"
            return (
                f"{ANSI_GRAY}│ {ANSI_RESET}"
                f"{ANSI_LIGHT_BLUE}{content}{ANSI_RESET}"
                f"{padding}"
                f"{ANSI_GRAY} │{ANSI_RESET}"
            )

        horizontal = "─" * (content_width + 2)
        if self.capabilities.ansi:
            top = f"{ANSI_GRAY}┌{horizontal}┐{ANSI_RESET}"
            bottom = f"{ANSI_GRAY}└{horizontal}┘{ANSI_RESET}"
        else:
            top = f"+{'-' * (content_width + 2)}+"
            bottom = top

        with self._lock:
            print(top)
            print(render_row(title))
            for line in lines:
                print(render_row(line))
            print(bottom)

    def prompt(self) -> str:
        return f"\n{USER_PREFIX} "

    def inline_turn_base(self, user_text: str) -> str:
        """把刚提交的输入行改写成对话行前半段。

        支持 ANSI 时会回到上一行重绘；不支持时退化为新起一行。
        """

        base_text = f"{USER_PREFIX} {user_text} "
        with self._lock:
            if self.capabilities.ansi:
                print(f"{ANSI_PREVIOUS_LINE}\r{ANSI_CLEAR_LINE}{base_text}", end="", flush=True)
                return base_text
        return ""

    def print_ai_prefix(self) -> None:
        with self._lock:
            print(f"{AI_PREFIX} ", end="", flush=True)

    def write(self, text: str) -> None:
        with self._lock:
            print(text, end="", flush=True)

    def newline(self) -> None:
        with self._lock:
            print()

    def status(self, message: str) -> None:
        with self._lock:
            print(f"\n{self.muted(f'[{message}]')}", flush=True)

    def notice(self, message: str) -> None:
        with self._lock:
            print(self.muted(message), flush=True)

    def prompt_yes_no(self, prompt: str) -> bool:
        """以默认 YES 的方式确认一次高风险操作。

        Windows 下优先支持单键输入：回车确认，右箭头或 N 取消。其他平台退化为
        传统文本输入，仍保持回车默认确认。
        """

        with self._lock:
            print(f"\n{prompt}")
            print(self.muted("Enter=YES，右箭头/N=NO"), flush=True)

        try:
            import msvcrt
        except ImportError:
            answer = input("确认？[Enter=YES / n=NO] ").strip().lower()
            return answer not in {"n", "no", "否", "false"}

        while True:
            char = msvcrt.getwch()
            if char in {"\r", "\n"}:
                return True
            if char in {"n", "N"}:
                return False
            if char in {"\x00", "\xe0"}:
                key = msvcrt.getwch()
                if key == "M":
                    return False


class StatusLine:
    """当前行上的弱提示和等待动画。

    同一行重绘只在支持 ANSI 时启用；否则每次 show 都退化为普通状态行，
    避免把转义字符显示给用户。
    """

    def __init__(self, ui: TerminalUI, base_text: str = "") -> None:
        self._ui = ui
        self._base_text = base_text
        self._visible = False

    def show(self, text: str) -> None:
        with self._ui._lock:
            if self._ui.capabilities.ansi:
                print(
                    f"\r{ANSI_CLEAR_LINE}{self._base_text}{self._ui.muted(text)}",
                    end="",
                    flush=True,
                )
                self._visible = True
            elif not self._visible:
                print(self._ui.muted(text), flush=True)
                self._visible = True

    def clear(self) -> None:
        with self._ui._lock:
            if not self._visible:
                return
            if self._ui.capabilities.ansi:
                print(f"\r{ANSI_CLEAR_LINE}{self._base_text}", end="", flush=True)
            self._visible = False

    def clear_all(self) -> None:
        with self._ui._lock:
            if self._ui.capabilities.ansi:
                print(f"\r{ANSI_CLEAR_LINE}", end="", flush=True)
            self._visible = False

    def new_line_for_input(self, prefix: str = USER_PREFIX) -> None:
        with self._ui._lock:
            if self._ui.capabilities.ansi:
                print(f"\r{ANSI_CLEAR_LINE}", end="", flush=True)
            print(f"{prefix} ", end="", flush=True)
            self._visible = False


class WaitingIndicator:
    """模型返回前的轻量等待动画。"""

    def __init__(self, status_line: StatusLine) -> None:
        self._status_line = status_line
        self._stop = threading.Event()
        self._thread: threading.Thread | None = None

    def start(self) -> None:
        self._stop.clear()
        self._thread = threading.Thread(target=self._run, daemon=True)
        self._thread.start()

    def stop(self) -> None:
        self._stop.set()
        if self._thread is not None:
            self._thread.join(timeout=1.0)
            self._thread = None
        self._status_line.clear()

    def _run(self) -> None:
        kaomoji = random.choice(WAITING_KAOMOJI)
        dot_index = 0
        while not self._stop.is_set():
            dots = WAITING_DOTS[dot_index % len(WAITING_DOTS)]
            self._status_line.show(f"按 Enter 打断  {kaomoji}{dots}")
            dot_index += 1
            self._stop.wait(0.35)
