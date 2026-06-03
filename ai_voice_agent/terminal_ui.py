from __future__ import annotations

import os
import random
import re
import shutil
import sys
import threading
import ctypes
import unicodedata
from dataclasses import dataclass


AI_PREFIX = "^"
USER_PREFIX = ">"
ANSI_CLEAR_LINE = "\033[2K"
ANSI_CLEAR_TO_LINE_END = "\033[K"
ANSI_PREVIOUS_LINE = "\033[1A"
ANSI_MUTED = "\033[2;90m"
ANSI_GRAY = "\033[90m"
ANSI_LIGHT_BLUE = "\033[94m"
ANSI_BOLD = "\033[1m"
ANSI_DIM_YELLOW = "\033[2;33m"
ANSI_RESET = "\033[0m"
ANSI_SAVE_CURSOR = "\033[s"
ANSI_RESTORE_CURSOR = "\033[u"
ANSI_ERASE_TO_END = "\033[J"
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


@dataclass
class MarkdownSpan:
    """Markdown 行内片段，style 为 ANSI SGR 前缀。"""

    text: str
    style: str | None = None


@dataclass
class MarkdownStreamState:
    """记录流式 Markdown 渲染所需的跨行状态。

    TUI 中的 AI 回复是按增量返回的，当前行会反复重绘成 Markdown 预览；
    代码块围栏会跨多行生效，因此需要把状态保存在播放器实例里。
    """

    in_code_block: bool = False
    rendered_lines: int = 0
    pending_line: str = ""
    preview_width: int = 0
    preview_visible: bool = False
    preview_needs_newline: bool = False
    passthrough_line: bool = False
    passthrough_printed_chars: int = 0


def _style(*styles: str | None) -> str | None:
    return "".join(style for style in styles if style) or None


def _append_markdown_span(
    spans: list[MarkdownSpan],
    text: str,
    style: str | None,
) -> None:
    if not text:
        return
    if spans and spans[-1].style == style:
        spans[-1].text += text
    else:
        spans.append(MarkdownSpan(text, style))


def _parse_inline_markdown(text: str, default_style: str | None = None) -> list[MarkdownSpan]:
    """解析 TUI 中最常见的行内 Markdown 标记。

    这里保持轻量，不引入 rich 等新依赖；目标是让 AI 常输出的标题、列表、
    加粗、代码和链接不再以原始 Markdown 标记裸露在终端里。
    """

    pattern = re.compile(
        r"(`[^`\n]+`|\*\*[^*\n]+\*\*|__[^_\n]+__|\*[^*\n]+\*|_[^_\n]+_|\[[^\]]+\]\([^)]+\))"
    )
    spans: list[MarkdownSpan] = []
    cursor = 0
    for match in pattern.finditer(text):
        _append_markdown_span(spans, text[cursor : match.start()], default_style)
        token = match.group(0)

        if token.startswith("`") and token.endswith("`"):
            _append_markdown_span(spans, token[1:-1], _style(ANSI_BOLD, default_style))
        elif token.startswith(("**", "__")) and token.endswith(("**", "__")):
            _append_markdown_span(spans, token[2:-2], _style(ANSI_BOLD, default_style))
        elif token.startswith("["):
            link_match = re.fullmatch(r"\[([^\]]+)\]\(([^)]+)\)", token)
            if link_match is not None:
                label, url = link_match.groups()
                _append_markdown_span(spans, f"{label} ({url})", default_style)
            else:
                _append_markdown_span(spans, token, default_style)
        else:
            _append_markdown_span(spans, token[1:-1], default_style)

        cursor = match.end()

    _append_markdown_span(spans, text[cursor:], default_style)
    return spans or [MarkdownSpan("", default_style)]


def _render_markdown_stream_line(
    raw_line: str,
    state: MarkdownStreamState,
    default_style: str | None = None,
) -> list[MarkdownSpan] | None:
    """把一行 Markdown 转成终端可写的片段。

    返回 None 表示这一行只是代码块围栏，不应单独显示。
    """

    stripped = raw_line.strip()
    if stripped.startswith("```"):
        state.in_code_block = not state.in_code_block
        return None

    if state.in_code_block:
        return [MarkdownSpan(f"    {raw_line}", default_style)]

    if not stripped:
        return [MarkdownSpan("", default_style)]

    heading_match = re.match(r"^\s{0,3}(#{1,6})\s+(.+?)\s*#*\s*$", raw_line)
    if heading_match is not None:
        return _parse_inline_markdown(heading_match.group(2), _style(ANSI_BOLD, default_style))

    if re.match(r"^\s{0,3}([-*_]\s*){3,}$", raw_line):
        return [MarkdownSpan("─" * 20, ANSI_MUTED)]

    quote_match = re.match(r"^\s{0,3}>\s?(.*)$", raw_line)
    if quote_match is not None:
        spans = [MarkdownSpan("│ ", ANSI_LIGHT_BLUE)]
        spans.extend(_parse_inline_markdown(quote_match.group(1), default_style))
        return spans

    task_match = re.match(r"^(\s*)[-*+]\s+\[([ xX])\]\s+(.*)$", raw_line)
    if task_match is not None:
        indent, checked, body = task_match.groups()
        marker = "[x] " if checked.lower() == "x" else "[ ] "
        spans = [MarkdownSpan(indent + marker, default_style)]
        spans.extend(_parse_inline_markdown(body, default_style))
        return spans

    list_match = re.match(r"^(\s*)([-*+]|\d+[.)])\s+(.*)$", raw_line)
    if list_match is not None:
        indent, marker, body = list_match.groups()
        normalized_marker = f"{marker} " if marker[0].isdigit() else "- "
        spans = [MarkdownSpan(indent + normalized_marker, default_style)]
        spans.extend(_parse_inline_markdown(body, default_style))
        return spans

    return _parse_inline_markdown(raw_line, default_style)


def _render_markdown_preview_line(
    raw_line: str,
    state: MarkdownStreamState,
    default_style: str | None = None,
) -> list[MarkdownSpan] | None:
    """渲染当前未提交行的预览，不改变代码块等跨行状态。"""

    preview_state = MarkdownStreamState(in_code_block=state.in_code_block)
    return _render_markdown_stream_line(raw_line, preview_state, default_style)


def _spans_display_width(spans: list[MarkdownSpan]) -> int:
    return sum(_display_width(span.text) for span in spans)


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

    def __init__(
        self,
        capabilities: TerminalCapabilities | None = None,
        *,
        model_label: str | None = None,
    ) -> None:
        self.capabilities = capabilities or detect_capabilities()
        self.model_label = model_label
        self._lock = threading.Lock()

    def muted(self, text: str) -> str:
        if not self.capabilities.ansi:
            return text
        return f"{ANSI_MUTED}{text}{ANSI_RESET}"

    def print_startup_panel(
        self,
        title: str,
        lines: list[str],
    ) -> None:
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

    def mark_transient_output_start(self) -> bool:
        """标记临时启动输出起点，便于初始化完成后清空麦克风选择等信息。"""

        if not self.capabilities.ansi:
            return False

        with self._lock:
            print(ANSI_SAVE_CURSOR, end="", flush=True)
        return True

    def clear_transient_output(self) -> None:
        """清空从最近一次标记起点到当前光标之间的临时启动输出。"""

        if not self.capabilities.ansi:
            return

        with self._lock:
            print(f"{ANSI_RESTORE_CURSOR}{ANSI_ERASE_TO_END}", end="", flush=True)

    def prompt(self) -> str:
        return f"\n{USER_PREFIX} "

    def prompt_width(self) -> int:
        return _display_width(f"{USER_PREFIX} ")

    def print_prompt_status(self, cursor_column: int = 0) -> None:
        """在当前输入行下方显示模型状态，并把光标放回输入行。"""

        if not self.model_label or not self.capabilities.ansi:
            return

        line = f"- {self.model_label}"
        cursor_target = max(1, self.prompt_width() + cursor_column + 1)

        with self._lock:
            print(
                f"\n{ANSI_CLEAR_LINE}{ANSI_DIM_YELLOW}{line}{ANSI_RESET}"
                f"{ANSI_PREVIOUS_LINE}\033[{cursor_target}G",
                end="",
                flush=True,
            )

    def clear_prompt_status(self) -> None:
        """清空输入行下方的模型状态行，保持光标回到输入行。"""

        if not self.model_label or not self.capabilities.ansi:
            return

        with self._lock:
            print(f"\n{ANSI_CLEAR_LINE}{ANSI_PREVIOUS_LINE}", end="", flush=True)

    def replace_current_input_with_status(self, message: str) -> None:
        """用灰色弱提示覆盖当前输入行，用于录音过程中的临时状态。

        用户空回车触发录音后，光标已经停在下一行；这里先回到上一行清空原来的
        `> ` 输入提示，再写入最新状态。后续状态继续覆盖同一行，避免把麦克风、
        校准和开始说话提示刷成多行日志。
        """

        status_text = self._single_line_status_text(message)
        with self._lock:
            if self.capabilities.ansi:
                print(
                    f"{ANSI_PREVIOUS_LINE}\r{ANSI_CLEAR_LINE}{self.muted(status_text)}\n",
                    end="",
                    flush=True,
                )
            else:
                print(self.muted(status_text), flush=True)

    @staticmethod
    def _single_line_status_text(message: str) -> str:
        text = " ".join(str(message).splitlines())
        width = shutil.get_terminal_size((100, 30)).columns
        max_width = max(1, width)
        if _display_width(text) <= max_width:
            return text
        if max_width <= 3:
            return _take_display_width(text, max_width)
        return f"{_take_display_width(text, max_width - 3)}..."

    def clear_current_input_status(self) -> None:
        """清空录音状态占用的临时输入行。"""

        if not self.capabilities.ansi:
            return

        with self._lock:
            print(f"{ANSI_PREVIOUS_LINE}\r{ANSI_CLEAR_LINE}", end="", flush=True)

    def inline_turn_base(self, user_text: str) -> str:
        """把已提交的用户输入固定成独立对话行。

        输入读取结束后终端已经换到下一行。这里只在当前空行位置重绘上一行，
        不再把等待动画挂在用户文本后面，避免 AI 回复和问题挤到同一行。
        """

        text = f"{USER_PREFIX} {user_text}"
        with self._lock:
            if self.capabilities.ansi:
                print(f"{ANSI_PREVIOUS_LINE}\r{ANSI_CLEAR_LINE}{text}\n", end="", flush=True)
        return ""

    def print_ai_prefix(self) -> None:
        with self._lock:
            print(f"{AI_PREFIX} ", end="", flush=True)

    def write(self, text: str) -> None:
        with self._lock:
            print(text, end="", flush=True)

    def write_markdown_delta(
        self,
        delta: str,
        state: MarkdownStreamState,
    ) -> None:
        """流式写入 AI 回复，并实时重绘当前行的轻量 Markdown 预览。"""

        state.pending_line += delta
        while "\n" in state.pending_line:
            line, state.pending_line = state.pending_line.split("\n", 1)
            if state.passthrough_line:
                self._finish_passthrough_line(line, state, newline=True)
            else:
                line_already_previewed = state.preview_visible
                self._clear_markdown_preview(state)
                self._write_markdown_line(
                    line,
                    state,
                    line_already_started=line_already_previewed,
                )
                state.preview_needs_newline = True
        self._write_markdown_preview(state)

    def flush_markdown(self, state: MarkdownStreamState) -> None:
        """回复结束时渲染最后一个没有换行结尾的 Markdown 片段。"""

        if state.pending_line:
            if state.passthrough_line:
                self._finish_passthrough_line(state.pending_line, state, newline=False)
                state.pending_line = ""
                return

            if state.preview_visible:
                state.pending_line = ""
                state.preview_visible = False
                state.preview_width = 0
                return

            self._clear_markdown_preview(state)
            line = state.pending_line
            state.pending_line = ""
            self._write_markdown_line(line, state, line_already_started=False)

    def _write_markdown_line(
        self,
        line: str,
        state: MarkdownStreamState,
        *,
        line_already_started: bool = False,
    ) -> None:
        spans = _render_markdown_stream_line(line, state)
        if spans is None:
            return

        with self._lock:
            if state.rendered_lines > 0 and not line_already_started:
                print()
            for span in spans:
                if self.capabilities.ansi and span.style:
                    print(f"{span.style}{span.text}{ANSI_RESET}", end="")
                else:
                    print(span.text, end="")
            sys.stdout.flush()
        state.rendered_lines += 1

    def _write_markdown_preview(self, state: MarkdownStreamState) -> None:
        if not state.pending_line or not self.capabilities.ansi:
            return

        if state.passthrough_line:
            self._write_passthrough_delta(state)
            return

        spans = _render_markdown_preview_line(state.pending_line, state)
        if spans is None:
            return
        if _spans_display_width(spans) > self._markdown_preview_max_width():
            self._start_passthrough_line(state)
            return

        with self._lock:
            if state.preview_needs_newline:
                print()
                state.preview_needs_newline = False
            elif state.preview_visible and state.preview_width > 0:
                print(f"\033[{state.preview_width}D", end="")
            for span in spans:
                if span.style:
                    print(f"{span.style}{span.text}{ANSI_RESET}", end="")
                else:
                    print(span.text, end="")
            print(ANSI_CLEAR_TO_LINE_END, end="", flush=True)
        state.preview_width = _spans_display_width(spans)
        state.preview_visible = True

    @staticmethod
    def _markdown_preview_max_width() -> int:
        terminal_width = shutil.get_terminal_size((100, 30)).columns
        # 只对单个视觉行做原地重绘。长行交给直写流式输出，避免终端自动换行后
        # 光标回退只能回到当前视觉行，导致上一轮预览残留并反复叠加。
        return max(20, terminal_width - 4)

    def _start_passthrough_line(self, state: MarkdownStreamState) -> None:
        self._clear_markdown_preview(state)
        if state.preview_needs_newline:
            with self._lock:
                print()
            state.preview_needs_newline = False
        with self._lock:
            print(state.pending_line, end="", flush=True)
        state.passthrough_line = True
        state.passthrough_printed_chars = len(state.pending_line)

    def _write_passthrough_delta(self, state: MarkdownStreamState) -> None:
        unprinted = state.pending_line[state.passthrough_printed_chars :]
        if not unprinted:
            return
        with self._lock:
            print(unprinted, end="", flush=True)
        state.passthrough_printed_chars = len(state.pending_line)

    def _finish_passthrough_line(
        self,
        line: str,
        state: MarkdownStreamState,
        *,
        newline: bool,
    ) -> None:
        unprinted = line[state.passthrough_printed_chars :]
        with self._lock:
            if unprinted:
                print(unprinted, end="")
            if newline:
                print()
            sys.stdout.flush()

        # 直写长行时不再重排 Markdown，但仍让围栏状态随完整行推进，
        # 避免长代码行之后的代码块状态错乱。
        _render_markdown_stream_line(line, state)
        state.rendered_lines += 1
        state.passthrough_line = False
        state.passthrough_printed_chars = 0
        state.preview_needs_newline = False

    def _clear_markdown_preview(self, state: MarkdownStreamState) -> None:
        if not state.preview_visible or not self.capabilities.ansi:
            return

        with self._lock:
            if state.preview_width > 0:
                print(f"\033[{state.preview_width}D", end="")
            print(ANSI_CLEAR_TO_LINE_END, end="", flush=True)
        state.preview_width = 0
        state.preview_visible = False

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
                    f"\r{ANSI_CLEAR_LINE}{self._ui.muted(text)}",
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
                print(f"\r{ANSI_CLEAR_LINE}", end="", flush=True)
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
