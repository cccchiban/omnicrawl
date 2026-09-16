"""Windows 终端适配：原始输入驱动、事件监视器与自愈/滚动条交互。

P1 重构从 ``ui/fullscreen/__init__.py`` 拆出的独立模块（2026-08-11）。
这里集中 Windows 控制台相关的所有代码：控制台输入模式恢复、鼠标报告兜底
关闭、原始输入事件监视器、输入驱动，以及 ``TerminalHandlingMixin``
（原 ``OmniCrawlApp`` 的失焦/聚焦/滚动条拖动/看门狗方法）。

``__init__.py`` 通过 re-export 保持全部对外名称不变，测试 patch 路径
（``omnicrawl.ui.fullscreen._restore_windows_vt_input_mode_if_needed``）
与 ``from omnicrawl.ui.fullscreen import OmniCrawlWindowsDriver`` 均不受影响。
"""

from __future__ import annotations

import logging
import sys
import time
from typing import Any, Callable

from textual import events
from textual.geometry import Offset
from textual.scrollbar import ScrollBar
from textual.widgets import TextArea

if sys.platform == "win32":
    from textual.drivers import win32 as _textual_win32
    from textual.drivers.windows_driver import WindowsDriver as _TextualWindowsDriver
else:
    _textual_win32 = None
    _TextualWindowsDriver = object

LOGGER = logging.getLogger(__name__)

# 输入流开头的控制字符（ASCII < 0x20 且非 \t\r\n）。
# conhost 在某些配置下会把 Ctrl+V 等快捷键透传为 \x16 之类的控制字符记录，
# 粘贴文本比较前剥离，避免把用户主动按下的快捷键误算进粘贴内容。
_STREAM_CONTROL_PREFIX_CHARS = "".join(
    chr(code) for code in range(32) if chr(code) not in "\t\r\n"
)


_MOUSE_REPORTING_DISABLE_SEQUENCE = (
    "\x1b[?1000l"
    "\x1b[?1002l"
    "\x1b[?1003l"
    "\x1b[?1015l"
    "\x1b[?1006l"
)

# 滚轮由应用自己处理（每刻度滚动消息区 5 行），因此额外关闭终端的
# 「滚轮→上下键」回退（alternate scroll，DECSET 1007）：鼠标模式被外部重置的
# 窗口里，滚轮不能退化成方向键——输入框会把上下键当历史回看，冒出上一条提问。
_MOUSE_REPORTING_ENABLE_SEQUENCE = (
    "\x1b[?1000h"
    "\x1b[?1003h"
    "\x1b[?1015h"
    "\x1b[?1006h"
    "\x1b[?1007l"
)
_ALTERNATE_SCROLL_RESTORE_SEQUENCE = "\x1b[?1007h"


def _disable_terminal_mouse_reporting(output_stream: Any | None = None) -> None:
    """在 Textual Driver 停止后兜底关闭鼠标报告，并还原终端 alternate scroll。"""

    output_stream = sys.__stdout__ if output_stream is None else output_stream
    if output_stream is None:
        return
    try:
        # 退出阶段不能继续使用 Driver.write：Windows WriterThread 此时已经停止，
        # 写入只会滞留在无人消费的队列中，因此必须直接写回真实控制台流。
        output_stream.write(_MOUSE_REPORTING_DISABLE_SEQUENCE)
        output_stream.write(_ALTERNATE_SCROLL_RESTORE_SEQUENCE)
        output_stream.flush()
    except (AttributeError, OSError, ValueError):
        # 关闭窗口或重定向流已提前失效时，不应让兜底清理覆盖原始退出结果。
        return


def _restore_windows_vt_input_mode_if_needed(
    *,
    platform_name: str | None = None,
    input_stream: Any | None = None,
    output_stream: Any | None = None,
    win32_api: Any | None = None,
) -> bool:
    """恢复可能被锁屏或息屏重置的 Windows 控制台 VT 输入模式。"""

    if (platform_name or sys.platform) != "win32":
        return False

    if win32_api is None:
        from textual.drivers import win32 as win32_api

    input_stream = sys.__stdin__ if input_stream is None else input_stream
    output_stream = sys.__stdout__ if output_stream is None else output_stream
    if input_stream is None or output_stream is None:
        return False

    try:
        input_mode = win32_api.get_console_mode(input_stream)
        required_input_mode = win32_api.ENABLE_VIRTUAL_TERMINAL_INPUT
        if input_mode == required_input_mode:
            return False

        # Textual 的 Windows 输入线程只解析 VT 字符序列；普通控制台模式下，
        # 方向键会变成空字符的虚拟键记录，鼠标会变成其未处理的 MOUSE_EVENT。
        if not win32_api.set_console_mode(input_stream, required_input_mode):
            return False

        output_mode = win32_api.get_console_mode(output_stream)
        win32_api.set_console_mode(
            output_stream,
            output_mode | win32_api.ENABLE_VIRTUAL_TERMINAL_PROCESSING,
        )
        return True
    except (AttributeError, OSError, ValueError):
        # 重定向输入、非控制台 Host 或终端关闭期间可能无法读取 ConsoleMode；
        # 此时保留 Textual 当前状态，避免定时器异常打断主事件循环。
        return False


def _restore_windows_raw_input_mode_if_needed(
    *,
    platform_name: str | None = None,
    input_stream: Any | None = None,
    output_stream: Any | None = None,
    win32_api: Any | None = None,
) -> bool:
    """恢复可保留修饰键和鼠标事件的 Windows 原始控制台输入模式。"""

    if (platform_name or sys.platform) != "win32":
        return False

    if win32_api is None:
        from textual.drivers import win32 as win32_api

    input_stream = sys.__stdin__ if input_stream is None else input_stream
    output_stream = sys.__stdout__ if output_stream is None else output_stream
    if input_stream is None or output_stream is None:
        return False

    try:
        input_mode = win32_api.get_console_mode(input_stream)
        required_input_mode = (
            input_mode
            | win32_api.ENABLE_MOUSE_INPUT
            | win32_api.ENABLE_WINDOW_INPUT
            | win32_api.ENABLE_EXTENDED_FLAGS
        ) & ~(
            win32_api.ENABLE_QUICK_EDIT_MODE
            | win32_api.ENABLE_VIRTUAL_TERMINAL_INPUT
        )
        if input_mode == required_input_mode:
            return False
        if not win32_api.set_console_mode(input_stream, required_input_mode):
            return False

        output_mode = win32_api.get_console_mode(output_stream)
        win32_api.set_console_mode(
            output_stream,
            output_mode | win32_api.ENABLE_VIRTUAL_TERMINAL_PROCESSING,
        )
        return True
    except (AttributeError, OSError, ValueError):
        return False


def _read_windows_clipboard_text() -> str | None:
    """读取 Windows 剪贴板 CF_UNICODETEXT；不可用或非文本时返回 None。

    供输入监视器把多行粘贴按键流识别为 Paste 事件。复用
    ``copy_to_clipboard`` 的 ctypes 风格：64 位系统上 HGLOBAL 是指针，
    必须显式声明 restype，否则 ctypes 按 32 位 int 截断句柄。剪贴板被
    其他进程锁定或 API 异常时静默返回 None，由调用方降级为普通按键流。
    """

    if sys.platform != "win32":
        return None
    try:
        import ctypes
        from ctypes import wintypes

        CF_UNICODETEXT = 13
        user32 = ctypes.windll.user32
        kernel32 = ctypes.windll.kernel32
        user32.OpenClipboard.argtypes = [wintypes.HWND]
        user32.GetClipboardData.argtypes = [wintypes.UINT]
        user32.GetClipboardData.restype = wintypes.HANDLE
        user32.IsClipboardFormatAvailable.argtypes = [wintypes.UINT]
        kernel32.GlobalLock.restype = ctypes.c_void_p
        kernel32.GlobalLock.argtypes = [wintypes.HGLOBAL]
        kernel32.GlobalUnlock.argtypes = [wintypes.HGLOBAL]

        if not user32.OpenClipboard(None):
            return None
        try:
            if not user32.IsClipboardFormatAvailable(CF_UNICODETEXT):
                return None
            handle = user32.GetClipboardData(CF_UNICODETEXT)
            if not handle:
                return None
            locked = kernel32.GlobalLock(handle)
            if not locked:
                return None
            try:
                # CF_UNICODETEXT 是 NUL 结尾的宽字符串；按首个 NUL 截止，
                # 不读取 GlobalSize 可能包含的对齐或填充字节。
                return ctypes.wstring_at(locked)
            finally:
                kernel32.GlobalUnlock(handle)
        finally:
            user32.CloseClipboard()
    except Exception:
        return None



class OmniCrawlWindowsEventMonitor(
    _textual_win32.EventMonitor if _textual_win32 is not None else object
):
    """将 Windows 原始控制台记录转换为 Textual 事件。"""

    WINDOWS_ALT_PRESSED = 0x0003
    WINDOWS_CTRL_PRESSED = 0x000C
    WINDOWS_SHIFT_PRESSED = 0x0010
    WINDOWS_RETURN_KEY = 0x000D
    WINDOWS_TAB_KEY = 0x0009
    WINDOWS_ESCAPE_KEY = 0x001B
    WINDOWS_MODIFIER_KEYS = {0x0010, 0x0011, 0x0012}
    WINDOWS_SPECIAL_KEYS = {
        0x0021: "pageup",
        0x0022: "pagedown",
        0x0023: "end",
        0x0024: "home",
        0x0025: "left",
        0x0026: "up",
        0x0027: "right",
        0x0028: "down",
        0x002D: "insert",
        0x002E: "delete",
    }
    WINDOWS_MOUSE_BUTTONS = (
        (0x0001, 1),
        (0x0004, 2),
        (0x0002, 3),
        (0x0008, 4),
        (0x0010, 5),
    )
    WINDOWS_MOUSE_BUTTON_MASK = 0x001F
    WINDOWS_MOUSE_MOVED = 0x0001
    WINDOWS_MOUSE_DOUBLE_CLICK = 0x0002
    WINDOWS_MOUSE_WHEELED = 0x0004
    WINDOWS_MOUSE_HWHEELED = 0x0008

    PASTE_PREFIX_TIMEOUT = 0.5
    """已含换行的粘贴前缀停止到码后，最长等待时间（秒）。"""

    PASTE_SINGLE_LINE_PREFIX_TIMEOUT = 0.05
    """尚无换行的候选前缀等待窗口，避免普通输入长期不显示。"""

    def __init__(self, *args: Any, **kwargs: Any) -> None:
        if _textual_win32 is not None:
            # 仅在 Windows 下传给 EventMonitor；非 win32 平台基类是 object，
            # 不接受参数（此时类只被测试实例化，用于验证纯逻辑方法）。
            super().__init__(*args, **kwargs)
        # 大文本粘贴会分多次 ReadConsoleInputW 到达；前缀匹配挂起期间记录
        # 起始时刻，超时后强制按普通按键流处理，避免用户输入被无限暂存。
        self._pending_prefix_since: float | None = None
        self._pending_prefix_length = 0
        self._pending_prefix_timeout = self.PASTE_PREFIX_TIMEOUT
        # 可注入的剪贴板读取器，测试时替换为假实现。
        self._clipboard_reader: Callable[[], str | None] = _read_windows_clipboard_text


    @classmethod
    def _key_with_modifiers(cls, key: str, control_key_state: int) -> str:
        """生成与 Textual XTermParser 一致的修饰键名称。"""

        modifiers: list[str] = []
        if control_key_state & cls.WINDOWS_ALT_PRESSED:
            modifiers.append("alt")
        if control_key_state & cls.WINDOWS_CTRL_PRESSED:
            modifiers.append("ctrl")
        if control_key_state & cls.WINDOWS_SHIFT_PRESSED:
            modifiers.append("shift")
        return "+".join([*modifiers, key])

    @classmethod
    def key_event_to_textual(cls, key_event: Any) -> events.Key | None:
        """转换需依赖虚拟键码的原始 Windows 按键事件。"""

        if not getattr(key_event, "bKeyDown", False):
            return None

        character = getattr(key_event.uChar, "UnicodeChar", "")
        virtual_key = int(getattr(key_event, "wVirtualKeyCode", 0))
        control_key_state = int(getattr(key_event, "dwControlKeyState", 0))
        if virtual_key in cls.WINDOWS_MODIFIER_KEYS:
            return None

        if (
            virtual_key == cls.WINDOWS_RETURN_KEY
            and character in {"\r", "\n"}
            and control_key_state & cls.WINDOWS_SHIFT_PRESSED
        ):
            return events.Key(
                cls._key_with_modifiers("enter", control_key_state), character
            )
        if (
            virtual_key == cls.WINDOWS_TAB_KEY
            and character == "\t"
            and control_key_state & cls.WINDOWS_SHIFT_PRESSED
        ):
            return events.Key(
                cls._key_with_modifiers("tab", control_key_state), None
            )

        key_name = cls.WINDOWS_SPECIAL_KEYS.get(virtual_key)
        if key_name is None and 0x0070 <= virtual_key <= 0x0087:
            key_name = f"f{virtual_key - 0x006F}"
        if key_name is not None and character == "\x00":
            return events.Key(
                cls._key_with_modifiers(key_name, control_key_state), None
            )
        if virtual_key == cls.WINDOWS_ESCAPE_KEY:
            return events.Key("escape", None)
        return None

    @classmethod
    def mouse_events_from_raw(
        cls,
        mouse_event: Any,
        *,
        previous_button_state: int,
        previous_position: tuple[int, int],
    ) -> tuple[list[events.MouseEvent], int, tuple[int, int]]:
        """将原始鼠标记录转换为 Textual 鼠标消息并保留按钮状态。"""

        x = int(mouse_event.dwMousePosition.X)
        y = int(mouse_event.dwMousePosition.Y)
        delta_x = x - previous_position[0]
        delta_y = y - previous_position[1]
        control_key_state = int(mouse_event.dwControlKeyState)
        button_state = int(mouse_event.dwButtonState) & cls.WINDOWS_MOUSE_BUTTON_MASK
        event_flags = int(mouse_event.dwEventFlags)
        shift = bool(control_key_state & cls.WINDOWS_SHIFT_PRESSED)
        meta = bool(control_key_state & cls.WINDOWS_ALT_PRESSED)
        ctrl = bool(control_key_state & cls.WINDOWS_CTRL_PRESSED)

        def make_mouse_event(
            event_type: type[events.MouseEvent], button: int = 0
        ) -> events.MouseEvent:
            return event_type(
                None,
                x,
                y,
                delta_x,
                delta_y,
                button,
                shift,
                meta,
                ctrl,
                screen_x=x,
                screen_y=y,
            )

        messages: list[events.MouseEvent] = []
        if event_flags & cls.WINDOWS_MOUSE_WHEELED:
            wheel_delta = (int(mouse_event.dwButtonState) >> 16) & 0xFFFF
            if wheel_delta & 0x8000:
                wheel_delta -= 0x10000
            if wheel_delta:
                event_type = (
                    events.MouseScrollUp
                    if wheel_delta > 0
                    else events.MouseScrollDown
                )
                messages.append(make_mouse_event(event_type))
        elif event_flags & cls.WINDOWS_MOUSE_HWHEELED:
            wheel_delta = (int(mouse_event.dwButtonState) >> 16) & 0xFFFF
            if wheel_delta & 0x8000:
                wheel_delta -= 0x10000
            if wheel_delta:
                event_type = (
                    events.MouseScrollRight
                    if wheel_delta > 0
                    else events.MouseScrollLeft
                )
                messages.append(make_mouse_event(event_type))
        elif event_flags & cls.WINDOWS_MOUSE_MOVED:
            button = next(
                (
                    button_number
                    for mask, button_number in cls.WINDOWS_MOUSE_BUTTONS
                    if button_state & mask
                ),
                0,
            )
            messages.append(make_mouse_event(events.MouseMove, button))
        else:
            released = previous_button_state & ~button_state
            pressed = button_state & ~previous_button_state
            for mask, button in cls.WINDOWS_MOUSE_BUTTONS:
                if released & mask:
                    messages.append(make_mouse_event(events.MouseUp, button))
            for mask, button in cls.WINDOWS_MOUSE_BUTTONS:
                if pressed & mask:
                    messages.append(make_mouse_event(events.MouseDown, button))

        return messages, button_state, (x, y)

    @staticmethod
    def _normalize_stream_text(text: str) -> str:
        """把按键流/剪贴板文本统一为 LF 换行后比较。"""

        return text.replace("\r\n", "\n").replace("\r", "\n")

    @staticmethod
    def _strip_stream_control_prefix(text: str) -> str:
        """剥离流开头的控制字符（\x16 等快捷键透传残留），不影响正文。"""

        return text.lstrip(_STREAM_CONTROL_PREFIX_CHARS)

    def _paste_event_for_stream(self, text: str) -> events.Paste | None:
        """按键字符流与剪贴板完整匹配时返回 Paste 事件，否则 None。

        只处理含换行的多行流：单行粘贴走普通按键路径即可正确插入，
        避免为每次击键读取剪贴板。完整匹配才判定为粘贴，把多行内容
        以一次 Paste 事件交给输入框（走已有的压缩/展开链路）。
        """

        if "\r" not in text and "\n" not in text:
            return None
        clipboard = self._clipboard_reader()
        if not clipboard:
            return None
        stripped = self._strip_stream_control_prefix(text)
        if self._normalize_stream_text(stripped) == self._normalize_stream_text(
            clipboard
        ):
            return events.Paste(clipboard)
        return None

    def _is_pending_clipboard_prefix(self, text: str) -> bool:
        """按键流是否为多行剪贴板的严格前缀（等待后续 read 批次）。

        首批记录可能在剪贴板首个换行到达前就已读满，甚至只有一个
        文本字符，因此不能要求当前流自身含换行；只要剪贴板是多行文本
        且规范化后严格前缀匹配，就短暂等待后续批次。单独的换行仍按
        普通 Enter 处理，避免提交键被挂起。
        """

        if text in {"\r", "\n", "\r\n"}:
            return False
        stripped = self._strip_stream_control_prefix(text)
        # 剥离前导控制字符后为空（如长按退格积累的 \x08 流）时不能视为
        # 粘贴前缀：空串是任意多行文本的前缀，会把编辑键无限挂起，直到
        # 后续输入打破前缀匹配才一次性冲刷，表现为“先不动、松键后猛删”。
        if not stripped:
            return False
        clipboard = self._clipboard_reader()
        if not clipboard or ("\r" not in clipboard and "\n" not in clipboard):
            return False
        normalized_stream = self._normalize_stream_text(stripped)
        normalized_clipboard = self._normalize_stream_text(clipboard)
        return (
            normalized_clipboard.startswith(normalized_stream)
            and normalized_stream != normalized_clipboard
        )

    def _should_flush_before_return(self, text: str) -> bool:
        """根据当前缓冲判断 Return 是否仍属于候选多行粘贴。"""

        candidate = f"{text}\r"
        if self._paste_event_for_stream(candidate) is not None:
            return False
        if (
            self._pending_prefix_since is not None
            and time.monotonic() - self._pending_prefix_since
            > self._pending_prefix_timeout
        ):
            return True
        if not self._is_pending_clipboard_prefix(candidate):
            return True
        # 无法仅凭字符流区分“手输首行后 Enter”和“粘贴首个换行”；
        # 保留严格前缀，交给短前缀超时回退，避免跨 ReadConsoleInputW
        # 批次的粘贴在换行处被拆成提交事件。
        return False

    def run(self) -> None:
        if _textual_win32 is None:
            return

        win32 = _textual_win32
        exit_requested = self.exit_event.is_set
        parser = win32.XTermParser(debug=win32.constants.DEBUG)

        try:
            read_count = win32.wintypes.DWORD(0)
            h_in = win32.GetStdHandle(win32.STD_INPUT_HANDLE)
            max_events = 1024
            key_event_type = 0x0001
            mouse_event_type = 0x0002
            window_buffer_size_event = 0x0004
            focus_event_type = 0x0010
            input_records = (win32.INPUT_RECORD * max_events)()
            read_console_input = win32.KERNEL32.ReadConsoleInputW
            keys: list[str] = []
            mouse_button_state = 0
            mouse_position = (0, 0)

            def flush_keys(force: bool = False) -> None:
                if not keys:
                    return
                text = (
                    "".join(keys)
                    .encode("utf-16", "surrogatepass")
                    .decode("utf-16")
                )
                if not force:
                    # 多行粘贴在 raw input 模式下是普通按键记录流；与剪贴板
                    # 完整匹配时转为一次 Paste 事件，避免换行被解析成 Enter
                    # 导致逐段提交。前缀匹配说明大文本还在分批到达，暂缓冲刷。
                    paste_event = self._paste_event_for_stream(text)
                    if paste_event is not None:
                        self.process_event(paste_event)
                        del keys[:]
                        self._pending_prefix_since = None
                        self._pending_prefix_length = 0
                        return
                    if self._is_pending_clipboard_prefix(text):
                        normalized_text = self._normalize_stream_text(text)
                        normalized_length = len(normalized_text)
                        if normalized_length > self._pending_prefix_length:
                            self._pending_prefix_since = time.monotonic()
                            self._pending_prefix_length = normalized_length
                        # 只有首行及其换行时仍可能是普通 Enter，快速回退；
                        # 已收到第二行字符或更多换行后，给跨批大粘贴更长窗口。
                        first_newline = normalized_text.find("\n")
                        has_multiline_evidence = (
                            first_newline >= 0
                            and normalized_text[first_newline + 1 :] != ""
                        )
                        self._pending_prefix_timeout = (
                            self.PASTE_PREFIX_TIMEOUT
                            if has_multiline_evidence
                            else self.PASTE_SINGLE_LINE_PREFIX_TIMEOUT
                        )
                        return
                self._pending_prefix_since = None
                self._pending_prefix_length = 0
                for parsed_event in parser.feed(text):
                    self.process_event(parsed_event)
                del keys[:]

            while not exit_requested():
                for event in parser.tick():
                    self.process_event(event)

                if win32.wait_for_handles([h_in], 100) is None:
                    # 无新事件时仍要推进前缀挂起的超时：若用户手动输入的内容
                    # 恰好与剪贴板前缀相同且已停止输入，不能无限等待后续批次，
                    # 超时后强制按普通按键流处理。
                    if (
                        self._pending_prefix_since is not None
                        and time.monotonic() - self._pending_prefix_since
                        > self._pending_prefix_timeout
                    ):
                        self._pending_prefix_since = None
                        self._pending_prefix_length = 0
                        flush_keys(force=True)
                    continue

                read_console_input(
                    h_in,
                    win32.byref(input_records),
                    max_events,
                    win32.byref(read_count),
                )
                read_input_records = input_records[: read_count.value]
                new_size: tuple[int, int] | None = None

                for input_record in read_input_records:
                    event_type = input_record.EventType
                    if event_type == key_event_type:
                        key_event = input_record.Event.KeyEvent
                        normalized_event = self.key_event_to_textual(key_event)
                        if normalized_event is not None:
                            flush_keys()
                            self.process_event(normalized_event)
                        elif key_event.bKeyDown:
                            if (
                                key_event.wVirtualKeyCode == self.WINDOWS_RETURN_KEY
                                and self._should_flush_before_return("".join(keys))
                            ):
                                # 内部换行保留；普通 Enter 或粘贴后的 Enter
                                # 先冲刷已有文本，再单独作为提交键处理。
                                flush_keys()
                            key = key_event.uChar.UnicodeChar
                            if key and key != "\x00":
                                keys.append(key)
                    elif event_type == mouse_event_type:
                        flush_keys()
                        mouse_messages, mouse_button_state, mouse_position = (
                            self.mouse_events_from_raw(
                                input_record.Event.MouseEvent,
                                previous_button_state=mouse_button_state,
                                previous_position=mouse_position,
                            )
                        )
                        for mouse_message in mouse_messages:
                            self.process_event(mouse_message)
                    elif event_type == window_buffer_size_event:
                        size = input_record.Event.WindowBufferSizeEvent.dwSize
                        new_size = (size.X, size.Y)
                    elif event_type == focus_event_type:
                        flush_keys()
                        focus_event = input_record.Event.FocusEvent
                        self.process_event(
                            events.AppFocus()
                            if focus_event.bSetFocus
                            else events.AppBlur()
                        )

                flush_keys()
                if new_size is not None:
                    self.on_size_change(*new_size)
        except Exception as error:
            self.app.log.error("EVENT MONITOR ERROR", error)



class OmniCrawlWindowsDriver(_TextualWindowsDriver):
    """Windows 全屏输入驱动：使用原始控制台事件保留修饰键。"""

    KEYBOARD_PROTOCOL = "\x1b[>25u"

    def start_application_mode(self) -> None:
        original_event_monitor = _textual_win32.EventMonitor
        _textual_win32.EventMonitor = OmniCrawlWindowsEventMonitor
        try:
            super().start_application_mode()
        finally:
            _textual_win32.EventMonitor = original_event_monitor
        _restore_windows_raw_input_mode_if_needed()
        self.flush()


class TerminalHandlingMixin:
    """原 ``OmniCrawlApp`` 的 Windows 终端交互方法。

    P1 重构把失焦/聚焦清理、滚动条拖拽增强、输入模式自愈看门狗从巨型 App 类
    搬到这里。方法名、签名与行为保持逐字不变；``OmniCrawlApp`` 改为继承本
    混入类，实例属性（``_interaction_watchdog_signature`` 等）仍在
    ``OmniCrawlApp.__init__`` 初始化。
    """

    def on_app_blur(self, _event: events.AppBlur) -> None:
        """窗口失焦时清除未完成的交互状态。"""

        self._reset_mouse_interaction_state()

    def on_app_focus(self, _event: events.AppFocus) -> None:
        """窗口重新聚焦时恢复输入焦点和终端输入协议。"""

        self._reset_mouse_interaction_state(
            focus_composer=True,
            rearm_terminal_protocols=True,
        )

    async def on_event(self, event: events.Event) -> None:
        """在 Textual 标准处理之前补齐滚动条鼠标交互。

        Textual 8.2.7 的 ``ScrollBar`` 只对渲染在 thumb 上的 MouseDown 启动
        拖动（meta ``@mouse.down: grab``）；轨道上按下只触发一次性跳转，
        且无法“按住滑动”。这里让滚动条任意位置按住都能拖动：thumb 上原位
        抓取，轨道上先跳到点击位置对应的比例再抓取。

        拖动中的 MouseMove 由本方法直接换算并滚动容器：Textual 的
        ``ScrollBar._on_mouse_move`` 只把 ``ScrollTo`` 消息发给滚动条自身，
        而滚动条 ``_allow_scroll`` 恒为 False，消息实际不会生效，原生
        拖动因此不可靠。
        """

        captured_scrollbar = self.mouse_captured
        if (
            isinstance(captured_scrollbar, ScrollBar)
            and isinstance(event, (events.MouseMove, events.MouseUp))
            and event.button != 1
        ):
            # Textual 的滚动条鼠标处理不区分按键；非左键事件不能推进或结束
            # 已存在的左键拖动，也不能触发渲染元数据中的 grab 动作。
            return
        if isinstance(event, events.MouseMove) and isinstance(
            captured_scrollbar, ScrollBar
        ):
            self._drag_scrollbar_by_mouse(captured_scrollbar, event)
        elif isinstance(event, events.MouseDown) and not event.is_forwarded:
            try:
                widget, _region = self.get_widget_at(event.x, event.y)
            except Exception:  # noqa: BLE001
                widget = None
            if isinstance(widget, ScrollBar):
                if event.button != 1:
                    # 必须在进入 Textual 默认分发前返回；ScrollBarRender 的
                    # @mouse.down 元数据本身也不会校验鼠标按键。
                    return
                if not widget.grabbed:
                    self._begin_scrollbar_drag(widget, event)
        await super().on_event(event)

    def _drag_scrollbar_by_mouse(
        self,
        scrollbar: ScrollBar,
        event: events.MouseMove,
    ) -> None:
        """拖动中按鼠标位移换算目标位置并滚动容器（立即、无动画）。"""

        if not scrollbar.grabbed:
            return
        parent = scrollbar.parent
        virtual_size = scrollbar.window_virtual_size
        window_size = scrollbar.window_size
        if parent is None or not window_size or virtual_size <= window_size:
            return
        ratio = virtual_size / window_size
        if scrollbar.vertical:
            target = scrollbar.grabbed_position + (
                (event.screen_y - scrollbar.grabbed.y) * ratio
            )
            maximum = float(parent.max_scroll_y)
            parent.scroll_to(y=target, animate=False)
        else:
            target = scrollbar.grabbed_position + (
                (event.screen_x - scrollbar.grabbed.x) * ratio
            )
            maximum = float(parent.max_scroll_x)
            parent.scroll_to(x=target, animate=False)
        target = min(max(0.0, target), maximum)
        # 同步 thumb 位置：滚动容器的异步刷新会回填 position，这里先对齐
        # 保证 grab 起点与下次位移计算连续。
        scrollbar.position = target

    @staticmethod
    def _scrollbar_thumb_bounds(
        scrollbar: ScrollBar,
        bar_size: int,
    ) -> tuple[float, float]:
        """计算滚动条 thumb 在条身内的区间，与 ScrollBarRender 公式一致。"""

        virtual_size = scrollbar.window_virtual_size
        window_size = scrollbar.window_size
        if bar_size <= 0 or virtual_size <= window_size:
            return (0.0, float(bar_size))
        thumb_size = max(1.0, window_size * bar_size / virtual_size)
        position_ratio = scrollbar.position / (virtual_size - window_size)
        start = (bar_size - thumb_size) * position_ratio
        return (start, start + thumb_size)

    def _begin_scrollbar_drag(
        self,
        scrollbar: ScrollBar,
        event: events.MouseDown,
    ) -> None:
        """滚动条按下：thumb 上原位抓取；轨道上先按比例跳转再抓取。"""

        region = scrollbar.region
        if scrollbar.vertical:
            bar_size = region.height
            position_in_bar = event.screen_y - region.y
        else:
            bar_size = region.width
            position_in_bar = event.screen_x - region.x
        parent = scrollbar.parent
        thumb_start, thumb_end = self._scrollbar_thumb_bounds(scrollbar, bar_size)
        on_thumb = thumb_start <= position_in_bar <= thumb_end
        if not on_thumb and parent is not None and bar_size > 0:
            # 轨道点击：先跳到点击位置对应的滚动比例，再进入拖动状态，
            # 用户不移动即停在跳转点，继续移动则从该点拖动。
            ratio = min(1.0, max(0.0, position_in_bar / bar_size))
            if scrollbar.vertical:
                target = ratio * parent.max_scroll_y
                parent.scroll_to(y=target, animate=False)
            else:
                target = ratio * parent.max_scroll_x
                parent.scroll_to(x=target, animate=False)
            # scroll_to 的生效与 scrollbar.position 的同步是异步的；先手动
            # 对齐 thumb 位置，保证抓取后的拖动起点正确。
            scrollbar.position = target
        # 本方法在 App.on_event 更新 mouse_position 之前执行，capture_mouse
        # 会把它写进 MouseCapture.mouse_position（即 grabbed 起点）。不同步
        # 的话拖动偏移全错；且若起点恰好是 Offset(0,0)，ScrollBar._on_mouse_up
        # 的 ``if self.grabbed`` 判定为 False，MouseUp 后永不释放、交互卡死。
        self.mouse_position = Offset(event.screen_x, event.screen_y)
        scrollbar.action_grab()

    def _restore_terminal_input_mode(self) -> bool:
        """核对并恢复控制台输入模式；返回是否真的恢复过。

        工具子进程（cmd / python 等）会把共享控制台的鼠标输入位清掉，鼠标
        记录随之消失；工具一结束就立刻核对，不等周期看门狗。终端状态短暂
        不可读时按未恢复处理，不把异常抛给调用方。
        """

        try:
            driver = self._driver
            if driver.is_headless:
                return False
            restore_input_mode = (
                _restore_windows_raw_input_mode_if_needed
                if isinstance(driver, OmniCrawlWindowsDriver)
                else _restore_windows_vt_input_mode_if_needed
            )
            return bool(restore_input_mode())
        except Exception:  # noqa: BLE001 - 自愈边界，读取失败只跳过本次
            return False

    def _reassert_terminal_mouse_reporting(self) -> None:
        """重｛Desensitized:764｝终端鼠标报告，并保持 alternate scroll 关闭。"""

        try:
            driver = self._driver
            if driver.is_headless:
                return
            write = getattr(driver, "write", None)
            if not callable(write):
                return
            write(_MOUSE_REPORTING_ENABLE_SEQUENCE)
            flush = getattr(driver, "flush", None)
            if callable(flush):
                flush()
        except Exception:  # noqa: BLE001 - Driver 退出瞬间不可写时忽略
            return

    def _recover_stale_mouse_interaction(self) -> None:
        """周期核对控制台/终端输入模式；被系统重置时恢复。"""

        try:
            if self._restore_terminal_input_mode():
                # 控制台模式被系统重置后不会再产生 Textual 可识别的
                # AppFocus，因此必须由周期看门狗主动恢复终端协议。
                LOGGER.warning("控制台输入模式被外部重置，已恢复终端鼠标与键盘协议。")
                self._reset_mouse_interaction_state(rearm_terminal_protocols=True)
        except Exception as exc:  # noqa: BLE001
            # 这是周期自愈边界，而不是业务主流程。锁屏恢复、窗口关闭或 Driver
            # 正在停止时，私有终端状态可能短暂不可读；异常若逃逸，Textual 会
            # 将定时器错误升级为致命退出，正是长时间闲置后窗口自行结束的根因。
            try:
                self.log.debug("终端输入自愈暂时跳过", exc)
            except Exception:  # noqa: BLE001
                pass
        finally:
            self._interaction_watchdog_signature = None
            self._interaction_watchdog_stable_ticks = 0

    def _reset_mouse_interaction_state(
        self,
        *,
        focus_composer: bool = False,
        rearm_terminal_protocols: bool = False,
    ) -> None:
        """统一清除 Textual 组件、Screen 和 Driver 的未完成鼠标状态。"""

        self.capture_mouse(None)
        screen = self.screen
        screen.clear_selection()
        # Textual 8.2.7 的公开 clear_selection() 不会清理这两个按下状态；
        # MouseUp 丢失后必须同步复位，否则选区自动滚动仍会继续拦截鼠标。
        screen._mouse_down_offset = None
        screen._selecting = False

        driver = self._driver
        down_buttons = getattr(driver, "_down_buttons", None)
        if isinstance(down_buttons, list):
            down_buttons.clear()
        if rearm_terminal_protocols:
            if isinstance(driver, OmniCrawlWindowsDriver):
                if not driver.is_headless:
                    _restore_windows_raw_input_mode_if_needed()
            elif not driver.is_headless:
                _restore_windows_vt_input_mode_if_needed()
            enable_mouse_support = getattr(driver, "_enable_mouse_support", None)
            if callable(enable_mouse_support):
                enable_mouse_support()
            write = getattr(driver, "write", None)
            if callable(write):
                write("\033[?1004h")
                write(OmniCrawlWindowsDriver.KEYBOARD_PROTOCOL)
                # Textual 的 _enable_mouse_support 只写它自己的鼠标模式；这里
                # 补齐并关闭终端的「滚轮→上下键」回退（alternate scroll）。
                write(_MOUSE_REPORTING_ENABLE_SEQUENCE)
            enable_bracketed_paste = getattr(driver, "_enable_bracketed_paste", None)
            if callable(enable_bracketed_paste):
                enable_bracketed_paste()
            flush = getattr(driver, "flush", None)
            if callable(flush):
                flush()

        self._interaction_watchdog_signature = None
        self._interaction_watchdog_stable_ticks = 0
        if focus_composer and len(self.screen_stack) == 1:
            self.query_one("#composer", TextArea).focus()


