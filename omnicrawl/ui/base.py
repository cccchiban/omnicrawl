"""UI 抽象基类 — 定义前端接口契约，TUI / Web 等实现均继承此类。"""

from __future__ import annotations


import abc
import threading
from typing import Any


class UIStartupError(RuntimeError):
    """前端界面启动失败时抛出，通常由缺少 GUI 依赖或系统图形环境异常引起。"""


class BaseUI(abc.ABC):
    """前端 UI 的抽象接口。

    所有前端实现（TUI、Web 等）必须提供这些方法，供 chat_session /
    speech_playback 等模块调用。方法分为三类：

    1. 样式快捷方法 — 返回带样式标记的文本（实现可忽略样式）
    2. 输出方法 — 向用户展示信息
    3. 交互方法 — 获取用户输入 / 确认
    """

    def __init__(self, *, model_label: str | None = None) -> None:
        self.model_label = model_label
        self._lock = threading.Lock()
        self._input_tokens = 0
        self._output_tokens = 0
        self._cached_input_tokens = 0

    # ── Token 统计 ───────────────────────────────────────────

    def update_token_usage(
        self,
        input_tokens: int,
        output_tokens: int,
        cached_input_tokens: int = 0,
    ) -> None:
        with self._lock:
            self._input_tokens = max(0, int(input_tokens))
            self._output_tokens = max(0, int(output_tokens))
            self._cached_input_tokens = max(0, int(cached_input_tokens))

    def set_model_label(self, text: str) -> None:
        """更新当前模型标签；具体 UI 可覆盖此方法同步刷新界面。"""

        with self._lock:
            self.model_label = text.strip()

    def show_html(self, title: str, html: str) -> None:
        """在支持的图形界面中显示 HTML；终端界面默认忽略。"""

        return None

    # ── 样式快捷方法（默认无样式透传文本）──────────────────────

    def muted(self, text: str) -> str:
        return text

    def muted_italic(self, text: str) -> str:
        return text

    def result_text(self, ok: bool, text: str) -> str:
        return text

    def accent(self, text: str) -> str:
        return text

    def bright(self, text: str) -> str:
        return text

    def primary(self, text: str) -> str:
        return text

    def secondary(self, text: str) -> str:
        return text

    def success(self, text: str) -> str:
        return text

    def warning(self, text: str) -> str:
        return text

    def error(self, text: str) -> str:
        return text

    def heading(self, text: str) -> str:
        return text

    # ── 输出方法 ─────────────────────────────────────────────

    @abc.abstractmethod
    def print_startup_panel(self, title: str, lines: list[str]) -> None: ...

    @abc.abstractmethod
    def print_tool_call_start(
        self,
        step: int,
        tool_name: str,
        arguments: dict[str, Any],
        *,
        leading_blank: bool = True,
    ) -> None: ...

    @abc.abstractmethod
    def print_tool_result_record(
        self,
        ok: bool,
        output: str | None = None,
        *,
        tool_name: str = "",
    ) -> None: ...

    @abc.abstractmethod
    def write_markdown_delta(self, delta: str, state: Any) -> None: ...

    @abc.abstractmethod
    def flush_markdown(self, state: Any) -> None: ...

    @abc.abstractmethod
    def print_ai_prefix(self) -> None: ...

    @abc.abstractmethod
    def write(self, text: str) -> None: ...

    @abc.abstractmethod
    def newline(self) -> None: ...

    @abc.abstractmethod
    def status(self, message: str, *, leading_blank: bool = True, italic: bool = False) -> None: ...

    @abc.abstractmethod
    def notice(self, message: str) -> None: ...

    @abc.abstractmethod
    def mark_transient_output_start(self) -> bool: ...

    @abc.abstractmethod
    def clear_transient_output(self) -> None: ...

    # ── 输入 / 交互方法 ───────────────────────────────────────

    @abc.abstractmethod
    def prompt(self) -> str: ...

    @abc.abstractmethod
    def prompt_yes_no(self, prompt: str, confirmed_label: str = "") -> bool: ...

    @abc.abstractmethod
    def inline_turn_base(self, user_text: str) -> str: ...

    @abc.abstractmethod
    def replace_current_input_with_status(self, message: str) -> None: ...

    @abc.abstractmethod
    def clear_current_input_status(self) -> None: ...
