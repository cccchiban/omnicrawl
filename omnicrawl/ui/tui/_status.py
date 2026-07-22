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
from ._display import _display_unit_width, _display_width, _iter_display_units, _split_display_rows

USER_PREFIX = "$"


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


def _prompt_status_parts(
    model_label: str | None,
    input_tokens: int,
    output_tokens: int,
    cached_input_tokens: int,
) -> list[tuple[str, ColorRole]]:
    """返回未着色的 token 状态片段，供单行和窄屏换行渲染共用。"""

    parts: list[tuple[str, ColorRole]] = []
    if model_label:
        parts.append((f"- {model_label}", "muted"))
    parts.extend(
        [
            (f"in:{input_tokens}", "primary"),
            (f"cache:{cached_input_tokens}", "success"),
            (f"out:{output_tokens}", "secondary"),
        ]
    )
    return parts


def _render_styled_status_rows(
    text: str,
    *,
    role: ColorRole,
    caps: TerminalCapabilities,
    max_width: int,
) -> list[str]:
    """在文本折行后为每个物理行独立应用状态色。"""

    return [
        color_text(row, role, caps)
        for row in _split_display_rows(text, max(1, max_width))
    ]


def _render_prompt_status_rows(
    model_label: str | None,
    input_tokens: int,
    output_tokens: int,
    cached_input_tokens: int,
    *,
    caps: TerminalCapabilities,
    max_width: int,
) -> list[str]:
    """按可见宽度分行后再着色，绝不在 ANSI SGR 序列中间换行。"""

    max_width = max(1, max_width)
    parts = _prompt_status_parts(
        model_label,
        input_tokens,
        output_tokens,
        cached_input_tokens,
    )
    rows: list[list[tuple[str, ColorRole]]] = [[]]
    row_width = 0
    for text, role in parts:
        separator = " " if rows[-1] else ""
        part_width = _display_width(separator + text)
        if row_width and row_width + part_width > max_width:
            rows.append([])
            row_width = 0
            separator = ""
        if separator:
            rows[-1].append((separator, "text"))
            row_width += 1
        for unit in _iter_display_units(text):
            unit_width = _display_unit_width(unit)
            if row_width and row_width + unit_width > max_width:
                rows.append([])
                row_width = 0
            rows[-1].append((unit, role))
            row_width += unit_width

    rendered_rows: list[str] = []
    for row in rows:
        if not row:
            continue
        # 同一语义色的连续显示单元合并后再套 SGR，既保证换行边界安全，
        # 又避免窄屏 token 行产生大量单字符 ANSI 序列。
        fragments: list[tuple[str, ColorRole]] = []
        for text, role in row:
            if fragments and fragments[-1][1] == role:
                fragments[-1] = (fragments[-1][0] + text, role)
            else:
                fragments.append((text, role))
        rendered_rows.append(
            "".join(color_text(text, role, caps) for text, role in fragments)
        )
    return rendered_rows or [""]


from typing import Any
