"""状态行与提示状态。"""

from __future__ import annotations

import shutil
import sys
import threading

from ._colors import (
    ANSI_CLEAR_LINE,
    ANSI_PREVIOUS_LINE,
    ANSI_RESET,
    ColorRole,
    color_text,
    get_color_sequence,
)
from ._capabilities import TerminalCapabilities
from ._display import _display_width

USER_PREFIX = "▸"


class StatusLine:
    """当前行上的弱提示和等待动画。"""

    def __init__(self, ui: Any) -> None:
        self._ui = ui
        self._visible = False

    def show(self, text: str) -> None:
        indent = " " * self._ui.prompt_width()
        rendered = f"{indent}{text}"
        with self._ui._lock:
            if self._ui.capabilities.ansi:
                prefix = "\n" if not self._visible else "\r"
                print(
                    f"{prefix}{ANSI_CLEAR_LINE}{self._ui.bright(rendered)}",
                    end="",
                    flush=True,
                )
                self._visible = True
            elif not self._visible:
                print(f"\n{rendered}", flush=True)
                self._visible = True

    def clear(self) -> None:
        with self._ui._lock:
            if not self._visible:
                return
            if self._ui.capabilities.ansi:
                print(f"\r{ANSI_CLEAR_LINE}", end="", flush=True)
            self._visible = False

    def new_line_for_input(self, prefix: str = USER_PREFIX) -> None:
        with self._ui._lock:
            if self._ui.capabilities.ansi:
                print(f"\r{ANSI_CLEAR_LINE}", end="", flush=True)
            print(f"{color_text(prefix, 'primary', self._ui.capabilities)} ", end="", flush=True)
            self._visible = False


def prompt_status_line(
    model_label: str | None,
    input_tokens: int,
    output_tokens: int,
    cached_input_tokens: int,
    *,
    caps: TerminalCapabilities,
) -> str:
    """返回输入框下方的模型和 token 状态行（语义化标签独立着色）。"""

    if not caps.ansi:
        token_text = f"in:{input_tokens} cache:{cached_input_tokens} out:{output_tokens}"
        label = model_label or ""
        return f"- {label} {token_text}".strip()

    parts: list[str] = []
    if model_label:
        parts.append(color_text(f"- {model_label}", "muted", caps))

    # 语义化指标标签
    parts.append(color_text("in:", "primary", caps) + color_text(str(input_tokens), "text", caps))
    parts.append(color_text("cache:", "success", caps) + color_text(str(cached_input_tokens), "text", caps))
    parts.append(color_text("out:", "secondary", caps) + color_text(str(output_tokens), "text", caps))

    return " ".join(parts)


from typing import Any
