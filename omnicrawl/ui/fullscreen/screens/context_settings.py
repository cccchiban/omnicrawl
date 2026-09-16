"""上下文设置（可内嵌右侧的 Pane）：上下文长度 + 压缩阈值，两个下拉各自即选即存。

与 ``SelectPane`` 同语义：选中候选立即调用 ``applier`` 保存；返回文本以
“设置未完成：”开头视为失败，把该字段回滚到原值并展示错误。
``fields`` 的当前值必须是候选之一（``Select`` 拒绝未知取值），由调用方折算。
"""

from __future__ import annotations

from typing import Any, Callable, Sequence

from textual.app import ComposeResult
from textual.binding import Binding
from textual.containers import Vertical
from textual.widgets import Select, Static

from ..terminal.theme import terminal_css, terminal_select_css
from .panes import SettingsPane

_FIELD_ID_PREFIX = "context-field-"

_PANE_CSS = """
#context-pane-body { height: 1fr; }
.context-field-label { height: 1; margin-top: 1; color: $terminal-text-muted; }
#context-pane-status { height: 2; margin-top: 1; color: $terminal-white; }
#context-pane-hint { height: 1; color: $terminal-text-muted; }
"""


class ContextSettingsPane(SettingsPane):
    """上下文二级面板：多个下拉字段，每个字段选中即保存、失败回滚。"""

    BINDINGS = [
        Binding("escape", "cancel", "返回", priority=True),
    ]

    DEFAULT_CSS = terminal_css(_PANE_CSS + terminal_select_css())

    def __init__(
        self,
        fields: Sequence[tuple[str, str, Sequence[tuple[str, Any]], Any]],
        applier: Callable[[str, Any], str],
        *,
        agent: Any | None = None,
    ) -> None:
        """``fields`` 为（子键、标签、候选、当前值）四元组序列。"""

        super().__init__(agent=agent)
        self._fields: list[tuple[str, str, list[tuple[str, Any]], Any]] = []
        self._applied: dict[str, Any] = {}
        for key, label, options, current in fields:
            candidates = [(str(text), value) for text, value in options]
            self._fields.append((key, label, candidates, current))
            self._applied[key] = current
        self._applier = applier
        self._busy = False
        self._status = ""

    def compose_pane(self) -> ComposeResult:
        with Vertical(id="context-pane-body"):
            for key, label, candidates, value in self._fields:
                yield Static(label, classes="context-field-label")
                yield Select(
                    candidates,
                    value=value,
                    allow_blank=False,
                    id=f"{_FIELD_ID_PREFIX}{key}",
                    classes="choice-select",
                )
            yield Static(self._status, id="context-pane-status")
            yield Static("Tab 切换字段；选中即保存。", id="context-pane-hint")

    def activate(self) -> None:
        selects = self.query(".choice-select")
        if selects:
            selects.first().focus()

    def action_cancel(self) -> None:
        if not self._busy:
            self.request_back()

    def on_select_changed(self, event: Select.Changed) -> None:
        if self._busy or event.value is Select.NULL:
            return
        key = str(getattr(event.select, "id", "") or "").removeprefix(_FIELD_ID_PREFIX)
        if key not in self._applied:
            return
        previous = self._applied[key]
        # 挂载初始同步或用户重选同一值：值未真正变化，不触发保存。
        if event.value == previous:
            return
        self._busy = True
        try:
            status = self._applier(key, event.value)
            if status.startswith("设置未完成"):
                event.select.value = previous
            else:
                self._applied[key] = event.value
            self._status = status
        finally:
            self._busy = False
        if self.is_mounted:
            self.query_one("#context-pane-status", Static).update(self._status)


__all__ = ["ContextSettingsPane"]
